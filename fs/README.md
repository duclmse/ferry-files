# fs

A Rust workspace for file search, with an optional archive-transfer extension.

| Crate            | Binary | Purpose                                                                                                     |
| ---------------- | ------ | ----------------------------------------------------------------------------------------------------------- |
| `fs-server`      | `fsd`  | Cross-platform file-search daemon - indexes 1–10M files, exposes REST/gRPC/IPC APIs with < 50ms search latency |
| `fs-cli`         | `fs`   | Command-line and TUI client; talks to `fsd` over IPC                                                        |
| `fs-protocol`    | -      | IPC wire format and client                                                                                  |
| `fs-ext-rar2zip` | `fs-ext-rar2zip` | Drop-in extension - converts RAR/7Z/TAR archives to ZIP and transfers them to SFTP servers                   |
| `common`         | -      | Shared logger, error types, and the extension trait                                                         |

---

## fs

### What it does

`fs` runs as a background daemon that:

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
# fs/config.toml
[daemon]
watch_paths = ["C:/Projects", "D:/Data"]
pid_file    = "fs.pid"

[store]
sqlite_path  = "fs.db"
tantivy_path = "tantivy_index"

[api]
rest_port = 7700
grpc_port = 7701
ipc_path  = "\\\\.\\pipe\\fs"    # Windows named pipe

[indexer]
exclude_paths    = ["**/node_modules", "**/.git"]
index_content    = true
max_content_size = 10_485_760             # 10 MB
index_archives   = true
archive_passwords = []
```

```sh
# Start the daemon
fs run --config config.toml

# Search (CLI)
fs search "*.rs size:>1kb"
fs search "fuzzy:reprot dm:lastweek"

# Force re-index
fs reindex

# Interactive TUI
fs tui
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

Proto at `crates/fs-server/proto/fs.proto`. Reflection enabled -
compatible with `grpcurl`.

---

## rar2zip

### What it does

`rar2zip` is a drop-in `fsd` extension that:

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
# config.toml
[extensions.rar2zip.watcher]
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

```sh
# Validate config without starting
fsd --config config.toml validate-config

# Run the daemon with the extension enabled
fsd --config config.toml daemon

# Check queue status
fs --config config.toml ext rar2zip status

# Retry failed jobs
fs --config config.toml ext rar2zip retry-failed
```

### Dashboard

When metrics are enabled, the dashboard is available at
`http://localhost:9090/dashboard` - queue stats, recent jobs (last 20), and
transfer stats with 5-second auto-refresh.

---

## REST file API

REST listens on `127.0.0.1` by default; set `api.rest_bind` to expose it. Routes
that change files (`/fs/write`, `copy`, `move`, `delete`, `mkdir`) are refused
until a token is configured:

```sh
fsd generate-token   # prints the token, and the digest for api.rest_file_token_hash
curl -X PUT -H "Authorization: Bearer $TOKEN" --data-binary @f \
  "localhost:7700/fs/write?path=/tmp/f"
```

Reading (`/fs/read`, `/file`) needs no token. Browsers may only read responses
from origins listed in `api.rest_cors_origins`, which is empty by default. gRPC
also listens on loopback by default (`api.grpc_bind`).

---

## Extensions

`fsd` loads extensions from the folder named by `extensions_dir`, a top-level
config key. Leaving it unset disables extensions. Each extension is a subfolder
holding an `extension.toml` and its executable:

```text
extensions/rar2zip/extension.toml
extensions/rar2zip/fs-ext-rar2zip
```

```toml
name = "rar2zip"         # must match the folder name
version = "0.1.0"
protocol = 1
executable = "fs-ext-rar2zip"
```

Extensions are discovered at startup, restarted if they crash, and receive the
daemon's config path so they can read their own `[extensions.<name>]` section.
Use `fs ext <name> <command>` to talk to one. The protocol is described in
`crates/common/src/extension_stdio.rs`.

**Extensions run with the daemon's privileges.** On Unix, `fsd` refuses a folder
unless it and its parents are writable only by root or the daemon user, and
refuses symlinks. On Windows it must be inside Program Files. The REST file API
cannot change the folder or the config file.

---

## Building

```sh
# Daemon and client
cargo build --release -p fs-server -p fs-cli

# The rar2zip extension (needs a C++ compiler)
cargo build --release -p fs-ext-rar2zip
```

**Requirements:**

- Rust stable (edition 2021)
- A C++ compiler, for the rar2zip extension's bundled UnRAR source

---

## Testing

```sh
cargo test --workspace
cargo test -p fs-server
cargo test -p fs-ext-rar2zip
```

---

## License

See [LICENSE](LICENSE).
