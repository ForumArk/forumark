//! Parser for SMF (Simple Machines Forum) pages: SMF 1.x (table themes) and
//! SMF 2.0/2.1 (div themes), including heavily customised themes.
//!
//! Themes vary a lot, so posts are found by what every theme keeps: the message
//! body (`div.inner#msg_N` in 2.x, `div.post` in 1.x) plus, around it, the
//! author's profile link and the "on: <date>" line. Each body's "post box" is the
//! largest ancestor that holds no other post body.
//!
//! Stored paths keep SMF's own form: `/forum/index.php?topic=12.0`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use scraper::{ElementRef, Html, Selector};

use crate::model::{Attachment, Forum, ForumPage, Page, Post, ThreadPage, ThreadSummary, User};
use crate::parse::phpbb::parse_date;
use crate::parse::xenforo::clean_text;
use crate::urls::{SmfLink, smf_link, smf_params, strip_wayback};

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

fn text_of(el: ElementRef) -> String {
    clean_text(el.text())
}

fn first<'a>(root: ElementRef<'a>, s: &str) -> Option<ElementRef<'a>> {
    root.select(&sel(s)).next()
}

fn href_link(el: ElementRef) -> Option<SmfLink> {
    el.value().attr("href").and_then(smf_link)
}

/// Does this document look like an SMF page?
pub fn detect(doc: &Html) -> bool {
    let html = doc.html();
    html.contains("smf_scripturl")
        || html.contains("smf_theme_url")
        || html.contains("Simple Machines")
        || html.contains("Powered by SMF")
        || (html.contains("index.php?action=profile;u=") && (html.contains("?topic=") || html.contains("/topic,")))
}

/// Folder of the forum's `index.php`, e.g. `/forum/`.
fn board_dir(url: &str) -> String {
    let url = strip_wayback(url.trim());
    // Site-relative path, keeping everything up to the script name.
    let after_scheme = url.split_once("://").map_or(url, |x| x.1);
    let path = match after_scheme.find('/') {
        Some(i) if url.contains("://") => &after_scheme[i..],
        _ => url,
    };
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    match path.find("index.php") {
        Some(i) => path[..i].to_string(),
        None => match path.rfind('/') {
            Some(i) => path[..=i].to_string(),
            None => "/".into(),
        },
    }
}

fn topic_path(dir: &str, id: i64) -> String {
    format!("{dir}index.php?topic={id}.0")
}

fn board_path(dir: &str, id: i64) -> String {
    format!("{dir}index.php?board={id}.0")
}

/// Parse one SMF page. Returns `Ok(None)` for pages we don't archive.
pub fn parse(html: &str, url: &str) -> Result<Option<Page>> {
    let doc = Html::parse_document(html);
    let root = doc.root_element();
    let url = strip_wayback(url);
    let dir = board_dir(url);
    let params = smf_params(url).unwrap_or_default();
    let has = |k: &str| params.iter().any(|(key, _)| key == k);
    if has("action") {
        return Ok(None);
    }
    if has("topic") || has("msg") {
        return Ok(parse_topic(root, url, &dir).map(Page::Thread));
    }
    if has("board") {
        return Ok(parse_board(root, url, &dir).map(Page::Forum));
    }
    Ok(None)
}

// ---------------------------------------------------------------- topics

