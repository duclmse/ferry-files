# Phase 3 — Indexer & Watcher

**Goal:** Implement the `filesearch` indexer pipeline (exclusions, content
extraction, parallel crawler) and the cross-platform filesystem watcher. After
this phase, `filesearch daemon` can crawl a directory tree, populate both
stores, and keep them up-to-date in real time.

Also covers the `rar2zip` folder watcher and intake logic (dedup,
multi-part detection, job enqueue).

**Estimated scope:** ~1 400 lines of Rust (includes `archive.rs`).

---

## 3.1 filesearch — Exclusion Engine (`indexer/exclusions.rs`)

```rust
pub struct ExclusionEngine {
    patterns: Vec<glob::Pattern>,
    exclude_hidden: bool,
}

impl ExclusionEngine {
    pub fn new(cfg: &ExclusionConfig) -> Result<Self>;
    pub fn should_exclude(&self, path: &Path, is_hidden: bool) -> bool;
}
```

Exclusion pipeline (short-circuit on first match):
1. If `exclude_hidden && is_hidden` → exclude.
2. For each glob pattern: if `pattern.matches(path_str)` → exclude.
3. Otherwise → include.

### Network Mount Detection

Platform-specific `is_network_mount(path) -> bool`:
- **Linux:** parse `/proc/mounts`, match `fstype` against `nfs`, `nfs4`, `cifs`, `smbfs`.
- **macOS:** `nix::sys::statfs::statfs(path)`, check `f_fstypename`.
- **Windows:** `GetDriveTypeW` returning `DRIVE_REMOTE (4)`.

Used in content extraction (skip content on network mounts if
`skip_network_content = true`).

---

## 3.2 filesearch — Content Extraction (`indexer/content.rs`)

```rust
pub fn content_limit(path: &Path, cfg: &ContentConfig) -> Option<u64>;
pub async fn extract_content(path: &Path, max_bytes: u64) -> Result<Option<String>>;
pub fn is_binary(buf: &[u8]) -> bool;
```

### Binary Detection

Scan first 8 KB. If > 0.1% of bytes are `\0` (null) → binary → return `None`.

### Read Strategy

| File size | Strategy |
|-----------|---------|
| ≤ 1 MB | `tokio::fs::read()` into a `Vec<u8>` |
| > 1 MB | `memmap2::Mmap` (read-only) |

### Encoding Detection

Use `encoding_rs::Encoding::for_bom()` first (BOM detection).
If no BOM: use `encoding_rs::UTF_8::decode()` with lossy fallback.
Log encoding at `debug` level.

### Truncation

After decoding, truncate `String` to `max_bytes` at the nearest UTF-8 char
boundary to avoid splitting multi-byte chars.

---

## 3.3 filesearch — Parallel Crawler (`indexer/crawler.rs`)

### Full Crawl

```rust
pub async fn full_crawl(
    config: Arc<RwLock<Config>>,
    sqlite: Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    shutdown: CancellationToken,
) -> Result<()>
```

Algorithm:
1. For each `watch_path` in config:
   a. Build `ignore::WalkBuilder` with `hidden(false)` and `git_ignore(false)`.
   b. Walk with `rayon` parallel iterator.
   c. For each entry:
      - Apply `ExclusionEngine::should_exclude()`.
      - Build `FileRecord` from `std::fs::Metadata`.
      - Populate `attrib_bitmask` via platform-specific helper.
      - Call `sqlite.upsert_file(record)`.
      - If `content_limit(path, cfg).is_some()`: send to content channel.
   d. After walk: final Tantivy commit.
2. Reconcile: for each `watch_path`, call `sqlite.paths_under()`, compare
   against on-disk state, call `sqlite.remove_file()` for stale entries.
3. Update `index_meta` with `last_crawl_time` and `total_files`.

### Incremental Update

```rust
pub async fn process_event(
    event: IndexEvent,
    config: Arc<RwLock<Config>>,
    sqlite: Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
)
```

