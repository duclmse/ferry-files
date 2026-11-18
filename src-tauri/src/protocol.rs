//! The `ferry://` scheme:
//!   GET ferry://localhost/book/{id}/{path inside the zip}
//!       files from open EPUBs, so the chapter view can load the book's images, styles and fonts.
//!   GET ferry://localhost/thumb?src={mac|device id}&p={path}&s={size}&m={mtime}
//!       a thumbnail of an image file or an EPUB's cover (see thumbs.rs).
//!   GET ferry://localhost/file?src={mac|device id}&p={path}&s={size}&m={mtime}
//!       a file itself, for the preview's image, video and audio. Honours Range, and reads only the
//!       part asked for, so a video starts and seeks without copying the file to the Mac.

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
    format!("ferry://localhost/book/{id}/{}?v={rev}", encode_path(path))
}

/// Base URL for relative links in a chapter: the chapter's folder, with a trailing slash.
pub fn book_base(id: u32, chapter: &str) -> String {
    let dir = chapter.rfind('/').map_or("", |k| &chapter[..=k]);
    format!("ferry://localhost/book/{id}/{}", encode_path(dir))
}

fn respond(status: StatusCode, ctype: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    respond_with(status, ctype, "no-store", body)
}

fn respond_with(status: StatusCode, ctype: &str, cache: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Cache-Control", cache)
        // Fonts are fetched with CORS; the chapter view's origin is the app's, not ferry://.
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
    if path == "/zipped" {
        let range = req.headers().get("range").and_then(|v| v.to_str().ok()).map(str::to_string);
        return zipped(app, req.uri().query().unwrap_or(""), range.as_deref());
    }
    if path == "/cover" {
        return cover(app, req.uri().query().unwrap_or(""));
    }
    if path == "/file" {
        let range = req.headers().get("range").and_then(|v| v.to_str().ok()).map(str::to_string);
        return file(app, req.uri().query().unwrap_or(""), range.as_deref());
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

/// A file packed in a ZIP (`e`, its name in it), unpacked whole, then sent in the parts asked
/// for, as `file` does.
fn zipped(app: &AppHandle, q: &str, range: Option<&str>) -> Response<Vec<u8>> {
    let bad = || respond(StatusCode::BAD_REQUEST, "text/plain", b"Bad request.".to_vec());
    let (Some(path), Some(src), Some(name)) = (query_param(q, "p"), query_param(q, "src"), query_param(q, "e")) else { return bad() };
    let Some(size) = query_param(q, "s").and_then(|s| s.parse::<u64>().ok()) else { return bad() };
    let dev = match crate::state(app).source(Some(&src)) {
        Ok(d) => d,
        Err(e) => return respond(StatusCode::NOT_FOUND, "text/plain", e.into_bytes()),
    };
    let data = match crate::files::unpacked(&dev, &path, size, &name) {
        Ok(d) => d,
        Err(e) => return respond(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e.into_bytes()),
    };
    let len = data.len() as u64;
    let base = || {
        Response::builder()
            .header("Content-Type", media_type(&name))
            .header("Accept-Ranges", "bytes")
            .header("Cache-Control", "no-store")
            .header("Access-Control-Allow-Origin", "*")
    };
    let body = match range.and_then(|h| parse_range(h, len)) {
        Some(Err(())) => base().status(StatusCode::RANGE_NOT_SATISFIABLE).header("Content-Range", format!("bytes */{len}")).body(vec![]),
        Some(Ok((start, end))) => base()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Content-Range", format!("bytes {start}-{end}/{len}"))
            .body(data[start as usize..=end as usize].to_vec()),
        None => base().status(StatusCode::OK).body(data.to_vec()),
    };
    body.unwrap_or_else(|_| Response::new(Vec::new()))
}

/// An EPUB's cover at its own size (the preview's, when it's bigger than the thumbnail).
fn cover(app: &AppHandle, q: &str) -> Response<Vec<u8>> {
    let (Some(path), Some(src)) = (query_param(q, "p"), query_param(q, "src")) else {
        return respond(StatusCode::BAD_REQUEST, "text/plain", b"Bad cover request.".to_vec());
    };
    let size = query_param(q, "s").and_then(|s| s.parse::<u64>().ok()).unwrap_or(u64::MAX);
    let dev = match crate::state(app).source(Some(&src)) {
        Ok(d) => d,
        Err(e) => return respond(StatusCode::NOT_FOUND, "text/plain", e.into_bytes()),
    };
    match thumbs::cover(&dev, &path, size) {
        // Size and mtime are in the URL, so it changes when the book does.
        Ok(Some((data, mime))) => respond_with(StatusCode::OK, &mime, "max-age=86400", data),
        Ok(None) => respond(StatusCode::NOT_FOUND, "text/plain", b"No cover.".to_vec()),
        Err(e) => respond(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e.into_bytes()),
    }
}

/// Most bytes sent for one range request. The player asks again for the rest; small pieces keep
/// seeking quick and memory low.
const CHUNK: u64 = 2 * 1024 * 1024;
/// Most bytes sent for a request without Range (an `<img>`): the whole file.
const WHOLE_MAX: u64 = 64 * 1024 * 1024;

/// The bytes a Range header asks for, as (start, end inclusive), cut to `CHUNK` and the file.
/// None: no usable range (send the whole file). Some(Err): a range past the end.
fn parse_range(h: &str, size: u64) -> Option<std::result::Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    // Several ranges at once aren't worth it here; take the first.
    let (a, b) = spec.split(',').next()?.trim().split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (size.saturating_sub(n), size.saturating_sub(1))
        }
        (a, "") => (a.parse().ok()?, size.saturating_sub(1)),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(size.saturating_sub(1))),
    };
    if size == 0 || start >= size || end < start {
        return Some(Err(()));
    }
    Some(Ok((start, end.min(start + CHUNK - 1))))
}

