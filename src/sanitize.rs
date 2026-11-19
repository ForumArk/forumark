//! Make captured post HTML safe to publish and keep its links working.
//!
//! * scripts, styles, event handlers and other dangerous markup are removed (ammonia)
//! * Wayback Machine prefixes are stripped from links
//! * links to threads/forums/members of the archived forum point into the archive
//! * other links to the original forum and images go through the Wayback Machine,
//!   which keeps serving them after the forum is gone

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::embeds::embeds_to_links;
use crate::tidy::{smilies_to_emoji, staff_notes};

use crate::urls::{PhpbbLink, SmfLink, id_after, normalize_path, page_number, phpbb_link, smf_link, strip_wayback};

/// Archive href for a route/id/page, e.g. `/rm/threads/4242/page-2/`.
pub fn local_href(base: &str, route: &str, id: i64, page: u32) -> String {
    if page > 1 { format!("{base}{route}/{id}/page-{page}/") } else { format!("{base}{route}/{id}/") }
}

#[derive(Debug, Clone)]
pub struct LinkContext {
    /// Original forum origin, e.g. `https://forums.rpgmakerweb.com` (no trailing slash).
    pub origin: String,
    /// Prefix the archive is served under, e.g. `/` or `/rpgmaker/`.
    pub base: String,
    /// Pages that exist in the built archive. When set, links to anything else
    /// (a thread the Wayback Machine never saved, a deleted member, ...) go to
    /// the Wayback Machine instead of a missing archive page.
    pub known: Option<Arc<KnownPages>>,
    /// Folder the forum software lives in (`/` or e.g. `/phpBB2/`), for relative
    /// phpBB links such as `./viewtopic.php?t=1`.
    pub board_dir: String,
}

/// What the site builder generates pages for.
#[derive(Debug, Clone, Default)]
pub struct KnownPages {
    /// Thread id -> number of pages built.
    pub threads: HashMap<i64, u32>,
    /// Forum or category id -> its route (`forums` or `categories`).
    pub forums: HashMap<i64, &'static str>,
    pub members: HashSet<i64>,
    /// Post id -> (thread id, page), so links to a single post open the right page.
    pub posts: HashMap<i64, (i64, u32)>,
    /// Posts per thread page on the original forum (0 = unknown). phpBB links say
    /// `start=30` instead of a page number.
    pub per_page: u32,
}

impl LinkContext {
    fn host(&self) -> &str {
        self.origin.split("://").nth(1).unwrap_or(&self.origin).trim_end_matches('/')
    }

    /// Is this (Wayback-stripped) URL on the archived forum?
    fn is_forum_url(&self, url: &str) -> bool {
        let rest = url.split("://").nth(1).unwrap_or("");
        let host = rest.split('/').next().unwrap_or("");
        let h = self.host();
        host == h || host == format!("www.{h}") || h == format!("www.{host}")
    }

    /// Archive-local href for a forum path, if the archive has a page for it.
    ///
    /// Archive pages live at ID-only paths (`threads/4242/`, `members/777/`), so
    /// links keep working even when a thread was renamed and its slug changed.
    /// Post permalinks (`/threads/x.1/post-9`) go to the thread's first page.
    pub fn local(&self, path: &str) -> Option<String> {
        let known = self.known.as_deref();
        // XenForo post links: `/posts/9/`, `/threads/x.1/post-9`, `/goto/post?id=9`.
        let post_id = id_after(path, "posts").or_else(|| {
            path.split('/').find_map(|seg| seg.strip_prefix("post-").and_then(|n| n.parse().ok()))
        });
        if let Some(pid) = post_id {
            if let Some(href) = self.post_href(pid) {
                return Some(href);
            }
        }
        if let Some(id) = id_after(path, "threads") {
            let page = page_number(path);
            if let Some(k) = known {
                match k.threads.get(&id) {
                    Some(&pages) if page <= pages => {}
                    _ => return None,
                }
            }
            return Some(local_href(&self.base, "threads", id, page));
        }
        for route in ["forums", "categories"] {
            if let Some(id) = id_after(path, route) {
                // Listing pages are numbered differently in the archive, so always open page 1.
                let route = match known {
                    Some(k) => *k.forums.get(&id)?,
                    None => route,
                };
                return Some(local_href(&self.base, route, id, 1));
            }
        }
        if let Some(id) = id_after(path, "members") {
            if known.is_some_and(|k| !k.members.contains(&id)) {
                return None;
            }
            return Some(local_href(&self.base, "members", id, 1));
        }
        None
    }

