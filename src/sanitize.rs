//! Make captured post HTML safe to publish and keep its links working.
//!
//! * scripts, styles, event handlers and other dangerous markup are removed (ammonia)
//! * Wayback Machine prefixes are stripped from links
//! * links to threads/forums/members of the archived forum point into the archive
//! * other links to the original forum and images go through the Wayback Machine,
//!   which keeps serving them after the forum is gone

use std::borrow::Cow;
use std::collections::HashSet;

use crate::urls::{id_after, normalize_path, page_number, strip_wayback};

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
        for route in ["threads", "forums", "categories", "members"] {
            if let Some(id) = id_after(path, route) {
                let page = page_number(path);
                return Some(local_href(&self.base, route, id, page));
            }
        }
        None
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
        } else if v.starts_with('/') || v.starts_with("index.php") {
            self.original(&format!("/{}", v.trim_start_matches('/')))
        } else {
            v.to_string()
        };
        if !(absolute.starts_with("http://") || absolute.starts_with("https://")) {
            return absolute; // mailto:, #anchor, relative oddities: leave alone
        }
        if self.is_forum_url(&absolute) {
            if !is_image {
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
            ("a", "href") => Some(Cow::Owned(ctx.rewrite(value, false))),
            ("img", "src") => Some(Cow::Owned(ctx.rewrite(value, true))),
            _ => Some(Cow::Borrowed(value)),
        });
    // Lazy-load images: thread pages can carry dozens of them.
    b.clean(html).to_string().replace("<img ", "<img loading=\"lazy\" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> LinkContext {
        LinkContext { origin: "https://forums.example.com".into(), base: "/rm/".into() }
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
