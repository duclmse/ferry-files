# fs

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
  │ CLI (fs search)   TUI (ratatui)   │
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
cargo build --release -p fs-server
```

The binary is placed at `target/release/fsd` (Linux/macOS) or
`target\release\fsd.exe` (Windows).

---

## 3. Configuration

Create `config.toml` (see `crates/fs-server/config.toml` for a fully-annotated
example):

```toml
watch_paths = ["/home/user/documents", "/home/user/projects"]
data_dir    = "/var/lib/fs"
log_dir     = "/var/log/fs"

[indexer]
crawl_threads        = 4
commit_interval_secs = 5

[api]
rest_port = 7700
grpc_port = 7701
# ipc_path defaults to /tmp/fs.sock (Unix) or \\.\pipe\fs (Windows)

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
# zidx_dir      = "/var/lib/fs/zidx"
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
| `exclusions.skip_network_content`       | `bool`         | `false`          | No content for a watch root on a network mount    |
| `content.extension_limits`              | `{ext: bytes}` | `{}`             | Per-extension content read limit; `"*"` = default |
| `indexer.crawl_threads`                 | `int`          | CPU count (≤ 12) | Directory walker threads                          |
| `indexer.content_threads`               | `int`          | half CPU count   | Crawl threads reading content and archives        |
| `indexer.commit_interval_secs`          | `int`          | `5`              | Tantivy commit interval                           |
| `indexer.use_mft`                       | `bool`/`"auto"` | `"auto"`       | Windows: crawl from the NTFS file table; `auto` for whole-drive roots and roots with ≥ 200,000 entries last crawl |
| `indexer.event_batch_size`              | `int`          | `100`            | Watcher events applied per write transaction      |
| `api.rest_port`                         | `int`          | `7700`           | HTTP REST port                                    |
| `api.grpc_port`                         | `int`          | `7701`           | gRPC port                                         |
| `api.ipc_path`                          | `string`       | platform default | Unix socket or Named Pipe path                    |
| `api.rest_cors_origins`                 | `[string]`     | `[]`             | Allowed CORS origins                              |
| `search.default_limit`                  | `int`          | `100`            | Default result limit                              |
| `search.max_limit`                      | `int`          | `10000`          | Maximum allowed limit                             |
| `search.fuzzy_threshold`                | `float`        | `0.7`            | nucleo fuzzy match threshold (0–1)                |
| `search.max_fuzzy_candidates`           | `int`          | `50000`          | Max candidates scored by fuzzy search             |
| `search.max_filter_candidates`          | `int`          | `200000`         | Max rows fetched when filters run after the fetch |
| `archive_index.enabled`                 | `bool`         | `true`           | Index ZIP inner entries                           |
| `archive_index.formats`                 | `[string]`     | `["zip"]`        | Supported archive formats                         |
| `archive_index.max_entries_per_archive` | `int`          | `10000`          | Entry limit per archive                           |
| `archive_index.max_nesting_depth`       | `int`          | `0`              | Max nested ZIP depth                              |
| `archive_index.passwords`               | `[string]`     | `[]`             | Passwords for encrypted ZIPs                      |
| `macros.<name>`                         | `string`       | _(none)_         | User-defined query macros                         |
| `exclusions.max_content_size_bytes`     | `int`          | `0` (disabled)   | Caps every `content.extension_limits` value       |
| `archive_service.enabled`              | `bool`         | `false`          | Enable `/archive/*` REST routes and .zidx sidecars |
| `archive_service.zidx_dir`             | `string`       | `{data_dir}/zidx`| Root directory for .zidx sidecar files            |
| `archive_service.index_cache_size`     | `int`          | `256`            | Max ZidxIndex instances held open in the LRU cache |

---

## 4. Running the Daemon

```sh
fsd --config /etc/fs/config.toml daemon
```

### Background / Service

**Linux (systemd):**

