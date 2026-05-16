//! Archive Service API — REST handlers and ArchiveRegistry.
//!
//! Routes (mounted only when archive_service.enabled = true):
//!   GET  /archive                   — list all registered archives
//!   GET  /archive/:id/index         — raw .zidx binary
//!   GET  /archive/:id/dir?node=<u32>  — directory listing
//!   GET  /archive/:id/file?node=<u32> — stream file content
//!   GET  /archive/:id/search?q=&limit=&offset= — archive-scoped search

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{ConnectInfo, Path as AxumPath, Query, State},
    http::{header, Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
    Json, Router,
    routing::get,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use crate::api::rest::AppState;
use crate::query::{evaluator, parser};
use crate::query::ast::{Expr, Function, Modifiers, Predicate, PredicateKind};
use crate::archive::{LocalFileSource, StreamExtractor};
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
    /// Weak validator for `ETag:` — derived from file mtime+size at register
    /// time, O(1) stat only.  Stale after rebuild but that also re-registers.
    etag:      String,
}

pub struct ArchiveSummary {
    pub id:       String,
    pub zip_path: PathBuf,
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ArchiveRegistry
// ---------------------------------------------------------------------------

pub struct ArchiveRegistry {
    entries:      DashMap<String, RegistryEntry>,
    /// Per-shard concurrent cache — no global lock on the hot read path.
    /// `index_cache_size` from config is retained in the struct for future
    /// bounded-eviction support but not enforced here; ZidxIndex is mmap-backed
    /// and holds negligible memory beyond the OS page cache.
    index_cache:  DashMap<String, Arc<ZidxIndex>>,
    /// Cached open file handle per archive — avoids an open()+fstat() syscall
    /// on every streaming request.  Evicted (along with index_cache) on rebuild.
    source_cache: DashMap<String, Arc<LocalFileSource>>,
    zidx_dir:     PathBuf,
    enabled:      bool,
}

/// Compute a cheap ETag from file mtime + size — one stat syscall, O(1).
fn zidx_etag(path: &Path) -> String {
    use std::time::UNIX_EPOCH;
    std::fs::metadata(path)
        .ok()
        .and_then(|m| {
            let secs = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            Some(format!("{secs:016x}-{:016x}", m.len()))
        })
        .unwrap_or_else(|| "unknown".into())
}

impl ArchiveRegistry {
    pub fn new(zidx_dir: PathBuf, _cache_size: usize) -> Self {
        Self {
            entries:      DashMap::new(),
            index_cache:  DashMap::new(),
            source_cache: DashMap::new(),
            zidx_dir,
            enabled:      true,
        }
    }

    pub fn disabled() -> Self {
        Self {
            entries:      DashMap::new(),
            index_cache:  DashMap::new(),
            source_cache: DashMap::new(),
            zidx_dir:     PathBuf::new(),
            enabled:      false,
        }
    }

    pub fn is_enabled(&self) -> bool { self.enabled }

    /// Register a ZIP and its .zidx sidecar. Called after ZidxBuilder::build() succeeds.
    pub fn register(&self, zip_path: PathBuf) {
        if !self.enabled { return; }
        let id        = archive_id(&zip_path);
        let zidx_path = zidx_path_for(&self.zidx_dir, &zip_path);
        let etag      = zidx_etag(&zidx_path);
        // Invalidate caches so the next request re-opens the freshly rebuilt file.
        self.index_cache.remove(&id);
        self.source_cache.remove(&id);
        self.entries.insert(id, RegistryEntry { zip_path, zidx_path, etag });
    }

    /// Remove a ZIP from the registry. Called when the ZIP is deleted.
    pub fn unregister(&self, zip_path: &Path) {
        if !self.enabled { return; }
        let id = archive_id(zip_path);
        self.entries.remove(&id);
        self.index_cache.remove(&id);
        self.source_cache.remove(&id);
    }

