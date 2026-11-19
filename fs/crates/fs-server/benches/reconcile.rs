//! Full-crawl reconciliation over 1M rows (docs/plans/performance-memory-update.md).
//!
//! Setup writes 1,000,000 synthetic rows for one watch root into an on-disk
//! SQLite store, as a first crawl would. Each iteration is a later crawl
//! that finds all but 1% of them again: a new generation, every surviving
//! row re-upserted in 10,000-row batches, then the stale sweep. The skipped
//! 1% shifts every iteration, so each sweep deletes ~10,000 rows and each
//! crawl brings back the previous iteration's.
//!
//! Peak resident memory for the whole run is printed at the end on Unix;
//! reconciliation must not grow with the row count, since stale rows are
//! found in SQL rather than by listing paths in memory.
//!
//!   cargo bench -p fs-server --bench reconcile

use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{criterion_group, Criterion};

use fs_server::store::{sqlite_store::SqliteStore, FileRecord};

const ROWS: usize = 1_000_000;
const BATCH: usize = 10_000;
const ROOT: &str = "/bench";

fn record(i: usize) -> FileRecord {
    let dir = i % 10_000;
    let name = format!("file_{i}.rs");
    FileRecord {
        id: None,
        path: format!("{ROOT}/dir_{dir}/{name}"),
        name,
        extension: Some("rs".into()),
        size_bytes: (i % 1_000) as u64 * 512,
        mtime: 1_700_000_000,
        ctime: 1_700_000_000,
        is_dir: false,
        is_hidden: false,
        is_symlink: false,
        attrib_bitmask: 0,
        content_indexed: false,
        tantivy_id: None,
        indexed_at: 1_700_000_000,
        archive_path: None,
    }
}

fn crawl(store: &SqliteStore, skip: Option<usize>) -> usize {
    let generation = store.begin_crawl_generation().expect("generation");
    let mut batch = Vec::with_capacity(BATCH);
    for i in (0..ROWS).filter(|i| skip != Some(i % 100)) {
        batch.push((record(i), ROOT.to_owned()));
        if batch.len() == BATCH {
            store
                .batch_upsert_crawl(&batch, generation)
                .expect("upsert");
            batch.clear();
        }
    }
    store
        .batch_upsert_crawl(&batch, generation)
        .expect("upsert");
    store
        .remove_stale_for_root(ROOT, generation)
        .expect("sweep")
}

fn bench_reconcile(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteStore::new(&dir.path().join("files.db")).expect("store");
    crawl(&store, None);

    let round = AtomicUsize::new(0);
    let mut group = c.benchmark_group("reconcile");
    group.sample_size(10);
    group.bench_function("recrawl_1m_rows_1pct_gone", |b| {
        b.iter(|| {
            let skip = round.fetch_add(1, Ordering::Relaxed) % 100;
            crawl(&store, Some(skip))
        })
    });
    group.finish();
}

criterion_group!(benches, bench_reconcile);

fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();
    #[cfg(unix)]
    if let Ok(usage) = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_SELF) {
        // ru_maxrss is in bytes on macOS and in KiB on Linux.
        let kib = if cfg!(target_os = "macos") {
            usage.max_rss() / 1024
        } else {
            usage.max_rss()
        };
        // Bench output, not a daemon path: no tracing subscriber runs here.
        eprintln!("peak RSS: {} MiB", kib / 1024);
    }
}
