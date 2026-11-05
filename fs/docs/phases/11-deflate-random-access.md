# Phase 11 - Deflate Random Access (`.dzx` Block Index)

**Goal:** Enable O(1) seeks within large compressed ZIP entries. A one-time
post-processing pass inflates each entry, recording decoder state at 64 KB block
boundaries into a sidecar `.dzx` file. The archive service can then serve HTTP
Range requests by decoding only the relevant block, not the full entry.

**Estimated scope:** ~700 lines of Rust.

**Depends on:** Phase 10 (streaming ZIP core, `ByteSource`, `Decoder` traits).

**Design source:** `docs/research/deflate-random-access.md` (Option B - post-process
existing ZIP, preserves original compressed bytes).

> **Why Option B only:** Option A requires re-compressing the entry with forced
> flush boundaries, which modifies the archive and invalidates checksums. Option
> B inflates once and records decoder state snapshots, leaving the original
> archive untouched. This is the only viable strategy for externally-produced
> archives.

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
[Header - 32 bytes]
[Block Table - block_count × 56 bytes]
[Window Snapshots - block_count × 32768 bytes]
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
> implementation used during index build. The `.dzx` reader **must** use the
> same decoder implementation. Document the decoder version in the header's
> `flags` field (bits 4–7 reserved for decoder ID). For Phase 11 use only
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

## 11.3 `DzxBuilder` - Index Construction

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

## 11.4 `DzxIndex` - mmap-backed Reader

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
with many entries produce many `.dzx` files; throttle index building to avoid
I/O storms (see `DzxConfig.max_concurrent_builds`).

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

## 11.8 Tests - Phase 11

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

## 11.9 Relationship to `.zidx` and Why Two Sidecar Files

`.zidx` and `.dzx` solve completely different problems and are not
interchangeable:

|                          | `.zidx`                                                        | `.dzx`                                                                 |
| ------------------------ | -------------------------------------------------------------- | ---------------------------------------------------------------------- |
| **One per**              | ZIP file                                                       | ZIP entry (one per large deflated entry)                               |
| **Answers**              | "What nodes (files/dirs) are inside this ZIP?"                 | "How do I seek to byte N inside this specific entry's deflate stream?" |
| **Built by reading**     | Central directory only - no decompression                      | Full inflation pass through the entry                                  |
| **Indexed by**           | `blake3(zip_path).zidx`                                        | `blake3(zip_path + "#" + local_header_offset).dzx`                     |
| **Runtime cost to open** | O(1) mmap, pointer arithmetic                                  | O(log n) block lookup, then O(block_size) decompress                   |
| **Storage per unit**     | ~40 bytes/node + string pool                                   | header + **32 KB/block** - can exceed the compressed data size         |
| **Used for**             | Directory listing, file lookup, locating `local_header_offset` | HTTP `Range:` requests - partial reads without full inflation          |

The two are also sequenced: `.zidx` is built first (Phase 8, during the archive
crawl). `.dzx` is built afterwards (Phase 11, as a post-processing step) using
the `local_header_offset` that `.zidx` already recorded. If `.zidx` does not
exist for a ZIP, there is no `.dzx` for its entries either - the daemon builds
`.dzx` only after a successful `.zidx` pass.

---

## 11.10 Hard Parts and Implementation Traps

### 1. `resume_decoder` - correct `flate2` sequence

The document says "seed sliding window via `set_dictionary()`". The exact
sequence matters:

```rust
// Correct: new(false) = raw deflate (no zlib header), then set_dictionary BEFORE decompress
let mut d = flate2::Decompress::new(false);
d.set_dictionary(window)?;   // seeds the 32 KB LZ77 history
// Now call d.decompress() from comp_offset
```

**Do not** call `decompress()` first and then `set_dictionary()`. The dictionary
must be set while the stream is in the "initialized" state. `set_dictionary` on
a raw-deflate `Decompress` works because `flate2`/`miniz` mirrors zlib's
`inflateSetDictionary` semantics even for `windowBits < 0` (raw) streams.

**Edge case:** `set_dictionary` returns `DataError` if called after any
`decompress()` output has been produced. Guard with an assertion that
`d.total_out() == 0` before the call.

### 2. Maintaining the sliding window during build

`DzxBuilder::build` must track the last 32768 bytes of all decompressed output.
The naive approach (keep a `Vec<u8>` of all output) consumes
O(uncompressed_size) memory. Use a fixed-size ring buffer instead:

```rust
struct Window {
    buf: Box<[u8; 32768]>,
    head: usize,   // next write position (mod 32768)
    filled: bool,  // true once head has wrapped around at least once
}

impl Window {
    fn push_slice(&mut self, data: &[u8]) { /* write into ring, advance head */ }

    /// Return the 32 KB window in order (oldest → newest).
    fn snapshot(&self) -> [u8; 32768] {
        let mut out = [0u8; 32768];
        if !self.filled {
            out[..self.head].copy_from_slice(&self.buf[..self.head]);
        } else {
            let (a, b) = self.buf.split_at(self.head);
            out[..b.len()].copy_from_slice(b);
            out[b.len()..].copy_from_slice(a);
        }
        out
    }
}
```

Populate it by feeding every output chunk from
`flate2::Decompress::decompress()` through
`window.push_slice(&out_buf[..n_out])`. At each block boundary, call
`window.snapshot()` and write it to the snapshot section.

### 3. Block boundary detection without SYNC_FLUSH

Real-world ZIP files compressed by standard tools (zip, 7-Zip, etc.) do **not**
insert `Z_SYNC_FLUSH` / `Z_FULL_FLUSH` markers. The `bit_count = 0`
simplification means you can only place a block boundary where the inflater has
consumed a whole number of input bytes.

In practice: **deflate block boundaries are always byte-aligned**. Each deflate
block (`BTYPE=00/01/02`) ends with its last byte fully consumed - there are no
pending bits that straddle the boundary between deflate blocks. The issue is
that `flate2::Decompress` does not expose "am I at a deflate block boundary
right now?"

Practical approach:

- Drive the inflater in a loop with fixed-size output chunks (e.g., 8 KB).
- After each `decompress()` call, check `total_out`. If
  `total_out >= next_boundary`:
  - Record `comp_offset = total_in` (bytes consumed from the compressed stream).
  - `bit_count = 0`, `bit_buffer = 0` - always, because we only place boundaries
    at positions where `total_in` advances to a byte boundary (guaranteed when
    `flate2` has no partial-byte look-ahead pending).
- Advance `next_boundary += block_size`.

**Why this is safe:** `flate2::Decompress::decompress()` with `MZFlush::None`
consumes input bytes completely before returning partial output - it does not
hold back partial input bytes in a bit buffer that is invisible to the caller.
The `total_in` counter reflects fully consumed bytes. So
`comp_offset = total_in` after a `decompress()` return is always byte-aligned.

### 4. `DzxBlock` on-disk size - fix the inconsistency

The block entry comment in §11.2 incorrectly computes 36 bytes and then says
"pad to 56". The canonical layout is:

```text
comp_offset:   8 bytes  (u64)
uncomp_offset: 8 bytes  (u64)
comp_size:     4 bytes  (u32)
uncomp_size:   4 bytes  (u32)
bit_count:     1 byte   (u8)
_pad:          3 bytes
bit_buffer:    4 bytes  (u32)
snapshot_idx:  4 bytes  (u32)
_pad2:        20 bytes  (reserved, written as 0)
───────────────────────
Total:        56 bytes
```

All padding fields must be written as zero bytes (do not let Rust struct layout
write garbage). Use explicit `to_le_bytes()` calls as with `.zidx`, never
transmute.

### 5. Multi-block spanning in `read_range`

The algorithm in §11.5 says "may span multiple blocks; recurse into next block"
without showing the iteration. Implement as a loop, not recursion (recursion
depth is unbounded for large reads):

```rust
let mut remaining = len;
let mut output = BytesMut::with_capacity(len);
let mut cur_offset = offset;

while remaining > 0 {
    let block = dzx.find_block(cur_offset)?;
    let window = dzx.snapshot(block.snapshot_idx)?;
    let comp_data = source.read_range(data_start + block.comp_offset, block.comp_size as u64).await?;

    let skip = (cur_offset - block.uncomp_offset) as usize;
    let available = block.uncomp_size as usize - skip;
    let take = available.min(remaining);

    let decompressed = decompress_block(&comp_data, &window, skip, take)?;
    output.extend_from_slice(&decompressed);

    remaining -= take;
    cur_offset += take as u64;
}
```

### 6. Storage cost of window snapshots

32 KB per block is large. For a 100 MB entry at the default 64 KB block size:

```
blocks    = ceil(100 MB / 64 KB) ≈ 1 600
snapshots = 1 600 × 32 768 = 52 MB
```

