# Reference: rar2zip - Module Specifications

> Production-grade Windows service written in Rust. Watches a folder for
> archive files, converts them to `.zip` via a streaming pipeline, and
> transfers them to one or more remote machines via SFTP with full
> observability, recovery, and hot-reload config. Lives at
> `crates/fs-ext-rar2zip/` in the workspace.

---

## 1. Project Layout

```
crates/fs-ext-rar2zip/
├── Cargo.toml
├── config.toml                  ← example config, all fields documented inline
├── install-service.ps1          ← PowerShell: register/unregister Windows Service
├── VERSIONS.md                  ← resolved crate versions
├── README.md
├── data/                        ← created at runtime
│   └── jobs.db                  ← SQLite database
├── logs/                        ← created at runtime
│   └── app.log.<date>
├── failed/                      ← dead-letter folder, created at runtime
└── src/
    ├── main.rs                  ← CLI entrypoint, subcommand dispatch, runtime init
    ├── service.rs               ← Windows Service host (ffi_service_main)
    ├── app.rs                   ← core run loop, wires all components together
    ├── config.rs                ← Config struct, validation, hot-reload watcher
    ├── watcher.rs               ← folder watcher, debounce, pattern filtering
    ├── intake.rs                ← dedup, multi-part aggregation, job enqueue [tested]
    ├── convert.rs               ← streaming archive conversion pipeline [tested]
    ├── transfer.rs              ← SFTP/SCP transfer, bandwidth throttle, checksum [tested]
    ├── queue.rs                 ← SQLite job queue, WAL mode, all DB ops [tested]
    ├── metrics.rs               ← Prometheus counters, tiny_http server
    ├── dashboard.rs             ← inline HTML dashboard, /ui route
    ├── alerts.rs                ← webhook + email alerts
    └── error.rs                 ← unified AppError type
```

> **Note:** There is no `logger.rs` - logging initialisation is provided by
> `common::logger::init()` from `crates/common`.

---

## 2. Cargo.toml

```toml
[package]
name    = "rar2zip"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "rar2zip"
path = "src/main.rs"

[dependencies]
# Workspace shared
tokio      = { workspace = true }
tokio-util = { workspace = true }
serde      = { workspace = true }
serde_json = { workspace = true }
toml       = { workspace = true }
tracing    = { workspace = true }
tracing-subscriber = { workspace = true }
tracing-appender   = { workspace = true }
thiserror  = { workspace = true }
anyhow     = { workspace = true }
chrono     = { workspace = true }
uuid       = { workspace = true }
ssh2       = { workspace = true }
openssl    = { workspace = true }
common     = { path = "../common" }

# Watching
notify                = "6.1"
notify-debouncer-full = "0.3"

# Archive formats
unrar     = "0.5"
sevenz-rust = "0.6"
tar       = "0.4"
flate2    = "1"
bzip2     = "0.4"
zip       = { version = "2", default-features = false, features = ["deflate"] }

# Database
rusqlite    = { version = "0.31", features = ["bundled"] }
r2d2        = "0.8"
r2d2_sqlite = "0.23"

# Hashing
sha2 = "0.10"
hex  = "0.4"

# Glob
glob = "0.3"

# CLI
clap = { version = "4", features = ["derive"] }

# HTTP (metrics + dashboard)
tiny_http = "0.12"

# Alerting
ureq   = "2"
lettre = { version = "0.11", features = ["smtp-transport", "tokio1-native-tls", "builder"] }

# Windows Service
windows-service = "0.6"

# Utilities
bytes = "1"

[dev-dependencies]
tempfile   = "3"
tokio-test = "0.4"

[profile.release]
opt-level = 3
strip     = true
```

See `VERSIONS.md` for resolved crate versions.

---

## 3. error.rs