fn parse_topic(root: ElementRef, url: &str, dir: &str) -> Option<ThreadPage> {
    // Topic id: URL, then links that name the topic (print, quick-moderation form, breadcrumbs).
    let mut thread_id = match smf_link(url) {
        Some(SmfLink::Topic { id, .. }) => Some(id),
        _ => None,
    };
    let mut form_start = None;
    for el in root.select(&sel("form#quickModForm, form[name=quickModForm]")) {
        if let Some(SmfLink::Topic { id, start }) = el.value().attr("action").and_then(smf_link) {
            thread_id = thread_id.or(Some(id));
            form_start = Some(start);
        }
    }
    if thread_id.is_none() {
        for a in root.select(&sel("a[href*='printpage'], a[href*='topic=']")) {
            let href = a.value().attr("href").unwrap_or("");
            if let Some(v) = smf_params(href).and_then(|p| p.into_iter().find(|(k, _)| k == "topic")) {
                if let Some(id) = v.1.split('.').next().and_then(|n| n.parse().ok()) {
                    thread_id = Some(id);
                    break;
                }
            }
        }
    }
    let thread_id = thread_id?;

    let title = first(root, "#top_subject")
        .map(text_of)
        .map(|t| clean_title(&t))
        .filter(|t| !t.is_empty())
        .or_else(|| {
            crumb_links(root).into_iter().rev().find_map(|a| match href_link(a) {
                Some(SmfLink::Topic { id, .. }) if id == thread_id => Some(text_of(a)),
                _ => None,
            })
        })
        .unwrap_or_default();

    let (page, total_pages) = pagination(root, form_start);
    if page == 0 {
        return None;
    }
    let forums = breadcrumbs(root, dir);
    let posts = parse_posts(root);
    if posts.is_empty() {
        return None;
    }
    Some(ThreadPage { thread_id, title, path: topic_path(dir, thread_id), forums, page, total_pages, posts })
}

/// "Topic: Feedback on a system  (Read 436 times)" → "Feedback on a system".
fn clean_title(t: &str) -> String {
    let t = t.trim();
    let t = t.strip_prefix("Topic:").unwrap_or(t).trim();
    let t = match t.rfind("(Read ") {
        Some(i) if t.ends_with(')') => t[..i].trim(),
        _ => t,
    };
    t.to_string()
}

/// Post bodies on the page: `div.inner#msg_N` (2.x) or `div.post` (1.x).
fn bodies(root: ElementRef) -> Vec<ElementRef> {
    let v2: Vec<_> = root.select(&sel("div.inner[id^='msg_'], div.inner[data-msgid]")).collect();
    if !v2.is_empty() {
        return v2;
    }
    root.select(&sel("div.post")).filter(|d| first(*d, "div.post").is_none()).collect()
}

fn parse_posts(root: ElementRef) -> Vec<Post> {
    let bodies = bodies(root);
    let body_ids: Vec<_> = bodies.iter().map(|b| b.id()).collect();
    let mut posts = Vec::new();
    for body in &bodies {
        // The post box. SMF 2 wraps each post in `.post_wrapper`; otherwise climb while
        // the ancestor still holds only this body, but never past the posts form
        // (on a one-post page that would reach the page header and its links).
        let wrapper = body
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|a| a.value().attr("class").is_some_and(|c| c.split_whitespace().any(|c| c == "post_wrapper")));
        let container = wrapper.unwrap_or_else(|| {
            let mut container = *body;
            for anc in body.ancestors().filter_map(ElementRef::wrap) {
                let is_limit = anc.value().attr("id").is_some_and(|i| i == "quickModForm" || i == "forumposts")
                    || anc.value().attr("name") == Some("quickModForm");
                if is_limit || anc.descendants().filter(|n| body_ids.contains(&n.id())).count() > 1 {
                    break;
                }
                container = anc;
            }
            container
        });

        let id = body
            .value()
            .attr("id")
            .and_then(|v| v.strip_prefix("msg_"))
            .and_then(|n| n.parse().ok())
            .or_else(|| body.value().attr("data-msgid").and_then(|n| n.parse().ok()))
            .or_else(|| {
                first(container, "[id^='subject_']")
                    .and_then(|e| e.value().attr("id"))
                    .and_then(|v| v.strip_prefix("subject_"))
                    .and_then(|n| n.parse().ok())
            })
            .or_else(|| {
                container.select(&sel("a[href]")).filter_map(href_link).find_map(|l| match l {
                    SmfLink::Post { id } => Some(id),
                    _ => None,
                })
            });
        let Some(id) = id else { continue };
        if posts.iter().any(|p: &Post| p.id == id) {
            continue;
        }

        // Author: the first profile link in the box that isn't inside the message itself.
        let in_body = |e: &ElementRef| e.ancestors().any(|a| a.id() == body.id());
        let author = container
            .select(&sel("a[href*='action=profile']"))
            .filter(|a| !in_body(a))
            .find_map(|a| match href_link(a) {
                Some(SmfLink::Member { id }) => {
                    let name = text_of(a);
                    (!name.is_empty()).then_some(User { id: Some(id), name })
                }
                _ => None,
            })
            .or_else(|| {
                // Guests: SMF 2 shows the name in the poster block without a link.
                first(container, ".poster h4, .poster b, td b")
                    .map(text_of)
                    .filter(|n| !n.is_empty())
                    .map(|name| User { id: None, name })
            })
            .unwrap_or_else(|| User { id: None, name: "Guest".into() });

        let posted_at = first(container, "time[datetime]")
            .filter(|t| !in_body(t))
            .and_then(|t| t.value().attr("datetime"))
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&Utc).to_rfc3339())
            .or_else(|| {
                container
                    .select(&sel(".postinfo a.smalltext, .keyinfo .smalltext, div.smalltext, td.smalltext, span.smalltext, .keyinfo a[id^='subject_']"))
                    .filter(|e| !in_body(e))
                    .find_map(|e| date_from(&text_of(e)))
            });

        let html = body.inner_html();
        posts.push(Post {
            id,
            author,
            posted_at,
            body_html: html.trim().to_string(),
            body_text: text_of(*body),
            attachments: attachments(container),
        });
    }
    posts
}

