//! Filesystem operations API.
//!
//! Routes:
//!   HEAD   /fs/read?path=             — file metadata as response headers
//!   GET    /fs/read?path=             — stream file; `Range:` header → 206 Partial Content
//!   PUT    /fs/write?path=            — write/overwrite file content (request body → file)
//!   POST   /fs/copy[?stream=true]     — copy file or directory { "from", "to" }
//!   POST   /fs/move[?stream=true]     — move / rename          { "from", "to" }
//!   DELETE /fs/delete?path=[&stream=true] — delete file or directory
//!   POST   /fs/mkdir?path=            — create directory (with parents)
//!
//! **Streaming progress** (`?stream=true`): copy/move/delete return
//! `text/event-stream` (SSE).  Each event is a JSON object:
//!   `{ "processed": N, "total": N, "current": "path", "done": bool, "error"?: "..." }`
//!
//! Paths must be absolute.  Relative paths and `..` components are rejected
//! with 400 to prevent directory traversal.

use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::{
    body::Body,
    extract::{Query, Request, State},
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;

use crate::api::rest::AppState;

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/fs/read",   get(read_handler).head(head_handler))
        .route("/fs/write",  put(write_handler))
        .route("/fs/copy",   post(copy_handler))
        .route("/fs/move",   post(move_handler))
        .route("/fs/delete", delete(delete_handler))
        .route("/fs/mkdir",  post(mkdir_handler))
        .route("/fs/drives", get(drives_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// One entry returned by `GET /fs/read` when the path is a directory.
#[derive(Serialize)]
struct DirEntry {
    name:       String,
    path:       String,
    is_dir:     bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size:       Option<u64>,
    mtime_secs: u64,
}

/// Progress event emitted over SSE for long-running operations.
#[derive(Serialize, Clone)]
pub struct ProgressEvent {
    pub processed: usize,
    pub total:     usize,
    pub current:   String,
    pub done:      bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:     Option<String>,
}

#[derive(Deserialize)]
struct PathParam {
    path:   Option<String>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
struct StreamParam {
    #[serde(default)]
    stream: bool,
}

fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::BAD_REQUEST, Json(ErrorBody { error: msg.into() }))
}

fn not_found(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::NOT_FOUND, Json(ErrorBody { error: msg.into() }))
}

fn internal(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody { error: msg.into() }))
}

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

/// Reject empty, relative, and `..`-containing paths.
fn validate_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("path must not be empty".into());
    }
    let p = PathBuf::from(raw);
    if p.is_relative() {
        return Err("path must be absolute".into());
    }
    for c in p.components() {
        if c == Component::ParentDir {
            return Err("path must not contain '..' components".into());
        }
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// Helpers — mime, etag, range
// ---------------------------------------------------------------------------

fn mime_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("txt" | "md" | "rs" | "toml" | "yaml" | "yml" | "json" | "xml"
            | "csv" | "log" | "ini" | "cfg" | "conf" | "sh" | "ps1" | "py"
            | "js" | "ts" | "html" | "css" | "c" | "cpp" | "h" | "go") => "text/plain; charset=utf-8",
        Some("png")       => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif")       => "image/gif",
        Some("pdf")       => "application/pdf",
        Some("zip")       => "application/zip",
        _                 => "application/octet-stream",
    }
}

fn file_etag(mtime_secs: u64, size: u64) -> String {
    format!("\"{mtime_secs:016x}-{size:016x}\"")
}

/// Parse a single `bytes=start-end` range.  Returns `(start, end)` (both
/// inclusive) or `None` when the range string is malformed or unsatisfiable
/// given `file_size`.  Multi-range (comma-separated) is not supported.
fn parse_range(range: &str, file_size: u64) -> Option<(u64, u64)> {
    let s = range.strip_prefix("bytes=")?;
    if s.contains(',') {
        return None; // multi-range: not supported
    }
    let (start_str, end_str) = s.split_once('-')?;

    if start_str.is_empty() {
        // Suffix form: bytes=-N  →  last N bytes
        let n: u64 = end_str.parse().ok()?;
        if n == 0 || file_size == 0 {
            return None;
        }
        Some((file_size.saturating_sub(n), file_size - 1))
    } else {
        let start: u64 = start_str.parse().ok()?;
        if start >= file_size {
            return None; // range starts beyond EOF
        }
        let end: u64 = if end_str.is_empty() {
            file_size - 1
        } else {
            end_str.parse::<u64>().ok()?.min(file_size - 1)
        };
        if start > end { return None; }
        Some((start, end))
    }
}

