//! Static site builder: archive database → plain HTML pages + Pagefind search index.
//!
//! Output layout (all under `out/`, served at `base`):
//! ```text
//! index.html                      board index
//! forums/<id>/[page-N/]index.html thread listings (categories use categories/<id>/)
//! threads/<id>/[page-N/]index.html
//! members/<id>/index.html
//! search/index.html
//! <collection>/[<board id>/]index.html   curated A–Z lists (optional)
//! sitemap.xml, sitemap-N.xml      for search engines (with --public-url)
//! assets/style.css
//! pagefind/, pagefind-N/          search index parts + UI
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime};
use minijinja::{Environment, Value, context};

fn v<T: Serialize>(x: T) -> Value {
    Value::from(minijinja::value::Serde(x))
}
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::json;

use crate::sanitize::{KnownPages, LinkContext, clean_post_html, local_href};
use crate::store::Store;

pub struct BuildOptions {
    pub out_dir: PathBuf,
    /// URL prefix the archive is served under; must start and end with `/`.
    pub base: String,
    pub title: String,
    /// Original forum origin, e.g. `https://forums.rpgmakerweb.com`.
    pub origin: String,
    pub contact: String,
    /// Optional links shown in the footer: your removal policy, privacy policy and about page.
    pub removal_url: Option<String>,
    pub privacy_url: Option<String>,
    pub about_url: Option<String>,
    /// Short notice shown in the footer of every page.
    pub notice: String,
    pub threads_per_page: usize,
    /// Skip building the search index (faster, for tests and previews).
    pub skip_search: bool,
    /// Thread pages per search index part. Pagefind keeps a whole index in memory
    /// while building it (about 270 KB per RPG Maker thread page), so big forums
    /// are indexed in parts that the search page merges again.
    pub search_chunk: usize,
    /// Public address of the site, scheme and host only (e.g. `https://thecodelair.com`).
    /// Enables canonical links, structured data for search engines and `sitemap.xml`.
    pub public_url: Option<String>,
    /// Curated A–Z lists of threads from chosen boards, e.g. "Plugins & Scripts".
    pub collections: Vec<Collection>,
}

/// A curated list of boards, shown as one A–Z page per board.
#[derive(Debug, Clone)]
pub struct Collection {
    pub slug: String,
    pub title: String,
    /// Board ids in display order, each with an optional display name.
    pub groups: Vec<(i64, Option<String>)>,
}

/// Paths the archive itself uses; a collection can't take them.
const RESERVED: [&str; 8] = ["threads", "forums", "categories", "members", "search", "settings", "assets", "sitemap"];

impl Collection {
    /// Parse `slug|Title|12=Display name,34,56`: board ids, optionally with a name.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut parts = spec.splitn(3, '|');
        let slug = parts.next().unwrap_or("").trim().to_string();
        let title = parts.next().unwrap_or("").trim().to_string();
        let list = parts.next().unwrap_or("").trim();
        anyhow::ensure!(
            !slug.is_empty() && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "collection name must be lowercase letters, digits or '-': {spec}"
        );
        anyhow::ensure!(
            !RESERVED.contains(&slug.as_str()) && !slug.starts_with("pagefind"),
            "collection name '{slug}' is used by the archive itself"
        );
        anyhow::ensure!(!title.is_empty() && !list.is_empty(), "collection needs name|Title|board ids: {spec}");
        let mut groups = Vec::new();
        for item in list.split(',') {
            let (id, label) = match item.split_once('=') {
                Some((i, l)) => (i, Some(l.trim().to_string()).filter(|l| !l.is_empty())),
                None => (item, None),
            };
            let id = id.trim().parse::<i64>().with_context(|| format!("bad board id '{id}' in {spec}"))?;
            groups.push((id, label));
        }
        Ok(Collection { slug, title, groups })
    }
}

/// Search-engine details for one page.
#[derive(Default)]
struct PageMeta {
    description: Option<String>,
    noindex: bool,
    /// Newest post date on the page (RFC 3339), for the sitemap.
    lastmod: Option<String>,
    /// JSON-LD structured data, already serialized.
    jsonld: Option<String>,
}

