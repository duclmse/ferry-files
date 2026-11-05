# Reference: filesearch - Module Specifications

> Cross-platform file search daemon (Windows, Linux, macOS). Indexes 1–10M
> files including filenames, metadata, and full-text content. Exposes REST,
> IPC (Unix socket / Named Pipe), and gRPC APIs. Targets < 50ms search
> latency. Lives at `crates/filesearch/` in the workspace.

---

## 1. Architecture

```
┌─────────────────────────────────────────────────────────┐
│                        Daemon                           │
│                                                         │
│  ┌──────────┐   ┌────────────┐   ┌──────────────────┐   │
│  │ Watcher  │──>│  Indexer   │──>│   Index Store    │   │
│  │(per OS)  │   │  Pipeline  │   │ Tantivy + SQLite │   │
│  └──────────┘   └────────────┘   └──────────────────┘   │
│                                          │              │
│  ┌───────────────────────────────────────▼─────────┐    │
│  │               Query Engine                      │    │
│  │  Everything-compatible parser + evaluator       │    │
│  └───────────────────────────────────────┬─────────┘    │
│                                          │              │
│  ┌──────────┐  ┌──────────┐  ┌───────────▼──────────┐   │
│  │REST API  │  │ gRPC API │  │  Unix/Named Pipe API │   │
│  └──────────┘  └──────────┘  └──────────────────────┘   │
└─────────────────────────────────────────────────────────┘
         │                │               │
    ┌────▼───┐       ┌────▼───┐      ┌────▼────┐
    │CLI TUI │       │Web UI  │      │3rd party│
    │frontend│       │frontend│      │clients  │
    └────────┘       └────────┘      └─────────┘
```

---

## 2. Project Layout

```
crates/filesearch/
├── Cargo.toml
├── build.rs                      ← proto compilation
├── config.toml                   ← example config (all fields documented)
├── VERSIONS.md                   ← resolved crate versions
├── proto/
│   └── filesearch.proto          ← gRPC service definition
├── README.md
├── data/                         ← created at runtime
│   ├── tantivy/                  ← Tantivy index segments
│   └── meta.db                   ← SQLite metadata store
└── src/
    ├── main.rs                   ← CLI entrypoint, subcommand dispatch
    ├── daemon.rs                 ← daemon lifecycle, signal handling
    ├── config.rs                 ← Config struct, validation, hot-reload [tested]
    ├── error.rs                  ← unified AppError type
    ├── watcher/
    │   └── mod.rs                ← notify-debouncer-full, platform dispatch
    ├── indexer/
    │   ├── mod.rs                ← indexer pipeline, worker pool [tested]
    │   ├── crawler.rs            ← initial full crawl, parallel walk [tested]
    │   ├── archive.rs            ← ZIP inner-file crawler [tested]
    │   ├── content.rs            ← per-extension content extraction [tested]
    │   └── exclusions.rs         ← exclusion rule engine [tested]
    ├── store/
    │   ├── mod.rs                ← unified store interface
    │   ├── tantivy_store.rs      ← full-text index (Tantivy) [tested]
    │   └── sqlite_store.rs       ← metadata index (SQLite) [tested]
    ├── query/
    │   ├── mod.rs                ← public execute() entry point, QueryResult
    │   ├── ast.rs                ← Expr, Predicate, Function, Modifiers enums
    │   ├── parser.rs             ← tokeniser + recursive-descent parser [tested]
    │   ├── evaluator.rs          ← AST → QueryPlan → SQL execution [tested]
    │   ├── date.rs               ← date constant resolution + parser [tested]
    │   ├── size.rs               ← size value parser [tested]
    │   ├── glob.rs               ← glob → SQLite GLOB translation [tested]
    │   ├── dupes.rs              ← dupe search two-pass query logic [tested]
    │   └── macros.rs             ← type macro expansion, user macro resolution
    ├── api/
    │   ├── mod.rs                ← API server launcher
    │   ├── rest.rs               ← Axum HTTP REST server
    │   ├── grpc.rs               ← Tonic gRPC server
    │   └── ipc.rs                ← Unix socket / named pipe server
    └── frontend/
        └── cli.rs                ← interactive CLI/TUI frontend
```

> **Note:** `query/exact.rs`, `query/fuzzy.rs`, and `query/regex_search.rs`
> do **not** exist. All search logic lives in `evaluator.rs`. The `~` and `/`
> prefix syntax from the base spec is **not supported** - use the
> `fuzzy:` and `regex:` query modifiers instead.

---

## 3. Cargo.toml

