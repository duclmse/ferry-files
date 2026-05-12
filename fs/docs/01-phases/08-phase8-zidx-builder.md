# Phase 8 — `.zidx` Index Builder

**Goal:** Implement the binary `.zidx` index format and its builder. The builder
reads a ZIP's central directory once and emits a flat, mmap-friendly inode tree
that the archive service API (Phase 9) serves without any per-request parsing.
The reader provides zero-copy traversal over the mmap at request time.

**Estimated scope:** ~650 lines of Rust.

**Depends on:** Phase 2 (SQLite store), Phase 3 (archive.rs ZIP iteration).

**Design source:** `docs/00/zidx.md`, `docs/00/backend design.md`.

---

## 8.1 Module Layout

```
crates/filesearch/src/zidx/
├── mod.rs        ← public API: ZidxBuilder, ZidxIndex, ZidxError
├── builder.rs    ← build pipeline: ZIP → tree → flat nodes → write
└── reader.rs     ← mmap-backed reader, node/name accessors
```

Add to `crates/filesearch/src/lib.rs`:

```rust
pub mod zidx;
```

---

## 8.2 Binary Format

All integer fields are **little-endian** on disk. Never write raw Rust struct
bytes — use `to_le_bytes()` for each field. This is the stable on-disk contract.

### Layout

```text
[Header — 20 bytes]
[Node Table — node_count × 40 bytes]
[String Pool — string_bytes bytes, UTF-8]
```

### Header

```rust
// On-disk layout only. Do not transmute mmap bytes to this type.
struct Header {
    magic:        [u8; 4],  // b"ZIDX"
    version:      u32,      // 1
    node_count:   u32,
    string_bytes: u32,
    root_node:    u32,      // always 0
}
// Total: 20 bytes
```

### Node

```rust
// On-disk layout only.
struct Node {
    parent:               u32,   // index into node table; 0 for root (points to self)
    first_child:          u32,   // u32::MAX = none
    next_sibling:         u32,   // u32::MAX = none
    name_off:             u32,   // byte offset into string pool
    name_len:             u16,   // byte length of name in string pool
    kind:                 u8,    // 0 = file, 1 = dir
    _pad:                 u8,    // reserved, write 0
    compression:          u16,   // ZIP method: 0=stored, 8=deflate
    _pad2:                u16,   // reserved, write 0
    compressed_size:      u32,
    uncompressed_size:    u32,
    local_header_offset:  u64,   // byte offset of local file header in the ZIP blob
}
// Total: 4+4+4+4+2+1+1+2+2+4+4+8 = 40 bytes
```

**Sentinel:** `u32::MAX` represents a null pointer (no child / no sibling).

---

## 8.3 Build Pipeline

```text
ZIP → enumerate entries (zip crate)
    → normalise paths
    → insert into BuildNode tree (BTreeMap children — sorted)
    → flatten tree with preorder DFS → Vec<FlatNode>
    → compute first_child / next_sibling linked-list links
    → intern all names into StringPool (deduplicated)
    → fill Node structs
    → write Header + Node Table + String Pool using to_le_bytes()
```

### 8.3.1 `BuildNode` (temporary, mutable)

```rust
struct BuildNode {
    name:                  String,
    kind:                  NodeKind,
    children:              BTreeMap<String, BuildNode>,  // BTreeMap → deterministic order
    compression:           u16,
    compressed_size:       u32,
    uncompressed_size:     u32,
    local_header_offset:   u64,
}

enum NodeKind { File, Dir }
```

### 8.3.2 Path Normalisation

Before inserting:
- Strip leading `/`.
- Collapse consecutive `/` sequences.
- Remove trailing `/` (directories are determined by `ZipFile::is_dir()`).
- Split on `/` and filter empty segments.

### 8.3.3 DFS Flatten

Use preorder DFS: parent always receives a lower ID than its children. Children
within the same directory are ordered by their `BTreeMap` key (lexicographic),
giving deterministic output.

```rust
fn flatten(node: &BuildNode, parent_id: u32, out: &mut Vec<FlatNode>) -> u32 {
    let id = out.len() as u32;
    out.push(FlatNode { parent: parent_id, name: node.name.clone(),
                        kind: node.kind, children: vec![],
                        /* copy file metadata */ });
    let mut child_ids = vec![];
    for child in node.children.values() {
        let cid = flatten(child, id, out);
        child_ids.push(cid);
    }
    out[id as usize].children = child_ids;
    id
}
```

### 8.3.4 Sibling Linking

After flatten, convert `children: Vec<u32>` → `first_child` + `next_sibling`
linked list:

```rust
for flat in &flat_nodes {
    nodes[flat.id].first_child = flat.children.first().copied().unwrap_or(u32::MAX);
    for window in flat.children.windows(2) {
        nodes[window[0] as usize].next_sibling = window[1];
    }
    if let Some(&last) = flat.children.last() {
        nodes[last as usize].next_sibling = u32::MAX;
    }
}
```