/// "« Reply #3 on: May 14, 2014, 09:10:32 PM »" → the date.
fn date_from(t: &str) -> Option<String> {
    let t = t.replace(['«', '»'], " ");
    // Some themes write "#1 • Jun 3, 2005".
    let t = t.rsplit('•').next().unwrap_or(&t).to_string();
    let after = match t.find("on:") {
        Some(i) => &t[i + 3..],
        None => t.as_str(),
    };
    let after = after.split("Last Edit").next().unwrap_or(after);
    parse_date(after.trim())
}

fn attachments(container: ElementRef) -> Vec<Attachment> {
    let mut out = Vec::new();
    for a in container.select(&sel("a[href*='action=dlattach']")) {
        let Some(href) = a.value().attr("href") else { continue };
        let Some(params) = smf_params(href) else { continue };
        let Some(id) = params.iter().find(|(k, _)| k == "attach").and_then(|(_, v)| v.parse::<i64>().ok()) else {
            continue;
        };
        if out.iter().any(|x: &Attachment| x.id == Some(id)) {
            continue;
        }
        let dir = board_dir(href);
        let name = text_of(a);
        let filename = if name.is_empty() { format!("attachment-{id}") } else { name };
        out.push(Attachment { id: Some(id), filename, path: format!("{dir}index.php?action=dlattach;attach={id}") });
    }
    out
}

// ---------------------------------------------------------------- shared bits

/// Breadcrumb links: SMF 2.1 `.navigate_section`, 2.0 `#linktree`, 1.x `div.nav`.
fn crumb_links(root: ElementRef) -> Vec<ElementRef> {
    for c in [".navigate_section a", "#linktree a", ".linktree a", "div.nav a.nav", "div.nav a"] {
        let v: Vec<_> = root.select(&sel(c)).collect();
        if !v.is_empty() {
            return v;
        }
    }
    Vec::new()
}

fn breadcrumbs(root: ElementRef, dir: &str) -> Vec<Forum> {
    let mut out: Vec<Forum> = Vec::new();
    for a in crumb_links(root) {
        let Some(SmfLink::Board { id, .. }) = href_link(a) else { continue };
        let title = text_of(a);
        if title.is_empty() || out.iter().any(|f| f.id == id) {
            continue;
        }
        out.push(Forum { id, title, path: board_path(dir, id), parent_id: None });
    }
    out
}

