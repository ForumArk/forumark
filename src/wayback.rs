//! Import a forum from the Internet Archive's Wayback Machine.
//!
//! Two steps, both resumable:
//! 1. `list`: ask the CDX API which thread and forum pages were captured, keeping the
//!    newest successful capture of each page (`wayback_queue` table).
//! 2. `fetch`: download each queued capture in its raw, un-rewritten form (`id_`),
//!    parse it and save it. Raw HTML is optionally kept gzipped on disk, so the
//!    archive can be re-parsed later without downloading it again.
//!
//! The fetcher is deliberately gentle: a few workers, a delay between requests,
//! and exponential backoff when the Archive is busy or offline.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::Capture;
use crate::parse::{Software, parse_page};
use crate::store::Store;
use crate::urls::{id_after, normalize_path, page_number};

const CDX: &str = "https://web.archive.org/cdx/search/cdx";
pub const USER_AGENT: &str = concat!("ForumArk/", env!("CARGO_PKG_VERSION"), " (forum preservation; +https://github.com/ForumArk/forumark)");

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS wayback_queue (
            key         TEXT PRIMARY KEY,   -- normalized page key, e.g. /threads/123/page-2
            url         TEXT NOT NULL,      -- original URL as captured
            timestamp   TEXT NOT NULL,      -- newest successful capture
            status      TEXT,               -- NULL = pending, ok, skipped, error:<msg>
            attempts    INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS wayback_pending ON wayback_queue(status);
        CREATE TABLE IF NOT EXISTS wayback_resume (
            prefix TEXT PRIMARY KEY,
            resume_key TEXT,
            done INTEGER NOT NULL DEFAULT 0
        );",
    )?;
    // Added in 0.6: the date cutoff a prefix was listed with (NULL = none).
    let has_until = conn
        .prepare("SELECT 1 FROM pragma_table_info('wayback_resume') WHERE name = 'until_ts'")?
        .exists([])?;
    if !has_until {
        conn.execute_batch("ALTER TABLE wayback_resume ADD COLUMN until_ts TEXT;")?;
    }
    Ok(())
}

/// Check a `--until` value: a Wayback timestamp prefix of 4 to 14 digits (YYYY, YYYYMMDD,
/// YYYYMMDDhhmmss…). Dashes are allowed for convenience (2016-05-31).
pub fn parse_until(s: &str) -> Result<String> {
    let digits: String = s.chars().filter(|c| *c != '-').collect();
    if !(4..=14).contains(&digits.len()) || !digits.chars().all(|c| c.is_ascii_digit()) {
        bail!("--until must be a date like 20160531 (YYYYMMDD), got {s:?}");
    }
    Ok(digits)
}

/// True if a 14-digit capture timestamp falls on or before the cutoff. A short cutoff
/// covers its whole period: 20160531 includes every capture made on May 31, 2016.
pub fn within_until(ts: &str, until: Option<&str>) -> bool {
    match until {
        None => true,
        Some(u) => ts.get(..u.len()).is_some_and(|head| head <= u),
    }
}

/// Where listing of `prefix` stands: (resume key, finished). A prefix remembers the cutoff it
/// was listed with, and resuming it with a different one is refused, because the half that
/// was already read would follow other rules than the rest.
pub fn resume_state(conn: &Connection, prefix: &str, until: Option<&str>) -> Result<(Option<String>, bool)> {
    let row: Option<(Option<String>, bool, Option<String>)> = conn
        .query_row("SELECT resume_key, done, until_ts FROM wayback_resume WHERE prefix = ?1", [prefix], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?))
        })
        .optional()?;
    let Some((resume, done, stored)) = row else { return Ok((None, false)) };
    if stored.as_deref() != until {
        let show = |u: Option<&str>| u.map_or("no cutoff".to_string(), |u| format!("--until {u}"));
        bail!(
            "{prefix} was listed with {} before; this run uses {}. Use the same cutoff to resume, \
             or start over with a new database",
            show(stored.as_deref()),
            show(until)
        );
    }
    Ok((resume, done))
}

