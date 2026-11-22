//! Parser for phpBB forum pages: phpBB 2 (subSilver and look-alike table themes)
//! and phpBB 3.0–3.3 (prosilver and its many derivatives, subsilver2).
//!
//! phpBB pages are addressed with query strings (`viewtopic.php?t=12&start=30`),
//! and boards often live in a sub-folder (`/forum/`, `/phpBB2/`), so stored paths
//! keep that folder: `/phpBB2/viewtopic.php?t=12`.

use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};
use scraper::{ElementRef, Html, Selector};

use crate::model::{Attachment, Forum, ForumPage, Page, Post, ThreadPage, ThreadSummary, User};
use crate::parse::xenforo::visible_text;
use crate::urls::{PhpbbLink, phpbb_link, query_params, script_dir, strip_wayback};

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

fn text_of(el: ElementRef) -> String {
    visible_text(el)
}

fn first<'a>(root: ElementRef<'a>, s: &str) -> Option<ElementRef<'a>> {
    root.select(&sel(s)).next()
}

fn href_link(el: ElementRef) -> Option<PhpbbLink> {
    el.value().attr("href").and_then(phpbb_link)
}

/// Does this document look like a phpBB page?
pub fn detect(doc: &Html) -> bool {
    let root = doc.root_element();
    if first(root, "body#phpbb").is_some() {
        return true;
    }
    let has_phpbb_links = first(root, "a[href*='viewtopic.php'], a[href*='viewforum.php']").is_some();
    has_phpbb_links
        && (first(root, "div.post div.postbody, span.postbody, a.topictitle, a.maintitle, .topiclist, div.postbody").is_some()
            || doc.html().contains("phpBB"))
}

/// Parse one phpBB page. Returns `Ok(None)` for pages we don't archive.
pub fn parse(html: &str, url: &str) -> Result<Option<Page>> {
    let doc = Html::parse_document(html);
    let root = doc.root_element();
    let url = strip_wayback(url);
    let dir = script_dir(url);
    let script = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("").to_ascii_lowercase();

    let has_posts = first(root, "div.post[id^='p'], span.postbody, div.postbody").is_some();
    if script == "viewtopic.php" || (script.is_empty() && has_posts) {
        return Ok(parse_topic(root, url, &dir).map(Page::Thread));
    }
    if script == "viewforum.php" {
        return Ok(parse_forum(root, url, &dir).map(Page::Forum));
    }
    Ok(None)
}

fn topic_path(dir: &str, id: i64) -> String {
    format!("{dir}viewtopic.php?t={id}")
}

fn forum_path(dir: &str, id: i64) -> String {
    format!("{dir}viewforum.php?f={id}")
}

// ---------------------------------------------------------------- topics

fn parse_topic(root: ElementRef, url: &str, dir: &str) -> Option<ThreadPage> {
    // The topic id: from the title link, any link on the page that names it, or the URL.
    let title_link = first(root, "h2.topic-title a, h2 a[href*='viewtopic.php'], a.maintitle[href*='viewtopic.php']");
    let mut thread_id = title_link.and_then(href_link).and_then(|l| match l {
        PhpbbLink::Topic { id, .. } => Some(id),
        _ => None,
    });
    if thread_id.is_none() {
        if let Some(PhpbbLink::Topic { id, .. }) = phpbb_link(url) {
            thread_id = Some(id);
        }
    }
    if thread_id.is_none() {
        // posting.php?mode=reply&t=ID, print view links, pagination links...
        for a in root.select(&sel("a[href*='t=']")) {
            let href = a.value().attr("href").unwrap_or("");
            let script_ok = href.contains("viewtopic.php") || href.contains("posting.php") || href.contains("mode=reply");
            if !script_ok {
                continue;
            }
            let params = query_params(href);
            if let Some(id) = params.iter().rev().find(|(k, _)| k == "t").and_then(|(_, v)| v.parse().ok()) {
                thread_id = Some(id);
                break;
            }
        }
    }
    let thread_id = thread_id?;

    let title = title_link.map(text_of).filter(|t| !t.is_empty()).unwrap_or_else(|| {
        first(root, "title").map(text_of).map(|t| strip_title_site(&t)).unwrap_or_default()
    });

    let (page, total_pages) = pagination(root, url);
    if page == 0 {
        return None; // can't tell which page of the topic this is
    }
    let forums = breadcrumbs(root, dir);

    let mut posts = Vec::new();
    // phpBB 3 (prosilver and derivatives): <div id="p123" class="post ...">
    for el in root.select(&sel("div.post")) {
        if let Some(p) = parse_post_v3(el) {
            if !posts.iter().any(|x: &Post| x.id == p.id) {
                posts.push(p);
            }
        }
    }
    // phpBB 2 and table themes (subSilver, subsilver2).
    if posts.is_empty() {
        posts = parse_posts_tables(root);
    }
    // subsilver2-style custom themes that split each post over two rows (HBGames' "aqua"/"hb").
    if posts.is_empty() {
        posts = parse_posts_split_rows(root);
    }
    if posts.is_empty() {
        return None;
    }

    Some(ThreadPage { thread_id, title, path: topic_path(dir, thread_id), forums, page, total_pages, posts })
}

