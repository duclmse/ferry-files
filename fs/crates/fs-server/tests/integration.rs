//! Integration tests for fs.
//!
//! Each test starts a real daemon instance (in-process, on dynamic ports) and
//! drives it through the REST API. Tests are deliberately independent: they
//! allocate unique ports and IPC paths via an atomic counter so they can run
//! in parallel without port conflicts.
//!
//! A global semaphore (TEST_GATE) limits concurrent daemons to 1 so that the
//! heavy per-daemon work (Tantivy writer, file-watcher threads, SQLite WAL,
//! REST + gRPC servers) does not overwhelm the machine when all tests fire at once.
//!
//! Run with:
//!   cargo test -p fs --test integration

use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Concurrency gate - one daemon at a time to avoid resource starvation
// ---------------------------------------------------------------------------

fn test_gate() -> Arc<Semaphore> {
    static GATE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(Semaphore::new(1))).clone()
}

// ---------------------------------------------------------------------------
// Port / IPC allocation
// ---------------------------------------------------------------------------

static TEST_ID: AtomicU16 = AtomicU16::new(1);

fn next_id() -> u16 {
    TEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn free_port() -> u16 {
    // Bind to port 0 to ask the OS for a free port, then release.
    TcpListener::bind("127.0.0.1:0")
        .expect("bind port 0")
        .local_addr()
        .unwrap()
        .port()
}

fn ipc_path_for(id: u16) -> String {
    #[cfg(windows)]
    return format!(r"\\.\pipe\fs_test_{id}");
    #[cfg(not(windows))]
    return format!("/tmp/fs_test_{id}.sock");
}

// ---------------------------------------------------------------------------
// Config generator
// ---------------------------------------------------------------------------

fn write_test_config(
    watch_dir: &Path,
    dir: &Path,
    rest_port: u16,
    grpc_port: u16,
    ipc: &str,
) -> std::path::PathBuf {
    let watch = watch_dir.to_string_lossy().replace('\\', "/");
    let data = dir.join("data").to_string_lossy().replace('\\', "/");
    let logs = dir.join("logs").to_string_lossy().replace('\\', "/");

    // IPC path in TOML: wrap in single quotes (literal string) so Windows
    // backslashes are not interpreted as escape sequences.
    let ipc_toml = format!("'{ipc}'");

    let toml = format!(
        r#"
watch_paths = ["{watch}"]
data_dir    = "{data}"
log_dir     = "{logs}"

[api]
rest_port = {rest_port}
grpc_port = {grpc_port}
ipc_path  = {ipc_toml}

[indexer]
commit_interval_secs = 1
crawl_threads        = 2
content_threads      = 2

[content.extension_limits]
rs   = 524288
toml = 65536
txt  = 131072
"*"  = 32768
"#
    );

    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, toml).unwrap();
    config_path
}

// ---------------------------------------------------------------------------
// TestDaemon - starts daemon, waits for /health, shuts down on drop
// ---------------------------------------------------------------------------

struct TestDaemon {
    rest_base: String,
    shutdown: CancellationToken,
    _dir: TempDir,                 // keeps temp dir alive for duration of test
    _permit: OwnedSemaphorePermit, // released on drop, unblocks next test
}

impl TestDaemon {
    async fn start(watch_dir: &Path, dir: TempDir) -> Self {
        // Serialize daemon startups: acquire before spawning threads.
        let _permit = test_gate().acquire_owned().await.unwrap();

        let id = next_id();
        let rest_port = free_port();
        let grpc_port = free_port();
        let ipc = ipc_path_for(id);

        let config_path = write_test_config(watch_dir, dir.path(), rest_port, grpc_port, &ipc);

        let shutdown = CancellationToken::new();
        let shutdown_c = shutdown.clone();
        tokio::spawn(async move {
            let _ = fs_server::daemon::run_with_shutdown(config_path, shutdown_c).await;
        });

        let rest_base = format!("http://127.0.0.1:{rest_port}");
        let client = reqwest::Client::new();

        // Poll /health until the REST server is up (max 30 s).
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                std::time::Instant::now() < deadline,
                "daemon did not start within 30 s"
            );
            if client
                .get(format!("{rest_base}/health"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
        }

        Self {
            rest_base,
            shutdown,
            _dir: dir,
            _permit,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.rest_base)
    }

