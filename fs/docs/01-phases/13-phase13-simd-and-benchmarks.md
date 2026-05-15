# Phase 13 — SIMD Inflate Pipeline & Benchmarks

**Goal:** Replace the scalar `flate2` inflate path in the streaming extractor
with a SIMD-accelerated match-copy engine, and establish a criterion benchmark
suite that measures each pipeline stage independently. The benchmark suite also
provides regression protection for all archive service throughput targets.

**Estimated scope:** ~600 lines of Rust + benchmarks.

**Depends on:** Phase 10 (streaming ZIP core, `Decoder` trait), Phase 11 (`.dzx`
block reader), Phase 12 (scheduler).

**Design source:** `docs/00/simd and benchmark.md`.

---

## 13.1 Where SIMD Helps (and Where It Doesn't)

| Stage                        | SIMD impact   | Reason                              |
| ---------------------------- | ------------- | ----------------------------------- |
| Huffman decode               | Low           | Bit-level, branchy, data-dependent  |
| LZ77 match copy              | **Very high** | Wide stores on long back-references |
| Decoder control flow         | None          | Serial by nature                    |
| Output formatting / chunking | Medium        | Memcpy-class operations             |

The strategy: **scalar Huffman decode → SIMD match-copy engine**. Touching
Huffman decoding is not worth the complexity. The match-copy loop is where CPU
time actually goes for typical data.

---

## 13.2 Module Layout

```
crates/filesearch/src/archive/
├── ...             ← existing (Phases 10–11)
└── inflate.rs      ← SIMD copy engine, ring buffer, fast Huffman table
```

```
crates/filesearch/benches/
├── archive_inflate.rs   ← criterion: inflate throughput
├── archive_range.rs     ← criterion: random-access read latency
└── scheduler_load.rs    ← criterion: scheduler overhead
```

---

## 13.3 SIMD Copy Engine

### Ring Buffer

The LZ77 sliding window is maintained as a double-buffer to avoid modulo on
every access:

```rust
const WINDOW: usize = 32 * 1024;

pub struct RingBuffer {
    buf: Box<[u8; WINDOW * 2]>,
    pos: usize,
}

impl RingBuffer {
    pub fn write_byte(&mut self, b: u8) {
        self.buf[self.pos] = b;
        self.buf[self.pos + WINDOW] = b;  // mirror
        self.pos = (self.pos + 1) & (WINDOW - 1);
    }

    pub fn slice_at(&self, back: usize, len: usize) -> &[u8] {
        let start = (self.pos + WINDOW - back) & (WINDOW - 1);
        &self.buf[start..start + len]
    }
}
```

The double-buffer trick (writing each byte at `pos` and `pos + WINDOW`) means
`slice_at` always returns a contiguous slice — no wrap-around handling.

### Overlap-safe SIMD Copy

LZ77 allows `dist < len` (expanding runs like `aaaa...`). Handle with stride:

```rust
pub fn copy_match(dst: &mut Vec<u8>, ring: &RingBuffer, dist: usize, len: usize) {
    if dist >= len {
        // no overlap: SIMD bulk copy
        let src = ring.slice_at(dist, len);
        copy_simd(dst, src, len);
    } else {
        // overlapping run: copy `dist` bytes at a time
        let mut copied = 0;
        while copied < len {
            let chunk = (len - copied).min(dist);
            let src = ring.slice_at(dist, chunk);
            copy_simd(dst, src, chunk);
            copied += chunk;
        }
    }
}
```

### `copy_simd` Implementation

```rust
#[cfg(target_arch = "x86_64")]
unsafe fn copy_simd_avx2(dst: *mut u8, src: *const u8, len: usize) {
    use std::arch::x86_64::*;
    let mut i = 0;
    while i + 32 <= len {
        let v = _mm256_loadu_si256(src.add(i) as *const __m256i);
        _mm256_storeu_si256(dst.add(i) as *mut __m256i, v);
        i += 32;
    }
    // scalar tail
    while i < len {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
}

pub fn copy_simd(dst: &mut Vec<u8>, src: &[u8], len: usize) {
    let start = dst.len();
    dst.resize(start + len, 0);
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe { copy_simd_avx2(dst[start..].as_mut_ptr(), src.as_ptr(), len); }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    dst[start..].copy_from_slice(&src[..len]);
}
```

