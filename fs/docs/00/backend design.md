# Rust Backend Design Note

This document is a design sketch for a future archive-serving backend. Code
snippets are illustrative unless explicitly marked as production-ready. The
current production specs remain in `docs/02-ref/`.

Below is a **Rust backend design** built around:

* **Async I/O (Tokio)**
* **Memory-mapped index (.zidx)**
* **Zero-copy responses where possible**
* **Streaming decompression for files**

I’ll give you a concrete, implementable blueprint.

---

# 1. High-level architecture

```text
    ┌─────────────────────────┐
    │        Frontend         │
    │  (your column UI)       │
    └──────────┬──────────────┘
               │ HTTP
    ┌──────────▼──────────────┐
    │      Rust Backend       │
    │  axum + tokio           │
    ├──────────┬──────────────┤
    │  Index   │  File Stream │
    │  (.zidx) │  (inflate)   │
    └────┬─────┴──────┬───────┘
         │            │
  mmap() │            │ read + seek
         ▼            ▼
    ZIP Index File  ZIP Blob
```

---

# 2. Core components

## Crates

```toml
[dependencies]
axum = "0.7"
tokio = { version = "1", features = ["full"] }
memmap2 = "0.9"
bytes = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
flate2 = { version = "1", features = ["zlib"] }
tokio-util = { version = "0.7", features = ["io"] }
async-stream = "0.3"
```

---

# 3. Index format (.zidx)

### Design goals

* mmap-friendly
* no pointers
* contiguous layout

---

## Binary layout

```text
[Header]
[Node Table]
[String Pool]
```

### Header

```rust
#[repr(C)]
struct Header {
    magic: [u8; 4], // "ZIDX"
    version: u32,
    node_count: u32,
    string_size: u32,
}
```

---

### Node (inode-style)

```rust
#[repr(C)]
struct Node {
    parent: u32,
    first_child: u32,
    next_sibling: u32,

    name_offset: u32,
    name_len: u16,

    compression: u16,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u64,
}
```

---

# 4. Building the index (one-time)

Use libzip or manual parser.

### Key idea

* Parse central directory
* Build flat node list
* Write `.zidx`

---

## Skeleton

```rust
fn build_index(zip_path: &str, out_path: &str) -> anyhow::Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut nodes = Vec::new();
    let mut strings = Vec::new();

    // root node
    nodes.push(Node {
        parent: 0,
        first_child: u32::MAX,
        next_sibling: u32::MAX,
        name_offset: 0,
        name_len: 1,
        compression: 0,
        compressed_size: 0,
        uncompressed_size: 0,
        local_header_offset: 0,
    });

    // build tree...
    // (you already know how from previous step)

    // write file
    let mut out = std::fs::File::create(out_path)?;

    // write header + nodes + strings
    Ok(())
}
```

---

# 5. mmap index (zero-copy)

```rust
use memmap2::Mmap;

struct Index {
    mmap: Mmap,
    header_offset: usize,
    nodes_offset: usize,
    strings_offset: usize,
}

impl Index {
    fn open(path: &str) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        // Production code must validate file length, magic, version, node_count,
        // string_size, and all computed offsets before exposing typed accessors.
        let header_offset = 0;
        let header = read_header_le(&mmap)?;

        if header.magic != *b"ZIDX" {
            anyhow::bail!("invalid zidx magic");
        }

        let nodes_offset = std::mem::size_of::<Header>();
        let nodes_bytes = header.node_count as usize * std::mem::size_of::<Node>();
        let strings_offset = nodes_offset + nodes_bytes;

        if strings_offset + header.string_size as usize > mmap.len() {
            anyhow::bail!("truncated zidx file");
        }

        Ok(Self { mmap, header_offset, nodes_offset, strings_offset })
    }

    fn node(&self, id: u32) -> anyhow::Result<Node> {
        let header = read_header_le(&self.mmap)?;
        if id >= header.node_count {
            anyhow::bail!("node id out of range");
        }
        read_node_le(&self.mmap, self.nodes_offset, id)
    }

    fn name<'a>(&'a self, node: &Node) -> anyhow::Result<&'a str> {
        let start = self.strings_offset + node.name_offset as usize;
        let end = start + node.name_len as usize;
        let bytes = self.mmap.get(start..end).ok_or_else(|| anyhow::anyhow!("bad name range"))?;
        Ok(std::str::from_utf8(bytes)?)
    }
}
```

`read_header_le` and `read_node_le` should decode fields from bytes using
`from_le_bytes()`. Avoid casting mmap bytes directly to Rust structs unless the
type has a proven plain-data layout and alignment has been checked.

---

# 6. Directory endpoint (fits your column UI)

## API

```http
GET /archive/{id}/dir?node=123
```

---

## Handler

```rust
async fn dir(
    State(state): State<AppState>,
    Query(q): Query<DirQuery>,
) -> Result<Json<Vec<DirEntry>>, StatusCode> {
    let idx = &state.index;

    let node = idx.node(q.node).map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut result = Vec::new();
    let mut child = node.first_child;

    while child != u32::MAX {
        let n = idx.node(child).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let name = idx.name(&n).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        result.push(DirEntry {
            id: child,
            name: name.to_string(),
            is_dir: n.kind == 1,  // use kind field; first_child == u32::MAX for empty dirs
            size: n.uncompressed_size,
        });

        child = n.next_sibling;
    }

    Ok(Json(result))
}
```

---

# 7. File streaming endpoint

## API

```http
GET /archive/{id}/file?node=123
```

---

## Core idea

* Seek to `local_header_offset`
* Skip local header
* Stream compressed data
* Decompress on the fly

---

## Implementation guidance

Do not wrap a blocking `flate2::read::DeflateDecoder` around a Tokio file on
the async runtime. Either:

- use a fully async decoder from the async compression ecosystem; or
- run the blocking seek/read/inflate path in `tokio::task::spawn_blocking` and
  stream bounded `Bytes` chunks back over a channel.

Production handlers must avoid `unwrap()`, validate node IDs, validate local
headers, cap response sizes where appropriate, and return typed HTTP errors.

---

# 8. App state

```rust
struct AppState {
    index: Index,
    zip_path: String,
}
```

---

# 9. Router

```rust
use axum::{Router, routing::get};

fn app(state: AppState) -> Router {
    Router::new()
        .route("/archive/:id/dir", get(dir))
        .route("/archive/:id/file", get(file))
        .with_state(state)
}
```

---

# 10. Why this matches your UI perfectly

Your frontend already expects:

* hierarchical navigation → `/dir`
* lazy loading → `/file`
* node-based traversal → `node id`

So:

| Frontend     | Backend             |
| ------------ | ------------------- |
| column click | `/dir?node=`        |
| file click   | `/file?node=`       |
| no zip.js    | backend handles all |

---

# 11. Performance characteristics

### Index load

* mmap → **O(1)** startup
* no parsing at runtime

### Directory listing

* pointer traversal only
* no allocations except response JSON

### File read

* single seek + stream
* no full buffering

---

# 12. Where to go next (high impact)

### 1. Parallel decompression

* spawn blocking pool for inflate

### 2. Range support on decompressed stream

* requires block index

### 3. Cache hot files

* LRU in memory

### 4. Multi-archive support

* mount multiple `.zidx`

---

# Bottom line

This backend turns your system into:

> **A zero-copy, mmap-backed virtual filesystem over ZIP**

with:

* predictable memory usage
* minimal latency
* clean separation of concerns
