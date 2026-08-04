# filesearch

A cross-platform file-search daemon with full-text indexing, ZIP inner-file
support, and multiple query interfaces (REST, gRPC, IPC, TUI).

```
┌────────────┐ notify   ┌────────────┐ SQL/Tantivy   ┌──────────┐
│ Filesystem │────────> │ Indexer    │─────────────> │ Stores   │
│ (watched)  │          │ (crawler + │               │ SQLite   │
└────────────┘          │  watcher)  │               │ Tantivy  │
                        └────────────┘               └──────────┘
                                                      │
    ┌─────────────────────────────────┐               │
    │            API Layer            <───────────────┘
    │  REST (Axum)  gRPC (Tonic)  IPC |
    └─────────────────────────────────┘
                        │
  ┌─────────────────────┴─────────────────────┐
  │ CLI (filesearch search)   TUI (ratatui)   │
  └───────────────────────────────────────────┘
```

---

## 1. Prerequisites

| Requirement | Version |
| ----------- | ------- |
| Rust        | ≥ 1.78  |

**Linux:** raise the inotify watch limit before running with large directories:

```sh
sudo sysctl -w fs.inotify.max_user_watches=524288
# Persist across reboots:
echo fs.inotify.max_user_watches=524288 | sudo tee -a /etc/sysctl.conf
```

**macOS:** no per-file watch limit; FSEvents is used automatically.

**Windows:** Named Pipes are used for IPC. No `unrar.dll` is required.

---

## 2. Build

```sh
cargo build --release -p filesearch
```

The binary is placed at `target/release/filesearch` (Linux/macOS) or
`target\release\filesearch.exe` (Windows).

---

## 3. Configuration

Create `config.toml` (see `crates/filesearch/config.toml` for a fully-annotated
example):

```toml
watch_paths = ["/home/user/documents", "/home/user/projects"]
data_dir    = "/var/lib/filesearch"
log_dir     = "/var/log/filesearch"

[indexer]
crawl_threads        = 4
commit_interval_secs = 5

[api]
rest_port = 7700
grpc_port = 7701
# ipc_path defaults to /tmp/filesearch.sock (Unix) or \\.\pipe\filesearch (Windows)

[content]
# Index up to 512 KB of .rs and .py files, 64 KB for everything else.
[content.extension_limits]
rs  = 524288
py  = 524288
"*" = 65536

[archive_index]
enabled = true
formats = ["zip"]
max_entries_per_archive = 10000

[archive_service]
# Enable the /archive/* REST routes and .zidx sidecar generation.
enabled         = false
# zidx_dir defaults to {data_dir}/zidx when absent.
# zidx_dir      = "/var/lib/filesearch/zidx"
index_cache_size = 256
```

### Full config reference

