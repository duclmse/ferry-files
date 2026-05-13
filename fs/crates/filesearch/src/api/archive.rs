//! Archive Service API — REST handlers and ArchiveRegistry.
//!
//! Routes (mounted only when archive_service.enabled = true):
//!   GET  /archive                   — list all registered archives
//!   GET  /archive/:id/index         — raw .zidx binary
//!   GET  /archive/:id/dir?node=<u32>  — directory listing
//!   GET  /archive/:id/file?node=<u32> — stream file content
//!   GET  /archive/:id/search?q=&limit=&offset= — archive-scoped search

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json, Router,
    routing::get,
};
use dashmap::DashMap;
use lru::LruCache;
use serde::{Deserialize, Serialize};

use crate::api::rest::AppState;
use crate::query::{evaluator, parser};
use crate::archive::StreamExtractor;
use crate::zidx::{ZidxIndex, zidx_path_for};

// ---------------------------------------------------------------------------
// Archive ID
// ---------------------------------------------------------------------------

/// Blake3 hex of zip_path string, first 24 chars (96-bit prefix).
/// Stable across restarts; collision-negligible for realistic archive counts.
pub fn archive_id(zip_path: &Path) -> String {
    let key = zip_path.to_string_lossy();
    let hex = blake3::hash(key.as_bytes()).to_hex();
    hex[..24].to_string()
}

// ---------------------------------------------------------------------------
// Registry internals
// ---------------------------------------------------------------------------

struct RegistryEntry {
    zip_path:  PathBuf,
    zidx_path: PathBuf,
}

pub struct ArchiveSummary {
    pub id:       String,
    pub zip_path: PathBuf,
}

// ---------------------------------------------------------------------------
// ArchiveRegistry
// ---------------------------------------------------------------------------

pub struct ArchiveRegistry {
    entries:     DashMap<String, RegistryEntry>,
    index_cache: Mutex<LruCache<String, Arc<ZidxIndex>>>,
    zidx_dir:    PathBuf,
    enabled:     bool,
}

impl ArchiveRegistry {
    pub fn new(zidx_dir: PathBuf, cache_size: usize) -> Self {
        let cap = NonZeroUsize::new(cache_size.max(1)).unwrap();
        Self {
            entries:     DashMap::new(),
            index_cache: Mutex::new(LruCache::new(cap)),
            zidx_dir,
            enabled:     true,
        }
    }

    pub fn disabled() -> Self {
        let cap = NonZeroUsize::new(1).unwrap();
        Self {
            entries:     DashMap::new(),
            index_cache: Mutex::new(LruCache::new(cap)),
            zidx_dir:    PathBuf::new(),
            enabled:     false,
        }
    }

    pub fn is_enabled(&self) -> bool { self.enabled }

    /// Register a ZIP and its .zidx sidecar. Called after ZidxBuilder::build() succeeds.
    pub fn register(&self, zip_path: PathBuf) {
        if !self.enabled { return; }
        let id        = archive_id(&zip_path);
        let zidx_path = zidx_path_for(&self.zidx_dir, &zip_path);
        self.entries.insert(id, RegistryEntry { zip_path, zidx_path });
    }

    /// Remove a ZIP from the registry. Called when the ZIP is deleted.
    pub fn unregister(&self, zip_path: &Path) {
        if !self.enabled { return; }
        let id = archive_id(zip_path);
        self.entries.remove(&id);
        // Let the LRU evict the cached index naturally; stale entries never
        // surface because we check entries before the cache in index().
    }

    /// Open (or return a cached) ZidxIndex for the given archive ID.
    /// Returns None if the ID is unknown or the .zidx file is missing/corrupt.
    pub fn index(&self, id: &str) -> Option<Arc<ZidxIndex>> {
        if !self.enabled { return None; }
        // Check registration first — this is a fast DashMap lookup.
        let zidx_path = {
            let entry = self.entries.get(id)?;
            entry.zidx_path.clone()
        };
        // Return from cache if present.
        {
            let mut cache = self.index_cache.lock().unwrap();
            if let Some(idx) = cache.get(id) {
                return Some(Arc::clone(idx));
            }
        }
        // Cache miss — open the file (may involve a syscall for mmap).
        match ZidxIndex::open(&zidx_path) {
            Ok(idx) => {
                let idx = Arc::new(idx);
                let mut cache = self.index_cache.lock().unwrap();
                cache.put(id.to_string(), Arc::clone(&idx));
                Some(idx)
            }
            Err(e) => {
                tracing::warn!("failed to open .zidx for archive {id}: {e}");
                None
            }
        }
    }

    /// Resolve an archive ID to its ZIP path.
    pub fn zip_path(&self, id: &str) -> Option<PathBuf> {
        if !self.enabled { return None; }
        self.entries.get(id).map(|e| e.zip_path.clone())
    }

