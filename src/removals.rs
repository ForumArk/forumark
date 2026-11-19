//! Removal requests: content left out of the built site.
//!
//! A removal list is a plain text file, one request per line, applied every time the site is
//! built. The database itself is never changed, so removed content stays removed when pages are
//! imported again. Lines:
//!
//! ```text
//! # Anything after a '#' is a comment: note the date and who asked.
//! thread 1234                     the whole thread disappears (no page, not in lists or search)
//! post 98765                      that post is replaced by "removed on request"
//! user 42                         every post by member 42 is replaced, and their member page goes
//! user "Some Name"                the same, by name (also catches posts without a member id)
//! anonymize 42                    posts stay, but are shown as "Former member"; no member page
//! anonymize "Some Name"
//! https://example.org/arc/threads/1234/page-2/#post-98765    a pasted link works too:
//!                                 #post-N removes that post, /threads/N/ the thread,
//!                                 /members/N/ the member
//! ```
//!
//! Wherever a removed or anonymized member's name appears as a quote attribution
//! ("Name said:", "Name wrote:", "Quote from: Name") or a mention (@Name) in other posts,
//! it is replaced by "a former member".
//!
//! Unknown lines are an error rather than being skipped, so a typo can't silently leave
//! content online.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

pub const FORMER_MEMBER: &str = "Former member";
pub const REMOVED_AUTHOR: &str = "Removed";
pub const REMOVED_BODY: &str = "<p class=\"removed-note\">This post was removed on request.</p>";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Removals {
    pub threads: HashSet<i64>,
    pub posts: HashSet<i64>,
    pub users: HashSet<i64>,
    pub user_names: HashSet<String>,
    pub anon_users: HashSet<i64>,
    pub anon_names: HashSet<String>,
}

enum Target {
    Id(i64),
    Name(String),
}

fn target(arg: &str) -> Option<Target> {
    let arg = arg.trim();
    if let Some(name) = arg.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return (!name.trim().is_empty()).then(|| Target::Name(name.trim().to_string()));
    }
    if let Ok(id) = arg.parse::<i64>() {
        return Some(Target::Id(id));
    }
    // A link: /threads/N/..., /members/N/..., ...#post-N
    if let Some((_, frag)) = arg.split_once("#post-") {
        return frag.trim_end_matches('/').parse().ok().map(Target::Id);
    }
    for kind in ["/threads/", "/members/"] {
        if let Some((_, rest)) = arg.split_once(kind) {
            return rest.split('/').next()?.parse().ok().map(Target::Id);
        }
    }
    None
}

/// What a pasted link points at: ("post" | "thread" | "user", id).
fn link_kind(link: &str) -> Option<(&'static str, i64)> {
    let Some(Target::Id(id)) = target(link) else { return None };
    if link.contains("#post-") {
        Some(("post", id))
    } else if link.contains("/threads/") {
        Some(("thread", id))
    } else if link.contains("/members/") {
        Some(("user", id))
    } else {
        None
    }
}

