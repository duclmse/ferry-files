# Agent Handoff: Performance and Memory Update Plan

> **Status:** implemented (mark-and-sweep crawl, indexed `parent`, bounded
> fuzzy/content search, multi-destination `Strategy::Disk`, postcard IPC).
> Kept for design history; current behaviour is specified in `docs/reference/`.

Use this document to update both the project documentation and the Rust
implementation. The goal is to keep the documented architecture aligned with
the performance targets in `docs/reference/filesearch-modules.md` while
reducing avoidable memory growth, repeated CPU work, and large intermediate
result sets.

## Scope

Focus on these five review findings:

1. Full-crawl reconciliation can materialize the full indexed path list.
2. `fuzzy:` search can fetch and score too many full `FileRecord`s.
3. Parent/direct-child queries need an indexed `parent` column.
4. Content search can fan out too many Tantivy paths before SQLite filtering.
5. `rar2zip` stream transfer recompresses once per destination.
6. IPC can use a compact binary protocol instead of newline-delimited JSON.
7. Several current implementation details allocate or block more than needed.

Do not change unrelated query semantics, API schemas, or transfer guarantees.
Preserve the existing invariants in `AGENTS.md`, especially:

- no full file list in memory;
- Tantivy commits stay batched;
- `fuzzy:` remains a first-class query function;
- `Strategy::Stream` does not create a ZIP on disk;
- public-key SSH only.

## Documentation Updates

Update these docs before or alongside implementation changes:

- `docs/reference/filesearch-modules.md`
- `docs/reference/query-language.md`
- `docs/phases/02-data-stores.md`
- `docs/phases/03-indexer-and-watcher.md`
- `docs/phases/04-conversion-and-transfer.md`
- `docs/phases/07-testing-and-docs.md`
- `docs/README.md`

Required doc changes:

- Add `parent TEXT` to the files table schema and index it with
  `idx_files_parent ON files(parent)`.
- Add crawl generation fields to the files table, such as `watch_root TEXT` and
  `crawl_generation INTEGER`, or document equivalent metadata that supports
  DB-side stale-row cleanup.
- Replace `paths_under()`-based full-crawl reconciliation with a mark-and-sweep
  design:
  - create a new crawl generation at crawl start;
  - mark each seen row with `watch_root` and `crawl_generation`;
  - delete stale rows in SQL after the crawl;
  - avoid returning all paths to Rust.
- Document a bounded fuzzy plan:
  - fetch a narrow candidate projection, not full records;
  - apply all SQL filters first;
  - cap candidate count by config;
  - keep only top-k scored rows;
  - fetch full records only for final hits.
- Document a bounded content-search plan:
  - push supported filters into Tantivy where possible;
  - page Tantivy hits in bounded batches;
  - avoid one huge path vector or one huge SQLite `IN` clause;
  - preserve limit/offset behavior.
- Document `rar2zip` multi-destination stream behavior:
  - either convert once and fan out to bounded destination writers, or
  - automatically choose `Strategy::Disk` when there is more than one
    destination and fan-out would repeat conversion.
- Add memory assertions to benchmark/release docs:
  - peak RSS during crawl;
  - fuzzy candidate peak memory;
  - content-search path fan-out memory;
  - multi-destination stream CPU/read amplification.
- Replace the IPC docs' schema-less JSON protocol with a typed protocol if
  binary IPC is accepted. Keep REST JSON and gRPC Protobuf unchanged.

## Implementation Tasks

### 1. SQLite Schema and Store

Update `crates/filesearch/src/store/sqlite_store.rs` and shared store types.

Implementation requirements:

- Add `parent`, `watch_root`, and `crawl_generation` fields to the schema.
- Add matching fields to `FileRecord` if records need to carry them.
- Populate `parent` for both filesystem and virtual archive paths.
  - For `/a/b/file.rs`, parent is `/a/b`.
  - For `/a/archive.zip!/src/main.rs`, parent is `/a/archive.zip!/src`.
- Replace `idx_files_parent ON files(path)` with `idx_files_parent ON files(parent)`.
- Add methods for crawl generation cleanup, for example:
  - `begin_crawl_generation() -> i64`
  - `mark_seen(path, watch_root, generation)` or generation-aware upsert
  - `remove_stale_for_root(watch_root, generation)`
