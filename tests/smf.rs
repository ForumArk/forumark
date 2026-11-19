//! Parser tests against hand-made SMF 1.x and 2.x pages (made-up content,
//! real markup structure).

use forumark::model::Page;
use forumark::parse::{Software, parse_page};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn smf1_topic_page() {
    let url = "http://old.example.net:80/index.php/topic,321.15.html";
    let page = parse_page(&fixture("smf1_topic.html"), url, Software::Auto).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.thread_id, 321);
    assert_eq!(t.title, "Battle sounds pack");
    assert_eq!(t.path, "/index.php?topic=321.0");
    assert_eq!((t.page, t.total_pages), (2, 2));
    let crumbs: Vec<_> = t.forums.iter().map(|f| (f.id, f.title.as_str())).collect();
    assert_eq!(crumbs, vec![(4, "Resources"), (9, "Audio")]);

    assert_eq!(t.posts.len(), 2);
    let p = &t.posts[0];
    assert_eq!(p.id, 701);
    assert_eq!((p.author.id, p.author.name.as_str()), (Some(42), "Fennel"), "not the board moderator");
    assert_eq!(p.posted_at.as_deref(), Some("2009-03-03T20:15:00"));
    assert!(p.body_html.contains("class=\"quote\"") && p.body_html.contains("class=\"code\""));
    assert!(!p.body_html.contains("sound shop"), "signature is not part of the post");
    assert_eq!(p.attachments.len(), 1);
    assert_eq!(p.attachments[0].filename, "clang.ogg");
    assert_eq!(p.attachments[0].path, "/index.php?action=dlattach;attach=55");

    let guest = &t.posts[1];
    assert_eq!((guest.id, guest.author.id, guest.author.name.as_str()), (702, None, "Visitor"));
    assert_eq!(guest.body_text, "Thank you!");
}

#[test]
fn smf2_topic_page_with_one_post() {
    let url = "https://board.example.org/forum/index.php?topic=88.0";
    let page = parse_page(&fixture("smf2_topic.html"), url, Software::Auto).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!((t.thread_id, t.title.as_str(), t.path.as_str()), (88, "Lighting plugin help", "/forum/index.php?topic=88.0"));
    assert_eq!((t.page, t.total_pages), (1, 2));
    assert_eq!(t.forums.iter().map(|f| f.id).collect::<Vec<_>>(), vec![7]);
    assert_eq!(t.posts.len(), 1);
    let p = &t.posts[0];
    assert_eq!(p.id, 900);
    assert_eq!((p.author.id, p.author.name.as_str()), (Some(12), "Wren"), "not the board moderator in the header");
    assert_eq!(p.posted_at.as_deref(), Some("2014-05-14T21:10:32"));
    assert!(p.body_html.contains("<cite>"));
    assert!(!p.body_html.contains("workshop"));
}

#[test]
fn smf2_board_page() {
    let url = "https://board.example.org/forum/index.php?board=7.0";
    let page = parse_page(&fixture("smf2_board.html"), url, Software::Auto).unwrap().unwrap();
    let Page::Forum(f) = page else { panic!("expected board") };
    assert_eq!((f.forum.id, f.forum.title.as_str()), (7, "Scripts"));
    assert_eq!(f.parents.iter().map(|p| p.id).collect::<Vec<_>>(), vec![3]);
    assert_eq!((f.page, f.total_pages), (1, 2));
    let topics: Vec<_> = f.threads.iter().map(|t| (t.id, t.title.as_str(), t.author.as_deref())).collect();
    assert_eq!(topics, vec![(40, "Switch guide", Some("Wren")), (88, "Lighting plugin help", Some("Pip"))]);
}

#[test]
fn login_pages_are_skipped() {
    let html = r#"<html><body><script>var smf_scripturl = "x";</script>
        <div class="navigate_section"><ul><li><a href="https://f.example/index.php">Board</a></li></ul></div>
        <p>The topic or board you are looking for appears to be either missing or off limits to you.</p>
        <span id="msg_5"><a href="https://f.example/index.php?topic=5.0">Sidebar topic</a></span></body></html>"#;
    assert!(parse_page(html, "https://f.example/index.php?board=1.0", Software::Auto).unwrap().is_none());
    assert!(parse_page(html, "https://f.example/index.php?topic=5.0", Software::Auto).unwrap().is_none());
}