impl Removals {
    pub fn parse(text: &str) -> Result<Self> {
        let mut r = Removals::default();
        for (n, raw) in text.lines().enumerate() {
            // Comments start at '#', except the '#post-' of a pasted link.
            let mut line = raw;
            let mut search = 0;
            while let Some(i) = line[search..].find('#').map(|i| i + search) {
                if line[i..].starts_with("#post-") {
                    search = i + 1;
                } else {
                    line = &line[..i];
                    break;
                }
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (word, arg) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let bad = || anyhow::anyhow!("line {}: don't understand {:?}", n + 1, raw.trim());
            let (word, arg) = if word.starts_with("http") || word.starts_with('/') {
                let (kind, id) = link_kind(word).ok_or_else(bad)?;
                (kind, id.to_string())
            } else {
                (word, arg.to_string())
            };
            match (word.to_ascii_lowercase().as_str(), target(&arg)) {
                ("thread", Some(Target::Id(id))) => r.threads.insert(id),
                ("post", Some(Target::Id(id))) => r.posts.insert(id),
                ("user", Some(Target::Id(id))) => r.users.insert(id),
                ("user", Some(Target::Name(name))) => r.user_names.insert(name),
                ("anonymize", Some(Target::Id(id))) => r.anon_users.insert(id),
                ("anonymize", Some(Target::Name(name))) => r.anon_names.insert(name),
                _ => return Err(bad()),
            };
        }
        Ok(r)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading removal list {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in removal list {}", path.display()))
    }

    pub fn is_empty(&self) -> bool {
        *self == Removals::default()
    }

    pub fn count(&self) -> usize {
        self.threads.len() + self.posts.len() + self.users.len() + self.user_names.len() + self.anon_users.len() + self.anon_names.len()
    }

    /// Make this connection see the archive with the removals applied: temporary views named
    /// like the real tables (`posts`, `threads`, `users`, `attachments`) take their place for
    /// every query on this connection, until [`Applied`] is dropped. Returns the names to
    /// scrub from quotes and mentions.
    pub fn apply<'c>(&self, conn: &'c Connection) -> Result<Applied<'c>> {
        if self.is_empty() {
            return Ok(Applied { conn, names: Vec::new() });
        }
        conn.execute_batch(
            "DROP VIEW IF EXISTS temp.posts; DROP VIEW IF EXISTS temp.threads;
             DROP VIEW IF EXISTS temp.users; DROP VIEW IF EXISTS temp.attachments;
             DROP TABLE IF EXISTS temp.rm_thread; DROP TABLE IF EXISTS temp.rm_post;
             DROP TABLE IF EXISTS temp.rm_user; DROP TABLE IF EXISTS temp.anon_user; DROP TABLE IF EXISTS temp.rm_name;
             CREATE TEMP TABLE rm_thread (id INTEGER PRIMARY KEY);
             CREATE TEMP TABLE rm_post (id INTEGER PRIMARY KEY);
             CREATE TEMP TABLE rm_user (id INTEGER PRIMARY KEY);
             CREATE TEMP TABLE anon_user (id INTEGER PRIMARY KEY);
             -- lower-cased names; hidden = 1 removes the posts, 0 only anonymizes them
             CREATE TEMP TABLE rm_name (name TEXT PRIMARY KEY, hidden INTEGER NOT NULL);",
        )?;
        let mut names: Vec<String> = Vec::new();
        for (table, ids) in [("rm_thread", &self.threads), ("rm_post", &self.posts), ("rm_user", &self.users), ("anon_user", &self.anon_users)] {
            let mut ins = conn.prepare(&format!("INSERT OR IGNORE INTO temp.{table} (id) VALUES (?1)"))?;
            for id in ids {
                ins.execute([id])?;
            }
        }
        // Names of members given by id, so their quotes and name-only posts are caught too.
        let mut lookup = conn.prepare("SELECT name FROM main.users WHERE id = ?1")?;
        let add_name = |name: &str, hidden: bool, names: &mut Vec<String>| -> Result<()> {
            conn.execute(
                "INSERT INTO temp.rm_name (name, hidden) VALUES (lower(?1), ?2)
                 ON CONFLICT(name) DO UPDATE SET hidden = max(hidden, excluded.hidden)",
                params![name, hidden as i64],
            )?;
            names.push(name.to_string());
            Ok(())
        };
        for (ids, hidden) in [(&self.users, true), (&self.anon_users, false)] {
            for id in ids {
                let found: Option<String> = lookup.query_row([id], |r| r.get(0)).ok();
                if let Some(name) = found {
                    add_name(&name, hidden, &mut names)?;
                }
            }
        }
        for (set, hidden) in [(&self.user_names, true), (&self.anon_names, false)] {
            for name in set {
                add_name(name, hidden, &mut names)?;
            }
        }
        let hidden = "(p.id IN (SELECT id FROM temp.rm_post) OR p.author_id IN (SELECT id FROM temp.rm_user)
                       OR lower(p.author_name) IN (SELECT name FROM temp.rm_name WHERE hidden = 1))";
        let anon = "(p.author_id IN (SELECT id FROM temp.anon_user)
                     OR lower(p.author_name) IN (SELECT name FROM temp.rm_name WHERE hidden = 0))";
        let body = REMOVED_BODY.replace('\'', "''");
        conn.execute_batch(&format!(
            "CREATE TEMP VIEW posts AS
               SELECT p.id, p.thread_id, p.page, p.position,
                      CASE WHEN {hidden} OR {anon} THEN NULL ELSE p.author_id END AS author_id,
                      CASE WHEN {hidden} THEN '{REMOVED_AUTHOR}' WHEN {anon} THEN '{FORMER_MEMBER}' ELSE p.author_name END AS author_name,
                      p.posted_at,
                      CASE WHEN {hidden} THEN '{body}' ELSE p.body_html END AS body_html,
                      CASE WHEN {hidden} THEN '' ELSE p.body_text END AS body_text,
                      p.captured_at, p.source_url
                 FROM main.posts p
                WHERE p.thread_id NOT IN (SELECT id FROM temp.rm_thread);
             CREATE TEMP VIEW threads AS
               SELECT t.id, t.forum_id, t.title, t.path,
                      CASE WHEN lower(t.author_name) IN (SELECT name FROM temp.rm_name) THEN '{FORMER_MEMBER}'
                           ELSE t.author_name END AS author_name,
                      t.total_pages, t.captured_at
                 FROM main.threads t
                WHERE t.id NOT IN (SELECT id FROM temp.rm_thread);
             CREATE TEMP VIEW users AS
               SELECT u.id, u.name FROM main.users u
                WHERE u.id NOT IN (SELECT id FROM temp.rm_user) AND u.id NOT IN (SELECT id FROM temp.anon_user)
                  AND lower(u.name) NOT IN (SELECT name FROM temp.rm_name);
             CREATE TEMP VIEW attachments AS
               SELECT a.post_id, a.att_id, a.filename, a.path
                 FROM main.attachments a JOIN main.posts p ON p.id = a.post_id
                WHERE NOT {hidden} AND p.thread_id NOT IN (SELECT id FROM temp.rm_thread);"
        ))?;
        // Longest names first, so "Bob Smith" is replaced before "Bob".
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        names.dedup();
        Ok(Applied { conn, names })
    }
}