- Keep any compatibility methods only if still needed by query execution.

Acceptance checks:

- Direct parent queries use indexed `parent = ?`.
- Full-crawl cleanup does not allocate a vector of all paths.
- Archive entries are removed correctly when an outer ZIP is deleted or changed.

### 2. Crawler Reconciliation

Update `crates/filesearch/src/indexer/crawler.rs`.

Implementation requirements:

- At full-crawl start, create one generation per crawl.
- During crawl, generation-mark every upserted file and archive entry.
- After each watch root finishes, delete rows for that root whose generation is
  older than the active generation.
- Avoid `sqlite.paths_under()` for full-crawl stale detection.
- Keep incremental delete/rename behavior intact.

Acceptance checks:

- Removing a file from disk and running full crawl removes the stale DB row.
- Crawling 1M synthetic rows does not require holding 1M paths in Rust memory.
- Existing archive modified/deleted tests still pass.

### 3. Fuzzy Query Candidate Bounding

Update `crates/filesearch/src/query/evaluator.rs` and SQLite query helpers.

Implementation requirements:

- Add a config field such as `search.max_fuzzy_candidates`.
- Fetch only projected candidate rows needed for scoring: id, name, path, and
  archive marker if required.
- Apply SQL filters before fuzzy scoring.
- Use a bounded top-k heap instead of collecting and sorting every scored hit.
- Fetch full `FileRecord`s only for the final hit ids/paths.
- Return a clear error or degraded result metadata if the candidate cap is hit.

Acceptance checks:

- `fuzzy:config ext:toml` only scores `.toml` candidates.
- `fuzzy:<term>` respects limit/offset.
- A broad fuzzy query does not allocate all matching `FileRecord`s.

### 4. Content Search Batching

Update `crates/filesearch/src/store/tantivy_store.rs` and
`crates/filesearch/src/query/evaluator.rs`.

Implementation requirements:

- Push size, mtime, extension, and archive filters into Tantivy when matching
  fields exist.
- Page Tantivy hits in bounded batches.
- Resolve SQLite metadata in chunks or via a temporary table, not one huge
  path list.
- Stop paging when enough post-filtered hits are collected for limit/offset.

Acceptance checks:

- `content:TODO ext:rs` does not ask SQLite to filter every content hit.
- `archive: content:TODO` still returns only archive entries.
- Broad content queries avoid unbounded path vectors.

### 5. rar2zip Stream Fan-Out

Update `crates/rar2zip/src/intake.rs`, `crates/rar2zip/src/transfer.rs`, and
docs around strategy selection.

Preferred implementation:

- If `destinations.len() > 1`, choose `Strategy::Disk` unless a new config flag
  explicitly allows repeated stream conversion.
- Keep `Strategy::Stream` for single-destination large files.

Alternative implementation:

- Convert once into a bounded broadcast pipeline where all destination writers
  receive the same ZIP byte stream.
- Ensure slow destinations apply backpressure without unbounded buffering.
- Retry semantics must remain correct. If any destination fails, source archive
  remains available until all destinations pass checksum.

Acceptance checks:

- Multi-destination jobs do not decompress/recompress the same archive once per
  destination by default.
- Single-destination stream jobs still create no local ZIP.
- Source archive deletion still happens only after remote checksum success.

### 6. IPC Serialization With `postcard`

Current docs specify newline-delimited JSON over Unix sockets / Windows named
pipes, with request/response payloads shaped as `serde_json::Value`. This is
easy to debug, but it allocates more, parses text, and keeps the protocol
schema loose.

`postcard` is viable for the local IPC channel because the project already uses
Serde-derived Rust types and the daemon/CLI are both Rust programs shipped from
the same workspace. `postcard` has a stable wire format as of 1.0 and is
designed for resource-efficient Serde serialization.

Use `postcard` only for local IPC. Do not replace:

- REST JSON, because it is the human/tool-friendly external HTTP API.
- gRPC Protobuf, because that is already the typed cross-language API.
- rar2zip queue JSON fields, unless there is a separate migration plan.

Protocol design:

```rust
#[derive(Serialize, Deserialize)]
pub struct IpcEnvelope {
    pub magic: [u8; 4],      // b"ZFS1"
    pub version: u16,        // start at 1
    pub request_id: u128,
    pub message: IpcMessage,
}

#[derive(Serialize, Deserialize)]
pub enum IpcMessage {
    Request(IpcRequest),
    Response(IpcResponse),
}

#[derive(Serialize, Deserialize)]
pub enum IpcRequest {
    Search { query: String, limit: u32, offset: u32 },
    Stats,
    Reindex { full: bool },
    Open { path: String },
}

#[derive(Serialize, Deserialize)]
pub enum IpcResponse {
    Search(SearchResponse),
    Stats(StatsResponse),
    Reindex { accepted: bool },
    Open { opened: bool, temp_path: Option<String> },
    Error { code: String, message: String },
}
```

Framing:

- Use length-prefixed frames: `u32 little-endian length` followed by postcard
  bytes.
- Reject frames above a configured cap, for example 16 MiB.
- Never rely on delimiters such as newline when using binary payloads.
- Include `magic` and `version` in every frame or in a connection handshake.
- Keep a fallback `--ipc-format json` or debug subcommand only if useful for
  development.

Cargo changes:

```toml
postcard = { version = "1", features = ["alloc"] }
```

Implementation requirements:

- Add `Serialize`/`Deserialize` to IPC DTOs. Do not expose raw `FileRecord` if a
  smaller `SearchHitDto` is enough.
- Create `api/ipc_protocol.rs` or equivalent for shared daemon/CLI types.
- Keep IPC DTOs stable and versioned. Avoid `serde_json::Value` for the binary
  protocol.
- Add tests for round-trip encode/decode, frame length rejection, version
  mismatch, and error response encoding.

Acceptance checks:

- CLI search works through postcard IPC.
- Binary IPC response for 100 search hits is smaller than JSON in a unit or
  snapshot-style test.
- Invalid frame length and invalid protocol version return controlled errors.

#### Implementation notes (phase 6 - 2026-05-08)

Files created:

- `crates/filesearch/src/api/ipc_protocol.rs` - `IpcEnvelope`, `IpcMessage`,
  `IpcRequest`, `IpcResponse`, `SearchHitDto`, `SearchResponse`, `StatsResponse`,
  `encode_frame`, `decode_payload`, `DecodeError`. Magic `b"ZFS1"`, version 1,
  max frame 16 MiB.
- `crates/filesearch/src/api/ipc_server.rs` - daemon listener. Unix: binds
  `UnixListener`, removes stale socket on startup and shutdown. Windows: named
  pipe via `tokio::net::windows::named_pipe::ServerOptions`. Each connection is
  handled in its own task; requests are dispatched to `search_handler`,
  `stats_handler`, `reindex_handler`, `open_handler`.
- `crates/filesearch/src/api/ipc_client.rs` - CLI client. Connects to the
  daemon socket, sends a single `IpcRequest`, reads back the `IpcResponse`.
  Request IDs are generated from an `AtomicU64` counter cast to `u128`.
- `crates/filesearch/src/api/mod.rs` - re-exports the three sub-modules.

Changes to existing files:

- `Cargo.toml` - added `postcard = { version = "1", features = ["alloc"] }`.
- `src/lib.rs` - added `pub mod api`.
- `src/main.rs` - IPC server spawned inside `run_daemon`; `Command::Search`,
  `Command::Stats`, `Command::Reindex`, `Command::Open` now connect via
  `IpcClient` and print results. `Command::Tui` still deferred to Phase 7.
- `src/indexer/crawler.rs` - moved platform-specific `store::ATTRIB_*` imports
  inside their respective `#[cfg]` blocks to pass `clippy -D warnings` on
  non-Windows targets.

Design decisions:

- `request_id: u128` kept in the wire type for future cross-process correlation;
  the client counter uses `AtomicU64` (stable) cast to `u128`.
- `SearchHitDto` does not carry internal `id` or `tantivy_id` fields - smaller
  on the wire and avoids leaking store internals to the CLI.
- `Reindex` and `Open` handlers accept the request and return an acknowledged
  response; full daemon signalling for reindex is deferred to Phase 7.
- `--ipc-format json` flag not added; JSON fallback can be added if debugging
  need arises (Phase 7 tooling).

