# SIMD Inflate & Benchmarking Design Note

> **Status:** research note. Design source for `docs/phases/13-simd-and-benchmarks.md`; the phase doc is the
> implementation plan and `docs/reference/` stays authoritative.

1. a **SIMD-friendly deflate pipeline design** (what actually moves the needle)
2. a **benchmarking framework** that isolates bottlenecks (so you don’t optimize
   the wrong thing)

---

# Part 1 - SIMD-friendly Deflate Pipeline

## Reality check (important)

Deflate is not trivially SIMD-izable because:

- Huffman decoding = bit-level, branchy
- LZ77 backreferences = data-dependent copies

So the win comes from:

> **hybrid design: scalar control + SIMD data movement**

---

## 1. Pipeline structure

```text id="7j7pxg"
[ByteSource]
    ↓
[Chunk Reader] (64KB aligned)
    ↓
[Bitstream Decoder] (scalar)
    ↓
[Token Stream]
    ↓
[SIMD Copy Engine]  ← big win here
    ↓
[Output Ring Buffer]
    ↓
[Chunked Bytes → network]
```

---

## 2. Token-based execution model

Instead of “decode + emit immediately”, split:

```rust id="u86xg5"
enum Token {
    Literal(u8),
    Match { len: usize, dist: usize },
}
```

Why:

- separates control flow from data movement
- lets you batch operations → SIMD-friendly

---

## 3. SIMD copy engine (core optimization)

### Problem

Naive copy:

```rust id="1z3q3p"
for i in 0..len {
    out[pos + i] = out[pos + i - dist];
}
```

→ terrible for large matches

---

### SIMD strategy

Use wide loads/stores:

```rust id="3g3d9n"
use std::arch::x86_64::*;

unsafe fn copy_simd(dst: *mut u8, src: *const u8, len: usize) {
    let mut i = 0;

    while i + 32 <= len {
        let v = _mm256_loadu_si256(src.add(i) as *const _);
        _mm256_storeu_si256(dst.add(i) as *mut _, v);
        i += 32;
    }

    while i < len {
        *dst.add(i) = *src.add(i);
        i += 1;
    }
}
```

---

### Handling overlap (critical)

LZ77 allows overlap (dist < len)

Solution:

- fallback to smaller stride OR
- use “expanding copy”

```rust id="6g7h4r"
while copied < len {
    let chunk = min(dist, len - copied);
    memcpy(dst + copied, dst + copied - dist, chunk);
    copied += chunk;
}
```

Then SIMD inside each chunk.

---

## 4. Ring buffer design

```rust id="n2u1c9"
const WINDOW: usize = 32 * 1024;

struct Ring {
    buf: [u8; WINDOW * 2], // double buffer trick
    pos: usize,
}
```

Why double buffer:

- avoids modulo on every access
- simplifies SIMD copies

---

## 5. Bitstream decoder (keep scalar, optimize branches)

Key optimizations:

### A. Lookup tables (fast Huffman)

```rust id="k1n8df"
struct HuffmanTable {
    fast: [u16; 1 << 9], // 9-bit lookup
}
```

- decode most symbols in one lookup
- fallback for long codes

---

### B. Prefetch input

```rust id="0y4u3z"
core::arch::x86_64::_mm_prefetch(ptr.add(128), _MM_HINT_T0);
```

---

## 6. Chunked streaming

Emit in fixed-size chunks:

```rust id="r9d3d1"
const OUT_CHUNK: usize = 64 * 1024;
```

Avoid:

- reallocations
- large buffers

---

## 7. Parallelism (don’t overdo)

Deflate is mostly sequential, but you can:

- parallelize across files (best)
- NOT within a single stream (usually not worth it)

---

## 8. Where SIMD actually helps most

| Stage             | SIMD impact   |
| ----------------- | ------------- |
| Huffman decode    | low           |
| Match copy        | **very high** |
| Output formatting | medium        |

---

# Part 2 - Benchmarking Framework

You cannot optimize blindly. You need **stage-level visibility**.

---

## 1. Benchmark dimensions

You must separate:

```text id="0b9l3p"
1. IO (read_range latency)
2. Parsing (central directory)
3. Decode (inflate)
4. Copy (LZ77 expansion)
5. Scheduling overhead
```

---

## 2. Micro-bench (criterion)

Use criterion

### Example

```rust id="z8h8c7"
fn bench_inflate(c: &mut Criterion) {
    let data = load_sample();

    c.bench_function("inflate", |b| {
        b.iter(|| inflate(&data))
    });
}
```

---

## 3. Stage instrumentation (critical)

Add counters:

```rust id="9p6h7g"
struct Stats {
    io_ns: AtomicU64,
    decode_ns: AtomicU64,
    copy_ns: AtomicU64,
}
```

Wrap sections:

```rust id="y1g5p6"
let t0 = now();
decode();
stats.decode_ns += now() - t0;
```

---

## 4. End-to-end load test

Simulate real usage:

```text id="q1w2e3"
- 10 tenants
- 100 archives
- mixed file sizes
- concurrent requests
```

Measure:

- p50 / p95 latency
- throughput (MB/s)
- CPU utilization

---

## 5. Flamegraphs

Use where supported by the platform:

- cargo-flamegraph

```bash id="t6k8p1"
cargo flamegraph
```

This tells you:

- where CPU actually goes
- whether SIMD is worth it

---

## 6. IO vs CPU separation test

### Trick

On Linux test machines where you control the host, run with:

```bash id="f4n2d7"
echo 3 > /proc/sys/vm/drop_caches
```

Then:

- compare warm vs cold runs

Do not run this on a shared developer machine or CI worker without explicit
approval. It requires root and disrupts the whole system page cache. Prefer a
dedicated benchmark host, large test data that exceeds RAM, or per-process
cache-control techniques where available.

---

## 7. Scheduler impact measurement

Track:

```rust id="m3c8v2"
queue_wait_ns
execution_ns
```

If wait >> execution: → scheduler bottleneck

---

## 8. Synthetic worst-case tests

Test:

- many small files (metadata heavy)
- huge compressed file (CPU heavy)
- high concurrency (scheduler stress)

---

## 9. KPI targets (realistic)

For a good system:

- inflate throughput: **300–800 MB/s per core**
- latency (small file): **<5 ms**
- scheduler overhead: **<5% CPU**

---

# Putting it together

## Optimized execution path

```text id="c8k9p0"
HTTP request
    ↓
Scheduler (tenant + archive fairness)
    ↓
Streaming extractor
    ↓
Range IO (`filesearch` API / local file)
    ↓
Deflate pipeline:
    - scalar decode
    - SIMD match copy
    ↓
Chunked output (Bytes)
    ↓
Network
```

---

# What actually gives you the biggest wins

In order:

1. **Avoid full-file load (you already did)**
2. **Scheduler fairness (prevents collapse under load)**
3. **SIMD match copy (real CPU gain)**
4. **Memory layout (ring buffer, no realloc)**
5. **Decoder tweaks (smaller gains)**

---

# Bottom line

- SIMD helps, but **only in the right place (copy stage)**
- Architecture (streaming + scheduling) gives bigger gains than
  instruction-level tricks
- Benchmarking is non-negotiable - most “optimizations” don’t matter without it

---

If you want, next step I can:

- give you a **minimal SIMD deflate prototype (compilable Rust)**
- or design a **block-indexed deflate format for true random access inside large
  files** (that’s a serious upgrade)
