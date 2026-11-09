use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::{http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::api::rest::AppState;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(super) struct OkResponse {
    pub(super) ok: bool,
}

#[derive(Serialize)]
pub(super) struct ErrorBody {
    error: String,
}

/// One entry returned by `GET /fs/read` when the path is a directory.
#[derive(Serialize)]
pub(super) struct DirEntry {
    pub(super) name: String,
    pub(super) path: String,
    pub(super) is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) size: Option<u64>,
    pub(super) mtime_secs: u64,
}

/// Progress event emitted over SSE for long-running operations.
#[derive(Serialize, Clone)]
pub struct ProgressEvent {
    pub processed: usize,
    pub total: usize,
    pub current: String,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct PathParam {
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) stream: bool,
}

#[derive(Deserialize)]
pub(super) struct StreamParam {
    #[serde(default)]
    pub(super) stream: bool,
}

pub(super) fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorBody { error: msg.into() }),
    )
}

pub(super) fn not_found(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::NOT_FOUND, Json(ErrorBody { error: msg.into() }))
}

pub(super) fn internal(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody { error: msg.into() }),
    )
}

// ---------------------------------------------------------------------------
// Path validation
// ---------------------------------------------------------------------------

/// Reject empty, relative, and `..`-containing paths.
pub(super) fn validate_path(raw: &str) -> Result<PathBuf, String> {
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
// Protected paths
// ---------------------------------------------------------------------------

/// Refuses changes that could plant code the daemon would later execute.
///
/// The extensions folder and the config that names it stay off limits; file
/// permissions cannot do this, since the API runs as the daemon's own user.
pub(super) async fn ensure_unprotected(
    state: &AppState,
    targets: &[&Path],
) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    let protected: Vec<PathBuf> = {
        let cfg = state.config.read().await;
        match &cfg.extensions_dir {
            Some(dir) => std::iter::once(dir.clone())
                .chain(cfg.source_path.clone())
                .collect(),
            None => return Ok(()),
        }
    };
    let targets: Vec<PathBuf> = targets.iter().map(|p| p.to_path_buf()).collect();
    let blocked = tokio::task::spawn_blocking(move || {
        let protected: Vec<PathBuf> = protected.iter().map(|p| resolve(p)).collect();
        targets
            .into_iter()
            .find(|t| overlaps(&resolve(t), &protected))
    })
    .await
    .map_err(|e| internal(e.to_string()))?;
    match blocked {
        Some(path) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorBody {
                error: format!("path is protected: {}", path.display()),
            }),
        )),
        None => Ok(()),
    }
}

// Resolves symlinks through the deepest part that exists, because the final
// components of a write target usually do not exist yet.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return missing.iter().rev().fold(canonical, |p, c| p.join(c));
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

// Ancestors count too: moving or deleting a parent removes the protected path.
fn overlaps(target: &Path, protected: &[PathBuf]) -> bool {
    protected
        .iter()
        .any(|p| target.starts_with(p) || p.starts_with(target))
}

// ---------------------------------------------------------------------------
// Helpers - mime, etag, range
// ---------------------------------------------------------------------------

pub(super) fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some(
            "txt" | "md" | "rs" | "toml" | "yaml" | "yml" | "json" | "xml" | "csv" | "log" | "ini"
            | "cfg" | "conf" | "sh" | "ps1" | "py" | "js" | "ts" | "html" | "css" | "c" | "cpp"
            | "h" | "go",
        ) => "text/plain; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

pub(super) fn file_etag(mtime_secs: u64, size: u64) -> String {
    format!("\"{mtime_secs:016x}-{size:016x}\"")
}

/// Parse a single `bytes=start-end` range.  Returns `(start, end)` (both
/// inclusive) or `None` when the range string is malformed or unsatisfiable
/// given `file_size`.  Multi-range (comma-separated) is not supported.
pub(crate) fn parse_range(range: &str, file_size: u64) -> Option<(u64, u64)> {
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
        if start > end {
            return None;
        }
        Some((start, end))
    }
}

// ---------------------------------------------------------------------------
// Helpers - metadata extraction
// ---------------------------------------------------------------------------

pub(super) struct FileMeta {
    pub(super) size: u64,
    pub(super) mtime_secs: u64,
    pub(super) is_dir: bool,
    pub(super) mode_str: String,
}

pub(super) fn extract_meta(meta: &std::fs::Metadata) -> FileMeta {
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

    FileMeta {
        size: meta.len(),
        mtime_secs,
        is_dir: meta.is_dir(),
        mode_str,
    }
}