```ini
[Unit]
Description=fsd daemon
After=network.target

[Service]
ExecStart=/usr/local/bin/fsd --config /etc/fs/config.toml daemon
Restart=on-failure
User=fs

[Install]
WantedBy=multi-user.target
```

**macOS (launchd):**

```xml
<key>ProgramArguments</key>
<array>
  <string>/usr/local/bin/fs</string>
  <string>--config</string>
  <string>/etc/fs/config.toml</string>
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
  fs.pid    PID file written on startup, removed on clean exit (Unix only)
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

When `fsd daemon` is invoked the following steps execute in order inside
`daemon::run_with_shutdown`:

```
 1. Load & validate config.toml
 2. Create data_dir and log_dir if absent
 3. Init tracing (stderr + daily rolling log file)
 4. Acquire single-instance guard
    ├─ Unix:    write {data_dir}/fs.pid
    └─ Windows: CreateMutexW("Global\fs_daemon")
 5. Open stores
    ├─ SqliteStore  → {data_dir}/index.db  (WAL, r2d2 pool)
    └─ TantivyStore → {data_dir}/tantivy/  (exclusive writer lock)
 6. Build ArchiveRegistry (warm from existing .zidx files if archive_service.enabled)
 7. Wrap config in Arc<RwLock<Config>> for shared hot-reload access
 8. Spawn: config hot-reload watcher (notify, 500 ms poll)
 9. Spawn: filesystem watcher → IndexEvent channel (notify-debouncer)
10. Spawn: IPC server  (Unix socket / Named Pipe)
11. Spawn: REST server (Axum, {rest_bind}:{rest_port}, loopback by default)
12. Spawn: gRPC server (Tonic + reflection, {grpc_bind}:{grpc_port}, loopback by default)
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
fs search "main.rs"

# Glob pattern
fs search "*.rs"

# Extension filter
fs search "ext:rs"

# Full-text content search
fs search "content:TODO"

# Fuzzy search
fs search "fuzzy:mian"

# Size filter
fs search "size:tiny"
fs search "size:>10mb"
fs search "size:1kb..100mb"

# Date modified filter
fs search "dm:today"
fs search "dm:last7days"
fs search "dm:2024-01-01..2024-12-31"

# Archive inner files
fs search "archive: *.rs"
fs search "noarchive: *.rs"

# Boolean operators
fs search "main.rs AND ext:rs"
fs search "*.rs OR *.toml"
fs search "NOT hidden: *.tmp"

# Type macros
fs search "type:source"
fs search "type:document"

# Combined
fs search "ext:rs size:>1kb dm:thisweek"

# JSON output
fs search "*.rs" --json
fs search "*.rs" --limit 500 --offset 0
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
  127.0.0.1:7701 fs.FileSearch/Search

# Stats
grpcurl -plaintext 127.0.0.1:7701 fs.FileSearch/Stats

# Reindex
grpcurl -plaintext -d '{"full":false}' \
  127.0.0.1:7701 fs.FileSearch/Reindex
```

### IPC (CLI / TUI)

The IPC channel is for the `fs` CLI subcommands (`search`, `stats`,
`reindex`, `open`). It uses a length-prefixed postcard binary protocol over a
Unix socket (Linux/macOS) or Named Pipe (Windows). Third-party clients should
use the REST or gRPC API instead.

---

## 9. TUI

```sh
fs tui
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

## 10. ZIP Inner-File Indexing

With `[archive_index] enabled = true` (the default), every `.zip` under a watch
path is opened during the crawl, and each entry inside it is indexed as a
virtual file. Nothing is extracted to disk while indexing; entries are read
one at a time.

**Virtual paths.** An inner entry's path is the archive path, then `!/`, then
the entry name:

```
/home/me/src/bundle.zip!/inner/lib.rs
```

The `archive_path` field (REST/gRPC) holds the outer `.zip`. For real files it
is `null`. If you script against the APIs, split on `!/`. Don't treat the
whole string as a filesystem path.

**Querying.**

