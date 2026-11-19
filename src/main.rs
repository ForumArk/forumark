use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use forumark::model::{Capture, Page};
use forumark::parse::{Software, parse_page};
use forumark::store::Store;

#[derive(Parser)]
#[command(name = "forumark", version, about = "Forum Ark: preserve forums as searchable static sites")]
struct Cli {
    /// SQLite archive database.
    #[arg(long, global = true, default_value = "archive.db")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import saved HTML pages from disk.
    ImportHtml {
        /// HTML files to import.
        files: Vec<PathBuf>,
        /// Original URL of the page. Only valid with a single file; otherwise the
        /// page's canonical link is used.
        #[arg(long)]
        url: Option<String>,
        /// Capture time as YYYYMMDDhhmmss (defaults to the file's modification time).
        #[arg(long)]
        captured_at: Option<String>,
        #[arg(long, value_enum, default_value_t = Software::Auto)]
        software: Software,
    },
    /// Build the static website from the archive.
    Build {
        /// Output directory.
        #[arg(long, default_value = "site")]
        out: PathBuf,
        /// Original forum address, e.g. https://forums.rpgmakerweb.com
        #[arg(long)]
        origin: String,
        /// Archive title shown on every page.
        #[arg(long, default_value = "Forum Archive")]
        title: String,
        /// URL prefix the site is served under (start and end with '/').
        #[arg(long, default_value = "/")]
        base: String,
        /// Your contact address for removal requests (required: people must be able to reach you).
        #[arg(long)]
        contact: String,
        /// Link to your removal / takedown policy page (shown in the footer).
        #[arg(long)]
        removal_url: Option<String>,
        /// Link to your privacy policy page (shown in the footer).
        #[arg(long)]
        privacy_url: Option<String>,
        /// Link to your about page (shown in the footer).
        #[arg(long)]
        about_url: Option<String>,
        #[arg(long, default_value = "This is an unofficial, read-only archive preserved for the community.")]
        notice: String,
        #[arg(long, default_value_t = 50)]
        threads_per_page: usize,
        /// Skip the search index (faster previews).
        #[arg(long)]
        no_search: bool,
        /// Thread pages per search index part (lower = less memory while building).
        #[arg(long, default_value_t = 12000)]
        search_chunk: usize,
    },
    /// Import from the Internet Archive's Wayback Machine.
    #[command(subcommand)]
    Wayback(WaybackCmd),
    /// Show what the archive contains.
    Stats,
}

