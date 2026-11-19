//! Small clean-ups of captured post HTML that make old posts readable again.
//!
//! * XenForo "sprite" smilies are a blank 1×1 image plus CSS the archive doesn't
//!   have, so they would show as broken images. They become emoji (or their code).
//! * RPG Maker Forums staff typed tags such as `[move]Forum[/move]` by hand. The
//!   forum showed them as raw text; here they become small labelled notes.

use std::borrow::Cow;

/// Replace sprite smilies with emoji. Smilies with a real image URL are left alone.
pub fn smilies_to_emoji(html: &str) -> Cow<'_, str> {
    if !html.contains("smilie") {
        return Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(i) = html[pos..].find("<img").map(|i| pos + i) {
        let Some(end) = html[i..].find('>').map(|e| i + e + 1) else { break };
        let tag = &html[i..end];
        out.push_str(&html[pos..i]);
        if tag.contains("class=\"smilie") && tag.contains("src=\"data:") {
            let code = attr(tag, "data-shortname").or_else(|| attr(tag, "alt")).unwrap_or_default();
            let code = unescape(&code);
            match emoji_for(&code) {
                Some(e) => out.push_str(&format!(r#"<span class="smilie" title="{}">{e}</span>"#, escape(&code))),
                None => out.push_str(&format!(r#"<span class="smilie smilie-text">{}</span>"#, escape(&code))),
            }
        } else {
            out.push_str(tag);
        }
        pos = end;
    }
    out.push_str(&html[pos..]);
    Cow::Owned(out)
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let i = tag.find(&key)? + key.len();
    let end = tag[i..].find('"')?;
    Some(tag[i..i + end].to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&#039;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The RPG Maker Forums' smilies, matched to the closest emoji.
fn emoji_for(code: &str) -> Option<&'static str> {
    let c = code.trim();
    Some(match c {
        ":)" | ":smile:" | ":LZSsmile:" | ":rhappy:" => "🙂",
        ":D" | ":biggrin:" | ":LZScheeze:" => "😀",
        ":p" | ":P" | ":stickytongue:" | ":LZStongue:" => "😛",
        ";)" | ":wink:" | ":LZSwink:" | ":rwink:" | ":kaowink:" => "😉",
        ":(" | ":frown:" | ":kaosad:" | ":LZSsad:" => "🙁",
        ":o" | ":O" | ":eek:" | ":kaoshock:" => "😮",
        "B)" | ":cool:" | ":kaocool:" => "😎",
        "o_O" | "O_o" | "o.O" => "😳",
        ";_;" | ":kaocry:" | ":LZScry:" | ":cry:" => "😢",
        ":kaojoy:" | ":LZSjoy:" => "😂",
        ":guffaw:" | ":LZSlol:" | ":kaolol:" => "🤣",
        ":kaohi:" | ":LZShi:" => "👋",
        ":kaoluv:" | ":wub" | ":wub:" | ":LZSluv:" => "😍",
        ":kaoswt:" | ":kaoswt2:" | ":rswt" | ":rswt:" | ":aswt:" | ":yswt:" | ":LZSswt:" => "😅",
        ":kaopride:" => "😤",
        ":kaothx:" => "🙏",
        ":kaoslp:" => "😴",
        ":kaophew:" => "😮‍💨",
        ":kaoblush:" | ":LZSblush:" => "😊",
        ":kaosigh:" => "😔",
        ":kaodes:" => "😩",
        ":kaoeh:" => "😐",
        ":kaoback:" => "👋",
        ":kaoangry:" | ":LZSangry:" | ":mad:" => "😠",
        ":kaoyay:" | ":LZSexcite:" => "🤩",
        ":cutesmile:" => "☺️",
        ":thumbsup-right:" | ":thumbsup-left:" | ":thumbsup:" => "👍",
        ":rock-right:" | ":rock-left:" => "🤘",
        ":unsure:" | ":kaohmm:" => "🤔",
        ":rolleyes:" => "🙄",
        ":confused:" => "😕",
        _ => return None,
    })
}

/// Staff tags and how they are labelled.
const STAFF_TAGS: [(&str, &str); 5] = [
    ("move", "Moved to:"),
    ("closed", "Thread closed"),
    ("mod", "Moderator note:"),
    ("necro", "Necro-post notice:"),
    ("dpost", "Double-post notice:"),
];

/// Turn `[move]X[/move]`-style staff tags into labelled notes.
pub fn staff_notes(html: &str) -> Cow<'_, str> {
    if !html.contains("[/") {
        return Cow::Borrowed(html);
    }
    let mut s = html.to_string();
    for (tag, label) in STAFF_TAGS {
        let open = format!("[{tag}]");
        let close = format!("[/{tag}]");
        let mut out = String::with_capacity(s.len());
        let mut pos = 0;
        let lower = s.to_ascii_lowercase();
        while let Some(a) = lower[pos..].find(&open).map(|i| pos + i) {
            let Some(b) = lower[a..].find(&close).map(|i| a + i) else { break };
            let inner = s[a + open.len()..b].trim();
            out.push_str(&s[pos..a]);
            // "[closed].[/closed]" carries no text: just say it was closed.
            let body = if inner.is_empty() || inner == "." { String::new() } else { format!(" {inner}") };
            out.push_str(&format!(r#"<span class="staff-note">{label}{body}</span>"#));
            pos = b + close.len();
        }
        out.push_str(&s[pos..]);
        s = out;
    }
    if s == html { Cow::Borrowed(html) } else { Cow::Owned(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_smilies_become_emoji() {
        let html = r#"easy <img alt=":)" class="smilie smilie--sprite" data-shortname=":)" src="data:image/gif;base64,R0l" title="Smile"> and <img class="smilie" alt=":zzz:" src="data:x"> <img class="smilie" alt=":x:" src="/data/attachments/smilies/x.png">"#;
        let out = smilies_to_emoji(html);
        assert!(out.contains(r#"<span class="smilie" title=":)">🙂</span>"#), "{out}");
        assert!(out.contains(r#"<span class="smilie smilie-text">:zzz:</span>"#), "{out}");
        assert!(out.contains(r#"src="/data/attachments/smilies/x.png""#), "real smilie images stay: {out}");
    }

    #[test]
    fn staff_tags_become_notes() {
        assert_eq!(
            staff_notes("[move]RGSS3 Script Requests[/move]<br>Hi"),
            r#"<span class="staff-note">Moved to: RGSS3 Script Requests</span><br>Hi"#
        );
        assert_eq!(staff_notes("[CLOSED].[/CLOSED]"), r#"<span class="staff-note">Thread closed</span>"#);
        assert_eq!(staff_notes("a [move] but never closed"), "a [move] but never closed");
    }
}
