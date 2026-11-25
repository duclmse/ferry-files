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
    Book::open(&sample_bytes(), "/sdcard/b.epub".into(), "v1".into()).unwrap()
}

fn sample_bytes() -> Vec<u8> {
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
    w.finish().unwrap().into_inner()
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
    assert!(crate::books::batch::check(&bytes, "b.epub").iter().all(|i| !i.text.contains("mimetype")));
    let (small, s) = crate::books::batch::shrink(&bytes, "b.epub", 1200, 75).unwrap();
    assert_eq!(s.images, 1);
    assert!(s.after < s.before / 2, "{s:?}");
    let sb = Book::open(&small, "x".into(), "".into()).unwrap();
    let photo = image::load_from_memory(sb.file("OEBPS/images/photo.jpg").unwrap()).unwrap();
    assert_eq!((photo.width(), photo.height()), (1200, 600));
    assert_eq!(sb.file("OEBPS/images/pic.png").unwrap(), b"PNG", "not an image: left alone");
    // Changing info in bulk: only the given fields.
    let c = crate::books::batch::Change { language: Some("fr".into()), add_subjects: vec!["Poetry".into()], ..Default::default() };
    let changed = crate::books::batch::change(&small, "x", &c).unwrap().unwrap();
    let m = Book::open(&changed, "x".into(), "".into()).unwrap().meta().unwrap();
    assert_eq!((m.language.as_str(), m.subjects.as_slice(), m.creators.len()), ("fr", &["Poetry".to_string()][..], 2));
    assert!(crate::books::batch::change(&changed, "x", &c).unwrap().is_none(), "nothing left to change");
    assert_eq!(crate::books::batch::smaller_name("A.b.epub"), "A.b (smaller).epub");
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
    let q = Query { find: "cat".into(), replace: "dog".into(), match_case: false, whole_word: true, regex: false, preserve_case: false };
    let f = b.find(&q).unwrap();
    assert_eq!(f.total, 2, "{f:?}"); // not "Concatenate", not inside <style>
    assert_eq!(f.hits[0].snippets[0].before, "The ");
    assert_eq!(b.replace_all(&q).unwrap(), 2);
    let ch1 = b.text("OEBPS/text/ch 1.xhtml").unwrap();
    assert!(ch1.contains("<p>The dog &amp; the hat.<a id=\"p1\"/></p>"), "{ch1}");
    assert!(ch1.contains("p{color:cat}"));
    assert!(b.text("OEBPS/text/ch2.xhtml").unwrap().contains("Concatenate the dog."));
    let q = Query { find: "THE".into(), replace: String::new(), match_case: true, whole_word: false, regex: false, preserve_case: false };
    assert_eq!(b.find(&q).unwrap().total, 0);
    // A regular expression, its groups in the replacement.
    let q = Query { find: r"(\w+) &".into(), replace: "$1 and".into(), match_case: false, whole_word: false, regex: true, preserve_case: false };
    assert_eq!(b.find(&q).unwrap().total, 1);
    assert_eq!(b.replace_all(&q).unwrap(), 1);
    assert!(b.text("OEBPS/text/ch 1.xhtml").unwrap().contains("<p>The dog and the hat."));
    // One at a time, going on after each replacement (so "dog" → "dogs" isn't replaced again).
    let q = Query { find: "dog".into(), replace: "dogs".into(), match_case: false, whole_word: false, regex: false, preserve_case: false };
    let c2 = "OEBPS/text/ch2.xhtml";
    let at = b.replace_one(&q, c2, 0).unwrap().unwrap();
    assert!(b.text(c2).unwrap().contains("Concatenate the dogs."));
    assert_eq!(b.replace_one(&q, c2, at).unwrap(), None);
    // Preserve case: each match's case goes to its replacement.
    let c1 = "OEBPS/text/ch 1.xhtml";
    b.set_source(c1, "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>t</title></head><body><p>Cat cat CAT</p></body></html>").unwrap();
    let q = Query { find: "cat".into(), replace: "dog".into(), match_case: false, whole_word: true, regex: false, preserve_case: true };
    assert_eq!(b.replace_all(&q).unwrap(), 3);
    assert!(b.text(c1).unwrap().contains("<p>Dog dog DOG</p>"), "{}", b.text(c1).unwrap());
    let bad = Query { find: "(".into(), replace: String::new(), match_case: false, whole_word: false, regex: true, preserve_case: false };
    assert!(b.find(&bad).unwrap_err().contains("regular expression"));
}

