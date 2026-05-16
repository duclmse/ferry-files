//! Criterion benchmarks for streaming ZIP extraction throughput (Phase 13).
//!
//! Measures end-to-end: open LocalFileSource, create SimdDeflateDecoder, feed
//! compressed bytes in 64 KB chunks, collect decompressed output.
//!
//! Run:
//!   cargo bench -p filesearch --bench archive_range

use std::io::Write as _;
use std::path::PathBuf;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::TempDir;

use filesearch::archive::inflate::SimdDeflateDecoder;
use filesearch::archive::{Decoder, LocalFileSource, ByteSource};

// ---------------------------------------------------------------------------
// Fixture: write a deflate-compressed 1 MB blob to disk
// ---------------------------------------------------------------------------

fn make_deflated_file() -> (TempDir, PathBuf, Vec<u8>, u64) {
    let original: Vec<u8> = (0u8..=255).cycle().take(1024 * 1024).collect();
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&original).unwrap();
    let compressed = enc.finish().unwrap();

    let dir  = TempDir::new().unwrap();
    let path = dir.path().join("payload.deflate");
    std::fs::write(&path, &compressed).unwrap();
    let file_len = compressed.len() as u64;
    (dir, path, original, file_len)
}

// ---------------------------------------------------------------------------
// Benchmark: read-all via LocalFileSource + SimdDeflateDecoder
// ---------------------------------------------------------------------------

fn bench_range_read(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let (_dir, path, original, file_len) = make_deflated_file();
    let mut g = c.benchmark_group("range_read");
    g.throughput(Throughput::Bytes(original.len() as u64));

    g.bench_function("1mb_deflate_sequential", |b| {
        b.to_async(&rt).iter(|| {
            let p = path.clone();
            async move {
                let src    = LocalFileSource::new(p).unwrap();
                let chunk  = 65_536usize;
                let mut dec = SimdDeflateDecoder::new();
                let mut out = Vec::new();
                let mut offset = 0u64;
                while offset < file_len {
                    let take = chunk.min((file_len - offset) as usize);
                    let bytes = src.read_range(offset, take).await.unwrap();
                    out.extend_from_slice(&dec.feed(black_box(&bytes)).unwrap());
                    offset += take as u64;
                }
                out.extend_from_slice(&Box::new(dec).finish().unwrap());
                black_box(out)
            }
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: partial-range read of a large compressed payload
//
// Simulates the "find block + partial inflate" pattern that DZX enables.
// This benchmark uses the mid-point of the compressed stream, decompressiong
// a 64 KB window, which represents the hot path for range requests.
// ---------------------------------------------------------------------------

fn bench_partial_inflate(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let (_dir, path, _original, file_len) = make_deflated_file();
    let mut g = c.benchmark_group("range_read");
    g.throughput(Throughput::Bytes(65_536));

    // Read a 64 KB window from the start (simulates sequential range access).
    g.bench_function("64kb_window_from_start", |b| {
        b.to_async(&rt).iter(|| {
            let p = path.clone();
            async move {
                let src   = LocalFileSource::new(p).unwrap();
                let take  = 65_536usize.min(file_len as usize);
                let bytes = src.read_range(0, take).await.unwrap();
                let mut dec = SimdDeflateDecoder::new();
                let out = dec.feed(black_box(&bytes)).unwrap();
                black_box(out)
            }
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(benches, bench_range_read, bench_partial_inflate,);
criterion_main!(benches);