| Event | Action |
|-------|--------|
| `Created(path)` | `sqlite.upsert_file()`, enqueue content if applicable |
| `Modified(path)` | Same as Created |
| `Deleted(path)` | `sqlite.remove_file()`, `tantivy.remove()` |
| `Renamed { from, to }` | Update path+name in SQLite; if directory, prefix-replace all children; update Tantivy |

### IndexMetrics

```rust
pub struct IndexMetrics {
    pub total_files:      AtomicU64,
    pub total_dirs:       AtomicU64,
    pub content_indexed:  AtomicU64,
    pub last_crawl_secs:  AtomicU64,
    pub events_processed: AtomicU64,
}
```

---

## 3.4 filesearch — Filesystem Watcher (`watcher/mod.rs`)

```rust
pub async fn run_watcher(
    config: Arc<RwLock<Config>>,
    tx: mpsc::Sender<IndexEvent>,
    shutdown: CancellationToken,
) -> Result<()>
```

- Uses `notify-debouncer-full` with `RecommendedWatcher`.
- Collects events for up to 500 ms **or** `event_batch_size`, whichever first.
- Deduplication priority within a batch: `Delete > Rename > Modify > Create`.
- Converts `notify::Event` → `IndexEvent`.
- On each batch: re-read config to pick up new/removed `watch_paths`.

---

## 3.5 rar2zip — Folder Watcher (`watcher.rs`)

```rust
pub async fn run_watcher(
    config: Arc<RwLock<Config>>,
    tx: mpsc::Sender<PathBuf>,
    shutdown: CancellationToken,
) -> Result<()>
```

Filter pipeline (applied in order for each `Create(File)` event):
1. Extension in `include_extensions` (case-insensitive).
2. Filename does not match any `exclude_patterns` glob.
3. If `watch_subdirs` is set: parent dir matches one of the listed subdirs.
4. File `mtime` is older than `min_file_age_secs`. If not yet old, schedule
   a delayed re-check with `tokio::time::sleep`.

---

## 3.6 rar2zip — Intake (`intake.rs`)

### Responsibilities

1. Receive raw paths from watcher.
2. Apply include/exclude filters (redundant check, belt-and-suspenders).
3. Detect multi-part RAR sets; hold until complete.
4. Compute SHA-256 hash for dedup.
5. Determine `Strategy` (disk vs stream) by file size vs threshold.
6. Enqueue to `JobQueue`.

### Multi-part Detection

**New style** (`archive.part1.rar`, `archive.part2.rar`, …):
- Regex: `^(.+)\.part(\d+)\.rar$`
- `group_id` = base name before `.partN.rar`.
- Complete when: all `part1..partN` present and no `partN+1` on disk.

**Old style** (`archive.rar`, `archive.r00`, `archive.r01`, …):
- Regex: `^(.+)\.(rar|r\d{2})$`
- `group_id` = base name.
- Complete when: `.rar` + all `.r00`–`.rNN` present, no `.rNN+1`.

---

## 3.7 filesearch — Archive Entry Crawler (`indexer/archive.rs`)

Full specification in `zip-inner-streaming-spec.md` sections 4–5.

Summary of responsibilities:
- `ArchiveCrawler` streams all indexable entries from a ZIP using `zip::ZipArchive`.
- Builds virtual paths: `<archive_path>!/<entry_name>`.
- Applies `max_entries_per_archive` safety limit.
- Attempts passwords for encrypted entries.
- Calls `content::extract_content_from_bytes()` for text entries.
- **Never writes to disk.**

In `full_crawl()`: after upserting a ZIP's own `FileRecord`, call
`crawl_archive()` to stream and upsert all its inner entries. All inner
upserts are wrapped in a single SQLite transaction per archive.

In `process_event()`: `Modified(zip)` → `remove_archive_entries()` then
`crawl_archive()`; `Deleted(zip)` → `remove_archive_entries()` only.