/// Plain-text excerpt of at most `max` characters, cut at a word boundary.
fn snippet(text: &str, max: usize) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        return t;
    }
    let cut: String = t.chars().take(max).collect();
    match cut.rfind(' ') {
        Some(i) if i > max / 2 => format!("{}…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}

/// Text for a search-result description: drops code banners like `//=====`
/// and the "Code:" labels XenForo puts above code blocks.
fn description_text(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| *w != "Code:" && *w != "JavaScript:" && *w != "Ruby:")
        .filter(|w| {
            // A "word" that is mostly repeated punctuation is decoration, not content.
            let punct = w.chars().filter(|c| !c.is_alphanumeric()).count();
            !(punct >= 4 && punct * 4 >= w.chars().count() * 3)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Serialize JSON-LD so it is safe inside a `<script>` element.
fn json_for_script(value: &serde_json::Value) -> String {
    value.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

/// Sort key for titles: ignore leading tags like `[MZ]` or `(MV)` and punctuation.
fn title_sort_key(title: &str) -> String {
    let mut t = title.trim();
    loop {
        let stripped = if let Some(rest) = t.strip_prefix('[') {
            rest.split_once(']').map(|(_, r)| r)
        } else if let Some(rest) = t.strip_prefix('(') {
            rest.split_once(')').map(|(_, r)| r)
        } else {
            None
        };
        match stripped {
            Some(r) => t = r.trim_start(),
            None => break,
        }
    }
    let key = t.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    if key.is_empty() {
        // The whole title was a tag, e.g. "(removed)": sort by the tag's text.
        title.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
    } else {
        key
    }
}

#[derive(Debug, Default)]
pub struct BuildReport {
    pub pages: usize,
    pub thread_pages: usize,
    pub missing_thread_pages: usize,
    pub search_pages_indexed: usize,
    pub sitemap_urls: usize,
}

#[derive(Serialize)]
struct Site<'a> {
    title: &'a str,
    base: &'a str,
    origin: &'a str,
    contact: &'a str,
    notice: &'a str,
    removal_url: Option<&'a str>,
    privacy_url: Option<&'a str>,
    about_url: Option<&'a str>,
    project_url: &'static str,
}

#[derive(Serialize, Clone)]
struct Crumb {
    title: String,
    href: String,
}

#[derive(Serialize)]
struct PagerLink {
    num: u32,
    href: String,
    current: bool,
    gap: bool,
}

#[derive(Serialize)]
struct Pager {
    current: u32,
    total: u32,
    links: Vec<PagerLink>,
}

fn pager(current: u32, total: u32, href: impl Fn(u32) -> String) -> Pager {
    let mut links = Vec::new();
    let mut last_shown = 0;
    for n in 1..=total {
        let show = n == 1 || n == total || n.abs_diff(current) <= 2;
        if !show {
            continue;
        }
        if last_shown != 0 && n > last_shown + 1 {
            links.push(PagerLink { num: 0, href: String::new(), current: false, gap: true });
        }
        links.push(PagerLink { num: n, href: href(n), current: n == current, gap: false });
        last_shown = n;
    }
    Pager { current, total, links }
}

/// Human date from the stored RFC 3339 / naive timestamp.
pub fn display_date(s: Option<&str>) -> String {
    let Some(s) = s else { return String::new() };
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return d.format("%b %-d, %Y %H:%M").to_string();
    }
    if let Ok(d) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        // Some old forums only show the day; don't invent a time for those.
        if s.ends_with("T00:00:00") {
            return d.format("%b %-d, %Y").to_string();
        }
        return d.format("%b %-d, %Y %H:%M").to_string();
    }
    s.to_string()
}

#[derive(Clone)]
struct ForumRow {
    id: i64,
    title: String,
    path: String,
    parent_id: Option<i64>,
}

impl ForumRow {
    /// XenForo categories are containers, linked as `categories/<id>/`.
    fn route(&self) -> &'static str {
        if self.path.starts_with("/categories/") { "categories" } else { "forums" }
    }
}

#[derive(Serialize)]
struct ForumLink {
    title: String,
    href: String,
    threads: i64,
}

/// Escape text for HTML element content and quoted attributes. Unlike
/// minijinja's default this leaves `/` alone, which keeps URLs readable and
/// pages smaller (`/` needs no escaping in either context).
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn env() -> Result<Environment<'static>> {
    let mut env = Environment::new();
    env.set_formatter(|out, _state, value| {
        if value.is_safe() || value.is_undefined() || value.is_none() {
            if !value.is_undefined() && !value.is_none() {
                write!(out, "{value}")?;
            }
        } else {
            out.write_str(&escape_html(&value.to_string()))?;
        }
        Ok(())
    });
    env.add_template("base.html", include_str!("../templates/base.html"))?;
    env.add_template("macros.html", include_str!("../templates/macros.html"))?;
    env.add_template("index.html", include_str!("../templates/index.html"))?;
    env.add_template("forum.html", include_str!("../templates/forum.html"))?;
    env.add_template("thread.html", include_str!("../templates/thread.html"))?;
    env.add_template("member.html", include_str!("../templates/member.html"))?;
    env.add_template("search.html", include_str!("../templates/search.html"))?;
    env.add_template("settings.html", include_str!("../templates/settings.html"))?;
    env.add_template("collection.html", include_str!("../templates/collection.html"))?;
    env.add_template("collection_group.html", include_str!("../templates/collection_group.html"))?;
    Ok(env)
}

