use super::*;
use super::common::validate_path;
#[cfg(windows)]
use super::drives::list_drives;

use axum::{
    body::Body,
    http::{header, StatusCode},
};

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
        sqlite:              Arc::new(FileStore::Sqlite(Arc::new(
            SqliteStore::new(std::path::Path::new(":memory:")).unwrap(),
        ))),
        tantivy:             Arc::new(TantivyStore::new(&tantivy_dir).unwrap()),
        config:              Arc::new(tokio::sync::RwLock::new(config)),
        metrics:             IndexMetrics::new(),
        reindex_tx:          tokio::sync::watch::channel(false).0,
        archive_registry:    Arc::new(ArchiveRegistry::disabled()),
        archive_semaphores:  crate::scheduler::ArchiveSemaphores::new(4),
        scheduler_metrics:   crate::scheduler::SchedulerMetrics::new(),
        rate_limiter:        Arc::new(crate::scheduler::ClientRateLimiter::new(100, 200)),
    }
}