### 8.3.5 String Pool

```rust
struct StringPool {
    map: HashMap<String, u32>,
    buf: Vec<u8>,
}

impl StringPool {
    fn intern(&mut self, s: &str) -> (u32, u16) {
        if let Some(&off) = self.map.get(s) {
            return (off, s.len() as u16);
        }
        let off = self.buf.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        self.map.insert(s.to_string(), off);
        (off, s.len() as u16)
    }
}
```

---

## 8.4 `ZidxBuilder` Public API

```rust
pub struct ZidxBuilder;

impl ZidxBuilder {
    /// Build a `.zidx` sidecar for the ZIP at `zip_path`.
    /// Writes to `out_path`. Overwrites if file already exists.
    /// Returns the number of nodes written.
    pub fn build(zip_path: &Path, out_path: &Path) -> Result<u32, ZidxError>;
}
```

`ZidxError`:

```rust
#[derive(thiserror::Error, Debug)]
pub enum ZidxError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Archive too large: {0} entries exceeds u32::MAX")]
    TooLarge(usize),
    #[error("Invalid .zidx file: {0}")]
    Corrupt(String),
}
```

---

## 8.5 Write Helpers

Never transmute structs to bytes. Write each field individually:

```rust
fn write_header_le(w: &mut impl Write, h: &HeaderData) -> io::Result<()> {
    w.write_all(&h.magic)?;
    w.write_all(&h.version.to_le_bytes())?;
    w.write_all(&h.node_count.to_le_bytes())?;
    w.write_all(&h.string_bytes.to_le_bytes())?;
    w.write_all(&h.root_node.to_le_bytes())?;
    Ok(())
}

fn write_node_le(w: &mut impl Write, n: &NodeData) -> io::Result<()> {
    w.write_all(&n.parent.to_le_bytes())?;
    w.write_all(&n.first_child.to_le_bytes())?;
    w.write_all(&n.next_sibling.to_le_bytes())?;
    w.write_all(&n.name_off.to_le_bytes())?;
    w.write_all(&n.name_len.to_le_bytes())?;
    w.write_all(&[n.kind, 0])?;         // kind + _pad
    w.write_all(&n.compression.to_le_bytes())?;
    w.write_all(&[0u8; 2])?;            // _pad2
    w.write_all(&n.compressed_size.to_le_bytes())?;
    w.write_all(&n.uncompressed_size.to_le_bytes())?;
    w.write_all(&n.local_header_offset.to_le_bytes())?;
    Ok(())
}
```

Use a `BufWriter` wrapping the output `File` (256 KB buffer).

---

## 8.6 `ZidxIndex` — mmap-backed Reader

```rust
pub struct ZidxIndex {
    mmap:           memmap2::Mmap,
    node_count:     u32,
    nodes_offset:   usize,   // = 20 (header size)
    strings_offset: usize,   // = 20 + node_count * 40
    string_bytes:   u32,
}

impl ZidxIndex {
    /// Open and validate a .zidx file. Returns Err if magic/version/bounds fail.
    pub fn open(path: &Path) -> Result<Self, ZidxError>;

    /// Read node `id` by decoding fields from mmap bytes (from_le_bytes).
    pub fn node(&self, id: u32) -> Result<NodeData, ZidxError>;

    /// Borrow the name of a node from the string pool.
    pub fn name<'a>(&'a self, node: &NodeData) -> Result<&'a str, ZidxError>;

    /// Iterate direct children of `parent_id` via first_child / next_sibling links.
    pub fn children(&self, parent_id: u32) -> ChildIter<'_>;

    /// Walk from root to find the node for a path string.
    /// Path components are split on '/'. Returns None if not found.
    pub fn lookup(&self, path: &str) -> Result<Option<u32>, ZidxError>;
}
```

Validation in `open()`:
1. File length ≥ 20 (header size).
2. `magic == b"ZIDX"`.
3. `version == 1`.
4. `file_len >= 20 + node_count * 40 + string_bytes`.

All reads use `from_le_bytes()` on mmap slices, with bounds checks returning
`ZidxError::Corrupt` on out-of-range access.

---

## 8.7 Integration with Existing Indexer

`ZidxBuilder::build()` is called from `crawl_archive()` in
`indexer/crawler.rs` when `config.archive_service.enabled = true`:

```rust
if cfg.archive_service.enabled {
    let zidx_path = zidx_path_for(&cfg.archive_service.zidx_dir, archive_path);
    tokio::task::spawn_blocking(move || {
        ZidxBuilder::build(&archive_path, &zidx_path)
    }).await??;
}
```

`zidx_path_for(zidx_dir, zip_path)` maps the zip path to a sidecar path under
`zidx_dir`, using a blake3 hex of the zip path as the filename stem:

