# Phase 6 — APIs, Service Layer & Orchestration

**Goal:** Wire up all the APIs for `filesearch` (REST, gRPC, IPC) and finish the
`rar2zip-transfer` service layer (app.rs, service.rs, main.rs). After this
phase, both programs are fully functional end-to-end.

**Estimated scope:** ~1 400 lines of Rust + proto + PowerShell.

---

## 6.1 filesearch — REST API (`api/rest.rs`)

Built with `axum 0.7` + `tower-http` for CORS and tracing.

### Routes

| Method | Path                              | Handler           |
| ------ | --------------------------------- | ----------------- |
| `GET`  | `/search?q=&limit=&offset=`       | `search_handler`  |
| `GET`  | `/stats`                          | `stats_handler`   |
| `POST` | `/reindex` body: `{"full": bool}` | `reindex_handler` |
| `GET`  | `/health`                         | `health_handler`  |
| `GET`  | `/file?path=`                     | `file_handler`    |
| `POST` | `/open` body: `{"path": "..."}`   | `open_handler`    |

### AppState

```rust
pub struct AppState {
    pub sqlite:  Arc<SqliteStore>,
    pub tantivy: Arc<TantivyStore>,
    pub config:  Arc<RwLock<Config>>,
    pub metrics: Arc<IndexMetrics>,
    pub reindex_tx: tokio::sync::watch::Sender<bool>,
}
```

### Response Format

```json
{
  "result": [
    {
      "path": "/home/user/projects/main.rs",
      "name": "main.rs",
      "size_bytes": 4096,
      "mtime": 1712345678,
      "type": "file",
      "score": 0.92,
      "snippet": "fn <mark>main</mark>() {"
    }
  ],
  "total": 42,
  "took_ms": 12,
  "mode": "exact"
}
```

Errors: `400` on invalid query (body: `{"error": "..."}`), `500` on internal
error.

Response `SearchHit` includes two new optional fields for inner archive entries:

- `"archive_path"` — real outer ZIP path (`null` for filesystem files).
- `"archive_entry"` — entry name within the archive (`null` for filesystem
  files).

`/stats` response includes:

- `"archive_entries_indexed"` — count of inner ZIP file records.
- `"archives_crawled"` — count of ZIP files processed.

### Open File Endpoint

```rust
// For real filesystem paths — platform dispatch:
// Linux:   Command::new("xdg-open").arg(path)
// macOS:   Command::new("open").arg(path)
// Windows: Command::new("cmd").args(["/C", "start", "", path])

// For virtual archive paths (contains "!/") — extract then open:
// 1. Split virtual path at "!/" → (archive_path, entry_name)
// 2. Open outer ZIP, find entry by name
// 3. Read entry bytes into a NamedTempFile (OS temp dir, original filename)
// 4. Open temp file with OS default handler
// 5. Schedule temp file deletion after 30 seconds (best-effort)
// Returns {"opened": true, "temp_path": "/tmp/main.rs"}
```

Returns `400` if path not found in index, outer ZIP missing, or entry not found
in the ZIP (archive changed since indexing).

---

## 6.2 filesearch — gRPC API (`api/grpc.rs`)

Implements the `FileSearch` service from `proto/filesearch.proto` using `tonic`
(latest stable; record resolved version in `VERSIONS.md`).

| RPC            | Implementation                               |
| -------------- | -------------------------------------------- |
| `Search`       | `query::execute()` → map to `SearchResponse` |
| `SearchStream` | Execute query, stream results one by one     |
| `GetStats`     | Read from `Arc<IndexMetrics>`                |
| `Reindex`      | Send on `reindex_tx` watch channel           |

Enable `tonic_reflection` for gRPC reflection (compatible with `grpcurl`).

### Proto File

```protobuf
syntax = "proto3";
package filesearch;

service FileSearch {
  rpc Search(SearchRequest) returns (SearchResponse);
  rpc SearchStream(SearchRequest) returns (stream SearchResult);
  rpc GetStats(StatsRequest) returns (StatsResponse);
  rpc Reindex(ReindexRequest) returns (ReindexResponse);
}
```

Full message definitions per spec section 13.

### build.rs

```rust
fn main() {
    tonic_build::compile_protos("proto/filesearch.proto")
        .unwrap_or_else(|e| panic!("protoc failed: {}", e));
}
```

---

## 6.3 filesearch — IPC Server (`api/ipc.rs`)

Newline-delimited JSON protocol over Unix socket / Windows Named Pipe.

**Request format:**

```json
{ "id": "uuid", "method": "search", "params": { "q": "main.rs", "limit": 50 } }
```

**Response format:**

