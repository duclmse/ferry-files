# Phase 4 - Archive Conversion & Transfer (rar2zip)

**Goal:** Implement the streaming archive conversion pipeline and the SFTP
transfer engine for `rar2zip`. After this phase, the core function of the
service works end-to-end: watch → intake → convert/stream → transfer.

**Estimated scope:** ~900 lines of Rust.

---

## 4.1 Streaming Conversion Pipeline (`convert.rs`)

### Entry Point

```rust
pub async fn convert(job: &Job, config: &ConversionConfig) -> Result<PathBuf>
```

Returns the path of the produced `.zip` file (same directory as source, `.zip`
extension). Dispatches by extension:

| Extension(s)         | Function                   |
| -------------------- | -------------------------- |
| `.rar`, `.part1.rar` | `convert_rar()`            |
| `.7z`                | `convert_7z()`             |
| `.tar`               | `convert_tar(None)`        |
| `.tar.gz`, `.tgz`    | `convert_tar(Some(Gzip))`  |
| `.tar.bz2`           | `convert_tar(Some(Bzip2))` |

### Pre-conversion Integrity Check

Before conversion, verify the source archive:

- **RAR:** call `unrar`'s test/verify mode.
- **7Z:** `sevenz_rust` verify method.
- **TAR:** attempt to iterate all entries; any error → abort.

If verification fails → `AppError::Conversion("integrity check failed: ...")`.

### Streaming RAR → ZIP (`convert_rar`)

```
1. Open .rar archive (with password if cfg.archive_password is set)
2. Create BufWriter<File>(256 KB buffer) for dst .zip
3. Create ZipWriter wrapping BufWriter
4. For each entry in rar:
   a. directory entry → add ZipWriter dir entry
   b. file entry:
      - Compression: if rar entry is Stored → use Stored in zip
        else → use Deflated at cfg.compression_level
      - Start zip entry with original relative path + mtime
      - Stream bytes in 64 KB chunks: rar reader → zip writer
      - Never buffer entire entry
5. Finalise ZipWriter
```

### Streaming 7Z → ZIP (`convert_7z`)

Same structure using `sevenz_rust::Archive` reader. Each entry streamed into
`ZipWriter` in 64 KB chunks.

### Streaming TAR → ZIP (`convert_tar`)

```rust
fn convert_tar<R: Read>(reader: R, dst: &Path, cfg: &ConversionConfig) -> Result<()>
```

Wrap reader in `GzDecoder` or `BzDecoder` before passing to
`tar::Archive::new()`.

### Multi-part RAR

When `job.file_path` is `archive.part1.rar`:

- Pass base path to `unrar` - it auto-discovers subsequent parts in the same
  folder.
- Caller (`app.rs`) deletes all parts after confirmed success.

### Cleanup Contract

`convert()` does **not** delete any source files. The caller (`app.rs`) handles
deletion after the downstream step succeeds.

---

## 4.2 SFTP Transfer Engine (`transfer.rs`)

### Entry Point

```rust
pub async fn transfer_all(
    zip_path: &Path,
    job: &Job,
    config: &Config,
    metrics: Arc<Metrics>,
    semaphore: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()>
```

Returns `Ok(())` only if **all** destinations succeeded. Each destination is
retried independently.

### Per-Destination Transfer

Steps:

1. Acquire semaphore permit (global concurrency limit across all transfers).
2. TCP connect to `dest.host:dest.port`.
3. `ssh2::Session::handshake()`, then `userauth_pubkey_file()`.
4. Try `session.sftp()`. If SFTP unavailable → fall back to SCP.
5. Upload with bandwidth throttle (see below).
6. Verify remote checksum via `sha256sum` over SSH exec channel.
7. Release semaphore.

### Retry Loop with Exponential Backoff

```
for attempt in 0..=max_retries:
    result = try_transfer()
    if result.ok → return Ok
    if attempt < max_retries:
        delay = min(retry_delay_secs * 2^attempt, 300)  ← cap at 5 minutes
        check shutdown between retries
    else:
        move zip to failed/ folder
        return Err
```

Before each retry (except first): delete partial remote file.

### SFTP Upload with Token-Bucket Bandwidth Throttle

```
1. BufReader<File> (256 KB buffer)
2. Open remote SFTP file (create + truncate)
3. Loop read 64 KB:
   a. Write chunk to remote
   b. If limit_kbps > 0:
      allowed_bytes = limit_kbps * 1024 * elapsed_secs
      if bytes_sent > allowed_bytes:
          sleep(bytes_sent/allowed_rate - elapsed)
4. Flush + close
```

### Remote Checksum Verification

