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

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

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
    Ok(())
}

/// Key a captured URL by the page it shows, so slug changes, http/https,
/// `index.php?` style and tracking parameters all collapse into one entry.
/// Returns `None` for URLs that are not plain thread or forum pages.
pub fn capture_key(url: &str) -> Option<String> {
    let raw = crate::urls::strip_wayback(url);
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
                let body = resp.text().unwrap_or_default();
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

/// URL prefixes (scheme-less) that hold thread and forum pages of a XenForo forum.
pub fn default_prefixes(host: &str) -> Vec<String> {
    let host = host.trim_end_matches('/');
    vec![
        format!("{host}/threads/"),
        format!("{host}/forums/"),
        format!("{host}/index.php?threads/"),
        format!("{host}/index.php?forums/"),
    ]
}

#[derive(Debug, Default)]
pub struct ListReport {
    pub rows_seen: u64,
    pub pages_queued: u64,
}

/// Step 1: list captures through the CDX API (resumable per prefix).
pub fn list(store: &mut Store, prefixes: &[String], batch: u32, mut progress: impl FnMut(&str, u64)) -> Result<ListReport> {
    ensure_schema(store.conn())?;
    let http = client(180)?;
    let mut report = ListReport::default();
    for prefix in prefixes {
        let (mut resume, done): (Option<String>, bool) = store
            .conn()
            .query_row("SELECT resume_key, done FROM wayback_resume WHERE prefix = ?1", [prefix], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? != 0))
            })
            .unwrap_or((None, false));
        if done {
            continue;
        }
        loop {
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
                if let Some(k) = &resume {
                    q.append_pair("resumeKey", k);
                }
            }
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
                "INSERT INTO wayback_resume (prefix, resume_key, done) VALUES (?1, ?2, ?3)
                 ON CONFLICT(prefix) DO UPDATE SET resume_key = excluded.resume_key, done = excluded.done",
                params![prefix, next_resume, finished as i64],
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
            // Forum listings first (they build the board tree), then threads in ID order.
            "SELECT key, url, timestamp FROM wayback_queue WHERE {where_clause}
              ORDER BY key LIKE '/threads/%', key"
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

    let queue = Arc::new(Mutex::new(jobs.into_iter()));
    let (tx, rx) = mpsc::channel::<(Job, Result<(u16, String)>)>();
    let mut handles = Vec::new();
    for _ in 0..opts.workers.max(1) {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let delay = opts.delay;
        handles.push(thread::spawn(move || -> Result<()> {
            let http = client(120)?;
            loop {
                let next = queue.lock().unwrap().next();
                let Some(job) = next else { break };
                let url = format!("https://web.archive.org/web/{}id_/{}", job.timestamp, job.url);
                let res = get_with_retry(&http, &url, 6);
                if tx.send((job, res)).is_err() {
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
            Ok((200, html)) => {
                if let Some(dir) = &opts.raw_dir {
                    save_raw(dir, &job, &html)?;
                }
                match parse_page(&html, &job.url, Software::Xenforo) {
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
                }
            }
            Ok((code, _)) => {
                report.errors += 1;
                format!("error:HTTP {code}")
            }
            Err(e) => {
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

fn save_raw(dir: &std::path::Path, job: &Job, html: &str) -> Result<()> {
    let name = format!("{}_{}.html.gz", job.key.trim_start_matches('/').replace('/', "_"), job.timestamp);
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
    use super::capture_key;

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
}
