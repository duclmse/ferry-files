# AGENTS.md - zipfs Workspace

This document is for AI agents working on this codebase. It describes the
architecture, module contracts, key invariants, and patterns you need to
understand before making changes.

---

## Workspace layout

```
crates/
  common/          # Shared foundation - always build this first
  fs-protocol/     # IPC wire format and client
  fs-server/       # File-search daemon, binary `fsd`
  fs-cli/          # CLI and TUI client, binary `fs`
  fs-ext-rar2zip/  # Archive-transfer daemon extension (feature `rar2zip`)
```

`crates/common` is a library crate shared by the daemon and its extensions. It owns:

- `error.rs` - `CommonError`, `ProjectError` trait
- `logger.rs` - `tracing` init (JSON format, daily rolling file)

---

## fs architecture

### Data flow

```
Filesystem events (notify-debouncer-full, 500ms debounce)
  → indexer/mod.rs   (full crawl on startup, incremental on events)
      ├─ crawler.rs  (rayon parallel walk, reconcile deletes)
      ├─ archive.rs  (ZIP inner-file streaming, virtual paths)
      ├─ content.rs  (binary detect, mmap for > 1MB, encoding_rs)
      └─ exclusions.rs (glob patterns, network mount detection)
  → store/sqlite_store.rs  (file metadata, WAL mode)
  → store/tantivy_store.rs (full-text content, batched commits)
  → query/mod.rs     (execute entry point)
      ├─ parser.rs   (recursive descent tokenizer)
      ├─ evaluator.rs (AST → QueryPlan → SQL / Tantivy)
      ├─ dupes.rs    (two-pass GROUP BY + IN)
      └─ macros.rs   (type macro expansion)
  → api/rest.rs      (Axum, :7700)
  → api/grpc.rs      (Tonic, :7701)
  → api/ipc.rs       (Named Pipe / Unix socket)
  → fs-cli           (CLI + ratatui TUI, over IPC only)
```

### Key invariants

- **Virtual paths** use `!/` as separator: `/path/archive.zip!/inner/file.rs`.
  The `archive_path` column in SQLite stores the containing archive; `path`
  stores the full virtual path.
- **No file list in memory.** Records live in SQLite; the only in-memory cache
  is a `DashMap<String, i64>` capped at 500k entries for path→rowid lookups.
- **Tantivy commits are batched.** Writes go to an in-memory segment; commits
  happen every `commit_interval_secs` (default 5s) and after each bulk crawl.
  Never call `commit()` inside a tight loop.
- **Query plans** are one of four variants - `SqliteOnly`, `TantivyThenSqlite`,
  `DupeSearch`, `FuzzyFilter`. The evaluator selects the plan; don't add
  cross-plan branching inside individual plan arms.
- **`fuzzy:` is a first-class query function**, not a query mode switch. It maps
  to a `FuzzyFilter` plan: SQLite candidates → rayon parallel nucleo scoring.

### SQLite schema (fs)

Key columns in the `files` table:

| Column            | Type      | Notes                                             |
| ----------------- | --------- | ------------------------------------------------- |
| `path`            | TEXT PK   | Full virtual path (includes `!/` for inner files) |
| `name`            | TEXT      | Filename only                                     |
| `size`            | INTEGER   | Bytes                                             |
| `mtime`           | INTEGER   | Unix timestamp                                    |
| `extension`       | TEXT      | Lowercase, no dot                                 |
| `parent`          | TEXT      | Parent directory                                  |
| `archive_path`    | TEXT NULL | Set only for ZIP inner files                      |
| `attributes`      | INTEGER   | 12-bit bitmask                                    |
| `content_indexed` | INTEGER   | 0/1                                               |

Indices exist on: `name`, `extension`, `size`, `mtime`, `parent`,
`archive_path`.

### Adding a new query function

1. Add a `Function` variant in `query/ast.rs`.
2. Parse it in `query/parser.rs` (the tokenizer treats `word:` as a function
   prefix).
3. Map it to a `QueryPlan` arm in `query/evaluator.rs`.
4. Add tests in `query/parser.rs` and `query/evaluator.rs`.
5. Document it in `docs/reference/query-language.md`.

### Adding a new API endpoint

REST lives in `api/rest.rs` (Axum router). gRPC service methods are in
`api/grpc.rs` and must be declared in `proto/fs.proto` first
(`cargo build` regenerates the stubs via `build.rs`).

---

## rar2zip architecture

### Data flow

