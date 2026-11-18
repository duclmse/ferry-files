//! The `yomi://` scheme serves files from open EPUBs, so the chapter view can load the book's
//! images, styles and fonts:
//!   GET yomi://localhost/book/{id}/{path inside the zip}

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tauri::http::{Request, Response, StatusCode};
use tauri::AppHandle;

/// Keep these unescaped in path segments; everything else is percent-encoded.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn encode_path(p: &str) -> String {
    p.split('/').map(|s| utf8_percent_encode(s, SEGMENT).to_string()).collect::<Vec<_>>().join("/")
}

/// URL of a file in an open book. `rev` changes whenever the book does, so edits show up.
pub fn book_url(id: u32, path: &str, rev: u32) -> String {
    format!("yomi://localhost/book/{id}/{}?v={rev}", encode_path(path))
}

/// Base URL for relative links in a chapter: the chapter's folder, with a trailing slash.
pub fn book_base(id: u32, chapter: &str) -> String {
    let dir = chapter.rfind('/').map_or("", |k| &chapter[..=k]);
    format!("yomi://localhost/book/{id}/{}", encode_path(dir))
}

fn respond(status: StatusCode, ctype: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Cache-Control", "no-store")
        // Fonts are fetched with CORS; the chapter view's origin is the app's, not yomi://.
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

pub fn handle(app: &AppHandle, req: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let not_found = || respond(StatusCode::NOT_FOUND, "text/plain", b"Not found.".to_vec());
    if req.method() != "GET" {
        return respond(StatusCode::METHOD_NOT_ALLOWED, "text/plain", b"Method not allowed.".to_vec());
    }
    let path = req.uri().path().to_string();
    let Some(rest) = path.strip_prefix("/book/") else { return not_found() };
    let Some((id, file)) = rest.split_once('/') else { return not_found() };
    let Ok(id) = id.parse::<u32>() else { return not_found() };
    let file = percent_decode_str(file).decode_utf8_lossy().into_owned();
    let state = crate::state(app);
    let books = state.books.lock().unwrap_or_else(|e| e.into_inner());
    let Some(book) = books.get(&id) else { return not_found() };
    match book.file(&file) {
        Some(data) => respond(StatusCode::OK, &book.media_type(&file), data.to_vec()),
        None => not_found(),
    }
}