```toml
[package]
name = "filesearch"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "filesearch"
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
common     = { path = "../common" }

# Watching
notify                = "6.1"
notify-debouncer-full = "0.3"

# Index: full-text
tantivy = "0.22"

# Index: metadata
rusqlite     = { version = "0.31", features = ["bundled"] }
r2d2         = "0.8"
r2d2_sqlite  = "0.23"

# gRPC
tonic            = "0.11"
tonic-reflection = "0.11"
prost            = "0.12"

# REST
axum       = { version = "0.7", features = ["macros"] }
tower      = "0.4"
tower-http = { version = "0.5", features = ["cors", "trace"] }
hyper      = { version = "1", features = ["full"] }

# Fuzzy matching
nucleo = "0.5"           # SIMD scorer (same engine as Helix/Zed)

# Regex
regex = "1"

# Parallel crawl
walkdir = "2"
rayon   = "1"
ignore  = "0.4"          # fast parallel walk

# Content extraction
encoding_rs = "0.8"      # detect + decode non-UTF8 text files
memmap2     = "0.9"      # mmap large files for content reading

# Archive inner-file indexing
zip      = { version = "2", default-features = false, features = ["deflate"] }
tempfile = "3"

# CLI / TUI
clap      = { version = "4", features = ["derive"] }
ratatui   = "0.26"
crossterm = "0.27"

# Utilities
glob      = "0.3"
bytes     = "1"
humansize = "2"
once_cell = "1"
dashmap   = "5"

[target.'cfg(unix)'.dependencies]
nix = { version = "0.28", features = ["fs", "process"] }

[target.'cfg(windows)'.dependencies]
windows = { version = "0.54", features = [
    "Win32_Storage_FileSystem",
    "Win32_System_Services",
]}

[build-dependencies]
tonic-build = "0.11"

[dev-dependencies]
tempfile    = "3"
tokio-test  = "0.4"
assert_fs   = "1"
```

See `VERSIONS.md` for resolved crate versions.

---

## 4. error.rs

```rust
#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Index error: {0}")]
    Index(String),

    #[error("Query error: {0}")]
    Query(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Tantivy error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("gRPC error: {0}")]
    Grpc(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Content extraction error: {0}")]
    Content(String),

    #[error("Common error: {0}")]
    Common(#[from] common::CommonError),
}

pub type Result<T> = std::result::Result<T, AppError>;
```

---

## 5. config.rs [tested]

