# CLAUDE.md - fs

## Build commands

The fs crates are members of Ferry's workspace (the repo root's `Cargo.toml`,
one `Cargo.lock`, one `target/` at the root). `--workspace` takes in Ferry too,
and a plain `cargo build` builds only Ferry's UI. The root `.cargo/config.toml`
has aliases for the fs crates alone (`fs-build`, `fs-check`, `fs-test`,
`fs-clippy`, `fs-doc`), and its `[env]` holds fs's `LIBSQLITE3_FLAGS`.

```sh
# All fs crates
cargo fs-build
cargo fs-build --release

# Single crate
cargo build -p fs-server          # binary: fsd
cargo build -p fs-cli             # binary: fs
cargo build -p fs-ext-rar2zip     # binary: fs-ext-rar2zip
cargo build -p fs-ext-sftpd       # binary: fs-ext-sftpd

# Check only (fast)
cargo fs-check

# Lint (must pass clean)
cargo fs-clippy -- -D warnings
cargo clippy -p fs-server --all-targets --no-default-features -- -D warnings

# Docs
cargo fs-doc
```

All crates build on Windows, Linux and macOS. `fsd install-service` registers
with the Windows SCM (`windows-service`), systemd or launchd (`service.rs`).
The systemd and launchd code compiles on every OS, so a Windows check covers it.
rar2zip and sftpd are executable extensions that `fsd` discovers under
`extensions_dir` and configures from `[extensions.<name>]`. RAR extraction uses the `unrar`
crate, which compiles the bundled RARLAB UnRAR C++ source, so building
`fs-ext-rar2zip` requires a C++ compiler.

## Test commands

```sh
# All fs tests
cargo fs-test

# Single crate
cargo test -p fs-server
cargo test -p fs-cli
cargo test -p fs-protocol
cargo test -p fs-ext-rar2zip
cargo test -p fs-ext-sftpd
cargo test -p fs-common

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
  fs-common/src/
    error.rs      CommonError, ProjectError trait
    logger.rs     tracing init (JSON, daily rolling)
    extension.rs  Extension trait
    extension_stdio.rs  JSON-lines protocol between fsd and extension executables
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
    service.rs    install-service: Windows SCM, systemd, launchd
    config.rs     TOML config, read once at startup
    config_edit.rs  config get / set / reload for the APIs; edits the file
    watcher/      notify wrapper; live/ native backends (ReadDirectoryChangesW, USN)
    indexer/      crawler, archive (ZIP), content, exclusions
    store/        sqlite_store, tantivy_store
    query/        parser, evaluator, ast, date, size, glob, dupes, macros
    api/          rest (Axum), grpc (Tonic), ipc_server (socket / Named Pipe)
    archive/      streaming ZIP extraction, .dzx random access
    zidx/         .zidx sidecar index
    scheduler/    archive request scheduling and rate limits
    extensions.rs, extensions/  discovery, trust checks, process supervision
    watch_list.rs WatchList: add / remove watch paths for the APIs; edits the config file
  fs-ext-rar2zip/src/
    main.rs       extension executable (`run`, `validate`)
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
    hostkey.rs    SSH host key policy (strict / accept-new / off, pins)
  fs-ext-sftpd/src/
    main.rs       extension executable (`run`, `validate`)
    extension.rs  Extension impl (`status` command)
    config.rs     [extensions.sftpd] section
    server.rs     russh server: public-key auth, sftp subsystem, exec
    sftp.rs       SFTP v3 handler over the jail
    jail.rs       client path -> real path, refuses escapes (.., symlinks)
    exec.rs       no shell: only `sha256sum` and `rm -f`, shell-style quoting
    keys.rs       host key generation, authorized_keys parsing
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

- `notify` / `notify-debouncer-full` - filesystem watcher on Linux and macOS
  (and Windows with `watcher.backend = "notify"`; the default there is
  `watcher/live/rdcw.rs`). Debounce is 500ms in
  fs, 5s min-age gate in rar2zip.
- `tantivy` - full-text index. Schema is defined once in `tantivy_store.rs`; do
  not add fields without migrating existing indices.
- `nucleo` - SIMD fuzzy matcher (used for `fuzzy:` queries). Called from rayon
  threads via `spawn_blocking`.
- `russh` / `russh-sftp` - pure-Rust SSH and SFTP: rar2zip's client and
  sftpd's server. Public-key auth only; no password auth. rar2zip verifies
  server host keys in `hostkey.rs` (default `strict`).
- rar2zip's remote commands (`sha256sum`, `rm -f`) go through `shell_quote`;
  sftpd's `exec.rs` parses the same quoting. Change them together.
- rar2zip's transfer tests (`tests/sftpd.rs`) run against sftpd, a
  dev-dependency only. Extension crates must not depend on each other
  otherwise.
- `windows-service` - Windows SCM integration. Only compiled on Windows
  (`#[cfg(target_os = "windows")]`).
