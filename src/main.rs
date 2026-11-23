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
        /// Public address of the site, e.g. https://example.org (enables sitemap.xml,
        /// canonical links and structured data for search engines).
        #[arg(long)]
        public_url: Option<String>,
        /// A curated A–Z list: "name|Title|board ids", e.g.
        /// "plugins|Plugins & Scripts|184=MZ plugins,135=MV plugins". Repeatable.
        #[arg(long)]
        collection: Vec<String>,
        /// A more complete archive of the same forum elsewhere: "Name|https://...".
        /// Shown on the archive's front page and on pages that weren't preserved.
        #[arg(long)]
        see_also: Option<String>,
        /// Removal requests to apply: a text file with lines like "thread 123", "post 456",
        /// "user 78", "anonymize 78" or a pasted archive link. Applied on every build.
        #[arg(long)]
        removals: Option<PathBuf>,
        /// Name of the whole website the archive is part of, e.g. "Forum Ark". Added to every
        /// page title ("Thread · Archive · Forum Ark") and to link previews (og:site_name).
        #[arg(long)]
        site_name: Option<String>,
        /// Absolute address of a picture (1200×630 PNG) shown when a page is shared on
        /// Discord, Reddit, WhatsApp, ... Needs --public-url.
        #[arg(long)]
        og_image: Option<String>,
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
        /// Forum host, e.g. forums.rpgmakerweb.com. Include the board's folder if it
        /// lives in one, e.g. example.com/phpBB2
        #[arg(long)]
        host: String,
        /// Forum software, to know which addresses hold threads (auto = try all).
        #[arg(long, value_enum, default_value_t = Software::Auto)]
        software: Software,
        /// CDX rows per request.
        #[arg(long, default_value_t = 5000)]
        batch: u32,
        /// Only list these URL prefixes instead of the whole forum (repeatable).
        #[arg(long)]
        prefix: Vec<String>,
        /// Ignore captures made after this date (YYYYMMDD). Use it when the domain was taken
        /// over after the forum died, so spam pages don't replace the real ones.
        #[arg(long)]
        until: Option<String>,
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
    /// Parse downloaded pages again from the raw folder (after a tool update), without
    /// downloading anything. Needs the folder `fetch --raw-dir` saved them in.
    Reparse {
        #[arg(long)]
        raw_dir: PathBuf,
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
                let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
                let html = forumark::urls::decode_html(&bytes, None);
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
        Cmd::Build { out, origin, title, base, contact, removal_url, privacy_url, about_url, notice, threads_per_page, no_search, search_chunk, public_url, collection, see_also, removals, site_name, og_image } => {
            let collections = collection.iter().map(|c| forumark::site::Collection::parse(c)).collect::<Result<Vec<_>>>()?;
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
                public_url,
                collections,
                see_also: see_also
                    .map(|s| match s.split_once('|') {
                        Some((name, url)) if !name.trim().is_empty() && url.trim().starts_with("http") => {
                            Ok((name.trim().to_string(), url.trim().to_string()))
                        }
                        _ => Err(anyhow::anyhow!("--see-also must look like \"Name|https://...\"")),
                    })
                    .transpose()?,
                site_name,
                og_image,
                removals: match &removals {
                    Some(path) => {
                        let r = forumark::removals::Removals::load(path)?;
                        eprintln!("applying {} removal request(s) from {}", r.count(), path.display());
                        r
                    }
                    None => Default::default(),
                },
            };
            let r = forumark::site::build(&store, &opts)?;
            println!(
                "built {} pages into {} ({} thread pages, {} not preserved); search indexed {} pages; sitemap {} urls",
                r.pages,
                out.display(),
                r.thread_pages,
                r.missing_thread_pages,
                r.search_pages_indexed,
                r.sitemap_urls
            );
        }
        Cmd::Wayback(WaybackCmd::List { host, software, batch, prefix, until }) => {
            let until = until.as_deref().map(forumark::wayback::parse_until).transpose()?;
            let mut store = Store::open(&cli.db)?;
            let prefixes = if prefix.is_empty() { forumark::wayback::default_prefixes(&host, software) } else { prefix };
            let r = forumark::wayback::list(&mut store, &prefixes, batch, until.as_deref(), |prefix, rows| {
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
        Cmd::Wayback(WaybackCmd::Reparse { raw_dir }) => {
            let mut store = Store::open(&cli.db)?;
            let r = forumark::wayback::reparse(&mut store, &raw_dir, |r, total| {
                let done = r.ok + r.skipped + r.errors + r.missing;
                if done % 500 == 0 || done == total {
                    eprintln!("{done}/{total} pages (ok {}, skipped {}, errors {}, no raw copy {})", r.ok, r.skipped, r.errors, r.missing);
                }
            })?;
            println!("reparsed: ok {}, skipped {}, errors {}, no raw copy {}", r.ok, r.skipped, r.errors, r.missing);
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