    /// Open (or return a cached) ZidxIndex for the given archive ID.
    /// Returns None if the ID is unknown or the .zidx file is missing/corrupt.
    pub fn index(&self, id: &str) -> Option<Arc<ZidxIndex>> {
        if !self.enabled { return None; }
        // Fast path: warm cache — DashMap shard lock, no blocking I/O.
        if let Some(idx) = self.index_cache.get(id) {
            return Some(Arc::clone(&*idx));
        }
        // Verify the archive is registered before opening the file.
        let zidx_path = self.entries.get(id)?.zidx_path.clone();
        // Open the file outside any lock.
        match ZidxIndex::open(&zidx_path) {
            Ok(idx) => {
                let idx = Arc::new(idx);
                // or_insert avoids overwriting a concurrent winner.
                let stored = self.index_cache
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::clone(&idx));
                Some(Arc::clone(&*stored))
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

    /// Return a cached (or freshly opened) `LocalFileSource` for `id`.
    /// Returns `None` if the archive is unknown or the file cannot be opened.
    pub fn source(&self, id: &str) -> Option<Arc<LocalFileSource>> {
        if !self.enabled { return None; }
        if let Some(src) = self.source_cache.get(id) {
            return Some(Arc::clone(&*src));
        }
        let zip_path = self.entries.get(id)?.zip_path.clone();
        match LocalFileSource::new(zip_path) {
            Ok(src) => {
                let src     = Arc::new(src);
                let stored  = self.source_cache
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::clone(&src));
                Some(Arc::clone(&*stored))
            }
            Err(e) => {
                tracing::warn!("failed to open source for archive {id}: {e}");
                None
            }
        }
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
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Rate-limit middleware (Step 2)
// ---------------------------------------------------------------------------

async fn rate_limit_middleware(
    State(state): State<AppState>,
    req:          Request<Body>,
    next:         Next,
) -> axum::response::Response {
    let enabled = {
        let cfg = state.config.read().await;
        cfg.archive_service.scheduler.rate_limit_enabled
    };
    if !enabled {
        return next.run(req).await;
    }

    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));

    if state.rate_limiter.check(ip, 1).is_err() {
        state.scheduler_metrics.inc_rejected();
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded")
            .into_response();
    }

    next.run(req).await
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
    req_headers:  axum::http::HeaderMap,
) -> impl IntoResponse {
    let (zidx_path, etag) = match state.archive_registry.entries.get(&id) {
        Some(e) => (e.zidx_path.clone(), e.etag.clone()),
        None    => return err_not_found(&id).into_response(),
    };

    // Conditional GET: skip re-reading the file when client has a fresh copy.
    if req_headers.get(header::IF_NONE_MATCH).map(|v| v.as_bytes()) == Some(etag.as_bytes()) {
        return axum::http::Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap()
            .into_response();
    }

    let file = match tokio::fs::File::open(&zidx_path).await {
        Ok(f)  => f,
        Err(_) => return err_not_found(&id).into_response(),
    };
    let len = match file.metadata().await {
        Ok(m)  => m.len(),
        Err(e) => return err_internal(e.to_string()).into_response(),
    };

    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE,   "application/octet-stream")
        .header(header::CACHE_CONTROL,  "private, max-age=3600")
        .header(header::CONTENT_LENGTH, len)
        .header(header::ETAG,           &etag)
        .body(Body::from_stream(ReaderStream::new(file)))
    {
        Ok(r)  => r.into_response(),
        Err(e) => err_internal(e.to_string()).into_response(),
    }
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
            ("file", Some(child.compressed_size), Some(child.compression))
        };
        entries.push(DirEntry {
            id:   child_id,
            name,
            kind,
            size: child.uncompressed_size,
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

    let node = match idx.node(node_id) {
        Ok(n)  => n,
        Err(_) => return err_bad_node("node out of range", node_id).into_response(),
    };

    if node.kind != 0 {
        return err_bad_node("node is a directory", node_id).into_response();
    }

    let meta = crate::archive::EntryMeta {
        compression:         node.compression,
        compressed_size:     node.compressed_size,
        uncompressed_size:   node.uncompressed_size,
        local_header_offset: node.local_header_offset,
    };
    let source = match state.archive_registry.source(&id) {
        Some(s) => s,
        None    => return err_not_found(&id).into_response(),
    };

    // ── Per-archive concurrency limit (semaphore) ─────────────────────────
    let _permit = state.archive_semaphores.acquire(&id).await;

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

    let (macros, default_limit) = {
        let cfg = state.config.read().await;
        (cfg.macros.clone(), cfg.search.default_limit)
    };

    // Build the archiveof: predicate directly from the PathBuf so spaces and
    // special characters in the path never corrupt the query string.
    let zip_str = zip_path.to_string_lossy().replace('\\', "/");
    let archive_pred = Expr::Predicate(Predicate {
        kind:      PredicateKind::Function(Function::ArchiveOf(zip_str)),
        modifiers: Modifiers::default(),
    });

    let expr = if q.is_empty() {
        archive_pred
    } else {
        match parser::parse(&q, &macros) {
            Ok(user_expr) => Expr::And(Box::new(archive_pred), Box::new(user_expr)),
            Err(e)        => return (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody { error: e.to_string(), id: None, node: None }),
            ).into_response(),
        }
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

    // ----- integration: GET /archive/:id/file via full Axum router ----------

    fn make_test_app_state(dir: &TempDir, registry: Arc<ArchiveRegistry>) -> AppState {
        use crate::config::{
            ApiConfig, ArchiveIndexConfig, ArchiveServiceConfig, ContentConfig,
            ExclusionConfig, IndexerConfig, MacroConfig, SearchConfig, StoreConfig,
        };
        use crate::indexer::crawler::IndexMetrics;
        use crate::store::{FileStore, sqlite_store::SqliteStore, tantivy_store::TantivyStore};

        let tantivy_dir = dir.path().join("tantivy");
        std::fs::create_dir_all(&tantivy_dir).unwrap();

        let config = crate::config::Config {
            watch_paths:     vec![],
            data_dir:        dir.path().to_path_buf(),
            log_dir:         dir.path().to_path_buf(),
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
            sqlite:              Arc::new(FileStore::Sqlite(Arc::new(
                SqliteStore::new(std::path::Path::new(":memory:")).unwrap(),
            ))),
            tantivy:             Arc::new(TantivyStore::new(&tantivy_dir).unwrap()),
            config:              Arc::new(tokio::sync::RwLock::new(config)),
            metrics:             IndexMetrics::new(),
            reindex_tx:          tokio::sync::watch::channel(false).0,
            archive_registry:    registry,
            archive_semaphores:  crate::scheduler::ArchiveSemaphores::new(4),
            scheduler_metrics:   crate::scheduler::SchedulerMetrics::new(),
            rate_limiter:        Arc::new(crate::scheduler::ClientRateLimiter::new(100, 200)),
        }
    }

    #[tokio::test]
    async fn stream_file_handler_body_matches_content() {
        use tower::ServiceExt;

        let dir     = TempDir::new().unwrap();
        let content = b"hello from the HTTP handler";
        let (zip_path, registry) = make_registry_with_zip(&dir, &[("hello.txt", content)]);
        let id      = archive_id(&zip_path);

        let idx     = registry.index(&id).unwrap();
        let node_id = idx.children(0).next().unwrap().unwrap();

        let state = make_test_app_state(&dir, registry);
        // rest::router returns Router<()> which implements Service.
        let app   = crate::api::rest::router(state);

        let request = axum::http::Request::builder()
            .uri(format!("/archive/{id}/file?node={node_id}"))
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], content as &[u8]);
    }