```
1. Open SSH exec channel
2. Execute: sha256sum <remote_path>
3. Read stdout, parse first whitespace-delimited field as hex
4. Compare with locally-computed hash
5. Mismatch → AppError::Transfer("checksum mismatch: expected X got Y")
```

---

## 4.3 Stream Strategy - `stream_convert_and_transfer()`

### Strategy Selection

```rust
pub fn select_strategy(
    file_size_bytes:   u64,
    threshold_mb:      u64,
    destination_count: usize,
) -> Strategy
```

- If `destination_count > 1`: always returns `Strategy::Disk`.
  Streaming is consumed exactly once, so multiple destinations would require
  repeating the full decompress/recompress cycle per destination. Using
  `Strategy::Disk` converts once and fans out the file to all destinations.
- If `threshold_mb > 0` and `file_size_bytes > threshold_mb * 1024 * 1024`:
  returns `Strategy::Stream` (single destination only, large file).
- Otherwise: `Strategy::Disk`.

`select_strategy` is called in `IntakeProcessor::enqueue_path()` after the
destination list is known.

### Stream Execution

When `job.strategy == Strategy::Stream`, `convert::convert()` is **not** called.
Instead, `transfer::stream_convert_and_transfer()` runs the entire pipeline:
source archive → in-memory pipe → SFTP. **No zip file exists on disk at any
point.**

```rust
/// Stream-convert a source archive directly into an SFTP write stream.
/// No .zip file is written to disk at any point.
/// The source archive remains on disk until remote checksum is verified,
/// then deleted.
///
/// On any failure: delete the partial remote file, then retry from scratch
/// (re-reads and re-streams the source archive). The source archive is the
/// retry anchor.
async fn stream_convert_and_transfer(
    src_path: &Path,
    dest: &Destination,
    cfg: &Config,
    shutdown: CancellationToken,
) -> Result<()>
```

### Internal Pipeline

1. Open SSH session + SFTP channel.
2. Open SFTP remote file for write.
3. Create in-memory pipe: `tokio::io::duplex(256 * 1024)`.
4. **Task A** (spawn_blocking): stream-decompress `src_path` → write zip-format
   bytes into the pipe write end. Same streaming logic as `convert_rar()` /
   `convert_7z()` / `convert_tar()` but targeting a `Write` impl backed by the
   pipe sender rather than a file.
5. **Task B** (spawn_blocking): read from pipe read end → write to SFTP remote
   file, applying bandwidth throttle if configured.
6. Await both tasks. If either fails: cancel the other, delete partial remote
   file, return `Err`.
7. After both complete: run `verify_remote_checksum()`.
8. On checksum pass: delete source archive. On mismatch: retry.

### Retry Behaviour for Strategy::Stream

On any failure (task error or checksum mismatch):

1. Call `delete_remote_file()` to remove the partial upload.
2. Re-run `stream_convert_and_transfer()` from step 1. The source archive is
   still present - no intermediate state to restore.
3. Apply exponential backoff between retries.
4. After `max_retries` exhausted: move **source archive** (not a zip) to
   `failed/` folder, update job status to `Failed`.

### `stream_all()` - multi-destination fan-out

```rust
/// Fan out stream_convert_and_transfer to all destinations concurrently.
/// Returns Ok(()) only if ALL destinations succeeded.
pub async fn stream_all(
    src_path: &Path,
    job: &Job,
    config: &Config,
    metrics: Arc<Metrics>,
    semaphore: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()>
```

Each destination is an independent `stream_convert_and_transfer()` call behind
the semaphore, retried independently.

---

## 4.4 Metrics (`metrics.rs`)

```rust
pub struct Metrics {
    pub files_processed:     AtomicU64,
    pub bytes_transferred:   AtomicU64,
    pub conversion_failures: AtomicU64,
    pub transfer_failures:   AtomicU64,
}
```

HTTP server uses `tiny_http` in `tokio::task::spawn_blocking`.

Routes:

- `GET /metrics` → Prometheus text format.
- `GET /health` → `{"status":"ok","queue_depth":N}`.
- `GET /ui` → delegate to `dashboard::render()`.

### Prometheus Format

```
# HELP rar2zip_files_processed_total ...
# TYPE rar2zip_files_processed_total counter
rar2zip_files_processed_total N

# HELP rar2zip_bytes_transferred_total ...
# TYPE rar2zip_bytes_transferred_total counter
rar2zip_bytes_transferred_total N

# HELP rar2zip_conversion_failures_total ...
# TYPE rar2zip_conversion_failures_total counter
rar2zip_conversion_failures_total N

# HELP rar2zip_transfer_failures_total ...
rar2zip_transfer_failures_total N

# HELP rar2zip_queue_depth ...
# TYPE rar2zip_queue_depth gauge
rar2zip_queue_depth N
```