### 5.1 Structs

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub watch_paths:   Vec<PathBuf>,
    pub data_dir:      PathBuf,
    pub log_dir:       PathBuf,
    pub exclusions:    ExclusionConfig,
    pub content:       ContentConfig,
    pub api:           ApiConfig,
    pub search:        SearchConfig,
    pub indexer:       IndexerConfig,
    pub archive_index: ArchiveIndexConfig,
    pub macros:        MacroConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExclusionConfig {
    pub path_patterns:        Vec<String>,  // glob patterns
    pub exclude_hidden:       bool,         // default false
    pub max_content_size_bytes: u64,        // 0 = no limit
    pub skip_network_content: bool,         // default false
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentConfig {
    /// Map of file extension → max bytes to index.
    /// Extension "*" acts as a default for unspecified extensions.
    pub extension_limits: HashMap<String, u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    pub rest_port:         u16,          // default 7700
    pub grpc_port:         u16,          // default 7701
    pub ipc_path:          PathBuf,
    // Linux/macOS: /tmp/filesearch.sock
    // Windows: \\.\pipe\filesearch
    pub rest_cors_origins: Vec<String>,  // default ["*"]
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchConfig {
    pub default_limit:    usize,  // default 100
    pub max_limit:        usize,  // default 10000
    pub fuzzy_threshold:      f32,    // default 0.7; range 0.0–1.0
    pub max_fuzzy_candidates: usize,  // default 50_000; 0 = no cap
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexerConfig {
    pub crawl_threads:         usize,  // default num_cpus / 2
    pub content_threads:       usize,  // default num_cpus / 2
    pub commit_interval_secs:  u64,    // default 5
    pub event_batch_size:      usize,  // default 100
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveIndexConfig {
    pub enabled:                  bool,               // default true
    pub formats:                  Vec<String>,         // default ["zip"]
    pub inner_extension_limits:   HashMap<String, u64>,
    pub max_entries_per_archive:  usize,              // default 10_000; 0 = no limit
    pub max_nesting_depth:        u8,                 // default 0
    pub passwords:                Vec<String>,         // default []
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MacroConfig {
    #[serde(flatten)]
    pub entries: HashMap<String, String>,
}
```

### 5.2 Functions

```rust
/// Load config from path. Returns AppError::Config on parse error.
pub fn load(path: &Path) -> Result<Config>;

/// Validate config. Returns Vec<String> of errors (empty = valid).
/// Checks: watch_paths exist, data_dir writable, rest_port != grpc_port,
/// fuzzy_threshold in 0.0–1.0, crawl_threads > 0,
/// archive_index.formats only contains supported values,
/// max_nesting_depth > 1 logged as warning.
pub fn validate(cfg: &Config) -> Vec<String>;

/// Watch config file for changes. On valid change, update Arc<RwLock<Config>>.
/// Invalid configs are logged and rejected (current config kept).
pub async fn watch_config(
    path: PathBuf,
    config: Arc<RwLock<Config>>,
    shutdown: CancellationToken,
);
```

### 5.3 Tests

- Load valid config → succeeds
- Missing `watch_paths` → Config error
- `fuzzy_threshold = 1.5` → validate returns error
- `rest_port == grpc_port` → validate returns error
- `archive_index.formats = ["rar"]` → validate returns error (unsupported)
- Hot reload: write updated config to temp file → Arc updated

---

## 6. SQLite Schema (`store/sqlite_store.rs`) [tested]

### 6.1 Schema

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA cache_size=-65536;   -- 64MB page cache

CREATE TABLE IF NOT EXISTS files (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    path             TEXT NOT NULL UNIQUE,
    name             TEXT NOT NULL,
    extension        TEXT,                   -- lowercase, no dot
    size_bytes       INTEGER NOT NULL,
    mtime            INTEGER NOT NULL,       -- Unix timestamp
    ctime            INTEGER NOT NULL,
    is_dir           INTEGER NOT NULL,       -- 0 or 1
    is_hidden        INTEGER NOT NULL,       -- 0 or 1
    is_symlink       INTEGER NOT NULL,       -- 0 or 1
    attrib_bitmask   INTEGER NOT NULL DEFAULT 0,
    content_indexed  INTEGER NOT NULL DEFAULT 0,
    tantivy_id       TEXT,
    indexed_at       INTEGER NOT NULL,
    archive_path     TEXT DEFAULT NULL,
    -- NULL = real filesystem file
    -- Non-NULL = virtual inner-archive entry; value is the outer ZIP path
    parent           TEXT DEFAULT NULL,
    -- Direct parent directory path, derived from `path` on upsert.
    -- For /a/b/c.rs → /a/b. For /a.zip!/src/c.rs → /a.zip!/src.
    -- Enables O(log n) direct-child queries via idx_files_parent.
    watch_root       TEXT DEFAULT NULL,
    -- Set by mark_seen() during full crawl. Used by stale-entry cleanup.
    crawl_generation INTEGER DEFAULT NULL
    -- Monotonically increasing counter set by mark_seen() each full crawl.
    -- Rows with generation < current are stale and deleted in SQL.
);

CREATE INDEX IF NOT EXISTS idx_files_name      ON files(name);
CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
CREATE INDEX IF NOT EXISTS idx_files_mtime     ON files(mtime);
CREATE INDEX IF NOT EXISTS idx_files_size      ON files(size_bytes);
CREATE INDEX IF NOT EXISTS idx_files_parent    ON files(parent);
CREATE INDEX IF NOT EXISTS idx_files_attrib    ON files(attrib_bitmask);
CREATE INDEX IF NOT EXISTS idx_files_archive   ON files(archive_path)
    WHERE archive_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS index_meta (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
-- Stores: last_crawl_time, crawl_generation, total_files
```

> **Migration note:** `parent`, `watch_root`, and `crawl_generation` are added
> via `ALTER TABLE ADD COLUMN` on existing databases during `SqliteStore::new()`.
> The old `idx_files_parent ON files(path)` index is dropped and replaced with
> `idx_files_parent ON files(parent)` in the same migration pass.

`attrib_bitmask` encoding:
```
Archive=1, Compressed=2, Directory=4, Encrypted=8, Hidden=16,
ReparsePoint=32, Normal=64, Offline=128, Sparse=256,
ReadOnly=512, System=1024, Temporary=2048
```

### 6.2 FileRecord

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id:              Option<i64>,
    pub path:            String,
    pub name:            String,
    pub extension:       Option<String>,
    pub size_bytes:      u64,
    pub mtime:           i64,
    pub ctime:           i64,
    pub is_dir:          bool,
    pub is_hidden:       bool,
    pub is_symlink:      bool,
    pub attrib_bitmask:  u32,
    pub content_indexed: bool,
    pub tantivy_id:      Option<String>,
    pub indexed_at:      i64,
    /// None for real filesystem files.
    /// Some(archive_path) for inner ZIP entries.
    pub archive_path:    Option<String>,
}
```

For inner ZIP entries: `is_hidden = false`, `is_symlink = false`, `attrib_bitmask = 0`.

### 6.3 Methods

```rust
impl SqliteStore {
    pub fn new(db_path: &Path) -> Result<Self>;
    pub fn upsert_file(&self, file: &FileRecord) -> Result<i64>;
    // Computes and stores `parent` from file.path automatically.
    pub fn upsert_file_conn(&self, conn: &rusqlite::Connection,
                             file: &FileRecord) -> Result<i64>;
    pub fn remove_file(&self, path: &str) -> Result<()>;
    pub fn remove_directory(&self, path: &str) -> Result<usize>;
    pub fn get_by_path(&self, path: &str) -> Result<Option<FileRecord>>;
    pub fn search_name(&self, name: &str, filters: &MetaFilters,
                       limit: usize) -> Result<Vec<FileRecord>>;
    pub fn search_extension(&self, ext: &str, filters: &MetaFilters,
                            limit: usize) -> Result<Vec<FileRecord>>;
    pub fn search_meta(&self, filters: &MetaFilters,
                       limit: usize) -> Result<Vec<FileRecord>>;
    pub fn total_files(&self) -> Result<i64>;
    pub fn get_meta(&self, key: &str) -> Result<Option<String>>;
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()>;
    // Use only for small subtrees. Full-crawl stale detection uses generation methods.
    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>>;
    // Uses indexed `parent = ?` for O(log n) lookup.
    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool>;
    pub fn find_dupes(&self, kind: DupeKind,
                      extra_conditions: &[SqlCondition],
                      limit: usize) -> Result<Vec<FileRecord>>;

    // Crawl generation management (mark-and-sweep reconciliation)
    pub fn begin_crawl_generation(&self) -> Result<i64>;
    pub fn mark_seen(&self, path: &str, watch_root: &str, generation: i64) -> Result<()>;
    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> Result<usize>;

    // Archive inner-file methods
    pub fn remove_archive_entries(&self, archive_path: &str) -> Result<usize>;
    pub fn paths_in_archive(&self, archive_path: &str) -> Result<Vec<String>>;
    pub fn total_archive_entries(&self) -> Result<i64>;
}
```

### 6.4 Tests

- Upsert + get_by_path roundtrip
- Remove file → get_by_path returns None
- search_name with size/extension filters
- Concurrent upserts from multiple threads succeed
- paths_under returns correct subset
- Upsert inner file with archive_path set; remove_archive_entries removes it

---

## 7. Tantivy Store (`store/tantivy_store.rs`) [tested]

### 7.1 Schema Fields

```
path        TEXT STORED INDEXED   (dedup key)
name        TEXT STORED INDEXED   (tokenised)
content     TEXT INDEXED only     (NOT stored - retrieve from disk/archive on hit)
extension   TEXT STORED INDEXED
mtime       U64 STORED FAST
size_bytes  U64 STORED FAST
```

### 7.2 Struct & Methods

```rust
pub struct TantivyStore {
    index:  tantivy::Index,
    reader: tantivy::IndexReader,
    writer: Arc<Mutex<tantivy::IndexWriter>>,
    schema: tantivy::Schema,
}

impl TantivyStore {
    pub fn new(index_dir: &Path) -> Result<Self>;
    /// Updates are delete-then-add by path term.
    pub fn upsert(&self, record: &FileRecord, content: Option<&str>) -> Result<()>;
    pub fn remove(&self, path: &str) -> Result<()>;
    /// Commit buffered writes. Call every commit_interval_secs.
    pub fn commit(&self) -> Result<()>;
    /// Full-text search. Returns (path, score) ordered by relevance.
    pub fn search_fulltext(&self, query: &str, limit: usize,
                           filters: &MetaFilters) -> Result<Vec<SearchHit>>;
    pub fn reload_reader(&self) -> Result<()>;
}
```

### 7.3 Commit Strategy

Background Tokio task calls `commit()` every `commit_interval_secs`. Also commit
after bulk crawl. `IndexWriter` heap budget: 128 MB.

### 7.4 Tests

- Upsert → search_fulltext finds it after commit
- Remove → no longer returned
- Content snippet highlighted correctly
- Extension and size filters applied post-search

---

## 8. indexer/exclusions.rs [tested]

```rust
pub struct ExclusionEngine {
    patterns:        Vec<glob::Pattern>,
    exclude_hidden:  bool,
}

impl ExclusionEngine {
    pub fn new(cfg: &ExclusionConfig) -> Result<Self>;
    pub fn should_exclude(&self, path: &Path, is_hidden: bool) -> bool;
}
```

Exclusion pipeline (short-circuit on first match):
1. If `exclude_hidden && is_hidden` → exclude.
2. For each glob pattern: if `pattern.matches(path_str)` → exclude.

### Network Mount Detection

```rust
#[cfg(target_os = "linux")]
pub fn is_network_mount(path: &Path) -> bool;
// Reads /proc/mounts, checks fstype for nfs, nfs4, cifs, smbfs

#[cfg(target_os = "macos")]
pub fn is_network_mount(path: &Path) -> bool;
// nix::sys::statfs::statfs(), checks f_fstypename

#[cfg(target_os = "windows")]
pub fn is_network_mount(path: &Path) -> bool;
// GetDriveTypeW returning DRIVE_REMOTE (4)
```

---

## 9. indexer/content.rs [tested]

```rust
/// Returns max bytes to index for this extension. None = skip content.
pub fn content_limit(path: &Path, cfg: &ContentConfig) -> Option<u64>;

/// Extract text from a file. Uses mmap for > 1 MB. Returns None if binary.
pub async fn extract_content(path: &Path, max_bytes: u64) -> Result<Option<String>>;

/// Extract text from raw bytes (used by archive crawler).
/// Returns None if binary detected.
pub fn extract_content_from_bytes(buf: &[u8]) -> Option<String>;

/// Binary detection: scan first 8KB. If > 0.1% null bytes → binary.
pub fn is_binary(buf: &[u8]) -> bool;
```

Read strategy: files ≤ 1 MB use `tokio::fs::read()`; files > 1 MB use `memmap2::Mmap`.
Encoding: try BOM via `encoding_rs::Encoding::for_bom()`, fall back to UTF-8 lossy.
Truncate decoded string at `max_bytes` at nearest UTF-8 char boundary.

---

## 10. indexer/archive.rs [tested]

> See also: [Phase 3 doc §3.7](../phases/03-indexer-and-watcher.md) and
> [ZIP inner streaming ref](zip-inner-streaming.md).

```rust
pub struct ArchiveCrawler<'cfg> {
    config:         &'cfg ArchiveIndexConfig,
    content_config: &'cfg ContentConfig,
}

impl<'cfg> ArchiveCrawler<'cfg> {
    pub fn new(config: &'cfg ArchiveIndexConfig,
               content_config: &'cfg ContentConfig) -> Self;

    /// Iterate ZIP entries one at a time. Never writes to disk; never
    /// allocates a Vec proportional to the archive's entry count.
    /// Returns `ArchiveEntryIter`, a named iterator struct that owns the
    /// open archive and yields one entry per `next()` call.
    pub fn crawl(
        &self,
        archive_path: &Path,
        nesting_depth: u8,
    ) -> Result<ArchiveEntryIter>;
}

/// Named streaming iterator returned by `ArchiveCrawler::crawl`.
/// Processes one ZIP entry at a time; safe to collect or break early.
pub struct ArchiveEntryIter { /* ... */ }
impl Iterator for ArchiveEntryIter {
    type Item = Result<ArchiveEntry>;
}

pub struct ArchiveEntry {
    pub virtual_path:         String,   // "<archive>!/<entry>"
    pub outer_archive_path:   String,   // real ZIP path on disk
    pub entry_name:           String,   // normalised to forward slashes
    pub extension:            Option<String>,
    pub size_bytes:           u64,
    pub mtime:                i64,      // from ZIP central directory
    pub is_dir:               bool,
    pub content:              Option<String>,
}

pub fn archive_entry_to_record(entry: &ArchiveEntry) -> FileRecord;
```

---

## 11. indexer/crawler.rs [tested]

### 11.1 Full Crawl

```rust
pub async fn full_crawl(
    config:  Arc<RwLock<Config>>,
    sqlite:  Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    shutdown: CancellationToken,
) -> Result<()>;
```

Algorithm:
1. Call `sqlite.begin_crawl_generation()` once at crawl start.
2. For each `watch_path`: build `ignore::WalkBuilder` with `hidden(false)`, `git_ignore(false)`.
3. Walk in parallel.
4. Apply `ExclusionEngine::should_exclude()`.
5. Build `FileRecord` from metadata (populate `attrib_bitmask` via platform helper).
6. `sqlite.upsert_file(record)` then `sqlite.mark_seen(path, watch_root, generation)`.
7. If `content_limit(path, cfg).is_some()`: extract content and call `tantivy.upsert()`.
8. If entry is a ZIP and `archive_index.enabled`: call `crawl_archive()`.
9. After each watch root finishes: `sqlite.remove_stale_for_root(watch_root, generation)` - deletes stale rows in SQL, no path list in Rust memory.
10. Final Tantivy commit, update `index_meta.last_crawl_time`.

```rust
async fn crawl_archive(
    archive_path: &Path,
    crawler:  &ArchiveCrawler<'_>,
    sqlite:   &Arc<SqliteStore>,
    tantivy:  &Arc<TantivyStore>,
    metrics:  &Arc<IndexMetrics>,
) -> Result<()>;
// Wraps all inner upserts in a single SQLite transaction per archive.
```

### 11.2 Incremental Update

```rust
pub async fn process_event(
    event:   IndexEvent,
    config:  Arc<RwLock<Config>>,
    sqlite:  Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
);

pub enum IndexEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
    ArchiveModified(PathBuf),  // ZIP changed - re-index entries
    ArchiveDeleted(PathBuf),   // ZIP deleted - remove all entries
}
```

| Event | Action |
|-------|--------|
| `Created(zip)` | Treat as `ArchiveModified` |
| `Modified(zip)` | `remove_archive_entries()` + `crawl_archive()` |
| `Deleted(zip)` | `remove_archive_entries()` + remove ZIP's own record |
| `Renamed { from, to }` | Remove old entries, update ZIP record, `crawl_archive(to)` |
| `Renamed(dir)` | Prefix-replace all children in SQLite |

### 11.3 IndexMetrics

```rust
pub struct IndexMetrics {
    pub total_files:             AtomicU64,
    pub total_dirs:              AtomicU64,
    pub content_indexed:         AtomicU64,
    pub last_crawl_secs:         AtomicU64,
    pub events_processed:        AtomicU64,
    pub archive_entries_indexed: AtomicU64,
    pub archives_crawled:        AtomicU64,
}
```

### 11.4 Platform Attribute Helper

```rust
#[cfg(target_os = "windows")]
pub fn attrib_from_metadata(meta: &std::fs::Metadata) -> u32;
// Uses std::os::windows::fs::MetadataExt::file_attributes()

#[cfg(unix)]
pub fn attrib_from_metadata(meta: &std::fs::Metadata, path: &Path) -> u32;
// Hidden: name starts with '.'; ReadOnly: mode & 0o200 == 0
```

---

## 12. watcher/mod.rs

```rust
pub async fn run_watcher(
    config:  Arc<RwLock<Config>>,
    tx:      mpsc::Sender<IndexEvent>,
    shutdown: CancellationToken,
) -> Result<()>;
```

Uses `notify-debouncer-full` with `RecommendedWatcher` (auto-selects inotify /
FSEvents / ReadDirectoryChangesW). Batch window: 500ms or `event_batch_size`,
whichever first. Dedup priority: `Delete > Rename > Modify > Create`.
Re-reads config on each batch to pick up hot-reloaded `watch_paths`.
Converts `notify::Event` to `ArchiveModified`/`ArchiveDeleted` when the changed
path has an extension in `config.archive_index.formats`.

---

## 13. query/ Module

> Full specification: [query-language.md](query-language.md).

The `query/` module implements a full voidtools Everything-compatible query
language plus the `fuzzy:` extension. The public entry point:

```rust
pub async fn execute(
    raw_query: &str,
    sqlite:    Arc<SqliteStore>,
    tantivy:   Arc<TantivyStore>,
    config:    Arc<RwLock<Config>>,
) -> Result<QueryResult, QueryError>;

pub struct QueryResult {
    pub hits:       Vec<SearchHit>,
    pub total:      usize,
    pub took_ms:    u64,
    pub mode_used:  String,
}
```

Files: `ast.rs`, `parser.rs`, `evaluator.rs`, `date.rs`, `size.rs`, `glob.rs`,
`dupes.rs`, `macros.rs`. No `exact.rs`, `fuzzy.rs`, or `regex_search.rs`.

---

## 14. proto/filesearch.proto

```protobuf
syntax = "proto3";
package filesearch;

service FileSearch {
  rpc Search       (SearchRequest)  returns (SearchResponse);
  rpc SearchStream (SearchRequest)  returns (stream SearchResult);
  rpc GetStats     (StatsRequest)   returns (StatsResponse);
  rpc Reindex      (ReindexRequest) returns (ReindexResponse);
}

message SearchRequest  { string query = 1; int32 limit = 2; int32 offset = 3; }

message SearchResult {
  string path          = 1;
  string name          = 2;
  string extension     = 3;
  int64  size_bytes    = 4;
  int64  mtime         = 5;
  float  score         = 6;
  string snippet       = 7;
  bool   is_dir        = 8;
  string archive_path  = 10;  // empty for real files
  string archive_entry = 11;  // empty for real files
}

message SearchResponse { repeated SearchResult results = 1; int32 total = 2; int64 took_ms = 3; }
message StatsRequest {}
message StatsResponse {
  int64  total_files              = 4;
  int64  total_dirs               = 5;
  int64  content_indexed          = 6;
  int64  last_crawl_secs          = 7;
  int64  events_processed         = 8;
  string index_size_human         = 9;
  int64  archive_entries_indexed  = 10;
  int64  archives_crawled         = 11;
}
message ReindexRequest  { bool full = 1; }
message ReindexResponse { bool accepted = 1; }
```

### build.rs

```rust
fn main() {
    tonic_build::compile_protos("proto/filesearch.proto")
        .unwrap_or_else(|e| panic!("protoc failed: {}", e));
}
```

---

## 15. api/rest.rs

### Routes

| Method | Path | Handler |
|--------|------|---------|
| `GET` | `/search?q=&limit=&offset=` | `search_handler` |
| `GET` | `/stats` | `stats_handler` |
| `POST` | `/reindex` body: `{"full": bool}` | `reindex_handler` |
| `GET` | `/health` | `health_handler` |
| `GET` | `/file?path=` | `file_handler` |
| `POST` | `/open` body: `{"path": "..."}` | `open_handler` |

### AppState

```rust
pub struct AppState {
    pub sqlite:      Arc<SqliteStore>,
    pub tantivy:     Arc<TantivyStore>,
    pub config:      Arc<RwLock<Config>>,
    pub metrics:     Arc<IndexMetrics>,
    pub reindex_tx:  tokio::sync::watch::Sender<bool>,
}
```

### Response Format

```json
{
  "hits": [{
    "path": "/home/user/projects/main.rs",
    "name": "main.rs",
    "extension": "rs",
    "size_bytes": 4096,
    "size_human": "4.0 KB",
    "mtime": 1712345678,
    "mtime_human": "2024-04-05 12:34:38",
    "is_dir": false,
    "score": 0.92,
    "snippet": "fn <mark>main</mark>() {",
    "archive_path": null,
    "archive_entry": null
  }],
  "total": 42,
  "took_ms": 12,
  "mode": "exact"
}
```

For inner archive entries, `archive_path` and `archive_entry` are non-null.

### `/stats` response includes

```json
{
  "total_files": 500000,
  "archive_entries_indexed": 12000,
  "archives_crawled": 150
}
```

### Open File Endpoint

```rust
// For real filesystem paths - platform dispatch:
// Linux:   Command::new("xdg-open").arg(path)
// macOS:   Command::new("open").arg(path)
// Windows: Command::new("cmd").args(["/C", "start", "", path])

// For virtual archive paths (archive_path IS NOT NULL):
// 1. Open outer ZIP, find entry by name
// 2. Read entry bytes into NamedTempFile (OS temp dir, original filename)
// 3. Open temp file with OS default handler
// 4. Schedule temp file deletion after 30 seconds (best-effort)
// Returns {"opened": true, "temp_path": "/tmp/main.rs"}
// Returns 400 if outer ZIP missing or entry not found
```

---

## 16. api/grpc.rs

Implements `FileSearch` service from `filesearch.proto` using `tonic`.
Enable `tonic_reflection` for gRPC reflection (`grpcurl` compatible).

| RPC | Implementation |
|-----|---------------|
| `Search` | `query::execute()` → map to `SearchResponse` |
| `SearchStream` | Execute query, stream results one by one |
| `GetStats` | Read from `Arc<IndexMetrics>` |
| `Reindex` | Send on `reindex_tx` watch channel |

---

## 17. api/ipc.rs

Newline-delimited JSON over Unix socket (`tokio::net::UnixListener`) /
Windows Named Pipe (`tokio::net::windows::named_pipe::ServerOptions`).

```json
// Request
{"id": "uuid", "method": "search", "params": {"q": "main.rs", "limit": 50}}
// Response
{"id": "uuid", "result": {...}}
{"id": "uuid", "error": "query parse error: ..."}
```

Methods: `search`, `stats`, `reindex`, `open`.
Remove stale socket file on startup (Unix). Remove on shutdown.
Each client connection → spawned task → read lines → dispatch → write response.

---

## 18. frontend/cli.rs

### Non-interactive Mode

```
filesearch search "main.rs"
filesearch search "fuzzy:mian" --limit 20 --json
filesearch search "content:TODO" --ext rs
filesearch search "archive: *.rs"
filesearch stats
filesearch reindex --full
filesearch open "/path/to/file"
```

All commands except `daemon` send IPC requests via `ipc_request()`.

### IPC Client

```rust
pub async fn ipc_request(
    ipc_path: &Path,
    method:   &str,
    params:   serde_json::Value,
) -> Result<serde_json::Value>;
```

### TUI Mode (`filesearch tui`)

Built with `ratatui 0.26` + `crossterm 0.27`.

```
┌─────────────────────────────────────────────────────┐
│ 🔍  [search query input box                     ]   │
├─────────────────────────────────────────────────────┤
│ > /home/user/projects/main.rs        4KB  rs        │
│   /data/project.zip!/src/main.rs     2KB  rs  [zip] │
│   /home/user/notes/readme.md        12KB  md        │
├─────────────────────────────────────────────────────┤
│ 42 results  •  12ms  •  exact  •  3.2M idx  4k zip  │
└─────────────────────────────────────────────────────┘
```

Inner archive entries show a dim `[zip]` tag (outer archive basename).
Status bar shows indexed ZIP inner entry count (`4k zip`).

| Key | Action |
|-----|--------|
| Type | Update query (debounced 100ms → IPC search) |
| ↑/↓ | Navigate results |
| Enter | Open file via `/open` (temp extract for inner entries) |
| Tab | Cycle: exact → `fuzzy:` → `regex:` → `content:` → `archive:` → [clear] |
| Ctrl+C / Esc | Exit TUI |
| F5 | Force re-index |

---

## 19. daemon.rs

### Startup Sequence

```rust
pub async fn run(config: Arc<RwLock<Config>>, shutdown: CancellationToken) -> Result<()>;
```

1. Validate config. Abort if invalid.
2. `common::logger::init(&config.log_dir)`.
3. PID check (Unix: write `data_dir/filesearch.pid`; Windows: named mutex).
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

1. Stop watcher.
2. Stop accepting new API requests (drain in-flight, max 10s).
3. Drain indexer event queue (max 10s).
4. Final Tantivy commit.
5. Close SQLite connections.
6. Remove IPC socket file (Unix).
7. Remove PID file (Unix).
8. Exit 0.

---

## 20. main.rs

```rust
#[derive(clap::Parser)]
#[command(name = "filesearch", version, about = "Fast cross-platform file search")]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    Daemon,
    Search {
        query: String,
        #[arg(long, default_value_t = 100)] limit: usize,
        #[arg(long, default_value_t = 0)]   offset: usize,
        #[arg(long)] json: bool,
        #[arg(long)] ext:  Option<String>,
        #[arg(long)] dir:  Option<String>,
    },
    Tui,
    Stats,
    Reindex { #[arg(long)] full: bool },
    Open { path: PathBuf },
    ValidateConfig,
}
```

Dispatch:
- `Daemon` → `daemon::run()`
- `Search` → `ipc_request("search", ...)`, format output
- `Tui` → `frontend::cli::run_tui()`
- `Stats` → `ipc_request("stats", ...)`, pretty-print
- `Reindex` → `ipc_request("reindex", {"full": bool})`
- `Open` → `ipc_request("open", {"path": ...})`
- `ValidateConfig` → `config::validate()`, exit 0/1

---

## 21. config.toml (Example)

```toml
watch_paths = ["/home/user", "/mnt/nas/data"]
data_dir    = "./data"
log_dir     = "./logs"

[exclusions]
path_patterns         = ["**/node_modules/**", "**/.git/**", "**/target/**",
                          "**/__pycache__/**", "**/*.sock", "**/proc/**"]
exclude_hidden        = false
max_content_size_bytes = 10485760
skip_network_content  = true

[content.extension_limits]
rs    = 1048576
go    = 1048576
py    = 1048576
js    = 524288
ts    = 524288
md    = 524288
txt   = 524288
log   = 262144
toml  = 131072
yaml  = 131072
json  = 131072
"*"   = 0

[api]
rest_port         = 7700
grpc_port         = 7701
ipc_path          = "/tmp/filesearch.sock"
rest_cors_origins = ["*"]

[search]
default_limit        = 100
max_limit            = 10000
fuzzy_threshold      = 0.7
max_fuzzy_candidates = 50000   # 0 = no cap

[indexer]
crawl_threads        = 4
content_threads      = 4
commit_interval_secs = 5
event_batch_size     = 100

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

[macros]
# work = "path:/home/user/projects"
# pics = "pic: dm:thismonth"
```

---

## 22. Platform Notes

### Linux

- inotify default limit: 8192 watches. For large trees:
  `sysctl fs.inotify.max_user_watches=524288`
- Detect this limit at runtime and log a clear error message.

### macOS

- FSEvents watches at directory level; no per-file watch limit.
- `is_network_mount` uses `nix::sys::statfs::statfs()`.

### Windows

- IPC uses Named Pipes: `\\.\pipe\filesearch`
- No PID file - use named mutex (`CreateMutexW`) to detect duplicates.
- `is_network_mount` uses `GetDriveTypeW` returning `DRIVE_REMOTE`.

---

## 23. Performance Targets

| Metric | Target |
|--------|--------|
| Exact search, 1M files | < 10ms |
| `fuzzy:` search, 1M files | < 50ms (nucleo SIMD) |
| Regex search, 1M files | < 200ms (rayon par_iter) |
| Content search | < 100ms (Tantivy BM25) |
| Initial crawl, 1M files | < 60s (rayon parallel walk) |
| Watcher event latency | < 1s (notify debouncer 500ms) |
| Index size, 1M files, no content | < 500MB |
| RAM (daemon idle) | < 200MB |
| RAM (during crawl) | < 1GB |

**No full file list in memory.** All records live in SQLite. Only a bounded
`DashMap<String, i64>` (500k entries, LRU eviction) for path→id cache.
