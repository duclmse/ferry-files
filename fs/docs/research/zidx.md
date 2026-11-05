# `.zidx` Builder Design Note

This is a binary-format design note. Treat the snippets as layout sketches, not
portable serialization code. Production code must define byte order, validate
all offsets, and avoid writing Rust struct padding as part of the stable format.

Below is a **full, implementation-grade design** for a `.zidx` builder that produces:

* a **flat, mmap-friendly inode table**
* a **deduplicated string pool**
* **parent / first_child / next_sibling** links (no recursion at runtime)
* stable **node IDs** for your API

The goal is **zero parsing at request time**: just pointer arithmetic over the mmap.

---

# 1) File layout (final)

```text id="c6q1lx"
[Header]
[Node Table]        // fixed-size array
[String Pool]       // UTF-8 bytes, deduplicated
[Optional: Path Index / Hash (future)]
```

---

## 1.1 Header

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: [u8; 4],     // b"ZIDX"
    version: u32,       // 1
    node_count: u32,
    string_bytes: u32,
    root_node: u32,     // usually 0
}
```

All integer fields are little-endian on disk. Readers must reject unknown
versions unless explicit compatibility code exists.

---

## 1.2 Node (inode-style)

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct Node {
    parent: u32,          // index into Node table
    first_child: u32,     // u32::MAX if none
    next_sibling: u32,    // u32::MAX if none

    name_off: u32,        // offset into string pool
    name_len: u16,

    kind: u8,             // 0=file, 1=dir
    _pad: u8,

    compression: u16,
    _pad2: u16,

    compressed_size: u32,
    uncompressed_size: u32,

    local_header_offset: u64, // into ZIP blob
}
```

**Sentinel:** `u32::MAX` = null pointer

---

# 2) Build pipeline (end-to-end)

```text id="x7b2vz"
ZIP → enumerate entries
    → normalize paths
    → build tree (temporary)
    → assign node IDs (stable order)
    → build sibling links
    → build string pool (dedup)
    → emit Node table
    → write file
```

---

# 3) Input: enumerating ZIP entries

Use zip only here (offline).

Extract for each entry:

```rust
struct RawEntry {
    path: String,                // e.g. "a/b/c.txt"
    is_dir: bool,
    compression: u16,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u64,
}
```

---

# 4) Path normalization

Canonicalize aggressively:

```text id="n1n4r0"
- remove leading "/"
- collapse "//"
- remove trailing "/" (except directories)
- UTF-8 normalize if needed (optional)
```

Split:

```rust
fn split(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}
```

---

# 5) Temporary tree (builder structure)

You do NOT build directly into `Node`. Use a mutable graph first.

```rust
use std::collections::BTreeMap;

struct BuildNode {
    name: String,
    kind: Kind,
    children: BTreeMap<String, BuildNode>, // sorted for deterministic output

    // file-only
    compression: u16,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u64,
}

enum Kind { File, Dir }
```

---

## 5.1 Root

```rust
let mut root = BuildNode {
    name: "/".into(),
    kind: Kind::Dir,
    children: BTreeMap::new(),
    ...
};
```

---

## 5.2 Insert entries

```rust
fn insert(root: &mut BuildNode, entry: RawEntry) {
    let parts = split(&entry.path);
    let mut cur = root;

    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;

        cur = cur.children.entry((*part).to_string())
            .or_insert_with(|| BuildNode {
                name: (*part).to_string(),
                kind: if is_last && !entry.is_dir { Kind::File } else { Kind::Dir },
                children: BTreeMap::new(),
                compression: 0,
                compressed_size: 0,
                uncompressed_size: 0,
                local_header_offset: 0,
            });

        if is_last && matches!(cur.kind, Kind::File) {
            cur.compression = entry.compression;
            cur.compressed_size = entry.compressed_size;
            cur.uncompressed_size = entry.uncompressed_size;
            cur.local_header_offset = entry.local_header_offset;
        }
    }
}
```

**Behavior:**

* implicit directories are created automatically
* directory entries in ZIP are optional → we reconstruct them

---

# 6) Node ID assignment (stable, cache-friendly)

We flatten tree → array.

## Strategy: **preorder DFS (parent before children)**

Benefits:

* children are contiguous (good locality)
* simple parent linking

---

## 6.1 Flatten

```rust
struct FlatNode {
    parent: u32,
    name: String,
    kind: Kind,
    children: Vec<usize>, // indices into flat vec (temp)
    // file metadata...
}
```

---

### DFS

