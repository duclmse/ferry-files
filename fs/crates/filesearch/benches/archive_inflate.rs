//! Criterion benchmarks for the SIMD inflate pipeline (Phase 13).
//!
//! Run:
//!   cargo bench -p filesearch --bench archive_inflate

use std::io::Write as _;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use filesearch::archive::inflate::{copy_match, copy_simd, RingBuffer, SimdDeflateDecoder};
use filesearch::archive::Decoder;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Compress 1 MB of repeating-pattern data (high match ratio for LZ77).
fn make_compressed_fixture() -> (Vec<u8>, Vec<u8>) {
    let original: Vec<u8> = (0u8..=255).cycle().take(1024 * 1024).collect();
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&original).unwrap();
    let compressed = enc.finish().unwrap();
    (original, compressed)
}

// ---------------------------------------------------------------------------
// inflate throughput: SimdDeflateDecoder end-to-end
// ---------------------------------------------------------------------------

fn bench_inflate_simd(c: &mut Criterion) {
    let (original, compressed) = make_compressed_fixture();
    let mut g = c.benchmark_group("inflate");
    g.throughput(Throughput::Bytes(original.len() as u64));

    g.bench_function("simd_1mb", |b| {
        b.iter(|| {
            let mut dec = SimdDeflateDecoder::new();
            let mut out = Vec::new();
            out.extend_from_slice(&dec.feed(black_box(&compressed)).unwrap());
            out.extend_from_slice(&Box::new(dec).finish().unwrap());
            black_box(out)
        });
    });

    // 64 KB chunks, same total size.
    g.bench_function("simd_1mb_64kb_chunks", |b| {
        b.iter(|| {
            let mut dec = SimdDeflateDecoder::new();
            let mut out = Vec::new();
            for chunk in compressed.chunks(65_536) {
                out.extend_from_slice(&dec.feed(black_box(chunk)).unwrap());
            }
            out.extend_from_slice(&Box::new(dec).finish().unwrap());
            black_box(out)
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// copy_match kernel in isolation
// ---------------------------------------------------------------------------

fn bench_copy_match(c: &mut Criterion) {
    let mut g = c.benchmark_group("inflate_copy_match");

    // Pre-fill ring with 64 KB so slice_at never underflows.
    let mut ring = RingBuffer::new();
    for i in 0u8..=255 {
        for _ in 0..256 {
            ring.write_byte(i);
        }
    }

    // Non-overlapping: dist > len, SIMD path.
    g.throughput(Throughput::Bytes(64));
    g.bench_function("no_overlap_64b", |b| {
        let mut out = Vec::with_capacity(64);
        b.iter(|| {
            out.clear();
            copy_match(&mut out, black_box(&ring), 256, 64);
            black_box(&out);
        });
    });

    // Overlapping expanding run: dist=1 → byte run.
    g.throughput(Throughput::Bytes(256));
    g.bench_function("overlap_run_256b", |b| {
        let mut out = Vec::with_capacity(256);
        b.iter(|| {
            out.clear();
            copy_match(&mut out, black_box(&ring), 1, 256);
            black_box(&out);
        });
    });

    // Large non-overlapping.
    g.throughput(Throughput::Bytes(1024));
    g.bench_function("no_overlap_1kb", |b| {
        let mut out = Vec::with_capacity(1024);
        b.iter(|| {
            out.clear();
            copy_match(&mut out, black_box(&ring), 1024, 1024);
            black_box(&out);
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// copy_simd raw memcpy throughput at various sizes
// ---------------------------------------------------------------------------

fn bench_copy_simd(c: &mut Criterion) {
    let mut g = c.benchmark_group("copy_simd");

    for &size in &[64usize, 256, 4096, 65_536] {
        let src: Vec<u8> = (0u8..=255).cycle().take(size).collect();
        g.throughput(Throughput::Bytes(size as u64));
        g.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            let mut dst = Vec::with_capacity(size);
            b.iter(|| {
                dst.clear();
                copy_simd(&mut dst, black_box(&src), size);
                black_box(&dst);
            });
        });
    }

    g.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(benches, bench_inflate_simd, bench_copy_match, bench_copy_simd,);
criterion_main!(benches);