/// "Board name • View topic - Title" / "Board :: View topic - Title" → "Title".
fn strip_title_site(t: &str) -> String {
    for sep in ["View topic - ", "Topic - ", " :: "] {
        if let Some((_, rest)) = t.rsplit_once(sep) {
            return rest.trim().to_string();
        }
    }
    t.trim().to_string()
}

fn digits(s: &str) -> Option<i64> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}

fn member_of(root: ElementRef) -> Option<(Option<i64>, String)> {
    for a in root.select(&sel("a[href*='mode=viewprofile']")) {
        if let Some(PhpbbLink::Member { id }) = href_link(a) {
            let name = text_of(a);
            if !name.is_empty() {
                return Some((Some(id), name));
            }
        }
    }
    None
}

fn parse_post_v3(el: ElementRef) -> Option<Post> {
    let id = el.value().attr("id").filter(|v| v.starts_with('p')).and_then(digits)?;
    let content = first(el, "div.content")?;

    let author_line = first(el, "p.author");
    let author = first(el, ".postprofile a.username, .postprofile a.username-coloured")
        .and_then(|a| href_link(a).map(|l| (l, text_of(a))))
        .and_then(|(l, name)| match l {
            PhpbbLink::Member { id } => Some((Some(id), name)),
            _ => None,
        })
        .or_else(|| author_line.and_then(member_of))
        .or_else(|| first(el, ".postprofile").and_then(member_of))
        .or_else(|| {
            // Guests: <strong><span class="username">Name</span></strong> or plain text.
            first(el, "p.author .username, p.author strong, .postprofile dt .username, .postprofile dt strong")
                .map(|s| (None, text_of(s)))
        })
        .filter(|(_, n)| !n.is_empty())
        .map(|(id, name)| User { id, name })
        .unwrap_or_else(|| User { id: None, name: "Guest".into() });

    let posted_at = author_line.and_then(|p| {
        first(p, "time[datetime]")
            .and_then(|t| t.value().attr("datetime"))
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&Utc).to_rfc3339())
            .or_else(|| {
                let text = text_of(p);
                let after = text.rsplit_once('»').map(|x| x.1).unwrap_or(&text);
                parse_date(after)
            })
    });

    Some(Post {
        id,
        author,
        posted_at,
        body_html: content.inner_html().trim().to_string(),
        body_text: text_of(content),
        attachments: attachments(el),
    })
}

