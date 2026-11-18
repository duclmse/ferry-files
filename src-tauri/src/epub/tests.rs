use super::*;

const OPF: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:identifier id="uid">urn:uuid:1234</dc:identifier>
<dc:title>Old &amp; Tired</dc:title>
<dc:creator id="c1">Ann</dc:creator>
<meta refines="#c1" property="role">aut</meta>
<dc:creator id="c2">Bob</dc:creator>
<meta refines="#c2" property="role">ill</meta>
<dc:language>en</dc:language>
<meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
<item id="c1" href="text/ch%201.xhtml" media-type="application/xhtml+xml"/>
<item id="c2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
<item id="img" href="images/pic.png" media-type="image/png"/>
  </manifest>
  <spine>
<itemref idref="c1"/>
<itemref idref="c2"/>
  </spine>
</package>
"##;

const NAV: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Nav</title></head>
<body><nav epub:type="toc"><ol><li><a href="text/ch%201.xhtml">Chapter <b>One</b></a></li><li><a href="text/ch2.xhtml#s">Two</a></li></ol></nav></body></html>"#;

const CH1: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title/><link rel="stylesheet" href="../style.css"/></head>
<body class="c"><p>The cat &amp; the hat.<a id="p1"/></p><img src="../images/pic.png" alt=""/><style>p{color:cat}</style></body></html>"#;

const CH2: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>2</title></head><body><p>Concatenate the cat.</p></body></html>"#;

fn sample() -> Book {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    for (n, d) in [
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", r#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#),
        ("OEBPS/content.opf", OPF),
        ("OEBPS/nav.xhtml", NAV),
        ("OEBPS/text/ch 1.xhtml", CH1),
        ("OEBPS/text/ch2.xhtml", CH2),
        ("OEBPS/images/pic.png", "PNG"),
    ] {
        w.start_file(n, o).unwrap();
        w.write_all(d.as_bytes()).unwrap();
    }
    let bytes = w.finish().unwrap().into_inner();
    Book::open(&bytes, "/sdcard/b.epub".into(), "v1".into()).unwrap()
}

#[test]
fn checks_change_and_shrink_books() {
    let mut b = sample();
    let issues = b.check();
    let texts: Vec<&str> = issues.iter().map(|i| i.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("style.css") && t.contains("isn't in the book")), "{texts:?}");
    assert!(texts.contains(&"No cover image is marked."));
    assert!(!texts.iter().any(|t| t.contains("no title")));
    // A large photo, to be scaled down.
    let img = image::RgbImage::from_fn(2400, 1200, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
    let mut jpg = vec![];
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 98).encode_image(&img).unwrap();
    b.put_file("OEBPS/images/photo.jpg", jpg);
    let bytes = b.to_bytes().unwrap();
    assert!(crate::books::check(&bytes, "b.epub").iter().all(|i| !i.text.contains("mimetype")));
    let (small, s) = crate::books::shrink(&bytes, "b.epub", 1200, 75).unwrap();
    assert_eq!(s.images, 1);
    assert!(s.after < s.before / 2, "{s:?}");
    let sb = Book::open(&small, "x".into(), "".into()).unwrap();
    let photo = image::load_from_memory(sb.file("OEBPS/images/photo.jpg").unwrap()).unwrap();
    assert_eq!((photo.width(), photo.height()), (1200, 600));
    assert_eq!(sb.file("OEBPS/images/pic.png").unwrap(), b"PNG", "not an image: left alone");
    // Changing info in bulk: only the given fields.
    let c = crate::books::Change { language: Some("fr".into()), add_subjects: vec!["Poetry".into()], ..Default::default() };
    let changed = crate::books::change(&small, "x", &c).unwrap().unwrap();
    let m = Book::open(&changed, "x".into(), "".into()).unwrap().meta().unwrap();
    assert_eq!((m.language.as_str(), m.subjects.as_slice(), m.creators.len()), ("fr", &["Poetry".to_string()][..], 2));
    assert!(crate::books::change(&changed, "x", &c).unwrap().is_none(), "nothing left to change");
    assert_eq!(crate::books::smaller_name("A.b.epub"), "A.b (smaller).epub");
}

#[test]
fn reads_book() {
    let b = sample();
    let m = b.meta().unwrap();
    assert_eq!(m.title, "Old & Tired");
    assert_eq!(m.creators, ["Ann", "Bob"]);
    assert_eq!(m.identifier, "urn:uuid:1234");
    let ch = b.chapters().unwrap();
    assert_eq!(ch.len(), 2);
    assert_eq!((ch[0].path.as_str(), ch[0].label.as_str()), ("OEBPS/text/ch 1.xhtml", "Chapter One"));
    assert_eq!(ch[1].label, "Two");
    assert_eq!(b.stats().unwrap().images, 1);
    assert_eq!(b.cover_path(), None);
    assert_eq!(resolve("OEBPS/text/a.xhtml", "../images/x.png#f").as_deref(), Some("OEBPS/images/x.png"));
    assert_eq!(resolve("a.xhtml", "https://x.org/"), None);
    assert_eq!(relative("OEBPS/content.opf", "OEBPS/images/c.png"), "images/c.png");
    assert_eq!(relative("OEBPS/text/a.xhtml", "OEBPS/images/c.png"), "../images/c.png");
}