    /// phpBB posts often link relatively (`./viewtopic.php?t=1`, `viewtopic.php?p=2`).
    /// Those resolve against the board folder, which the stored path doesn't know
    /// here, but the script name is enough to recognise and map them.
    fn relative_board_link(&self, v: &str) -> Option<String> {
        let rel = v.strip_prefix("./").unwrap_or(v);
        if rel.contains("://") || rel.starts_with('#') || rel.starts_with("mailto:") {
            return None;
        }
        phpbb_link(rel)?;
        Some(self.original(&format!("{}{rel}", self.board_dir)))
    }

    /// Archive link to one post (its page plus `#post-N`), if the archive has it.
    fn post_href(&self, post: i64) -> Option<String> {
        let &(thread, page) = self.known.as_deref()?.posts.get(&post)?;
        Some(format!("{}#post-{post}", local_href(&self.base, "threads", thread, page)))
    }

    /// Archive-local href for a phpBB link, if the archive has that page.
    fn local_phpbb(&self, link: PhpbbLink) -> Option<String> {
        let known = self.known.as_deref();
        match link {
            PhpbbLink::Topic { id, start } => {
                let per_page = known.map_or(0, |k| k.per_page);
                let page = if start > 0 && per_page > 0 { start / per_page + 1 } else { 1 };
                if let Some(k) = known {
                    match k.threads.get(&id) {
                        Some(&pages) if page <= pages => {}
                        Some(_) => return Some(local_href(&self.base, "threads", id, 1)),
                        None => return None,
                    }
                }
                Some(local_href(&self.base, "threads", id, page))
            }
            PhpbbLink::Post { id } => self.post_href(id),
            PhpbbLink::Forum { id, .. } => {
                let route = match known {
                    Some(k) => *k.forums.get(&id)?,
                    None => "forums",
                };
                Some(local_href(&self.base, route, id, 1))
            }
            PhpbbLink::Member { id } => {
                if known.is_some_and(|k| !k.members.contains(&id)) {
                    return None;
                }
                Some(local_href(&self.base, "members", id, 1))
            }
        }
    }

    /// Original-forum URL of page `page` of a thread whose page-1 URL is `thread_url`.
    pub fn original_page(&self, thread_url: &str, page: u32) -> String {
        if page <= 1 {
            return thread_url.to_string();
        }
        // SMF: index.php?topic=12.0 → index.php?topic=12.30
        if let Some(base) = thread_url.strip_suffix(".0").filter(|b| b.contains("?topic=")) {
            let per_page = self.known.as_deref().map_or(0, |k| k.per_page);
            return if per_page > 0 { format!("{base}.{}", (page - 1) * per_page) } else { thread_url.to_string() };
        }
        if thread_url.contains("viewtopic.php") {
            let per_page = self.known.as_deref().map_or(0, |k| k.per_page);
            return if per_page > 0 { format!("{thread_url}&start={}", (page - 1) * per_page) } else { thread_url.to_string() };
        }
        format!("{thread_url}page-{page}")
    }

    /// A Wayback Machine URL that redirects to the nearest capture of `url`.
    pub fn wayback(&self, url: &str, image: bool) -> String {
        let flag = if image { "im_" } else { "" };
        format!("https://web.archive.org/web/2{flag}/{url}")
    }

    /// Absolute original-forum URL for a forum path.
    pub fn original(&self, path: &str) -> String {
        format!("{}{}", self.origin.trim_end_matches('/'), path)
    }

