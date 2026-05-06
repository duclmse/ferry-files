# Phase 9 — Archive Service API

**Goal:** Expose a REST API that turns indexed ZIP files into a navigable virtual
filesystem. Clients can list directories, stream individual files, fetch the raw
`.zidx` binary, and search within an archive — without any client-side ZIP
parsing. The archive service routes are mounted on the existing Axum server.

**Estimated scope:** ~800 lines of Rust.

**Depends on:** Phase 6 (Axum REST server), Phase 8 (ZidxIndex builder/reader).

**Design source:** `docs/00/archive service.md`, `docs/00/backend design.md`.

---

## 9.1 New Config Section

Add to `crates/filesearch/src/config.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveServiceConfig {
    /// Enable archive service routes. Default: false.
    pub enabled: bool,

    /// Directory for .zidx sidecar files. Default: "{data_dir}/zidx"
    pub zidx_dir: PathBuf,

    /// Max number of ZidxIndex instances to keep open (LRU). Default: 256.
    pub index_cache_size: usize,
}
```

Add `pub archive_service: ArchiveServiceConfig` to `Config`.

### Validation Rules

- If `enabled = true`: `zidx_dir` must be writable (create on startup).
- `index_cache_size > 0`.

### config.toml Example

```toml
[archive_service]
enabled          = false
zidx_dir         = "./data/zidx"
index_cache_size = 256
```

---

## 9.2 Module Layout

```
crates/filesearch/src/api/
├── archive.rs    ← new: archive service route handlers
└── ...           ← existing REST / gRPC / IPC
```

Add `pub mod archive;` to `api/mod.rs`.

---

## 9.3 Archive Registry

The registry holds all ZIP files that have a valid `.zidx` sidecar and caches
open `ZidxIndex` instances.

```rust
pub struct ArchiveRegistry {
    /// Maps blake3 hex of zip_path → RegistryEntry
    entries: Arc<DashMap<String, RegistryEntry>>,
    /// LRU cache of open ZidxIndex instances (bounded by config.index_cache_size)
    index_cache: Arc<Mutex<LruCache<String, Arc<ZidxIndex>>>>,
    zidx_dir: PathBuf,
}

struct RegistryEntry {
    zip_path:  PathBuf,
    zidx_path: PathBuf,
}

impl ArchiveRegistry {
    pub fn new(zidx_dir: PathBuf, cache_size: usize) -> Self;

    /// Register a ZIP + its .zidx sidecar. Called from crawl_archive() after
    /// ZidxBuilder::build() succeeds.
    pub fn register(&self, zip_path: PathBuf);

    /// Retrieve (or open and cache) the ZidxIndex for a given archive ID.
    /// Returns None if the ID is unknown or the .zidx file is missing/corrupt.
    pub fn index(&self, id: &str) -> Option<Arc<ZidxIndex>>;

    /// Resolve an archive ID to its ZIP path. Returns None if unknown.
    pub fn zip_path(&self, id: &str) -> Option<PathBuf>;

    /// Remove a ZIP from the registry (called when ZIP is deleted from the index).
    pub fn unregister(&self, zip_path: &Path);

    /// List all registered archives.
    pub fn list(&self) -> Vec<ArchiveSummary>;
}

pub struct ArchiveSummary {
    pub id:       String,
    pub zip_path: PathBuf,
}
```

Archive ID is `blake3::hash(zip_path_as_bytes).to_hex()[..24]` (24 hex chars,
96-bit prefix). Stable across restarts as long as the ZIP path doesn't change.
Truncation to 24 chars keeps URLs short while keeping collision probability
negligible for any realistic number of archives.

---

## 9.4 Routes

Mount under `/archive` prefix on the existing Axum router in `daemon.rs`:

```rust
let archive_router = Router::new()
    .route("/archive",                  get(list_archives))
    .route("/archive/:id/index",        get(get_index))
    .route("/archive/:id/dir",          get(list_dir))
    .route("/archive/:id/file",         get(stream_file))
    .route("/archive/:id/search",       get(search_archive))
    .with_state(state.clone());
```

The `ArchiveRegistry` is added to `AppState`:

```rust
pub struct AppState {
    // ... existing fields ...
    pub archive_registry: Arc<ArchiveRegistry>,
}
```

---

## 9.5 Endpoint Specifications

### `GET /archive` — List Archives

Returns all registered archives.

**Response (200):**

```json
[
  { "id": "a1b2c3d4e5f6a1b2c3d4e5f6", "path": "/data/project.zip" },
  { "id": "9f8e7d6c5b4a9f8e7d6c5b4a", "path": "/data/archive.zip" }
]
```

---

### `GET /archive/:id/index` — Binary `.zidx`

Returns the raw `.zidx` binary for efficient client-side mmap.

**Response (200):** `Content-Type: application/octet-stream`, body is the `.zidx`
file bytes served with `tokio::fs::File` via `axum::body::StreamBody`.

**Headers:**
```
Content-Type: application/octet-stream
ETag: <blake3 hex of .zidx file contents, first 16 bytes>
Cache-Control: private, max-age=3600
```

**Errors:**
- `404` — archive ID unknown or `.zidx` file missing.

---

### `GET /archive/:id/dir?node=<u32>` — Directory Listing