fn write_page(out: &Path, rel_dir: &str, html: &str) -> Result<()> {
    let dir = out.join(rel_dir.trim_matches('/'));
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("index.html"), html).with_context(|| format!("writing {}", dir.display()))
}

struct Builder<'a> {
    conn: &'a Connection,
    opts: &'a BuildOptions,
    env: Environment<'static>,
    links: LinkContext,
    forums: HashMap<i64, ForumRow>,
    thread_counts: HashMap<i64, i64>,
    report: BuildReport,
    /// Indexable pages for the sitemap: absolute URL and last modification.
    sitemap: Vec<(String, Option<String>)>,
    /// Collection cards for the archive's front page.
    collection_cards: Vec<CollectionCard>,
}

#[derive(Serialize, Clone)]
struct CollectionCard {
    title: String,
    href: String,
    threads: usize,
    groups: Vec<String>,
}

impl<'a> Builder<'a> {
    /// Absolute URL of an archive page, when `--public-url` is set.
    fn canonical(&self, rel_dir: &str) -> Option<String> {
        let rel = rel_dir.trim_matches('/');
        self.opts.public_url.as_ref().map(|u| {
            let slash = if rel.is_empty() { "" } else { "/" };
            format!("{}{}{rel}{slash}", u.trim_end_matches('/'), self.opts.base)
        })
    }

