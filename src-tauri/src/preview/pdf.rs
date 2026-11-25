//! A PDF's page count and outline (its bookmarks), for the bar above the PDF preview; finding
//! text in it, for the preview's Search tab; and each page's text, to select. All read by PDFium
//! (`pdfium.rs`): without it there's none of these, and the preview shows the web view's viewer.

use crate::books::epub::{find_all, snippet, Book, Found, Hit, Query};
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub struct Heading {
    pub title: String,
    /// 1 for the top level.
    pub level: usize,
    /// From 1.
    pub page: usize,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Outline {
    pub pages: usize,
    /// Empty when the PDF has none.
    pub headings: Vec<Heading>,
}

use pdfium_render::prelude::PdfDocument;

/// An outline can be built to loop, or be huge: past these it's cut off.
const HEADINGS_MAX: usize = 20_000;
const LEVELS_MAX: usize = 64;

pub fn outline(doc: &PdfDocument) -> Result<Outline, String> {
    use pdfium_render::prelude::PdfBookmark;
    fn walk(first: Option<PdfBookmark>, level: usize, out: &mut Vec<Heading>, seen: &mut usize) {
        let mut at = first;
        while let Some(b) = at {
            *seen += 1;
            if *seen > HEADINGS_MAX {
                return;
            }
            // A heading's page is in its destination, or in its action's (a "go to" action).
            let page = b
                .destination()
                .and_then(|d| d.page_index().ok())
                .or_else(|| b.action().and_then(|a| a.as_local_destination_action().and_then(|l| l.destination().ok()).and_then(|d| d.page_index().ok())));
            // One that goes nowhere in the PDF (a link to a web page, say) is left out; what's under
            // it isn't.
            if let Some(page) = page {
                out.push(Heading { title: b.title().unwrap_or_default().trim().to_string(), level, page: page as usize + 1 });
            }
            if level < LEVELS_MAX {
                walk(b.first_child(), level + 1, out, seen);
            }
            at = b.next_sibling();
        }
    }
    let mut headings = vec![];
    walk(doc.bookmarks().root(), 1, &mut headings, &mut 0);
    Ok(Outline { pages: doc.pages().len() as usize, headings })
}

/// A page's matches listed with their text around them; past this, only counted.
const SNIPPETS_MAX: usize = 200;

fn matches(text: &str, q: &Query, re: &Option<regex::Regex>) -> Vec<(usize, usize)> {
    match re {
        Some(re) => re.find_iter(text).filter(|m| !m.is_empty()).map(|m| (m.start(), m.end())).collect(),
        None => find_all(text, &q.find, q.match_case, q.whole_word),
    }
}

fn add_hit(found: &mut Found, n: usize, text: &str, ms: &[(usize, usize)]) {
    if !ms.is_empty() {
        found.total += ms.len();
        let snippets = ms.iter().take(SNIPPETS_MAX).map(|&m| snippet(text, m)).collect();
        found.hits.push(Hit { path: n.to_string(), label: format!("Page {n}"), count: ms.len(), snippets });
    }
}

/// The matches of `q` in the PDF's text, a page at a time (each hit's `path` is its page number,
/// from 1). Pages whose text can't be read are listed in `skipped`. A scanned PDF has no text, so
/// nothing is found in it. `marks` finds the matches in the same text, so those listed are those
/// marked.
pub fn find(doc: &PdfDocument, q: &Query) -> Result<Found, String> {
    let mut found = Found::default();
    if q.find.trim().is_empty() {
        return Ok(found);
    }
    let re = Book::regex(q)?;
    super::pdfium::each_text(doc, |i, text| match text {
        Ok(t) => add_hit(&mut found, i + 1, &t.text, &matches(&t.text, q, &re)),
        Err(_) => found.skipped.push(format!("Page {}", i + 1)),
    });
    Ok(found)
}

/// Where the matches of `q` are on page `n` (from 1), in the order `find` lists them: for each,
/// a box a line, as fractions of the page shown (left, top, width, height).
pub fn marks(doc: &PdfDocument, n: usize, q: &Query) -> Result<Vec<Vec<[f32; 4]>>, String> {
    if q.find.trim().is_empty() {
        return Ok(vec![]);
    }
    let re = Book::regex(q)?;
    let t = super::pdfium::text_of(doc, n.checked_sub(1).ok_or("No such page.")?)?;
    Ok(matches(&t.text, q, &re).into_iter().map(|m| t.boxes(m)).collect())
}

/// The text of page `n` (from 1), in runs along its lines (`PageText::runs`), and its links.
pub fn layer(doc: &PdfDocument, n: usize) -> Result<super::pdfium::Layer, String> {
    super::pdfium::layer_of(doc, n.checked_sub(1).ok_or("No such page.")?)
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Bookmark, Document, Object, ObjectId};

    /// These need PDFium (scripts/fetch-pdfium.sh); without it there's nothing to test.
    fn no_pdfium() -> bool {
        crate::preview::pdfium::pdfium().is_none()
    }

    fn opened(file: &[u8]) -> pdfium_render::prelude::PdfDocument<'_> {
        crate::preview::pdfium::open(crate::preview::pdfium::pdfium().unwrap(), file).unwrap()
    }

    #[test]
    fn pages_and_headings() {
        if no_pdfium() {
            return;
        }
        let mut doc = Document::with_version("1.7");
        let pages: ObjectId = (1, 0);
        let kids: Vec<ObjectId> = vec![(2, 0), (3, 0), (4, 0)];
        for &k in &kids {
            doc.objects.insert(k, Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => pages }));
        }
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids.iter().map(|&k| k.into()).collect::<Vec<Object>>(), "Count" => 3 }),
        );
        doc.objects.insert((5, 0), Object::Dictionary(dictionary! { "Type" => "Catalog", "Pages" => pages }));
        doc.trailer.set("Root", (5, 0));
        doc.max_id = 5;
        let one = doc.add_bookmark(Bookmark::new("One".into(), [0.0; 3], 0, kids[0]), None);
        doc.add_bookmark(Bookmark::new("One, part two".into(), [0.0; 3], 0, kids[1]), Some(one));
        doc.add_bookmark(Bookmark::new("Two".into(), [0.0; 3], 0, kids[2]), None);
        let at = doc.build_outline().unwrap();
        doc.get_object_mut((5, 0)).unwrap().as_dict_mut().unwrap().set("Outlines", at);
        let mut file = Vec::new();
        doc.save_to(&mut file).unwrap();

        let o = super::outline(&opened(&file)).unwrap();
        assert_eq!(o.pages, 3);
        let got: Vec<_> = o.headings.iter().map(|h| (h.title.as_str(), h.level, h.page)).collect();
        assert_eq!(got, [("One", 1, 1), ("One, part two", 2, 2), ("Two", 1, 3)]);

        assert!(crate::preview::pdfium::open(crate::preview::pdfium::pdfium().unwrap(), b"not a pdf").is_err());
    }

    /// A PDF with one page per text, in Courier.
    fn with_text(texts: &[&str]) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        let mut doc = Document::with_version("1.5");
        let pages: ObjectId = doc.new_object_id();
        let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier", "Encoding" => "WinAnsiEncoding" });
        let resources = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font } });
        let mut kids: Vec<Object> = vec![];
        for t in texts {
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                    Operation::new("Td", vec![72.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(*t)]),
                    Operation::new("ET", vec![]),
                ],
            };
            let stream = doc.add_object(lopdf::Stream::new(dictionary! {}, content.encode().unwrap()));
            kids.push(
                doc.add_object(
                    dictionary! { "Type" => "Page", "Parent" => pages, "Contents" => stream, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] },
                )
                .into(),
            );
        }
        let count = kids.len() as i64;
        doc.objects.insert(pages, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count, "Resources" => resources }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut file = Vec::new();
        doc.save_to(&mut file).unwrap();
        file
    }

    fn query(find: &str) -> super::Query {
        serde_json::from_value(serde_json::json!({ "find": find })).unwrap()
    }

    #[test]
    fn finds_text_by_page() {
        if no_pdfium() {
            return;
        }
        let file = with_text(&["The cat sat.", "No match here.", "A cat and a Cat."]);
        let file = opened(&file);
        let f = super::find(&file, &query("cat")).unwrap();
        assert_eq!(f.total, 3);
        let got: Vec<_> = f.hits.iter().map(|h| (h.path.as_str(), h.label.as_str(), h.count)).collect();
        assert_eq!(got, [("1", "Page 1", 1), ("3", "Page 3", 2)]);
        assert_eq!(f.hits[0].snippets[0].text, "cat");

        let mut q = query("Cat");
        q.match_case = true;
        assert_eq!(super::find(&file, &q).unwrap().total, 1);
        let mut q = query("c.t");
        q.regex = true;
        assert_eq!(super::find(&file, &q).unwrap().total, 3);
        assert_eq!(super::find(&file, &query("  ")).unwrap().total, 0);
    }

    #[test]
    fn pages_drawn_and_matches_marked() {
        if no_pdfium() {
            return;
        }
        let bytes = with_text(&["The cat sat.", "A cat and a Cat."]);
        let file = opened(&bytes);
        assert_eq!(crate::preview::pdfium::sizes(&file), [[612.0, 792.0], [612.0, 792.0]]);

        let drawn = crate::preview::pdfium::render(&file, 0, 300, None).unwrap();
        let (w, h) = (u32::from_le_bytes(drawn[..4].try_into().unwrap()), u32::from_le_bytes(drawn[4..8].try_into().unwrap()));
        assert_eq!((w, h), (300, 388));
        assert_eq!(drawn.len(), 8 + 300 * 388 * 4);
        // White and opaque in the corner, with the text drawn somewhere.
        assert_eq!(drawn[8..12], [255, 255, 255, 255]);
        assert!(drawn[8..].chunks_exact(4).all(|p| p[3] == 255));
        assert!(drawn[8..].chunks_exact(4).any(|p| p[0] < 200));
        assert!(crate::preview::pdfium::render(&file, 5, 300, None).is_err());

        // A piece of the page drawn 3000 wide: the text's corner, 72 points in and 92 down, is
        // as dark as where it is in the whole page drawn that wide.
        let whole = crate::preview::pdfium::render(&file, 0, 3000, None).unwrap();
        let piece = crate::preview::pdfium::render(&file, 0, 3000, Some([300, 300, 400, 200])).unwrap();
        assert_eq!(piece[..8], [144, 1, 0, 0, 200, 0, 0, 0]);
        assert_eq!(piece.len(), 8 + 400 * 200 * 4);
        let at = |px: &[u8], w: usize, x: usize, y: usize| px[8 + (y * w + x) * 4];
        let mut same = 0;
        for y in 0..200 {
            for x in 0..400 {
                same += usize::from(at(&piece, 400, x, y) == at(&whole, 3000, 300 + x, 300 + y));
            }
        }
        assert!(same > 400 * 200 * 99 / 100, "{same}");
        assert!(piece[8..].chunks_exact(4).any(|p| p[0] < 200));

        let marks = super::marks(&file, 2, &query("cat")).unwrap();
        assert_eq!(marks.len(), 2);
        // One box each, on the line drawn 72 points in and 92 down (of 612 × 792).
        for m in &marks {
            assert_eq!(m.len(), 1);
            let [x, y, w, h] = m[0];
            assert!(x > 72.0 / 612.0 - 0.01 && y > 80.0 / 792.0 && y < 100.0 / 792.0 && w > 0.0 && h > 0.0, "{m:?}");
        }
        assert!(marks[0][0][0] < marks[1][0][0]);
        assert!(super::marks(&file, 9, &query("cat")).is_err());
    }

    #[test]
    fn text_in_runs() {
        if no_pdfium() {
            return;
        }
        let file = with_text(&["The cat sat.", "A cat and a Cat."]);
        let file = opened(&file);
        let runs = super::layer(&file, 2).unwrap().runs;
        let t: String = runs.iter().map(|r| format!("{}{}", r.t, r.e)).collect();
        assert_eq!(t.trim_end(), "A cat and a Cat.");
        for r in &runs {
            let [x, y, w, h] = r.b;
            assert!(x > 0.1 && y > 0.1 && y < 0.13 && w > 0.0 && h > 0.0, "{r:?}");
        }
        assert!(super::layer(&file, 0).is_err() && super::layer(&file, 3).is_err());
    }

    #[test]
    fn links_to_pages_and_the_web() {
        if no_pdfium() {
            return;
        }
        let mut doc = Document::with_version("1.5");
        let pages: ObjectId = doc.new_object_id();
        let second: ObjectId = doc.new_object_id();
        let box_of = |x: i64| -> Object { vec![x.into(), 600.into(), (x + 100).into(), 620.into()].into() };
        let web = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => box_of(72),
        "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.com/a") } });
        let to_page = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => box_of(200),
        "Dest" => vec![second.into(), "Fit".into()] });
        let file = doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => box_of(330),
        "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal("file:///etc/passwd") } });
        let media: Object = vec![0.into(), 0.into(), 612.into(), 792.into()].into();
        let first = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages, "MediaBox" => media.clone(),
        "Annots" => vec![web.into(), to_page.into(), file.into()] });
        doc.objects.insert(second, Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => pages, "MediaBox" => media, "Rotate" => 90 }));
        doc.objects.insert(pages, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![first.into(), second.into()], "Count" => 2 }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        let bytes = opened(&bytes);
        // The second page is turned on its side.
        assert_eq!(crate::preview::pdfium::sizes(&bytes), [[612.0, 792.0], [792.0, 612.0]]);
        let links = super::layer(&bytes, 1).unwrap().links;
        let to: Vec<_> = links.iter().map(|l| (l.page, l.uri.as_deref())).collect();
        assert_eq!(to, [(None, Some("https://example.com/a")), (Some(2), None)]);
        let [x, y, w, h] = links[0].b;
        assert!((x - 72.0 / 612.0).abs() < 0.01 && (y - 172.0 / 792.0).abs() < 0.01, "{:?}", links[0]);
        assert!((w - 100.0 / 612.0).abs() < 0.01 && (h - 20.0 / 792.0).abs() < 0.01, "{:?}", links[0]);
        assert!(super::layer(&bytes, 2).unwrap().links.is_empty());
        assert!(crate::preview::pdfium::openable("MAILTO:a@b.c") && !crate::preview::pdfium::openable("javascript:x"));
    }
}