```json
{"id": "uuid", "result": {...}}
{"id": "uuid", "error": "query parse error: ..."}
```

Methods: `search`, `stats`, `reindex`, `open`.

### Platform Implementation

```rust
#[cfg(unix)]
async fn listen(path: &Path, state: Arc<AppState>, shutdown: CancellationToken) {
    // tokio::net::UnixListener
    // Remove stale socket file on startup
    // Remove on shutdown
}

#[cfg(windows)]
async fn listen(path: &Path, state: Arc<AppState>, shutdown: CancellationToken) {
    // tokio::net::windows::named_pipe::ServerOptions
    // New pipe instance per connection
}
```

Each client connection → spawned task → read lines → dispatch → write response.
Disconnect on client EOF or shutdown.

---

## 6.4 filesearch — Daemon Orchestration (`daemon.rs`)

### Startup Sequence

```rust
pub async fn run(config: Arc<RwLock<Config>>, shutdown: CancellationToken) -> Result<()>
```

1. Validate config. Abort if invalid.
2. Initialise logger.
3. PID check (Unix: write PID file; Windows: named mutex).
4. `SqliteStore::new()`.
5. `TantivyStore::new()`.
6. Check `index_meta.last_crawl_time`. If absent → `full_crawl()`.
7. Start Tantivy commit background task.
8. Start config hot-reload watcher.
9. Start filesystem watcher.
10. Start indexer event loop.
11. Start REST server.
12. Start gRPC server.
13. Start IPC server.
14. `tokio::select!` waiting for shutdown signal.

### Graceful Shutdown

On `CancellationToken` cancel:

1. Stop watcher.
2. Stop accepting new API requests (drain in-flight, max 10s).
3. Drain indexer event queue (max 10s).
4. Final Tantivy commit.
5. Close SQLite connections.
6. Remove IPC socket file (Unix).
7. Remove PID file (Unix).
8. Exit 0.

---

## 6.5 filesearch — CLI Frontend (`frontend/cli.rs`)

### Non-interactive Mode

```
filesearch search "main.rs"
filesearch search "fuzzy:mian" --limit 20 --json
filesearch search "content:TODO" --ext rs
filesearch search "fuzzy:config ext:toml"
filesearch stats
filesearch reindex --full
filesearch open "/path/to/file"
```

All commands (except `daemon`) send IPC requests via `ipc_request()`.

### IPC Client

```rust
pub async fn ipc_request(
    ipc_path: &Path,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value>
```

Connects to socket/pipe, sends one JSON request, reads one JSON response.

### TUI Mode (`filesearch tui`)

Built with `ratatui 0.26` + `crossterm 0.27`.

```
┌─────────────────────────────────────────────────────┐
│ 🔍  [search query input box                     ]   │
├─────────────────────────────────────────────────────┤
│ results (scrollable list)                           │
│ > /home/user/projects/main.rs        4KB  rs        │
│   /data/project.zip!/src/main.rs     2KB  rs  [zip] │
│   /home/user/notes/readme.md        12KB  md        │
├─────────────────────────────────────────────────────┤
│ 42 results  •  12ms  •  exact  •  3.2M idx  4k zip  │
└─────────────────────────────────────────────────────┘
```

Inner archive entries show a dim `[zip]` tag (basename of outer archive). The
status line shows the count of indexed ZIP inner entries (`4k zip`).

Keybindings:

| Key          | Action                                                                      |
| ------------ | --------------------------------------------------------------------------- | --- | ----- | -------------------------------------------------------------- |
| Type         | Update query (debounced 100ms → IPC search)                                 |
| ↑/↓          | Navigate results                                                            |     | Enter | Open file via `/open` API (extracts to temp for inner entries) |
| Tab          | Cycle mode: exact → `fuzzy:` → `regex:` → `content:` → `archive:` → [clear] |
| Ctrl+C / Esc | Exit TUI                                                                    |
| F5           | Force re-index                                                              |

---

## 6.6 rar2zip — Core Run Loop (`app.rs`)

### Startup Sequence

```rust
pub async fn run(config: Arc<RwLock<Config>>, shutdown: CancellationToken) -> Result<()>
```

1. `JobQueue::new()` — open/migrate SQLite.
2. Re-enqueue stale jobs (`queue.get_stale_jobs()`). 2b. If
   `multipart_timeout_hours > 0`: call `queue.get_expired_groups()`, move each
   expired group's parts to `failed/`, call `queue.clear_parts()`.
3. Start config hot-reload watcher.
4. Start metrics + dashboard HTTP server.
5. Start folder watcher.
6. Start periodic expired-group check task (every 1 hour).
7. Start main event loop (see below).

### Main Event Loop