```rust
fn flatten(node: &BuildNode, parent: u32, out: &mut Vec<FlatNode>) -> u32 {
    let id = out.len() as u32;

    out.push(FlatNode {
        parent,
        name: node.name.clone(),
        kind: node.kind.clone(),
        children: Vec::new(),
        // copy metadata
    });

    let mut child_ids = Vec::new();

    for child in node.children.values() {
        let cid = flatten(child, id, out);
        child_ids.push(cid as usize);
    }

    out[id as usize].children = child_ids;

    id
}
```

Call:

```rust
let mut flat = Vec::new();
flatten(&root, 0, &mut flat); // root.parent can point to itself or MAX
```

---

# 7) Build sibling + first_child links

Convert `children: Vec<usize>` → linked list.

```rust
let mut nodes = vec![Node::default(); flat.len()];

for (i, f) in flat.iter().enumerate() {
    let first_child = f.children.first().cloned()
        .map(|x| x as u32)
        .unwrap_or(u32::MAX);

    let mut prev: Option<usize> = None;

    for &cid in &f.children {
        if let Some(p) = prev {
            nodes[p].next_sibling = cid as u32;
        }
        prev = Some(cid);
    }

    nodes[i].first_child = first_child;

    if let Some(last) = prev {
        nodes[last].next_sibling = u32::MAX;
    }
}
```

---

# 8) String pool (deduplicated)

## Goal

* avoid repeating filenames
* contiguous memory

---

## 8.1 Builder

```rust
use std::collections::HashMap;

struct StringPool {
    map: HashMap<String, u32>,
    buf: Vec<u8>,
}
```

---

## 8.2 Insert

```rust
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

## 8.3 Populate nodes

```rust
let mut pool = StringPool { map: HashMap::new(), buf: Vec::new() };

for (i, f) in flat.iter().enumerate() {
    let (off, len) = pool.intern(&f.name);

    nodes[i].name_off = off;
    nodes[i].name_len = len;
}
```

---

# 9) Fill remaining node fields

```rust
for (i, f) in flat.iter().enumerate() {
    nodes[i].parent = f.parent;
    nodes[i].kind = match f.kind {
        Kind::File => 0,
        Kind::Dir => 1,
    };

    if matches!(f.kind, Kind::File) {
        nodes[i].compression = f.compression;
        nodes[i].compressed_size = f.compressed_size;
        nodes[i].uncompressed_size = f.uncompressed_size;
        nodes[i].local_header_offset = f.local_header_offset;
    } else {
        nodes[i].compression = 0;
        nodes[i].compressed_size = 0;
        nodes[i].uncompressed_size = 0;
        nodes[i].local_header_offset = 0;
    }
}
```

---

# 10) Write `.zidx` file

```rust
use std::io::Write;

fn write_zidx(path: &str, nodes: &[Node], pool: &StringPool) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;

    let header = Header {
        magic: *b"ZIDX",
        version: 1,
        node_count: nodes.len() as u32,
        string_bytes: pool.buf.len() as u32,
        root_node: 0,
    };

    // Write fields explicitly in little-endian order. Do not write raw structs:
    // padding bytes and native endianness are not a stable file format.
    write_header_le(&mut f, &header)?;
    for node in nodes {
        write_node_le(&mut f, node)?;
    }

    // write string pool
    f.write_all(&pool.buf)?;

    Ok(())
}
```

The helper functions should write each field with `to_le_bytes()`. If this
format later needs zero-copy typed access, use a crate such as `zerocopy` or
`bytemuck` with explicit padding fields and validation rather than raw pointer
casts.

---

# 11) Determinism guarantees

You get stable output if:

* use `BTreeMap` (sorted children)
* deterministic DFS order
* consistent normalization

This matters for:

* caching
* reproducible builds

---

# 12) Complexity

| Stage       | Complexity |
| ----------- | ---------- |
| Insert      | O(n log n) |
| Flatten     | O(n)       |
| Linking     | O(n)       |
| String pool | O(n) avg   |

Memory:

* ~64 bytes per node
* * string pool

---

# 13) Optional upgrades (worth it)

## A. Path hash index

```c
hash(path) → node_id
```

Fast lookup for direct open.

---

## B. Directory span

Add:

```c
u32 child_count;
```

So you can iterate without pointer chasing.

---

## C. Compression of string pool

* front-coding or prefix compression

---

## D. Inline small names

Store short names directly in node (SBO-style)

---

# 14) What you end up with

At runtime:

```text id="y2r7qk"
mmap → pointer to nodes → pointer to strings
```

Operations:

* list dir → follow `first_child` / `next_sibling`
* open file → direct index → no parsing
* no allocations needed

---

# Bottom line

This `.zidx` design gives you:

* **O(1) metadata access**
* **zero parsing cost at runtime**
* **cache-friendly traversal**
* perfect fit for your:

  * scheduler
  * streaming extractor
  * HTTP API

