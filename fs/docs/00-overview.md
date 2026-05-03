# Project Overview — zipfs

## What This Is

Three crates in a single Cargo workspace:

| Crate        | Purpose                                                   |
| ------------ | --------------------------------------------------------- |
| `common`     | Shared logger init and base error types                   |
| `filesearch` | Cross-platform file-search daemon (Everything-compatible) |
| `rar2zip`    | Windows service: watch → convert archives → SFTP transfer |

---

## Architecture

```
zipfs/
├── Cargo.toml              ← workspace root (resolver = "2")
├── docs/                   ← this folder
└── crates/
    ├── common/             ← shared logger + base errors
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs
    │       ├── error.rs
    │       └── logger.rs
    ├── filesearch/         ← search daemon crate
    │   ├── Cargo.toml
    │   ├── build.rs        ← proto compilation
    │   ├── VERSIONS.md     ← resolved crate versions + API deviations
    │   ├── proto/
    │   └── src/
    └── rar2zip/            ← archive transfer service crate
        ├── Cargo.toml
        ├── VERSIONS.md
        └── src/
```

---

## Key Design Decisions

### Workspace

- **`crates/common`** holds the logger initialiser and `CommonError` so neither
  `filesearch` nor `rar2zip` duplicate tracing setup.
- **Latest stable crate versions** are used at implementation time. Resolved
  versions and any API deviations from the spec are recorded in each crate's
  `VERSIONS.md`.

### filesearch

- **Dual store:** SQLite for metadata (name, size, mtime, extension, path,
  attrib_bitmask), Tantivy for full-text content search.
- **Query language:** voidtools Everything-compatible syntax — same queries
  users already know. Hand-written recursive-descent parser; no
  parser-generator.
- **`fuzzy:<term>`** is a first-class query function (filesearch extension, not
  in Everything). Uses nucleo SIMD scorer against SQLite candidates.
- **`exact.rs`, `fuzzy.rs`, `regex_search.rs` do not exist.** All query logic
  lives in `query/evaluator.rs` and its sub-modules.
- **Query execution:** AST → QueryPlan → SQL + optional Tantivy pass →
  post-filters in Rust for predicates SQLite cannot express.
- **No full file list in memory.** All records live in SQLite; only a bounded
  DashMap path→id cache lives in RAM.
- **Three APIs:** REST (Axum), gRPC (Tonic), IPC (Unix socket / Named Pipe).
- **TUI frontend** built with ratatui + crossterm.

### rar2zip

- **Streaming pipeline:** archives are converted to ZIP entry-by-entry in 64 KB
  chunks — no full extraction to disk.
- **Two transfer strategies:**
  - `Disk`: zip written to disk first, then SFTP'd. Source archive is the only
    local file until transfer completes.
  - `Stream`: source archive → in-memory pipe → SFTP. **No zip file exists on
    disk at any point.** The source archive is deleted only after remote
    checksum verification passes.
- **Durable job queue:** SQLite WAL-mode DB survives crashes; stale jobs are
  re-enqueued on startup.
- **Multi-part timeout:** incomplete multi-part RAR groups are expired after
  `multipart_timeout_hours` (default 24). Parts are moved to `failed/`.
- **Windows Service:** registers with SCM via `windows-service` crate; also
  runnable directly from CLI for development.

---

## Resolved Decisions (from patch-spec.md)

| #   | Decision                                                                                                                            |
| --- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Q1  | Single Cargo workspace with three crates under `crates/`                                                                            |
| Q2  | `exact.rs`, `fuzzy.rs`, `regex_search.rs` do **not** exist; logic is internal to `evaluator.rs`; `fuzzy:` is a first-class function |
| Q3  | Latest stable crate versions; deviations recorded in `VERSIONS.md`                                                                  |
| Q4  | `Strategy::Stream` = source archive piped directly into SFTP — no zip on disk                                                       |
| Q5  | `crates/common` first, then both other crates in parallel phases                                                                    |
| Q6  | `multipart_timeout_hours` config field (default 24h); periodic expiry check in `app.rs`                                             |