/// The removals in effect on a connection; dropping it removes the temporary views again.
pub struct Applied<'c> {
    conn: &'c Connection,
    names: Vec<String>,
}

impl Applied<'_> {
    /// Replace removed and anonymized members' names in quote attributions and mentions.
    pub fn scrub(&self, html: &str) -> String {
        scrub_names(html, &self.names)
    }
}

impl Drop for Applied<'_> {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch(
            "DROP VIEW IF EXISTS temp.posts; DROP VIEW IF EXISTS temp.threads;
             DROP VIEW IF EXISTS temp.users; DROP VIEW IF EXISTS temp.attachments;",
        );
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn scrub_names(html: &str, names: &[String]) -> String {
    let mut out = html.to_string();
    for raw in names {
        for name in [raw.clone(), escape(raw)] {
            if !out.contains(&name) {
                continue;
            }
            for (from, to) in [
                (format!("{name} said:"), "A former member said:".to_string()),
                (format!("{name} wrote:"), "A former member wrote:".to_string()),
                (format!(">{name}</a> wrote:"), ">A former member</a> wrote:".to_string()),
                (format!(">{name}</a> said:"), ">A former member</a> said:".to_string()),
                (format!("Quote from: {name}"), "Quote from: a former member".to_string()),
                (format!("data-quote=\"{name}\""), "data-quote=\"\"".to_string()),
                (format!("@{name}"), "@former member".to_string()),
            ] {
                out = out.replace(&from, &to);
            }
        }
    }
    out
}