| Field                                   | Type           | Default          | Description                                       |
| --------------------------------------- | -------------- | ---------------- | ------------------------------------------------- |
| `watch_paths`                           | `[string]`     | _(required)_     | Directories to watch and index                    |
| `data_dir`                              | `string`       | _(required)_     | SQLite + Tantivy data directory                   |
| `log_dir`                               | `string`       | _(required)_     | Rolling log files                                 |
| `exclusions.path_patterns`              | `[string]`     | `[]`             | Glob patterns to exclude                          |
| `exclusions.exclude_hidden`             | `bool`         | `false`          | Skip dot-files/directories                        |
| `exclusions.skip_network_content`       | `bool`         | `false`          | Skip content indexing on network mounts           |
| `content.extension_limits`              | `{ext: bytes}` | `{}`             | Per-extension content read limit; `"*"` = default |
| `indexer.crawl_threads`                 | `int`          | half CPU count   | Parallel crawl threads                            |
| `indexer.content_threads`               | `int`          | half CPU count   | Parallel content-extraction threads               |
| `indexer.commit_interval_secs`          | `int`          | `5`              | Tantivy commit interval                           |
| `indexer.event_batch_size`              | `int`          | `100`            | Max watcher events per batch                      |
| `api.rest_port`                         | `int`          | `7700`           | HTTP REST port                                    |
| `api.grpc_port`                         | `int`          | `7701`           | gRPC port                                         |
| `api.ipc_path`                          | `string`       | platform default | Unix socket or Named Pipe path                    |
| `api.rest_cors_origins`                 | `[string]`     | `["*"]`          | Allowed CORS origins                              |
| `search.default_limit`                  | `int`          | `100`            | Default result limit                              |
| `search.max_limit`                      | `int`          | `10000`          | Maximum allowed limit                             |
| `search.fuzzy_threshold`                | `float`        | `0.7`            | nucleo fuzzy match threshold (0–1)                |
| `search.max_fuzzy_candidates`           | `int`          | `50000`          | Max candidates scored by fuzzy search             |
| `archive_index.enabled`                 | `bool`         | `true`           | Index ZIP inner entries                           |
| `archive_index.formats`                 | `[string]`     | `["zip"]`        | Supported archive formats                         |
| `archive_index.max_entries_per_archive` | `int`          | `10000`          | Entry limit per archive                           |
| `archive_index.max_nesting_depth`       | `int`          | `0`              | Max nested ZIP depth                              |
| `archive_index.passwords`               | `[string]`     | `[]`             | Passwords for encrypted ZIPs                      |
| `macros.<name>`                         | `string`       | _(none)_         | User-defined query macros                         |
| `exclusions.max_content_size_bytes`     | `int`          | `0` (disabled)   | Global hard cap on content bytes read per file    |
| `archive_service.enabled`              | `bool`         | `false`          | Enable `/archive/*` REST routes and .zidx sidecars |
| `archive_service.zidx_dir`             | `string`       | `{data_dir}/zidx`| Root directory for .zidx sidecar files            |
| `archive_service.index_cache_size`     | `int`          | `256`            | Max ZidxIndex instances held open in the LRU cache |

---

## 4. Running the Daemon

```sh
filesearch --config /etc/filesearch/config.toml daemon
```

### Background / Service

**Linux (systemd):**

```ini
[Unit]
Description=filesearch daemon
After=network.target

[Service]
ExecStart=/usr/local/bin/filesearch --config /etc/filesearch/config.toml daemon
Restart=on-failure
User=filesearch

[Install]
WantedBy=multi-user.target
```

**macOS (launchd):**

```xml
<key>ProgramArguments</key>
<array>
  <string>/usr/local/bin/filesearch</string>
  <string>--config</string>
  <string>/etc/filesearch/config.toml</string>
  <string>daemon</string>
</array>
```

**Windows (Task Scheduler / NSSM):** Use NSSM to wrap the binary as a Windows
Service, or run via Task Scheduler with "Run whether user is logged on or not".

---

## 5. Data Directory Layout

All persistent state lives under the single `data_dir` set in config.

```
{data_dir}/
  index.db          SQLite - file records, index metadata (WAL mode)
  tantivy/          Tantivy full-text index segments and meta files
  filesearch.pid    PID file written on startup, removed on clean exit (Unix only)
  zidx/             .zidx sidecar files (only when archive_service.enabled = true)
```

**SQLite schema highlights** (`index.db`):

| Table        | Purpose                                                   |
| ------------ | --------------------------------------------------------- |
| `files`      | One row per indexed path; `archive_path IS NULL` for real files, non-null for ZIP inner entries |
| `index_meta` | Key/value store for daemon state (e.g. `last_crawl_time`) |

**Tantivy directory** (`tantivy/`): managed entirely by the Tantivy library. Do not
modify files inside it manually. The daemon holds an exclusive writer lock on this
directory - only one daemon instance may run against the same `data_dir` at a time
(enforced by a PID file on Unix and a named mutex on Windows).

---

## 6. Startup Sequence

When `filesearch daemon` is invoked the following steps execute in order inside
`daemon::run_with_shutdown`:

```
 1. Load & validate config.toml
 2. Create data_dir and log_dir if absent
 3. Init tracing (stderr + daily rolling log file)
 4. Acquire single-instance guard
    ├─ Unix:    write {data_dir}/filesearch.pid
    └─ Windows: CreateMutexW("Global\filesearch_daemon")
 5. Open stores
    ├─ SqliteStore  → {data_dir}/index.db  (WAL, r2d2 pool)
    └─ TantivyStore → {data_dir}/tantivy/  (exclusive writer lock)
 6. Build ArchiveRegistry (warm from existing .zidx files if archive_service.enabled)
 7. Wrap config in Arc<RwLock<Config>> for shared hot-reload access
 8. Spawn: config hot-reload watcher (notify, 500 ms poll)
 9. Spawn: filesystem watcher → IndexEvent channel (notify-debouncer)
10. Spawn: IPC server  (Unix socket / Named Pipe)
11. Spawn: REST server (Axum, 0.0.0.0:{rest_port})
12. Spawn: gRPC server (Tonic + reflection, 0.0.0.0:{grpc_port})
13. Spawn: Tantivy commit loop (every commit_interval_secs)
14. Conditional: initial full crawl (skipped if last_crawl_time present in index_meta)
15. Main event loop - select! on:
    ├─ IndexEvent from watcher  → process_event (semaphore: max 16 concurrent tasks)
    ├─ reindex_rx watch channel → spawn_crawl (deduped via crawl_active AtomicBool)
    └─ shutdown token cancelled → break
16. Graceful shutdown
    ├─ Drain remaining IndexEvents (10 s deadline)
    ├─ Final tantivy commit
    └─ Remove PID file (Unix)
```

**Reindex flow**: any API (REST `POST /reindex`, gRPC `Reindex`, IPC) sends on the
`reindex_tx` watch channel. The main loop picks it up, checks the `crawl_active`
flag, and calls `spawn_crawl` if no crawl is already running.

**Config hot-reload**: the watcher re-parses the TOML and validates it on every
`Modify` event. Invalid configs are logged and discarded; the running config is kept.
Watch-path additions take effect on the next crawl; removals require a restart.

---

## 7. Searching

### CLI

```sh
# Basic name search
filesearch search "main.rs"

# Glob pattern
filesearch search "*.rs"

# Extension filter
filesearch search "ext:rs"

# Full-text content search
filesearch search "content:TODO"

# Fuzzy search
filesearch search "fuzzy:mian"

# Size filter
filesearch search "size:tiny"
filesearch search "size:>10mb"
filesearch search "size:1kb..100mb"

# Date modified filter
filesearch search "dm:today"
filesearch search "dm:last7days"
filesearch search "dm:2024-01-01..2024-12-31"

# Archive inner files
filesearch search "archive: *.rs"
filesearch search "noarchive: *.rs"

# Boolean operators
filesearch search "main.rs AND ext:rs"
filesearch search "*.rs OR *.toml"
filesearch search "NOT hidden: *.tmp"

# Type macros
filesearch search "type:source"
filesearch search "type:document"

# Combined
filesearch search "ext:rs size:>1kb dm:thisweek"

# JSON output
filesearch search "*.rs" --json
filesearch search "*.rs" --limit 500 --offset 0
```

### Full Query Syntax

| Syntax                      | Description                                                                          |
| --------------------------- | ------------------------------------------------------------------------------------ |
| `word`                      | Glob match on filename (`*word*`)                                                    |
| `*.ext`                     | Glob pattern on filename                                                             |
| `ext:rs`                    | Extension filter                                                                     |
| `dir:src`                   | Parent directory name filter                                                         |
| `content:word`              | Full-text search (Tantivy)                                                           |
| `fuzzy:word`                | Fuzzy filename match (nucleo)                                                        |
| `size:tiny`                 | Size named constant                                                                  |
| `size:>10mb`                | Size comparison                                                                      |
| `size:1kb..10mb`            | Size range                                                                           |
| `dm:today`                  | Date modified - named constant                                                       |
| `dm:2024-01-15`             | Date modified - absolute                                                             |
| `dm:2024-01-01..2024-12-31` | Date modified - range                                                                |
| `dc:thisweek`               | Date created                                                                         |
| `archive: <query>`          | Only inner-archive entries                                                           |
| `noarchive: <query>`        | Exclude inner-archive entries                                                        |
| `archiveof:/path/to.zip`    | All entries inside a specific ZIP                                                    |
| `type:audio`                | File-type macro (audio, video, picture, document, archive, executable, source, font) |
| `hidden: <query>`           | Hidden files only                                                                    |
| `A AND B`                   | Both predicates must match                                                           |
| `A OR B`                    | Either predicate matches                                                             |
| `NOT A`                     | Negation                                                                             |
| `(A OR B) AND C`            | Grouping                                                                             |