Use `#[cfg(target_feature = "avx2")]` for compile-time dispatch; runtime
detection is not required because release builds on the target hardware will
have AVX2 available. Document the required CPU feature in `VERSIONS.md`.

### Fast Huffman Table

9-bit lookup for common symbols (codes ≤ 9 bits cover > 99% of typical data):

```rust
pub struct HuffmanTable {
    fast: Box<[u16; 512]>,  // 2^9 entries; value = (symbol << 4) | code_len
}

impl HuffmanTable {
    pub fn decode(&self, bits: u32) -> Option<(u16, u8)> {
        let entry = self.fast[(bits & 0x1FF) as usize];
        if entry == 0 { return None; }  // long code, fall back to canonical
        Some(((entry >> 4), (entry & 0xF) as u8))
    }
}
```

The fallback canonical decode is the existing `flate2` path — only triggered for
the rare long-code symbols.

---

## 13.4 Plugging Into the `Decoder` Trait

Replace `DeflateDecoder` (Phase 10) with `SimdDeflateDecoder`:

```rust
pub struct SimdDeflateDecoder {
    ring:    RingBuffer,
    table:   HuffmanTable,
    // flate2 Decompress for bit-stream parsing (Huffman + lengths/distances)
    inner:   flate2::Decompress,
    out_buf: Vec<u8>,
}

impl Decoder for SimdDeflateDecoder {
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes> {
        // 1. flate2::Decompress parses tokens from the bitstream
        // 2. For each Literal token: ring.write_byte(b); out_buf.push(b)
        // 3. For each Match (len, dist): copy_match(&mut out_buf, &ring, dist, len)
        //    and update ring for each copied byte
        // 4. Return Bytes::from(out_buf.drain(..).collect::<Vec<_>>())
        todo!("integrate token loop with flate2 low-level API")
    }

    fn finish(self: Box<Self>) -> io::Result<Bytes> {
        Ok(Bytes::from(self.out_buf))
    }
}
```

> **Implementation note:** `flate2::Decompress` does not expose individual token
> events. To intercept match-copy operations, one of these approaches is needed:
> (a) Use a lower-level zlib/miniz binding that exposes token-level output; or
> (b) Use `flate2::Decompress::decompress_vec()` in 64-byte chunks and apply
> SIMD only to copy segments detected by comparing output positions across
> calls; or (c) Link `libdeflate` which provides a callback-based API.
>
> For Phase 13, **approach (b)** is recommended: decompress into a temporary
> Vec, then re-scan the output for long runs and copy them into the ring buffer
> with SIMD. This is less optimal than true token interception but avoids
> non-trivial FFI and can be upgraded later. The benchmark (§13.5) will reveal
> whether the gain justifies approach (c).

---

## 13.5 Benchmark Suite

Located in `crates/filesearch/benches/`. Run with:

```sh
cargo bench -p filesearch
```

### `archive_inflate.rs`

```rust
fn bench_inflate_scalar(c: &mut Criterion) {
    let data = load_compressed_fixture();   // pre-compressed 1 MB payload
    c.bench_function("inflate/scalar", |b| {
        b.iter(|| inflate_scalar(&data))
    });
}

fn bench_inflate_simd(c: &mut Criterion) {
    let data = load_compressed_fixture();
    c.bench_function("inflate/simd", |b| {
        b.iter(|| inflate_simd(&data))
    });
}

fn bench_inflate_match_copy(c: &mut Criterion) {
    // Benchmark only the copy_match kernel in isolation
    let mut ring = RingBuffer::default();
    let mut out = Vec::with_capacity(65536);
    c.bench_function("inflate/copy_match/no_overlap", |b| {
        b.iter(|| {
            out.clear();
            copy_match(&mut out, &ring, 256, 64);
        })
    });
}
```