/// (current page, total pages). SMF writes "Pages: [1] 2 3" (1.x), bold or
/// `strong` in `.pagelinks` (2.0) or `span.current_page` (2.1).
fn pagination(root: ElementRef, form_start: Option<u32>) -> (u32, u32) {
    let areas: Vec<ElementRef> = root
        .select(&sel(".pagelinks, .pagesection, td.middletext, td.smalltext, div.middletext, .pages"))
        .filter(|e| {
            let t = text_of(*e);
            t.contains("Pages") || t.contains('[') || first(*e, ".current_page, .navPages, a.nav_page").is_some()
        })
        .collect();
    let mut page = 0;
    let mut total = 0;
    for area in &areas {
        if page == 0 {
            if let Some(n) = first(*area, "span.current_page, strong, b").and_then(|e| text_of(e).parse::<u32>().ok()) {
                page = n;
            }
        }
        for a in area.select(&sel("a, strong, b, span.current_page")) {
            if let Ok(n) = text_of(a).parse::<u32>() {
                total = total.max(n);
            }
        }
    }
    if page == 0 {
        // No page links at all: a one-page topic, unless the form says we're further in.
        return if form_start.unwrap_or(0) == 0 && total <= 1 { (1, 1) } else { (0, 0) };
    }
    (page, total.max(page))
}

// ---------------------------------------------------------------- board listings

fn parse_board(root: ElementRef, url: &str, dir: &str) -> Option<ForumPage> {
    let Some(SmfLink::Board { id, .. }) = smf_link(url) else { return None };
    let mut parents = breadcrumbs(root, dir);
    // A real board page names itself in the breadcrumbs; "off limits" / login pages don't.
    let title = parents.iter().find(|f| f.id == id).map(|f| f.title.clone())?;
    parents.retain(|f| f.id != id);
    let forum = Forum { id, title, path: board_path(dir, id), parent_id: None };
    let (page, total_pages) = match pagination(root, None) {
        (0, _) => (1, 1),
        p => p,
    };

    let mut threads = Vec::new();
    // Topic titles sit in <span id="msg_N"><a href="...topic=ID.0">Title</a></span>.
    for link in root.select(&sel("span[id^='msg_'] a, td.subject a, .subject a, .info a")) {
        let Some(SmfLink::Topic { id: tid, start: 0 }) = href_link(link) else { continue };
        if threads.iter().any(|t: &ThreadSummary| t.id == tid) {
            continue;
        }
        let title = text_of(link);
        if title.is_empty() || title.chars().all(|c| c.is_ascii_digit()) {
            continue; // page-number links next to a title
        }
        let row = link
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|e| e.value().name() == "tr" || e.value().attr("class").is_some_and(|c| c.contains("windowbg")));
        let author = row.and_then(|r| {
            let mut after = false;
            for e in r.descendants().filter_map(ElementRef::wrap) {
                if e.id() == link.id() {
                    after = true;
                    continue;
                }
                if after && e.value().name() == "a" {
                    if let Some(SmfLink::Member { .. }) = href_link(e) {
                        return Some(text_of(e));
                    }
                }
            }
            None
        });
        threads.push(ThreadSummary { id: tid, title, path: topic_path(dir, tid), author });
    }
    Some(ForumPage { forum, parents, page, total_pages, threads })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_dates() {
        assert_eq!(clean_title("Topic: Feedback on a gun reload system.  (Read 436 times)"), "Feedback on a gun reload system.");
        assert_eq!(date_from("« on: August 12, 2007, 02:36:31 PM »").as_deref(), Some("2007-08-12T14:36:31"));
        assert_eq!(date_from("« Reply #1 on: May 14, 2014, 09:10:32 PM »").as_deref(), Some("2014-05-14T21:10:32"));
        assert_eq!(date_from("May 14, 2014, 09:10:32 PM").as_deref(), Some("2014-05-14T21:10:32"));
        assert_eq!(date_from("#1 • Jun 3, 2005").as_deref(), Some("2005-06-03T00:00:00"));
    }
}
