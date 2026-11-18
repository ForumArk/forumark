//! Parser for XenForo forum pages, covering both XenForo 2 (2017+) and the
//! older XenForo 1 markup, since Wayback captures of long-lived forums mix both.

use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};
use scraper::{ElementRef, Html, Selector};

use crate::model::{Attachment, Forum, ForumPage, Page, Post, ThreadPage, ThreadSummary, User};
use crate::urls::{base_path, id_after, normalize_path, page_number};

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

/// Collapse all whitespace runs into single spaces.
pub fn clean_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    let joined: String = parts.collect::<Vec<_>>().join(" ");
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_of(el: ElementRef) -> String {
    clean_text(el.text())
}

fn first<'a>(root: ElementRef<'a>, s: &str) -> Option<ElementRef<'a>> {
    root.select(&sel(s)).next()
}

/// Does this document look like a XenForo page?
pub fn detect(doc: &Html) -> bool {
    let root = doc.root_element();
    root.value().attr("data-app").is_some()
        || root.value().attr("data-template").is_some()
        || first(root, "#XenForo, html#XenForo, .p-pageWrapper, #pageDescription, .xenOverlay").is_some()
        || doc.html().contains("XenForo")
}

/// Parse one XenForo page. Returns `Ok(None)` for page types we don't archive
/// (member profiles, search results, error pages, ...).
pub fn parse(html: &str, url: &str) -> Result<Option<Page>> {
    let doc = Html::parse_document(html);
    let root = doc.root_element();
    let template = root.value().attr("data-template").unwrap_or("");
    let canonical = first(root, "link[rel=canonical]")
        .and_then(|e| e.value().attr("href"))
        .map(normalize_path);
    let path = canonical.clone().unwrap_or_else(|| normalize_path(url));

    // XenForo 2 declares the template on <html>; XenForo 1 puts it on #content.
    let xf1_content_class = first(root, "#content").and_then(|e| e.value().attr("class")).unwrap_or("");
    let is_thread = template == "thread_view" || xf1_content_class.contains("thread_view");
    let is_forum = template == "forum_view" || xf1_content_class.contains("forum_view");

    if is_thread {
        return Ok(parse_thread(root, &path, url).map(Page::Thread));
    }
    if is_forum {
        return Ok(parse_forum(root, &path, url).map(Page::Forum));
    }
    // Fallback: decide by URL and content.
    if path.contains("/threads/") && (first(root, "article.message--post").is_some() || first(root, "li.message").is_some()) {
        return Ok(parse_thread(root, &path, url).map(Page::Thread));
    }
    Ok(None)
}

// ---------------------------------------------------------------- threads

fn parse_thread(root: ElementRef, path: &str, url: &str) -> Option<ThreadPage> {
    let thread_id = id_after(path, "threads").or_else(|| id_after(&normalize_path(url), "threads"))?;
    let title = first(root, "h1.p-title-value")
        .or_else(|| first(root, ".titleBar h1"))
        .map(text_of)
        .unwrap_or_default();
    let thread_path = base_path(path, "threads").unwrap_or_else(|| format!("/threads/{thread_id}/"));

    let (page, total_pages) = pagination(root, path, url);
    let forums = breadcrumbs(root);

    let mut posts = Vec::new();
    for el in root.select(&sel("article.message--post")) {
        if let Some(p) = parse_post_xf2(el) {
            posts.push(p);
        }
    }
    if posts.is_empty() {
        for el in root.select(&sel("li.message")) {
            if let Some(p) = parse_post_xf1(el) {
                posts.push(p);
            }
        }
    }

    Some(ThreadPage { thread_id, title, path: thread_path, forums, page, total_pages, posts })
}

fn post_id_from_attrs(el: ElementRef) -> Option<i64> {
    let v = el.value();
    for attr in ["data-content", "id"] {
        if let Some(raw) = v.attr(attr) {
            if let Some(n) = raw.rsplit('-').next().and_then(|n| n.parse().ok()) {
                return Some(n);
            }
        }
    }
    None
}

