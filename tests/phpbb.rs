//! Parser tests against hand-made phpBB 2 and phpBB 3 pages (made-up content,
//! real markup structure).

use forumark::model::Page;
use forumark::parse::{Software, parse_page};
use forumark::urls::decode_html;

fn fixture(name: &str) -> String {
    let bytes = std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    decode_html(&bytes, None)
}

#[test]
fn phpbb3_topic_page() {
    let url = "https://board.example.org/forum/viewtopic.php?p=501";
    let page = parse_page(&fixture("phpbb3_topic.html"), url, Software::Auto).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.thread_id, 42);
    assert_eq!(t.title, "Lantern puzzle help");
    assert_eq!(t.path, "/forum/viewtopic.php?t=42");
    assert_eq!((t.page, t.total_pages), (2, 2));

    let crumbs: Vec<_> = t.forums.iter().map(|f| (f.id, f.title.as_str(), f.path.as_str())).collect();
    assert_eq!(
        crumbs,
        vec![(3, "Game Making", "/forum/viewforum.php?f=3"), (7, "Puzzle Design", "/forum/viewforum.php?f=7")]
    );

    assert_eq!(t.posts.len(), 2);
    let p = &t.posts[0];
    assert_eq!(p.id, 501);
    assert_eq!((p.author.id, p.author.name.as_str()), (Some(12), "Wren"));
    assert_eq!(p.posted_at.as_deref(), Some("2016-03-04T18:30:00+00:00"));
    assert!(p.body_html.contains("<cite>Pip wrote:</cite>"));
    assert!(p.body_html.contains("codebox"));
    assert!(!p.body_html.contains("Wren's workshop"), "signature is not part of the post");
    assert!(p.body_text.starts_with("Pip wrote: How do I light all four?"));
    assert_eq!(p.attachments.len(), 1);
    assert_eq!(p.attachments[0].path, "/download/file.php?id=88");
    assert_eq!(p.attachments[0].filename, "map.png");

    let guest = &t.posts[1];
    assert_eq!((guest.author.id, guest.author.name.as_str()), (None, "Visitor"));
    assert_eq!(guest.posted_at.as_deref(), Some("2016-03-05T09:05:00"));
}

#[test]
fn phpbb3_forum_page() {
    let url = "https://board.example.org/forum/viewforum.php?f=7";
    let page = parse_page(&fixture("phpbb3_forum.html"), url, Software::Auto).unwrap().unwrap();
    let Page::Forum(f) = page else { panic!("expected forum") };
    assert_eq!((f.forum.id, f.forum.title.as_str()), (7, "Puzzle Design"));
    assert_eq!(f.parents.iter().map(|p| p.id).collect::<Vec<_>>(), vec![3]);
    assert_eq!((f.page, f.total_pages), (1, 2));
    let topics: Vec<_> = f.threads.iter().map(|t| (t.id, t.title.as_str(), t.author.as_deref())).collect();
    assert_eq!(topics, vec![(40, "Switch guide", Some("Wren")), (42, "Lantern puzzle help", Some("Pip"))]);
    assert_eq!(f.threads[1].path, "/forum/viewtopic.php?t=42");
}

#[test]
fn phpbb2_topic_page_in_latin1() {
    let url = "http://old.example.net:80/phpBB2/viewtopic.php?t=9";
    let page = parse_page(&fixture("phpbb2_topic_latin1.html"), url, Software::Auto).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.thread_id, 9);
    assert_eq!(t.title, "Café tileset");
    assert_eq!(t.path, "/phpBB2/viewtopic.php?t=9");
    assert_eq!((t.page, t.total_pages), (1, 2));
    assert_eq!(t.forums.iter().map(|f| (f.id, f.title.as_str())).collect::<Vec<_>>(), vec![(2, "Resources")]);

    assert_eq!(t.posts.len(), 2);
    let p = &t.posts[0];
    assert_eq!(p.id, 301);
    assert_eq!((p.author.id, p.author.name.as_str()), (Some(5), "Mossy"));
    assert_eq!(p.posted_at.as_deref(), Some("2007-01-01T13:23:00"));
    assert!(p.body_html.contains("café tileset"));
    assert!(p.body_html.contains("class=\"code\""), "code boxes outside the body span are kept");
    assert!(!p.body_html.contains("Mossy makes maps"), "signature is cut off");

    let anon = &t.posts[1];
    assert_eq!((anon.author.id, anon.author.name.as_str()), (None, "Anonymous"));
    assert_eq!(anon.body_text, "Lovely, thanks!");
}

#[test]
fn xenforo_pages_are_not_mistaken_for_phpbb() {
    let html = std::fs::read_to_string(format!("{}/tests/fixtures/xf2_thread.html", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let page = parse_page(&html, "file.html", Software::Auto).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.thread_id, 4242);
}

#[test]
fn phpbb3_table_theme_with_split_rows() {
    // Custom subsilver2-style themes (HBGames' "aqua"/"hb"): the post anchor, author and date
    // sit in one table row, the message in the next one.
    let url = "http://board.example.org/forums/viewtopic.php?f=9&t=77";
    let page = parse_page(&fixture("phpbb3_split_rows_topic.html"), url, Software::Phpbb).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.thread_id, 77);
    assert_eq!((t.page, t.total_pages), (1, 1));
    let crumbs: Vec<_> = t.forums.iter().map(|f| (f.id, f.title.as_str())).collect();
    assert_eq!(crumbs, vec![(4, "Art"), (9, "Tilesets")]);
    assert_eq!(t.posts.len(), 2);

    let p = &t.posts[0];
    assert_eq!(p.id, 9001);
    assert_eq!((p.author.id, p.author.name.as_str()), (Some(31), "Moss"));
    assert_eq!(p.posted_at.as_deref(), Some("2010-01-28T01:38:00"));
    assert!(p.body_html.contains("<strong>then</strong>"));
    assert!(p.body_html.contains("quotecontent"));
    assert!(!p.body_html.contains("tile shop"), "signature is not part of the post");
    assert_eq!(p.attachments.len(), 1);
    assert_eq!(p.attachments[0].path, "/download/file.php?id=55");

    let g = &t.posts[1];
    assert_eq!(g.id, 9002);
    assert_eq!((g.author.id, g.author.name.as_str()), (None, "Visitor"));
    assert_eq!(g.posted_at.as_deref(), Some("2010-01-29T21:05:00"));
    assert_eq!(g.body_text, "Thanks, that helped.");
}

#[test]
fn phpbb3_subsilver2_date_after_subject_and_profile_button_below() {
    // Stock phpBB 3.0 subsilver2 (Box2D forum, 2007-2008): "Post subject: … Posted: <date>"
    // in the header row; the profile link is a button in the row after the message.
    let url = "http://board.example.org/forum/viewtopic.php?f=3&t=15";
    let page = parse_page(&fixture("phpbb3_subsilver2_topic.html"), url, Software::Phpbb).unwrap().unwrap();
    let Page::Thread(t) = page else { panic!("expected thread") };
    assert_eq!(t.posts.len(), 1);
    let p = &t.posts[0];
    assert_eq!(p.id, 59);
    assert_eq!((p.author.id, p.author.name.as_str()), (Some(76), "Kestrel"));
    assert_eq!(p.posted_at.as_deref(), Some("2007-09-19T13:30:00"));
    assert_eq!(p.body_text, "How do I limit a hinge?");
}