    pub fn rewrite(&self, value: &str, is_image: bool) -> String {
        let v = strip_wayback(value.trim());
        let absolute = if v.starts_with("//") {
            format!("https:{v}")
        } else if v.starts_with("index.php?") && smf_link(v).is_some() {
            self.original(&format!("{}{v}", self.board_dir))
        } else if v.starts_with('/') || v.starts_with("index.php") {
            self.original(&format!("/{}", v.trim_start_matches('/')))
        } else if let Some(rel) = self.relative_board_link(v) {
            rel
        } else {
            v.to_string()
        };
        if !(absolute.starts_with("http://") || absolute.starts_with("https://")) {
            return absolute; // mailto:, #anchor, relative oddities: leave alone
        }
        if self.is_forum_url(&absolute) {
            if !is_image {
                if let Some(link) = phpbb_link(&absolute) {
                    if let Some(local) = self.local_phpbb(link) {
                        return local;
                    }
                    return self.wayback(&absolute, false);
                }
                if let Some(link) = smf_link(&absolute) {
                    // Same shapes as phpBB: topic + start offset, post, board, member.
                    let as_phpbb = match link {
                        SmfLink::Topic { id, start } => PhpbbLink::Topic { id, start },
                        SmfLink::Post { id } => PhpbbLink::Post { id },
                        SmfLink::Board { id, start } => PhpbbLink::Forum { id, start },
                        SmfLink::Member { id } => PhpbbLink::Member { id },
                    };
                    if let Some(local) = self.local_phpbb(as_phpbb) {
                        return local;
                    }
                    return self.wayback(&absolute, false);
                }
                if let Some(local) = self.local(&normalize_path(&absolute)) {
                    return local;
                }
            }
            return self.wayback(&absolute, is_image);
        }
        if is_image {
            // Third-party images (imgur, ...) frequently vanish; the Wayback copy outlives them.
            return self.wayback(&absolute, true);
        }
        absolute
    }
}

