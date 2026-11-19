//! Turn embedded media (YouTube, Imgur, X/Twitter, Pastebin, ...) into plain links.
//!
//! XenForo's media-embed add-on (s9e) stores embeds as empty `<span>`/`<iframe>`
//! tags whose real address hides in a `data-s9e-mediaembed-iframe` (JSON) or
//! `data-s9e-mediaembed-src` attribute. Sanitizing removes those attributes and
//! the iframes, so without this step such posts render as nothing.
//!
//! The archive deliberately does not embed players: a link to the original keeps
//! pages fast, works without third-party scripts and doesn't track readers.

use std::borrow::Cow;

use url::Url;

const MARKERS: [&str; 2] = ["data-s9e-mediaembed-iframe=\"", "data-s9e-mediaembed-src=\""];

/// Insert a link card before every embed in `html`. The empty embed tags are
/// left for the sanitizer to strip.
pub fn embeds_to_links(html: &str) -> Cow<'_, str> {
    if !html.contains("data-s9e-mediaembed-") {
        return Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len() + 256);
    let mut pos = 0;
    while let Some((at, marker)) = next_marker(html, pos) {
        let tag_start = html[..at].rfind('<').unwrap_or(at);
        let value_start = at + marker.len();
        let Some(value_len) = html[value_start..].find('"') else { break };
        let raw = &html[value_start..value_start + value_len];
        let tag_end = html[value_start..].find('>').map(|i| value_start + i + 1).unwrap_or(html.len());

        out.push_str(&html[pos..tag_start]);
        let src = if marker.contains("iframe") { src_from_json(&unescape(raw)) } else { Some(unescape(raw)) };
        if let Some(src) = src {
            let (label, href) = embed_target(&src);
            out.push_str(&card(&label, href.as_deref()));
        }
        out.push_str(&html[tag_start..tag_end]);
        pos = tag_end;
    }
    out.push_str(&html[pos..]);
    Cow::Owned(out)
}

fn next_marker(html: &str, from: usize) -> Option<(usize, &'static str)> {
    MARKERS
        .iter()
        .filter_map(|m| html[from..].find(m).map(|i| (from + i, *m)))
        .min_by_key(|(i, _)| *i)
}

