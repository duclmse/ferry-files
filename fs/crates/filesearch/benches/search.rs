//! Criterion benchmarks for the filesearch query engine.
//!
//! Setup: 500 000 synthetic file records are inserted into an in-memory
//! SQLite store once per benchmark group. Each bench case then repeatedly
//! runs a query and measures the per-iteration latency.
//!
//! Run with:
//!   cargo bench -p filesearch
//!
//! Targets (from spec §7.2):
//!   Exact search,  1M files  < 10 ms
//!   Fuzzy search,  1M files  < 50 ms
//!   Regex search,  1M files  < 200 ms
//!   Content search           < 100 ms (Tantivy - not benchmarked here)

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

use filesearch::config::Config;
use filesearch::store::{sqlite_store::SqliteStore, tantivy_store::TantivyStore, FileStore};

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

fn build_runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn minimal_config(watch_dir: &std::path::Path) -> Arc<RwLock<Config>> {
    let dir = watch_dir.to_string_lossy().replace('\\', "/");
    let toml = format!(
        r#"
watch_paths = ["{dir}"]
data_dir    = "{dir}/data"
log_dir     = "{dir}/logs"
"#
    );
    let cfg: Config = toml::from_str(&toml).expect("valid config");
    Arc::new(RwLock::new(cfg))
}

/// Populate SQLite with `count` synthetic records and return the store.
fn setup_sqlite(count: usize) -> Arc<FileStore> {
    let inner =
        Arc::new(SqliteStore::new(std::path::Path::new(":memory:")).expect("sqlite in-memory"));
    inner.populate_for_bench(count).expect("populate");
    Arc::new(FileStore::Sqlite(inner))
}

/// Create an empty Tantivy index in a temp directory.
fn setup_tantivy(dir: &tempfile::TempDir) -> Arc<TantivyStore> {
    Arc::new(TantivyStore::new(dir.path()).expect("tantivy"))
}

// ---------------------------------------------------------------------------
// Benchmark groups
// ---------------------------------------------------------------------------

fn bench_exact_search(c: &mut Criterion) {
    let rt = build_runtime();
    let watch = tempfile::TempDir::new().unwrap();
    let tantivy_dir = tempfile::TempDir::new().unwrap();
    let sqlite = setup_sqlite(500_000);
    let tantivy = setup_tantivy(&tantivy_dir);
    let config = minimal_config(watch.path());

    c.bench_function("exact_search_500k_files", |b| {
        b.to_async(&rt).iter(|| {
            let sq = Arc::clone(&sqlite);
            let tv = Arc::clone(&tantivy);
            let cf = Arc::clone(&config);
            async move {
                let _ = filesearch::query::execute("*.rs", sq, tv, cf).await;
            }
        });
    });
}

fn bench_extension_filter(c: &mut Criterion) {
    let rt = build_runtime();
    let watch = tempfile::TempDir::new().unwrap();
    let tantivy_dir = tempfile::TempDir::new().unwrap();
    let sqlite = setup_sqlite(500_000);
    let tantivy = setup_tantivy(&tantivy_dir);
    let config = minimal_config(watch.path());

    c.bench_function("ext_filter_500k_files", |b| {
        b.to_async(&rt).iter(|| {
            let sq = Arc::clone(&sqlite);
            let tv = Arc::clone(&tantivy);
            let cf = Arc::clone(&config);
            async move {
                let _ = filesearch::query::execute("ext:rs", sq, tv, cf).await;
            }
        });
    });
}

fn bench_fuzzy_search(c: &mut Criterion) {
    let rt = build_runtime();
    let watch = tempfile::TempDir::new().unwrap();
    let tantivy_dir = tempfile::TempDir::new().unwrap();
    let sqlite = setup_sqlite(500_000);
    let tantivy = setup_tantivy(&tantivy_dir);
    let config = minimal_config(watch.path());

    c.bench_function("fuzzy_search_500k_files", |b| {
        b.to_async(&rt).iter(|| {
            let sq = Arc::clone(&sqlite);
            let tv = Arc::clone(&tantivy);
            let cf = Arc::clone(&config);
            async move {
                let _ = filesearch::query::execute("fuzzy:flie", sq, tv, cf).await;
            }
        });
    });
}

fn bench_size_filter(c: &mut Criterion) {
    let rt = build_runtime();
    let watch = tempfile::TempDir::new().unwrap();
    let tantivy_dir = tempfile::TempDir::new().unwrap();
    let sqlite = setup_sqlite(500_000);
    let tantivy = setup_tantivy(&tantivy_dir);
    let config = minimal_config(watch.path());

    c.bench_function("size_filter_500k_files", |b| {
        b.to_async(&rt).iter(|| {
            let sq = Arc::clone(&sqlite);
            let tv = Arc::clone(&tantivy);
            let cf = Arc::clone(&config);
            async move {
                let _ = filesearch::query::execute("size:small", sq, tv, cf).await;
            }
        });
    });
}

fn bench_regex_search(c: &mut Criterion) {
    let rt = build_runtime();
    let watch = tempfile::TempDir::new().unwrap();
    let tantivy_dir = tempfile::TempDir::new().unwrap();
    let sqlite = setup_sqlite(500_000);
    let tantivy = setup_tantivy(&tantivy_dir);
    let config = minimal_config(watch.path());

    // Spec target: regex search < 200 ms on 1 M files (500 K used here).
    c.bench_function("regex_search_500k_files", |b| {
        b.to_async(&rt).iter(|| {
            let sq = Arc::clone(&sqlite);
            let tv = Arc::clone(&tantivy);
            let cf = Arc::clone(&config);
            async move {
                let _ = filesearch::query::execute(r"regex:file_[0-9]+\.rs", sq, tv, cf).await;
            }
        });
    });
}

// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_exact_search,
    bench_extension_filter,
    bench_fuzzy_search,
    bench_size_filter,
    bench_regex_search,
);
criterion_main!(benches);
