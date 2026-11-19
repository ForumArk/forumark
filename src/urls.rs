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

/// Decode a downloaded or saved page. Old forums often aren't UTF-8 (phpBB 2 boards
/// were usually ISO-8859-1 / Windows-1252), so use the declared charset: the HTTP
/// header if given, else the page's `<meta charset>`, else Windows-1252.
pub fn decode_html(bytes: &[u8], content_type: Option<&str>) -> String {
    fn label_after(s: &str) -> Option<&str> {
        let i = s.to_ascii_lowercase().find("charset=")?;
        let rest = s[i + 8..].trim_start_matches(['"', '\'', ' ']);
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' || c == '.')).unwrap_or(rest.len());
        Some(&rest[..end]).filter(|l| !l.is_empty())
    }
    // Valid UTF-8 is taken as UTF-8 even when the page claims otherwise: legacy text
    // with accents is practically never valid UTF-8, and pages Forum Ark saved itself
    // (`--raw-dir`) are stored as UTF-8 but keep their original `<meta charset>`.
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let label = content_type.and_then(label_after).map(str::to_string).or_else(|| label_after(&head).map(str::to_string));
    if let Some(enc) = label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        return enc.decode(bytes).0.into_owned();
    }
    encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()
}

/// A link to a phpBB page (phpBB 2 and 3 use the same script names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhpbbLink {
    /// `viewtopic.php?t=ID[&start=N]`
    Topic { id: i64, start: u32 },
    /// `viewtopic.php?p=ID` (opens the page holding that post)
    Post { id: i64 },
    /// `viewforum.php?f=ID[&start=N]`
    Forum { id: i64, start: u32 },
    /// `memberlist.php?mode=viewprofile&u=ID` (3.x) or `profile.php?mode=viewprofile&u=ID` (2.x)
    Member { id: i64 },
}

/// Query parameters of a URL, with `&amp;` decoded. Later keys win.
pub fn query_params(url: &str) -> Vec<(String, String)> {
    let Some((_, q)) = url.split_once('?') else { return Vec::new() };
    let q = q.split('#').next().unwrap_or("").replace("&amp;", "&");
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_ascii_lowercase(), v.to_string()),
            None => (kv.to_ascii_lowercase(), String::new()),
        })
        .collect()
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn num<T: std::str::FromStr>(params: &[(String, String)], key: &str) -> Option<T> {
    param(params, key).and_then(|v| v.parse().ok())
}

/// The script file name of a (Wayback-stripped) URL, e.g. `viewtopic.php`.
fn script_name(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.rsplit('/').next().unwrap_or("")
}

/// Recognise a phpBB topic, post, forum or member link.
pub fn phpbb_link(url: &str) -> Option<PhpbbLink> {
    let url = strip_wayback(url.trim());
    let script = script_name(url).to_ascii_lowercase();
    let params = query_params(url);
    match script.as_str() {
        "viewtopic.php" => {
            if let Some(id) = num(&params, "t") {
                Some(PhpbbLink::Topic { id, start: num(&params, "start").unwrap_or(0) })
            } else {
                num(&params, "p").map(|id| PhpbbLink::Post { id })
            }
        }
        "viewforum.php" => num(&params, "f").map(|id| PhpbbLink::Forum { id, start: num(&params, "start").unwrap_or(0) }),
        "memberlist.php" | "profile.php" => {
            if param(&params, "mode") == Some("viewprofile") {
                num(&params, "u").map(|id| PhpbbLink::Member { id })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Folder of a forum script, relative to the site root: `/phpBB2/viewtopic.php?t=1` → `/phpBB2/`.
pub fn script_dir(url: &str) -> String {
    let path = normalize_path(url);
    match path.rfind('/') {
        Some(i) => path[..=i].to_string(),
        None => "/".to_string(),
    }
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

    #[test]
    fn decoding() {
        let latin = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1\"><p>caf\xe9</p>";
        assert!(decode_html(latin, None).contains("café"));
        assert!(decode_html(b"<p>caf\xe9</p>", None).contains("café"));
        assert!(decode_html("<p>café</p>".as_bytes(), Some("text/html; charset=UTF-8")).contains("café"));
    }

    #[test]
    fn phpbb_links() {
        use PhpbbLink::*;
        assert_eq!(phpbb_link("./viewtopic.php?f=1&amp;t=427852&amp;start=135"), Some(Topic { id: 427852, start: 135 }));
        assert_eq!(phpbb_link("https://web.archive.org/web/2005/http://f.example:80/phpBB/viewtopic.php?t=9"), Some(Topic { id: 9, start: 0 }));
        assert_eq!(phpbb_link("viewtopic.php?p=1101513#1101513"), Some(Post { id: 1101513 }));
        assert_eq!(phpbb_link("/forum/viewforum.php?f=12&start=50"), Some(Forum { id: 12, start: 50 }));
        assert_eq!(phpbb_link("./memberlist.php?mode=viewprofile&amp;u=38086&amp;sid=ab"), Some(Member { id: 38086 }));
        assert_eq!(phpbb_link("profile.php?mode=viewprofile&u=7"), Some(Member { id: 7 }));
        assert_eq!(phpbb_link("memberlist.php?mode=email&u=7"), None);
        assert_eq!(phpbb_link("/threads/a.1/"), None);
        assert_eq!(script_dir("http://f.example:80/phpBB/viewtopic.php?t=9"), "/phpBB/");
        assert_eq!(script_dir("https://f.example/viewtopic.php?t=9"), "/");
    }
}