The `.dzx` file for this entry is ~52 MB - larger than many compressed streams.
This is expected and acceptable because:

- `.dzx` is only built for entries above `dzx_min_entry_bytes` (default 10 MB).
- The trade-off is storage for random-access speed. Without `.dzx`, a Range
  request for the last byte of a 100 MB entry requires decompressing all 100 MB.
- Snapshots are read via `mmap` and only the relevant 32 KB page is faulted in
  per Range request.

If storage is a concern, a future optimisation is snapshot compression (each 32
KB window is highly compressible because it's raw text/binary output). Phase 11
does not implement snapshot compression - leave `flags` bit 1 reserved for it.

---

## 11.11 Implementation Reasoning

This section records the chain of thought behind the design decisions. It is not
prescriptive - §11.10 is. Read this when a decision looks arbitrary or when
something fails in an unexpected way.

### Why deflate random access requires two things, not one

The intuition is: "just seek to byte N in the compressed stream and start
decompressing." This fails for two independent reasons.

**Reason 1 - LZ77 history.** Deflate is LZ77 + Huffman. LZ77 back-references say
"emit the bytes from 12345 bytes ago, length 5." If you start mid-stream, you
don't have those previous bytes, so back-references that reach before your start
point produce garbage. The fix is the 32 KB window snapshot: replay the last
32768 bytes of output as the LZ77 history before decompressing.

**Reason 2 - bit-stream alignment.** The compressed byte stream is a packed
sequence of bits. Huffman codes and length/distance codes are not byte-aligned.
If you start decompressing from a byte that is mid-code, the inflater reads the
wrong bit pattern and produces garbage immediately. The fix is to only start
from a position where the inflater's internal bit buffer is empty - i.e., where
the compressed stream is byte-aligned. `DzxBlock.bit_count = 0` encodes this
guarantee.

Both problems must be solved simultaneously. The window snapshot alone is not
enough: even with correct history, the first symbol will be wrong if the start
position is not byte-aligned. Similarly, byte alignment alone is not enough: the
first symbol might decode correctly but reference bytes outside the window.

### Why `bit_count = 0` is a simplification, not a guarantee

Mark Adler's canonical `zran.c` implementation records the full bit-buffer state
(`bit_count`, `bit_buffer`) at every access point and uses `inflatePrime()` on
resume to re-inject those pending bits. This works for access points that fall
at any bit offset in the compressed stream.

Phase 11 simplifies by always writing `bit_count = 0, bit_buffer = 0` and only
recording access points at byte-aligned positions. This eliminates the need for
`inflatePrime` (no equivalent in `flate2`'s public API) but creates a
constraint: `comp_offset` must genuinely point to a byte-aligned position where
the inflater has no pending bits.

The question is when this is true:

- **STORED blocks (BTYPE=00):** always byte-aligned - the spec requires padding
  bits after the block header to reach a byte boundary before the length fields.
- **After Z_SYNC_FLUSH:** zlib emits padding + an empty stored block, leaving
  the stream byte-aligned. Compressed streams produced with SYNC_FLUSH between
  chunks (common in HTTP gzip, some streaming encoders) are safe.
- **Mid-block Huffman (BTYPE=01/02):** NOT byte-aligned in general. A Huffman
  block is a continuous bit-packed stream; the transition from one symbol to the
  next does not land on a byte boundary. Recording `total_in` at an arbitrary
  output position mid-block captures a `comp_offset` that may be mid-byte.

For typical ZIP entries (one large BTYPE=02 Huffman block, no SYNC_FLUSH), the
simplified approach produces a `comp_offset` that may be off by a fraction of a
byte. Resuming from that offset with `bit_count = 0` will produce wrong output
because the inflater starts reading bits from the wrong alignment.

**Practical consequence:** the DoD requirement "resume from window snapshot
produces byte-identical output vs. full inflate" is the critical test. Run it
for every fixture entry. If a fixture uses a single Huffman block, the test will
catch misalignment at block 2+. The correct fix for general streams is either:
(a) expose bit state via `miniz_oxide` internals (unsafe, version-coupled), or
(b) only build `.dzx` for entries that are STORED or known to contain SYNC_FLUSH
markers (detectable by scanning for the `0x00 0x00 0xFF 0xFF` stored-block
pattern in the compressed bytes). Option (b) is the recommended Phase 11 scope
limit.

### Why `set_dictionary` works for raw deflate

`flate2::Decompress::new(false)` creates a raw-deflate inflater (no zlib
wrapper, `windowBits = -15` in zlib terms). The `set_dictionary()` method maps
to zlib's `inflateSetDictionary()`. The zlib spec allows calling
`inflateSetDictionary` on a raw stream as a way to pre-populate the 32 KB
history buffer - it works because the history buffer is an implementation detail
of the LZ77 decoder, independent of whether the stream has a zlib header.

The constraint is timing: `set_dictionary` must be called before any
`decompress()` output is produced. Once the inflater has emitted bytes, its
internal state no longer matches "freshly initialized" and the call will fail
with `DataError`. The implementation must enforce `d.total_out() == 0` before
calling `set_dictionary`.

This is also why `resume_decoder` cannot share a `Decompress` instance across
multiple `read_range` calls - each call needs a fresh instance with the window
loaded before any decompression.

### Why O(1) `find_block`, not O(log n) binary search

Because blocks are fixed-size (`block_size` uncompressed bytes), the block
containing `uncomp_offset` is exactly `uncomp_offset / block_size` (integer
division), clamped to `block_count - 1` for the last block which may be smaller.
No search is needed. The doc comment in §11.4 says "O(log n)" - that comment is
wrong. The implementation
`(target / self.block_size as u64).min(self.block_count - 1)` is O(1).

This only holds because blocks have uniform uncompressed size. The one exception
is the last block, which has `uncomp_size <= block_size`. The clamp handles it.

### Why a ring buffer for the sliding window

During `DzxBuilder::build`, the full uncompressed output is never needed - only
the last 32768 bytes. A `Vec<u8>` growing to the full uncompressed size would
consume O(entry_size) memory, which for a 1 GB entry is 1 GB just for the build
pass.

A 32 KB ring buffer (`Box<[u8; 32768]>`) is constant memory regardless of entry
size. The snapshot is produced by linearising the ring: the half from `head` to
end of buffer is the oldest, the half from start to `head` is the newest. On
`snapshot()`, copy oldest-half first, then newest-half, into a fresh 32 KB
array. This is two `copy_from_slice` calls.

The edge case: before the buffer has been fully written once (`!filled`), the
bytes from `head` to end of buffer are uninitialized (or zeroed from `Box`
allocation). The snapshot in that case is the written prefix padded with zeros.
This matches zlib's behavior when `inflateSetDictionary` receives fewer than
32768 bytes - it still works, the history is just shorter.

### Why the storage cost is acceptable despite exceeding the compressed size

A `.dzx` for a 100 MB entry at 64 KB blocks is ~52 MB (1600 snapshots × 32 KB).
The uncompressed entry is 100 MB, the compressed entry might be 30 MB. The
`.dzx` is larger than the compressed data.

The trade-off is still correct:

- Without `.dzx`, a Range request for the last 1 KB of a 100 MB entry reads and
  inflates all 30 MB of compressed data - bounded by I/O and CPU.
- With `.dzx`, the same request reads 32 KB (one snapshot) + ~64 KB (one block)
  = ~96 KB - two orders of magnitude less I/O.
- The 52 MB `.dzx` is written once and read at most one 32 KB slice per request.
  The OS page cache makes repeated Range requests on the same block essentially
  free after the first.

The `dzx_min_entry_bytes` gate (default 10 MB) is the primary storage control.
An archive with 1000 entries of which 10 are > 10 MB will produce 10 `.dzx`
files. The storage overhead is proportional to the entries that actually benefit
from random access.

### Why `read_range` is a loop, not recursion

The naive reading of the algorithm is "if the range spans multiple blocks,
recurse into the next block." Recursion depth is `ceil(len / block_size)`. For a
Range request of 100 MB with 64 KB blocks, that's 1600 stack frames. Stack
overflow. The loop implementation is the same logic with an explicit `remaining`
counter and no stack growth.

---

## 11.12 Definition of Done

- [x] `cargo test -p filesearch archive::dzx` passes all tests.
- [x] `GET /archive/:id/file` with `Range:` header returns `206 Partial Content`
      using the `.dzx` path when available.
- [x] `.dzx` build is gated by `dzx_min_entry_bytes` - small entries are not
      indexed.
- [x] Resume from window snapshot produces byte-identical output vs. full
      inflate.
- [x] No `unwrap()` in `archive/dzx.rs`.
- [x] `cargo clippy -- -D warnings` clean.