**Query params:**
- `node` — node ID from the `.zidx` tree. Defaults to `0` (root).

**Response (200):**

```json
[
  { "id": 1, "name": "src",     "kind": "dir",  "size": 0 },
  { "id": 5, "name": "main.rs", "kind": "file", "size": 4096,
    "compressed_size": 2048, "compression": 8 }
]
```

**Errors:**
- `404` — archive ID unknown.
- `400` — `node` is out of range or not a directory.

**Implementation:** open `ZidxIndex` from registry, call `index.children(node)`,
build response JSON. All work is O(children count) pointer traversal; no disk I/O
beyond the initial mmap.

---

### `GET /archive/:id/file?node=<u32>` — Stream File Content

Decompresses and streams the entry's content to the client.

**Query params:**
- `node` — node ID of the file entry.

**Optional headers:**
- `Range: bytes=N-M` — partial read (requires Phase 11 `.dzx` index; 416 if
  not yet indexed).

**Response (200 or 206):**
```
Content-Type: application/octet-stream  (or inferred from extension)
Content-Length: <uncompressed_size>
Transfer-Encoding: chunked
```

**Implementation:**

The blocking seek-read-inflate path runs in `spawn_blocking`:

1. Read `local_header_offset` from the `.zidx` node.
2. Seek to that offset in the ZIP file.
3. Parse the local file header to find the data start (variable-length filename
   + extra fields).
4. Feed compressed bytes through `flate2::read::DeflateDecoder` (for method=8)
   or pass through directly (method=0, stored).
5. Stream 64 KB output chunks back via a bounded `tokio::sync::mpsc` channel to
   the async Axum response body.

**Errors:**
- `404` — archive ID unknown or node out of range.
- `400` — node is a directory, not a file.
- `500` — ZIP file missing from disk or decompression error.

---

### `GET /archive/:id/search?q=<query>` — Archive-scoped Search

Delegates to the existing `filesearch` query engine with an implicit
`archiveof:<zip_path>` filter added to the parsed AST.

**Query params:**
- `q` — Everything-compatible query string.
- `limit` — Default 100.
- `offset` — Default 0.

**Response:** Same JSON format as `GET /search`.

**Implementation:**

```rust
async fn search_archive(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<SearchParams>,
) -> Result<Json<QueryResult>, StatusCode> {
    let zip_path = state.archive_registry.zip_path(&id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let augmented = format!("archiveof:{} {}", zip_path.display(), params.q);

    let result = query::execute(&augmented, state.sqlite.clone(),
                                state.tantivy.clone(), state.config.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(result))
}
```

---

## 9.6 Integration Points

### Registration on Crawl

In `indexer/crawler.rs`, after `ZidxBuilder::build()` succeeds:

```rust
state.archive_registry.register(archive_path.to_path_buf());
```

### Unregistration on Delete

In `process_event()`, on `ArchiveDeleted(path)`:

```rust
state.archive_registry.unregister(&path);
```

### Startup Population

On daemon startup, after `SqliteStore` is open, scan `zidx_dir` for existing
`.zidx` files and populate the registry from the corresponding `files` table
entries (match by path hash). This ensures the registry is warm after restart
without a full re-crawl.

---

## 9.7 Error Responses

All handlers return JSON errors:

```json
{ "error": "archive not found", "id": "a1b2c3..." }
{ "error": "node out of range", "node": 99999 }
{ "error": "node is a directory" }
```

Use `(StatusCode, Json<ErrorBody>)` as the handler return type.

---

## 9.8 Tests — Phase 9

```
registry:
- register("/a.zip") → id is deterministic on repeated calls
- index(id) → returns Arc<ZidxIndex>
- unregister("/a.zip") → list() no longer contains it
- list() returns all registered archives

handlers (axum::test::TestClient or similar):
- GET /archive → [{"id":..., "path":...}]
- GET /archive/:id/index → 200, Content-Type: application/octet-stream, correct bytes
- GET /archive/unknown/index → 404
- GET /archive/:id/dir?node=0 → 200, root children listed
- GET /archive/:id/dir?node=<dir_node> → 200, correct children
- GET /archive/:id/dir?node=<file_node> → 400
- GET /archive/:id/dir?node=99999 → 400
- GET /archive/:id/file?node=<text_file> → 200, decompressed content matches expected
- GET /archive/:id/file?node=<stored_entry> → 200, pass-through content correct
- GET /archive/:id/file?node=<dir_node> → 400
- GET /archive/:id/search?q=*.rs → returns inner .rs entries from that archive only
- startup population: zidx files present → registry warm without crawl
```

---

## 9.9 Definition of Done

- [ ] `cargo test -p filesearch api::archive` passes.
- [ ] `GET /archive/:id/file` streams a ZIP entry correctly without writing to disk.
- [ ] `GET /archive/:id/index` returns binary `.zidx` with correct `ETag`.
- [ ] `GET /archive/:id/search` honours the `archiveof:` filter.
- [ ] Registry survives daemon restart (warm from existing `.zidx` files on disk).
- [ ] `cargo clippy -- -D warnings` clean.
- [ ] No `unwrap()` in `api/archive.rs`.
- [ ] Archive service routes are **not mounted** when `archive_service.enabled = false`.