---

## 4.5 Dashboard (`dashboard.rs`)

Self-contained HTML page as an inline Rust string literal. No external files.

Sections:

1. Queue Stats table (pending / converting / transferring / done / failed
   counts).
2. Recent Jobs table (last 20: id truncated, filename, status, retries,
   updated_at, error truncated to 80 chars).
3. Transfer Stats (bytes transferred, files processed, failure counts).

Auto-refresh: `setTimeout(() => location.reload(), 5000)`.

Status badge colours: green=done, red=failed, yellow=converting/transferring,
grey=pending.

---

## 4.6 Alerts (`alerts.rs`)

```rust
pub async fn notify(
    payload: AlertPayload,
    config: &ObservabilityConfig,
    smtp: &Option<SmtpConfig>,
)
```

Non-blocking - errors are logged but not propagated.

**Webhook:** POST JSON `{"text": "[rar2zip] ERROR: ...", "job_id": "..."}` via
`ureq`. Timeout: 5 seconds.

**Email:** `lettre` with `AsyncSmtpTransport::<Tokio1Executor>`. Build message
with From/To/Subject/Body from config.

**Alert triggers** (called from `app.rs`):

- Job exhausts all retries → `AlertLevel::Error`.
- Queue depth > `alert_queue_depth_threshold` → `AlertLevel::Warning`,
  rate-limited to once per 5 minutes.

---

## 4.7 Tests - Phase 4

### convert.rs (unit + fixture tests)

```
- convert known small .rar fixture → valid .zip (verify with zip crate)
- corrupt .rar → returns Conversion error
- .tar.gz fixture → valid .zip
- stored-method entry → copied without re-compression (verify method in zip)
- multi-part .rar → all entries present in output .zip
```

### transfer.rs (unit tests only - no mock SSH server)

```
- bandwidth throttle: given limit_kbps=100 and 200KB sent in 0.5s →
  sleep duration ≈ 1.5s
- exponential backoff: delays = [10, 20, 40, 80, 160, 300, 300] (capped)
- checksum parse: "abc123  /remote/file\n" → extracts "abc123"
- checksum mismatch → AppError::Transfer("checksum mismatch")
- Strategy::Stream: select_strategy(600MB, 500MB, 1) → Stream;
  select_strategy(400MB, 500MB, 1) → Disk;
  select_strategy(600MB, 0, 1) → Disk (threshold_mb=0 disables stream)
  select_strategy(600MB, 500MB, 2) → Disk (multi-destination always Disk)
```

---

## 4.8 Implementation Notes

### SSH library: russh instead of ssh2

The original plan used `ssh2` (libssh2 C bindings). On Windows, `libssh2-sys`
requires OpenSSL to be available (vendored or system). Vendored build invokes
`openssl-src`, which calls `perl ./Configure` - the MSYS2/Git-Bash Perl that
ships with Git for Windows lacks `Locale::Maketext::Simple` and aborts.
WinCNG (`features = ["wincng"]`) does not exist in libssh2-sys 0.3.x.

**Resolution:** switch to `russh = "0.60"` (pure-Rust SSH client, no OpenSSL).
Use `default-features = false, features = ["ring", "flate2", "rsa"]` to select
the `ring` crypto backend and avoid `aws-lc-rs`, which requires NASM on Windows.

russh 0.60 API differences from the plan:
- The `Handler` trait uses `impl Future` (no `#[async_trait]` needed).
- `check_server_key` takes `&russh::keys::PublicKey` (from the internal
  `internal-russh-forked-ssh-key` crate re-exported by `russh::keys`).
- `authenticate_publickey` takes `PrivateKeyWithHashAlg`, not an `Arc<KeyPair>`.
- Private keys are loaded with `russh::keys::load_secret_key(path, None)`.
- Call `session.best_supported_rsa_hash().await?.flatten()` to negotiate the RSA
  hash algorithm before calling `authenticate_publickey`. Ignored for Ed25519.

### Stream strategy: Cursor<Vec<u8>>, not a pipe

`zip::ZipWriter<W>` requires `W: Write + Seek`. A true OS pipe or `tokio::io::duplex`
does not implement `Seek`. The stream strategy therefore converts the entire
source archive to a `Vec<u8>` in memory (`Cursor<Vec<u8>>` as the ZipWriter
backing store), then uploads the bytes via SFTP. No zip file ever touches disk,
but the full compressed zip must fit in RAM.

A future upgrade to the `async_zip` crate would enable a genuinely bounded-memory
pipe-based stream.