### Size constants

`empty` (0 B), `tiny` (< 10 KB), `small` (< 1 MB), `medium` (< 128 MB), `large`
(≥ 128 MB), `huge` (≥ 1 GB)

### Date constants

`today`, `yesterday`, `thisweek`, `lastweek`, `thismonth`, `lastmonth`,
`thisyear`, `lastyear`, `last7days`, `last30days`, `last90days`, `last365days`,
weekday names (`monday`…`sunday`), month names (`january`…`december`)

---

## 8. APIs

### REST

Base URL: `http://127.0.0.1:7700`

| Method | Path                        | Description                            |
| ------ | --------------------------- | -------------------------------------- |
| `GET`  | `/search?q=&limit=&offset=` | Search                                 |
| `GET`  | `/stats`                    | Index statistics                       |
| `POST` | `/reindex` `{"full":bool}`  | Trigger re-index                       |
| `GET`  | `/health`                   | Health check                           |
| `GET`  | `/file?path=`               | File metadata                          |
| `POST` | `/open` `{"path":"..."}`    | Open file (extracts ZIP entry to temp) |

### gRPC

```sh
# List services (reflection must be enabled in config)
grpcurl -plaintext 127.0.0.1:7701 list

# Search
grpcurl -plaintext -d '{"query":"*.rs","limit":50}' \
  127.0.0.1:7701 filesearch.FileSearch/Search

# Stats
grpcurl -plaintext 127.0.0.1:7701 filesearch.FileSearch/Stats

# Reindex
grpcurl -plaintext -d '{"full":false}' \
  127.0.0.1:7701 filesearch.FileSearch/Reindex
```

### IPC (CLI / TUI)

The IPC channel is for the `filesearch` CLI subcommands (`search`, `stats`,
`reindex`, `open`). It uses a length-prefixed postcard binary protocol over a
Unix socket (Linux/macOS) or Named Pipe (Windows). Third-party clients should
use the REST or gRPC API instead.

---

## 9. TUI

```sh
filesearch tui
```

| Key              | Action                                       |
| ---------------- | -------------------------------------------- |
| Type             | Filter results (500 ms debounce)             |
| `↑` / `↓`        | Navigate results                             |
| `Enter`          | Open selected file                           |
| `Tab`            | Toggle search mode (Exact / Fuzzy / Content) |
| `Esc` / `Ctrl-C` | Quit                                         |

The TUI shows:

- Input bar (top)
- Result list with `[zip]` tag for inner-archive entries (middle)
- Status bar: hit count, search time, mode, index stats (bottom)

---

## 10. Performance Tuning

**inotify limit (Linux):** The default `max_user_watches` of 8192 is far too low
for large directories. Set it to at least `524288` (see Prerequisites).

**Crawl threads:** Increase `indexer.crawl_threads` for faster initial crawl on
machines with many CPU cores (default is half the available cores).

**Commit interval:** Decrease `indexer.commit_interval_secs` for near-real-time
full-text search at the cost of more frequent disk I/O.

**Fuzzy search candidates:** `search.max_fuzzy_candidates` limits how many rows
the nucleo scorer examines. Lower it if fuzzy searches are slow on very large
indices.

---

## 11. Troubleshooting

| Symptom                            | Cause                                | Fix                                                   |
| ---------------------------------- | ------------------------------------ | ----------------------------------------------------- |
| inotify error: too many open files | `max_user_watches` too low           | `sysctl -w fs.inotify.max_user_watches=524288`        |
| `address already in use`           | Port conflict                        | Change `api.rest_port` / `api.grpc_port` in config    |
| `permission denied` on socket      | Stale socket file                    | Delete `/tmp/filesearch.sock` and restart             |
| gRPC reflection not found          | Old `grpcurl` or reflection disabled | Use `grpcurl -plaintext` and ensure daemon is running |
| Content search returns no results  | Tantivy not committed yet            | Wait for `commit_interval_secs`, or lower it          |
| ZIP inner files not indexed        | `archive_index.enabled = false`      | Set to `true` in config                               |
