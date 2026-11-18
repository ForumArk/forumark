//! SQLite storage for the normalized model.
//!
//! Every row remembers the capture time it came from. When the same post or
//! thread shows up in several captures, the newest capture wins, so later edits
//! replace earlier text but an older capture never overwrites a newer one.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

use crate::model::{Capture, Forum, ForumPage, Page, ThreadPage};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS forums (
    id          INTEGER PRIMARY KEY,
    title       TEXT NOT NULL,
    path        TEXT NOT NULL,
    parent_id   INTEGER,
    captured_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS threads (
    id          INTEGER PRIMARY KEY,
    forum_id    INTEGER,
    title       TEXT NOT NULL,
    path        TEXT NOT NULL,
    author_name TEXT,
    total_pages INTEGER NOT NULL DEFAULT 1,
    captured_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS threads_forum ON threads(forum_id);

CREATE TABLE IF NOT EXISTS posts (
    id          INTEGER PRIMARY KEY,
    thread_id   INTEGER NOT NULL,
    page        INTEGER NOT NULL,
    position    INTEGER NOT NULL,
    author_id   INTEGER,
    author_name TEXT NOT NULL,
    posted_at   TEXT,
    body_html   TEXT NOT NULL,
    body_text   TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    source_url  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS posts_thread ON posts(thread_id, page, position);
CREATE INDEX IF NOT EXISTS posts_author ON posts(author_id);

CREATE TABLE IF NOT EXISTS attachments (
    post_id  INTEGER NOT NULL,
    att_id   INTEGER,
    filename TEXT NOT NULL,
    path     TEXT NOT NULL,
    PRIMARY KEY (post_id, path)
);

CREATE TABLE IF NOT EXISTS captures (
    url         TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    PRIMARY KEY (url, captured_at)
);
"#;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub forums: i64,
    pub threads: i64,
    pub posts: i64,
    pub users: i64,
    pub attachments: i64,
    pub captures: i64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Save one parsed page in a single transaction.
    pub fn save(&mut self, page: &Page, cap: &Capture) -> Result<()> {
        let tx = self.conn.transaction()?;
        let kind = match page {
            Page::Thread(_) => "thread",
            Page::Forum(_) => "forum",
        };
        tx.execute(
            "INSERT OR IGNORE INTO captures (url, captured_at, source, kind) VALUES (?1, ?2, ?3, ?4)",
            params![cap.url, cap.captured_at, cap.source, kind],
        )?;
        match page {
            Page::Thread(t) => save_thread(&tx, t, cap)?,
            Page::Forum(f) => save_forum_page(&tx, f, cap)?,
        }
        tx.commit()?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        Self::stats_of(&self.conn)
    }

    pub fn stats_of(conn: &Connection) -> Result<Stats> {
        let count = |table: &str| -> Result<i64> {
            Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
        };
        Ok(Stats {
            forums: count("forums")?,
            threads: count("threads")?,
            posts: count("posts")?,
            users: count("users")?,
            attachments: count("attachments")?,
            captures: count("captures")?,
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Insert or update a forum chain (outermost first), linking each to its parent.
fn save_forum_chain(tx: &rusqlite::Transaction, chain: &[Forum], cap: &Capture) -> Result<Option<i64>> {
    let mut parent: Option<i64> = None;
    for f in chain {
        upsert_forum(tx, f, parent.or(f.parent_id), cap)?;
        parent = Some(f.id);
    }
    Ok(parent)
}

fn upsert_forum(tx: &rusqlite::Transaction, f: &Forum, parent: Option<i64>, cap: &Capture) -> Result<()> {
    tx.execute(
        "INSERT INTO forums (id, title, path, parent_id, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
            title = CASE WHEN excluded.captured_at >= forums.captured_at THEN excluded.title ELSE forums.title END,
            path  = CASE WHEN excluded.captured_at >= forums.captured_at THEN excluded.path  ELSE forums.path  END,
            parent_id = COALESCE(CASE WHEN excluded.captured_at >= forums.captured_at
                                      THEN excluded.parent_id END, forums.parent_id),
            captured_at = MAX(forums.captured_at, excluded.captured_at)",
        params![f.id, f.title, f.path, parent, cap.captured_at],
    )?;
    Ok(())
}

fn save_thread(tx: &rusqlite::Transaction, t: &ThreadPage, cap: &Capture) -> Result<()> {
    let forum_id = save_forum_chain(tx, &t.forums, cap)?;
    let first_author = if t.page == 1 { t.posts.first().map(|p| p.author.name.clone()) } else { None };
    tx.execute(
        "INSERT INTO threads (id, forum_id, title, path, author_name, total_pages, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
            forum_id = COALESCE(CASE WHEN excluded.captured_at >= threads.captured_at THEN excluded.forum_id END, threads.forum_id),
            title = CASE WHEN excluded.captured_at >= threads.captured_at THEN excluded.title ELSE threads.title END,
            path  = CASE WHEN excluded.captured_at >= threads.captured_at THEN excluded.path  ELSE threads.path  END,
            author_name = COALESCE(excluded.author_name, threads.author_name),
            total_pages = MAX(threads.total_pages, excluded.total_pages),
            captured_at = MAX(threads.captured_at, excluded.captured_at)",
        params![t.thread_id, forum_id, t.title, t.path, first_author, t.total_pages, cap.captured_at],
    )?;

    for (pos, p) in t.posts.iter().enumerate() {
        if let Some(uid) = p.author.id {
            tx.execute(
                "INSERT INTO users (id, name) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET name = excluded.name",
                params![uid, p.author.name],
            )?;
        }
        let existing: Option<String> = tx
            .query_row("SELECT captured_at FROM posts WHERE id = ?1", [p.id], |r| r.get(0))
            .optional()?;
        if existing.as_deref().is_some_and(|old| old > cap.captured_at.as_str()) {
            continue; // a newer capture of this post is already stored
        }
        tx.execute(
            "INSERT OR REPLACE INTO posts
               (id, thread_id, page, position, author_id, author_name, posted_at, body_html, body_text, captured_at, source_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                p.id, t.thread_id, t.page, pos as i64, p.author.id, p.author.name, p.posted_at,
                p.body_html, p.body_text, cap.captured_at, cap.url
            ],
        )?;
        tx.execute("DELETE FROM attachments WHERE post_id = ?1", [p.id])?;
        for a in &p.attachments {
            tx.execute(
                "INSERT OR IGNORE INTO attachments (post_id, att_id, filename, path) VALUES (?1, ?2, ?3, ?4)",
                params![p.id, a.id, a.filename, a.path],
            )?;
        }
    }
    Ok(())
}

fn save_forum_page(tx: &rusqlite::Transaction, f: &ForumPage, cap: &Capture) -> Result<()> {
    let mut chain = f.parents.clone();
    chain.push(f.forum.clone());
    let forum_id = save_forum_chain(tx, &chain, cap)?;
    for t in &f.threads {
        // Listing pages only give a summary: never overwrite data from a thread page.
        tx.execute(
            "INSERT INTO threads (id, forum_id, title, path, author_name, total_pages, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, '')
             ON CONFLICT(id) DO UPDATE SET
                forum_id = COALESCE(threads.forum_id, excluded.forum_id),
                author_name = COALESCE(threads.author_name, excluded.author_name)",
            // captured_at '' so any real thread-page capture always wins.
            params![t.id, forum_id, t.title, t.path, t.author],
        )?;
    }
    Ok(())
}