```
Folder watcher (notify-debouncer-full, 5s min-age gate)
  → intake.rs    (dedup by SHA-256, multi-part detection, strategy selection)
  → queue.rs     (SQLite WAL, job states: pending→converting/transferring→done/failed)
  → app.rs       (process_job dispatch, stale-job recovery on startup, hourly expiry check)
      ├─ convert.rs  (streaming RAR/7Z/TAR → ZIP, 64KB chunks, integrity check first)
      └─ transfer.rs (SSH public-key, SFTP/SCP, token-bucket throttle, remote checksum)
  → metrics.rs   (Prometheus via tiny_http, spawn_blocking)
  → dashboard.rs (inline HTML, 5s JS auto-refresh)
  → alerts.rs    (webhook POST + SMTP email, 5min rate limit)
```

### Key invariants

- **Two strategies.** `Strategy::Disk` creates a ZIP on disk; `Strategy::Stream`
  pipes directly into SFTP. Strategy is selected in `intake.rs` based on file
  size vs `stream_to_transfer_threshold_mb`. Never hard-code a strategy.
- **Source file is the retry anchor for Stream.** Do not delete the source
  archive until the remote checksum passes.
- **Job queue survives crashes.** `converting` and `transferring` jobs are
  re-enqueued as `pending` on startup (see `queue.rs::recover_stale`). If you
  add a new intermediate state, update `recover_stale`.
- **Multi-part RAR groups** are detected by pure string operations (no regex).
  New-style: `archive.part1.rar`, `archive.part2.rar`. Old-style: `archive.rar`,
  `archive.r00`. Groups are complete when all parts are present; incomplete
  groups older than `multipart_timeout_hours` are expired to `failed/`.
- **No passwords in transfer.** SSH uses public-key auth only. Do not add
  password auth paths.
- **Alerts are non-blocking.** `alerts::notify()` is fire-and-forget
  (tokio::spawn). Rate-limited at 5 minutes per alert type. Do not await alert
  results in the hot path.

### Job state machine

```
pending
  → converting   (convert.rs running)
      → transferring (transfer.rs running)
          → done     (remote checksum passed, source deleted)
          → failed   (retry limit exceeded)
      → failed   (conversion error, retry limit exceeded)
  (crash recovery: converting/transferring → pending on startup)
```

### Adding a new archive format

1. Add the extension to `watcher.rs` filter list.
2. Add an `integrity_check_*` function in `convert.rs`.
3. Add a streaming conversion arm in `convert.rs::convert_to_zip`.
4. Add fixtures and tests in `convert.rs` tests.

### Adding a new SFTP destination field

1. Add the field to `config.rs::Destination`.
2. Update `transfer.rs::build_session` to use it.
3. Update the `crates/fs-ext-rar2zip/config.toml` example.

---

## Cross-cutting conventions

### Error handling

- Use `?` everywhere in production paths. No `unwrap()` except in tests and
  const contexts.
- Both crates define a local `Error` enum that wraps `common::CommonError` and
  crate-specific variants.
- Return `Result<T, crate::error::Error>` from all non-trivial functions.

### Async

- Everything runs on Tokio. CPU-bound work (rayon walks, nucleo scoring, archive
  conversion) is spawned via `tokio::task::spawn_blocking`.
- Hold no async locks across `.await` points. Use `tokio::sync::Mutex` only when
  necessary; prefer message-passing.

### Logging

- Use `tracing::{info, warn, error, debug, trace}` macros exclusively. No
  `println!` or `eprintln!` in production code.
- Log at `info` for significant state changes, `debug` for per-file events,
  `trace` for inner loops.

### Config hot-reload

Both crates watch their config files via a secondary `notify` watcher.
Hot-reload is additive-only for watch paths (new paths are added; removed paths
are not un-watched until restart). Do not assume config is static after startup.

### Testing

- Unit tests live in the same file as the code (`#[cfg(test)]` modules).
- Integration tests that need real files use `tempfile::TempDir`.
- No database mocks - tests hit real SQLite (in-memory via `":memory:"`
  connection string).
- Archive tests use fixture files checked into `tests/fixtures/`.

---

## Reference docs

| File                                    | Contents                                                            |
| --------------------------------------- | ------------------------------------------------------------------- |
| `docs/reference/fs-server-modules.md`  | Module-by-module function signatures and contracts                  |
| `docs/reference/query-language.md`      | Complete query language grammar and examples                        |
| `docs/reference/rar2zip-modules.md`     | rar2zip module contracts                                            |
| `docs/reference/zip-inner-streaming.md` | ZIP virtual path spec and archive indexing rules                    |
| `docs/phases/`                          | Phased implementation plan (useful for understanding design intent) |