    async fn stats(&self) -> StatsResponse {
        reqwest::get(self.url("/stats"))
            .await
            .unwrap()
            .json::<StatsResponse>()
            .await
            .unwrap()
    }

    async fn search(&self, query: &str) -> SearchResponse {
        let url = self.url(&format!("/search?q={query}&limit=200"));
        reqwest::get(url)
            .await
            .unwrap()
            .json::<SearchResponse>()
            .await
            .unwrap()
    }

    /// Poll /stats until `predicate` returns true or the timeout elapses.
    async fn wait_until(&self, timeout: Duration, predicate: impl Fn(&StatsResponse) -> bool) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let s = self.stats().await;
            if predicate(&s) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for stats condition"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.shutdown.cancel();
        // _permit is dropped here, releasing the gate for the next test.
    }
}

// ---------------------------------------------------------------------------
// REST response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct StatsResponse {
    total_files: u64,
    #[allow(dead_code)]
    total_dirs: u64,
    content_indexed: u64,
    archive_entries_indexed: u64,
    #[allow(dead_code)]
    archives_crawled: u64,
    events_processed: u64,
    #[allow(dead_code)]
    last_crawl_secs: u64,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    result: Vec<SearchHit>,
    total: usize,
    #[allow(dead_code)]
    took_ms: u64,
    #[allow(dead_code)]
    mode: String,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    path: String,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    archive_path: Option<String>,
    archive_entry: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenBody {
    path: String,
}

