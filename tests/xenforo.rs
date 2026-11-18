//! Parser and storage tests against hand-made XenForo 1 and 2 pages.
//! These copy the structure of real XenForo markup. Real Wayback captures
//! will be added as fixtures once the Wayback importer (M2) can fetch them.

use forumark::model::{Capture, Page};
use forumark::parse::{Software, parse_page};
use forumark::store::Store;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn cap(url: &str, at: &str) -> Capture {
    Capture { url: url.into(), captured_at: at.into(), source: "test".into() }
}

#[test]
fn xf2_thread_page() {
    let page = parse_page(&fixture("xf2_thread.html"), "file.html", Software::Auto).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.thread_id, 4242);
    assert_eq!(t.title, "MZ Simple Lighting Plugin");
    assert_eq!(t.path, "/threads/mz-simple-lighting-plugin.4242/");
    assert_eq!((t.page, t.total_pages), (2, 3));

    let crumbs: Vec<_> = t.forums.iter().map(|f| (f.id, f.title.as_str())).collect();
    assert_eq!(crumbs, vec![(3, "RPG Maker MZ"), (12, "JS Plugin Releases (RMMZ)")]);

    assert_eq!(t.posts.len(), 2);
    let p = &t.posts[0];
    assert_eq!(p.id, 900001);
    assert_eq!(p.author.name, "LightSmith");
    assert_eq!(p.author.id, Some(777));
    assert_eq!(p.posted_at.as_deref(), Some("2021-03-04T18:22:05+00:00"));
    assert!(p.body_html.contains("<b>Changelog:</b>"));
    assert!(p.body_text.contains("Version 1.2 is out!"));
    assert!(!p.body_text.contains("signature"), "signature must not be part of the body");
    assert_eq!(p.attachments.len(), 1);
    assert_eq!(p.attachments[0].filename, "lighting-demo.png");
    assert_eq!(p.attachments[0].id, Some(1555));

    assert_eq!(t.posts[1].author.id, Some(1234));
    assert_eq!(t.posts[1].body_text, "Thanks, works great!");
}

#[test]
fn xf1_thread_from_wayback() {
    let html = fixture("xf1_thread_wayback.html");
    let Page::Thread(t) = parse_page(&html, "x", Software::Auto).unwrap().unwrap() else { panic!() };
    assert_eq!(t.thread_id, 31337);
    assert_eq!(t.title, "Old Battle System Help");
    assert_eq!(t.path, "/threads/old-battle-system-help.31337/");
    assert_eq!((t.page, t.total_pages), (1, 4));

    let crumbs: Vec<_> = t.forums.iter().map(|f| (f.id, f.path.as_str())).collect();
    assert_eq!(crumbs, vec![(9, "/categories/rpg-maker-vx-ace.9/"), (44, "/forums/rgss3-support.44/")]);

    assert_eq!(t.posts.len(), 2);
    let p = &t.posts[0];
    assert_eq!(p.id, 500100);
    assert_eq!((p.author.name.as_str(), p.author.id), ("ScriptKid", Some(88)));
    assert_eq!(p.posted_at.as_deref(), Some("2015-01-01T00:00:00+00:00"));
    assert!(!p.body_html.contains("messageTextEndMarker"));
    assert_eq!(p.attachments[0].path, "/attachments/screenshot-png.2020/");
    assert_eq!(p.attachments[0].filename, "screenshot.png");

    assert_eq!(t.posts[1].posted_at.as_deref(), Some("2015-01-02T15:04:00"));
    assert_eq!(t.posts[1].body_text, "Use the Database > Troops tab.");
}

#[test]
fn xf2_forum_listing() {
    let Page::Forum(f) = parse_page(&fixture("xf2_forum.html"), "x", Software::Auto).unwrap().unwrap() else {
        panic!()
    };
    assert_eq!(f.forum.id, 12);
    assert_eq!(f.forum.title, "JS Plugin Releases (RMMZ)");
    assert_eq!(f.parents.iter().map(|p| p.id).collect::<Vec<_>>(), vec![3]);
    assert_eq!(f.total_pages, 57);
    let ids: Vec<_> = f.threads.iter().map(|t| (t.id, t.title.as_str(), t.author.as_deref())).collect();
    assert_eq!(ids, vec![(4242, "Simple Lighting Plugin", Some("LightSmith")), (4300, "Another Plugin", Some("Someone"))]);
}

#[test]
fn non_forum_pages_are_skipped() {
    let html = "<html><body><h1>Hello</h1></body></html>";
    assert!(parse_page(html, "https://example.com/", Software::Auto).unwrap().is_none());
}

#[test]
fn store_newest_capture_wins() {
    let mut store = Store::open_in_memory().unwrap();
    let html = fixture("xf2_thread.html");
    let page = parse_page(&html, "x", Software::Auto).unwrap().unwrap();
    store.save(&page, &cap("u", "20210601000000")).unwrap();

    // An older capture with different text must not overwrite the newer one.
    let older = html.replace("Thanks, works great!", "OLD TEXT");
    let page_old = parse_page(&older, "x", Software::Auto).unwrap().unwrap();
    store.save(&page_old, &cap("u", "20210401000000")).unwrap();
    let body: String = store
        .conn()
        .query_row("SELECT body_text FROM posts WHERE id = 900002", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "Thanks, works great!");

    // A newer capture (an edit) does overwrite.
    let edited = html.replace("Thanks, works great!", "EDITED");
    let page_new = parse_page(&edited, "x", Software::Auto).unwrap().unwrap();
    store.save(&page_new, &cap("u", "20220101000000")).unwrap();
    let body: String = store
        .conn()
        .query_row("SELECT body_text FROM posts WHERE id = 900002", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "EDITED");

    // Forum listing adds threads without clobbering the thread page's data.
    let listing = parse_page(&fixture("xf2_forum.html"), "x", Software::Auto).unwrap().unwrap();
    store.save(&listing, &cap("l", "20230101000000")).unwrap();
    let title: String =
        store.conn().query_row("SELECT title FROM threads WHERE id = 4242", [], |r| r.get(0)).unwrap();
    assert_eq!(title, "MZ Simple Lighting Plugin");

    let s = store.stats().unwrap();
    assert_eq!((s.threads, s.posts, s.users, s.attachments), (2, 2, 2, 1));
    assert_eq!(s.forums, 2);
}