    /// List all registered archives.
    pub fn list(&self) -> Vec<ArchiveSummary> {
        if !self.enabled { return vec![]; }
        self.entries
            .iter()
            .map(|e| ArchiveSummary { id: e.key().clone(), zip_path: e.value().zip_path.clone() })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/archive",            get(list_archives))
        .route("/archive/:id/index",  get(get_index))
        .route("/archive/:id/dir",    get(list_dir))
        .route("/archive/:id/file",   get(stream_file))
        .route("/archive/:id/search", get(search_archive))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id:    Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node:  Option<u32>,
}

fn err_not_found(id: &str) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::NOT_FOUND, Json(ErrorBody {
        error: "archive not found".into(),
        id:    Some(id.to_string()),
        node:  None,
    }))
}

fn err_bad_node(msg: impl Into<String>, node: u32) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::BAD_REQUEST, Json(ErrorBody {
        error: msg.into(),
        id:    None,
        node:  Some(node),
    }))
}

fn err_internal(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody {
        error: msg.into(),
        id:    None,
        node:  None,
    }))
}

// ---------------------------------------------------------------------------
// Handler: GET /archive
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ArchiveListItem {
    id:   String,
    path: String,
}

async fn list_archives(State(state): State<AppState>) -> impl IntoResponse {
    let items: Vec<ArchiveListItem> = state.archive_registry
        .list()
        .into_iter()
        .map(|s| ArchiveListItem {
            id:   s.id,
            path: s.zip_path.to_string_lossy().into_owned(),
        })
        .collect();
    Json(items)
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/index
// ---------------------------------------------------------------------------

async fn get_index(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let entry = match state.archive_registry.entries.get(&id) {
        Some(e) => e,
        None    => return err_not_found(&id).into_response(),
    };
    let zidx_path = entry.zidx_path.clone();
    drop(entry);

    let bytes = match tokio::fs::read(&zidx_path).await {
        Ok(b)  => b,
        Err(_) => return err_not_found(&id).into_response(),
    };

    let etag = {
        let hex = blake3::hash(&bytes).to_hex();
        hex[..16].to_string()
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE,  "application/octet-stream"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
            (header::ETAG,          &etag as &str),
        ],
        bytes,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/dir?node=<u32>
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NodeParam {
    node: Option<u32>,
}

#[derive(Serialize)]
struct DirEntry {
    id:   u32,
    name: String,
    kind: &'static str,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    compressed_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compression:     Option<u16>,
}

async fn list_dir(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<NodeParam>,
) -> impl IntoResponse {
    let node_id = params.node.unwrap_or(0);

    let idx = match state.archive_registry.index(&id) {
        Some(i) => i,
        None    => return err_not_found(&id).into_response(),
    };

    let node = match idx.node(node_id) {
        Ok(n)  => n,
        Err(_) => return err_bad_node("node out of range", node_id).into_response(),
    };

    if node.kind != 1 {
        return err_bad_node("node is not a directory", node_id).into_response();
    }

    let mut entries = Vec::new();
    for child_result in idx.children(node_id) {
        let child_id = match child_result {
            Ok(id) => id,
            Err(e) => return err_internal(e.to_string()).into_response(),
        };
        let child = match idx.node(child_id) {
            Ok(n)  => n,
            Err(e) => return err_internal(e.to_string()).into_response(),
        };
        let name = match idx.name(&child) {
            Ok(n)  => n.to_owned(),
            Err(e) => return err_internal(e.to_string()).into_response(),
        };
        let (kind, compressed_size, compression) = if child.kind == 1 {
            ("dir", None, None)
        } else {
            ("file", Some(child.compressed_size as u64), Some(child.compression))
        };
        entries.push(DirEntry {
            id:   child_id,
            name,
            kind,
            size: child.uncompressed_size as u64,
            compressed_size,
            compression,
        });
    }

    Json(entries).into_response()
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/file?node=<u32>
// ---------------------------------------------------------------------------

async fn stream_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<NodeParam>,
) -> impl IntoResponse {
    let node_id = params.node.unwrap_or(0);

    let idx = match state.archive_registry.index(&id) {
        Some(i) => i,
        None    => return err_not_found(&id).into_response(),
    };

    let zip_path = match state.archive_registry.zip_path(&id) {
        Some(p) => p,
        None    => return err_not_found(&id).into_response(),
    };

    let node = match idx.node(node_id) {
        Ok(n)  => n,
        Err(_) => return err_bad_node("node out of range", node_id).into_response(),
    };

    if node.kind != 0 {
        return err_bad_node("node is a directory", node_id).into_response();
    }

    let meta = crate::archive::EntryMeta {
        compression:         node.compression,
        compressed_size:     node.compressed_size as u64,
        uncompressed_size:   node.uncompressed_size as u64,
        local_header_offset: node.local_header_offset,
    };
    let source    = Arc::new(crate::archive::LocalFileSource::new(zip_path));
    let extractor = crate::archive::ZipStreamExtractor { source };
    let stream    = match extractor.stream(&meta) {
        Ok(s)  => s,
        Err(e) => return err_internal(e.to_string()).into_response(),
    };

    match axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, node.uncompressed_size)
        .body(axum::body::Body::from_stream(stream))
    {
        Ok(r)  => r.into_response(),
        Err(e) => err_internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/search?q=&limit=&offset=
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SearchParams {
    q:      Option<String>,
    limit:  Option<usize>,
    offset: Option<usize>,
}

async fn search_archive(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let zip_path = match state.archive_registry.zip_path(&id) {
        Some(p) => p,
        None    => return err_not_found(&id).into_response(),
    };

    let q = params.q.unwrap_or_default();

    // Prepend archiveof: filter so results are scoped to this archive.
    let zip_str = zip_path.to_string_lossy().replace('\\', "/");
    let augmented = format!("archiveof:{zip_str} {q}");

    let (macros, default_limit) = {
        let cfg = state.config.read().await;
        (cfg.macros.clone(), cfg.search.default_limit)
    };

    let expr = match parser::parse(&augmented, &macros) {
        Ok(e)  => e,
        Err(e) => return err_internal(e.to_string()).into_response(),
    };

    let mut plan = match evaluator::plan(&expr, &Default::default()) {
        Ok(p)  => p,
        Err(e) => return err_internal(e.to_string()).into_response(),
    };
    plan.limit  = Some(params.limit.unwrap_or(default_limit));
    plan.offset = params.offset.unwrap_or(0);

    match evaluator::execute_plan(plan, state.sqlite, state.tantivy, state.config).await {
        Ok(result) => {
            use crate::api::rest::SearchApiResponse;
            let hits = result.hits.iter().map(|r| crate::api::rest::SearchHit {
                path:          r.path.clone(),
                name:          r.name.clone(),
                size_bytes:    r.size_bytes,
                mtime:         r.mtime,
                kind:          if r.is_dir { "dir" } else { "file" },
                score:         0.0,
                snippet:       None,
                archive_path:  r.archive_path.clone(),
                archive_entry: crate::api::dto::archive_entry_opt(r),
            }).collect();
            Json(SearchApiResponse {
                result:  hits,
                total:   result.total,
                took_ms: result.took_ms,
                mode:    "exact",
            })
            .into_response()
        }
        Err(e) => err_internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    use crate::zidx::ZidxBuilder;

    fn make_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw  = zip::ZipWriter::new(&mut buf);
        let opts    = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        buf.into_inner()
    }

    fn make_registry_with_zip(dir: &TempDir, entries: &[(&str, &[u8])]) -> (PathBuf, Arc<ArchiveRegistry>) {
        let zip_path  = dir.path().join("test.zip");
        let zidx_dir  = dir.path().join("zidx");
        std::fs::create_dir_all(&zidx_dir).unwrap();
        std::fs::write(&zip_path, make_zip_bytes(entries)).unwrap();
        ZidxBuilder::build(&zip_path, &zidx_path_for(&zidx_dir, &zip_path)).unwrap();
        let registry = Arc::new(ArchiveRegistry::new(zidx_dir.clone(), 256));
        registry.register(zip_path.clone());
        (zip_path, registry)
    }

    #[test]
    fn archive_id_is_deterministic() {
        let p = PathBuf::from("/some/path/test.zip");
        assert_eq!(archive_id(&p), archive_id(&p));
        assert_eq!(archive_id(&p).len(), 24);
    }

    #[test]
    fn registry_register_and_list() {
        let dir      = TempDir::new().unwrap();
        let (zip, r) = make_registry_with_zip(&dir, &[("a.txt", b"hello")]);
        let list     = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].zip_path, zip);
    }

    #[test]
    fn registry_index_returns_zidx() {
        let dir      = TempDir::new().unwrap();
        let (zip, r) = make_registry_with_zip(&dir, &[("a.txt", b"hello")]);
        let id       = archive_id(&zip);
        let idx      = r.index(&id).unwrap();
        assert!(idx.node_count >= 1);
    }

    #[test]
    fn registry_unregister_removes_entry() {
        let dir      = TempDir::new().unwrap();
        let (zip, r) = make_registry_with_zip(&dir, &[("a.txt", b"hello")]);
        r.unregister(&zip);
        assert!(r.list().is_empty());
        let id = archive_id(&zip);
        assert!(r.index(&id).is_none());
    }

    #[test]
    fn registry_unknown_id_returns_none() {
        let dir = TempDir::new().unwrap();
        let r   = Arc::new(ArchiveRegistry::new(dir.path().to_path_buf(), 256));
        assert!(r.index("000000000000000000000000").is_none());
        assert!(r.zip_path("000000000000000000000000").is_none());
    }

    #[test]
    fn disabled_registry_returns_empty() {
        let r = ArchiveRegistry::disabled();
        assert!(!r.is_enabled());
        assert!(r.list().is_empty());
        assert!(r.index("any").is_none());
    }

}
