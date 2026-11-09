//! Criterion benchmarks for the archive service pipeline.
//!
//! Run:
//!   cargo bench -p fs --bench archive
//!
//! # Performance targets and drive-scale projections
//!
//! ## ZidxBuilder throughput
//!
//! Building a `.zidx` is CPU-bound (zip central-directory parse + tree build +
//! write).  Measured single-threaded on an NVMe SSD:
//!
//!   | Entries   | Expected build time |
//!   |-----------|---------------------|
//!   |     1 000 |  < 5 ms             |
//!   |    10 000 |  < 30 ms            |
//!   |   100 000 |  < 300 ms           |
//!   | 1 000 000 |  < 3 s              |
//!
//! ## LocalFileSource read throughput
//!
//! After the fix (single open + try_clone per read):
//!   Sequential 64 KB chunks: > 1 GB/s on NVMe, > 300 MB/s on SATA SSD
//!
//! ## Drive-scale indexing estimates (full crawl, metadata only)
//!
//! Assumptions:
//!   - Average file density: ~500 000 files per TB (mixed media + docs)
//!   - SQLite WAL insert rate: ~120 000 rows/s (batched, in-memory WAL)
//!   - Filesystem stat rate: ~50 000 files/s (NVMe), ~5 000 files/s (HDD)
//!   - ZidxBuilder for a 1 GB archive (10 000 entries): ~30 ms
//!   - `crawl_threads` / `content_threads` at default (CPU count / 2)
//!
//!   | Drive size | Est. files | Crawl (NVMe) | Crawl (HDD) |
//!   |------------|------------|--------------|-------------|
//!   |       1 TB |    500 000 |        ~10 s |      ~100 s |
//!   |       2 TB |  1 000 000 |        ~20 s |      ~200 s |
//!   |       5 TB |  2 500 000 |        ~50 s |      ~500 s |
//!   |      10 TB |  5 000 000 |       ~100 s |    ~1 000 s |
//!
//! The bottleneck on HDD is directory enumeration (random I/O); on NVMe it is
//! SQLite commit frequency and Tantivy segment merging.  Tuning:
//!   - Increase `commit_interval_secs` to 30+ for large initial crawls.
//!   - Set `content_threads = 0` to skip content indexing during the first pass.
//!   - Use `--release` build; debug builds are 5–10× slower for tree operations.

use std::io::Write as _;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use fs_server::archive::{ByteSource, LocalFileSource};
use fs_server::zidx::{ZidxBuilder, ZidxIndex};

// ---------------------------------------------------------------------------
// ZIP fixture helpers
// ---------------------------------------------------------------------------

/// Build an in-memory ZIP with `n` stored entries ("file_0.txt" … "file_n.txt").
fn make_zip_bytes(n: usize) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut zw = zip::ZipWriter::new(&mut buf);
    let opts =
        zip::write::FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..n {
        zw.start_file(format!("dir_{}/file_{i}.txt", i / 100), opts)
            .unwrap();
        zw.write_all(b"x").unwrap();
    }
    zw.finish().unwrap();
    buf.into_inner()
}

/// Write `n`-entry ZIP to a temp file; return (TempDir, zip_path, zidx_path).
fn fixture(n: usize) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let zip_path = dir.path().join("bench.zip");
    let idx_path = dir.path().join("bench.zidx");
    std::fs::write(&zip_path, make_zip_bytes(n)).unwrap();
    (dir, zip_path, idx_path)
}

// ---------------------------------------------------------------------------
// ZidxBuilder benchmarks
// ---------------------------------------------------------------------------

fn bench_zidx_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("zidx_build");

    for &n in &[1_000usize, 10_000, 100_000] {
        let (_dir, zip_path, idx_path) = fixture(n);

        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                ZidxBuilder::build(&zip_path, &idx_path).unwrap();
            });
        });
    }

    g.finish();
}

// ---------------------------------------------------------------------------
// ZidxIndex lookup benchmarks
// ---------------------------------------------------------------------------

fn bench_zidx_lookup(c: &mut Criterion) {
    let mut g = c.benchmark_group("zidx_lookup");

    for &n in &[1_000usize, 10_000, 100_000] {
        let (_dir, zip_path, idx_path) = fixture(n);
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let idx = ZidxIndex::open(&idx_path).unwrap();

        // Look up a path that exists near the middle of the tree.
        let mid = n / 2;
        let path = format!("dir_{}/file_{mid}.txt", mid / 100);

        g.throughput(Throughput::Elements(1));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let _ = idx.lookup(&path).unwrap();
            });
        });
    }

    g.finish();
}

// ---------------------------------------------------------------------------
// LocalFileSource read throughput
// ---------------------------------------------------------------------------

fn bench_local_file_read(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut g = c.benchmark_group("local_file_read");

    // Write a 4 MB temp file.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let data = vec![0xABu8; 4 * 1024 * 1024];
    std::fs::write(tmp.path(), &data).unwrap();

    let chunk = 64 * 1024usize; // 64 KB - matches pump_inflate chunk size

    g.throughput(Throughput::Bytes(chunk as u64));
    g.bench_function("64kb_chunk_sequential", |b| {
        b.to_async(&rt).iter(|| {
            let src = LocalFileSource::new(tmp.path().to_path_buf()).unwrap();
            async move {
                // Read 8 sequential chunks (= 512 KB) to amortise open cost.
                for i in 0u64..8 {
                    let _ = src.read_range(i * chunk as u64, chunk).await.unwrap();
                }
            }
        });
    });

    // Throughput benchmark - 1 MB read in 64 KB chunks.
    let total = 1024 * 1024usize;
    g.throughput(Throughput::Bytes(total as u64));
    g.bench_function("1mb_sequential", |b| {
        b.to_async(&rt).iter(|| {
            let src = LocalFileSource::new(tmp.path().to_path_buf()).unwrap();
            async move {
                let mut offset = 0u64;
                let end = total as u64;
                while offset < end {
                    let take = chunk.min((end - offset) as usize);
                    let _ = src.read_range(offset, take).await.unwrap();
                    offset += take as u64;
                }
            }
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_zidx_build,
    bench_zidx_lookup,
    bench_local_file_read,
);
criterion_main!(benches);