/// phpBB 2 / table themes: each post is a table row whose first cell holds the
/// author (`span.name` with `<a name="ID">`) and whose second cell holds the body.
fn parse_posts_tables(root: ElementRef) -> Vec<Post> {
    let mut posts = Vec::new();
    let tr_sel = sel("tr");
    for row in root.select(&tr_sel) {
        let cells: Vec<ElementRef> = row.children().filter_map(ElementRef::wrap).filter(|c| c.value().name() == "td").collect();
        if cells.len() < 2 {
            continue;
        }
        let Some(name) = first(cells[0], "span.name, b.postauthor, .postauthor") else { continue };
        // Post id: <a name="123"> (phpBB 2), <a name="p123"> (subsilver2), or the post link.
        let id = first(cells[0], "a[name]")
            .or_else(|| first(row, "a[name]"))
            .and_then(|a| a.value().attr("name"))
            .and_then(digits)
            .or_else(|| {
                row.select(&sel("a[href*='viewtopic.php']"))
                    .filter_map(href_link)
                    .find_map(|l| if let PhpbbLink::Post { id } = l { Some(id) } else { None })
            });
        let Some(id) = id else { continue };
        let Some(body) = first(cells[1], "span.postbody, div.postbody") else { continue };
        if posts.iter().any(|p: &Post| p.id == id) {
            continue;
        }

        let mut author_name = text_of(name);
        let mut author_id = None;
        if let Some(a) = first(name, "a[href]") {
            if let Some(PhpbbLink::Member { id }) = href_link(a) {
                author_id = Some(id);
                author_name = text_of(a);
            }
        }
        if author_id.is_none() {
            // phpBB 2 puts the profile button in the next row.
            let next_row = row.next_siblings().filter_map(ElementRef::wrap).find(|e| e.value().name() == "tr");
            if let Some((id, _)) = next_row.and_then(|r| {
                r.select(&sel("a[href*='mode=viewprofile']")).filter_map(href_link).find_map(|l| match l {
                    PhpbbLink::Member { id } => Some((id, ())),
                    _ => None,
                })
            }) {
                author_id = Some(id);
            }
        }

        let posted_at = cells[1]
            .select(&sel(".postdetails, .postdate, div.gensmall, span.gensmall"))
            .map(text_of)
            .find_map(|t| {
                let t = t.split("Post subject").next().unwrap_or(&t).to_string();
                let t = t.trim_start_matches("Posted:").trim_start_matches("Posted").trim();
                parse_date(t)
            });

        // phpBB 2 closes the body span around quotes and code boxes (they are tables),
        // so the message is the whole cell that holds the first body span.
        let holder = body
            .parent()
            .and_then(ElementRef::wrap)
            .filter(|p| p.value().name() == "td")
            .unwrap_or(body);
        let mut html = holder.inner_html();
        // The signature follows a line of underscores; "Last edited by" notes follow the body.
        for cut in ["_________________", "<span class=\"gensmall\">Last edited by", "<span class=\"edit\">"] {
            if let Some(i) = html.find(cut) {
                html.truncate(i);
            }
        }
        let html = html.trim_end().trim_end_matches("<br>").trim_end_matches("<br />").to_string();
        let text = visible_text(Html::parse_fragment(&html).root_element());
        posts.push(Post {
            id,
            author: User { id: author_id, name: if author_name.is_empty() { "Guest".into() } else { author_name } },
            posted_at,
            body_html: html.trim().to_string(),
            body_text: text,
            attachments: attachments(cells[1]),
        });
    }
    posts
}

/// Table themes where one row holds the post anchor (`<a name="p123">`), author and date,
/// and the next row holds the profile column and the message (`div.postbody`; a second
/// `div.postbody` in the same cell is the signature).
fn parse_posts_split_rows(root: ElementRef) -> Vec<Post> {
    let mut posts = Vec::new();
    for anchor in root.select(&sel("a[name^='p']")) {
        let Some(id) = anchor.value().attr("name").and_then(|n| n.strip_prefix('p')).and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };
        if posts.iter().any(|p: &Post| p.id == id) {
            continue;
        }
        // The header row: the nearest <tr> around the anchor that is followed by another row.
        let Some(head) = anchor
            .ancestors()
            .filter_map(ElementRef::wrap)
            .filter(|e| e.value().name() == "tr")
            .find(|tr| tr.next_siblings().filter_map(ElementRef::wrap).any(|e| e.value().name() == "tr"))
        else {
            continue;
        };
        let Some(body_row) = head.next_siblings().filter_map(ElementRef::wrap).find(|e| e.value().name() == "tr") else {
            continue;
        };
        let Some(body) = first(body_row, "div.postbody, span.postbody") else { continue };

        let author = head
            .select(&sel("a[href*='mode=viewprofile']"))
            .find_map(|a| match href_link(a) {
                Some(PhpbbLink::Member { id }) => Some(User { id: Some(id), name: text_of(a) }),
                _ => None,
            })
            .or_else(|| {
                // Guests: the bold name next to the anchor.
                anchor.parent().and_then(ElementRef::wrap).and_then(|cell| first(cell, "b")).map(|b| User { id: None, name: text_of(b) })
            })
            .filter(|u| !u.name.is_empty())
            .unwrap_or_else(|| User { id: None, name: "Guest".into() });

        let posted_at = head.select(&sel("span.genmed, .postdetails, span.gensmall, td")).map(text_of).find_map(|t| {
            let t = t.split("Post subject").next().unwrap_or(&t).to_string();
            let t = t.trim_start_matches("Posted:").trim_start_matches("Posted").trim();
            parse_date(t)
        });

        let html = body.inner_html().trim().to_string();
        let text = text_of(body);
        posts.push(Post { id, author, posted_at, body_html: html, body_text: text, attachments: attachments(body_row) });
    }
    posts
}