- `tonic` / `prost` - gRPC. Uses a bundled `protoc`; stubs
  regenerate during `cargo build` via `build.rs`.
- fs-server's APIs are cargo features, all on by default: `rest` (axum,
  also the archive routes and `/fs/*`), `grpc` (tonic, protoc) and `ipc`.
  Code for one goes behind `#[cfg(feature = "...")]`; the indexer, stores
  and `ArchiveRegistry` must build with none. CI lints each alone and none.

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
- A system service (`daemon --service`: every Windows service, `--system`
  units elsewhere) opens its IPC socket or pipe to all local users, and
  `ipc_server` refuses requests that act as the daemon's account (`Open`,
  extension commands). A new IPC request that does that must be refused too.
  Watch path changes are allowed from an administrator only
  (`client_is_admin`, `may_change_watch_paths`).
- fsd does not follow its config file. The APIs change it through
  `ConfigEditor` (`config_edit.rs`), which writes the file and swaps the
  shared config; a setting read only at startup goes in its `AT_STARTUP`
  list. Extensions get `HostMessage::Reload` when their section changes.
- `follow_watch_paths` in `daemon.rs` follows changed watch paths: it
  restarts the watcher and crawls only the added roots (`crawl_roots`).
  A removed root stops being watched; its rows stay unless it was removed
  with `forget` (`forget_roots`). No watch paths means no watcher.
  `process_events` drops events outside every watch path. A missing watch
  path is offline, not an error: the watcher follows only present ones,
  `crawl_roots` skips missing ones (a walk would sweep their rows away), and
  one that returns is caught up (`catch_up_roots`).
- `[[watch]]` tables (`config::RootConfig`) join `watch_paths` in `load`,
  so code that walks the roots sees them; look settings up with
  `Config::root_config` / `content_under` / `polled`. Polled roots get no
  watcher; `daemon::Polls` scans them.
- Change-log positions are per watch path (`journal::root_key`); an older
  fsd's shared `journal_position` is split once (`split_shared`).
- Extension executables must keep stdout for the protocol and log to stderr.
- `fsd` runs whatever is in `extensions_dir`. `extensions/trust.rs` and
  `ensure_unprotected` in `api/fs/common.rs` guard it; new file-mutating routes
  must call `ensure_unprotected`.
- New `/fs/*` routes that change the filesystem belong in the token-protected
  `mutating` router in `api/fs.rs`.
- The client must stay thin: `fs-cli` depends only on `fs-protocol`, never on
  `fs-server` or an extension crate. New client features go through IPC.
- SQLite writes go through `SqliteStore::write_conn()`, a single
  connection; reads use `get_conn()`. Never take `write_conn()` while
  holding it, and route new write methods through it.
- A directory rename, move-in or move-out arrives as one watcher event for
  the directory alone. `process_event` walks or removes the subtree; keep
  that when changing event handling. FSEvents may report a deletion as a
  modification of a vanished path.
- Live watcher backends (`watcher.backend`, `watcher/live/`) must report
  lost changes (`Msg::Lost`) rather than drop them; recovery runs the
  timestamp scan. notify's Windows backend drops overflows silently.
- Watcher events are applied by one consumer in arrival order
  (`process_events`), in batches of `indexer.event_batch_size`; do not
  reintroduce concurrent per-event tasks, which reorder a path's events.
- SQLite subtree predicates must stay index ranges (`UNDER_DIR_1`):
  `LIKE` is case-insensitive here and scans the whole table.
- Exclusions that apply to the index come from `ExclusionEngine::for_config`
  or `::shared(&cfg)`, which also keep fsd's own folders (`own_dirs`: data,
  logs, sidecars) out. `ExclusionEngine::new` has the patterns only.
- Walkers come from `indexer::walk_builder`, which keeps git ignore rules
  out of the crawl.
- The default store is the memory store (`store.mode = "auto"`). Every
  `FileStore` method needs a memory arm, and memory store buffers grow by
  an eighth (`state::growth`), not by doubling.
- The memory store's name filters (`memory_store/name_filter.rs`) skip
  blocks during name scans. Anything that gives an entry a name, or revives
  a dead one, must call `NameFilters::changed` for it, as `upsert` does, or
  searches silently miss that entry.
- The file table crawl streams rows to the writer; do not collect a whole
  volume's rows before sending them.
- Entries from `[file_lists]` (`indexer/file_lists.rs`) carry no marker in
  the store; `data_dir/file_lists/` records which paths each list added.
  Code that removes rows outside a watch path must not assume the crawl
  owns them.
- `fs_protocol::attrib` is the one definition of the attribute bitmask and
  its Windows mapping; the store's `ATTRIB_*` names re-export it.
- Tantivy's `IndexWriter` holds a lock file. Only one writer per index
  directory. The daemon enforces a PID file check before acquiring the writer.