fn cdx_list_url(prefix: &str, batch: u32, resume: Option<&str>, until: Option<&str>) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(CDX)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("url", prefix)
            .append_pair("matchType", "prefix")
            .append_pair("filter", "statuscode:200")
            .append_pair("filter", "mimetype:text/html")
            .append_pair("fl", "timestamp,original")
            .append_pair("limit", &batch.to_string())
            .append_pair("showResumeKey", "true");
        if let Some(u) = until {
            q.append_pair("to", u);
        }
        if let Some(k) = resume {
            q.append_pair("resumeKey", k);
        }
    }
    Ok(url)
}

/// Key a captured URL by the page it shows, so slug changes, http/https,
/// `index.php?` style and tracking parameters all collapse into one entry.
/// Returns `None` for URLs that are not plain thread or forum pages.
pub fn capture_key(url: &str) -> Option<String> {
    let raw = crate::urls::strip_wayback(url);
    if let Some(key) = phpbb_capture_key(raw) {
        return key;
    }
    if let Some(key) = smf_capture_key(raw) {
        return key;
    }
    // Real query strings (not the XenForo 1 `index.php?route` form) mean filtered
    // or sorted listings, reply forms, etc. Skip them.
    let after_host = raw.split("://").nth(1).unwrap_or(raw);
    if let Some((before, query)) = after_host.split_once('?') {
        if !before.ends_with("index.php") {
            return None;
        }
        // XenForo 1: index.php?threads/x.1/page-2&foo=bar → extra params mean a variant page.
        if query.contains('&') && !query.split('&').skip(1).all(|p| p.is_empty() || p.starts_with("utm_")) {
            return None;
        }
    }
    let path = normalize_path(raw);
    for route in ["threads", "forums"] {
        let Some(id) = id_after(&path, route) else { continue };
        // Accept only `/route/slug.id/` and `/route/slug.id/page-N`.
        let start = path.find(&format!("/{route}/"))? + route.len() + 2;
        let rest: Vec<&str> = path[start..].split('/').filter(|s| !s.is_empty()).collect();
        let ok = match rest.len() {
            1 => true,
            2 => rest[1].starts_with("page-") && rest[1][5..].parse::<u32>().is_ok(),
            _ => false,
        };
        if !ok {
            return None;
        }
        let page = page_number(&path);
        return Some(if page > 1 { format!("/{route}/{id}/page-{page}") } else { format!("/{route}/{id}") });
    }
    None
}

/// phpBB keys: `/threads/<t>[/start-N]`, `/forums/<f>[/start-N]`, `/posts/<p>`.
/// Returns `None` if the URL isn't a phpBB page at all, `Some(None)` for phpBB
/// pages we skip (sorted, printed, filtered or reply-form variants).
fn phpbb_capture_key(raw: &str) -> Option<Option<String>> {
    use crate::urls::{PhpbbLink, phpbb_link, query_params};
    let link = phpbb_link(raw)?;
    let params = query_params(raw);
    let ok_param = |k: &str, v: &str| match k {
        "t" | "f" | "p" | "start" | "sid" | "highlight" | "hilit" => true,
        "postdays" | "topicdays" | "st" => v == "0",
        "postorder" => v == "asc",
        "sk" => v == "t",
        "sd" => v == "a" || v == "d" && raw.contains("viewforum"),
        "utm_source" | "utm_medium" | "utm_campaign" => true,
        _ => false,
    };
    if !params.iter().all(|(k, v)| ok_param(k, v)) {
        return Some(None);
    }
    let with_start = |route: &str, id: i64, start: u32| {
        if start > 0 { format!("/{route}/{id}/start-{start}") } else { format!("/{route}/{id}") }
    };
    let has_post = params.iter().any(|(k, v)| k == "p" && v.parse::<i64>().is_ok());
    Some(match link {
        // `viewtopic.php?t=1&p=55` shows the page holding post 55, whatever `start` says.
        PhpbbLink::Topic { .. } if has_post => {
            let p = params.iter().rev().find(|(k, _)| k == "p").and_then(|(_, v)| v.parse::<i64>().ok())?;
            Some(format!("/posts/{p}"))
        }
        PhpbbLink::Topic { id, start } => Some(with_start("threads", id, start)),
        PhpbbLink::Post { id } => Some(format!("/posts/{id}")),
        PhpbbLink::Forum { id, start } => Some(with_start("forums", id, start)),
        PhpbbLink::Member { .. } => None,
    })
}

