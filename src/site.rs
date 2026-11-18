//! Static site builder: archive database → plain HTML pages + Pagefind search index.
//!
//! Output layout (all under `out/`, served at `base`):
//! ```text
//! index.html                      board index
//! forums/<id>/[page-N/]index.html thread listings (categories use categories/<id>/)
//! threads/<id>/[page-N/]index.html
//! members/<id>/index.html
//! search/index.html
//! assets/style.css
//! pagefind/                       search index + UI
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

use crate::sanitize::{LinkContext, clean_post_html, local_href};
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
}

#[derive(Debug, Default)]
pub struct BuildReport {
    pub pages: usize,
    pub thread_pages: usize,
    pub missing_thread_pages: usize,
    pub search_pages_indexed: usize,
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
}

impl<'a> Builder<'a> {
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

    fn render(&mut self, tpl: &str, rel_dir: &str, ctx: minijinja::Value) -> Result<()> {
        let html = self.env.get_template(tpl)?.render(ctx)?;
        write_page(&self.opts.out_dir, rel_dir, &html)?;
        self.report.pages += 1;
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
        let ctx = context! {
            site => v(self.site()),
            crumbs => v(Vec::<Crumb>::new()),
            categories => v(categories),
            stats => v(context! { threads => stats.threads, posts => stats.posts, users => stats.users }),
        };
        self.render("index.html", "", ctx)
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
                self.render("forum.html", &dir, ctx)?;
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
            "SELECT id, page, author_id, author_name, posted_at, body_html FROM posts
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
            let rows: Vec<(i64, u32, Option<i64>, String, Option<String>, String)> = posts_stmt
                .query_map([tid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (pid, page, author_id, author, posted_at, body) in rows {
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
                    let orig = if page > 1 { format!("{original_thread}page-{page}") } else { original_thread.clone() };
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
                self.render("thread.html", &dir, ctx)?;
                self.report.thread_pages += 1;
            }
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
            self.render("member.html", &local_href("", "members", uid, 1), ctx)?;
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

    let mut b = Builder {
        conn,
        opts,
        env: env()?,
        links: LinkContext { origin: opts.origin.trim_end_matches('/').to_string(), base: opts.base.clone() },
        forums,
        thread_counts,
        report: BuildReport::default(),
    };

    b.build_index()?;
    b.build_forums()?;
    b.build_threads()?;
    b.build_members()?;
    let ctx = context! { site => v(b.site()), crumbs => v(Vec::<Crumb>::new()) };
    b.render("search.html", "search", ctx)?;
    let ctx = context! { site => v(b.site()), crumbs => v(Vec::<Crumb>::new()) };
    b.render("settings.html", "settings", ctx)?;

    let assets = opts.out_dir.join("assets");
    fs::create_dir_all(&assets)?;
    fs::write(assets.join("style.css"), include_str!("../templates/style.css"))?;
    fs::write(assets.join("favicon.svg"), include_str!("../templates/favicon.svg"))?;

    if !opts.skip_search {
        b.report.search_pages_indexed = build_search_index(&opts.out_dir)?;
    }
    Ok(b.report)
}

/// Index the generated thread pages with Pagefind and write `out/pagefind/`.
fn build_search_index(out: &Path) -> Result<usize> {
    use pagefind::api::PagefindIndex;
    use pagefind::options::PagefindServiceConfig;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let config = PagefindServiceConfig::builder().force_language("en".to_string()).build();
        let mut index = PagefindIndex::new(Some(config))?;
        let site = out.to_string_lossy().to_string();
        let count = index.add_directory(site, Some("threads/**/*.html".into())).await?;
        index.write_files(Some(out.join("pagefind").to_string_lossy().to_string())).await?;
        Ok(count)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_escaped_but_slashes_kept() {
        let env = env().unwrap();
        let out = env
            .render_str("{{ t }}|<a href=\"{{ h }}\">", context! { t => "<script>x</script>", h => "/a/b?q=\"1\"" })
            .unwrap();
        assert_eq!(out, "&lt;script&gt;x&lt;/script&gt;|<a href=\"/a/b?q=&quot;1&quot;\">");
    }
}