Content extraction for inner files delegates to the new
`content::extract_content_from_bytes(buf: &[u8]) -> Option<String>` function
(binary detection + encoding_rs decode, no file path needed).

---

### Hashing

```rust
pub async fn hash_file(path: &Path) -> Result<String>
```

Stream-read with 64 KB buffer, feed `sha2::Sha256`, return hex string.
Never load entire file into memory.

### Strategy Selection

```rust
pub fn select_strategy(file_size_bytes: u64, threshold_mb: u64) -> Strategy {
    if threshold_mb > 0 && file_size_bytes > threshold_mb * 1024 * 1024 {
        Strategy::Stream   // source archive → in-memory pipe → SFTP; no zip on disk
    } else {
        Strategy::Disk     // zip written to disk first, then SFTP'd
    }
}
```

> `Strategy::Stream`: no zip file is ever created. The source archive is
> piped through the conversion stage directly into the SFTP write stream.
> The source archive is the retry anchor and is deleted only after remote
> checksum verification passes.
>
> `Strategy::Disk`: zip is the transfer artifact; deleted after checksum
> passes. Source archive deleted after zip is confirmed complete.

---

## 3.7 Tests — Phase 3

### filesearch Exclusion Engine
```
- path matching **/node_modules/** → excluded
- hidden file, exclude_hidden=false → not excluded
- hidden file, exclude_hidden=true → excluded
- non-matching path → not excluded
```

### filesearch Content Extractor
```
- UTF-8 text → extracted correctly
- Latin-1 encoded → transcoded to UTF-8
- PNG binary header → returns None
- file > max_bytes → truncated at byte boundary
- extension not in config → returns None
- "*" wildcard → used as default
```

### filesearch Crawler
```
- crawl temp dir of 100 files → all records in SQLite
- path matching exclusion pattern → not in SQLite
- reconcile: delete file on disk, re-reconcile → removed from SQLite
- rename event: old path gone, new path present in SQLite
- delete event: removed from both SQLite and Tantivy
```

### filesearch Archive Crawler (see zip-inner-streaming-spec.md §4.5)
```
- crawl fixture ZIP with 5 text entries → 5 ArchiveEntry items, virtual paths correct
- directory entries yield is_dir=true, content=None
- entry count > max_entries_per_archive → limited to max, warn logged
- encrypted entry + no passwords → skipped, warn logged
- binary entry → content=None, metadata record still indexed
- entry > content_limit → content truncated
- nested ZIP, nesting_depth=0 → inner ZIP treated as opaque file, not recursed
- crawl_archive() called from full_crawl → inner records in SQLite with archive_path set
- Modified(zip) event → stale inner entries removed, new entries re-indexed
- Deleted(zip) event → all inner entries removed from SQLite and Tantivy
```

### rar2zip Intake
```
- single .rar → enqueued immediately
- part1.rar arrives → not enqueued; part2.rar arrives → enqueued
- excluded file (*.tmp) → not enqueued
- duplicate hash (already done) → not enqueued, logged
- small file → Strategy::Disk; large file → Strategy::Stream
```

---

## 3.8 Definition of Done

- [ ] `cargo test -p filesearch indexer` passes.
- [ ] `cargo test -p filesearch archive` passes (zip-inner-streaming-spec §4.5).
- [ ] `cargo test -p rar2zip intake` passes.
- [ ] `filesearch daemon` starts, crawls a test directory, populates SQLite.
- [ ] ZIP files in watched dir have inner entries indexed automatically.
- [ ] File created in watched dir appears in SQLite within 1 second.
- [ ] File deleted from watched dir is removed from SQLite within 1 second.
- [ ] ZIP modified → inner entries refreshed within 1 second.
- [ ] ZIP deleted → all inner entries removed within 1 second.
- [ ] No `unwrap()` in indexer, archive, or watcher code.
- [ ] `cargo clippy -- -D warnings` clean.