```
{zidx_dir}/{blake3_hex(zip_path_str)}.zidx
```

This keeps `.zidx` files out of the watched directories and provides a stable,
collision-free lookup key.

---

## 8.8 New Config Fields

Add to `ArchiveServiceConfig` (introduced in Phase 9, but referenced here):

```rust
/// Root directory for .zidx sidecar files.
/// Default: "{data_dir}/zidx"
pub zidx_dir: PathBuf,
```

For Phase 8 only, validate that `zidx_dir` is writable and create it on startup.

---

## 8.9 New Workspace Dependencies

```toml
# In workspace Cargo.toml [workspace.dependencies] if not already present:
blake3   = "1"

# In crates/filesearch/Cargo.toml [dependencies] if not already present:
memmap2  = "0.9"   # already present per ref doc §3
blake3   = { workspace = true }
```

---

## 8.10 Tests — Phase 8

```
builder:
- build from fixture ZIP with 5 entries → node_count == 6 (5 + root)
- output file has correct magic "ZIDX" at offset 0
- root node at id=0, kind=dir
- file nodes have correct compressed_size, uncompressed_size, local_header_offset
- directory nodes: kind=1, compressed_size=0
- build from ZIP with 0 entries → node_count == 1 (root only)
- build from nonexistent path → ZidxError::Io
- deterministic: build same ZIP twice → identical output bytes

reader:
- open valid .zidx → Ok
- open file with wrong magic → ZidxError::Corrupt
- open truncated file → ZidxError::Corrupt
- node(0) returns root
- node(out_of_range) → ZidxError::Corrupt
- name() returns correct UTF-8 for each node
- children() iteration matches expected file list
- lookup("src/main.rs") returns correct node id
- lookup("nonexistent") → Ok(None)

integration:
- build .zidx from fixture ZIP, open with ZidxIndex, walk all entries →
  virtual paths match what ArchiveCrawler produces for the same ZIP
```

---

## 8.11 Definition of Done

- [x] `cargo test -p filesearch zidx` passes all builder and reader tests.
- [x] `.zidx` files are written to `zidx_dir` during `full_crawl()` when
      `archive_service.enabled = true`.
- [x] All write paths use `to_le_bytes()` — no transmute/raw casts.
- [x] `ZidxIndex::open()` rejects malformed files with `ZidxError::Corrupt`.
- [x] Deterministic: rebuilding the same ZIP produces byte-identical output.
- [x] No `unwrap()` in `zidx/` module code.
- [x] `cargo clippy -- -D warnings` clean.

---

## Implementation Status (2026-05-12)

### Files created

| File | Description |
|------|-------------|
| `crates/filesearch/src/zidx/mod.rs` | Public API: `ZidxError`, `zidx_path_for()`, re-exports |
| `crates/filesearch/src/zidx/builder.rs` | `ZidxBuilder::build()` + 8 unit tests |
| `crates/filesearch/src/zidx/reader.rs` | `ZidxIndex`, `NodeData`, `ChildIter` + 9 unit tests |

### Files modified

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Added `blake3 = "1"` to `[workspace.dependencies]` |
| `crates/filesearch/Cargo.toml` | Added `blake3 = { workspace = true }` |
| `crates/filesearch/src/lib.rs` | Added `pub mod zidx;` |
| `crates/filesearch/src/config.rs` | Added `ArchiveServiceConfig { enabled, zidx_dir }` and field `archive_service` in `Config` |
| `crates/filesearch/src/indexer/crawler.rs` | `crawl_archive_sync` gains `zidx_dir: Option<&Path>` parameter; all 3 call sites updated; `.zidx` built after archive crawl when enabled |

### Implementation notes

**`compression_method_to_u16`** — zip 2.x exposes `CompressionMethod` as a non-exhaustive enum
(not a struct with public field, and no `From<CompressionMethod> for u16` impl). A `match` on
`Stored` (→ 0) / `Deflated` (→ 8) / `_` (→ 0) covers all cases seen in practice.

**`ZidxIndex::Debug`** — `memmap2::Mmap` does not implement `Debug`, so `ZidxIndex` has a hand-written
impl that only shows `node_count`.

**`zidx_path_for`** — uses `blake3::hash(zip_path_as_bytes).to_hex()` as the stem so sidecar files
never collide regardless of ZIP directory depth, and they live entirely under `zidx_dir` (not in
the watched directory tree).

**Crawler integration** — `crawl_archive_sync` is already in a blocking context (`rayon` walk thread
or `tokio::task::spawn_blocking`), so `ZidxBuilder::build()` is called synchronously — no extra
`spawn_blocking` needed. Failures are logged as warnings and do not abort the archive crawl.

**Test count after Phase 8:** 165 filesearch unit (was 148 + 17 new zidx) + 5 integration + 67 rar2zip = 237 total.