fn user_from(el: ElementRef, fallback_name: Option<&str>) -> User {
    let link = first(el, "a.username");
    let id = link
        .and_then(|a| {
            a.value()
                .attr("data-user-id")
                .and_then(|s| s.parse().ok())
                .or_else(|| a.value().attr("href").and_then(|h| id_after(&normalize_path(h), "members")))
        });
    let name = fallback_name
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| link.map(text_of))
        .unwrap_or_else(|| "Guest".to_string());
    User { id, name }
}

fn attachments(el: ElementRef, item_sel: &str) -> Vec<Attachment> {
    let mut out = Vec::new();
    for li in el.select(&sel(item_sel)) {
        let Some(a) = li.select(&sel("a[href*='attachments/']")).next() else { continue };
        let Some(href) = a.value().attr("href") else { continue };
        let path = normalize_path(href);
        let filename = first(li, ".file-name")
            .map(|f| f.value().attr("title").map(str::to_string).unwrap_or_else(|| text_of(f)))
            .or_else(|| first(li, ".filename a, .filename").map(text_of))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.trim_end_matches('/').rsplit('/').next().unwrap_or("file").to_string());
        let id = id_after(&path, "attachments");
        if !out.iter().any(|x: &Attachment| x.path == path) {
            out.push(Attachment { id, filename, path });
        }
    }
    out
}

fn parse_post_xf2(el: ElementRef) -> Option<Post> {
    let id = post_id_from_attrs(el)?;
    let author = user_from(first(el, ".message-name, .message-user").unwrap_or(el), el.value().attr("data-author"));
    let posted_at = first(el, ".message-attribution-main time, time.u-dt")
        .and_then(|t| t.value().attr("datetime").map(normalize_datetime).or_else(|| unix_attr(t)));
    let body = first(el, "article.message-body .bbWrapper").or_else(|| first(el, ".message-body .bbWrapper"))?;
    Some(Post {
        id,
        author,
        posted_at,
        body_html: body.inner_html().trim().to_string(),
        body_text: text_of(body),
        attachments: attachments(el, ".attachmentList li"),
    })
}

fn parse_post_xf1(el: ElementRef) -> Option<Post> {
    // Only real posts: <li id="post-123" class="message" data-author="...">
    let raw_id = el.value().attr("id")?;
    let id: i64 = raw_id.strip_prefix("post-")?.parse().ok()?;
    let author = user_from(first(el, ".messageUserInfo, .userText").unwrap_or(el), el.value().attr("data-author"));
    let posted_at = first(el, ".messageMeta .DateTime, .DateTime").and_then(|t| {
        unix_attr(t).or_else(|| {
            t.value()
                .attr("title")
                .map(str::to_string)
                .or_else(|| Some(text_of(t)))
                .and_then(|s| parse_xf1_date(&s))
        })
    });
    let body = first(el, "blockquote.messageText")?;
    let mut html = body.inner_html();
    if let Some(i) = html.find("<div class=\"messageTextEndMarker\">") {
        html.truncate(i);
    }
    Some(Post {
        id,
        author,
        posted_at,
        body_html: html.trim().to_string(),
        body_text: text_of(body),
        attachments: attachments(el, ".attachedFiles li.attachment, .attachedFiles .attachment"),
    })
}

fn unix_attr(t: ElementRef) -> Option<String> {
    let secs: i64 = t.value().attr("data-time")?.parse().ok()?;
    DateTime::<Utc>::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
}