// ---------------------------------------------------------------------------
// Helpers — metadata extraction
// ---------------------------------------------------------------------------

struct FileMeta {
    size:       u64,
    mtime_secs: u64,
    is_dir:     bool,
    mode_str:   String,
}

fn extract_meta(meta: &std::fs::Metadata) -> FileMeta {
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    #[cfg(unix)]
    let mode_str = {
        use std::os::unix::fs::MetadataExt;
        format!("{:o}", meta.mode())
    };
    #[cfg(windows)]
    let mode_str = {
        use std::os::windows::fs::MetadataExt;
        format!("0x{:08X}", meta.file_attributes())
    };
    #[cfg(not(any(unix, windows)))]
    let mode_str = String::new();

    FileMeta { size: meta.len(), mtime_secs, is_dir: meta.is_dir(), mode_str }
}

// ---------------------------------------------------------------------------
// HEAD /fs/read?path=  — file metadata as headers, empty body
// ---------------------------------------------------------------------------

async fn head_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p)  => p,
        Err(e) => return bad_request(e).into_response(),
    };

    let std_meta = match std::fs::metadata(&path) {
        Ok(m)  => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
               => return not_found(format!("path not found: {raw}")).into_response(),
        Err(e) => return internal(e.to_string()).into_response(),
    };

    let fm   = extract_meta(&std_meta);
    let etag = file_etag(fm.mtime_secs, fm.size);
    let mime = if fm.is_dir { "inode/directory" } else { mime_for_path(&path) };

    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE,   mime)
        .header(header::CONTENT_LENGTH, fm.size)
        .header(header::ACCEPT_RANGES,  "bytes")
        .header(header::ETAG,           etag)
        .header("X-File-Size",          fm.size)
        .header("X-File-Mtime",         fm.mtime_secs)
        .header("X-File-Is-Dir",        if fm.is_dir { "true" } else { "false" })
        .header("X-File-Mode",          fm.mode_str)
        .body(Body::empty())
        .unwrap()
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /fs/read?path=  — stream file; honours Range header
// ---------------------------------------------------------------------------

async fn read_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
    req_headers:   axum::http::HeaderMap,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p)  => p,
        Err(e) => return bad_request(e).into_response(),
    };

    let std_meta = match tokio::fs::metadata(&path).await {
        Ok(m) if m.is_dir() => return list_dir(&path).await,
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
               => return not_found(format!("path not found: {raw}")).into_response(),
        Err(e) => return internal(e.to_string()).into_response(),
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f)  => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
               => return not_found(format!("file not found: {raw}")).into_response(),
        Err(e) => return internal(e.to_string()).into_response(),
    };

    let file_size   = std_meta.len();
    let mime        = mime_for_path(&path);
    let fm          = extract_meta(&std_meta);
    let etag        = file_etag(fm.mtime_secs, fm.size);

    // ── Range request ───────────────────────────────────────────────────────
    if let Some(range_val) = req_headers.get(header::RANGE) {
        let range_str = match range_val.to_str() {
            Ok(s)  => s,
            Err(_) => return bad_request("invalid Range header encoding").into_response(),
        };

        match parse_range(range_str, file_size) {
            None => {
                // 416 Range Not Satisfiable
                return axum::http::Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{file_size}"))
                    .body(Body::empty())
                    .unwrap()
                    .into_response();
            }
            Some((start, end)) => {
                use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
                if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                    return internal(e.to_string()).into_response();
                }
                let len = end - start + 1;
                let limited = file.take(len);
                return axum::http::Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE,   mime)
                    .header(header::CONTENT_LENGTH, len)
                    .header(header::CONTENT_RANGE,  format!("bytes {start}-{end}/{file_size}"))
                    .header(header::ACCEPT_RANGES,  "bytes")
                    .header(header::ETAG,           etag)
                    .body(Body::from_stream(ReaderStream::new(limited)))
                    .unwrap()
                    .into_response();
            }
        }
    }

    // ── Full file ────────────────────────────────────────────────────────────
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE,   mime)
        .header(header::CONTENT_LENGTH, file_size)
        .header(header::ACCEPT_RANGES,  "bytes")
        .header(header::ETAG,           etag)
        .body(Body::from_stream(ReaderStream::new(file)))
        .unwrap()
        .into_response()
}