/// SMF keys, same shapes as phpBB. `None` = not an SMF address; `Some(None)` = an
/// SMF variant we skip (print view, "all posts", next/previous, sorted lists, ...).
fn smf_capture_key(raw: &str) -> Option<Option<String>> {
    use crate::urls::{SmfLink, smf_link, smf_params};
    let params = smf_params(raw)?;
    if !params.iter().any(|(k, _)| k == "topic" || k == "board" || k == "msg") {
        return None;
    }
    let ok = |k: &str| matches!(k, "topic" | "board" | "msg" | "phpsessid" | "topicseen" | "") || k.starts_with("utm_");
    if !params.iter().all(|(k, _)| ok(k)) {
        return Some(None);
    }
    let with_start = |route: &str, id: i64, start: u32| {
        if start > 0 { format!("/{route}/{id}/start-{start}") } else { format!("/{route}/{id}") }
    };
    Some(match smf_link(raw) {
        Some(SmfLink::Topic { id, start }) => Some(with_start("threads", id, start)),
        Some(SmfLink::Post { id }) => Some(format!("/posts/{id}")),
        Some(SmfLink::Board { id, start }) => Some(with_start("forums", id, start)),
        _ => None,
    })
}

fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(timeout_secs))
        .build()?)
}

/// GET with retries and exponential backoff for "busy/offline" answers.
fn get_with_retry(client: &reqwest::blocking::Client, url: &str, tries: u32) -> Result<(u16, String)> {
    let mut wait = Duration::from_secs(5);
    let mut last_err = anyhow::anyhow!("no attempt made");
    for attempt in 0..tries {
        if attempt > 0 {
            thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(300));
        }
        match client.get(url).send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let bytes = resp.bytes().map(|b| b.to_vec()).unwrap_or_default();
                let body = crate::urls::decode_html(&bytes, content_type.as_deref());
                // The Archive answers "Temporarily Offline" pages with various codes.
                let offline = body.contains("<title>Internet Archive: Temporarily Offline</title>");
                if status == 429 || status >= 500 || offline {
                    last_err = anyhow::anyhow!("HTTP {status}{}", if offline { " (Archive offline)" } else { "" });
                    continue;
                }
                return Ok((status, body));
            }
            Err(e) => last_err = e.into(),
        }
    }
    Err(last_err)
}

/// URL prefixes (scheme-less) that hold thread and forum pages.
///
/// `host` may include the board's folder, e.g. `example.com/phpBB2` for a phpBB
/// board that lives in a sub-folder.
pub fn default_prefixes(host: &str, software: Software) -> Vec<String> {
    let host = host.trim_end_matches('/');
    let xenforo = [
        format!("{host}/threads/"),
        format!("{host}/forums/"),
        format!("{host}/index.php?threads/"),
        format!("{host}/index.php?forums/"),
    ];
    let phpbb = [format!("{host}/viewtopic.php"), format!("{host}/viewforum.php")];
    let smf = [
        format!("{host}/index.php?topic="),
        format!("{host}/index.php?board="),
        format!("{host}/index.php/topic,"),
        format!("{host}/index.php/board,"),
    ];
    match software {
        Software::Xenforo => xenforo.to_vec(),
        Software::Phpbb => phpbb.to_vec(),
        Software::Smf => smf.to_vec(),
        Software::Auto => xenforo.into_iter().chain(phpbb).chain(smf).collect(),
    }
}

#[derive(Debug, Default)]
pub struct ListReport {
    pub rows_seen: u64,
    pub pages_queued: u64,
}

