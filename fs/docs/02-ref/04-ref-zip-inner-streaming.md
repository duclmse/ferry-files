# Reference: ZIP Inner File Streaming

> Addendum to the filesearch specs. Adds support for indexing and searching
> files *inside* ZIP archives, streaming entries without extracting to disk.
> Changes affect: `config.rs`, `store/sqlite_store.rs`, `indexer/archive.rs`
> (new), `indexer/crawler.rs`, `indexer/content.rs`, `query/ast.rs`,
> `query/evaluator.rs`, `api/rest.rs`, `api/grpc.rs`, `frontend/cli.rs`.

---

## 1. Virtual Path Convention

Inner file paths use the separator `!/`:

```
<outer_archive_path>!/<entry_path_within_archive>
```

Examples:
```
/home/user/project.zip!/src/main.rs
/home/user/project.zip!/docs/readme.txt
C:\data\archive.zip!/reports/q1.xlsx
```

Rules:
- `<outer_archive_path>` - real, absolute filesystem path of the ZIP.
- `<entry_path_within_archive>` - entry name exactly as stored in the ZIP
  central directory, normalised to forward slashes.
- The separator is always `!/` regardless of OS path separator.
- Virtual paths are **not** valid filesystem paths. They exist only in the
  index. The `archive_path` column always points to the real ZIP on disk.
- The `path` column stores the full virtual path; used as unique key.
- The `archive_path` column stores the outer ZIP path; used for bulk
  invalidation when the ZIP changes.

---

## 2. Config Changes (`config.rs`)

### 2.1 New Section: `[archive_index]`

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveIndexConfig {
    /// Enable ZIP inner-file indexing. Default: true.
    pub enabled: bool,

    /// Archive formats to stream. Currently only "zip" is supported.
    /// Default: ["zip"]
    pub formats: Vec<String>,

    /// Per-extension content limits for inner files (bytes).
    /// Inherits from [content.extension_limits] for unlisted extensions.
    /// Set to 0 to skip content indexing for an inner extension.
    pub inner_extension_limits: HashMap<String, u64>,

    /// Max entries to index per ZIP. Entries beyond limit are skipped.
    /// Default: 10_000. Set to 0 for no limit.
    pub max_entries_per_archive: usize,

    /// Max depth of nested archive indexing.
    /// 0 = do not recurse into ZIPs inside ZIPs (default).
    pub max_nesting_depth: u8,

    /// Passwords to try for encrypted entries. Empty = skip encrypted entries.
    pub passwords: Vec<String>,
}
```

Add `pub archive_index: ArchiveIndexConfig` to `Config`.

### 2.2 Validation Rules

- `max_nesting_depth > 1` → warning log (allowed).
- `formats` items must be in supported set; unknown formats → validation error.
- Warn if `enabled = true` but no `inner_extension_limits` entry and no `"*"` fallback.

### 2.3 config.toml Example

```toml
[archive_index]
enabled                 = true
formats                 = ["zip"]
max_entries_per_archive = 10000
max_nesting_depth       = 0
passwords               = []

[archive_index.inner_extension_limits]
rs   = 524288
py   = 524288
md   = 262144
log  = 0
"*"  = 0
```

---

## 3. SQLite Schema Changes (`store/sqlite_store.rs`)

### 3.1 New Column

```sql
ALTER TABLE files ADD COLUMN archive_path TEXT DEFAULT NULL;
-- NULL = regular filesystem file
-- Non-NULL = virtual inner-archive entry; value is the real outer archive path

CREATE INDEX IF NOT EXISTS idx_files_archive ON files(archive_path)
    WHERE archive_path IS NOT NULL;
```

### 3.2 FileRecord

```rust
pub struct FileRecord {
    // ... existing fields ...
    /// None for real filesystem files.
    /// Some(archive_path) for inner ZIP entries.
    pub archive_path: Option<String>,
}
```

For inner ZIP entries: `is_hidden = false`, `is_symlink = false`,
`attrib_bitmask = 0`, `ctime = mtime` (ZIP has no ctime).

### 3.3 New Methods

```rust
/// Remove all inner-file records for the given outer archive path.
/// Called before re-indexing (modify event) or on ZIP deletion.
pub fn remove_archive_entries(&self, archive_path: &str) -> Result<usize>;

