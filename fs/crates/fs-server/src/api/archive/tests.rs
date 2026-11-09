use super::*;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use crate::zidx::{zidx_path_for, ZidxBuilder};

fn make_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut zw = zip::ZipWriter::new(&mut buf);
    let opts =
        zip::write::FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
    for (name, data) in entries {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap();
    buf.into_inner()
}

fn make_registry_with_zip(
    dir: &TempDir,
    entries: &[(&str, &[u8])],
) -> (PathBuf, Arc<ArchiveRegistry>) {
    let zip_path = dir.path().join("test.zip");
    let zidx_dir = dir.path().join("zidx");
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
    let dir = TempDir::new().unwrap();
    let (zip, r) = make_registry_with_zip(&dir, &[("a.txt", b"hello")]);
    let list = r.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].zip_path, zip);
}

#[test]
fn registry_index_returns_zidx() {
    let dir = TempDir::new().unwrap();
    let (zip, r) = make_registry_with_zip(&dir, &[("a.txt", b"hello")]);
    let id = archive_id(&zip);
    let idx = r.index(&id).unwrap();
    assert!(idx.node_count >= 1);
}

#[test]
fn registry_unregister_removes_entry() {
    let dir = TempDir::new().unwrap();
    let (zip, r) = make_registry_with_zip(&dir, &[("a.txt", b"hello")]);
    r.unregister(&zip);
    assert!(r.list().is_empty());
    let id = archive_id(&zip);
    assert!(r.index(&id).is_none());
}

#[test]
fn registry_unknown_id_returns_none() {
    let dir = TempDir::new().unwrap();
    let r = Arc::new(ArchiveRegistry::new(dir.path().to_path_buf(), 256));
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
        ApiConfig, ArchiveIndexConfig, ArchiveServiceConfig, ContentConfig, ExclusionConfig,
        IndexerConfig, MacroConfig, SearchConfig, StoreConfig,
    };
    use crate::indexer::crawler::IndexMetrics;
    use crate::store::{sqlite_store::SqliteStore, tantivy_store::TantivyStore, FileStore};

    let tantivy_dir = dir.path().join("tantivy");
    std::fs::create_dir_all(&tantivy_dir).unwrap();

    let config = crate::config::Config {
        watch_paths: vec![],
        data_dir: dir.path().to_path_buf(),
        log_dir: dir.path().to_path_buf(),
        exclusions: ExclusionConfig::default(),
        content: ContentConfig::default(),
        api: ApiConfig::default(),
        search: SearchConfig::default(),
        indexer: IndexerConfig::default(),
        archive_index: ArchiveIndexConfig::default(),
        macros: MacroConfig::default(),
        archive_service: ArchiveServiceConfig::default(),
        store: StoreConfig::default(),
    };

    AppState {
        sqlite: Arc::new(FileStore::Sqlite(Arc::new(
            SqliteStore::new(std::path::Path::new(":memory:")).unwrap(),
        ))),
        tantivy: Arc::new(TantivyStore::new(&tantivy_dir).unwrap()),
        config: Arc::new(tokio::sync::RwLock::new(config)),
        metrics: IndexMetrics::new(),
        reindex_tx: tokio::sync::watch::channel(false).0,
        archive_registry: registry,
        archive_semaphores: crate::scheduler::ArchiveSemaphores::new(4),
        scheduler_metrics: crate::scheduler::SchedulerMetrics::new(),
        rate_limiter: Arc::new(crate::scheduler::ClientRateLimiter::new(100, 200)),
    }
}

#[tokio::test]
async fn stream_file_handler_body_matches_content() {
    use tower::ServiceExt;

    let dir = TempDir::new().unwrap();
    let content = b"hello from the HTTP handler";
    let (zip_path, registry) = make_registry_with_zip(&dir, &[("hello.txt", content)]);
    let id = archive_id(&zip_path);

    let idx = registry.index(&id).unwrap();
    let node_id = idx.children(0).next().unwrap().unwrap();

    let state = make_test_app_state(&dir, registry);
    // rest::router returns Router<()> which implements Service.
    let app = crate::api::rest::router(state);

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

    let dir = TempDir::new().unwrap();
    let content = b"content length test ".repeat(20);

    // Deflated so compressed_size != uncompressed_size.
    let zip_bytes = {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::FileOptions::<()>::default()
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

    let id = archive_id(&zip_path);
    let idx = registry.index(&id).unwrap();
    let node_id = idx.children(0).next().unwrap().unwrap();
    let node = idx.node(node_id).unwrap();

    let state = make_test_app_state(&dir, registry);
    let app = crate::api::rest::router(state);

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

#[tokio::test]
async fn stream_file_handler_limits_concurrent_inflight_per_archive() {
    use tower::ServiceExt;

    let dir = TempDir::new().unwrap();
    // Deflated and large enough that inflate takes multiple 64 KB chunks,
    // giving concurrent requests a real window to overlap in-flight.
    let content = b"concurrency guard content ".repeat(20_000);
    let (zip_path, registry) = make_registry_with_zip(&dir, &[("big.bin", &content)]);
    let id = archive_id(&zip_path);

    let idx = registry.index(&id).unwrap();
    let node_id = idx.children(0).next().unwrap().unwrap();

    let mut state = make_test_app_state(&dir, registry);
    state.archive_semaphores = crate::scheduler::ArchiveSemaphores::new(2);
    let metrics = state.scheduler_metrics.clone();
    let app = crate::api::rest::router(state);

    // Sample the `inflight` gauge while the load runs; it must never exceed
    // max_per_archive=2 at any instant.
    let max_seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = tokio::spawn({
        let metrics = metrics.clone();
        let max_seen = max_seen.clone();
        let stop = stop.clone();
        async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let cur = metrics.inflight.load(std::sync::atomic::Ordering::Relaxed);
                max_seen.fetch_max(cur, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_micros(100)).await;
            }
        }
    });

    let mut handles = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let uri = format!("/archive/{id}/file?node={node_id}");
        handles.push(tokio::spawn(async move {
            let request = axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            body
        }));
    }

    for h in handles {
        let body = h.await.unwrap();
        assert_eq!(&body[..], content.as_slice());
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    poller.await.unwrap();

    let seen = max_seen.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        seen >= 1,
        "inflight metric never observed any in-flight request"
    );
    assert!(
        seen <= 2,
        "observed {seen} concurrent inflates, expected <= 2 (max_per_archive)"
    );
}

