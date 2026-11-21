//! What sort of file an item is, and its icon (every view, and the preview while a picture
//! loads).

use super::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum FileType {
    Folder,
    Book,
    Document,
    Code,
    Image,
    Video,
    Music,
    Other,
}

const DOC_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "odt", "rtf", "pages", "txt", "md", "markdown", "log", "xls", "xlsx", "ods", "csv",
    "tsv", "numbers", "ppt", "pptx", "odp", "key", "mobi", "azw3", "djvu",
];
const PIC_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "avif", "svg", "tif", "tiff", "ico", "raw", "dng", "cr2", "nef", "arw"];
const FILM_EXTS: &[&str] = &["mp4", "m4v", "mov", "webm", "3gp", "mkv", "avi", "wmv", "flv", "mpg", "mpeg", "ts"];
const SOUND_EXTS: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aiff", "aif", "alac", "mid", "midi"];

pub(super) fn file_type(e: &Entry) -> FileType {
    if e.dir {
        return FileType::Folder;
    }
    let x = ext(&e.name);
    let x = x.as_str();
    if x == "epub" {
        FileType::Book
    } else if PIC_EXTS.contains(&x) {
        FileType::Image
    } else if FILM_EXTS.contains(&x) {
        FileType::Video
    } else if SOUND_EXTS.contains(&x) {
        FileType::Music
    } else if DOC_EXTS.contains(&x) {
        FileType::Document
    } else if code_language(&e.name).is_some() || TEXT_EXTS.contains(&x) {
        FileType::Code
    } else if is_text_name(&e.name) {
        // README, LICENSE and the like.
        FileType::Document
    } else {
        FileType::Other
    }
}

/// The item's icon, drawn on a 48×48 grid: a page with a mark in the type's colour, a folder or a
/// book. Any other file shows its extension on the page.
pub(super) fn type_icon(e: &Entry) -> AnyView {
    const PAGE: &str = r#"<path class="page" d="M12 4h17l9 9v29a2 2 0 0 1-2 2H12a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z"/><path class="page" d="M29 4v9h9"/>"#;
    let t = file_type(e);
    let (class, mark) = match t {
        FileType::Folder => ("folder", r#"<path class="fill" d="M5 13a3 3 0 0 1 3-3h10l4 4h18a3 3 0 0 1 3 3v20a3 3 0 0 1-3 3H8a3 3 0 0 1-3-3z"/>"#),
        FileType::Book => ("book", r#"<path class="fill" d="M14 5h22a2 2 0 0 1 2 2v35H15a4 4 0 0 1-4-4V8a3 3 0 0 1 3-3z"/><path class="mark" d="M11 38a4 4 0 0 1 4-4h23M18 13h13M18 19h9"/>"#),
        FileType::Document => ("doc", r#"<path class="mark" d="M16 22h16M16 28h16M16 34h10"/>"#),
        FileType::Code => ("code", r#"<path class="mark" d="M19 23l-5 5 5 5M29 23l5 5-5 5M26 20l-4 16"/>"#),
        FileType::Image => ("image", r#"<path class="solid" d="M15 37l7-9 5 6 3-4 5 7z"/><circle class="solid" cx="29" cy="21" r="2.5"/>"#),
        FileType::Video => ("video", r#"<path class="solid" d="M19 21v14l12-7z"/>"#),
        FileType::Music => ("music", r#"<path class="mark" d="M21 35V22l11-2v12"/><circle class="solid" cx="18.5" cy="35" r="2.5"/><circle class="solid" cx="29.5" cy="32" r="2.5"/>"#),
        FileType::Other => ("other", ""),
    };
    let page = if matches!(t, FileType::Folder | FileType::Book) { "" } else { PAGE };
    // The extension on a page that has no mark, if it's short enough to fit.
    let label = match ext(&e.name) {
        x if t == FileType::Other && !x.is_empty() && x.chars().count() <= 4 => format!(r#"<text x="24" y="33">{}</text>"#, x.to_uppercase()),
        _ => String::new(),
    };
    let svg = format!("{page}{mark}{label}");
    view! { <svg class=format!("ft ft-{class}") viewBox="0 0 48 48" aria-hidden="true" inner_html=svg></svg> }.into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> Entry {
        Entry { name: name.into(), path: format!("/{name}"), dir: false, size: 10, mtime: 0 }
    }

    #[test]
    fn sorts_files_by_type() {
        assert_eq!(file_type(&Entry { dir: true, ..file("Photos") }), FileType::Folder);
        assert_eq!(file_type(&file("a.EPUB")), FileType::Book);
        assert_eq!(file_type(&file("a.pdf")), FileType::Document);
        assert_eq!(file_type(&file("notes.md")), FileType::Document);
        assert_eq!(file_type(&file("main.rs")), FileType::Code);
        assert_eq!(file_type(&file("app.json")), FileType::Code);
        assert_eq!(file_type(&file("IMG_1.HEIC")), FileType::Image);
        assert_eq!(file_type(&file("clip.mkv")), FileType::Video);
        assert_eq!(file_type(&file("song.flac")), FileType::Music);
        assert_eq!(file_type(&file("data.bin")), FileType::Other);
        assert_eq!(file_type(&file("README")), FileType::Document);
    }
}