```rust
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Conversion error: {0}")]
    Conversion(String),

    #[error("Transfer error: {0}")]
    Transfer(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("SSH error: {0}")]
    Ssh(#[from] ssh2::Error),

    #[error("Alert error: {0}")]
    Alert(String),

    #[error("Common error: {0}")]
    Common(#[from] common::CommonError),
}

pub type Result<T> = std::result::Result<T, AppError>;
```

---

## 4. config.rs [tested]

### 4.1 Structs

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub watch_folder:        PathBuf,
    pub exclude_patterns:    Vec<String>,      // glob patterns
    pub include_extensions:  Vec<String>,      // ["rar", "7z", "tar.gz"]
    pub watch_subdirs:       Option<Vec<String>>,
    pub min_file_age_secs:   u64,              // default 5
    pub failed_folder:       PathBuf,

    pub conversion:      ConversionConfig,
    pub destinations:    Vec<Destination>,
    pub transfer:        TransferConfig,
    pub observability:   ObservabilityConfig,
    pub smtp:            Option<SmtpConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConversionConfig {
    pub compression_level:               u8,   // 0–9, default 6
    pub archive_password:                Option<String>,
    /// Files larger than this (MB) are streamed directly without .zip on disk.
    /// 0 = always use Disk strategy.
    pub stream_to_transfer_threshold_mb: u64,  // default 500
    /// Multi-part groups older than this many hours are moved to failed/.
    /// 0 = never expire.
    pub multipart_timeout_hours:         u64,  // default 24
}

