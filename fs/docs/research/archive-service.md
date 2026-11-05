# Archive Service Design Note

This is a research note for a future archive browsing service. It is not the
current `filesearch` API contract. Keep the production specs in
`docs/reference/` authoritative until this design is promoted into a phase plan.

# 1. Core shift: from file server → archive service

### Don’t just serve:

```text
/file.zip
```

### Serve:

```text
/archive/{id}/index
/archive/{id}/entries
/archive/{id}/file
/archive/{id}/range
```

You’re turning ZIP into an **API-backed virtual filesystem**.

---

# 2. Precomputed Central Directory Index (server-side)

## What to add

When a ZIP is first seen:

* Parse central directory once (server-side, native code ideally)
* Build a compact index

### Suggested structure

```c
struct Entry {
  u32 parent;
  u32 nameOffset;
  u32 localHeaderOffset;
  u32 compressedSize;
  u32 uncompressedSize;
  u16 compression;
}
```

Store:

* Binary index file (`.zidx`)
* String pool
* Optional: folder hierarchy

---

## Endpoint

```http
GET /archive/{id}/index
```

Returns:

* Binary (ArrayBuffer-friendly)
* or JSON (dev mode)

---

## Why this is huge

* Browser skips EOCD scan entirely
* No string parsing cost
* No client-side worker needed for indexing
* Instant load even for 100k+ entries

---

# 3. Range-aware file extraction endpoint

## Endpoint

```http
GET /archive/{id}/file?path=dir/file.txt
```

### Behavior

Server:

1. Looks up entry in index
2. Seeks to `localHeaderOffset`
3. Streams decompressed content

---

## Optional modes

### Raw compressed

```http
GET /archive/{id}/file?path=...&raw=1
```

### Partial read

```http
GET /archive/{id}/file?path=...&offset=0&length=65536
```

---

## Why this matters

* Browser no longer needs:

  * decompression logic
  * WASM
  * zip parsing
* You reduce client to **pure UI + streaming consumer**

---

# 4. Directory listing endpoint

Instead of building tree client-side:

```http
GET /archive/{id}/dir?path=/foo/bar
```

Returns:

```json
[
  { "name": "a.txt", "type": "file", "size": 123 },
  { "name": "subdir", "type": "dir" }
]
```

---

## Why

* Avoids sending entire tree
* Enables **lazy navigation**
* Matches your column UI perfectly

---

# 5. Streaming endpoint (critical for large files)

Use chunked transfer:

```http
Transfer-Encoding: chunked
```

Server streams:

```text
[decompress chunk] → [send] → [decompress chunk] → ...
```

---

## Bonus: HTTP Range support on decompressed stream

Advanced but powerful:

```http
Range: bytes=0-65536
```

Requires:

* mapping compressed → uncompressed offsets
* (optional) block index

---

# 6. Block index for fast random access (high-end)

For large files:

* Precompute **deflate block boundaries**
* Store:

```c
struct Block {
  u64 compressedOffset;
  u64 uncompressedOffset;
}
```

---

## Result

You can:

* jump to middle of large file
* support hex viewer without full decompression

---

# 7. Search endpoint using `filesearch`

This project implements an Everything-compatible query language in
`filesearch`; it does not depend on the Windows Everything service.

```http
GET /search?q=*.log&archive=*
```

Return:

* matching archive paths
* matching internal paths

---

## This enables

* cross-archive search
* instant filtering before UI load

---

# 8. Caching layer

## Add:

### 1. Index cache

* Memory-mapped `.zidx`
* LRU eviction

### 2. Decompression cache

* Cache hot files (or chunks)

### 3. HTTP caching

```http
ETag
Cache-Control: private, max-age=3600
```

Use `public` caching only for archives that are explicitly world-readable and
whose authorization does not depend on cookies or tenant context.

---

# 9. Suggested backend stack

Use the existing Rust workspace unless a separate service boundary is
introduced.

### Language

* Rust

---

### Libraries

* libzip → full-featured, stable
* miniz → lightweight, fast

---

# 10. API design summary

```text
GET  /archive/{id}/index        → binary inode table
GET  /archive/{id}/dir          → directory listing
GET  /archive/{id}/file         → decompressed stream
GET  /archive/{id}/file?raw=1   → raw compressed bytes
GET  /archive/{id}/search       → internal search
```

---

# 11. Frontend simplification (massive)

With this backend:

### Remove:

* zip.js
* EOCD parsing
* WASM inflate
* SharedArrayBuffer complexity

### Keep:

* virtualized column UI
* keyboard navigation
* preview renderer

---

# 12. What you end up building

This becomes:

> A **network-backed virtual filesystem for archives**

Comparable to:

* how IDEs browse remote containers
* how OS mounts archive files

---

# 13. If you want to push even further

Now you're entering serious system design:

* FUSE-like mount (ZIP as real filesystem)
* gRPC streaming instead of HTTP
* multi-archive union FS
* content-based deduplication

---

## Bottom line

If backend is under your control:

> Move **all heavy work (indexing, decompression, random access)** to server

and let frontend focus on:

> fast rendering + navigation