    #[tokio::test]
    async fn stream_file_handler_content_length_matches_uncompressed_size() {
        use tower::ServiceExt;

        let dir     = TempDir::new().unwrap();
        let content = b"content length test ".repeat(20);

        // Deflated so compressed_size != uncompressed_size.
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            let mut zw  = zip::ZipWriter::new(&mut buf);
            let opts    = zip::write::FileOptions::<()>::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("data.bin", opts).unwrap();
            zw.write_all(&content).unwrap();
            zw.finish().unwrap();
            buf.into_inner()
        };

        let zip_path = dir.path().join("cl_test.zip");
        let zidx_dir = dir.path().join("zidx");
        std::fs::create_dir_all(&zidx_dir).unwrap();
        std::fs::write(&zip_path, &zip_bytes).unwrap();
        ZidxBuilder::build(&zip_path, &zidx_path_for(&zidx_dir, &zip_path)).unwrap();
        let registry = Arc::new(ArchiveRegistry::new(zidx_dir, 256));
        registry.register(zip_path.clone());

        let id      = archive_id(&zip_path);
        let idx     = registry.index(&id).unwrap();
        let node_id = idx.children(0).next().unwrap().unwrap();
        let node    = idx.node(node_id).unwrap();

        let state = make_test_app_state(&dir, registry);
        let app   = crate::api::rest::router(state);

        let request = axum::http::Request::builder()
            .uri(format!("/archive/{id}/file?node={node_id}"))
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content_length: u64 = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .expect("Content-Length header must be present");
        assert_eq!(content_length, node.uncompressed_size);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], content.as_slice());
    }

}