fn attachments(el: ElementRef) -> Vec<Attachment> {
    let mut out = Vec::new();
    for a in el.select(&sel("a[href*='download/file.php'], a[href*='download.php?id='], img[src*='download/file.php']")) {
        let Some(raw) = a.value().attr("href").or_else(|| a.value().attr("src")) else { continue };
        let params = query_params(raw);
        if params.iter().any(|(k, _)| k == "avatar") {
            continue;
        }
        let Some(id) = params.iter().find(|(k, _)| k == "id").and_then(|(_, v)| v.parse::<i64>().ok()) else { continue };
        let script = if raw.contains("download.php") && !raw.contains("download/file.php") { "download.php" } else { "download/file.php" };
        let dir_part = raw.split(script).next().unwrap_or("").trim_start_matches('.');
        let dir = if dir_part.starts_with("http") { crate::urls::normalize_path(dir_part) } else { dir_part.to_string() };
        let dir = if dir.ends_with('/') { dir } else { format!("{dir}/") };
        let dir = if dir.starts_with('/') { dir } else { format!("/{dir}") };
        let path = format!("{dir}{script}?id={id}");
        let filename = if a.value().name() == "img" {
            a.value().attr("alt").map(str::to_string).unwrap_or_default()
        } else {
            text_of(a)
        };
        let filename = if filename.is_empty() { format!("attachment-{id}") } else { filename };
        if !out.iter().any(|x: &Attachment| x.id == Some(id)) {
            out.push(Attachment { id: Some(id), filename, path });
        }
    }
    out
}