/// Return all virtual paths belonging to the given archive.
pub fn paths_in_archive(&self, archive_path: &str) -> Result<Vec<String>>;

/// Count of inner-file records across all archives.
pub fn total_archive_entries(&self) -> Result<i64>;
```

---

## 4. New Module: `indexer/archive.rs` [tested]

### 4.1 ArchiveCrawler

```rust
pub struct ArchiveCrawler<'cfg> {
    config:         &'cfg ArchiveIndexConfig,
    content_config: &'cfg ContentConfig,
}

impl<'cfg> ArchiveCrawler<'cfg> {
    pub fn new(config: &'cfg ArchiveIndexConfig,
               content_config: &'cfg ContentConfig) -> Self;

    /// Stream all indexable entries from a ZIP file. Never writes to disk.
    /// `nesting_depth`: 0 for top-level, incremented for nested archives.
    pub fn crawl(
        &self,
        archive_path:  &Path,
        nesting_depth: u8,
    ) -> Result<impl Iterator<Item = Result<ArchiveEntry>>>;
}

pub struct ArchiveEntry {
    pub virtual_path:        String,   // "<archive_path>!/<entry_name>"
    pub outer_archive_path:  String,
    pub entry_name:          String,   // normalised to forward slashes
    pub extension:           Option<String>,
    pub size_bytes:          u64,
    pub mtime:               i64,      // from ZIP central directory (DOS date)
    pub is_dir:              bool,
    pub content:             Option<String>,
}

pub fn archive_entry_to_record(entry: &ArchiveEntry) -> FileRecord;
```

### 4.2 Entry Iteration Algorithm

```
1. zip::ZipArchive::new(BufReader::new(File::open(archive_path)?, 65536))
2. effective_count = min(archive.len(), max_entries_per_archive)
   [log warn if truncated]

3. For i in 0..effective_count:
   a. entry = archive.by_index(i)?
   b. If encrypted:
      - No passwords configured → skip, warn
      - Try each password; if all fail → skip, warn
   c. Build virtual_path = format!("{}!/{}", archive_path, normalise(entry.name()))
   d. extension = extract_extension(entry.name())
   e. If is_dir → yield entry with is_dir=true, content=None; continue
   f. Determine content limit from inner_extension_limits, fallback to
      content_config.extension_limits, fallback to 0 (skip)
   g. If content_limit > 0 and size_bytes > 0:
      - Read up to min(content_limit, size_bytes) bytes
      - content::extract_content_from_bytes(buf)
   h. Yield ArchiveEntry

4. If nesting_depth < max_nesting_depth:
   Re-iterate entries where extension == "zip":
   - Extract inner ZIP bytes to in-memory cursor (capped at max_content_size_bytes)
   - Recursively crawl(cursor, nesting_depth + 1)
   - Prefix inner virtual paths with inner ZIP's virtual path
```

### 4.3 Tests

```
- crawl fixture ZIP with 5 text entries → 5 ArchiveEntry items, virtual paths correct
- directory entries: is_dir=true, content=None
- entry_count > max_entries_per_archive → only max returned, warn logged
- encrypted entry, no passwords → entry skipped, warn logged
- encrypted entry, correct password → returned with content
- binary entry (PNG) → content=None, metadata record still indexed
- entry > content_limit → content truncated
- extension not in inner_extension_limits, in content.extension_limits → outer limit used
- "*" = 0 in inner_extension_limits → no content for unknown extensions
- nested ZIP, nesting_depth=0 → inner ZIP indexed as file (not recursed)
- nested ZIP, nesting_depth=1 → inner ZIP entries indexed with nested virtual path
- crawl_archive() from full_crawl → inner records in SQLite with archive_path set
- Modified(zip) event → stale entries removed, new entries re-indexed
- Deleted(zip) event → all inner entries removed from SQLite and Tantivy
```

---

## 5. Crawler Changes (`indexer/crawler.rs`)

### 5.1 Full Crawl Integration

After `sqlite.upsert_file(record)` for each ZIP entry in `full_crawl()`:

```rust
if archive_index_cfg.enabled
    && archive_index_cfg.formats.contains(&extension)
    && !exclusions.should_exclude(path, is_hidden)
{
    crawl_archive(path, &archive_crawler, &sqlite, &tantivy, &metrics).await?;
}
```

```rust
async fn crawl_archive(
    archive_path: &Path,
    crawler:  &ArchiveCrawler<'_>,
    sqlite:   &Arc<SqliteStore>,
    tantivy:  &Arc<TantivyStore>,
    metrics:  &Arc<IndexMetrics>,
) -> Result<()> {
    sqlite.remove_archive_entries(archive_path.to_str().unwrap())?;
    // Batch all upserts in a single SQLite transaction:
    let conn = sqlite.pool.get()?;
    conn.execute_batch("BEGIN")?;
    for entry_result in crawler.crawl(archive_path, 0)? {
        let entry = entry_result?;
        let record = archive_entry_to_record(&entry);
        sqlite.upsert_file_conn(&conn, &record)?;
        if let Some(content) = &entry.content {
            tantivy.upsert(&record, Some(content))?;
            metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
        }
        metrics.archive_entries_indexed.fetch_add(1, Ordering::Relaxed);
    }
    conn.execute_batch("COMMIT")?;
    metrics.archives_crawled.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
```

### 5.2 Incremental Update

New `IndexEvent` variants:

```rust
pub enum IndexEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
    ArchiveModified(PathBuf),
    ArchiveDeleted(PathBuf),
}
```

| Event | Action |
|-------|--------|
| `Created(zip)` | Treat as `ArchiveModified` |
| `Modified(zip)` | `remove_archive_entries()` + `crawl_archive()` |
| `Deleted(zip)` | `remove_archive_entries()` + remove ZIP's own record |
| `Renamed { from: zip, to: new }` | Remove old entries, update ZIP record, `crawl_archive(new)` |

The watcher converts `notify::Event` to `ArchiveModified`/`ArchiveDeleted`
when the changed path has an extension in `config.archive_index.formats`.

### 5.3 IndexMetrics Additions

```rust
pub struct IndexMetrics {
    // ... existing fields ...
    pub archive_entries_indexed: AtomicU64,
    pub archives_crawled:        AtomicU64,
}
```

---

## 6. Content Module Changes (`indexer/content.rs`)

### 6.1 New Function

```rust
/// Extract indexable text from raw bytes (archive entry content).
/// `buf` should already be truncated to content limit by caller.
/// Returns None if binary detected.
pub fn extract_content_from_bytes(buf: &[u8]) -> Option<String> {
    if is_binary(buf) {
        return None;
    }
    let (cow, _, _) = encoding_rs::UTF_8.decode(buf);
    Some(cow.into_owned())
}
```

---

## 7. Query Engine Changes

### 7.1 AST Changes (`query/ast.rs`)

Add to `Modifiers`:

```rust
/// None  = include both real files and inner archive entries.
/// Some(true)  = archive: modifier - inner entries only.
/// Some(false) = noarchive: modifier - real filesystem files only.
pub archive_only: Option<bool>,
```

Add to `Function` enum:

```rust
/// Find all inner files whose outer archive matches path (glob supported).
ArchiveOf(String),
```

### 7.2 Evaluator Changes (`query/evaluator.rs`)

Add to `SqlCondition`:

```rust
IsArchiveEntry(bool),  // true: archive_path IS NOT NULL; false: IS NULL
ArchiveOf(String),     // archive_path = ? (exact) or GLOB ? (wildcard)
```

Dispatch in `plan()`:

```
modifier archive:     → SqlCondition::IsArchiveEntry(true)
modifier noarchive:   → SqlCondition::IsArchiveEntry(false)
archiveof:<path>      → SqlCondition::ArchiveOf(path)
```

SQL generation:
```sql
-- IsArchiveEntry(true):
archive_path IS NOT NULL

-- IsArchiveEntry(false):
archive_path IS NULL

-- ArchiveOf("/data/docs.zip") exact:
archive_path = '/data/docs.zip'

-- ArchiveOf("*.zip") glob:
archive_path GLOB '*.zip'
```

### 7.3 `parent:` Modifier with Virtual Paths

`parent:/data/docs.zip!/src` finds inner files in the `src/` subdirectory:

```sql
path LIKE '/data/docs.zip!/src/%' AND path NOT LIKE '/data/docs.zip!/src/%/%'
```

No special handling needed - `ParentPath` SQL already generates this.

### 7.4 `depth:` with Virtual Paths

`depth:` counts `/` separators in the `path` column. `!/` counts as a
separator. No special handling needed.

---

## 8. API Changes

### 8.1 REST API - SearchHit Response

```json
{
  "path": "/data/project.zip!/src/main.rs",
  "name": "main.rs",
  "extension": "rs",
  "size_bytes": 4096,
  "size_human": "4.0 KB",
  "mtime": 1712345678,
  "mtime_human": "2024-04-05 12:34:38",
  "is_dir": false,
  "score": 0.92,
  "snippet": "fn <mark>main</mark>() {",
  "archive_path": "/data/project.zip",
  "archive_entry": "src/main.rs"
}
```

For real filesystem files, `archive_path` and `archive_entry` are `null`.

### 8.2 `/stats` Response Additions

```json
{
  "archive_entries_indexed": 12000,
  "archives_crawled": 150
}
```

### 8.3 `POST /open` for Inner Archive Entries

Detect inner entries by `archive_path IS NOT NULL`:

```rust
async fn open_inner_file(
    archive_path: &str,
    entry_name:   &str,
    state:        &AppState,
) -> Result<(), AppError> {
    // 1. Open outer ZIP
    // 2. Find entry by name
    // 3. Read bytes into NamedTempFile in OS temp dir (original filename)
    // 4. Open temp file with OS default handler
    // 5. Schedule temp file deletion after 30s (tokio::spawn + sleep, best-effort)
    // Returns 400 if outer ZIP missing or entry not found
    // Returns 200 with {"opened": true, "temp_path": "/tmp/main.rs"}
}
```

### 8.4 gRPC Proto Changes

```protobuf
message SearchResult {
  // ... existing fields ...
  string archive_path  = 10;  // empty for real files
  string archive_entry = 11;  // empty for real files
}

message StatsResponse {
  // ... existing fields ...
  int64 archive_entries_indexed = 10;
  int64 archives_crawled        = 11;
}
```

### 8.5 IPC Protocol

Schema-less JSON; new fields are added automatically by serde. No version bump.

---

## 9. TUI Changes (`frontend/cli.rs`)

### 9.1 Result Display

```
> /data/project.zip!/src/main.rs     4KB  rs   [zip]
  /home/user/notes.zip!/readme.md   12KB  md   [zip]
  /home/user/documents/report.md    18KB  md
```

`[zip]` tag is the basename of the outer archive, displayed in dim style.

### 9.2 Open Behaviour

Pressing `Enter` on an inner file:
1. Sends `POST /open` with the virtual path.
2. Daemon extracts to temp file and opens.
3. TUI shows brief status: `Extracting…` then `Opened.`

### 9.3 Tab Mode Cycle Addition

New cycle: `exact → fuzzy: → regex: → content: → archive: → [clear]`

When `archive:` mode is active, prefix current query with `archive: ` before
sending to daemon.

---

## 10. New Cargo Dependencies

Add to `crates/filesearch/Cargo.toml`:

```toml
zip      = { version = "2", default-features = false, features = ["deflate"] }
tempfile = "3"
```

If `zip` is already in `[workspace.dependencies]`, reference with
`{ workspace = true }`. Both `filesearch` and `rar2zip` must use the same
major version.

---

## 11. Performance Considerations

| Concern | Mitigation |
|---------|-----------|
| ZIP with 100k entries | `max_entries_per_archive` limit (default 10k) |
| ZIP with huge individual entries | `inner_extension_limits` byte cap |
| ZIP on slow network mount | `skip_network_content = true` skips content; metadata still indexed |
| Re-indexing on every event | Watcher debounce 500ms coalesces events |
| Nested ZIP bomb | `max_nesting_depth = 0` default; depth > 2 produces error |
| Memory during content read | Bounded `read_to_end` capped at `content_limit` |
| SQLite lock contention | `crawl_archive()` batches upserts in one transaction per archive |