#[derive(Debug, Deserialize)]
struct OpenResponse {
    #[allow(dead_code)]
    opened: bool,
    temp_path: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper: create a small ZIP with named entries
// ---------------------------------------------------------------------------

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

/// Full crawl: 10 files created before daemon starts → all appear in index.
#[tokio::test]
async fn test_full_crawl_and_search() {
    let dir = TempDir::new().unwrap();
    let watch_dir = TempDir::new().unwrap();

    // Create 6 .rs and 4 .toml files.
    for i in 0..6 {
        std::fs::write(
            watch_dir.path().join(format!("main{i}.rs")),
            b"fn main() {}",
        )
        .unwrap();
    }
    for i in 0..4 {
        std::fs::write(
            watch_dir.path().join(format!("config{i}.toml")),
            b"key = \"val\"",
        )
        .unwrap();
    }

    let daemon = TestDaemon::start(watch_dir.path(), dir).await;

    // Wait for the crawl to index all 10 files (max 30 s).
    daemon
        .wait_until(Duration::from_secs(30), |s| s.total_files >= 10)
        .await;

    // ── query: *.rs → 6 results ──────────────────────────────────────────
    let resp = daemon.search("*.rs").await;
    assert_eq!(
        resp.result.len(),
        6,
        "expected 6 .rs files, got {}",
        resp.result.len()
    );
    assert!(resp.result.iter().all(|h| h.path.ends_with(".rs")));

    // ── query: *.toml → 4 results ────────────────────────────────────────
    let resp = daemon.search("*.toml").await;
    assert_eq!(resp.result.len(), 4);

    // ── query: dm:today → all 10 files created today ─────────────────────
    let resp = daemon.search("dm:today").await;
    assert!(resp.total >= 10, "dm:today should return all files");

    // ── query: size:tiny → all 10 files are < 10 KB ──────────────────────
    let resp = daemon.search("size:tiny").await;
    assert!(resp.total >= 10);
}

/// Incremental update: new file → appears; deleted → disappears.
#[tokio::test]
async fn test_incremental_update() {
    let dir = TempDir::new().unwrap();
    let watch_dir = TempDir::new().unwrap();

    // Start with an empty watch dir.
    let daemon = TestDaemon::start(watch_dir.path(), dir).await;

    // ── create a file ─────────────────────────────────────────────────────
    let new_file = watch_dir.path().join("new_file.rs");
    let ev0 = daemon.stats().await.events_processed;
    std::fs::write(&new_file, b"fn hello() {}").unwrap();

    // Snapshot-then-wait: capture events_processed before the action, then
    // wait until it increases. This avoids counting errors when one action
    // (e.g. rename) produces more than one watcher event.
    daemon
        .wait_until(Duration::from_secs(10), |s| s.events_processed > ev0)
        .await;

    let resp = daemon.search("new_file.rs").await;
    assert_eq!(resp.result.len(), 1, "new file must appear in search");

    // ── rename the file ───────────────────────────────────────────────────
    let renamed_file = watch_dir.path().join("renamed_file.rs");
    let ev1 = daemon.stats().await.events_processed;
    std::fs::rename(&new_file, &renamed_file).unwrap();

    daemon
        .wait_until(Duration::from_secs(10), |s| s.events_processed > ev1)
        .await;

    let resp = daemon.search("new_file.rs").await;
    assert_eq!(
        resp.result.len(),
        0,
        "old name must not appear after rename"
    );

    let resp = daemon.search("renamed_file.rs").await;
    assert_eq!(resp.result.len(), 1, "new name must appear after rename");

    // ── delete the renamed file ───────────────────────────────────────────
    let ev2 = daemon.stats().await.events_processed;
    std::fs::remove_file(&renamed_file).unwrap();

    daemon
        .wait_until(Duration::from_secs(10), |s| s.events_processed > ev2)
        .await;

    let resp = daemon.search("renamed_file.rs").await;
    assert_eq!(
        resp.result.len(),
        0,
        "deleted file must disappear from search"
    );
}

/// Content search: file containing "TODO: fix this" → returned by content:TODO.
#[tokio::test]
async fn test_content_search() {
    let dir = TempDir::new().unwrap();
    let watch_dir = TempDir::new().unwrap();

    std::fs::write(
        watch_dir.path().join("task.rs"),
        b"// TODO: fix this\nfn foo() {}",
    )
    .unwrap();

    let daemon = TestDaemon::start(watch_dir.path(), dir).await;

    // Wait for content indexing (may take slightly longer than metadata).
    daemon
        .wait_until(Duration::from_secs(30), |s| s.content_indexed >= 1)
        .await;

    // Commit tantivy (commit_interval_secs = 1 in test config, so wait a bit).
    tokio::time::sleep(Duration::from_secs(2)).await;

    let resp = daemon.search("content:TODO").await;
    assert!(
        resp.result.iter().any(|h| h.path.ends_with("task.rs")),
        "task.rs must appear in content:TODO search, got {:?}",
        resp.result.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
}

/// ZIP inner-file indexing: archive entries are searchable.
#[tokio::test]
async fn test_zip_inner_file_search() {
    let dir = TempDir::new().unwrap();
    let watch_dir = TempDir::new().unwrap();

    // Create a ZIP with 3 .rs files and 1 .png.
    // main.rs contains "TODO" so step 6 (archive content search) can find it.
    let zip_bytes = make_zip(&[
        ("src/main.rs", b"// TODO: implement\nfn main() {}"),
        ("src/lib.rs", b"pub mod foo;"),
        ("src/foo.rs", b"pub fn foo() {}"),
        ("assets/img.png", b"\x89PNG\r\n\x1a\n"),
    ]);
    let zip_path = watch_dir.path().join("project.zip");
    std::fs::write(&zip_path, &zip_bytes).unwrap();

    let daemon = TestDaemon::start(watch_dir.path(), dir).await;

    // Wait for the zip + its 4 inner entries to be indexed.
    // total_files = 1 (the zip) + 4 (inner entries)
    daemon
        .wait_until(Duration::from_secs(30), |s| s.archive_entries_indexed >= 3)
        .await;

    // ── archive: *.rs → 3 inner .rs files ───────────────────────────────
    let resp = daemon.search("archive: *.rs").await;
    assert_eq!(
        resp.result.len(),
        3,
        "expected 3 inner .rs entries, got {} - hits: {:?}",
        resp.result.len(),
        resp.result.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    assert!(
        resp.result.iter().all(|h| h.archive_entry.is_some()),
        "all hits must have archive_entry set"
    );

    // ── noarchive: *.rs → 0 (no real .rs files on disk) ─────────────────
    let resp = daemon.search("noarchive: *.rs").await;
    assert_eq!(
        resp.result.len(),
        0,
        "noarchive: *.rs must return 0 results"
    );

    // ── step 6: archive: content:TODO → main.rs inner entry ──────────────
    daemon
        .wait_until(Duration::from_secs(15), |s| s.content_indexed >= 1)
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let resp = daemon.search("archive: content:TODO").await;
    assert!(
        resp.result.iter().any(|h| h.path.ends_with("main.rs")),
        "archive content:TODO must find main.rs inside ZIP, got: {:?}",
        resp.result.iter().map(|h| &h.path).collect::<Vec<_>>()
    );

    // ── step 7: POST /open on inner ZIP entry ────────────────────────────
    let inner_hit = daemon
        .search("archive: *.rs")
        .await
        .result
        .into_iter()
        .find(|h| h.path.ends_with("main.rs"))
        .expect("main.rs inner entry must exist");
    let client = reqwest::Client::new();
    let open_resp = client
        .post(daemon.url("/open"))
        .json(&OpenBody {
            path: inner_hit.path.clone(),
        })
        .send()
        .await
        .unwrap();
    assert!(
        open_resp.status().is_success(),
        "POST /open returned HTTP {} for inner entry {:?}",
        open_resp.status(),
        inner_hit.path
    );
    let open_body = open_resp.json::<OpenResponse>().await.unwrap();
    assert!(
        open_body.temp_path.is_some(),
        "temp_path must be set when opening an inner-archive entry"
    );

    // ── step 8: delete ZIP → inner entries disappear ─────────────────────
    let events_before = daemon.stats().await.events_processed;
    std::fs::remove_file(&zip_path).unwrap();
    daemon
        .wait_until(Duration::from_secs(10), |s| {
            s.events_processed > events_before
        })
        .await;
    // Give the indexer a moment to process the removal of inner entries.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let resp = daemon.search("archive: *.rs").await;
    assert_eq!(
        resp.result.len(),
        0,
        "after ZIP deletion all inner entries must be removed from search"
    );
}

/// Query acceptance: all spec §5 query forms parse without error.
#[tokio::test]
async fn test_everything_query_acceptance() {
    let dir = TempDir::new().unwrap();
    let watch_dir = TempDir::new().unwrap();

    let daemon = TestDaemon::start(watch_dir.path(), dir).await;
    // Wait for crawl (empty dir - very fast).
    daemon
        .wait_until(Duration::from_secs(10), |s| s.total_files == 0)
        .await;

    // These must all return HTTP 200 (no parse error), even if result is empty.
    let queries = [
        "*.rs",
        "ext:rs",
        "dm:today",
        "dm:thisweek",
        "dm:last7days",
        "size:tiny",
        "size:small",
        "size:>1mb",
        "size:1kb..10mb",
        "dir:src",
        "fuzzy:main",
        "archive: *.rs",
        "noarchive: *.toml",
        "type:audio",
        "type:document",
        "main.rs AND ext:rs",
        "*.rs OR *.toml",
        "NOT hidden: *.rs",
    ];

    for q in queries {
        let url = daemon.url(&format!("/search?q={q}&limit=10"));
        let status = reqwest::get(&url).await.unwrap().status();
        assert!(status.is_success(), "query {q:?} returned HTTP {status}");
    }
}
