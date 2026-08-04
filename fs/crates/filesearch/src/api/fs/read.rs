use std::path::Path;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use tokio_util::io::ReaderStream;

use crate::api::rest::AppState;

use super::common::{
    bad_request, extract_meta, file_etag, internal, mime_for_path, not_found, parse_range,
    validate_path, DirEntry, PathParam,
};

// ---------------------------------------------------------------------------
// HEAD /fs/read?path= - file metadata as headers, empty body
// ---------------------------------------------------------------------------

pub(super) async fn head_handler(
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
// GET /fs/read?path= - stream file; honours Range header
// ---------------------------------------------------------------------------

pub(super) async fn read_handler(
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
// Directory listing helper - called by read_handler when path is a dir
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