#[derive(Subcommand)]
enum WaybackCmd {
    /// Step 1: list captured thread and forum pages (resumable).
    List {
        /// Forum host, e.g. forums.rpgmakerweb.com
        #[arg(long)]
        host: String,
        /// CDX rows per request.
        #[arg(long, default_value_t = 5000)]
        batch: u32,
        /// Only list these URL prefixes instead of the whole forum (repeatable).
        #[arg(long)]
        prefix: Vec<String>,
    },
    /// Step 2: download, parse and save queued pages (resumable).
    Fetch {
        #[arg(long, default_value_t = 3)]
        workers: usize,
        /// Delay per worker between requests, in milliseconds.
        #[arg(long, default_value_t = 1000)]
        delay_ms: u64,
        /// Only fetch this many pages (trial runs).
        #[arg(long)]
        limit: Option<usize>,
        /// Keep raw HTML (gzipped) in this folder for later re-parsing.
        #[arg(long)]
        raw_dir: Option<PathBuf>,
        /// Retry pages that failed earlier.
        #[arg(long)]
        retry_errors: bool,
        /// Only fetch pages whose key starts with this, e.g. /threads/ or /forums/
        #[arg(long)]
        only: Option<String>,
    },
    /// Show how many pages are pending, done, skipped or failed.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::ImportHtml { files, url, captured_at, software } => {
            anyhow::ensure!(url.is_none() || files.len() == 1, "--url can only be used with one file");
            let mut store = Store::open(&cli.db)?;
            let (mut threads, mut forums, mut skipped) = (0, 0, 0);
            for file in &files {
                let html = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
                let page_url = url.clone().unwrap_or_else(|| file.display().to_string());
                let when = match &captured_at {
                    Some(t) => t.clone(),
                    None => file_mtime(file)?,
                };
                match parse_page(&html, &page_url, software)? {
                    Some(page) => {
                        match &page {
                            Page::Thread(_) => threads += 1,
                            Page::Forum(_) => forums += 1,
                        }
                        let cap = Capture { url: page_url, captured_at: when, source: "file".into() };
                        store.save(&page, &cap)?;
                    }
                    None => {
                        skipped += 1;
                        eprintln!("skipped (not a thread or forum page): {}", file.display());
                    }
                }
            }
            println!("imported {threads} thread page(s), {forums} forum page(s); skipped {skipped}");
        }
        Cmd::Build { out, origin, title, base, contact, removal_url, privacy_url, about_url, notice, threads_per_page, no_search, search_chunk } => {
            let store = Store::open(&cli.db)?;
            let opts = forumark::site::BuildOptions {
                out_dir: out.clone(),
                base,
                title,
                origin,
                contact,
                removal_url,
                privacy_url,
                about_url,
                notice,
                threads_per_page,
                skip_search: no_search,
                search_chunk,
            };
            let r = forumark::site::build(&store, &opts)?;
            println!(
                "built {} pages into {} ({} thread pages, {} not preserved); search indexed {} pages",
                r.pages,
                out.display(),
                r.thread_pages,
                r.missing_thread_pages,
                r.search_pages_indexed
            );
        }
        Cmd::Wayback(WaybackCmd::List { host, batch, prefix }) => {
            let mut store = Store::open(&cli.db)?;
            let prefixes = if prefix.is_empty() { forumark::wayback::default_prefixes(&host) } else { prefix };
            let r = forumark::wayback::list(&mut store, &prefixes, batch, |prefix, rows| {
                eprintln!("{prefix}: {rows} capture rows read so far");
            })?;
            println!("read {} capture rows; {} pages queued or updated", r.rows_seen, r.pages_queued);
        }
        Cmd::Wayback(WaybackCmd::Fetch { workers, delay_ms, limit, raw_dir, retry_errors, only }) => {
            let mut store = Store::open(&cli.db)?;
            let opts = forumark::wayback::FetchOptions {
                workers,
                delay: std::time::Duration::from_millis(delay_ms),
                limit,
                raw_dir,
                retry_errors,
                only,
            };
            let r = forumark::wayback::fetch(&mut store, &opts, |r, total| {
                let done = r.ok + r.skipped + r.errors;
                if done % 25 == 0 || done == total {
                    eprintln!("{done}/{total} pages (ok {}, skipped {}, errors {})", r.ok, r.skipped, r.errors);
                }
            })?;
            println!("fetched: ok {}, skipped {}, errors {}", r.ok, r.skipped, r.errors);
        }
        Cmd::Wayback(WaybackCmd::Status) => {
            let store = Store::open(&cli.db)?;
            for (status, n) in forumark::wayback::queue_status(store.conn())? {
                println!("{status}: {n}");
            }
        }
        Cmd::Stats => {
            let s = Store::open(&cli.db)?.stats()?;
            println!(
                "forums: {}\nthreads: {}\nposts: {}\nusers: {}\nattachments: {}\ncaptures: {}",
                s.forums, s.threads, s.posts, s.users, s.attachments, s.captures
            );
        }
    }
    Ok(())
}

fn file_mtime(path: &PathBuf) -> Result<String> {
    let modified = fs::metadata(path)?.modified()?;
    let dt: chrono::DateTime<chrono::Utc> = modified.into();
    Ok(dt.format("%Y%m%d%H%M%S").to_string())
}
