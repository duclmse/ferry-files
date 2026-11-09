# CLAUDE.md - fs

## Build commands

```sh
# Full workspace
cargo build
cargo build --release

# Single crate
cargo build -p fs-server          # binary: fsd
cargo build -p fs-cli             # binary: fs
cargo build -p fs-ext-rar2zip

# Daemon with the rar2zip extension
cargo build -p fs-server --features rar2zip

# Check only (fast)
cargo check --workspace

# Lint (must pass clean)
cargo clippy --workspace -- -D warnings

# Docs
cargo doc --no-deps --workspace
```

All crates build on Windows, Linux and macOS. Only the Windows Service
integration in `fs-server` (`service.rs`, `windows-service`) is Windows-only.
rar2zip runs inside `fsd` when built with the `rar2zip` feature and configured
under `[extensions.rar2zip]`. RAR extraction uses the `unrar` crate, which
compiles the bundled RARLAB UnRAR C++ source, so that feature requires a C++
compiler.

## Test commands

```sh
# All tests
cargo test --workspace

# Single crate
cargo test -p fs-server
cargo test -p fs-cli
cargo test -p fs-protocol
cargo test -p fs-ext-rar2zip
cargo test -p fs-server --features rar2zip
cargo test -p common

# Single module
cargo test -p fs-server query::parser
cargo test -p fs-server store::sqlite_store

# With output (for debugging failing tests)
cargo test -p fs-server -- --nocapture

# Benchmarks (criterion)
cargo bench -p fs-server
```

## Project structure

```
crates/
  common/src/
    error.rs      CommonError, ProjectError trait
    logger.rs     tracing init (JSON, daily rolling)
    extension.rs  Extension trait implemented by daemon extensions
  fs-protocol/src/
    ipc_protocol.rs  wire format; changes must bump PROTOCOL_VERSION
    ipc_client.rs    client side of the IPC channel
  fs-cli/src/
    main.rs       client commands (binary `fs`)
    config.rs     reads only api.ipc_path from the shared config
    tui.rs, tui/  ratatui TUI
  fs-server/src/
    main.rs       daemon and offline commands (binary `fsd`)
    daemon.rs     startup orchestration, graceful shutdown
    config.rs     TOML config, hot-reload
    watcher/      notify-debouncer-full wrappers
    indexer/      crawler, archive (ZIP), content, exclusions
    store/        sqlite_store, tantivy_store
    query/        parser, evaluator, ast, date, size, glob, dupes, macros
    api/          rest (Axum), grpc (Tonic), ipc_server (socket / Named Pipe)
    archive/      streaming ZIP extraction, .dzx random access
    zidx/         .zidx sidecar index
    scheduler/    archive request scheduling and rate limits
    extensions.rs builds extensions listed under [extensions]
  fs-ext-rar2zip/src/
    extension.rs  Extension impl (lifecycle, client commands)
    app.rs        orchestration, process_job
    config.rs     TOML config, Destination struct
    watcher.rs    folder watcher, min-age gate
    intake.rs     dedup, multi-part RAR, strategy selection
    convert.rs    streaming RAR/7Z/TAR → ZIP
    transfer.rs   SSH/SFTP, token-bucket throttle, checksum
    queue.rs      SQLite job queue, WAL mode
    metrics.rs    Prometheus (tiny_http)
    dashboard.rs  auto-refresh HTML
    alerts.rs     webhook + email
```

## Key conventions

**No `unwrap()` in production paths.** Use `?` and return `Result`. `unwrap()`
is only acceptable in tests and `const` contexts.

**No `println!`.** Use `tracing::{info, warn, error, debug, trace}` everywhere.

**No comments that describe what the code does.** Only comment WHY - hidden
constraints, workarounds, non-obvious invariants.

**CPU-bound work goes in `spawn_blocking`.** Rayon walks, archive conversion,
nucleo fuzzy scoring must not block the Tokio runtime.

**No mocks for SQLite.** Tests use real SQLite with `":memory:"` or
`tempfile::TempDir`.

**Tantivy commits are batched.** Never call `commit()` in a loop; it flushes to
disk. The commit loop in `daemon.rs` runs every `commit_interval_secs`.

## Dependency notes

- `notify` / `notify-debouncer-full` - filesystem watcher. Debounce is 500ms in
  fs, 5s min-age gate in rar2zip.
- `tantivy` - full-text index. Schema is defined once in `tantivy_store.rs`; do
  not add fields without migrating existing indices.
- `nucleo` - SIMD fuzzy matcher (used for `fuzzy:` queries). Called from rayon
  threads via `spawn_blocking`.
- `russh` / `russh-sftp` - pure-Rust SSH and SFTP for rar2zip transfers.
  Public-key auth only; no password auth. The server host key is not
  verified yet (`SshHandler::check_server_key` accepts any key).
- `windows-service` - Windows SCM integration. Only compiled on Windows
  (`#[cfg(target_os = "windows")]`).
- `tonic` / `prost` - gRPC. Uses a bundled `protoc`; stubs
  regenerate during `cargo build` via `build.rs`.

## Gotchas

- Virtual ZIP paths use `!/` as separator (`archive.zip!/inner/file`). String
  operations that split paths must handle this separator - do not use
  `std::path::Path` on virtual paths.
- The `archive_path` column is `NULL` for real files and non-null for ZIP inner
  files. Queries that should exclude inner files must filter
  `WHERE archive_path IS NULL`.
- `rar2zip` job recovery runs on startup: any job stuck in `converting` or
  `transferring` is reset to `pending`. If you add a new non-terminal state,
  update `queue::recover_stale`.
- Config hot-reload is additive for watch paths. Removed paths are not
  un-watched until restart.
- The client must stay thin: `fs-cli` depends only on `fs-protocol`, never on
  `fs-server` or an extension crate. New client features go through IPC.
- Tantivy's `IndexWriter` holds a lock file. Only one writer per index
  directory. The daemon enforces a PID file check before acquiring the writer.