fn card(label: &str, href: Option<&str>) -> String {
    match href {
        Some(h) => format!(
            r#"<p class="embed-link"><a href="{}">▶ {}</a></p>"#,
            escape_attr(h),
            escape_text(label)
        ),
        None => format!(r#"<p class="embed-link embed-gone">{} (not preserved)</p>"#, escape_text(label)),
    }
}

/// The `src` from s9e's JSON attribute list, e.g. `["allowfullscreen","","src","https:\/\/..."]`.
fn src_from_json(json: &str) -> Option<String> {
    let i = json.find("\"src\",\"")? + "\"src\",\"".len();
    let end = json[i..].find('"')?;
    Some(json[i..i + end].replace("\\/", "/"))
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A readable label and, where it can be worked out, the original page's address.
pub fn embed_target(src: &str) -> (String, Option<String>) {
    let full = if src.starts_with("//") { format!("https:{src}") } else { src.to_string() };
    let Ok(u) = Url::parse(&full) else { return ("Embedded content".into(), None) };
    let host = u.host_str().unwrap_or("").trim_start_matches("www.").to_string();
    let path = u.path().to_string();
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let query = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    let link = |label: &str, href: String| (label.to_string(), Some(href));
    let gone = |label: &str| (label.to_string(), None);

    // s9e's own helper pages: the content id is in the #fragment.
    if host == "s9e.github.io" {
        let kind = seg.last().map(|s| s.trim_end_matches(".min.html")).unwrap_or("");
        let frag = u.fragment().unwrap_or("");
        let id = frag.split('#').next().unwrap_or("");
        if id.is_empty() {
            return gone(&format!("Embedded {kind} content"));
        }
        return match kind {
            "imgur" => link("Imgur image", format!("https://imgur.com/{id}")),
            "twitter" => link("Post on X (Twitter)", format!("https://twitter.com/i/web/status/{id}")),
            "reddit" => link("Reddit post", format!("https://www.reddit.com/r/{id}")),
            "facebook" => link("Facebook post", format!("https://www.facebook.com/{id}")),
            "pinterest" => link("Pinterest pin", format!("https://www.pinterest.com/pin/{id}")),
            "gist" => link("GitHub Gist", format!("https://gist.github.com/{id}")),
            "instagram" => link("Instagram post", format!("https://www.instagram.com/p/{id}")),
            "medium" => link("Medium article", format!("https://medium.com/p/{id}")),
            "telegram" => link("Telegram post", format!("https://t.me/{id}")),
            "twitch" => {
                let get = |k: &str| {
                    id.split(';').find_map(|p| p.strip_prefix(&format!("{k}="))).filter(|v| !v.is_empty())
                };
                if let Some(v) = get("video_id") {
                    link("Twitch video", format!("https://www.twitch.tv/videos/{v}"))
                } else if let Some(c) = get("clip_id") {
                    link("Twitch clip", format!("https://clips.twitch.tv/{c}"))
                } else if let Some(ch) = get("channel") {
                    link("Twitch channel", format!("https://www.twitch.tv/{ch}"))
                } else {
                    gone("Twitch video")
                }
            }
            "tumblr" => gone("Tumblr post"),
            other => gone(&format!("Embedded {other} content")),
        };
    }

    match host.as_str() {
        "youtube.com" | "youtube-nocookie.com" if seg.first() == Some(&"embed") && seg.len() > 1 => {
            link("YouTube video", format!("https://www.youtube.com/watch?v={}", seg[1]))
        }
        "drive.google.com" if seg.len() >= 3 && seg[0] == "file" => {
            link("Google Drive file", format!("https://drive.google.com/file/d/{}/view", seg[2]))
        }
        "docs.google.com" if seg.len() >= 3 => {
            link("Google document", format!("https://docs.google.com/{}/d/{}", seg[0], seg[2]))
        }
        "pastebin.com" => match query("i").or_else(|| seg.get(1).map(|s| s.to_string())) {
            Some(id) => link("Pastebin", format!("https://pastebin.com/{id}")),
            None => gone("Pastebin"),
        },
        "w.soundcloud.com" => match query("url") {
            Some(t) if t.starts_with("https://soundcloud.com/") => link("SoundCloud track", t),
            _ => gone("SoundCloud track"),
        },
        "store.steampowered.com" if seg.len() >= 2 && seg[0] == "widget" => {
            link("Steam store page", format!("https://store.steampowered.com/app/{}", seg[1]))
        }
        "player.vimeo.com" if seg.len() >= 2 => link("Vimeo video", format!("https://vimeo.com/{}", seg[1])),
        "streamable.com" if seg.len() >= 2 => link("Streamable video", format!("https://streamable.com/{}", seg[1])),
        "gfycat.com" if seg.len() >= 2 => link("Gfycat clip", format!("https://gfycat.com/{}", seg[1])),
        "giphy.com" if seg.len() >= 2 => link("GIPHY animation", format!("https://giphy.com/gifs/{}", seg[1])),
        "gifs.com" if seg.len() >= 2 => link("GIF", format!("https://gifs.com/gif/{}", seg[1])),
        "open.spotify.com" if seg.len() >= 3 => {
            link("Spotify", format!("https://open.spotify.com/{}/{}", seg[1], seg[2]))
        }
        "dailymotion.com" if seg.len() >= 3 => {
            link("Dailymotion video", format!("https://www.dailymotion.com/video/{}", seg[2]))
        }
        "codepen.io" if seg.len() >= 3 => link("CodePen", format!("https://codepen.io/{}/pen/{}", seg[0], seg[2])),
        "audiomack.com" if seg.len() >= 4 => {
            link("Audiomack track", format!("https://audiomack.com/{}/song/{}", seg[2], seg[3]))
        }
        "bitchute.com" if seg.len() >= 2 => link("BitChute video", format!("https://www.bitchute.com/video/{}/", seg[1])),
        "flickr.com" if seg.len() >= 3 => link("Flickr photo", format!("https://www.flickr.com/photo.gne?id={}", seg[2])),
        "vk.com" => match (query("oid"), query("id")) {
            (Some(o), Some(i)) => link("VK video", format!("https://vk.com/video{o}_{i}")),
            _ => gone("VK video"),
        },
        "kickstarter.com" => {
            let p = path.split("/widget/").next().unwrap_or(&path);
            link("Kickstarter project", format!("https://www.kickstarter.com{p}"))
        }
        "bandcamp.com" => gone("Bandcamp player"),
        h if h.contains("amazon") => gone("Amazon product widget"),
        _ => link("Embedded content", full.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s9e_youtube_span_becomes_link() {
        let html = r#"<p>Look:</p><span data-s9e-mediaembed="youtube"><span><span data-s9e-mediaembed-iframe="[&quot;allowfullscreen&quot;,&quot;&quot;,&quot;src&quot;,&quot;https:\/\/www.youtube.com\/embed\/puwxE0pRKAU&quot;]" style="x"></span></span></span>"#;
        let out = embeds_to_links(html);
        assert!(out.contains(r#"<a href="https://www.youtube.com/watch?v=puwxE0pRKAU">▶ YouTube video</a>"#), "{out}");
        assert_eq!(out.matches("embed-link").count(), 1);
    }

    #[test]
    fn iframe_src_variant() {
        let html = r#"<iframe allowfullscreen="" data-s9e-mediaembed="soundcloud" data-s9e-mediaembed-src="https://w.soundcloud.com/player/?url=https%3A//soundcloud.com/a/b&amp;x=1"></iframe>"#;
        assert!(embeds_to_links(html).contains(r#"href="https://soundcloud.com/a/b""#));
    }

    #[test]
    fn targets() {
        let t = |s: &str| embed_target(s).1;
        assert_eq!(t("https://s9e.github.io/iframe/2/imgur.min.html#a/e4yhf").as_deref(), Some("https://imgur.com/a/e4yhf"));
        assert_eq!(
            t("https://s9e.github.io/iframe/2/twitter.min.html#1040057251111817216").as_deref(),
            Some("https://twitter.com/i/web/status/1040057251111817216")
        );
        assert_eq!(t("//pastebin.com/embed_iframe.php?i=gZiXvzWY").as_deref(), Some("https://pastebin.com/gZiXvzWY"));
        assert_eq!(t("//store.steampowered.com/widget/932644").as_deref(), Some("https://store.steampowered.com/app/932644"));
        assert_eq!(
            t("//drive.google.com/file/d/137FJ/preview").as_deref(),
            Some("https://drive.google.com/file/d/137FJ/view")
        );
        assert_eq!(
            t("https://s9e.github.io/iframe/2/reddit.min.html#RPGMaker/comments/9pss0f#theme=light").as_deref(),
            Some("https://www.reddit.com/r/RPGMaker/comments/9pss0f")
        );
        assert_eq!(t("//player.vimeo.com/video/438178152").as_deref(), Some("https://vimeo.com/438178152"));
        assert_eq!(t("https://s9e.github.io/iframe/2/tumblr.min.html#xo/1"), None);
        assert_eq!(
            t("https://s9e.github.io/iframe/2/twitch.min.html#channel=angrybargy;clip_id=;t=;video_id=").as_deref(),
            Some("https://www.twitch.tv/angrybargy")
        );
    }

    #[test]
    fn untouched_without_embeds() {
        assert!(matches!(embeds_to_links("<p>hi</p>"), Cow::Borrowed(_)));
    }
}