// ---------------------------------------------------------------------------
// Directory listing helper — called by read_handler when path is a dir
// ---------------------------------------------------------------------------

async fn list_dir(path: &Path) -> Response {
    let mut rd = match tokio::fs::read_dir(path).await {
        Ok(rd) => rd,
        Err(e) => return internal(e.to_string()).into_response(),
    };
    let mut entries: Vec<DirEntry> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(entry)) => {
                let meta = match entry.metadata().await {
                    Ok(m)  => m,
                    Err(_) => continue,
                };
                let fm   = extract_meta(&meta);
                entries.push(DirEntry {
                    name:       entry.file_name().to_string_lossy().into_owned(),
                    path:       entry.path().to_string_lossy().into_owned(),
                    is_dir:     fm.is_dir,
                    size:       (!fm.is_dir).then_some(fm.size),
                    mtime_secs: fm.mtime_secs,
                });
            }
            Ok(None) => break,
            Err(e)   => return internal(e.to_string()).into_response(),
        }
    }
    Json(entries).into_response()
}

// ---------------------------------------------------------------------------
// PUT /fs/write?path=
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WriteResponse {
    written: u64,
}

async fn write_handler(
    State(_state): State<AppState>,
    Query(params):  Query<PathParam>,
    request:        Request,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p)  => p,
        Err(e) => return bad_request(e).into_response(),
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return internal(format!("could not create parent directories: {e}")).into_response();
        }
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(b)  => b,
        Err(e) => return internal(format!("body read error: {e}")).into_response(),
    };
    let written = bytes.len() as u64;

    if let Err(e) = tokio::fs::write(&path, &bytes).await {
        return internal(e.to_string()).into_response();
    }

    Json(WriteResponse { written }).into_response()
}

// ---------------------------------------------------------------------------
// SSE progress helpers
// ---------------------------------------------------------------------------

type ProgressTx = tokio::sync::mpsc::Sender<ProgressEvent>;

fn send_progress(tx: &ProgressTx, ev: ProgressEvent) {
    // Called from spawn_blocking threads; blocking_send is the right API.
    let _ = tx.blocking_send(ev);
}