/// Replace email addresses with "[email hidden]", anywhere in the string: post text, code
/// boxes, link addresses and usernames. Image names like `icon@2x.png` are left alone.
pub fn hide_emails(html: &str) -> String {
    if !html.contains('@') {
        return html.to_string();
    }
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut last = 0; // bytes of `html` copied so far
    let local = |c: u8| c.is_ascii_alphanumeric() || b"._%+-".contains(&c);
    let domain = |c: u8| c.is_ascii_alphanumeric() || c == b'.' || c == b'-';
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            let mut s = i;
            while s > last && local(b[s - 1]) {
                s -= 1;
            }
            let mut e = i + 1;
            while e < b.len() && domain(b[e]) {
                e += 1;
            }
            let dom = html[i + 1..e].trim_end_matches(['.', '-']);
            let e = i + 1 + dom.len();
            let tld = dom.rsplit('.').next().unwrap_or("");
            let looks_like_email = s < i
                && dom.contains('.')
                && tld.len() >= 2
                && tld.bytes().all(|c| c.is_ascii_alphabetic())
                && !["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "js", "css"].contains(&tld.to_ascii_lowercase().as_str());
            if looks_like_email {
                out.push_str(&html[last..s]);
                out.push_str("[email hidden]");
                last = e;
                i = e;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&html[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_links() {
        let r = Removals::parse(
            "# Nov 20: request by email\n\
             thread 12\n\
             post 34   # spam\n\
             user 5\n\
             user \"Some Name\"\n\
             anonymize 6\n\
             anonymize \"Other\"\n\
             https://thecodelair.com/rpgmaker/threads/77/page-2/#post-88  # pasted\n\
             https://thecodelair.com/rpgmaker/threads/99/\n\
             /rpgmaker/members/7/\n",
        )
        .unwrap();
        assert_eq!(r.threads, HashSet::from([12, 99]));
        assert_eq!(r.posts, HashSet::from([34, 88]));
        assert_eq!(r.users, HashSet::from([5, 7]));
        assert_eq!(r.user_names, HashSet::from(["Some Name".to_string()]));
        assert_eq!(r.anon_users, HashSet::from([6]));
        assert_eq!(r.anon_names, HashSet::from(["Other".to_string()]));
        assert_eq!(r.count(), 9);
        let err = Removals::parse("ok?\n").unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(Removals::parse("thread abc").is_err());
        assert!(Removals::parse("user \"\"").is_err());
        assert!(Removals::parse("  # only a comment\n\n").unwrap().is_empty());
    }

    #[test]
    fn hides_emails() {
        assert_eq!(hide_emails("mail me: john.doe+x@example.co.uk."), "mail me: [email hidden].");
        assert_eq!(hide_emails("<a href=\"?url=http://x@y.com\">a@b.org</a>"), "<a href=\"?url=http://[email hidden]\">[email hidden]</a>");
        assert_eq!(hide_emails("Connor &lt;smith@saelora.com&gt; <b>x</b>"), "Connor &lt;[email hidden]&gt; <b>x</b>");
        assert_eq!(hide_emails("code: <me@gmail.com>"), "code: <[email hidden]>");
        assert_eq!(hide_emails("icon@2x.png and @mention and a@b"), "icon@2x.png and @mention and a@b");
        assert_eq!(hide_emails("sprite@sheet.png"), "sprite@sheet.png");
        assert_eq!(hide_emails("Ünïcode x@y.nl ok"), "Ünïcode [email hidden] ok");
    }

    #[test]
    fn scrubs_quote_attributions() {
        let names = vec!["Bob Smith".to_string(), "A&B".to_string()];
        let html = "<blockquote data-quote=\"Bob Smith\"><a>Bob Smith said:</a></blockquote> hi @Bob Smith, <cite>A&amp;B wrote:</cite> Bob Smith is nice";
        assert_eq!(
            scrub_names(html, &names),
            "<blockquote data-quote=\"\"><a>A former member said:</a></blockquote> hi @former member, <cite>A former member wrote:</cite> Bob Smith is nice"
        );
    }
}