/// Sanitize post HTML and rewrite its links.
pub fn clean_post_html(html: &str, ctx: &LinkContext) -> String {
    let ctx = ctx.clone();
    let mut b = ammonia::Builder::default();
    b.add_tags(["details", "summary", "figure", "figcaption"])
        .add_generic_attributes(["class"])
        .add_tag_attributes("img", ["alt", "width", "height", "loading"])
        .rm_tag_attributes("img", ["srcset"])
        .link_rel(Some("nofollow noopener"))
        .url_relative(ammonia::UrlRelative::PassThrough)
        // Drop the XenForo/Wayback chrome that sometimes sits inside bodies.
        .clean_content_tags(HashSet::from(["script", "style", "noscript", "template"]))
        .attribute_filter(move |element, attribute, value| match (element, attribute) {
            // Email links are dropped (the address itself is hidden in the text).
            ("a", "href") if value.trim_start().to_ascii_lowercase().starts_with("mailto:") => None,
            ("a", "href") => Some(Cow::Owned(ctx.rewrite(value, false))),
            ("img", "src") => Some(Cow::Owned(ctx.rewrite(value, true))),
            _ => Some(Cow::Borrowed(value)),
        });
    // Lazy-load images: thread pages can carry dozens of them.
    let html = embeds_to_links(html);
    let html = smilies_to_emoji(&html);
    let html = staff_notes(&html);
    b.clean(&html).to_string().replace("<img ", "<img loading=\"lazy\" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> LinkContext {
        LinkContext { origin: "https://forums.example.com".into(), base: "/rm/".into(), known: None, board_dir: "/".into() }
    }

    #[test]
    fn mailto_links_are_dropped() {
        let out = clean_post_html(r#"<a href="mailto:a@b.com">write me</a>"#, &ctx());
        assert!(!out.contains("mailto"), "{out}");
        assert!(out.contains("write me"));
    }

    #[test]
    fn unknown_pages_go_to_wayback() {
        let mut k = KnownPages::default();
        k.threads.insert(5, 2);
        k.forums.insert(9, "categories");
        k.members.insert(7);
        let c = LinkContext { known: Some(Arc::new(k)), ..ctx() };
        assert_eq!(c.rewrite("/threads/a.5/page-2", false), "/rm/threads/5/page-2/");
        assert_eq!(
            c.rewrite("/threads/a.5/page-3", false),
            "https://web.archive.org/web/2/https://forums.example.com/threads/a.5/page-3"
        );
        assert_eq!(c.rewrite("/threads/b.6/", false), "https://web.archive.org/web/2/https://forums.example.com/threads/b.6/");
        assert_eq!(c.rewrite("/forums/x.9/page-4", false), "/rm/categories/9/");
        assert_eq!(c.rewrite("/members/bob.7/", false), "/rm/members/7/");
        assert_eq!(c.rewrite("/members/gone.8/", false), "https://web.archive.org/web/2/https://forums.example.com/members/gone.8/");
    }

    #[test]
    fn phpbb_links_map_into_the_archive() {
        let mut known = KnownPages { per_page: 15, ..Default::default() };
        known.threads.insert(5, 3);
        known.forums.insert(2, "forums");
        known.members.insert(7);
        known.posts.insert(99, (5, 2));
        let c = LinkContext { known: Some(Arc::new(known)), ..ctx() };
        assert_eq!(c.rewrite("./viewtopic.php?f=2&amp;t=5&amp;start=30", false), "/rm/threads/5/page-3/");
        assert_eq!(c.rewrite("https://forums.example.com/forum/viewtopic.php?t=5", false), "/rm/threads/5/");
        assert_eq!(c.rewrite("viewtopic.php?p=99#p99", false), "/rm/threads/5/page-2/#post-99");
        assert_eq!(c.rewrite("./viewforum.php?f=2", false), "/rm/forums/2/");
        assert_eq!(c.rewrite("./memberlist.php?mode=viewprofile&amp;u=7", false), "/rm/members/7/");
        assert_eq!(
            c.rewrite("./viewtopic.php?t=6", false),
            "https://web.archive.org/web/2/https://forums.example.com/viewtopic.php?t=6"
        );
        assert_eq!(c.rewrite("/threads/a.5/post-99", false), "/rm/threads/5/page-2/#post-99");
        let c2 = LinkContext { board_dir: "/forum/".into(), ..c.clone() };
        assert_eq!(
            c2.rewrite("./viewtopic.php?t=6", false),
            "https://web.archive.org/web/2/https://forums.example.com/forum/viewtopic.php?t=6"
        );
        assert_eq!(c.original_page("https://f.example/viewtopic.php?t=5", 3), "https://f.example/viewtopic.php?t=5&start=30");
        assert_eq!(c.original_page("https://f.example/threads/a.5/", 3), "https://f.example/threads/a.5/page-3");
        // SMF
        assert_eq!(c.rewrite("https://forums.example.com/index.php?topic=5.30", false), "/rm/threads/5/page-3/");
        assert_eq!(c.rewrite("https://forums.example.com/index.php/topic,5.msg99.html#msg99", false), "/rm/threads/5/page-2/#post-99");
        assert_eq!(c.rewrite("https://forums.example.com/index.php?board=2.0", false), "/rm/forums/2/");
        assert_eq!(c.rewrite("https://forums.example.com/index.php?action=profile;u=7", false), "/rm/members/7/");
        assert_eq!(c.original_page("https://f.example/index.php?topic=5.0", 3), "https://f.example/index.php?topic=5.30");
    }

    #[test]
    fn youtube_embed_survives_cleaning() {
        let html = r#"<span data-s9e-mediaembed="youtube"><span><span data-s9e-mediaembed-iframe="[&quot;src&quot;,&quot;https:\/\/www.youtube.com\/embed\/abc&quot;]"></span></span></span>"#;
        let out = clean_post_html(html, &ctx());
        assert!(out.contains(r#"<p class="embed-link"><a href="https://www.youtube.com/watch?v=abc" rel="nofollow noopener">▶ YouTube video</a></p>"#), "{out}");
    }

    #[test]
    fn strips_dangerous_markup() {
        let out = clean_post_html(r#"<b onclick="x()">hi</b><script>alert(1)</script><style>*{}</style>"#, &ctx());
        assert_eq!(out, "<b>hi</b>");
    }

    #[test]
    fn rewrites_links() {
        let c = ctx();
        assert_eq!(
            c.rewrite("https://web.archive.org/web/2015/http://forums.example.com/index.php?threads/a.5/", false),
            "/rm/threads/5/"
        );
        assert_eq!(c.rewrite("/threads/a.5/page-3", false), "/rm/threads/5/page-3/");
        assert_eq!(c.rewrite("https://forums.example.com/members/bob.7/", false), "/rm/members/7/");
        assert_eq!(
            c.rewrite("/data/avatars/m/1/1.jpg", true),
            "https://web.archive.org/web/2im_/https://forums.example.com/data/avatars/m/1/1.jpg"
        );
        assert_eq!(c.rewrite("https://github.com/x", false), "https://github.com/x");
        assert_eq!(
            c.rewrite("https://web.archive.org/web/2015im_/http://i.imgur.com/a.png", true),
            "https://web.archive.org/web/2im_/http://i.imgur.com/a.png"
        );
        assert_eq!(c.rewrite("#post-5", false), "#post-5");
    }

    #[test]
    fn images_lazy_and_nofollow() {
        let out = clean_post_html(r#"<a href="https://x.org/">x</a><img src="https://i.imgur.com/a.png">"#, &ctx());
        assert!(out.contains(r#"rel="nofollow noopener""#));
        assert!(out.contains(r#"<img loading="lazy" src="https://web.archive.org/web/2im_/https://i.imgur.com/a.png""#));
    }
}