fn make_sse(rx: tokio::sync::mpsc::Receiver<ProgressEvent>) -> Response {
    let stream = ReceiverStream::new(rx).map(|ev| {
        Ok::<Event, Infallible>(
            Event::default()
                .json_data(&ev)
                .unwrap_or_else(|_| Event::default().data("progress")),
        )
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /fs/copy[?stream=true]  { "from": "...", "to": "..." }
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FromToBody {
    from: String,
    to:   String,
}

async fn copy_handler(
    State(_state): State<AppState>,
    Query(qp):     Query<StreamParam>,
    Json(body):    Json<FromToBody>,
) -> Response {
    let src = match validate_path(&body.from) {
        Ok(p)  => p,
        Err(e) => return bad_request(format!("'from': {e}")).into_response(),
    };
    let dst = match validate_path(&body.to) {
        Ok(p)  => p,
        Err(e) => return bad_request(format!("'to': {e}")).into_response(),
    };

    if qp.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
        tokio::task::spawn_blocking(move || {
            let total = count_entries(&src);
            send_progress(&tx, ProgressEvent { processed: 0, total, current: String::new(), done: false, error: None });
            let mut processed = 0;
            match copy_recursive_tracked(&src, &dst, &tx, &mut processed, total) {
                Ok(()) => send_progress(&tx, ProgressEvent { processed: total, total, current: String::new(), done: true, error: None }),
                Err(e) => send_progress(&tx, ProgressEvent { processed, total, current: String::new(), done: true, error: Some(e) }),
            }
        });
        make_sse(rx)
    } else {
        let r = tokio::task::spawn_blocking(move || copy_recursive(&src, &dst))
            .await.map_err(|e| e.to_string());
        match r {
            Ok(Ok(())) => Json(OkResponse { ok: true }).into_response(),
            Ok(Err(e)) => internal(e).into_response(),
            Err(e)     => internal(e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// POST /fs/move[?stream=true]  { "from": "...", "to": "..." }
// ---------------------------------------------------------------------------

async fn move_handler(
    State(_state): State<AppState>,
    Query(qp):     Query<StreamParam>,
    Json(body):    Json<FromToBody>,
) -> Response {
    let src = match validate_path(&body.from) {
        Ok(p)  => p,
        Err(e) => return bad_request(format!("'from': {e}")).into_response(),
    };
    let dst = match validate_path(&body.to) {
        Ok(p)  => p,
        Err(e) => return bad_request(format!("'to': {e}")).into_response(),
    };

    if let Some(parent) = dst.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return internal(format!("cannot create destination directory: {e}")).into_response();
        }
    }

    if qp.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
        tokio::task::spawn_blocking(move || {
            // Try atomic rename first (O(1), same filesystem).
            if std::fs::rename(&src, &dst).is_ok() {
                send_progress(&tx, ProgressEvent { processed: 1, total: 1, current: src.display().to_string(), done: true, error: None });
                return;
            }
            // Fall back to copy+delete across filesystems.
            let total = count_entries(&src);
            send_progress(&tx, ProgressEvent { processed: 0, total, current: String::new(), done: false, error: None });
            let mut processed = 0;
            let r = copy_recursive_tracked(&src, &dst, &tx, &mut processed, total)
                .and_then(|()| std::fs::remove_dir_all(&src).map_err(|e| format!("remove source: {e}")));
            match r {
                Ok(())  => send_progress(&tx, ProgressEvent { processed: total, total, current: String::new(), done: true, error: None }),
                Err(e)  => send_progress(&tx, ProgressEvent { processed, total, current: String::new(), done: true, error: Some(e) }),
            }
        });
        make_sse(rx)
    } else {
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => return Json(OkResponse { ok: true }).into_response(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound
                   => return not_found(format!("source not found: {}", src.display())).into_response(),
            Err(_) => {} // fallthrough: cross-filesystem rename
        }
        let r = tokio::task::spawn_blocking(move || {
            copy_recursive(&src, &dst)?;
            std::fs::remove_dir_all(&src).map_err(|e| format!("remove source: {e}"))
        }).await.map_err(|e| e.to_string());
        match r {
            Ok(Ok(())) => Json(OkResponse { ok: true }).into_response(),
            Ok(Err(e)) => internal(e).into_response(),
            Err(e)     => internal(e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /fs/delete?path=[&stream=true]
// ---------------------------------------------------------------------------

async fn delete_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p)  => p,
        Err(e) => return bad_request(e).into_response(),
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(m)  => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
               => return not_found(format!("path not found: {raw}")).into_response(),
        Err(e) => return internal(e.to_string()).into_response(),
    };

    if params.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
        tokio::task::spawn_blocking(move || {
            let total = count_entries(&path);
            send_progress(&tx, ProgressEvent { processed: 0, total, current: String::new(), done: false, error: None });
            let mut processed = 0;
            match delete_recursive_tracked(&path, &tx, &mut processed, total) {
                Ok(()) => send_progress(&tx, ProgressEvent { processed: total, total, current: String::new(), done: true, error: None }),
                Err(e) => send_progress(&tx, ProgressEvent { processed, total, current: String::new(), done: true, error: Some(e) }),
            }
        });
        make_sse(rx)
    } else {
        let r = if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        match r {
            Ok(()) => Json(OkResponse { ok: true }).into_response(),
            Err(e) => internal(e.to_string()).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// POST /fs/mkdir?path=
// ---------------------------------------------------------------------------

async fn mkdir_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p)  => p,
        Err(e) => return bad_request(e).into_response(),
    };

    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Blocking helpers — called only from spawn_blocking
// ---------------------------------------------------------------------------

/// Count all files under `root` (directories not counted; they have no size
/// progress to report).  Iterative to avoid stack overflow on deep trees.
fn count_entries(root: &Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_file() {
            count += 1;
        } else if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    count
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(src)
        .map_err(|e| format!("cannot read source: {e}"))?;
    if meta.is_file() {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("cannot create destination directory: {e}"))?;
        }
        std::fs::copy(src, dst).map(|_| ()).map_err(|e| format!("copy failed: {e}"))
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| format!("cannot create destination directory: {e}"))?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        Err("source is neither a file nor a directory".into())
    }
}

fn copy_recursive_tracked(
    src:       &Path,
    dst:       &Path,
    tx:        &ProgressTx,
    processed: &mut usize,
    total:     usize,
) -> Result<(), String> {
    let meta = std::fs::metadata(src)
        .map_err(|e| format!("cannot read source: {e}"))?;
    if meta.is_file() {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        std::fs::copy(src, dst).map_err(|e| format!("copy failed: {e}"))?;
        *processed += 1;
        send_progress(tx, ProgressEvent {
            processed: *processed,
            total,
            current:   src.display().to_string(),
            done:      false,
            error:     None,
        });
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_recursive_tracked(&entry.path(), &dst.join(entry.file_name()), tx, processed, total)?;
        }
    }
    Ok(())
}