#[test]
fn finds_across_formatting_and_keeps_entities() {
    let mut b = sample();
    let c2 = "OEBPS/text/ch2.xhtml";
    let src = "<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>2</title></head><body>\
        <p>Caf&eacute; <b>au</b>\n   <i>lait</i> &custom; &#8212; caf&eacute;</p><p>au</p><p>lait</p></body></html>";
    b.set_source(c2, src).unwrap();
    let q = |find: &str, replace: &str| Query {
        find: find.into(),
        replace: replace.into(),
        match_case: false,
        whole_word: true,
        regex: false,
        preserve_case: false,
    };
    // Across <b> and <i>, a line break as a space, an entity as its letter; not from one
    // paragraph into the next.
    let f = b.find(&q("café au lait", "")).unwrap();
    assert_eq!(f.total, 1, "{f:?}");
    assert_eq!(f.hits[0].snippets[0].after, " &custom; — café");
    assert_eq!(b.find(&q("au lait", "")).unwrap().total, 1);
    assert_eq!(b.replace_all(&q("café au lait", "tea")).unwrap(), 1);
    let t = b.text(c2).unwrap();
    // The match is replaced in its first text and taken out of the rest; the entities after it
    // keep their bytes.
    assert!(t.contains("<p>tea<b></b><i></i> &custom; &#8212; caf&eacute;</p>"), "{t}");
    // Inside an entity the book defines itself, nothing is matched.
    assert_eq!(b.find(&q("custom", "")).unwrap().total, 0);
    assert_eq!(b.replace_all(&q("café", "x < y")).unwrap(), 1);
    assert!(b.text(c2).unwrap().contains("&#8212; x &lt; y</p>"));
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
    let c2 = "OEBPS/text/ch2.xhtml";
    let page = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>2</title><style>p{background:url( "../images/cover.jpg" )}</style></head><body><img src="../images/cover.jpg" alt=""/><p style="background:url(../images/cover.jpg#x)">x</p><a href="../images/pic.png">p</a></body></html>"#;
    b.set_source(c2, page).unwrap();
    // A new type renames the file to match, and what names it.
    b.set_cover(b"PNG2".to_vec(), "png").unwrap();
    assert_eq!(b.cover_path().as_deref(), Some("OEBPS/images/cover.png"));
    assert_eq!(b.media_type("OEBPS/images/cover.png"), "image/png");
    assert_eq!(b.file("OEBPS/images/cover.png"), Some(&b"PNG2"[..]));
    assert!(b.file("OEBPS/images/cover.jpg").is_none());
    assert!(b
        .text("OEBPS/content.opf")
        .unwrap()
        .contains(r#"<item id="cover-image" href="images/cover.png" media-type="image/png" properties="cover-image"/>"#));
    assert_eq!(b.text(c2).unwrap(), page.replace("cover.jpg", "cover.png"));
    let bytes = b.to_bytes().unwrap();
    assert_eq!(&bytes[30..38], b"mimetype");
    let again = Book::open(&bytes, String::new(), String::new()).unwrap();
    assert_eq!(again.meta().unwrap(), b.meta().unwrap());
    assert_eq!(again.file("OEBPS/images/cover.png"), Some(&b"PNG2"[..]));
    assert_eq!(again.chapters().unwrap().len(), 2);
}

#[test]
fn lists_chapters_from_a_few_files() {
    let bytes = sample_bytes();
    let mut asked = vec![];
    let mut read = |at: u64, len: u64| Ok(bytes.iter().skip(at as usize).take(len as usize).copied().collect());
    let b = crate::preview::archive::unpack_each(bytes.len() as u64, &mut read, 1 << 20, |get| {
        Book::contents_only(&mut |name: &str| {
            asked.push(name.to_string());
            get(name)
        })
    })
    .unwrap()
    .unwrap();
    assert_eq!(asked, ["META-INF/container.xml", "OEBPS/content.opf", "OEBPS/nav.xhtml"]);
    // Searching while reading reads the chapters' text as well, and no pictures.
    let mut asked = vec![];
    let mut read = |at: u64, len: u64| Ok(bytes.iter().skip(at as usize).take(len as usize).copied().collect());
    let found = crate::preview::archive::unpack_each(bytes.len() as u64, &mut read, 1 << 20, |get| {
        Book::with_chapters(&mut |name: &str| {
            asked.push(name.to_string());
            get(name)
        })
    })
    .unwrap()
    .unwrap()
    .find(&Query { find: "cat".into(), replace: String::new(), match_case: false, whole_word: true, regex: false, preserve_case: false })
    .unwrap();
    assert_eq!(found.total, 2);
    assert!(!asked.iter().any(|n| n.ends_with(".png")), "{asked:?}");
    assert_eq!(b.meta().unwrap().title, "Old & Tired");
    let ch = b.chapters().unwrap();
    assert_eq!((ch[0].path.as_str(), ch[0].label.as_str()), ("OEBPS/text/ch 1.xhtml", "Chapter One"));

    let html = Book::chapter_alone("OEBPS/text/ch2.xhtml", CH2.as_bytes().to_vec(), "ferry://localhost/inzip/x/1/b.epub/OEBPS/text/").unwrap();
    assert!(html.contains(r#"<base href="ferry://localhost/inzip/x/1/b.epub/OEBPS/text/">"#), "{html}");
    assert!(html.contains("Concatenate the cat."));
}

#[test]
fn moves_parts_between_chapters() {
    let mut b = sample();
    let (c1, c2) = ("OEBPS/text/ch 1.xhtml", "OEBPS/text/ch2.xhtml");
    assert_eq!(b.block_names(c1).unwrap(), ["p", "img", "style"]);
    // The first chapter has nothing before it, the last nothing after.
    assert!(b.move_part(c1, false, Cut::Block(1), None).unwrap_err().contains("first chapter"));
    assert!(b.move_part(c2, true, Cut::Block(0), None).unwrap_err().contains("last chapter"));
    // The visual view must see the same elements.
    let wrong = vec!["p".to_string()];
    assert!(b.move_part(c1, true, Cut::Block(1), Some(&wrong)).unwrap_err().contains("Source"));

    // The picture and the style, to the start of chapter two.
    let names = b.block_names(c1).unwrap();
    let m = b.move_part(c1, true, Cut::Block(1), Some(&names)).unwrap();
    assert_eq!((m.to.as_str(), m.blocks, m.links), ("Two", 2, 0));
    assert!(b.text(c1).unwrap().contains(r#"<body class="c"><p>The cat &amp; the hat.<a id="p1"/></p></body></html>"#), "{}", b.text(c1).unwrap());
    let two = b.text(c2).unwrap();
    assert!(two.contains("<body>\n<img src=\"../images/pic.png\" alt=\"\"/><style>p{color:cat}</style><p>Concatenate the cat.</p></body>"), "{two}");
    xml::check(&two).unwrap();

    // Back again, by a place in the source: the cursor in "Concatenate" keeps that paragraph.
    let off = two.find("Concatenate").unwrap();
    let m = b.move_part(c2, false, Cut::Offset(off, false), None).unwrap();
    assert_eq!(m.blocks, 2);
    assert!(b.text(c2).unwrap().contains("<body>\n<p>Concatenate the cat.</p></body>"), "{}", b.text(c2).unwrap());
    let one = b.text(c1).unwrap();
    assert!(one.contains(r#"hat.<a id="p1"/></p>"#) && one.ends_with("<style>p{color:cat}</style></body></html>"), "{one}");
    assert!(b.dirty);
}

#[test]
fn moves_keep_links_working() {
    let mut b = sample();
    let (c1, c2, nav) = ("OEBPS/text/ch 1.xhtml", "OEBPS/text/ch2.xhtml", "OEBPS/nav.xhtml");
    let head = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body>"#;
    b.set_source(
        c1,
        &format!(r##"{head}<p id="a">A <a href="#b">to b</a></p><p id="b">B <a href="#a">to a</a> <a href="ch2.xhtml#c">to c</a></p></body></html>"##),
    )
    .unwrap();
    b.set_source(c2, &format!(r#"{head}<p id="c">C <a href="ch%201.xhtml#b">to b</a> <img src="../images/pic.png"/></p></body></html>"#)).unwrap();
    b.set_source(nav, &NAV.replace("text/ch2.xhtml#s", "text/ch%201.xhtml#b")).unwrap();

    // B goes to the start of chapter two: links to it follow it, its own links still reach A
    // (and those that already work from there keep their bytes).
    let m = b.move_part(c1, true, Cut::Block(1), None).unwrap();
    assert_eq!(m.blocks, 1);
    let (one, two) = (b.text(c1).unwrap(), b.text(c2).unwrap());
    assert!(one.contains(r#"<a href="ch2.xhtml#b">to b</a>"#), "{one}");
    assert!(two.contains(r##"<p id="b">B <a href="ch%201.xhtml#a">to a</a> <a href="ch2.xhtml#c">to c</a></p>"##), "{two}");
    assert!(two.contains(r##"C <a href="#b">to b</a> <img src="../images/pic.png"/>"##), "{two}");
    assert!(b.text(nav).unwrap().contains(r#"href="text/ch2.xhtml#b""#));
    assert_eq!(m.links, 4);
}

#[test]
fn renames_chapters_in_the_contents() {
    let mut b = sample();
    let ncx = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap><navPoint id="n1"><navLabel><text>Old one</text></navLabel><content src="text/ch%201.xhtml"/></navPoint></navMap></ncx>"#;
    b.set_file("OEBPS/toc.ncx", ncx.as_bytes().to_vec());
    let opf = b
        .text("OEBPS/content.opf")
        .unwrap()
        .replace("</manifest>", "<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n  </manifest>");
    b.set_file("OEBPS/content.opf", opf.into_bytes());
    b.rename_chapter("OEBPS/text/ch 1.xhtml", "  Start &  end ").unwrap();
    assert_eq!(b.chapters().unwrap()[0].label, "Start & end");
    let nav = b.text("OEBPS/nav.xhtml").unwrap();
    assert!(nav.contains(r#"<a href="text/ch%201.xhtml">Start &amp; end</a></li><li><a href="text/ch2.xhtml#s">Two</a>"#), "{nav}");
    assert!(b.text("OEBPS/toc.ncx").unwrap().contains("<text>Start &amp; end</text>"));
    assert!(b.rename_chapter("OEBPS/text/ch2.xhtml", " ").is_err());
}

#[test]
fn adds_and_removes_contents_entries() {
    let mut b = sample();
    let ncx = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><head><meta name="dtb:depth" content="1"/></head><navMap>
  <navPoint id="n1" playOrder="1"><navLabel><text>One</text></navLabel><content src="text/ch%201.xhtml"/></navPoint>
  <navPoint id="n2" playOrder="2"><navLabel><text>Two</text></navLabel><content src="text/ch2.xhtml"/></navPoint>
</navMap></ncx>"#;
    b.set_file("OEBPS/toc.ncx", ncx.as_bytes().to_vec());
    let opf = b
        .text("OEBPS/content.opf")
        .unwrap()
        .replace("</manifest>", "<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n  </manifest>");
    b.set_file("OEBPS/content.opf", opf.into_bytes());
    let c1 = "OEBPS/text/ch 1.xhtml";
    b.set_source(
        c1,
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>t</title></head><body><h1>One</h1><h2>A part of it</h2><p id="p">Text</p></body></html>"#,
    )
    .unwrap();
    let toc = b.toc().unwrap();
    assert_eq!(toc.iter().map(|e| (e.label.as_str(), e.depth)).collect::<Vec<_>>(), [("Chapter One", 1), ("Two", 1)]);

    // A subsection at the h2: under chapter one, given an id, its text as its name.
    b.add_toc(c1, Cut::Block(1), None, "", Level::Subsection).unwrap();
    assert!(b.text(c1).unwrap().contains(r#"<h2 id="toc-1">A part of it</h2>"#));
    let toc = b.toc().unwrap();
    assert_eq!(
        toc.iter().map(|e| (e.label.as_str(), e.depth, e.fragment.as_deref())).collect::<Vec<_>>(),
        [("Chapter One", 1, None), ("A part of it", 2, Some("toc-1")), ("Two", 1, Some("s"))]
    );
    let nav = b.text("OEBPS/nav.xhtml").unwrap();
    assert!(nav.contains(r#"<a href="text/ch%201.xhtml">Chapter <b>One</b></a>"#), "{nav}");
    assert!(nav.contains(r#"<a href="text/ch%201.xhtml#toc-1">A part of it</a>"#), "{nav}");
    xml::check(&nav).unwrap();
    let ncx = b.text("OEBPS/toc.ncx").unwrap();
    assert!(
        ncx.contains(r#"<navPoint id="navPoint-toc-1" playOrder="2"><navLabel><text>A part of it</text></navLabel><content src="text/ch%201.xhtml#toc-1"/>"#),
        "{ncx}"
    );
    assert!(ncx.contains(r#"<navPoint id="n2" playOrder="3">"#) && ncx.contains(r#"content="2""#), "{ncx}");
    // The same place twice is refused.
    assert!(b.add_toc(c1, Cut::Block(1), None, "Again", Level::Chapter).unwrap_err().contains("already"));
    // A paragraph with its own id, as a chapter beside the subsection, named as asked.
    b.add_toc(c1, Cut::Block(2), None, "Text", Level::Chapter).unwrap();
    assert_eq!(b.toc().unwrap()[2].fragment.as_deref(), Some("p"));
    assert_eq!(b.toc().unwrap()[2].depth, 2);

    // Removing the subsection: what's after it keeps its place.
    b.remove_toc(1).unwrap();
    let toc = b.toc().unwrap();
    assert_eq!(toc.iter().map(|e| (e.label.as_str(), e.depth)).collect::<Vec<_>>(), [("Chapter One", 1), ("Text", 2), ("Two", 1)]);
    assert!(!b.text("OEBPS/toc.ncx").unwrap().contains("toc-1"));
    xml::check(&b.text("OEBPS/nav.xhtml").unwrap()).unwrap();
    xml::check(&b.text("OEBPS/toc.ncx").unwrap()).unwrap();
}

#[test]
fn an_unlisted_table_of_contents_is_named() {
    assert_eq!(meta::unlisted_label("OEBPS/toc.xhtml", ""), "Table of contents");
    assert_eq!(meta::unlisted_label("OEBPS/nav.xhtml", "nav"), "Table of contents");
    assert_eq!(meta::unlisted_label("OEBPS/ch1.xhtml", ""), "ch1.xhtml");
}
