# fs

A Rust workspace for file search, with optional archive-transfer and SFTP server extensions.

| Crate            | Binary | Purpose                                                                                                     |
| ---------------- | ------ | ----------------------------------------------------------------------------------------------------------- |
| `fs-server`      | `fsd`  | Cross-platform file-search daemon - indexes 1–10M files, exposes REST/gRPC/IPC APIs with < 50ms search latency |
| `fs-cli`         | `fs`   | Command-line and TUI client; talks to `fsd` over IPC                                                        |
| `fs-protocol`    | -      | IPC wire format and client                                                                                  |
| `fs-ext-rar2zip` | `fs-ext-rar2zip` | Drop-in extension - converts RAR/7Z/TAR archives to ZIP and transfers them to SFTP servers                   |
| `fs-ext-sftpd`   | `fs-ext-sftpd`   | Drop-in extension - serves one folder over SFTP, public-key login only                                       |
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
6. Reads and writes Everything file lists (`.efu`), so catalogues of offline
   drives and other machines are searchable next to local files.

### Query language

Implements the full
[voidtools Everything](https://www.voidtools.com/support/everything/searching/)
syntax plus three extensions:

```
# Boolean operators: space is AND, | is OR, ! is NOT, <> groups
report <pdf|docx>
report|summary !draft

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

Every component is covered, with commands and examples, in the
[usage guide](docs/USAGE.md).

### Quick start

A minimal `config.toml`; every other setting has a default.
`config.windows.toml`, `config.macos.toml` and
`crates/fs-server/config.toml` (Linux) are commented, complete examples.

```toml
watch_paths = ["C:/Projects", "D:/Data"]
data_dir    = "C:/ProgramData/fs"    # index snapshot / SQLite, Tantivy index
log_dir     = "C:/ProgramData/fs/logs"

[exclusions]
path_patterns = ["**/.git/**", "**/node_modules/**", "**/target/**"]

[api]
rest_port = 7700
grpc_port = 7701
# ipc_path defaults to \\.\pipe\fs on Windows and /tmp/fs.sock elsewhere
```

```sh
# Check the config, then start the daemon
fsd --config config.toml validate-config
fsd --config config.toml daemon

# Search. fs reads only api.ipc_path from the same file;
# --config defaults to ./config.toml
fs --config config.toml search "*.rs size:>1kb"
fs --config config.toml search "fuzzy:reprot dm:lastweek"
fs --config config.toml search --ext rs,toml --path C:/Projects --json

# Index statistics, re-index, interactive TUI
fs --config config.toml stats
fs --config config.toml reindex
fs --config config.toml tui
```

### Running as a service

`install-service` registers fsd with the system's service manager and starts
it; `uninstall-service` stops and removes it. Relative paths in the config
are read from the config's folder.

```sh
# Windows (administrator): the FsDaemon service, as LocalSystem, from boot
fsd --config C:/ProgramData/fs/config.toml install-service

# Linux: a systemd user unit (~/.config/systemd/user/fsd.service) ...
fsd --config ~/.config/fs/config.toml install-service
# ... or a system unit (/etc/systemd/system/fsd.service), as root, from boot
sudo fsd --config /etc/fs/config.toml install-service --system

# macOS: a LaunchAgent (~/Library/LaunchAgents/fs.fsd.plist) ...
fsd --config ~/.config/fs/config.toml install-service
# ... or a LaunchDaemon (/Library/LaunchDaemons/fs.fsd.plist), from boot
sudo fsd --config /etc/fs/config.toml install-service --system

fsd uninstall-service            # add --system for a system service
```

A system service (every Windows service, and `--system` elsewhere) serves
all local users: any of them can search its index over IPC, so watch only
folders they may all see. It refuses `fs open` and extension commands from
clients, which would otherwise run as LocalSystem or root, and takes
`fs watch add` / `remove` only from an administrator (an elevated prompt on
Windows; root or `api.admin_group` elsewhere). A user service
serves only its user. On macOS, read the notes at the top of
`config.macos.toml` first (Full Disk Access).

### Everything file lists (.efu)

An `.efu` file is the CSV file list voidtools Everything exports: full paths,
sizes, FILETIME dates and Windows attributes. List them in the config and
their entries are searched like crawled files, under the paths they name:

```toml
[file_lists]
paths     = ["D:/catalogues/backup-disk.efu", "D:/catalogues/laptop.efu"]
poll_secs = 30    # how often to look for a changed, added or removed list
```

- A list is read again when its size or modification time changes; rows it
  no longer has are removed. A list taken out of the config takes its entries
  with it. A list that cannot be read (say, on a drive that is not mounted)
  keeps its entries.
- Rows under a watch path are left out: the crawl knows those files first-hand.
- Entries are not on this machine, so opening or reading them fails like any
  missing file, and they have no content index.

To write one, add `--efu` to a search. It pages through every result up to
`--limit`, so raise that for a whole tree:

```sh
fs search "path:D:/Photos" --limit 1000000 --efu photos.efu
```

Everything opens the result (File > Open File List) or includes it in its
index (Tools > Options > Indexes > File Lists).

### REST API

```
GET  /search?q=<query>&limit=100   → JSON array of file records
GET  /stats                        → index statistics
POST /reindex                      → trigger full re-crawl
GET  /watch_paths                  → the folders indexed and watched
POST /watch_paths {"path": ...}    → add a folder (token)
DELETE /watch_paths?path=<folder>  → stop watching a folder (token); &forget=true drops its entries
POST /open {"path": ...}          → open a file; extracts a ZIP entry first
GET  /health
```

### gRPC

Proto at `crates/fs-server/proto/fs.proto`. Reflection enabled -
compatible with `grpcurl`.

---

## rar2zip

### What it does

`rar2zip` is a drop-in `fsd` extension that:

1. Watches an input folder for RAR, 7Z, TAR, TAR.GZ, and TAR.BZ2 archives
   that are created, moved or renamed into it, and on start picks up those
   already there. `failed_folder` is skipped even when it is inside it.
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
# config.toml (fsd's config; rar2zip reads its own section)
extensions_dir = "/usr/local/lib/fs/extensions"

[extensions.rar2zip]
watch_folder  = "/srv/incoming"
failed_folder = "/srv/incoming/failed"

[extensions.rar2zip.conversion]
stream_to_transfer_threshold_mb = 500

[extensions.rar2zip.transfer]
max_concurrent = 3
host_key_check = "strict"

[[extensions.rar2zip.destinations]]
host         = "sftp.example.com"
port         = 22
user         = "deploy"
remote_path  = "/uploads"
ssh_key_path = "/etc/fs/rar2zip/id_ed25519"

[extensions.rar2zip.observability]
metrics_port   = 9090
dashboard_port = 8080
```

Every setting is in `crates/fs-ext-rar2zip/config.toml`.

### Host keys

rar2zip verifies each SFTP server's host key before it authenticates:

| `host_key_check` | Unknown host                     | Changed key | Notes                               |
| ---------------- | -------------------------------- | ----------- | ----------------------------------- |
| `strict`         | refused                          | refused     | default                             |
| `accept-new`     | recorded in `known_hosts`, used  | refused     | trust on first use                  |
| `off`            | used                             | used        | logs a warning on every connection  |

`transfer.host_key_check` sets the mode and each destination can override it.
`transfer.known_hosts` names the OpenSSH file to use (default
`~/.ssh/known_hosts`). A destination with `host_keys = ["SHA256:..."]` is
checked against those fingerprints only. To add a host to `known_hosts`:
`ssh-keyscan -p 22 sftp.example.com >> ~/.ssh/known_hosts`. `validate` reports
destinations that `strict` would refuse.

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

The dashboard is at `http://localhost:8080/ui` (`dashboard_port`) - queue
stats, recent jobs (last 20), and transfer stats with 5-second auto-refresh.
Prometheus metrics are at `http://localhost:9090/metrics` (`metrics_port`).

---

## sftpd

`sftpd` is a drop-in `fsd` extension that serves one folder over SFTP, with
public-key login only. It can be a rar2zip destination on the same or another
machine. See `crates/fs-ext-sftpd/README.md` and
`crates/fs-ext-sftpd/config.toml`.

```toml
[extensions.sftpd]
listen   = "127.0.0.1:2222"
root     = "/srv/fs/incoming"
host_key = "/var/lib/fs/sftpd/ssh_host_ed25519_key"   # generated if missing

[[extensions.sftpd.users]]
name            = "rar2zip"
authorized_keys = "/etc/fs/sftpd/rar2zip.authorized_keys"
```

```sh
fs --config config.toml ext sftpd status   # prints the host key fingerprint
```

---

## REST file API

REST listens on `127.0.0.1` by default; set `api.rest_bind` to expose it. Routes
that change files (`/fs/write`, `copy`, `move`, `delete`, `mkdir`) are refused
until a token is configured:

```sh
fsd generate-token   # prints the token, and the digest for api.auth_token_hash
curl -X PUT -H "Authorization: Bearer $TOKEN" --data-binary @f \
  "localhost:7700/fs/write?path=/tmp/f"
```

By default reading (`/fs/read`, `/file`) needs no token. To require it on every
REST route except `/health` and on all gRPC calls (`authorization: Bearer <token>`
metadata), set `api.auth_mode = "all"`; the daemon then refuses to start without
`api.auth_token_hash`. Use this whenever you bind to a non-loopback address.
The local `fs` client talks over the IPC socket and is not affected.

Browsers may only read responses
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
extensions/sftpd/extension.toml
extensions/sftpd/fs-ext-sftpd
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

# The sftpd extension
cargo build --release -p fs-ext-sftpd
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
