//! A PDF's page count and outline (its bookmarks), for the bar above the PDF preview.

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

pub fn outline(data: &[u8]) -> Result<Outline, String> {
    let doc = lopdf::Document::load_mem(data).map_err(|e| format!("Couldn't read the PDF: {e}"))?;
    let pages = doc.get_pages().len();
    // No outline, or one this can't follow, is no headings rather than an error.
    let headings = doc
        .get_toc()
        .map(|t| t.toc.into_iter().map(|h| Heading { title: h.title.trim().to_string(), level: h.level, page: h.page }).collect())
        .unwrap_or_default();
    Ok(Outline { pages, headings })
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Bookmark, Document, Object, ObjectId};

    #[test]
    fn pages_and_headings() {
        let mut doc = Document::with_version("1.7");
        let pages: ObjectId = (1, 0);
        let kids: Vec<ObjectId> = vec![(2, 0), (3, 0), (4, 0)];
        for &k in &kids {
            doc.objects.insert(k, Object::Dictionary(dictionary! { "Type" => "Page", "Parent" => pages }));
        }
        doc.objects.insert(pages, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids.iter().map(|&k| k.into()).collect::<Vec<Object>>(), "Count" => 3 }));
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

        let o = super::outline(&file).unwrap();
        assert_eq!(o.pages, 3);
        let got: Vec<_> = o.headings.iter().map(|h| (h.title.as_str(), h.level, h.page)).collect();
        assert_eq!(got, [("One", 1, 1), ("One, part two", 2, 2), ("Two", 1, 3)]);

        assert!(super::outline(b"not a pdf").is_err());
    }
}