Test results: 8 new `api::ipc_protocol::tests::*` tests pass alongside 98
pre-existing tests (106 total, 0 failures).

### 7. Additional Implementation Optimizations

These are lower priority than the five original findings but are visible in the
current code.

SQLite statement reuse:

- `SqliteStore::upsert_file_conn()` prepares the upsert SQL on every file.
  During large crawls this causes avoidable parse/prepare overhead.
- Use a transaction-local prepared statement for batch upserts, or expose a
  `FileBatchWriter` that owns the transaction and prepared statements.

Archive crawler streaming:

- `ArchiveCrawler::crawl()` currently builds `Vec<Result<ArchiveEntry>>` with
  capacity equal to the effective entry count before returning an iterator.
- Replace this with a callback or custom iterator that processes one ZIP entry
  at a time, so large archives do not allocate an entry vector.
- Avoid cloning password and extension-limit maps per crawl. Borrow config or
  precompute `Arc` lookup tables.

Content extraction scheduling:

- Full crawl currently reads small file content directly inside the parallel
  walk closure. That ties filesystem walking to content I/O and Tantivy writes.
- Use a bounded content channel with `content_threads` workers so path walking,
  content extraction, and indexing apply backpressure cleanly.
- Respect `skip_network_content` before reading content.

Tantivy writer lock pressure:

- `TantivyStore::upsert()` locks the writer for every document.
- Batch content documents per worker or route Tantivy updates through a single
  bounded writer task. Keep commits interval-based, not per document.

SQLite pool sizing:

- A fixed pool size of 16 can increase writer contention. SQLite has one writer
  at a time even in WAL mode.
- Use fewer write connections or separate read/write pools. Prefer one batched
  writer path during crawl.

Path and string memory:

- Avoid repeated `to_string_lossy().into_owned()` in tight loops where borrowed
  paths can be normalized once.
- Intern or reuse common watch-root prefixes only if profiling shows path string
  allocation dominates.

Logging:

- Replace production `println!`/`eprintln!` in daemon paths with `tracing`
  macros, preserving CLI user output only for explicit CLI commands.

rar2zip hashing and dedup:

- Intake hashes entire files before enqueueing. That is correct but expensive
  for large archives and duplicates.
- Add a cheap prefilter before SHA-256, such as `(size, filename)` lookup, then
  compute SHA-256 only when necessary. Keep SHA-256 as the authoritative dedup
  key.

rar2zip queue schema:

- `destinations TEXT` stores a JSON array of host strings. For frequent status
  and filtering this is acceptable, but if destination-level retries/status are
  added, split destinations into a child table instead of parsing JSON.

## Tests and Benchmarks

Add or update tests near the implementation files, following the project
convention of in-file unit tests.

Minimum tests:

- SQLite schema migration/idempotent creation includes `parent`,
  `watch_root`, and `crawl_generation`.
- Parent query uses exact direct-child behavior for filesystem and virtual ZIP
  paths.
- Full-crawl stale cleanup removes only stale rows for the current watch root.
- Fuzzy query applies SQL filters before scoring and respects candidate caps.
- Content query processes Tantivy hits in bounded batches.
- Multi-destination `rar2zip` strategy selection avoids repeated stream
  conversion by default.
- IPC protocol round-trips typed postcard request/response frames.
- Archive crawling does not allocate a vector proportional to ZIP entry count.
- Full crawl content indexing uses bounded workers rather than unbounded inline
  reads in walker callbacks.

Benchmark updates:

- Add RSS or allocator-observed peak memory measurements where practical.
- Add a 1M-row synthetic full-crawl reconciliation benchmark.
- Add broad fuzzy and broad content search benchmarks that assert bounded
  candidate/path memory.

## Non-Goals

- Do not add new archive formats.
- Do not change the query language surface.
- Do not add password-based SSH.
- Do not call `tantivy.commit()` in per-file loops.
- Do not introduce an in-memory global file list.

## Final Verification

Run these commands when the implementation is complete:

```sh
cargo test -p filesearch
cargo test -p rar2zip
cargo clippy --workspace -- -D warnings
cargo build --release
```

If `rar2zip` release build is blocked by Windows-only dependencies or local
OpenSSL tooling, document the exact blocker in the final handoff.
