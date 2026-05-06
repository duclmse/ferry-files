# Phase 11 — Deflate Random Access (`.dzx` Block Index)

**Goal:** Enable O(1) seeks within large compressed ZIP entries. A one-time
post-processing pass inflates each entry, recording decoder state at 64 KB block
boundaries into a sidecar `.dzx` file. The archive service can then serve HTTP
Range requests by decoding only the relevant block, not the full entry.

**Estimated scope:** ~700 lines of Rust.

**Depends on:** Phase 10 (streaming ZIP core, `ByteSource`, `Decoder` traits).

**Design source:** `docs/00/deflate random access.md` (Option B — post-process
existing ZIP, preserves original compressed bytes).

> **Why Option B only:** Option A requires re-compressing the entry with forced
> flush boundaries, which modifies the archive and invalidates checksums. Option B
> inflates once and records decoder state snapshots, leaving the original archive
> untouched. This is the only viable strategy for externally-produced archives.

---

## 11.1 Module Layout

```
crates/filesearch/src/archive/
├── ...          ← existing (Phase 10)
└── dzx.rs       ← DzxBuilder, DzxIndex, block reader
```

---

## 11.2 `.dzx` Binary Format

All integer fields are **little-endian** on disk.

### Layout

```text
[Header — 32 bytes]
[Block Table — block_count × 56 bytes]
[Window Snapshots — block_count × 32768 bytes]
```

### Header

```rust
struct DzxHeader {
    magic:             [u8; 4],  // b"DZX1"
    version:           u32,      // 1
    block_size:        u32,      // uncompressed bytes per block (e.g. 65536)
    flags:             u32,      // bit 0: has_window_snapshots (always 1 for Option B)
    uncompressed_size: u64,
    block_count:       u64,
}
// Total: 32 bytes
```

### Block Entry

```rust
struct DzxBlock {
    comp_offset:    u64,   // byte offset of this block's compressed data in the ZIP entry
    uncomp_offset:  u64,   // logical uncompressed byte offset of block start
    comp_size:      u32,   // compressed bytes in this block (= next_block.comp_offset - this.comp_offset)
    uncomp_size:    u32,   // uncompressed bytes in this block (= block_size except last)
    bit_count:      u8,    // bits pending in bit_buffer at block start
    _pad:           [u8; 3],
    bit_buffer:     u32,   // pending bit buffer state
    snapshot_idx:   u32,   // index into window snapshot array
}
// Total: 8+8+4+4+1+3+4+4 = 36 bytes … let me recalculate
// Actually: 8+8+4+4+1+3+4+4 = 36 bytes but we want 56 for alignment; pad to 56
// Add _pad2: [u8; 20] to reach 56 bytes
```

> **Implementation note:** Because `bit_count` and `bit_buffer` capture the
> internal inflater state at a block boundary, they are specific to the decoder
> implementation used during index build. The `.dzx` reader **must** use the same
> decoder implementation. Document the decoder version in the header's `flags`
> field (bits 4–7 reserved for decoder ID). For Phase 11 use only
> `flate2`/`miniz`-compatible boundaries at `Z_SYNC_FLUSH` / natural block ends
> where the bit buffer is empty (`bit_count = 0`). This avoids storing mid-block
> state and eliminates portability concerns entirely.

The simplified approach: **capture block boundaries only at points where the
inflater bit buffer is empty** (i.e., at byte-aligned positions). This is always
true at `Z_SYNC_FLUSH` / `Z_FULL_FLUSH` output boundaries. For arbitrary deflate
streams without explicit flush markers, scan for the nearest preceding
byte-aligned position. The window snapshot is still required.

### Window Snapshot

Each block entry has an associated 32 KB window snapshot:

```text
snapshot_idx × 32768 bytes in the snapshot section
```

The snapshot is the LZ77 sliding window state at the block boundary, needed to
decode back-references that cross the boundary.

---

## 11.3 `DzxBuilder` — Index Construction

```rust
pub struct DzxBuilder {
    block_size: u32,  // default: 65536
}

impl DzxBuilder {
    pub fn new(block_size: u32) -> Self;

    /// Post-process an already-inflated entry, recording decoder state
    /// snapshots at `block_size` uncompressed byte intervals.
    ///
    /// `source` is the ZIP's ByteSource; `meta` identifies the entry.
    /// `out_path` receives the .dzx file.
    pub async fn build(
        &self,
        source: &dyn ByteSource,
        meta: &EntryMeta,
        out_path: &Path,
    ) -> Result<u64, DzxError>;  // returns block_count
}
```

### Build Algorithm

```text
1. Resolve data_start via parse_local_header_offset()
2. Read entire compressed stream from source (in 64 KB chunks)
3. Feed through a tracking inflater that captures state at block boundaries:
   a. Every `block_size` uncompressed bytes:
      - Record comp_offset (current input position)
      - Record uncomp_offset (accumulated output position)
      - Capture 32 KB sliding window (the last 32768 output bytes)
      - Record bit_count=0, bit_buffer=0 (only at byte-aligned positions)
4. Write .dzx header, block table, snapshot section
```

The "tracking inflater" wraps `flate2`'s `Decompress` (low-level zlib) to access
output position. Use `flate2::Decompress::decompress()` in a loop, recording the
`total_out` counter at each block boundary.

---

## 11.4 `DzxIndex` — mmap-backed Reader