fn delete_recursive_tracked(
    path:      &Path,
    tx:        &ProgressTx,
    processed: &mut usize,
    total:     usize,
) -> Result<(), String> {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            delete_recursive_tracked(&entry.path(), tx, processed, total)?;
        }
        std::fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
        *processed += 1;
        send_progress(tx, ProgressEvent {
            processed: *processed,
            total,
            current:   path.display().to_string(),
            done:      false,
            error:     None,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GET /fs/drives  — enumerate logical drives (Windows) / stub (other platforms)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DriveInfo {
    pub letter:      String,
    pub root:        String,
    pub kind:        &'static str,
    pub label:       Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes:  Option<u64>,
}

async fn drives_handler() -> impl IntoResponse {
    let drives = tokio::task::spawn_blocking(list_drives)
        .await
        .unwrap_or_default();
    Json(drives)
}

#[cfg(windows)]
fn list_drives() -> Vec<DriveInfo> {
    use windows::{
        Win32::Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        },
        core::PCWSTR,
    };

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED:     u32 = 3;
    const DRIVE_REMOTE:    u32 = 4;
    const DRIVE_CDROM:     u32 = 5;
    const DRIVE_RAMDISK:   u32 = 6;

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();

    for bit in 0u32..26 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let letter = (b'A' + bit as u8) as char;
        let root_str = format!("{letter}:\\");
        let root_wide: Vec<u16> = root_str.encode_utf16().chain(std::iter::once(0)).collect();
        let root_pcwstr = PCWSTR(root_wide.as_ptr());

        let kind = match unsafe { GetDriveTypeW(root_pcwstr) } {
            DRIVE_FIXED     => "fixed",
            DRIVE_REMOVABLE => "removable",
            DRIVE_REMOTE    => "network",
            DRIVE_CDROM     => "cdrom",
            DRIVE_RAMDISK   => "ramdisk",
            _               => "unknown",
        };

        // Volume label (may fail for empty optical drives, etc.)
        let mut label_buf = vec![0u16; 256];
        let label = unsafe {
            GetVolumeInformationW(
                root_pcwstr,
                Some(&mut label_buf),
                None,
                None,
                None,
                None,
            )
        }
        .ok()
        .and_then(|_| {
            let len = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16(&label_buf[..len]).ok()
        })
        .filter(|s| !s.is_empty());

        // Disk space (may fail for ejected media)
        let mut total = 0u64;
        let mut free  = 0u64;
        let space_ok = unsafe {
            GetDiskFreeSpaceExW(
                root_pcwstr,
                None,
                Some(&mut total),
                Some(&mut free),
            )
        }
        .is_ok();

        out.push(DriveInfo {
            letter:      format!("{letter}:"),
            root:        root_str,
            kind,
            label,
            total_bytes: space_ok.then_some(total),
            free_bytes:  space_ok.then_some(free),
        });
    }
    out
}

