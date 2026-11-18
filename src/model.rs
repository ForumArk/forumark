//! Normalized forum data model, independent of the forum software or the source
//! the pages came from (live crawl, WARC, Wayback Machine, ...).

use serde::{Deserialize, Serialize};

/// A board/sub-forum or category, as seen in breadcrumbs or forum listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forum {
    pub id: i64,
    pub title: String,
    /// Canonical path, e.g. `/forums/plugin-releases.12/`.
    pub path: String,
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: Option<i64>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Option<i64>,
    pub filename: String,
    /// Path of the attachment on the original forum (e.g. `/attachments/foo-png.123/`).
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Post {
    pub id: i64,
    pub author: User,
    /// RFC 3339 timestamp when known.
    pub posted_at: Option<String>,
    /// Inner HTML of the post body exactly as captured (sanitized at build time).
    pub body_html: String,
    /// Plain text of the body, used for search and previews.
    pub body_text: String,
    pub attachments: Vec<Attachment>,
}

/// A thread summary as found on a forum listing page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: i64,
    pub title: String,
    pub path: String,
    pub author: Option<String>,
}

/// Everything extracted from one page of a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPage {
    pub thread_id: i64,
    pub title: String,
    pub path: String,
    /// Breadcrumb trail, outermost first. The last entry is the thread's own forum.
    pub forums: Vec<Forum>,
    pub page: u32,
    pub total_pages: u32,
    pub posts: Vec<Post>,
}

/// Everything extracted from one page of a forum (board) listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPage {
    pub forum: Forum,
    pub parents: Vec<Forum>,
    pub page: u32,
    pub total_pages: u32,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    Thread(ThreadPage),
    Forum(ForumPage),
}

/// Where a page came from. Used for provenance and to prefer newer captures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    /// Original URL of the page on the forum.
    pub url: String,
    /// Capture time as `YYYYMMDDhhmmss` (Wayback style), lexically sortable.
    pub captured_at: String,
    /// Free-form source label, e.g. `file`, `wayback`, `warc:<name>`.
    pub source: String,
}