```
watcher sends PathBuf
    → intake::process() → enqueues job
    → tokio::spawn(process_job(job))
```

`process_job(job)`:

1. Match on `job.strategy`:

   **Strategy::Disk:**
   - `queue.set_status(id, Converting)`
   - `convert::convert(job, cfg)` → zip_path
   - On conversion error: `queue.set_status(id, Failed, error)`, increment
     metrics, fire alert, return
   - `queue.set_status(id, Transferring)`
   - `transfer::transfer_all(zip_path, job, cfg, metrics, semaphore, shutdown)`
   - On success:
     - Delete zip_path
     - If multi-part: `queue.get_parts()` → delete each part →
       `queue.clear_parts()`
     - Delete original source archive
     - `queue.set_status(id, Done)`; increment success metrics
   - On failure (all retries exhausted):
     - zip_path already moved to `failed/` by `transfer.rs`
     - `queue.set_status(id, Failed, error)`; fire alert

   **Strategy::Stream:**
   - `queue.set_status(id, Transferring)` ← no Converting step
   - `transfer::stream_all(src_path, job, cfg, metrics, semaphore, shutdown)`
   - On success:
     - If multi-part: `queue.get_parts()` → delete each part →
       `queue.clear_parts()`
     - Source archive already deleted inside `stream_convert_and_transfer()`
       after checksum pass
     - `queue.set_status(id, Done)`; increment success metrics
   - On failure (all retries exhausted):
     - Source archive moved to `failed/` inside `stream_all()`
     - `queue.set_status(id, Failed, error)`; fire alert

### Graceful Shutdown

On `CancellationToken` cancel:

- Stop accepting new watcher events.
- Wait for all in-flight `process_job` tasks (timeout: 30s).
- Flush logs.
- Exit.

---

## 6.7 rar2zip — Windows Service (`service.rs`)

```rust
windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(arguments: Vec<OsString>) {
    // 1. Create tokio Runtime
    // 2. Load config
    // 3. Create CancellationToken
    // 4. Register SERVICE_CONTROL_STOP → cancel token
    // 5. Set service status to Running
    // 6. runtime.block_on(app::run(config, token))
    // 7. Set service status to Stopped
}
```

### `install-service.ps1`

```powershell
param(
    [string]$BinaryPath = ".\rar2zip.exe",
    [string]$ServiceName = "rar2zip",
    [switch]$Uninstall
)

if ($Uninstall) {
    sc.exe stop $ServiceName
    sc.exe delete $ServiceName
} else {
    sc.exe create $ServiceName binPath= "$BinaryPath run --service" `
        start= auto DisplayName= "RAR to ZIP Transfer Service"
    sc.exe failure $ServiceName reset= 60 actions= restart/5000/restart/10000/restart/30000
    sc.exe start $ServiceName
}
```

---

## 6.8 rar2zip — Main CLI (`main.rs`)

All four subcommands:

- `run [--service] [--config]` → service mode or direct mode.
- `status [--config]` → print `queue_depth()` table.
- `retry-failed [--config]` → `queue.reset_failed()`.
- `validate-config [--config]` → validate + TCP ping each destination.

`validate-config` exit codes: `0` if all pass, `1` if any fail.

---

## 6.9 Definition of Done

### filesearch

- [ ] `filesearch daemon` starts and all three APIs respond.
- [ ] `curl localhost:7700/search?q=main.rs` returns JSON results.
- [ ] `curl "localhost:7700/search?q=archive:%20*.rs"` returns inner-archive
      entries.
- [ ] `grpcurl localhost:7701 filesearch.FileSearch/Search` returns results.
- [ ] IPC: `filesearch search "*.rs"` returns results including inner-archive
      entries.
- [ ] `POST /open` on an inner-archive entry extracts to temp and opens.
- [ ] `/stats` includes `archive_entries_indexed` and `archives_crawled`.
- [ ] `filesearch tui` launches interactive TUI with `[zip]` tags on inner
      entries.
- [ ] Graceful shutdown: `Ctrl+C` exits cleanly without data loss.

### rar2zip

- [ ] `run` (non-service mode) starts, watches folder, processes an archive.
- [ ] `status` prints queue depth table.
- [ ] `retry-failed` resets failed jobs.
- [ ] `validate-config` exits 0 on valid config.
- [ ] Windows Service: `sc start rar2zip` starts the service.
- [ ] `cargo build --release -p rar2zip` succeeds on Windows with zero warnings.
- [ ] Strategy::Stream: large archive transferred with no zip created on disk.

### Both

- [ ] `cargo clippy -- -D warnings` clean on all new files.
- [ ] No `unwrap()` in service or API code.