### `archive_range.rs`

```rust
fn bench_range_read_cold(c: &mut Criterion) {
    // Measures: dzx.find_block + read_range (disk I/O + partial inflate)
    let (source, meta, dzx) = setup_fixture();
    c.bench_function("range_read/1MB_entry/offset_512KB", |b| {
        b.to_async(tokio_rt()).iter(|| {
            read_range(&source, &meta, &dzx, 512 * 1024, 4096)
        })
    });
}
```

### `scheduler_load.rs`

```rust
fn bench_scheduler_dispatch(c: &mut Criterion) {
    let mut sched = ArchiveScheduler::new(4);
    // pre-fill with 1000 tasks across 10 clients × 10 archives
    c.bench_function("scheduler/dispatch_1000_tasks", |b| {
        b.iter(|| {
            while let Some(task) = sched.dispatch(now_ns()) {
                black_box(task);
            }
        })
    });
}
```

---

## 13.6 Stage Instrumentation

For non-criterion production profiling, add atomic counters to
`SimdDeflateDecoder`:

```rust
thread_local! {
    static INFLATE_STATS: RefCell<InflateStats> = RefCell::new(InflateStats::default());
}

#[derive(Default)]
pub struct InflateStats {
    pub io_ns:     u64,
    pub decode_ns: u64,
    pub copy_ns:   u64,
    pub bytes_out: u64,
}
```

Expose cumulative stats via `GET /stats` when `FILESEARCH_INFLATE_STATS=1` env
var is set. Do not add overhead in production builds without this env var.

---

## 13.7 KPI Targets

| Metric                                          | Target                                       |
| ----------------------------------------------- | -------------------------------------------- |
| Inflate throughput (SIMD, per core)             | ≥ 400 MB/s                                   |
| Inflate throughput (scalar baseline)            | ≥ 200 MB/s                                   |
| SIMD speedup over scalar                        | ≥ 1.5× (realistic for match-copy-heavy data) |
| Range read latency (p95, local SSD, 1 MB entry) | < 5 ms                                       |
| Scheduler dispatch overhead                     | < 5% of total CPU                            |
| Scheduler dispatch (1000 tasks)                 | < 100 µs                                     |

---

## 13.8 Tests — Phase 13

```
SIMD copy engine:
- copy_simd: output == input for various lengths (1, 31, 32, 33, 128, 255)
- copy_match, dist > len (no overlap): output == naive copy
- copy_match, dist < len (overlapping run): output == naive expanding copy
- copy_match, dist == 1: run of repeated bytes (e.g. aaaa...)

ring buffer:
- write 32KB bytes, slice_at(32KB, 64) wraps correctly
- slice_at returns contiguous slice (no panic on boundary)

fast Huffman table:
- decode(bits) for each 9-bit code in DEFLATE fixed table → correct symbol
- decode for 10-bit code → None (fallback triggered)

SimdDeflateDecoder:
- feed known-deflated bytes → output matches reference inflate (flate2)
- feed in 1-byte chunks → same output as single-chunk feed
- large input (1 MB): output size == expected uncompressed size

benchmarks (smoke test only, not perf assertion):
- bench_inflate_scalar completes without panic
- bench_inflate_simd completes without panic
- bench_range_read_cold: single iteration completes in < 1 s (sanity check)
```

---

## 13.9 Definition of Done

- [ ] `cargo test -p filesearch archive::inflate` passes all SIMD copy and
      ring-buffer tests.
- [ ] `SimdDeflateDecoder::feed` produces byte-identical output to `flate2`
      scalar decoder on the same input (golden-file test).
- [ ] `cargo bench -p filesearch` runs without errors.
- [ ] SIMD inflate throughput ≥ 400 MB/s on target hardware (measured with
      criterion).
- [ ] Scheduler dispatch of 1000 tasks < 100 µs (criterion).
- [ ] Stage instrumentation disabled in release builds without env var.
- [ ] `cargo clippy -- -D warnings` clean.
- [ ] No `unwrap()` in `archive/inflate.rs` or `scheduler/` module.