// ----- Range requests on GET /archive/:id/file ------------------------------

async fn get_file_with_range(
    state: AppState,
    id: &str,
    node: u32,
    range: &str,
) -> (StatusCode, axum::http::HeaderMap, bytes::Bytes) {
    use tower::ServiceExt;
    let app = crate::api::rest::router(state);
    let request = axum::http::Request::builder()
        .uri(format!("/archive/{id}/file?node={node}"))
        .header(header::RANGE, range)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, body)
}

/// Deflated single-entry ZIP registered with a dzx-enabled registry.
fn make_deflated_registry(
    dir: &TempDir,
    content: &[u8],
) -> (PathBuf, PathBuf, Arc<ArchiveRegistry>) {
    let zip_bytes = {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("data.bin", opts).unwrap();
        zw.write_all(content).unwrap();
        zw.finish().unwrap();
        buf.into_inner()
    };
    let zip_path = dir.path().join("deflated.zip");
    let zidx_dir = dir.path().join("zidx");
    let dzx_dir = dir.path().join("dzx");
    std::fs::create_dir_all(&zidx_dir).unwrap();
    std::fs::create_dir_all(&dzx_dir).unwrap();
    std::fs::write(&zip_path, &zip_bytes).unwrap();
    ZidxBuilder::build(&zip_path, &zidx_path_for(&zidx_dir, &zip_path)).unwrap();
    let registry = Arc::new(ArchiveRegistry::new(zidx_dir, 256).with_dzx_dir(dzx_dir.clone()));
    registry.register(zip_path.clone());
    (zip_path, dzx_dir, registry)
}

#[tokio::test]
async fn range_on_stored_entry_returns_206_slice() {
    let dir = TempDir::new().unwrap();
    let content = b"0123456789abcdefghij";
    let (zip_path, registry) = make_registry_with_zip(&dir, &[("s.txt", content)]);
    let id = archive_id(&zip_path);
    let node_id = registry
        .index(&id)
        .unwrap()
        .children(0)
        .next()
        .unwrap()
        .unwrap();

    let state = make_test_app_state(&dir, registry);
    let (status, headers, body) = get_file_with_range(state, &id, node_id, "bytes=5-9").await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 5-9/20");
    assert_eq!(&body[..], b"56789");
}

#[tokio::test]
async fn range_on_deflate_entry_with_dzx_returns_206_slice() {
    let dir = TempDir::new().unwrap();
    let content: Vec<u8> = (0..40_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let (zip_path, dzx_dir, registry) = make_deflated_registry(&dir, &content);
    let id = archive_id(&zip_path);
    let idx = registry.index(&id).unwrap();
    let node_id = idx.children(0).next().unwrap().unwrap();
    let node = idx.node(node_id).unwrap();

    let meta = crate::archive::EntryMeta {
        compression: node.compression,
        compressed_size: node.compressed_size,
        uncompressed_size: node.uncompressed_size,
        local_header_offset: node.local_header_offset,
    };
    let dzx_path = crate::archive::dzx_path_for(&dzx_dir, &zip_path, node.local_header_offset);
    crate::archive::DzxBuilder::new(crate::archive::DEFAULT_BLOCK_SIZE)
        .build(registry.source(&id).unwrap().as_ref(), &meta, &dzx_path)
        .await
        .unwrap();

    let state = make_test_app_state(&dir, registry);
    let (status, headers, body) =
        get_file_with_range(state, &id, node_id, "bytes=30000-30099").await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes 30000-30099/40000");
    assert_eq!(&body[..], &content[30_000..30_100]);
}

#[tokio::test]
async fn range_on_deflate_entry_without_dzx_falls_back_to_full_200() {
    let dir = TempDir::new().unwrap();
    let content = b"no sidecar here ".repeat(100);
    let (zip_path, _dzx_dir, registry) = make_deflated_registry(&dir, &content);
    let id = archive_id(&zip_path);
    let node_id = registry
        .index(&id)
        .unwrap()
        .children(0)
        .next()
        .unwrap()
        .unwrap();

    let state = make_test_app_state(&dir, registry);
    let (status, _headers, body) = get_file_with_range(state, &id, node_id, "bytes=10-19").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], content.as_slice());
}

#[tokio::test]
async fn range_beyond_eof_returns_416() {
    let dir = TempDir::new().unwrap();
    let (zip_path, registry) = make_registry_with_zip(&dir, &[("s.txt", b"short")]);
    let id = archive_id(&zip_path);
    let node_id = registry
        .index(&id)
        .unwrap()
        .children(0)
        .next()
        .unwrap()
        .unwrap();

    let state = make_test_app_state(&dir, registry);
    let (status, headers, _body) = get_file_with_range(state, &id, node_id, "bytes=100-200").await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes */5");
}