```sh
fs search "lib.rs"               # real files and inner entries
fs search "archive: *.rs"        # inner entries only
fs search "noarchive: *.rs"      # real files only
fs search "archive: content:fn"  # full-text inside archives
```

Inner entries support name, extension, size, date, `fuzzy:` and `regex:`
queries like real files. Their content is indexed when the entry's extension
has a limit in `[archive_index.inner_extension_limits]`, falling back to
`[content.extension_limits]`. A limit of `0`, or no matching entry, means the
entry is indexed by metadata only.

**Opening.** `POST /open` with an inner virtual path, or `Enter` in the TUI,
extracts that one entry to a temp file named `fs_*_<name>` and opens
it with the OS default handler. The temp file is deleted after 30 seconds.

**Keeping it current.** The watcher treats `.zip` changes as archive events:
- Creating or modifying a ZIP re-indexes all of its entries.
- Deleting or moving it away removes them.

**Limits and caveats.**

| Setting                   | Effect                                                                    |
| ------------------------- | ------------------------------------------------------------------------- |
| `formats`                 | Only `"zip"` is supported; other values fail `validate-config`.           |
| `max_entries_per_archive` | Entries past this count are skipped, with a warning (`0` = unlimited).    |
| `passwords`               | Tried in order for encrypted entries. If none work, the entry is indexed by name only and a warning is logged. |
| `max_nesting_depth`       | How many levels of ZIP-inside-ZIP to index (`0` = none, max `2`). See below. |

**Nested ZIPs.** With `max_nesting_depth = 1`, a `.zip` entry inside an
archive is itself indexed, with paths like
`/data/outer.zip!/inner.zip!/file.txt`. `archive_path` stays the real outer
file, so re-indexing or deleting it updates every level. Each nested ZIP is
read into memory to be opened (it cannot be seeked inside the outer one).
Nested ZIPs over 256 MiB, and encrypted nested ZIPs, are indexed as plain
files and not recursed, with a warning. `POST /open` and the TUI extract nested
entries the same way as first-level ones.

RAR, 7z and TAR archives are not indexed as archives. Use `rar2zip` to convert
them first.

---

## 11. Performance Tuning

**inotify limit (Linux):** The default `max_user_watches` of 8192 is far too low
for large directories. Set it to at least `524288` (see Prerequisites).

**Crawl threads:** Increase `indexer.crawl_threads` for faster initial crawl on
machines with many CPU cores (default is half the available cores).

**Commit interval:** Decrease `indexer.commit_interval_secs` for near-real-time
full-text search at the cost of more frequent disk I/O.

**Fuzzy search candidates:** `search.max_fuzzy_candidates` limits how many rows
the nucleo scorer examines. Lower it if fuzzy searches are slow on very large
indices.

**Post-fetch filters:** `regex:`, `child:`, `startwith:`, `endwith:`, `ww:`
and `|` / `!` over them are checked after rows are fetched. The daemon keeps
fetching larger batches until a full page survives, up to
`search.max_filter_candidates` rows. Lower it if such queries are slow on
very large indices; results may then be short when matches are rare.

---

## 12. Troubleshooting

| Symptom                            | Cause                                | Fix                                                   |
| ---------------------------------- | ------------------------------------ | ----------------------------------------------------- |
| inotify error: too many open files | `max_user_watches` too low           | `sysctl -w fs.inotify.max_user_watches=524288`        |
| `address already in use`           | Port conflict                        | Change `api.rest_port` / `api.grpc_port` in config    |
| `permission denied` on socket      | Stale socket file                    | Delete `/tmp/fs.sock` and restart             |
| gRPC reflection not found          | Old `grpcurl` or reflection disabled | Use `grpcurl -plaintext` and ensure daemon is running |
| Content search returns no results  | Tantivy not committed yet            | Wait for `commit_interval_secs`, or lower it          |
| ZIP inner files not indexed        | `archive_index.enabled = false`      | Set to `true` in config                               |
