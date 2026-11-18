//! The `yomi://` scheme:
//!   GET yomi://localhost/book/{id}/{path inside the zip}
//!       files from open EPUBs, so the chapter view can load the book's images, styles and fonts.
//!   GET yomi://localhost/thumb?src={mac|device id}&p={path}&s={size}&m={mtime}
//!       a thumbnail of an image file or an EPUB's cover (see thumbs.rs).

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tauri::http::{Request, Response, StatusCode};
use tauri::AppHandle;
use crate::thumbs::{self, Thumb};

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
    respond_with(status, ctype, "no-store", body)
}

fn respond_with(status: StatusCode, ctype: &str, cache: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Cache-Control", cache)
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
    if path == "/thumb" {
        return thumb(app, req.uri().query().unwrap_or(""));
    }
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

fn query_param(q: &str, name: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| percent_decode_str(v).decode_utf8_lossy().into_owned())
    })
}

fn thumb_response(t: Thumb) -> Response<Vec<u8>> {
    match t {
        // The URL changes when the file does (size and mtime are in it), so the webview may keep it.
        Thumb::Image(data, mime) => respond_with(StatusCode::OK, mime, "max-age=86400", data),
        Thumb::None => respond(StatusCode::NOT_FOUND, "text/plain", b"No thumbnail.".to_vec()),
    }
}

/// Phone thumbnails are made one at a time, so at most one of them waits for the device alongside
/// the user's own actions, and requests for a folder no pane shows any more are dropped instead of
/// read. Mac files are quick to read, so their thumbnails skip both.
fn thumb(app: &AppHandle, q: &str) -> Response<Vec<u8>> {
    let bad = || respond(StatusCode::BAD_REQUEST, "text/plain", b"Bad thumbnail request.".to_vec());
    let Some(path) = query_param(q, "p") else { return bad() };
    let Some(size) = query_param(q, "s").and_then(|s| s.parse::<u64>().ok()) else { return bad() };
    let Some(mtime) = query_param(q, "m").and_then(|s| s.parse::<i64>().ok()) else { return bad() };
    let Some(src) = query_param(q, "src") else { return bad() };
    let state = crate::state(app);
    let dev = match state.source(Some(&src)) {
        Ok(d) => d,
        Err(e) => return respond(StatusCode::NOT_FOUND, "text/plain", e.into_bytes()),
    };
    let phone = src != "mac";
    // The cache key names the device rather than its id, which changes on every connection.
    let key = if phone { format!("{}:{}", dev.kind(), dev.label()) } else { "mac".to_string() };
    if let Some(t) = thumbs::cached(&key, &path, size, mtime) {
        return thumb_response(t);
    }
    let _one = phone.then(|| state.thumb_lock.lock().unwrap_or_else(|e| e.into_inner()));
    if phone {
        if let Some(t) = thumbs::cached(&key, &path, size, mtime) {
            return thumb_response(t);
        }
        if !state.is_viewing(&src, &crate::device::dirname(&path)) {
            return respond(StatusCode::SERVICE_UNAVAILABLE, "text/plain", b"Folder no longer shown.".to_vec());
        }
    }
    let made = thumbs::make(&key, &dev, &path, size, mtime);
    match made {
        Ok(t) => thumb_response(t),
        Err(e) => respond(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e.into_bytes()),
    }
}
