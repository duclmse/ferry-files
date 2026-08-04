You’re trying to break a fundamental limitation: **Deflate is a serial stream**,
so random access requires reconstructing decoder state. The way around it is to
**insert restart points** and index them. What follows is a design that works
with existing ZIPs (sidecar index) and can evolve into a first-class format.

---

# 1) Design goals

- **O(1) seek to ~N KB granularity**
- **Bounded re-decode** (≤ block size + small window)
- **No full-file inflate**
- **Works over HTTP Range**
- **Compatible with existing ZIP entries (sidecar index)**
- **Upgradeable to embedded index later**

---

# 2) Core idea: block-indexed Deflate (BGZF-style, generalized)

Split the logical file into **independently-decodable blocks**. For each block,
store enough information to **resume decompression locally**.

There are two viable strategies:

### A. Hard resets (simpler, larger)

- Insert **Z_FULL_FLUSH / Z_SYNC_FLUSH** boundaries when compressing
- Each block can start decoding with a fresh or semi-fresh state
- Minimal side data

### B. Soft checkpoints (smaller, more complex)

- Don’t force flushes
- Instead **capture decoder state** at intervals:
  - bit buffer state
  - Huffman tables (or a way to reconstruct them)
  - **32 KB sliding window snapshot**

- Requires more index data, but better compression

**Recommendation:** start with **A (flush-based)** for production simplicity;
add **B** later if size matters.

---

# 3) Sidecar index format (`.dzx`)

For existing ZIP entries, a sidecar index can store checkpoints discovered
during a one-time full inflate. Flush-based restart points require recompressing
the entry and therefore do not preserve the original compressed bytes.

## Layout

```text
[Header]
[Block Table]
[Optional Window Snapshots]
```

### Header

```c
struct DzxHeader {
  char magic[4];      // "DZX1"
  u32  version;
  u32  block_size;    // e.g., 64KB uncompressed
  u64  uncompressed_size;
  u64  block_count;
  u32  flags;         // bit 0: has_snapshots
}
```

---

### Block entry

```c
struct BlockEntry {
  u64 comp_offset;    // offset in compressed stream
  u32 comp_size;      // compressed bytes for this block (optional if next offset known)
  u64 uncomp_offset;  // logical offset
  u32 uncomp_size;    // usually = block_size except last

  // minimal decoder state
  u8  bit_count;      // bits in bit buffer
  u32 bit_buffer;     // pending bits

  // optional (for soft checkpoints)
  u32 snapshot_offset; // offset into snapshot section
};
```

---

### Snapshot (optional, for soft checkpoints)

```c
struct Snapshot {
  u8 window[32768];   // sliding window
  // optional: serialized Huffman tables if mid-block
}
```

---

# 4) Building the index

## Option A (flush-based, requires recompression - NOT for existing archives)

During compression or a deliberate re-pack step:

```text
every 64KB uncompressed:
  emit Z_FULL_FLUSH
  record:
    - current compressed offset
    - current uncompressed offset
```

No snapshot needed, but the ZIP entry's **compressed bytes change**. This means
the original archive is replaced or a new copy is produced. Use this only when
the service owns the archive artifact end-to-end and can produce an optimized
copy at ingest time. **Do not use Option A on archives received from external
producers** - the re-compression invalidates checksums, modifies the archive, and
breaks round-trip integrity. For unmodified existing ZIPs, Option B is the only
viable strategy.

---

## Option B (post-process existing ZIP, preserves bytes)

If you can’t re-compress:

1. Inflate once
2. At each boundary:
   - record compressed offset
   - **capture 32KB window**
   - record bit buffer state

This is more expensive upfront but keeps original data intact. Treat the
captured decoder state as implementation-specific unless it is encoded by a
well-tested library; serializing raw internal inflater state is not portable
across decoder implementations.

---

# 5) Read path (random access)

## Goal

Read `[offset, offset+len)` without full decode

---

## Algorithm

```text
1. Locate block:
   block_id = offset / block_size

2. Fetch BlockEntry

3. Fetch compressed range:
   [block.comp_offset → next_block.comp_offset]

4. Initialize decoder:
   - reset or restore snapshot
   - restore bit buffer

5. Decode:
   - skip (offset % block_size)
   - stream output until len satisfied
```

---

## Pseudocode

```rust
fn read_range(offset: u64, len: usize) -> Bytes {
    let block = index.find_block(offset);

    let comp = fetch_range(
        block.comp_offset,
        next_block_offset(block)
    );

    let mut decoder = init_decoder(block);

    let mut out = Vec::with_capacity(len);

    decoder.skip(offset - block.uncomp_offset);

    while out.len() < len {
        out.extend(decoder.next_chunk()?);
    }

    out.truncate(len);
    Bytes::from(out)
}
```

---

# 6) HTTP Range integration

Perfect fit:

```http
GET /archive/file?offset=1048576&length=65536
```

Backend:

- translates to **compressed byte ranges**
- returns decompressed stream

No full file transfer.

---

# 7) Choosing block size

Trade-off:

| Block size | Pros              | Cons                 |
| ---------- | ----------------- | -------------------- |
| 32 KB      | minimal over-read | larger index         |
| 64 KB      | good default      | balanced             |
| 256 KB     | smaller index     | more decode per seek |

**Recommendation:** 64 KB or 128 KB

---

# 8) Compression impact

- With **Z_FULL_FLUSH** every 64KB:
  - overhead ≈ 0.5–2%

- With snapshots:
  - zero compression loss
  - index size increases (~32KB per block if naive)

---

# 9) Integration with your backend

## Extend your `.zidx`

Add:

```c
struct FileEntry {
  ...
  u64 dzx_offset;   // where block index starts
  u32 dzx_blocks;
}
```

---

## Request flow

```text
UI → /file?node=123&offset=...
   → scheduler
   → index lookup (node → dzx)
   → block lookup
   → HTTP Range fetch
   → streaming decoder
```

---

# 10) Advanced optimizations

## A. Multi-block prefetch

If user scrolls:

- prefetch next 2–3 blocks

---

## B. Cache decoded windows

LRU cache:

```text
(key: block_id) → 32KB window
```

Speeds sequential reads dramatically

---

## C. Parallel decode (coarse)

For large reads:

- decode multiple blocks in parallel
- merge output

---

## D. SIMD integration

Your earlier SIMD copy engine fits here:

- each block decoded independently
- better CPU utilization

---

# 11) Compatibility strategy

## Phase 1

- sidecar `.dzx`
- works with existing ZIPs

## Phase 2

- embed index in ZIP extra field

## Phase 3

- custom container (optional)

---

# 12) What this enables

You now get:

- true **seekable large files inside ZIP**
- efficient **hex viewer**
- partial reads for:
  - video
  - logs
  - databases

---

# 13) Limitations

- Requires preprocessing (index build)
- Slight storage overhead
- Not standard ZIP behavior (but compatible)

---

# Bottom line

You’ve turned Deflate from:

> sequential compression stream

into:

> a **block-addressable compressed storage format**

which aligns perfectly with:

- your scheduler
- HTTP range serving
- virtual filesystem model

---