/// Step 1: list captures through the CDX API (resumable per prefix). With `until`, captures
/// made after that date are ignored (for domains that were taken over by spam after the forum died).
pub fn list(
    store: &mut Store,
    prefixes: &[String],
    batch: u32,
    until: Option<&str>,
    mut progress: impl FnMut(&str, u64),
) -> Result<ListReport> {
    ensure_schema(store.conn())?;
    let http = client(180)?;
    let mut report = ListReport::default();
    for prefix in prefixes {
        let (mut resume, done) = resume_state(store.conn(), prefix, until)?;
        if done {
            continue;
        }
        loop {
            let url = cdx_list_url(prefix, batch, resume.as_deref(), until)?;
            let (status, body) = get_with_retry(&http, url.as_str(), 8)?;
            if status != 200 {
                bail!("CDX returned HTTP {status} for {prefix}");
            }
            // Lines of "timestamp original", then optionally a blank line and the resume key.
            let mut next_resume = None;
            let mut lines = body.lines().peekable();
            let tx = store.conn_mut().transaction()?;
            while let Some(line) = lines.next() {
                if line.trim().is_empty() {
                    next_resume = lines.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                    break;
                }
                let Some((ts, orig)) = line.split_once(' ') else { continue };
                report.rows_seen += 1;
                if !within_until(ts, until) {
                    continue; // CDX already applies `to`; this is a safety net
                }
                let Some(key) = capture_key(orig) else { continue };
                let changed = tx.execute(
                    "INSERT INTO wayback_queue (key, url, timestamp) VALUES (?1, ?2, ?3)
                     ON CONFLICT(key) DO UPDATE SET url = excluded.url, timestamp = excluded.timestamp,
                         status = CASE WHEN wayback_queue.status = 'ok' THEN NULL ELSE wayback_queue.status END
                     WHERE excluded.timestamp > wayback_queue.timestamp",
                    params![key, orig, ts],
                )?;
                report.pages_queued += changed as u64;
            }
            let finished = next_resume.is_none();
            tx.execute(
                "INSERT INTO wayback_resume (prefix, resume_key, done, until_ts) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(prefix) DO UPDATE SET resume_key = excluded.resume_key, done = excluded.done,
                     until_ts = excluded.until_ts",
                params![prefix, next_resume, finished as i64, until],
            )?;
            tx.commit()?;
            progress(prefix, report.rows_seen);
            if finished {
                break;
            }
            resume = next_resume;
        }
    }
    Ok(report)
}

pub struct FetchOptions {
    pub workers: usize,
    /// Pause per worker between requests.
    pub delay: Duration,
    /// Stop after this many pages (for trial runs).
    pub limit: Option<usize>,
    /// Keep raw HTML here (gzipped), keyed like the queue.
    pub raw_dir: Option<PathBuf>,
    /// Also retry pages that failed before.
    pub retry_errors: bool,
    /// Only fetch keys starting with this, e.g. `/threads/`.
    pub only: Option<String>,
}

#[derive(Debug, Default)]
pub struct FetchReport {
    pub ok: usize,
    pub skipped: usize,
    pub errors: usize,
}

struct Job {
    key: String,
    url: String,
    timestamp: String,
}

