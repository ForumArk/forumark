//! URL helpers: undo Wayback Machine rewriting and pull XenForo content IDs out of paths.

/// Strip a Wayback Machine prefix such as
/// `https://web.archive.org/web/20200101000000im_/https://forum.example/x`
/// (or the relative `/web/2020.../https://...`) and return the original URL.
pub fn strip_wayback(url: &str) -> &str {
    let rest = if let Some(i) = url.find("/web/") {
        &url[i + 5..]
    } else {
        return url;
    };
    // Timestamp is digits, optionally followed by a modifier like `id_`, `im_`, `js_`.
    let ts_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if ts_end < 4 {
        return url;
    }
    let mut after = &rest[ts_end..];
    if let Some(slash) = after.find('/') {
        if after[..slash].chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
            after = &after[slash + 1..];
            return after;
        }
    }
    url
}

/// Reduce a URL to a site-relative path in modern XenForo form.
///
/// * removes Wayback prefixes, scheme and host
/// * converts XenForo 1 style `index.php?threads/x.1/` to `/threads/x.1/`
/// * drops query strings and fragments
pub fn normalize_path(url: &str) -> String {
    let url = strip_wayback(url.trim());
    let mut s = url;
    if let Some(i) = s.find("://") {
        s = &s[i + 3..];
        s = match s.find('/') {
            Some(j) => &s[j..],
            None => "/",
        };
    }
    let owned;
    if let Some(i) = s.find("index.php?") {
        owned = format!("/{}", &s[i + "index.php?".len()..]);
        s = &owned;
        let s = s.split(['#']).next().unwrap_or("");
        // XenForo 1 puts extra params after `&`.
        let s = s.split('&').next().unwrap_or("");
        return ensure_leading_slash(s);
    }
    let s = s.split(['?', '#']).next().unwrap_or("");
    ensure_leading_slash(s)
}

fn ensure_leading_slash(s: &str) -> String {
    if s.starts_with('/') { s.to_string() } else { format!("/{s}") }
}

/// Extract the numeric ID that XenForo appends to a route segment.
///
/// `id_after("/threads/my-title.1234/page-2", "threads")` → `Some(1234)`;
/// also handles ID-only segments like `/threads/1234/`.
pub fn id_after(path: &str, route: &str) -> Option<i64> {
    let needle = format!("{route}/");
    let start = path.find(&needle)? + needle.len();
    let seg = path[start..].split('/').next()?;
    let num = match seg.rfind('.') {
        Some(dot) => &seg[dot + 1..],
        None => seg,
    };
    num.parse().ok()
}

/// Page number from a `page-N` path segment (1 when absent).
pub fn page_number(path: &str) -> u32 {
    path.split('/')
        .find_map(|seg| seg.strip_prefix("page-").and_then(|n| n.parse().ok()))
        .unwrap_or(1)
}

/// The canonical path of a thread or forum, without any `page-N` / `post-N` suffix.
pub fn base_path(path: &str, route: &str) -> Option<String> {
    let needle = format!("/{route}/");
    let start = path.find(&needle)?;
    let rest = &path[start + needle.len()..];
    let seg = rest.split('/').next()?;
    Some(format!("/{route}/{seg}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayback_prefixes() {
        assert_eq!(
            strip_wayback("https://web.archive.org/web/20190101120000/https://f.example/threads/a.1/"),
            "https://f.example/threads/a.1/"
        );
        assert_eq!(
            strip_wayback("/web/20190101120000im_/https://f.example/a.png"),
            "https://f.example/a.png"
        );
        assert_eq!(strip_wayback("https://f.example/threads/a.1/"), "https://f.example/threads/a.1/");
    }

    #[test]
    fn normalize() {
        assert_eq!(
            normalize_path("https://web.archive.org/web/2019/https://f.example/threads/a.1/page-2?x=1#post-5"),
            "/threads/a.1/page-2"
        );
        assert_eq!(normalize_path("http://f.example/index.php?threads/a.1/&page=2"), "/threads/a.1/");
        assert_eq!(normalize_path("/members/bob.7/"), "/members/bob.7/");
    }

    #[test]
    fn ids_and_pages() {
        assert_eq!(id_after("/threads/my-title.1234/page-2", "threads"), Some(1234));
        assert_eq!(id_after("/threads/1234/", "threads"), Some(1234));
        assert_eq!(id_after("/forums/a.b.c.99/", "forums"), Some(99));
        assert_eq!(id_after("/forums/", "forums"), None);
        assert_eq!(page_number("/threads/a.1/page-7"), 7);
        assert_eq!(page_number("/threads/a.1/"), 1);
        assert_eq!(base_path("/threads/a.1/page-7", "threads").as_deref(), Some("/threads/a.1/"));
    }
}
