use super::*;

const A4: [f64; 2] = [595.0, 842.0];

#[test]
fn info_rows_and_sizes() {
    assert_eq!(page_size(595.0, 842.0), "A4, 210 × 297 mm");
    assert_eq!(page_size(792.0, 612.0), "Letter, 279 × 216 mm");
    assert_eq!(page_size(500.0, 500.0), "176 × 176 mm");
    let i = PdfInfo {
        title: Some("T".into()),
        pages: 3,
        version: Some("1.7".into()),
        ..Default::default()
    };
    let keys: Vec<_> = info_rows(&i).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["Title", "Pages", "PDF version", "Encrypted"]);
}

#[test]
fn fits() {
    // A stage 624 × 524: a column of 600, 500 high inside the padding.
    let room = (600.0 + 2.0 * PDF_PAD, 500.0 + 2.0 * PDF_PAD);
    assert_eq!(fit_zoom(Fit::Free, room, A4, false), None);
    assert_eq!(fit_zoom(Fit::Width, room, A4, false), Some(1.0));
    // At zoom 1 the page is 600 × 842/595 high; 500 of it shows.
    let z = fit_zoom(Fit::Page, room, A4, false).unwrap();
    assert!((z * 600.0 * 842.0 / 595.0 - 500.0).abs() < 1e-9);
    assert!((fit_zoom(Fit::Actual, room, A4, false).unwrap() - 595.0 / 600.0).abs() < 1e-12);
}

#[test]
fn places_kept_most_recent_first() {
    let at = |key: &str, page| Place {
        key: key.into(),
        page,
        down: 0.5,
        across: 0.5,
        zoom: 1.0,
        fit: Fit::Width,
        turns: 0,
    };
    let mut all = vec![];
    for i in 0..PLACES_MAX + 5 {
        remember(&mut all, at(&i.to_string(), 0));
    }
    assert_eq!(all.len(), PLACES_MAX);
    assert_eq!(all[0].key, (PLACES_MAX + 4).to_string());
    // The oldest are gone; one read again moves to the front, once.
    assert!(all.iter().all(|p| p.key != "4"));
    remember(&mut all, at("100", 7));
    assert_eq!((all[0].key.as_str(), all[0].page), ("100", 7));
    assert_eq!(all.iter().filter(|p| p.key == "100").count(), 1);
    assert_eq!(all.len(), PLACES_MAX);
    // Changed since, a PDF is another one.
    assert_ne!(
        place_key("mac", "/a.pdf", 1, 2, None),
        place_key("mac", "/a.pdf", 1, 3, None)
    );
    assert_ne!(
        place_key("mac", "/a.zip", 1, 2, Some("x.pdf")),
        place_key("mac", "/a.zip", 1, 2, None)
    );
}

#[test]
fn turned_and_wide_pages() {
    let room = (600.0 + 2.0 * PDF_PAD, 500.0 + 2.0 * PDF_PAD);
    // On its side it's 842 wide: actual size is wider than the column.
    assert!((fit_zoom(Fit::Actual, room, A4, true).unwrap() - 842.0 / 600.0).abs() < 1e-12);
    // A page wider than tall already fits at the stage's width: no wider than that.
    assert_eq!(fit_zoom(Fit::Page, room, [1000.0, 200.0], false), Some(1.0));
    // Limited like the zoom buttons.
    assert_eq!(
        fit_zoom(Fit::Actual, (100.0, 100.0), [10000.0, 10.0], false),
        Some(ZOOM_MAX)
    );
}