#[test]
fn edits_meta() {
    let mut b = sample();
    let mut m = b.meta().unwrap();
    m.title = "New <Title>".into();
    m.creators = vec!["Ann".into()];
    m.subjects = vec!["Fiction".into(), "Cats".into()];
    m.publisher = "Pub".into();
    b.set_meta(&m).unwrap();
    assert!(b.dirty);
    let opf = b.text("OEBPS/content.opf").unwrap();
    assert!(opf.contains("<dc:title>New &lt;Title&gt;</dc:title>"), "{opf}");
    assert!(!opf.contains("Bob") && !opf.contains("#c2"), "creator and its refines removed: {opf}");
    assert!(opf.contains(r##"<meta refines="#c1" property="role">aut</meta>"##));
    assert!(opf.contains("<dc:subject>Fiction</dc:subject>\n    <dc:subject>Cats</dc:subject>"), "{opf}");
    assert_eq!(b.meta().unwrap(), Meta { creators: vec!["Ann".into()], ..m.clone() });
    // Untouched lines keep their exact text.
    assert!(opf.contains(r#"<item id="c1" href="text/ch%201.xhtml" media-type="application/xhtml+xml"/>"#));
    assert!(b.set_meta(&Meta { title: " ".into(), ..m }).is_err());
    b.touch_modified();
    assert!(!b.text("OEBPS/content.opf").unwrap().contains("2020-01-01"));
}

#[test]
fn finds_and_replaces_text_only() {
    let mut b = sample();
    let q = Query { find: "cat".into(), replace: "dog".into(), match_case: false, whole_word: true };
    let f = b.find(&q).unwrap();
    assert_eq!(f.total, 2, "{f:?}"); // not "Concatenate", not inside <style>
    assert_eq!(f.hits[0].snippets[0].before, "The ");
    assert_eq!(b.replace_all(&q).unwrap(), 2);
    let ch1 = b.text("OEBPS/text/ch 1.xhtml").unwrap();
    assert!(ch1.contains("<p>The dog &amp; the hat.<a id=\"p1\"/></p>"), "{ch1}");
    assert!(ch1.contains("p{color:cat}"));
    assert!(b.text("OEBPS/text/ch2.xhtml").unwrap().contains("Concatenate the dog."));
    let q = Query { find: "THE".into(), replace: String::new(), match_case: true, whole_word: false };
    assert_eq!(b.find(&q).unwrap().total, 0);
}

#[test]
fn visual_and_body() {
    let mut b = sample();
    let path = "OEBPS/text/ch 1.xhtml";
    let html = b.visual_html(path, "ferry://localhost/book/1/OEBPS/text/").unwrap();
    assert!(html.starts_with("\n<!DOCTYPE html>"), "{html}");
    assert!(html.contains(r#"<head><base href="ferry://localhost/book/1/OEBPS/text/"><title></title><link"#), "{html}");
    assert!(html.contains(r#"<a id="p1"></a>"#) && html.contains(r#"alt=""/>"#));
    assert!(b.body_inner(path).unwrap().starts_with("<p>The cat"));
    b.set_body(path, "<p>New</p>").unwrap();
    assert!(b.text(path).unwrap().contains(r#"<body class="c"><p>New</p></body>"#));
    assert!(b.set_body(path, "<p>broken").is_err());
    assert!(b.set_source(path, "<html><body>").is_err());
}

#[test]
fn covers_and_round_trip() {
    let mut b = sample();
    b.set_cover(b"JPG".to_vec(), "jpg").unwrap();
    assert_eq!(b.cover_path().as_deref(), Some("OEBPS/images/cover.jpg"));
    let opf = b.text("OEBPS/content.opf").unwrap();
    assert!(opf.contains(r#"<item id="cover-image" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>"#), "{opf}");
    assert!(opf.contains(r#"<meta name="cover" content="cover-image"/>"#));
    b.set_cover(b"PNG2".to_vec(), "png").unwrap();
    assert_eq!(b.media_type("OEBPS/images/cover.jpg"), "image/png");
    assert_eq!(b.file("OEBPS/images/cover.jpg"), Some(&b"PNG2"[..]));
    let bytes = b.to_bytes().unwrap();
    assert_eq!(&bytes[30..38], b"mimetype");
    let again = Book::open(&bytes, String::new(), String::new()).unwrap();
    assert_eq!(again.meta().unwrap(), b.meta().unwrap());
    assert_eq!(again.file("OEBPS/images/cover.jpg"), Some(&b"PNG2"[..]));
    assert_eq!(again.chapters().unwrap().len(), 2);
}
