# zipfs

A Rust workspace containing two production-grade tools for file management and
archive processing.

| Crate        | Binary       | Purpose                                                                                                            |
| ------------ | ------------ | ------------------------------------------------------------------------------------------------------------------ |
| `filesearch` | `filesearch` | Cross-platform file-search daemon - indexes 1–10M files, exposes REST/gRPC/IPC APIs with < 50ms search latency     |
| `rar2zip`    | `rar2zip`    | Windows service - watches a folder, converts RAR/7Z/TAR archives to ZIP, and transfers them to remote SFTP servers |
| `common`     | -            | Shared logger, error types, and constants                                                                          |

---

## filesearch

### What it does

`filesearch` runs as a background daemon that:

1. Walks configured watch paths and indexes all files into SQLite (metadata) and
   Tantivy (full-text content).
2. Maintains the index incrementally via filesystem watchers (< 1s event
   latency).
3. Indexes inner files inside ZIP archives as virtual paths
   (`/path/to/archive.zip!/inner/file.txt`).
4. Exposes search over three APIs: REST (Axum), gRPC (Tonic), and IPC (Unix
   socket / Named Pipe).
5. Provides a CLI and a ratatui TUI for interactive use.

### Query language

Implements the full
[voidtools Everything](https://www.voidtools.com/support/everything/searching/)
syntax plus three extensions:

```
# Boolean operators
report AND (pdf OR docx)
report OR summary NOT draft

# Wildcards
*.log
src/*.rs

# Size / date filters
size:>10mb dm:today
size:1kb..500kb dm:lastweek

# Type macros
audio: video: doc: zip: folder:

# Duplicate search
dupe:name  dupe:size  dupe:date

# Extensions (not in Everything)
fuzzy:reprot          # typo-tolerant SIMD matching
archive:*.rs          # scope to ZIP inner files only
archiveof:/path/a.zip # all entries inside a specific archive
```

### Quick start

```toml
# filesearch/config.toml
[daemon]
watch_paths = ["C:/Projects", "D:/Data"]
pid_file    = "filesearch.pid"

[store]
sqlite_path  = "filesearch.db"
tantivy_path = "tantivy_index"

[api]
rest_port = 7700
grpc_port = 7701
ipc_path  = "\\\\.\\pipe\\filesearch"    # Windows named pipe

[indexer]
exclude_paths    = ["**/node_modules", "**/.git"]
index_content    = true
max_content_size = 10_485_760             # 10 MB
index_archives   = true
archive_passwords = []
```

```sh
# Start the daemon
filesearch run --config config.toml

# Search (CLI)
filesearch search "*.rs size:>1kb"
filesearch search "fuzzy:reprot dm:lastweek"

# Force re-index
filesearch reindex

# Interactive TUI
filesearch tui
```

### REST API

```
GET  /search?q=<query>&limit=100   → JSON array of file records
GET  /stats                        → index statistics
POST /reindex                      → trigger full re-crawl
POST /open?path=<virtual_path>     → extract inner ZIP entry to temp dir
GET  /health
```

### gRPC

Proto at `crates/filesearch/proto/filesearch.proto`. Reflection enabled -
compatible with `grpcurl`.

---

## rar2zip

### What it does

`rar2zip` runs as a Windows Service that:

1. Watches an input folder for RAR, 7Z, TAR, TAR.GZ, and TAR.BZ2 archives.
2. Verifies archive integrity before processing.
3. Converts archives to ZIP via streaming (entry-by-entry, 64 KB chunks - never
   loads the full file).
4. Transfers the resulting ZIP to one or more remote SFTP servers.
5. Handles multi-part RAR groups (both `.part1.rar` and legacy `.r00`/`.r01`
   styles).
6. Exposes Prometheus metrics and an auto-refreshing dashboard.

Transfer **strategy** is selected automatically by file size:

| Strategy | Description                                                                        |
| -------- | ---------------------------------------------------------------------------------- |
| `Disk`   | Convert → ZIP on disk → transfer → delete                                          |
| `Stream` | Pipe archive directly into SFTP (no ZIP on disk). Source file is the retry anchor. |

### Quick start

```toml
# rar2zip/config.toml
[watcher]
watch_path  = "C:/Incoming"
min_file_age_secs = 5

[queue]
db_path     = "queue.db"
retry_limit = 5

[transfer]
stream_to_transfer_threshold_mb = 500
max_parallel_transfers = 3

[[destinations]]
host       = "sftp.example.com"
port       = 22
user       = "deploy"
key_path   = "C:/keys/id_ed25519"
remote_dir = "/uploads"

[metrics]
enabled = true
port    = 9090
```

```powershell
# Install as Windows Service
.\install-service.ps1

# Or run directly (development)
rar2zip run --config config.toml

# Check queue status
rar2zip status

# Retry failed jobs
rar2zip retry-failed

# Validate config without starting
rar2zip validate-config --config config.toml
```

### Dashboard

When metrics are enabled, the dashboard is available at
`http://localhost:9090/dashboard` - queue stats, recent jobs (last 20), and
transfer stats with 5-second auto-refresh.

---

## Building

```sh
# All crates
cargo build --release

# Single crate
cargo build --release -p filesearch
cargo build --release -p rar2zip

# rar2zip is Windows-only (Windows Service APIs)
# filesearch is cross-platform (Linux / macOS / Windows)
```

**Requirements:**

- Rust stable (edition 2021)
- Windows SDK (rar2zip)
- libssh2 / OpenSSL (rar2zip SFTP)
- Protocol Buffers compiler `protoc` (filesearch gRPC)

---

## Testing

```sh
cargo test --workspace
cargo test -p filesearch
cargo test -p rar2zip
```

---

## License

See [LICENSE](LICENSE).