#[cfg(not(windows))]
fn list_drives() -> Vec<DriveInfo> {
    vec![]
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 16);
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => out.push(b as char),
                other => { out.push('%'); out.push_str(&format!("{other:02X}")); }
            }
        }
        out
    }

    fn make_test_app(state: AppState) -> axum::Router {
        axum::Router::new().merge(router(state.clone())).with_state(state)
    }

    // --- validate_path ---

    #[test]
    fn validate_empty_rejected()           { assert!(validate_path("").is_err()); }
    #[test]
    fn validate_relative_rejected()        { assert!(validate_path("relative/path").is_err()); }
    #[test]
    fn validate_parent_component_rejected(){ assert!(validate_path("/some/../etc/passwd").is_err()); }
    #[test]
    fn validate_absolute_clean_path_accepted() {
        #[cfg(unix)]
        assert!(validate_path("/absolute/clean/path").is_ok());
        #[cfg(windows)]
        assert!(validate_path(r"C:\Users\user\file.txt").is_ok());
    }

    // --- parse_range ---

    #[test]
    fn parse_range_simple() {
        assert_eq!(parse_range("bytes=0-99",   200), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-199",200), Some((100, 199)));
    }

    #[test]
    fn parse_range_open_end() {
        assert_eq!(parse_range("bytes=50-", 100), Some((50, 99)));
    }

    #[test]
    fn parse_range_suffix() {
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
    }

    #[test]
    fn parse_range_clamps_to_file_end() {
        assert_eq!(parse_range("bytes=0-999", 100), Some((0, 99)));
    }

    #[test]
    fn parse_range_start_beyond_eof_is_none() {
        assert_eq!(parse_range("bytes=200-300", 100), None);
    }

    #[test]
    fn parse_range_multi_range_unsupported() {
        assert_eq!(parse_range("bytes=0-99,200-299", 400), None);
    }

    // --- HEAD ---

    #[tokio::test]
    async fn head_returns_content_length_and_custom_headers() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);

        let tmp  = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"1234567890").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let req = axum::http::Request::builder()
            .method("HEAD")
            .uri(format!("/fs/read?path={}", pct(&path)))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cl: u64 = resp.headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .expect("Content-Length header");
        assert_eq!(cl, 10);

        assert!(resp.headers().contains_key("X-File-Size"));
        assert!(resp.headers().contains_key("X-File-Mtime"));
        assert!(resp.headers().contains_key("X-File-Is-Dir"));
        assert_eq!(resp.headers().get("Accept-Ranges").and_then(|v| v.to_str().ok()), Some("bytes"));
        assert!(resp.headers().contains_key("ETag"));

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "HEAD response body must be empty");
    }

    // --- GET full + Range ---

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        use tower::ServiceExt;
        let state   = make_test_state();
        let app     = make_test_app(state);
        let tmp     = tempfile::NamedTempFile::new().unwrap();
        let path    = tmp.path().to_string_lossy().into_owned();
        let content = b"hello filesystem api";

        let put = axum::http::Request::builder()
            .method("PUT").uri(format!("/fs/write?path={}", pct(&path)))
            .body(Body::from(content.as_slice())).unwrap();
        assert_eq!(app.clone().oneshot(put).await.unwrap().status(), StatusCode::OK);

        let get = axum::http::Request::builder()
            .uri(format!("/fs/read?path={}", pct(&path)))
            .body(Body::empty()).unwrap();
        let resp = app.oneshot(get).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("Accept-Ranges").and_then(|v| v.to_str().ok()), Some("bytes"));
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], content as &[u8]);
    }

    #[tokio::test]
    async fn range_request_returns_206() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let tmp   = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"0123456789").unwrap();
        let path  = tmp.path().to_string_lossy().into_owned();

        let req = axum::http::Request::builder()
            .uri(format!("/fs/read?path={}", pct(&path)))
            .header(header::RANGE, "bytes=2-5")
            .body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

        let cr = resp.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap();
        assert_eq!(cr, "bytes 2-5/10");

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"2345");
    }

    #[tokio::test]
    async fn range_request_suffix_form() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let tmp   = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"0123456789").unwrap();
        let path  = tmp.path().to_string_lossy().into_owned();

        let req = axum::http::Request::builder()
            .uri(format!("/fs/read?path={}", pct(&path)))
            .header(header::RANGE, "bytes=-3")
            .body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"789");
    }

    #[tokio::test]
    async fn range_beyond_eof_returns_416() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let tmp   = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"short").unwrap();
        let path  = tmp.path().to_string_lossy().into_owned();

        let req = axum::http::Request::builder()
            .uri(format!("/fs/read?path={}", pct(&path)))
            .header(header::RANGE, "bytes=1000-2000")
            .body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    // --- read 404 ---

    #[tokio::test]
    async fn read_nonexistent_is_404() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        #[cfg(unix)]
        let path = "/nonexistent/that/does/not/exist.txt";
        #[cfg(windows)]
        let path = r"C:\nonexistent\that\does\not\exist.txt";
        let req = axum::http::Request::builder()
            .uri(format!("/fs/read?path={}", pct(path)))
            .body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::NOT_FOUND);
    }

    // --- copy / move / delete / mkdir ---

    #[tokio::test]
    async fn copy_file() {
        use tower::ServiceExt;
        let state   = make_test_state();
        let app     = make_test_app(state);
        let src_tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(src_tmp.path(), b"copy me").unwrap();
        let dst_tmp  = tempfile::NamedTempFile::new().unwrap();
        let dst_path = dst_tmp.path().to_string_lossy().into_owned();

        let body = serde_json::json!({ "from": src_tmp.path().to_string_lossy(), "to": dst_path });
        let req  = axum::http::Request::builder()
            .method("POST").uri("/fs/copy")
            .header("content-type","application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
        assert_eq!(std::fs::read(dst_path).unwrap(), b"copy me");
    }

    #[tokio::test]
    async fn copy_stream_returns_sse() {
        use tower::ServiceExt;
        let state   = make_test_state();
        let app     = make_test_app(state);
        let src_tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(src_tmp.path(), b"stream copy").unwrap();
        let dst_tmp  = tempfile::NamedTempFile::new().unwrap();
        let dst_path = dst_tmp.path().to_string_lossy().into_owned();

        let body = serde_json::json!({ "from": src_tmp.path().to_string_lossy(), "to": dst_path });
        let req  = axum::http::Request::builder()
            .method("POST").uri("/fs/copy?stream=true")
            .header("content-type","application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).unwrap_or("");
        assert!(ct.contains("text/event-stream"), "expected SSE content type, got: {ct}");
    }

    #[tokio::test]
    async fn delete_file() {
        use tower::ServiceExt;
        let state  = make_test_state();
        let app    = make_test_app(state);
        let tmp    = tempfile::NamedTempFile::new().unwrap();
        let (_, p) = tmp.keep().unwrap();
        let ps     = p.to_string_lossy().into_owned();
        let req    = axum::http::Request::builder()
            .method("DELETE").uri(format!("/fs/delete?path={}", pct(&ps)))
            .body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
        assert!(!p.exists());
    }

    #[tokio::test]
    async fn delete_stream_returns_sse() {
        use tower::ServiceExt;
        let state  = make_test_state();
        let app    = make_test_app(state);
        let tmp    = tempfile::NamedTempFile::new().unwrap();
        let (_, p) = tmp.keep().unwrap();
        let ps     = p.to_string_lossy().into_owned();
        let req    = axum::http::Request::builder()
            .method("DELETE").uri(format!("/fs/delete?path={}&stream=true", pct(&ps)))
            .body(Body::empty()).unwrap();
        let resp   = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()).unwrap_or("");
        assert!(ct.contains("text/event-stream"), "expected SSE, got: {ct}");
    }

    #[tokio::test]
    async fn mkdir_creates_nested_dirs() {
        use tower::ServiceExt;
        let state   = make_test_state();
        let app     = make_test_app(state);
        let base    = tempfile::TempDir::new().unwrap();
        let new_dir = base.path().join("a").join("b").join("c");
        let ps      = new_dir.to_string_lossy().into_owned();
        let req     = axum::http::Request::builder()
            .method("POST").uri(format!("/fs/mkdir?path={}", pct(&ps)))
            .body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
        assert!(new_dir.is_dir());
    }

    #[tokio::test]
    async fn move_file() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let dir   = tempfile::TempDir::new().unwrap();
        let src   = dir.path().join("src.txt");
        let dst   = dir.path().join("dst.txt");
        std::fs::write(&src, b"move me").unwrap();

        let body = serde_json::json!({ "from": src.to_string_lossy(), "to": dst.to_string_lossy() });
        let req  = axum::http::Request::builder()
            .method("POST").uri("/fs/move")
            .header("content-type","application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"move me");
    }

    // --- drives ---

    #[tokio::test]
    async fn drives_endpoint_returns_200() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let req = axum::http::Request::builder()
            .uri("/fs/drives")
            .body(Body::empty()).unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn drives_response_is_json_array() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let req = axum::http::Request::builder()
            .uri("/fs/drives")
            .body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let drives: Vec<serde_json::Value> = serde_json::from_slice(&body).expect("response is JSON array");
        #[cfg(windows)]
        assert!(!drives.is_empty(), "Windows should enumerate at least one drive");
        #[cfg(not(windows))]
        assert!(drives.is_empty(), "non-Windows stub should return empty array");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn drives_windows_fields_have_expected_shape() {
        use tower::ServiceExt;
        let state = make_test_state();
        let app   = make_test_app(state);
        let req = axum::http::Request::builder()
            .uri("/fs/drives")
            .body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let drives: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();

        for drive in &drives {
            let letter = drive["letter"].as_str().expect("letter field present");
            assert_eq!(letter.len(), 2, "letter should be 'X:' form, got: {letter}");
            assert!(letter.ends_with(':'), "letter should end with colon, got: {letter}");
            assert!(letter.chars().next().unwrap().is_ascii_uppercase(), "drive letter should be uppercase");

            let root = drive["root"].as_str().expect("root field present");
            assert!(root.ends_with('\\'), "root should end with backslash, got: {root}");
            assert_eq!(&root[..2], letter, "root should start with the same letter");

            let kind = drive["kind"].as_str().expect("kind field present");
            assert!(
                matches!(kind, "fixed" | "removable" | "network" | "cdrom" | "ramdisk" | "unknown"),
                "unexpected kind value: {kind}"
            );

            // total_bytes and free_bytes are either both present or both absent
            let has_total = !drive["total_bytes"].is_null();
            let has_free  = !drive["free_bytes"].is_null();
            assert_eq!(has_total, has_free, "total_bytes and free_bytes must both be present or both null");
            if has_total {
                assert!(
                    drive["total_bytes"].as_u64().unwrap() >= drive["free_bytes"].as_u64().unwrap(),
                    "total_bytes must be >= free_bytes"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn list_drives_c_drive_present() {
        let drives = list_drives();
        assert!(
            drives.iter().any(|d| d.letter == "C:"),
            "C: drive should be present on a Windows system"
        );
    }

    #[cfg(windows)]
    #[test]
    fn list_drives_c_drive_has_space_info() {
        let drives = list_drives();
        let c = drives.iter().find(|d| { d.letter == "C:" }).expect("C: must be present");
        assert_eq!(c.kind, "fixed", "C: should be a fixed drive");
        let total = c.total_bytes.expect("C: must report total_bytes");
        let free  = c.free_bytes.expect("C: must report free_bytes");
        assert!(total > 0, "C: total_bytes must be > 0");
        assert!(total >= free, "C: total_bytes must be >= free_bytes");
    }

    #[cfg(windows)]
    #[test]
    fn list_drives_letter_and_root_are_consistent() {
        for drive in list_drives() {
            println!("Drive: letter={}, kind={}, ({}/{})", drive.letter, drive.kind, drive.free_bytes.unwrap_or(0), drive.total_bytes.unwrap_or(0));
            assert!(
                drive.root.starts_with(&drive.letter),
                "root '{}' should start with letter '{}'", drive.root, drive.letter
            );
        }
    }

    fn make_test_state() -> AppState {
        use std::sync::Arc;
        use crate::config::{
            ApiConfig, ArchiveIndexConfig, ArchiveServiceConfig, ContentConfig,
            ExclusionConfig, IndexerConfig, MacroConfig, SearchConfig, StoreConfig,
        };
        use crate::indexer::crawler::IndexMetrics;
        use crate::store::{FileStore, sqlite_store::SqliteStore, tantivy_store::TantivyStore};
        use crate::api::archive::ArchiveRegistry;

        let tmp = tempfile::TempDir::new().unwrap();
        let tantivy_dir = tmp.path().join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();

        let config = crate::config::Config {
            watch_paths:     vec![],
            data_dir:        tmp.path().to_path_buf(),
            log_dir:         tmp.path().to_path_buf(),
            exclusions:      ExclusionConfig::default(),
            content:         ContentConfig::default(),
            api:             ApiConfig::default(),
            search:          SearchConfig::default(),
            indexer:         IndexerConfig::default(),
            archive_index:   ArchiveIndexConfig::default(),
            macros:          MacroConfig::default(),
            archive_service: ArchiveServiceConfig::default(),
            store:           StoreConfig::default(),
        };

        AppState {
            sqlite:           Arc::new(FileStore::Sqlite(Arc::new(
                SqliteStore::new(std::path::Path::new(":memory:")).unwrap(),
            ))),
            tantivy:          Arc::new(TantivyStore::new(&tantivy_dir).unwrap()),
            config:           Arc::new(tokio::sync::RwLock::new(config)),
            metrics:          IndexMetrics::new(),
            reindex_tx:       tokio::sync::watch::channel(false).0,
            archive_registry: Arc::new(ArchiveRegistry::disabled()),
        }
    }
}
