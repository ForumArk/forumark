//! End-to-end: sample pages → database → static site.

use forumark::model::Capture;
use forumark::parse::{Software, parse_page};
use forumark::site::{BuildOptions, build};
use forumark::store::Store;

#[test]
fn builds_site_with_local_links_and_missing_pages() {
    let mut store = Store::open_in_memory().unwrap();
    for name in ["xf2_thread.html", "xf1_thread_wayback.html", "xf2_forum.html"] {
        let html = std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let page = parse_page(&html, name, Software::Auto).unwrap().unwrap();
        let cap = Capture { url: name.into(), captured_at: "20210101000000".into(), source: "test".into() };
        store.save(&page, &cap).unwrap();
    }
    let out = tempfile::tempdir().unwrap();
    let opts = BuildOptions {
        out_dir: out.path().to_path_buf(),
        base: "/rm/".into(),
        title: "Test Archive".into(),
        origin: "https://forums.example.com".into(),
        contact: "contact@example.org".into(),
        removal_url: Some("/legal/removal/".into()),
        privacy_url: None,
        about_url: None,
        notice: "notice".into(),
        threads_per_page: 50,
        skip_search: true,
    };
    let r = build(&store, &opts).unwrap();
    assert_eq!(r.thread_pages, 7); // 3 pages + 4 pages
    assert_eq!(r.missing_thread_pages, 5);

    let page2 = std::fs::read_to_string(out.path().join("threads/4242/page-2/index.html")).unwrap();
    assert!(page2.contains("Version 1.2 is out!"));
    assert!(page2.contains(r#"href="/rm/members/777/""#));
    assert!(page2.contains(r#"href="/rm/threads/4242/page-3/""#));
    assert!(!page2.contains("My signature"));

    let missing = std::fs::read_to_string(out.path().join("threads/4242/index.html")).unwrap();
    assert!(missing.contains("was not preserved"));
    assert!(missing.contains("https://web.archive.org/web/2/https://forums.example.com/threads/mz-simple-lighting-plugin.4242/"));
    assert!(!missing.contains("data-pagefind-body"));

    let forum = std::fs::read_to_string(out.path().join("forums/12/index.html")).unwrap();
    assert!(forum.contains(r#"href="/rm/threads/4242/""#));
    // Thread 4300 has no archived posts: it links to the Wayback Machine instead.
    assert!(forum.contains("https://web.archive.org/web/2/https://forums.example.com/threads/another-plugin.4300/"));

    let index = std::fs::read_to_string(out.path().join("index.html")).unwrap();
    assert!(index.contains("RPG Maker MZ") && index.contains(r#"href="/rm/forums/12/""#));
    assert!(out.path().join("assets/style.css").exists());
}