### unrar 0.5 typestate API

`unrar 0.5` uses a typestate pattern - `Archive<S>` does not exist; the correct
types are:
- `unrar::Archive` (non-generic, used only for construction)
- `OpenArchive<Process, CursorBeforeHeader>` - returned by `open_for_processing()`
- `OpenArchive<Process, CursorBeforeFile>` - returned by `read_header()` on success

These types are not named in function signatures because naming them requires
importing private type-state markers. The archive is opened inline and the loop
variable is inferred by the compiler.

Key API facts:
- `header.entry()` returns `&FileHeader` which borrows from the header; extract
  all needed fields before consuming the header with `skip()` or `read()`. NLL
  ends the borrow at last use, so no explicit `drop()` is needed.
- `header.read()` returns `(Vec<u8>, OpenArchive<Process, CursorBeforeHeader>)`,
  **not** `Cursor<Vec<u8>>`.
- `FileHeader::is_directory()` (not `is_dir()`), and the field is `method` (not
  `compression_method`). RAR "stored" = method `0x30`.

### sevenz-rust closure type inference

`SevenZReader::for_each_entries` cannot infer the closure parameter types when
the method chain is complex. Explicit annotations are required:
```rust
.for_each_entries(&mut |entry: &sevenz_rust::SevenZArchiveEntry,
                         reader: &mut dyn std::io::Read| { ... })
```

### ureq::Error size

`ureq::Error` is 272 bytes. Clippy's `result_large_err` lint fires on any
`Result<_, ureq::Error>` returned from a closure (here, the `spawn_blocking`
closure in `send_webhook`). Fixed by mapping the error to `Box<ureq::Error>`
inside the closure: `.map_err(Box::new)`.

---

## 4.9 Definition of Done

- [x] `cargo test -p rar2zip convert` passes with fixture archives.
- [x] `cargo test -p rar2zip transfer` passes all unit tests.
- [x] `cargo test -p rar2zip metrics` passes.
- [ ] End-to-end smoke test (Disk): drop a `.rar` into `watch_folder` → zip
      appears in `failed_folder` or on remote.
- [ ] End-to-end smoke test (Stream): drop a large `.rar` → no zip created on
      disk; source archive deleted after remote checksum passes.
- [x] `GET /metrics` returns Prometheus text.
      *(`metrics::tests::routes_answer_over_http`)*
- [x] `GET /ui` returns HTML with auto-refresh.
      *(same test, plus `dashboard::tests`)*
- [x] No `unwrap()` in any conversion or transfer code.
- [x] `cargo clippy -- -D warnings` clean.

---

## Phase 4 Gap Closure (2026-05-12)

### Missing tests implemented

**`crates/rar2zip/src/convert.rs`** - 6 new tests:

- `tar_to_zip_entry_and_content` - creates a `.tar` programmatically, converts to zip,
  verifies entry name and byte content match.
- `tar_to_zip_multiple_entries_all_present` - 2-entry tar → zip, checks all names present.
- `tar_gz_to_zip_entry` - programmatic `.tar.gz` → zip, entry name check.
- `tar_bz2_to_zip_entry` - programmatic `.tar.bz2` → zip, entry name check.
- `corrupt_rar_returns_conversion_error` - garbage bytes in `.rar` → `Err` with
  descriptive message.
- `corrupt_7z_returns_conversion_error` - garbage bytes in `.7z` → `Err`.

Helper functions added in the test module: `make_tar_bytes`, `make_tar_gz_bytes`,
`make_tar_bz2_bytes`, `zip_entry_names`. These build test archives using the same
crates (`tar`, `flate2`, `bzip2`) used in production, without needing external
fixture files on disk.

**`crates/rar2zip/src/transfer.rs`** - pure `throttle_delay` extracted + 3 new tests:

- `throttle_delay` pure function extracts the sleep-duration computation from
  `throttle_async` so it can be tested synchronously. `throttle_async` now delegates
  to it.
- `throttle_delay_200kb_in_half_second_at_100kbps` - spec test case: rate=102 400 B/s,
  200 KB sent in 0.5 s → 1.5 s sleep (± 10 ms).
- `throttle_delay_within_budget_returns_none` - no sleep when bytes_sent < allowed.
- `throttle_delay_zero_limit_returns_none` - `limit_kbps=0` always returns `None`.

### Verification

```
cargo clippy --workspace -- -D warnings  →  0 warnings
cargo test --workspace                   →  148 filesearch + 67 rar2zip = 215 all pass
```

The end-to-end smoke tests (Disk/Stream strategies, live SFTP, `/metrics` HTTP
endpoint) remain manual-only; they require a real SSH server and are not automated.