    fn site(&self) -> Site<'a> {
        Site {
            title: &self.opts.title,
            base: &self.opts.base,
            origin: &self.opts.origin,
            contact: &self.opts.contact,
            notice: &self.opts.notice,
            removal_url: self.opts.removal_url.as_deref(),
            privacy_url: self.opts.privacy_url.as_deref(),
            about_url: self.opts.about_url.as_deref(),
            project_url: crate::PROJECT_URL,
        }
    }

    fn forum_href(&self, f: &ForumRow, page: u32) -> String {
        local_href(&self.opts.base, f.route(), f.id, page)
    }

    /// Breadcrumbs for a forum, outermost first (including the forum itself).
    fn forum_chain(&self, id: i64) -> Vec<Crumb> {
        let mut chain = Vec::new();
        let mut cur = self.forums.get(&id);
        let mut guard = 0;
        while let Some(f) = cur {
            chain.push(Crumb { title: f.title.clone(), href: self.forum_href(f, 1) });
            cur = f.parent_id.and_then(|p| self.forums.get(&p));
            guard += 1;
            if guard > 32 {
                break; // defensive: bad data could form a cycle
            }
        }
        chain.reverse();
        chain
    }

    fn children(&self, parent: Option<i64>) -> Vec<&ForumRow> {
        let mut v: Vec<_> = self.forums.values().filter(|f| f.parent_id == parent).collect();
        v.sort_by_key(|f| f.id);
        v
    }

    fn subtree_thread_count(&self, id: i64) -> i64 {
        let own = self.thread_counts.get(&id).copied().unwrap_or(0);
        own + self.children(Some(id)).iter().map(|c| self.subtree_thread_count(c.id)).sum::<i64>()
    }

    fn forum_link(&self, f: &ForumRow) -> ForumLink {
        ForumLink { title: f.title.clone(), href: self.forum_href(f, 1), threads: self.subtree_thread_count(f.id) }
    }

    fn render(&mut self, tpl: &str, rel_dir: &str, ctx: minijinja::Value, meta: PageMeta) -> Result<()> {
        let canonical = self.canonical(rel_dir);
        let page = context! {
            description => meta.description,
            noindex => meta.noindex,
            canonical => canonical.clone(),
            jsonld => meta.jsonld.map(Value::from_safe_string),
        };
        let html = self.env.get_template(tpl)?.render(context! { page => page, ..ctx })?;
        write_page(&self.opts.out_dir, rel_dir, &html)?;
        self.report.pages += 1;
        if let (false, Some(url)) = (meta.noindex, canonical) {
            self.sitemap.push((url, meta.lastmod));
        }
        Ok(())
    }

    fn build_index(&mut self) -> Result<()> {
        #[derive(Serialize)]
        struct Category {
            title: String,
            forums: Vec<ForumLink>,
        }
        let mut categories = Vec::new();
        let mut loose = Vec::new();
        for root in self.children(None) {
            let kids = self.children(Some(root.id));
            if kids.is_empty() {
                loose.push(self.forum_link(root));
            } else {
                categories.push(Category {
                    title: root.title.clone(),
                    forums: kids.iter().map(|k| self.forum_link(k)).collect(),
                });
            }
        }
        if !loose.is_empty() {
            categories.push(Category { title: "Other boards".into(), forums: loose });
        }
        let stats = Store::stats_of(self.conn)?;
        let description = format!(
            "Read-only archive of the {}: {} threads and {} posts, searchable, with no account needed.",
            self.opts.title.trim_end_matches(" Archive"),
            stats.threads,
            stats.posts
        );
        let ctx = context! {
            site => v(self.site()),
            crumbs => v(Vec::<Crumb>::new()),
            categories => v(categories),
            collections => v(self.collection_cards.clone()),
            stats => v(context! { threads => stats.threads, posts => stats.posts, users => stats.users }),
        };
        self.render("index.html", "", ctx, PageMeta { description: Some(description), ..Default::default() })
    }

    fn build_forums(&mut self) -> Result<()> {
        #[derive(Serialize)]
        struct Row {
            title: String,
            href: String,
            author: String,
            posts: i64,
            last: String,
        }
        let ids: Vec<i64> = self.forums.keys().copied().collect();
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, t.path, COALESCE(t.author_name, ''), COUNT(p.id) AS n, MAX(p.posted_at) AS last
               FROM threads t LEFT JOIN posts p ON p.thread_id = t.id
              WHERE t.forum_id = ?1
              GROUP BY t.id
              ORDER BY last IS NULL, last DESC, t.id DESC",
        )?;
        for id in ids {
            let f = self.forums[&id].clone();
            let rows: Vec<Row> = stmt
                .query_map([id], |r| {
                    let tid: i64 = r.get(0)?;
                    let path: String = r.get(2)?;
                    let n: i64 = r.get(4)?;
                    let last: Option<String> = r.get(5)?;
                    // Threads only seen in listings have no archived posts: send readers to Wayback.
                    let href = if n > 0 {
                        local_href(&self.opts.base, "threads", tid, 1)
                    } else {
                        self.links.wayback(&self.links.original(&path), false)
                    };
                    Ok(Row { title: r.get(1)?, href, author: r.get(3)?, posts: n, last: display_date(last.as_deref()) })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let subforums: Vec<ForumLink> = self.children(Some(id)).iter().map(|c| self.forum_link(c)).collect();
            let per = self.opts.threads_per_page.max(1);
            let total = rows.len().div_ceil(per).max(1) as u32;
            let mut chunks: Vec<&[Row]> = rows.chunks(per).collect();
            if chunks.is_empty() {
                chunks.push(&[]);
            }
            let mut crumbs = self.forum_chain(id);
            crumbs.pop();
            for (i, chunk) in chunks.into_iter().enumerate() {
                let page = i as u32 + 1;
                let base = self.opts.base.clone();
                let route = f.route();
                let p = pager(page, total, |n| local_href(&base, route, id, n));
                let ctx = context! {
                    site => v(self.site()),
                    crumbs => v(crumbs.clone()),
                    forum => v(context! { title => f.title.clone() }),
                    subforums => v(if page == 1 { subforums.iter().collect::<Vec<_>>() } else { vec![] }),
                    threads => v(chunk),
                    pager => v(p),
                };
                let dir = self.forum_href(&f, page).trim_start_matches(&self.opts.base).to_string();
                let description = format!("Archived threads from the {} board of the {}.", f.title, self.opts.title);
                self.render("forum.html", &dir, ctx, PageMeta { description: Some(description), ..Default::default() })?;
            }
        }
        Ok(())
    }

    fn build_threads(&mut self) -> Result<()> {
        #[derive(Serialize)]
        struct AttachmentView {
            filename: String,
            href: String,
        }
        #[derive(Serialize)]
        struct PostView {
            id: i64,
            author: String,
            author_href: Option<String>,
            date: String,
            body: String,
            attachments: Vec<AttachmentView>,
        }

        let threads: Vec<(i64, String, String, Option<i64>, u32)> = self
            .conn
            .prepare(
                "SELECT t.id, t.title, t.path, t.forum_id, t.total_pages FROM threads t
                  WHERE EXISTS (SELECT 1 FROM posts p WHERE p.thread_id = t.id) ORDER BY t.id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut posts_stmt = self.conn.prepare(
            "SELECT id, page, author_id, author_name, posted_at, body_html, body_text FROM posts
              WHERE thread_id = ?1 ORDER BY page, position, id",
        )?;
        let mut att_stmt =
            self.conn.prepare("SELECT post_id, filename, path FROM attachments WHERE post_id IN (SELECT id FROM posts WHERE thread_id = ?1)")?;

        for (tid, title, path, forum_id, total_pages) in threads {
            let mut atts: HashMap<i64, Vec<AttachmentView>> = HashMap::new();
            for row in att_stmt.query_map([tid], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))? {
                let (pid, filename, apath) = row?;
                let href = self.links.wayback(&self.links.original(&apath), false);
                atts.entry(pid).or_default().push(AttachmentView { filename, href });
            }
            let mut by_page: BTreeMap<u32, Vec<PostView>> = BTreeMap::new();
            // Plain text, author and date per post, for descriptions and structured data.
            let mut text_by_page: BTreeMap<u32, Vec<(String, Option<String>, String)>> = BTreeMap::new();
            type PostRow = (i64, u32, Option<i64>, String, Option<String>, String, String);
            let rows: Vec<PostRow> = posts_stmt
                .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (pid, page, author_id, author, posted_at, body, text) in rows {
                text_by_page.entry(page).or_default().push((author.clone(), posted_at.clone(), text));
                by_page.entry(page).or_default().push(PostView {
                    id: pid,
                    author,
                    author_href: author_id.map(|u| local_href(&self.opts.base, "members", u, 1)),
                    date: display_date(posted_at.as_deref()),
                    body: clean_post_html(&body, &self.links),
                    attachments: atts.remove(&pid).unwrap_or_default(),
                });
            }
            let max_page = by_page.keys().copied().max().unwrap_or(1);
            let total = total_pages.max(max_page).max(1);
            let crumbs = forum_id.map(|f| self.forum_chain(f)).unwrap_or_default();
            let forum_title = crumbs.last().map(|c| c.title.clone()).unwrap_or_default();
            let original_thread = self.links.original(&path);

            for page in 1..=total {
                let posts = by_page.remove(&page).unwrap_or_default();
                let missing = if posts.is_empty() {
                    self.report.missing_thread_pages += 1;
                    let orig = self.links.original_page(&original_thread, page);
                    Some(self.links.wayback(&orig, false))
                } else {
                    None
                };
                let base = self.opts.base.clone();
                let p = pager(page, total, |n| local_href(&base, "threads", tid, n));
                let ctx = context! {
                    site => v(self.site()),
                    crumbs => v(crumbs.clone()),
                    thread => v(context! { title => title.clone(), forum => forum_title.clone(), original => original_thread.clone() }),
                    posts => v(posts),
                    missing => v(missing),
                    pager => v(p),
                };
                let dir = local_href("", "threads", tid, page);
                let texts = text_by_page.remove(&page).unwrap_or_default();
                let meta = self.thread_meta(&title, &original_thread, &dir, page, &texts);
                self.render("thread.html", &dir, ctx, meta)?;
                self.report.thread_pages += 1;
            }
        }
        Ok(())
    }

    /// Description, sitemap date and structured data for one thread page.
    /// `texts` holds (author, date, plain text) for each post on the page.
    fn thread_meta(
        &self,
        title: &str,
        original: &str,
        dir: &str,
        page: u32,
        texts: &[(String, Option<String>, String)],
    ) -> PageMeta {
        let Some((first_author, first_date, first_text)) = texts.first() else {
            // "Not preserved" pages have nothing to offer a search engine.
            return PageMeta { noindex: true, ..Default::default() };
        };
        let description = {
            let s = snippet(&description_text(first_text), 155);
            if s.chars().count() < 40 { format!("{title}: archived thread from the {}.", self.opts.title) } else { s }
        };
        let lastmod = texts.iter().filter_map(|(_, d, _)| d.clone()).max();
        // Google reads forum threads from schema.org DiscussionForumPosting markup.
        let jsonld = match (page, self.canonical(dir)) {
            (1, Some(url)) => {
                let person = |name: &str| json!({ "@type": "Person", "name": name });
                let comments: Vec<_> = texts
                    .iter()
                    .skip(1)
                    .take(30)
                    .map(|(a, d, t)| {
                        json!({ "@type": "Comment", "author": person(a), "datePublished": d, "text": snippet(t, 500) })
                    })
                    .collect();
                let mut post = json!({
                    "@context": "https://schema.org",
                    "@type": "DiscussionForumPosting",
                    "headline": title,
                    "url": url,
                    "author": person(first_author),
                    "datePublished": first_date,
                    "text": snippet(first_text, 1500),
                    "isBasedOn": original,
                    "comment": comments,
                });
                if first_date.is_none() {
                    post.as_object_mut().map(|o| o.remove("datePublished"));
                }
                Some(json_for_script(&post))
            }
            _ => None,
        };
        PageMeta { description: Some(description), noindex: false, lastmod, jsonld }
    }

    /// Curated A–Z lists: `<slug>/` lists the boards, `<slug>/<board id>/` their threads.
    fn build_collections(&mut self) -> Result<()> {
        #[derive(Serialize)]
        struct Row {
            title: String,
            href: String,
            archived: bool,
            author: String,
            started: String,
            replies: i64,
        }
        #[derive(Serialize)]
        struct GroupLink {
            title: String,
            href: String,
            threads: usize,
        }
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, t.path, COALESCE(t.author_name, ''), COUNT(p.id), MIN(p.posted_at)
               FROM threads t LEFT JOIN posts p ON p.thread_id = t.id
              WHERE t.forum_id = ?1
              GROUP BY t.id",
        )?;
        let collections = self.opts.collections.clone();
        for c in &collections {
            let home = format!("{}{}/", self.opts.base, c.slug);
            let crumbs = vec![Crumb { title: c.title.clone(), href: home.clone() }];
            let mut groups = Vec::new();
            for (fid, label) in &c.groups {
                let Some(forum) = self.forums.get(fid).cloned() else {
                    eprintln!("note: collection '{}': board {fid} is not in the archive, skipped", c.slug);
                    continue;
                };
                let name = label.clone().unwrap_or_else(|| forum.title.clone());
                let mut rows: Vec<(String, Row)> = stmt
                    .query_map([fid], |r| {
                        let tid: i64 = r.get(0)?;
                        let title: String = r.get(1)?;
                        let path: String = r.get(2)?;
                        let n: i64 = r.get(4)?;
                        let started: Option<String> = r.get(5)?;
                        Ok((tid, title, path, r.get::<_, String>(3)?, n, started))
                    })?
                    .map(|r| {
                        r.map(|(tid, title, path, author, n, started)| {
                            let href = if n > 0 {
                                local_href(&self.opts.base, "threads", tid, 1)
                            } else {
                                self.links.wayback(&self.links.original(&path), false)
                            };
                            let day = display_date(started.as_deref()).rsplit_once(' ').map(|(d, _)| d.to_string());
                            (
                                title_sort_key(&title),
                                Row {
                                    title,
                                    href,
                                    archived: n > 0,
                                    author,
                                    started: day.unwrap_or_default(),
                                    replies: (n - 1).max(0),
                                },
                            )
                        })
                    })
                    .collect::<rusqlite::Result<_>>()?;
                rows.sort_by(|a, b| a.0.cmp(&b.0));
                let rows: Vec<Row> = rows.into_iter().map(|(_, r)| r).collect();
                let dir = format!("{}/{fid}", c.slug);
                let href = format!("{}{dir}/", self.opts.base);
                let count = rows.len();
                let ctx = context! {
                    site => v(self.site()),
                    crumbs => v(&crumbs),
                    collection => v(context! { title => c.title.clone(), href => home.clone() }),
                    group => v(context! { title => name.clone(), board => self.forum_href(&forum, 1) }),
                    threads => v(rows),
                };
                let description = format!("A–Z list of all {count} {name} threads in the {}.", self.opts.title);
                self.render("collection_group.html", &dir, ctx, PageMeta { description: Some(description), ..Default::default() })?;
                groups.push(GroupLink { title: name, href, threads: count });
            }
            let total: usize = groups.iter().map(|g| g.threads).sum();
            self.collection_cards.push(CollectionCard {
                title: c.title.clone(),
                href: home.clone(),
                threads: total,
                groups: groups.iter().map(|g| g.title.clone()).collect(),
            });
            let ctx = context! {
                site => v(self.site()),
                crumbs => v(Vec::<Crumb>::new()),
                collection => v(context! { title => c.title.clone(), threads => total }),
                groups => v(groups),
            };
            let description = format!("{}: {total} threads from the {}, listed A–Z.", c.title, self.opts.title);
            self.render("collection.html", &c.slug, ctx, PageMeta { description: Some(description), ..Default::default() })?;
        }
        Ok(())
    }

    fn build_members(&mut self) -> Result<()> {
        #[derive(Serialize)]
        struct Row {
            title: String,
            href: String,
            posts: i64,
        }
        let users: Vec<(i64, String)> = self
            .conn
            .prepare("SELECT id, name FROM users ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.title, COUNT(*) FROM posts p JOIN threads t ON t.id = p.thread_id
              WHERE p.author_id = ?1 GROUP BY t.id ORDER BY MAX(p.posted_at) DESC",
        )?;
        for (uid, name) in users {
            let threads: Vec<Row> = stmt
                .query_map(params![uid], |r| {
                    Ok(Row { href: local_href(&self.opts.base, "threads", r.get(0)?, 1), title: r.get(1)?, posts: r.get(2)? })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let total: i64 = threads.iter().map(|t| t.posts).sum();
            let ctx = context! {
                site => v(self.site()),
                crumbs => v(Vec::<Crumb>::new()),
                member => v(context! { name => name, posts => total }),
                threads => v(threads),
            };
            // Member pages stay out of search engines: they are about people, not topics.
            let meta = PageMeta { noindex: true, ..Default::default() };
            self.render("member.html", &local_href("", "members", uid, 1), ctx, meta)?;
        }
        Ok(())
    }
}

/// Build the whole site from the archive database.
pub fn build(store: &Store, opts: &BuildOptions) -> Result<BuildReport> {
    anyhow::ensure!(opts.base.starts_with('/') && opts.base.ends_with('/'), "--base must start and end with '/'");
    let conn = store.conn();
    fs::create_dir_all(&opts.out_dir)?;

    let forums: HashMap<i64, ForumRow> = conn
        .prepare("SELECT id, title, path, parent_id FROM forums")?
        .query_map([], |r| Ok(ForumRow { id: r.get(0)?, title: r.get(1)?, path: r.get(2)?, parent_id: r.get(3)? }))?
        .map(|r| r.map(|f| (f.id, f)))
        .collect::<rusqlite::Result<_>>()?;
    let thread_counts: HashMap<i64, i64> = conn
        .prepare("SELECT forum_id, COUNT(*) FROM threads WHERE forum_id IS NOT NULL GROUP BY forum_id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // Everything we will build a page for, so links inside posts can tell
    // archived pages from ones that only the Wayback Machine might have.
    let mut known = KnownPages::default();
    for row in conn
        .prepare(
            "SELECT t.id, MAX(t.total_pages, MAX(p.page)) FROM threads t JOIN posts p ON p.thread_id = t.id GROUP BY t.id",
        )?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
    {
        let (id, pages) = row?;
        known.threads.insert(id, pages.max(1) as u32);
    }
    known.forums = forums.values().map(|f| (f.id, f.route())).collect();
    for row in conn.prepare("SELECT id FROM users")?.query_map([], |r| r.get::<_, i64>(0))? {
        known.members.insert(row?);
    }
    for row in conn.prepare("SELECT id, thread_id, page FROM posts")?.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, u32>(2)?))
    })? {
        let (id, thread, page) = row?;
        known.posts.insert(id, (thread, page));
    }
    // Posts per page: the fullest first page of any multi-page thread.
    known.per_page = conn
        .query_row(
            "SELECT COALESCE(MAX(n), 0) FROM (SELECT COUNT(*) AS n FROM posts p JOIN threads t ON t.id = p.thread_id
               WHERE p.page = 1 AND t.total_pages > 1 GROUP BY p.thread_id)",
            [],
            |r| r.get::<_, u32>(0),
        )
        .unwrap_or(0);

    // phpBB and SMF boards often live in a folder (`/forum/viewtopic.php`): take it from the threads.
    let board_dir: String = conn
        .query_row(
            "SELECT CASE WHEN instr(path, 'viewtopic.php') > 0 THEN substr(path, 1, instr(path, 'viewtopic.php') - 1)
                         ELSE substr(path, 1, instr(path, 'index.php?topic=') - 1) END AS d FROM threads
              WHERE path LIKE '%viewtopic.php%' OR path LIKE '%index.php?topic=%' GROUP BY d ORDER BY COUNT(*) DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "/".to_string());

    let mut b = Builder {
        conn,
        opts,
        env: env()?,
        links: LinkContext {
            origin: opts.origin.trim_end_matches('/').to_string(),
            base: opts.base.clone(),
            known: Some(std::sync::Arc::new(known)),
            board_dir,
        },
        forums,
        thread_counts,
        report: BuildReport::default(),
        sitemap: Vec::new(),
        collection_cards: Vec::new(),
    };

    b.build_collections()?;
    b.build_index()?;
    b.build_forums()?;
    b.build_threads()?;
    b.build_members()?;
    // Search index parts: `pagefind/` plus `pagefind-1/`, `pagefind-2/`, ... for big archives.
    let search_files = if opts.skip_search { Vec::new() } else { thread_page_files(&opts.out_dir)? };
    let chunk = opts.search_chunk.max(1);
    let parts = search_files.len().div_ceil(chunk).max(1);
    let extra_bundles: Vec<String> = (1..parts).map(|i| format!("{}/", bundle_dir(i))).collect();
    let ctx = context! { site => v(b.site()), crumbs => v(Vec::<Crumb>::new()), extra_bundles => v(extra_bundles.join(" ")) };
    b.render("search.html", "search", ctx, PageMeta { noindex: true, ..Default::default() })?;
    let ctx = context! { site => v(b.site()), crumbs => v(Vec::<Crumb>::new()) };
    b.render("settings.html", "settings", ctx, PageMeta { noindex: true, ..Default::default() })?;

    let assets = opts.out_dir.join("assets");
    fs::create_dir_all(&assets)?;
    fs::write(assets.join("style.css"), include_str!("../templates/style.css"))?;
    fs::write(assets.join("favicon.svg"), include_str!("../templates/favicon.svg"))?;

    if !opts.skip_search {
        b.report.search_pages_indexed = build_search_index(&opts.out_dir, &search_files, chunk)?;
    }
    if let Some(public) = &opts.public_url {
        b.report.sitemap_urls = write_sitemaps(&opts.out_dir, public, &opts.base, &b.sitemap)?;
    }
    Ok(b.report)
}