#[derive(Debug, Clone, Deserialize)]
pub struct Destination {
    pub host:        String,
    pub port:        u16,         // default 22
    pub user:        String,
    pub remote_path: String,
    pub ssh_key_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransferConfig {
    pub max_concurrent:     usize,  // default 3
    pub max_retries:        u32,
    pub retry_delay_secs:   u64,
    pub bandwidth_limit_kbps: u64,  // 0 = unlimited
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    pub metrics_port:                u16,   // default 9090
    pub dashboard_port:              u16,   // default 8080
    pub webhook_url:                 Option<String>,
    pub alert_email:                 Option<String>,
    pub alert_queue_depth_threshold: usize,  // default 50
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmtpConfig {
    pub host:     String,
    pub port:     u16,
    pub user:     String,
    pub password: String,
    pub from:     String,
}
```

### 4.2 Functions

```rust
pub fn load(path: &Path) -> Result<Config>;

/// Validates: watch_folder exists, ssh_key_paths exist, failed_folder can be
/// created, metrics_port != dashboard_port, compression_level 0–9.
/// Returns Vec<String> of errors (empty = valid).
pub fn validate(cfg: &Config) -> Vec<String>;

pub async fn watch_config(
    config_path: PathBuf,
    config:      Arc<RwLock<Config>>,
    shutdown:    CancellationToken,
);
```

### 4.3 Tests

- Load valid config → succeeds
- Missing required field → Config error
- `compression_level = 10` → validate returns error
- Hot reload: write new config to temp file, watcher updates Arc

---

## 5. queue.rs [tested]

### 5.1 Schema

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS jobs (
    id           TEXT PRIMARY KEY,
    file_path    TEXT NOT NULL,
    file_hash    TEXT NOT NULL,
    status       TEXT NOT NULL CHECK(status IN
                 ('pending','converting','transferring','done','failed')),
    strategy     TEXT NOT NULL CHECK(strategy IN ('disk','stream')),
    destinations TEXT NOT NULL,   -- JSON array of host strings
    retry_count  INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    error        TEXT,
    file_size    INTEGER,         -- added by migrate(); NULL on older rows
    file_mtime   INTEGER          -- seconds; with file_size, for is_known_copy
);

CREATE TABLE IF NOT EXISTS multipart_groups (
    group_id   TEXT NOT NULL,
    part_path  TEXT NOT NULL,
    arrived_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, part_path)
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS idx_jobs_hash   ON jobs(file_hash);
CREATE INDEX IF NOT EXISTS idx_jobs_path   ON jobs(file_path);
```

Intake checks `is_known_copy(path, size, mtime)` before hashing: a file at
the same path with the same size and mtime as a job that has not failed is
the file already queued, seen again by the startup scan or a repeated
event, and is skipped without reading it. Otherwise it hashes the file
(SHA-256, on a blocking thread) and `is_duplicate` decides by content.

### 5.2 Methods

```rust
impl JobQueue {
    pub fn new(db_path: &Path) -> Result<Self>;
    pub fn enqueue(&self, file_path: &Path, file_hash: &str,
                   strategy: Strategy, destinations: &[String]) -> Result<String>;
    /// As `enqueue`, recording the file's (size, mtime) for is_known_copy.
    pub fn enqueue_stamped(&self, file_path: &Path, file_hash: &str,
                           stamp: Option<(u64, i64)>, strategy: Strategy,
                           destinations: &[String]) -> Result<String>;
    pub fn is_known_copy(&self, file_path: &Path, size: u64, mtime: i64) -> Result<bool>;
    pub fn set_status(&self, id: &str, status: Status,
                      error: Option<&str>) -> Result<()>;
    pub fn increment_retry(&self, id: &str) -> Result<()>;
    pub fn get_stale_jobs(&self) -> Result<Vec<Job>>;
    pub fn get_failed_jobs(&self) -> Result<Vec<Job>>;
    pub fn reset_failed(&self) -> Result<usize>;
    pub fn is_duplicate(&self, file_hash: &str) -> Result<bool>;
    pub fn queue_depth(&self) -> Result<HashMap<String, i64>>;
    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<Job>>;

    // Multipart
    pub fn add_part(&self, group_id: &str, part_path: &Path) -> Result<()>;
    pub fn get_parts(&self, group_id: &str) -> Result<Vec<PathBuf>>;
    pub fn clear_parts(&self, group_id: &str) -> Result<()>;

    /// Return multipart groups where the earliest arrived_at is older than
    /// `timeout_hours`. Used by the hourly expiry check in app.rs.
    pub fn get_expired_groups(&self, timeout_hours: u64) -> Result<Vec<String>>;
}
```

### 5.3 Tests

- Enqueue → set_status(converting) → get_stale_jobs returns it
- `is_duplicate`: enqueue, set_status(done), is_duplicate returns true
- `reset_failed`: enqueue 3, set all failed, reset_failed returns 3
- Multipart: add_part ×3, get_parts returns 3, clear_parts, get_parts empty
- Concurrent access: 10 threads enqueue, all succeed, queue_depth correct
- `get_expired_groups`: group arrived_at set to 25h ago with timeout=24 → returned

---

## 6. intake.rs [tested]

### 6.1 Responsibilities

1. Receive raw file paths from watcher.
2. Apply include/exclude filters (belt-and-suspenders).
3. Enforce `min_file_age_secs` gate.
4. Detect multi-part RAR sets; hold until complete.
5. Compute SHA-256 hash for dedup.
6. Determine `Strategy` (disk vs stream) by file size vs threshold.
7. Enqueue to `JobQueue`.

### 6.2 Multi-part Detection

**New style** (`archive.part1.rar`, `archive.part2.rar`, …):
- Regex: `^(.+)\.part(\d+)\.rar$`
- `group_id` = base name before `.partN.rar`.
- Complete when: all `part1..partN` present and no `partN+1` on disk.

**Old style** (`archive.rar`, `archive.r00`, `archive.r01`, …):
- Regex: `^(.+)\.(rar|r\d{2})$`
- `group_id` = base name.
- Complete when: `.rar` + all `.r00`–`.rNN` present, no `.rNN+1`.

### 6.3 Hashing

```rust
/// Stream-reads file with 64KB buffer, returns SHA-256 hex string.
pub async fn hash_file(path: &Path) -> Result<String>;
```

### 6.4 Strategy Selection

```rust
pub fn select_strategy(file_size_bytes: u64, threshold_mb: u64) -> Strategy {
    if threshold_mb > 0 && file_size_bytes > threshold_mb * 1024 * 1024 {
        Strategy::Stream   // no zip on disk; source piped directly into SFTP
    } else {
        Strategy::Disk     // zip written to disk first, then transferred
    }
}
```

> **Stream strategy:** no `.zip` file is ever created on disk. The source
> archive is piped through conversion directly into the SFTP write stream via
> `tokio::io::duplex(256*1024)`. The source archive is the retry anchor and is
> deleted only after remote checksum verification passes.

### 6.5 Tests

- Single `.rar` → enqueued immediately
- Multi-part: `part1.rar` → not enqueued; `part2.rar` → enqueued
- Excluded `*.tmp` → not enqueued
- Duplicate hash (done) → not enqueued, logged
- Small file → `Strategy::Disk`; large file → `Strategy::Stream`
- `threshold_mb = 0` → always `Strategy::Disk` regardless of file size

---

## 7. convert.rs [tested]

### 7.1 Entry Point

```rust
/// Convert archive at job.file_path to a .zip file in the same directory.
/// Returns path of the produced .zip. Uses streaming pipeline - never writes
/// extracted entries to disk.
pub async fn convert(job: &Job, config: &ConversionConfig) -> Result<PathBuf>;
```

Dispatch:
- `.rar`, `.part1.rar` → `convert_rar()`
- `.7z` → `convert_7z()`
- `.tar` → `convert_tar(None)`
- `.tar.gz`, `.tgz` → `convert_tar(Some(Gzip))`
- `.tar.bz2` → `convert_tar(Some(Bzip2))`

### 7.2 Streaming RAR → ZIP

```rust
fn convert_rar(src: &Path, dst: &Path, cfg: &ConversionConfig) -> Result<()>
// 1. Open .rar with password if set
// 2. Create BufWriter<File> for dst .zip (256KB buffer)
// 3. Create ZipWriter wrapping the BufWriter
// 4. For each entry:
//    - Directory → add directory entry to zip
//    - File → stream bytes in 64KB chunks from rar reader → zip writer
//      * Stored rar entry → zip Stored method
//      * Compressed rar entry → zip Deflated at compression_level
//    Preserve relative path and mtime.
// 5. Finalise ZipWriter
```

### 7.3 Streaming 7Z → ZIP

Same structure using `sevenz_rust::Archive` reader.

### 7.4 Streaming TAR → ZIP

```rust
fn convert_tar<R: Read>(reader: R, dst: &Path, cfg: &ConversionConfig) -> Result<()>;
// Wrap reader in GzDecoder / BzDecoder as needed, then tar::Archive::new(reader)
```

### 7.5 Streaming Conversion (Strategy::Stream)

```rust
/// Convert and transfer simultaneously - no .zip file on disk.
/// Uses tokio::io::duplex(256*1024) as in-memory pipe.
pub async fn stream_convert_and_transfer(
    src:      &Path,
    job:      &Job,
    dest:     &Destination,
    cfg:      &Config,
    metrics:  Arc<Metrics>,
    shutdown: CancellationToken,
) -> Result<()>;
```

Two concurrent tasks:
- **Task A** (`spawn_blocking`): reads source archive, converts to ZIP, writes
  to pipe writer (the `duplex` write half). Source archive is the retry anchor.
- **Task B** (async): reads from pipe reader, writes to remote SFTP channel.
  After SFTP write completes, verifies remote checksum.

Source archive deleted only after remote checksum passes.

### 7.6 Integrity Check

Before conversion, verify source archive:
- RAR: `unrar` test/verify mode.
- 7Z: `sevenz_rust` verify method.
- TAR: iterate all entries; abort on any error.

Returns `AppError::Conversion("integrity check failed: ...")` on failure.

### 7.7 Multi-part RAR

Pass `archive.part1.rar` base path to `unrar` - it auto-discovers subsequent
parts. Caller (`app.rs`) handles deleting all parts after success.

### 7.8 Cleanup Contract

`convert()` does **not** delete any files. `app.rs` handles deletion.

### 7.9 Tests

- Convert known `.rar` fixture → valid `.zip`
- Corrupt `.rar` → Conversion error
- `.tar.gz` fixture → valid `.zip`
- Stored-method entry → copied without re-compression
- Multi-part `.rar` → all entries in output `.zip`

---

## 8. transfer.rs [tested]

### 8.1 Entry Points

```rust
/// Transfer zip_path to all destinations concurrently.
/// Returns Ok(()) only if ALL destinations succeeded.
pub async fn transfer_all(
    zip_path: &Path,
    job:      &Job,
    config:   &Config,
    metrics:  Arc<Metrics>,
    semaphore: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()>;

/// Fan-out streaming transfer to all destinations (Strategy::Stream).
pub async fn stream_all(
    src_path: &Path,
    job:      &Job,
    config:   &Config,
    metrics:  Arc<Metrics>,
    semaphore: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()>;
```

### 8.2 Per-Destination Transfer

```rust
async fn transfer_one(
    zip_path:     &Path,
    dest:         &Destination,
    transfer_cfg: &TransferConfig,
    shutdown:     CancellationToken,
) -> Result<()>;
```

Steps:
1. Acquire semaphore permit (limits global concurrency).
2. TCP connect to `dest.host:dest.port`.
3. SSH handshake + `userauth_pubkey_file()`.
4. Open SFTP channel; fall back to SCP if SFTP fails.
5. Transfer file (see 8.3).
6. Verify remote checksum (see 8.4).

Retry loop with exponential backoff:
```
for attempt in 0..=max_retries:
    result = try_transfer()
    if Ok: return Ok
    if attempt < max_retries:
        sleep(min(retry_delay_secs * 2^attempt, 300s))
    else:
        move zip to failed/
        return Err
```

Check `shutdown` token between retries.

### 8.3 SFTP Upload with Bandwidth Throttle

```rust
fn sftp_upload(sftp: &ssh2::Sftp, local: &Path,
               remote: &Path, limit_kbps: u64) -> Result<()>
// Token bucket: allowed_bytes = limit_kbps * 1024 * elapsed_secs
// If sending faster: sleep for deficit duration
```

### 8.4 Remote Checksum Verification

```rust
fn verify_remote_checksum(session: &ssh2::Session,
                           remote_path: &Path,
                           expected_hex: &str) -> Result<()>
// SSH exec: sha256sum <remote_path>
// Parse first field, compare with expected_hex
// Mismatch → AppError::Transfer("checksum mismatch")
```

### 8.5 Remote Cleanup on Retry

Before each retry, delete partial remote file:
```rust
fn delete_remote_file(session: &ssh2::Session, remote_path: &Path) -> Result<()>;
```

### 8.6 Tests

- Bandwidth throttle: correct sleep durations
- Exponential backoff sequence: 10, 20, 40, 80, …, capped at 300
- Checksum parse: handles `sha256sum` output format

---

## 9. watcher.rs

```rust
pub async fn run_watcher(
    config:  Arc<RwLock<Config>>,
    tx:      mpsc::Sender<PathBuf>,
    shutdown: CancellationToken,
) -> Result<()>;
```

Uses `notify-debouncer-full`. Only processes `EventKind::Create(CreateKind::File)`.

Filter pipeline (in order):
1. Extension in `include_extensions` (case-insensitive).
2. Filename doesn't match any `exclude_patterns` glob.
3. If `watch_subdirs` is set, parent dir matches one of the listed subdirs.
4. File `mtime` older than `min_file_age_secs`. If not yet, schedule delayed re-check.

Re-reads config from `Arc<RwLock<Config>>` on each event.

---

## 10. app.rs

### 10.1 Startup Sequence

```rust
pub async fn run(config: Arc<RwLock<Config>>, shutdown: CancellationToken) -> Result<()>;
```

1. `JobQueue::new()` - open/migrate SQLite.
2. Re-enqueue stale jobs (`queue.get_stale_jobs()`).
2b. If `multipart_timeout_hours > 0`: call `queue.get_expired_groups()`,
    move each expired group's parts to `failed/`, call `queue.clear_parts()`.
3. Start config hot-reload watcher.
4. Start metrics + dashboard HTTP server.
5. Start folder watcher.
6. Start periodic expired-group check task (every 1 hour).
7. Start main event loop.

### 10.2 Main Event Loop

```
watcher sends PathBuf → intake::process() → enqueues job → tokio::spawn(process_job)
```

`process_job(job)` - Strategy::Disk path:
1. `queue.set_status(id, Converting)`
2. `convert::convert(job, cfg)` → zip_path
3. On error: `queue.set_status(id, Failed, error)`, increment metrics, alert; return
4. `queue.set_status(id, Transferring)`
5. `transfer::transfer_all(zip_path, ...)`
6. On success:
   - Delete zip_path
   - If multi-part: `queue.get_parts()` → delete parts → `queue.clear_parts()`
   - Delete original source archive
   - `queue.set_status(id, Done)`; increment success metrics
7. On failure: zip_path already moved to `failed/` by `transfer.rs`;
   `queue.set_status(id, Failed, error)`; fire alert

`process_job(job)` - Strategy::Stream path:
1. `queue.set_status(id, Transferring)` ← no Converting step
2. `transfer::stream_all(src_path, ...)`
3. On success:
   - If multi-part: `queue.get_parts()` → delete parts → `queue.clear_parts()`
   - Source archive already deleted inside `stream_convert_and_transfer()` after checksum
   - `queue.set_status(id, Done)`; increment success metrics
4. On failure: source archive moved to `failed/` inside `stream_all()`;
   `queue.set_status(id, Failed, error)`; fire alert

### 10.3 Graceful Shutdown

On `CancellationToken` cancel:
1. Stop accepting new watcher events.
2. Wait for in-flight `process_job` tasks (timeout: 30s).
3. Flush logs.
4. Exit.

---

## 11. metrics.rs

### 11.1 Counters

```rust
pub struct Metrics {
    pub files_processed:     AtomicU64,
    pub bytes_transferred:   AtomicU64,
    pub conversion_failures: AtomicU64,
    pub transfer_failures:   AtomicU64,
}
```

`queue_depth` is read live from DB (not stored in Metrics).

### 11.2 HTTP Server

`tiny_http` in `tokio::task::spawn_blocking`.

| Route | Response |
|-------|----------|
| `GET /metrics` | Prometheus text format |
| `GET /health` | `{"status":"ok","queue_depth":N}` |
| `GET /ui` | `dashboard::render(queue, metrics)` |

### 11.3 Prometheus Metrics

```
rar2zip_files_processed_total
rar2zip_bytes_transferred_total
rar2zip_conversion_failures_total
rar2zip_transfer_failures_total
rar2zip_queue_depth   (gauge)
```

---

## 12. dashboard.rs

```rust
pub fn render(queue: &JobQueue, metrics: &Metrics) -> String;
```

Self-contained inline HTML, no external deps. Sections:
1. Queue Stats - pending / converting / transferring / done / failed counts.
2. Recent Jobs - last 20 jobs: id (truncated), filename, status, retries, error.
3. Transfer Stats - bytes transferred, files processed, failure counts.

Auto-refresh every 5 seconds via `setTimeout(() => location.reload(), 5000)`.
Dark background, monospace font, coloured status badges.

---

## 13. alerts.rs

```rust
pub struct AlertPayload {
    pub level:   AlertLevel,   // Warning | Error
    pub title:   String,
    pub message: String,
    pub job_id:  Option<String>,
}

/// Non-blocking - errors are logged but not propagated.
pub async fn notify(payload: AlertPayload, config: &ObservabilityConfig,
                    smtp: &Option<SmtpConfig>);
```

**Webhook** - POST to `webhook_url` with JSON body; `ureq`, timeout 5s.

**Email** - `lettre` with `AsyncSmtpTransport<Tokio1Executor>`.

Alert triggers:
- Job exhausts all retries → `AlertLevel::Error`
- Queue depth exceeds `alert_queue_depth_threshold` → `AlertLevel::Warning`
  (rate-limited: at most once per 5 minutes)

---

## 14. service.rs

### 14.1 Windows Service Entry

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

### 14.2 install-service.ps1

```powershell
param(
    [string]$BinaryPath = ".\rar2zip.exe",
    [string]$ServiceName = "rar2zip",
    [switch]$Uninstall
)

if ($Uninstall) {
    sc.exe stop $ServiceName
    sc.exe delete $ServiceName
    Write-Host "Service uninstalled."
} else {
    sc.exe create $ServiceName binPath= "$BinaryPath run --service" `
        start= auto DisplayName= "RAR to ZIP Transfer Service"
    sc.exe failure $ServiceName reset= 60 actions= restart/5000/restart/10000/restart/30000
    sc.exe start $ServiceName
    Write-Host "Service installed and started."
}
```

---

## 15. main.rs

```rust
#[derive(clap::Parser)]
#[command(name = "rar2zip", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start the watcher (pass --service when launched by Windows SCM)
    Run {
        #[arg(long)] service: bool,
        #[arg(long, default_value = "config.toml")] config: PathBuf,
    },
    /// Print current queue statistics and exit
    Status {
        #[arg(long, default_value = "config.toml")] config: PathBuf,
    },
    /// Reset all failed jobs to pending and exit
    RetryFailed {
        #[arg(long, default_value = "config.toml")] config: PathBuf,
    },
    /// Validate config, check paths and SSH connectivity, then exit
    ValidateConfig {
        #[arg(long, default_value = "config.toml")] config: PathBuf,
    },
}
```

Dispatch:
- `Run { service: true }` → `windows_service::service_dispatcher::start()`
- `Run { service: false }` → `app::run()` with Ctrl+C handler
- `Status` → print `queue.queue_depth()` table, exit 0
- `RetryFailed` → `queue.reset_failed()`, print count, exit 0
- `ValidateConfig` → `config::validate()`, TCP ping each destination, exit 0/1

---

## 16. config.toml (Example)

```toml
watch_folder         = "C:/watched"
exclude_patterns     = ["*.tmp", "*.part", "~*", "*.crdownload"]
include_extensions   = ["rar", "7z", "tar.gz", "tar.bz2", "tar"]
min_file_age_secs    = 5
failed_folder        = "C:/watched/failed"

[conversion]
compression_level               = 6
archive_password                = ""
stream_to_transfer_threshold_mb = 500
multipart_timeout_hours         = 24

[[destinations]]
host         = "192.168.1.100"
port         = 22
user         = "alice"
remote_path  = "/home/alice/incoming"
ssh_key_path = "C:/Users/you/.ssh/id_rsa"

[transfer]
max_concurrent       = 3
max_retries          = 5
retry_delay_secs     = 10
bandwidth_limit_kbps = 0

[observability]
metrics_port                  = 9090
dashboard_port                = 8080
webhook_url                   = ""
alert_email                   = ""
alert_queue_depth_threshold   = 50

[smtp]
host     = "smtp.example.com"
port     = 587
user     = "alerts@example.com"
password = "secret"
from     = "rar2zip <alerts@example.com>"
```

---

## 17. Platform Notes

- Builds on **Windows only** (Windows Service APIs, `unrar.dll`).
- `unrar.dll` must be on PATH (installed with WinRAR).
- `sha256sum` must be available on the remote SFTP server (standard on Linux).
- SSH public key must be installed in `~/.ssh/authorized_keys` on remote.
- `cargo build --release` must succeed with zero warnings on Windows.