/// Step 2: download, parse and save queued captures.
pub fn fetch(store: &mut Store, opts: &FetchOptions, mut progress: impl FnMut(&FetchReport, usize)) -> Result<FetchReport> {
    ensure_schema(store.conn())?;
    let mut where_clause =
        if opts.retry_errors { "(status IS NULL OR status LIKE 'error:%')" } else { "status IS NULL" }.to_string();
    if let Some(only) = &opts.only {
        // Keys are generated by `capture_key`, so they never contain quotes.
        anyhow::ensure!(!only.contains('\''), "invalid --only value");
        where_clause.push_str(&format!(" AND key LIKE '{only}%'"));
    }
    let mut jobs: Vec<Job> = store
        .conn()
        .prepare(&format!(
            // Forum listings first (they build the board tree), then threads in ID order,
            // then phpBB post links (most of which turn out to be on pages we already have).
            "SELECT key, url, timestamp FROM wayback_queue WHERE {where_clause}
              ORDER BY CASE WHEN key LIKE '/forums/%' THEN 0 WHEN key LIKE '/threads/%' THEN 1 ELSE 2 END, key"
        ))?
        .query_map([], |r| Ok(Job { key: r.get(0)?, url: r.get(1)?, timestamp: r.get(2)? }))?
        .collect::<rusqlite::Result<_>>()?;
    if let Some(n) = opts.limit {
        jobs.truncate(n);
    }
    let total = jobs.len();
    if let Some(dir) = &opts.raw_dir {
        fs::create_dir_all(dir)?;
    }

    // phpBB `viewtopic.php?p=N` captures show a page of some topic. Skip the ones
    // whose post we already have (usually from a plain topic-page capture).
    let known_posts: Arc<Mutex<HashSet<i64>>> = Arc::new(Mutex::new(HashSet::new()));
    if jobs.iter().any(|j| j.key.starts_with("/posts/")) {
        let mut set = known_posts.lock().unwrap();
        for id in store.conn().prepare("SELECT id FROM posts")?.query_map([], |r| r.get::<_, i64>(0))? {
            set.insert(id?);
        }
    }

    let queue = Arc::new(Mutex::new(jobs.into_iter()));
    // `None` = not downloaded because we already have that post.
    let (tx, rx) = mpsc::channel::<(Job, Option<Result<(u16, String)>>)>();
    let mut handles = Vec::new();
    for _ in 0..opts.workers.max(1) {
        let queue = Arc::clone(&queue);
        let known_posts = Arc::clone(&known_posts);
        let tx = tx.clone();
        let delay = opts.delay;
        handles.push(thread::spawn(move || -> Result<()> {
            let http = client(120)?;
            loop {
                let next = queue.lock().unwrap().next();
                let Some(job) = next else { break };
                let post_id = job.key.strip_prefix("/posts/").and_then(|p| p.parse::<i64>().ok());
                if post_id.is_some_and(|p| known_posts.lock().unwrap().contains(&p)) {
                    if tx.send((job, None)).is_err() {
                        break;
                    }
                    continue;
                }
                let url = format!("https://web.archive.org/web/{}id_/{}", job.timestamp, job.url);
                let res = get_with_retry(&http, &url, 6);
                if tx.send((job, Some(res))).is_err() {
                    break;
                }
                thread::sleep(delay);
            }
            Ok(())
        }));
    }
    drop(tx);

    let mut report = FetchReport::default();
    for (job, res) in rx {
        let status = match res {
            None => {
                report.skipped += 1;
                "skipped:have-post".to_string()
            }
            Some(Ok((200, html))) => {
                if let Some(dir) = &opts.raw_dir {
                    save_raw(dir, &job, &html)?;
                }
                match parse_page(&html, &job.url, Software::Auto) {
                    Ok(Some(page)) => {
                        let cap = Capture { url: job.url.clone(), captured_at: job.timestamp.clone(), source: "wayback".into() };
                        store.save(&page, &cap)?;
                        if let crate::model::Page::Thread(t) = &page {
                            known_posts.lock().unwrap().extend(t.posts.iter().map(|p| p.id));
                        }
                        report.ok += 1;
                        "ok".to_string()
                    }
                    Ok(None) => {
                        report.skipped += 1;
                        "skipped".to_string()
                    }
                    Err(e) => {
                        report.errors += 1;
                        format!("error:parse {e}")
                    }
                }
            }
            Some(Ok((code, _))) => {
                report.errors += 1;
                format!("error:HTTP {code}")
            }
            Some(Err(e)) => {
                report.errors += 1;
                format!("error:{e}")
            }
        };
        store.conn().execute(
            "UPDATE wayback_queue SET status = ?1, attempts = attempts + 1 WHERE key = ?2",
            params![status, job.key],
        )?;
        progress(&report, total);
    }
    for h in handles {
        h.join().map_err(|_| anyhow::anyhow!("worker panicked"))??;
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct ReparseReport {
    pub ok: usize,
    pub skipped: usize,
    pub errors: usize,
    /// Downloaded pages with no raw copy on disk (left as they were).
    pub missing: usize,
}

/// Parse downloaded pages again from their raw copies (after a parser fix), without
/// downloading anything. Covers every page that was downloaded (`ok`, `skipped`, parse errors).
pub fn reparse(store: &mut Store, raw_dir: &std::path::Path, mut progress: impl FnMut(&ReparseReport, usize)) -> Result<ReparseReport> {
    ensure_schema(store.conn())?;
    let jobs: Vec<Job> = store
        .conn()
        .prepare(
            "SELECT key, url, timestamp FROM wayback_queue
              WHERE status IN ('ok', 'skipped') OR status LIKE 'error:parse%'
              ORDER BY CASE WHEN key LIKE '/forums/%' THEN 0 WHEN key LIKE '/threads/%' THEN 1 ELSE 2 END, key",
        )?
        .query_map([], |r| Ok(Job { key: r.get(0)?, url: r.get(1)?, timestamp: r.get(2)? }))?
        .collect::<rusqlite::Result<_>>()?;
    let total = jobs.len();
    let mut report = ReparseReport::default();
    for job in jobs {
        let path = raw_dir.join(raw_name(&job));
        let Ok(file) = fs::File::open(&path) else {
            report.missing += 1;
            progress(&report, total);
            continue;
        };
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(file), &mut bytes)
            .with_context(|| format!("reading {}", path.display()))?;
        // Raw copies are saved as UTF-8 (already decoded when downloaded).
        let html = String::from_utf8_lossy(&bytes).into_owned();
        let status = match parse_page(&html, &job.url, Software::Auto) {
            Ok(Some(page)) => {
                let cap = Capture { url: job.url.clone(), captured_at: job.timestamp.clone(), source: "wayback".into() };
                store.save(&page, &cap)?;
                report.ok += 1;
                "ok".to_string()
            }
            Ok(None) => {
                report.skipped += 1;
                "skipped".to_string()
            }
            Err(e) => {
                report.errors += 1;
                format!("error:parse {e}")
            }
        };
        store.conn().execute("UPDATE wayback_queue SET status = ?1 WHERE key = ?2", params![status, job.key])?;
        progress(&report, total);
    }
    Ok(report)
}

fn raw_name(job: &Job) -> String {
    format!("{}_{}.html.gz", job.key.trim_start_matches('/').replace('/', "_"), job.timestamp)
}

fn save_raw(dir: &std::path::Path, job: &Job, html: &str) -> Result<()> {
    let name = raw_name(job);
    let path = dir.join(name);
    let file = fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    // First line records the original URL so the file can be re-imported on its own.
    writeln!(gz, "<!-- forumark-source: {} {} -->", job.url, job.timestamp)?;
    gz.write_all(html.as_bytes())?;
    gz.finish()?;
    Ok(())
}

/// Queue status counts, for progress display.
pub fn queue_status(conn: &Connection) -> Result<Vec<(String, i64)>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT CASE WHEN status IS NULL THEN 'pending' WHEN status LIKE 'error:%' THEN 'error' ELSE status END s,
                COUNT(*) FROM wayback_queue GROUP BY s ORDER BY s",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_cutoff() {
        assert_eq!(parse_until("20160531").unwrap(), "20160531");
        assert_eq!(parse_until("2016-05-31").unwrap(), "20160531");
        assert!(parse_until("31-05-2016x").is_err());
        assert!(parse_until("201").is_err());
        assert!(within_until("20160531235959", Some("20160531")));
        assert!(!within_until("20160601000000", Some("20160531")));
        assert!(within_until("20990101000000", None));
        let url = cdx_list_url("f.com/threads/", 10, Some("k"), Some("20160531")).unwrap();
        assert!(url.as_str().contains("to=20160531"), "{url}");
        assert!(!cdx_list_url("f.com/threads/", 10, None, None).unwrap().as_str().contains("to="));
    }

    #[test]
    fn until_is_remembered_per_prefix() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn();
        // A database made before 0.6 has no until_ts column yet.
        conn.execute_batch(
            "CREATE TABLE wayback_resume (prefix TEXT PRIMARY KEY, resume_key TEXT, done INTEGER NOT NULL DEFAULT 0);
             INSERT INTO wayback_resume VALUES ('old.com/', 'r1', 0);",
        )
        .unwrap();
        ensure_schema(conn).unwrap();
        ensure_schema(conn).unwrap(); // running it twice is fine
        assert_eq!(resume_state(conn, "old.com/", None).unwrap(), (Some("r1".into()), false));
        assert!(resume_state(conn, "old.com/", Some("2016")).is_err());
        conn.execute("INSERT INTO wayback_resume VALUES ('spam.com/', 'r2', 0, '20160531')", []).unwrap();
        assert_eq!(resume_state(conn, "spam.com/", Some("20160531")).unwrap(), (Some("r2".into()), false));
        let err = resume_state(conn, "spam.com/", None).unwrap_err().to_string();
        assert!(err.contains("--until 20160531"), "{err}");
        assert!(resume_state(conn, "spam.com/", Some("20170101")).is_err());
        assert_eq!(resume_state(conn, "new.com/", Some("2016")).unwrap(), (None, false));
    }

    #[test]
    fn keys() {
        assert_eq!(capture_key("https://f.com/threads/a-b.95474/").as_deref(), Some("/threads/95474"));
        assert_eq!(capture_key("http://f.com/threads/renamed.95474/page-3").as_deref(), Some("/threads/95474/page-3"));
        assert_eq!(capture_key("http://f.com/index.php?threads/a.9/page-2").as_deref(), Some("/threads/9/page-2"));
        assert_eq!(capture_key("http://f.com/forums/plugins.12/page-4").as_deref(), Some("/forums/12/page-4"));
        assert_eq!(capture_key("https://f.com/threads/a.1/post-55"), None);
        assert_eq!(capture_key("https://f.com/threads/a.1/reply"), None);
        assert_eq!(capture_key("https://f.com/forums/a.12/?prefix_id=3"), None);
        assert_eq!(capture_key("https://f.com/threads/a.1/page-2?foo=1"), None);
        assert_eq!(capture_key("https://f.com/members/bob.1/"), None);
        assert_eq!(capture_key("https://f.com/threads/a.1/page-1").as_deref(), Some("/threads/1"));
    }

    #[test]
    fn phpbb_keys() {
        assert_eq!(capture_key("http://f.com:80/phpBB/viewtopic.php?t=200004").as_deref(), Some("/threads/200004"));
        assert_eq!(
            capture_key("http://f.com/phpBB/viewtopic.php?t=9&postdays=0&postorder=asc&start=15").as_deref(),
            Some("/threads/9/start-15")
        );
        assert_eq!(
            capture_key("https://f.com/community/viewtopic.php?f=1&t=427852&p=6418955&sid=2fb6").as_deref(),
            Some("/posts/6418955")
        );
        assert_eq!(capture_key("https://f.com/viewtopic.php?p=55#p55").as_deref(), Some("/posts/55"));
        assert_eq!(capture_key("https://f.com/viewforum.php?f=3&start=50").as_deref(), Some("/forums/3/start-50"));
        assert_eq!(capture_key("https://f.com/viewtopic.php?t=9&view=print"), None);
        assert_eq!(capture_key("https://f.com/viewtopic.php?t=9&postorder=desc"), None);
        assert_eq!(capture_key("https://f.com/viewtopic.php"), None);
        assert_eq!(capture_key("https://f.com/memberlist.php?mode=viewprofile&u=3"), None);
    }

    #[test]
    fn smf_keys() {
        assert_eq!(capture_key("https://f.com/index.php?topic=4.0").as_deref(), Some("/threads/4"));
        assert_eq!(capture_key("https://f.com/index.php?topic=4.25").as_deref(), Some("/threads/4/start-25"));
        assert_eq!(capture_key("http://f.com:80/index.php/topic,20005.0.html").as_deref(), Some("/threads/20005"));
        assert_eq!(capture_key("http://f.com:80/index.php?topic=20005.msg253566").as_deref(), Some("/posts/253566"));
        assert_eq!(capture_key("https://f.com/forum/index.php?board=100.0").as_deref(), Some("/forums/100"));
        assert_eq!(capture_key("https://f.com/index.php?board=1").as_deref(), Some("/forums/1"));
        assert_eq!(capture_key("https://f.com/index.php?topic=4.0;all"), None);
        assert_eq!(capture_key("https://f.com/index.php?topic=10015.0;prev_next=next"), None);
        assert_eq!(capture_key("http://f.com/index.php/topic,20005.0/prev_next,prev.html"), None);
        assert_eq!(capture_key("https://f.com/index.php?action=printpage;topic=4.0"), None);
        assert_eq!(capture_key("https://f.com/index.php?topic=4.new"), None);
        assert_eq!(capture_key("https://f.com/index.php?threads/a.9/page-2").as_deref(), Some("/threads/9/page-2"));
    }
}