```rust
pub struct DzxIndex {
    mmap:        memmap2::Mmap,
    block_size:  u32,
    block_count: u64,
    blocks_off:  usize,   // = 32 (header size)
    snaps_off:   usize,   // = 32 + block_count * BLOCK_ENTRY_SIZE
}

impl DzxIndex {
    pub fn open(path: &Path) -> Result<Self, DzxError>;

    /// Find the block containing `uncomp_offset`.
    pub fn find_block(&self, uncomp_offset: u64) -> Result<DzxBlockData, DzxError>;

    /// Get the 32 KB window snapshot for block `idx`.
    pub fn snapshot(&self, idx: u32) -> Result<&[u8], DzxError>;
}

pub struct DzxBlockData {
    pub comp_offset:   u64,
    pub uncomp_offset: u64,
    pub comp_size:     u32,
    pub uncomp_size:   u32,
    pub snapshot_idx:  u32,
}
```

`find_block` uses binary search over `uncomp_offset` fields (O(log n)):

```rust
pub fn find_block(&self, target: u64) -> Result<DzxBlockData, DzxError> {
    // block_id = target / block_size (integer division), clamped to block_count-1
    let id = (target / self.block_size as u64).min(self.block_count - 1);
    self.block(id)
}
```

---

## 11.5 Random-Access Read Path

```rust
pub async fn read_range(
    source: &dyn ByteSource,
    meta:   &EntryMeta,
    dzx:    &DzxIndex,
    offset: u64,
    len:    usize,
) -> io::Result<Bytes>
```

Algorithm:

```text
1. block = dzx.find_block(offset)
2. window = dzx.snapshot(block.snapshot_idx)
3. compressed = source.read_range(data_start + block.comp_offset, block.comp_size)
4. decoder = resume_decoder(window)   // seed sliding window
5. skip = offset - block.uncomp_offset
6. inflate from block.comp_offset, discard first `skip` bytes
7. collect `len` bytes of output (may span multiple blocks; recurse into next block)
8. return result
```

`resume_decoder(window: &[u8])`: create `flate2::Decompress` and pre-load the
window as dictionary via `set_dictionary()`.

---

## 11.6 HTTP Range Support in Archive Service

Extend `GET /archive/:id/file` to handle `Range:` headers when a `.dzx` sidecar
exists:

```rust
// In api/archive.rs stream_file handler:
if let Some(range) = range_header {
    if let Some(dzx_path) = dzx_path_for(&cfg, &zip_path, node.local_header_offset) {
        if let Ok(dzx) = DzxIndex::open(&dzx_path) {
            let (start, len) = parse_range(range, node.uncompressed_size)?;
            let source = Arc::new(LocalFileSource::new(zip_path));
            let bytes = read_range(source.as_ref(), &meta, &dzx, start, len).await?;
            return Ok((StatusCode::PARTIAL_CONTENT, bytes).into_response());
        }
    }
    // Fall back to full stream if no .dzx
}
```

### `.dzx` Path Convention

```
{dzx_dir}/{blake3_hex(zip_path + "#" + local_header_offset)}.dzx
```

One `.dzx` file per ZIP entry (keyed by archive + entry offset). Large archives
with many entries produce many `.dzx` files; throttle index building to avoid I/O
storms (see `DzxConfig.max_concurrent_builds`).

---

## 11.7 New Config Fields

Add to `ArchiveServiceConfig`:

```rust
/// Directory for .dzx deflate block index files.
/// Default: "{data_dir}/dzx"
pub dzx_dir: PathBuf,

/// Build .dzx indexes automatically for entries larger than this threshold.
/// 0 = disabled. Default: 10MB (10485760).
pub dzx_min_entry_bytes: u64,

/// Max concurrent .dzx build tasks. Default: 2.
pub dzx_max_concurrent_builds: usize,
```

`.dzx` index building runs in `spawn_blocking` after `.zidx` build completes,
subject to a `Semaphore(dzx_max_concurrent_builds)`.

---

## 11.8 Tests — Phase 11

```
builder:
- build .dzx for a known deflated fixture entry → block_count correct
- open .dzx → find_block(0) returns first block
- find_block(uncomp_offset) returns block where uncomp_offset falls
- find_block(total - 1) returns last block

read_range:
- read_range(source, meta, dzx, 0, n) == inflate first n bytes directly
- read_range(source, meta, dzx, block_size, n) == decompress from block 1
- read_range crosses block boundary → correct bytes spanning two blocks
- read_range on stored entry (no dzx needed) → pass-through

HTTP range:
- GET /archive/:id/file with Range header + .dzx present → 206, correct bytes
- GET /archive/:id/file with Range header, no .dzx → falls back to 200 full stream
- Range request for large entry in < 10ms (p95 target on local SSD)

edge cases:
- last block uncomp_size < block_size → correct
- single-block entry → works
- entry with uncompressed_size = 0 → empty response, no crash
```

---

## 11.9 Definition of Done

- [ ] `cargo test -p filesearch archive::dzx` passes all tests.
- [ ] `GET /archive/:id/file` with `Range:` header returns `206 Partial Content`
      using the `.dzx` path when available.
- [ ] `.dzx` build is gated by `dzx_min_entry_bytes` — small entries are not
      indexed.
- [ ] Resume from window snapshot produces byte-identical output vs. full inflate.
- [ ] No `unwrap()` in `archive/dzx.rs`.
- [ ] `cargo clippy -- -D warnings` clean.