fn media_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heic",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "pdf" => "application/pdf",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn file(app: &AppHandle, q: &str, range: Option<&str>) -> Response<Vec<u8>> {
    let bad = || respond(StatusCode::BAD_REQUEST, "text/plain", b"Bad file request.".to_vec());
    let Some(path) = query_param(q, "p") else { return bad() };
    let Some(size) = query_param(q, "s").and_then(|s| s.parse::<u64>().ok()) else { return bad() };
    let Some(src) = query_param(q, "src") else { return bad() };
    let dev = match crate::state(app).source(Some(&src)) {
        Ok(d) => d,
        Err(e) => return respond(StatusCode::NOT_FOUND, "text/plain", e.into_bytes()),
    };
    let ctype = media_type(&path);
    let fail = |e: crate::device::DevError| respond(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", e.to_string().into_bytes());
    let base = || {
        Response::builder()
            .header("Content-Type", ctype)
            .header("Accept-Ranges", "bytes")
            // Not kept by the web view: a video's pieces would pile up in its memory cache.
            .header("Cache-Control", "no-store")
            .header("Access-Control-Allow-Origin", "*")
    };
    match range.and_then(|h| parse_range(h, size)) {
        Some(Err(())) => base()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header("Content-Range", format!("bytes */{size}"))
            .body(vec![])
            .unwrap_or_else(|_| Response::new(Vec::new())),
        Some(Ok((start, end))) => match dev.read_range(&path, start, end - start + 1) {
            // The file may have shrunk since it was listed; say what was really read.
            Ok(data) if !data.is_empty() => {
                let end = start + data.len() as u64 - 1;
                base()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header("Content-Range", format!("bytes {start}-{end}/{size}"))
                    .body(data)
                    .unwrap_or_else(|_| Response::new(Vec::new()))
            }
            Ok(_) => base()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{size}"))
                .body(vec![])
                .unwrap_or_else(|_| Response::new(Vec::new())),
            Err(e) => fail(e),
        },
        None if size > WHOLE_MAX => respond(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", b"Too big to show whole.".to_vec()),
        None => match dev.read_range(&path, 0, size) {
            Ok(data) => base().status(StatusCode::OK).body(data).unwrap_or_else(|_| Response::new(Vec::new())),
            Err(e) => fail(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_range, CHUNK};

    #[test]
    fn ranges_are_read_and_cut_to_a_chunk() {
        assert_eq!(parse_range("bytes=0-1", 100), Some(Ok((0, 1))));
        assert_eq!(parse_range("bytes=10-", 100), Some(Ok((10, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=50-500", 100), Some(Ok((50, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=0-", 10 * CHUNK), Some(Ok((0, CHUNK - 1))));
        assert_eq!(parse_range("items=0-1", 100), None);
        assert_eq!(parse_range("bytes=x-1", 100), None);
    }

    #[test]
    fn mac_files_are_read_in_parts() {
        let f = std::env::temp_dir().join(format!("ferry-range-test-{}", std::process::id()));
        std::fs::write(&f, b"0123456789").unwrap();
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let p = f.display().to_string();
        assert_eq!(mac.read_range(&p, 3, 4).unwrap(), b"3456");
        assert_eq!(mac.read_range(&p, 8, 100).unwrap(), b"89");
        assert!(mac.read_range(&p, 20, 5).unwrap().is_empty());
        std::fs::remove_file(&f).unwrap();
    }
}