/// Most URLs allowed per sitemap file is 50,000; stay well under.
const SITEMAP_CHUNK: usize = 40_000;

/// Write `sitemap-N.xml` files and a `sitemap.xml` index pointing at them.
fn write_sitemaps(out: &Path, public: &str, base: &str, entries: &[(String, Option<String>)]) -> Result<usize> {
    let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let root = format!("{}{base}", public.trim_end_matches('/'));
    let mut index = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for (i, chunk) in entries.chunks(SITEMAP_CHUNK).enumerate() {
        let name = format!("sitemap-{}.xml", i + 1);
        let mut body = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
        for (url, lastmod) in chunk {
            body.push_str("<url><loc>");
            body.push_str(&xml(url));
            body.push_str("</loc>");
            // Dates are stored as RFC 3339; the sitemap only needs the day.
            if let Some(day) = lastmod.as_deref().filter(|d| d.len() >= 10).map(|d| &d[..10]) {
                body.push_str(&format!("<lastmod>{day}</lastmod>"));
            }
            body.push_str("</url>\n");
        }
        body.push_str("</urlset>\n");
        fs::write(out.join(&name), body)?;
        index.push_str(&format!("<sitemap><loc>{}</loc></sitemap>\n", xml(&format!("{root}{name}"))));
    }
    index.push_str("</sitemapindex>\n");
    fs::write(out.join("sitemap.xml"), index)?;
    Ok(entries.len())
}

