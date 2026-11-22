# Development Guide

How to build, run, test, benchmark and change this workspace. For what the code
does, read the [overview](overview.md) and the [reference docs](reference/); for
the rules every change must follow, read [`CLAUDE.md`](../CLAUDE.md)
(conventions and gotchas) and [`AGENTS.md`](../AGENTS.md) (architecture and
invariants).

## 1. Toolchain

| Need                                                                        | Why                                                               |
| --------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| Rust stable (1.94 or newer), with `clippy` and `rustfmt`                    | CI runs `cargo fmt --check` and `clippy -D warnings`              |
| A C/C++ compiler (MSVC on Windows, `build-essential` / Xcode CLT elsewhere) | bundled SQLite (`rusqlite`), RARLAB UnRAR (`unrar`, rar2zip only) |
| `curl` (bash scripts), PowerShell 5.1+ (Windows scripts)                    | benchmark and helper scripts                                      |
| `sqlite3` on `PATH` (optional)                                              | `scripts/db.sh` / `db.ps1`                                        |

No system `protoc` is needed: `fs-server/build.rs` uses a bundled one.

`.cargo/config.toml` sets `LIBSQLITE3_FLAGS=-DSQLITE_DEFAULT_MEMSTATUS=0` for
the bundled SQLite. With SQLite's default, every allocation takes one global
mutex, and concurrent searches slowed each other down (see
[BENCHMARKS.md](BENCHMARKS.md#findings)). Keep it when you change the
`rusqlite` version.

## 2. Workspace map

```
crates/
  common/          logger, error types, Extension trait, extension stdio protocol
  fs-protocol/     IPC wire format + client (bump PROTOCOL_VERSION on change)
  fs-server/       daemon `fsd`: watcher, indexer, stores, query engine, APIs
  fs-cli/          client `fs` (CLI + TUI); depends on fs-protocol only
  fs-ext-rar2zip/  extension: RAR/7Z/TAR -> ZIP, SFTP upload
  fs-ext-sftpd/    extension: public-key SFTP server over one folder
```

Dependency direction is strict: `fs-cli -> fs-protocol -> common`,
`fs-server -> {fs-protocol, common}`, extensions `-> common`. Extensions never
depend on each other (sftpd is only a dev-dependency of rar2zip's tests), and
the client never depends on the server.

## 3. Build

```sh
cargo check --workspace                     # fastest feedback
cargo build                                 # debug, whole workspace
cargo build --release -p fs-server -p fs-cli   # or scripts/build.sh / build.ps1
cargo build --release --workspace           # also both extensions
```

Release binaries land in `target/release/` (`fsd`, `fs`, `fs-ext-rar2zip`,
`fs-ext-sftpd`, plus `.exe` on Windows). The release profile is `opt-level = 3`
and `strip = true`.

## 4. Run locally

1. Copy `crates/fs-server/config.toml` somewhere and set `watch_paths`,
   `data_dir` and `log_dir`. Use a small tree while developing.
2. Validate, then run in the foreground:

   ```sh
   target/release/fsd --config my.toml validate-config
   target/release/fsd --config my.toml daemon     # or scripts/run.sh
   ```

3. Talk to it:

   ```sh
   target/release/fs search "*.rs size:>1kb"
   target/release/fs stats
   target/release/fs tui
   curl "http://127.0.0.1:7700/search?q=readme&limit=10"
   grpcurl -plaintext 127.0.0.1:7701 list          # reflection is on
   ```

Logs are JSON, daily-rolling, under `log_dir`; set `RUST_LOG=debug` (or
`RUST_LOG=fs_server::indexer=trace`) for more. Stop with Ctrl+C: that is the
clean shutdown, and the memory store only writes `index.bin` on it (or every
`store.snapshot_interval_secs`).

Useful settings while developing:

| Setting                                             | Effect                                                   |
| --------------------------------------------------- | -------------------------------------------------------- |
| `[store] mode = "memory"`                           | in-memory store + `index.bin` snapshot instead of SQLite |
| `[archive_index] enabled = false`                   | skip ZIP inner files                                     |
| `[content.extension_limits]` empty                  | no full-text extraction; crawl measures metadata only    |
| `[api] grpc_enabled = false`, `ipc_enabled = false` | fewer ports to collide with                              |
| `[indexer] use_mft = false`                         | Windows: force the directory walk (default `"auto"`)     |

To try an extension, build it, copy the executable and its `extension.toml` into
the folder named by `extensions_dir`, and add an `[extensions.<name>]` section.
`fsd` refuses an `extensions_dir` that others can write to
(`extensions/trust.rs`). Extensions talk JSON lines on stdout, so they must log
to stderr only.

Inspect an index database with `scripts/db.sh stats` (read-only; safe beside a
running daemon). Build or list `.zidx` sidecars with `fsd zidx build|ls`.

## 5. Tests

```sh
cargo test --workspace                     # what CI runs
cargo test -p fs-server query::parser      # one module
cargo test -p fs-server -- --nocapture     # see output
cargo test -p fs-ext-rar2zip --test sftpd  # rar2zip transfer against a real sftpd
```

- SQLite is never mocked: use `":memory:"` or a `tempfile::TempDir`.
- `crates/fs-server/tests/integration.rs` starts real stores and APIs.
- `fs-ext-rar2zip --features integration --test integration` needs a live SFTP
  server; see the env vars at the top of that file. Without them it skips.
- Watcher tests are timing-sensitive (500 ms debounce in fsd, 5 s min-age in
  rar2zip). Wait for the event, don't sleep a fixed short time.

## 6. Before you push

Exactly what CI (`.github/workflows/ci.yml`) runs on Ubuntu, macOS and Windows:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p fs-ext-rar2zip --all-targets --features integration -- -D warnings
cargo test --workspace
cargo build --release --workspace
```

The Windows job additionally runs `scripts/bench-windows.ps1` against
`C:\Windows\System32` with and without `-UseMft`, and installs, starts, queries
and stops the Windows Service.

## 7. Change checklists

| You are changing                           | Also do                                                                              |
| ------------------------------------------ | ------------------------------------------------------------------------------------ |
| `fs-protocol/src/ipc_protocol.rs`          | bump `PROTOCOL_VERSION`; client and daemon must match                                |
| Tantivy schema (`tantivy_store.rs`)        | migrate existing indices; never just add a field                                     |
| `index.bin` layout (`store/memory_store/`) | bump the header version; update [index-bin-format.md](reference/index-bin-format.md) |
| rar2zip job states                         | update `queue::recover_stale`                                                        |
| rar2zip `shell_quote` / sftpd `exec.rs`    | change both together                                                                 |
| a new `/fs/*` route that writes            | put it in the `mutating` router; call `ensure_unprotected`                           |
| a dependency version                       | update the crate's `VERSIONS.md`                                                     |
| watcher event handling                     | keep subtree walk/remove for directory renames and moves                             |
| SQLite writes                              | go through `SqliteStore::write_conn()`; never nest it                                |
| code on virtual paths (`a.zip!/x`)         | split on `!/` by hand, not with `std::path::Path`                                    |

Code rules (enforced in review): no `unwrap()` outside tests and `const`; no
`println!` (use `tracing`); CPU-bound work in `spawn_blocking`; Tantivy
`commit()` only from the commit loop; comments explain _why_, not _what_.

## 8. Benchmarks

### Micro-benchmarks (criterion)

```sh
cargo bench -p fs-server                         # all of them, slow
cargo bench -p fs-server --bench memory_search   # 2M-entry memory store
cargo bench -p fs-server --bench reconcile       # 1M-row SQLite re-crawl
cargo bench -p fs-server --bench search -- --quick
```

| Bench                                         | Measures                                                                             |
| --------------------------------------------- | ------------------------------------------------------------------------------------ |
| `search`                                      | SQLite-store query latency                                                           |
| `memory_search`                               | memory-store queries, REST/IPC overhead, `index.bin` save/load and size (2M entries) |
| `reconcile`                                   | full re-crawl over 1M existing rows (peak RSS printed on Unix)                       |
| `archive`, `archive_inflate`, `archive_range` | ZIP streaming, SIMD inflate, `.dzx` range reads                                      |
| `scheduler_load`                              | archive request scheduler under contention                                           |

Reports go to `target/criterion/`. Compare against a baseline with
`--save-baseline before` then `--baseline before`.

### End-to-end (real daemon, real tree)

```sh
scripts/bench.sh [DIR] [--with-content]          # crawl throughput only
scripts/bench-query.sh <kept-data-dir>           # query latency on a kept index
```

On Windows, `scriptsench.ps1 -KeepDb` and `scriptsench-query.ps1
<kept-data-dir>` do the same. `bench.ps1` builds a SQLite index unless you pass
`-Store memory`: it hard-kills the daemon, so a kept memory index would be
empty.

```powershell
# Crawl, idle, query latency, 8-client load, clean stop, offline changes,
# restart catch-up, all while sampling CPU, working set and private bytes;
# ends with the size of every file in the data dir (index.db, index.bin, tantivy).
.\scripts\bench-windows.ps1 -TargetDir C:\Windows\System32 -Store sqlite
.\scripts\bench-windows.ps1 -TargetDir C:\Windows\System32 -Store memory
.\scripts\bench-windows.ps1 -TargetDir C:\ -UseMft -NoContent   # elevated
```

Results go to `target\bench-windows\<timestamp>\` (`summary.txt`, `samples.csv`,
`queries.csv`, `phases.csv`). Run from an elevated prompt to exercise
USN-journal catch-up and the file table crawl; otherwise the timestamp scan and
the walk run. Start it from PowerShell or Windows Terminal, not Git Bash: a
daemon launched under Git Bash ignores Ctrl+C, so the clean stop turns into a
kill and the memory store never writes `index.bin`. Latest numbers are in
[BENCHMARKS.md](BENCHMARKS.md).

## 9. Helper scripts

| Script (`.sh` / `.ps1`)                 | Does                                                                    |
| --------------------------------------- | ----------------------------------------------------------------------- |
| `build`                                 | release build of `fsd` + `fs`; extra args go to cargo                   |
| `run`                                   | run `fsd daemon` in the foreground with a discovered config             |
| `install`                               | install binaries, config, and a systemd/launchd/Windows service         |
| `db`                                    | read-only SQLite inspection: `tables`, `ddl`, `stats`, `query`, `watch` |
| `worktree`                              | sibling git worktrees under `../<repo>-worktrees/`                      |
| `bench`, `bench-query`, `bench-windows` | see section 8                                                           |
| `clean`                                 | dev-machine disk cleanup (not repo-specific; read it before running)    |

## 10. Docs

`docs/reference/` is authoritative; `phases/`, `plans/` and `research/` are
history. When behaviour changes, update the reference doc in the same commit,
and the status table in [docs/README.md](README.md) when a feature lands.