fn normalize_datetime(s: &str) -> String {
    // XenForo 2 writes e.g. 2020-01-01T10:00:00+0000; convert to RFC 3339 if possible.
    DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .map(|d| d.with_timezone(&Utc).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// XenForo 1 tooltip dates such as "Jan 5, 2015 at 3:04 PM" (forum-local time,
/// stored as-is without timezone).
fn parse_xf1_date(s: &str) -> Option<String> {
    let s = s.trim();
    NaiveDateTime::parse_from_str(s, "%b %e, %Y at %l:%M %p")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%b %d, %Y at %I:%M %p"))
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())
        .ok()
}

// ---------------------------------------------------------------- shared bits

fn pagination(root: ElementRef, path: &str, url: &str) -> (u32, u32) {
    let mut page = page_number(path).max(page_number(&normalize_path(url)));
    let mut total = page;

    // XenForo 2
    for li in root.select(&sel(".pageNav-page")) {
        if let Ok(n) = text_of(li).parse::<u32>() {
            total = total.max(n);
            if li.value().attr("class").unwrap_or("").contains("pageNav-page--current") {
                page = n;
            }
        }
    }
    // XenForo 1: <div class="PageNav" data-page="2" data-last="9">
    if let Some(nav) = first(root, ".PageNav") {
        if let Some(n) = nav.value().attr("data-last").and_then(|s| s.parse().ok()) {
            total = total.max(n);
        }
        if let Some(n) = nav.value().attr("data-page").and_then(|s| s.parse().ok()) {
            page = n;
        }
    }
    // XenForo 1 also encodes the page in `index.php?threads/x.1/page-2`.
    (page.max(1), total.max(page).max(1))
}

/// Forum/category trail from the breadcrumbs, outermost first, de-duplicated.
fn breadcrumbs(root: ElementRef) -> Vec<Forum> {
    let mut out: Vec<Forum> = Vec::new();
    for a in root.select(&sel(".p-breadcrumbs a, .breadcrumb a.crumb, .breadcrumb .crust a")) {
        let Some(href) = a.value().attr("href") else { continue };
        let href_norm = normalize_path(href);
        // XF2 links categories as `/forums/#name.12` or `/categories/name.12/`.
        let frag_id = href.rsplit_once('#').and_then(|(_, f)| f.rsplit('.').next()?.parse().ok());
        let (id, path) = if let Some(id) = id_after(&href_norm, "forums") {
            (id, base_path(&href_norm, "forums").unwrap_or(href_norm.clone()))
        } else if let Some(id) = id_after(&href_norm, "categories") {
            (id, base_path(&href_norm, "categories").unwrap_or(href_norm.clone()))
        } else if let Some(id) = frag_id {
            (id, format!("/categories/{}/", href.rsplit_once('#').map(|x| x.1).unwrap_or("")))
        } else {
            continue;
        };
        let title = first(a, "[itemprop=name]").map(text_of).unwrap_or_else(|| text_of(a));
        if title.is_empty() || out.iter().any(|f| f.id == id) {
            continue;
        }
        out.push(Forum { id, title, path, parent_id: None });
    }
    out
}

// ---------------------------------------------------------------- forum listings

fn parse_forum(root: ElementRef, path: &str, url: &str) -> Option<ForumPage> {
    let id = id_after(path, "forums").or_else(|| id_after(&normalize_path(url), "forums"))?;
    let title = first(root, "h1.p-title-value")
        .or_else(|| first(root, ".titleBar h1"))
        .map(text_of)
        .unwrap_or_default();
    let forum = Forum { id, title, path: base_path(path, "forums").unwrap_or_else(|| path.to_string()), parent_id: None };
    let mut parents = breadcrumbs(root);
    parents.retain(|f| f.id != id);
    let (page, total_pages) = pagination(root, path, url);

    let mut threads = Vec::new();
    // XenForo 2
    for item in root.select(&sel(".structItem--thread")) {
        let link = item
            .select(&sel(".structItem-title a[href*='threads/']"))
            .last();
        let Some(link) = link else { continue };
        let tpath = normalize_path(link.value().attr("href").unwrap_or(""));
        let Some(tid) = id_after(&tpath, "threads") else { continue };
        let author = item
            .value()
            .attr("data-author")
            .map(str::to_string)
            .or_else(|| first(item, ".structItem-minor .username, .username").map(text_of));
        push_summary(&mut threads, tid, text_of(link), &tpath, author);
    }
    // XenForo 1
    for item in root.select(&sel("li.discussionListItem")) {
        let Some(link) = item.select(&sel("h3.title a[href*='threads/']")).last() else { continue };
        let tpath = normalize_path(link.value().attr("href").unwrap_or(""));
        let Some(tid) = id_after(&tpath, "threads") else { continue };
        let author = item.value().attr("data-author").map(str::to_string);
        push_summary(&mut threads, tid, text_of(link), &tpath, author);
    }
    Some(ForumPage { forum, parents, page, total_pages, threads })
}

fn push_summary(out: &mut Vec<ThreadSummary>, id: i64, title: String, path: &str, author: Option<String>) {
    if out.iter().any(|t| t.id == id) {
        return;
    }
    let path = base_path(path, "threads").unwrap_or_else(|| path.to_string());
    out.push(ThreadSummary { id, title, path, author });
}