fn bundle_dir(part: usize) -> String {
    if part == 0 { "pagefind".to_string() } else { format!("pagefind-{part}") }
}

/// Every generated thread page (`threads/**/index.html`), in a stable order.
fn thread_page_files(out: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {
                walk(&p, found)?;
            } else if p.file_name().is_some_and(|n| n == "index.html") {
                found.push(p);
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(&out.join("threads"), &mut found)?;
    found.sort();
    Ok(found)
}

/// Index thread pages with Pagefind, `chunk` pages per index part, one part at a
/// time so memory is released in between. Returns the number of pages added.
fn build_search_index(out: &Path, files: &[PathBuf], chunk: usize) -> Result<usize> {
    use pagefind::api::PagefindIndex;
    use pagefind::options::PagefindServiceConfig;

    // Parts from an earlier, bigger build would otherwise linger and be merged.
    for entry in fs::read_dir(out)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() && (name == "pagefind" || name.starts_with("pagefind-")) {
            fs::remove_dir_all(&p)?;
        }
    }

    let rt = tokio::runtime::Runtime::new()?;
    let mut total = 0;
    for (part, pages) in files.chunks(chunk.max(1)).enumerate() {
        total += rt.block_on(async {
            let config = PagefindServiceConfig::builder().force_language("en".to_string()).build();
            let mut index = PagefindIndex::new(Some(config))?;
            for file in pages {
                let rel = file.strip_prefix(out)?.parent().unwrap_or(Path::new(""));
                let url = format!("/{}/", rel.to_string_lossy().replace('\\', "/"));
                index.add_html_file(None, Some(url), fs::read_to_string(file)?).await?;
            }
            index.write_files(Some(out.join(bundle_dir(part)).to_string_lossy().to_string())).await?;
            anyhow::Ok(pages.len())
        })?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_skip_code_banners() {
        let t = "Code: //===================== //DisplacementMaps.js //===================== var Imported = Imported || {};";
        assert_eq!(description_text(t), "//DisplacementMaps.js var Imported = Imported || {};");
        assert_eq!(snippet("one two three four", 9), "one two…");
        assert_eq!(title_sort_key("[MZ] (WIP) Battle Core"), "battle core");
        assert_eq!(title_sort_key("(removed)"), "removed)");
        assert_eq!(title_sort_key("[MoreLoadingScreen]"), "moreloadingscreen]");
    }

    #[test]
    fn titles_are_escaped_but_slashes_kept() {
        let env = env().unwrap();
        let out = env
            .render_str("{{ t }}|<a href=\"{{ h }}\">", context! { t => "<script>x</script>", h => "/a/b?q=\"1\"" })
            .unwrap();
        assert_eq!(out, "&lt;script&gt;x&lt;/script&gt;|<a href=\"/a/b?q=&quot;1&quot;\">");
    }
}