/// Forum dates (phpBB, SMF) are free-form (each board picks its format) in board-local time.
/// Recognise the common English ones; store them without a timezone.
pub fn parse_date(s: &str) -> Option<String> {
    let s = s.replace('\u{a0}', " ");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_matches(|c: char| c == ',' || c == '-' || c.is_whitespace());
    const FORMATS: &[&str] = &[
        "%a %b %d, %Y %I:%M %p",
        // SMF defaults ("May 14, 2014, 09:10:32 PM") and common variants
        "%B %d, %Y, %I:%M:%S %p",
        "%B %d, %Y, %H:%M:%S",
        "%b %d, %Y, %I:%M:%S %p",
        "%b %d, %Y, %H:%M:%S",
        "%d %B %Y, %H:%M:%S",
        "%d %b %Y, %H:%M:%S",
        "%d %B %Y, %I:%M:%S %p",
        "%Y-%m-%d, %H:%M:%S",
        "%a %b %d, %Y %H:%M",
        "%a %b %d, %Y %I:%M:%S %p",
        "%b %d, %Y %I:%M %p",
        "%b %d, %Y %H:%M",
        "%d %b %Y %H:%M",
        "%d %b %Y, %H:%M",
        "%d %b %Y %I:%M %p",
        "%a %d %b, %Y %H:%M",
        "%a %d %b %Y %H:%M",
        "%a %d %b %Y, %H:%M",
        "%d %B %Y, %H:%M",
        "%d %B %Y %H:%M",
        "%B %d, %Y %I:%M %p",
        "%B %d, %Y, %I:%M %p",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%d.%m.%Y %H:%M",
        "%d/%m/%Y %H:%M",
        "%m/%d/%Y %I:%M %p",
    ];
    for f in FORMATS {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, f) {
            return Some(d.format("%Y-%m-%dT%H:%M:%S").to_string());
        }
    }
    // Date only ("Jun 3, 2005"): midnight.
    for f in ["%b %d, %Y", "%B %d, %Y", "%d %b %Y", "%d %B %Y", "%Y-%m-%d"] {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, f) {
            return Some(d.format("%Y-%m-%dT00:00:00").to_string());
        }
    }
    // Trailing junk (", edited 2 times in total", timezone names): try shorter prefixes.
    let words: Vec<&str> = s.split(' ').collect();
    for n in (3..words.len()).rev() {
        let cand = words[..n].join(" ");
        for f in FORMATS {
            if let Ok(d) = NaiveDateTime::parse_from_str(&cand, f) {
                return Some(d.format("%Y-%m-%dT%H:%M:%S").to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------- shared bits

/// (current page, total pages). phpBB shows "Page X of Y"; otherwise use the
/// highlighted number among the page links.
fn pagination(root: ElementRef, url: &str) -> (u32, u32) {
    let mut page = 0;
    let mut total = 0;
    for el in root.select(&sel(".pagination, .pagination span.sr-only, td.nav, span.nav, span.gensmall, .gensmall")) {
        let t = text_of(el);
        if let Some((p, n)) = page_of(&t) {
            page = p;
            total = total.max(n);
            break;
        }
    }
    if page == 0 {
        // prosilver 3.1+: <li class="active"><span>2</span></li>
        if let Some(n) = first(root, ".pagination li.active span").and_then(|e| text_of(e).parse().ok()) {
            page = n;
        }
        // phpBB 2: "Goto page <b>2</b>, <a>1</a>, <a>3</a>"
        for el in root.select(&sel("span.gensmall, span.nav")) {
            let t = text_of(el);
            if !t.contains("Goto page") {
                continue;
            }
            if page == 0 {
                if let Some(b) = el.select(&sel("b b, b > b, strong")).next() {
                    page = text_of(b).parse().unwrap_or(0);
                }
            }
            for part in t.split(|c: char| !c.is_ascii_digit()) {
                if let Ok(n) = part.parse::<u32>() {
                    total = total.max(n);
                }
            }
        }
        for a in root.select(&sel(".pagination a, .pagination li span")) {
            if let Ok(n) = text_of(a).parse::<u32>() {
                total = total.max(n);
            }
        }
    }
    if page == 0 {
        // phpBB 3.0 in any language: <a onclick="jumpto()">Page <strong>2</strong> of <strong>5</strong></a>
        if let Some(a) = first(root, ".pagination a[onclick*='jumpto']") {
            let nums: Vec<u32> = a.select(&sel("strong")).filter_map(|s| text_of(s).parse().ok()).collect();
            if let [p, n] = nums[..] {
                return (p, n.max(p));
            }
        }
    }
    if page == 0 && total > 1 {
        return (0, 0); // several pages, but we can't tell which one this is
    }
    if page == 0 {
        // No page info at all: a start offset other than 0 still means "not page 1".
        let start = phpbb_link(url).map(|l| match l {
            PhpbbLink::Topic { start, .. } | PhpbbLink::Forum { start, .. } => start,
            _ => 0,
        });
        page = if start.unwrap_or(0) == 0 { 1 } else { 0 };
    }
    if page == 0 {
        return (0, 0);
    }
    (page, total.max(page))
}

/// "Page 3 of 12" (also "Page <strong>3</strong> of <strong>12</strong>" once text-flattened).
fn page_of(t: &str) -> Option<(u32, u32)> {
    let i = t.find("Page ")?;
    let rest = &t[i + 5..];
    let mut it = rest.split_whitespace();
    let p: u32 = it.next()?.parse().ok()?;
    if it.next()? != "of" {
        return None;
    }
    let n: u32 = it.next()?.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok()?;
    Some((p, n))
}

/// Forum trail, outermost first. phpBB 3: `.navlinks` / `#nav-breadcrumbs`;
/// phpBB 2: `span.nav a.nav`.
fn breadcrumbs(root: ElementRef, dir: &str) -> Vec<Forum> {
    let mut out: Vec<Forum> = Vec::new();
    let containers = ["#nav-breadcrumbs", "ul.navlinks", ".navlinks", "p.breadcrumbs", "td.nav, span.nav", "#breadcrumbs", "#navstrip"];
    for c in containers {
        for cont in root.select(&sel(c)) {
            for a in cont.select(&sel("a[href*='viewforum.php']")) {
                let Some(PhpbbLink::Forum { id, .. }) = href_link(a) else { continue };
                let title = first(a, "[itemprop=name]").map(text_of).unwrap_or_else(|| text_of(a));
                if title.is_empty() || out.iter().any(|f| f.id == id) {
                    continue;
                }
                out.push(Forum { id, title, path: forum_path(dir, id), parent_id: None });
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------- forum listings

fn parse_forum(root: ElementRef, url: &str, dir: &str) -> Option<ForumPage> {
    let id = match phpbb_link(url) {
        Some(PhpbbLink::Forum { id, .. }) => id,
        _ => first(root, "h2.forum-title a, h2 a[href*='viewforum.php'], a.maintitle[href*='viewforum.php']")
            .and_then(href_link)
            .and_then(|l| if let PhpbbLink::Forum { id, .. } = l { Some(id) } else { None })?,
    };
    let mut parents = breadcrumbs(root, dir);
    let title = first(root, "h2.forum-title a, h2.forum-title, h2 a[href*='viewforum.php'], a.maintitle")
        .map(text_of)
        .filter(|t| !t.is_empty())
        .or_else(|| parents.iter().find(|f| f.id == id).map(|f| f.title.clone()))
        .unwrap_or_default();
    parents.retain(|f| f.id != id);
    let forum = Forum { id, title, path: forum_path(dir, id), parent_id: None };
    let (page, total_pages) = pagination(root, url);
    let (page, total_pages) = if page == 0 { (1, 1) } else { (page, total_pages) };

    let mut threads = Vec::new();
    for link in root.select(&sel("a.topictitle")) {
        let Some(PhpbbLink::Topic { id: tid, .. }) = href_link(link) else { continue };
        if threads.iter().any(|t: &ThreadSummary| t.id == tid) {
            continue;
        }
        // The topic starter: the first profile link in the same row after the title.
        let row = link
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|e| matches!(e.value().name(), "li" | "tr" | "dt" | "dl"));
        let author = row.and_then(|r| {
            let mut after = false;
            for e in r.descendants().filter_map(ElementRef::wrap) {
                if e.id() == link.id() {
                    after = true;
                    continue;
                }
                if after && e.value().name() == "a" {
                    if let Some(PhpbbLink::Member { .. }) = href_link(e) {
                        return Some(text_of(e));
                    }
                }
            }
            None
        });
        threads.push(ThreadSummary { id: tid, title: text_of(link), path: topic_path(dir, tid), author });
    }
    // "The forum you selected does not exist" and login walls: no name, no topics. Saving them
    // would replace a board's real name from an older capture with nothing.
    if forum.title.is_empty() && threads.is_empty() {
        return None;
    }
    Some(ForumPage { forum, parents, page, total_pages, threads })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(parse_date("Wed Jul 16, 2008 10:40 am").as_deref(), Some("2008-07-16T10:40:00"));
        assert_eq!(parse_date(" 31 May 2004 22:24 ").as_deref(), Some("2004-05-31T22:24:00"));
        assert_eq!(parse_date("Mon Jan 01, 2007 1:23 pm").as_deref(), Some("2007-01-01T13:23:00"));
        assert_eq!(parse_date("Sun Jan 08, 2017 10:14 am Post subject").as_deref(), Some("2017-01-08T10:14:00"));
        assert_eq!(parse_date("yesterday"), None);
        assert_eq!(parse_date("Jun 3, 2005").as_deref(), Some("2005-06-03T00:00:00"));
    }

    #[test]
    fn pages() {
        assert_eq!(page_of("144 posts • Page 10 of 10 • 1 ... 6, 7"), Some((10, 10)));
        assert_eq!(page_of("Page 1 of 25"), Some((1, 25)));
        assert_eq!(page_of("Pages: 3"), None);
    }
}
