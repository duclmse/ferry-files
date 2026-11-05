# Phase 2 - Data Stores

**Goal:** Implement both persistent storage backends used by `filesearch`
(SQLite metadata store + Tantivy full-text store) and the SQLite job queue used
by `rar2zip-transfer`. All store code is tested in isolation before any business
logic touches it.

**Estimated scope:** ~900 lines of Rust.

---

## 2.1 filesearch - SQLite Metadata Store (`store/sqlite_store.rs`)

### Schema

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA cache_size=-65536;   -- 64 MB page cache

CREATE TABLE IF NOT EXISTS files (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    path             TEXT NOT NULL UNIQUE,
    name             TEXT NOT NULL,
    extension        TEXT,
    size_bytes       INTEGER NOT NULL,
    mtime            INTEGER NOT NULL,
    ctime            INTEGER NOT NULL,
    is_dir           INTEGER NOT NULL,
    is_hidden        INTEGER NOT NULL,
    is_symlink       INTEGER NOT NULL,
    content_indexed  INTEGER NOT NULL DEFAULT 0,
    tantivy_id       TEXT,
    indexed_at       INTEGER NOT NULL,
    attrib_bitmask   INTEGER NOT NULL DEFAULT 0,
    archive_path     TEXT    DEFAULT NULL,       -- ZIP inner streaming; NULL = real file
    parent           TEXT    DEFAULT NULL,       -- direct parent dir; indexed for O(log n) child queries
    watch_root       TEXT    DEFAULT NULL,       -- watch root this file belongs to (set by mark_seen)
    crawl_generation INTEGER DEFAULT NULL        -- generation counter set by mark_seen; NULL = pre-generation
);

CREATE INDEX IF NOT EXISTS idx_files_name      ON files(name);
CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
CREATE INDEX IF NOT EXISTS idx_files_mtime     ON files(mtime);
CREATE INDEX IF NOT EXISTS idx_files_size      ON files(size_bytes);
CREATE INDEX IF NOT EXISTS idx_files_parent    ON files(parent);  -- NB: was ON files(path) - fixed
CREATE INDEX IF NOT EXISTS idx_files_attrib    ON files(attrib_bitmask);
CREATE INDEX IF NOT EXISTS idx_files_archive   ON files(archive_path)
    WHERE archive_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS index_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

> **Note:** `attrib_bitmask` from the query addendum is included from the start
> to avoid a schema migration later.

### Key Methods

| Method                                 | SQL                                                                                     |
| -------------------------------------- | --------------------------------------------------------------------------------------- |
| `upsert_file(record)`                  | `INSERT OR REPLACE INTO files ...`                                                      |
| `remove_file(path)`                    | `DELETE FROM files WHERE path = ?`                                                      |
| `remove_directory(path)`               | `DELETE FROM files WHERE path LIKE ? \|\| '/%'`                                         |
| `get_by_path(path)`                    | `SELECT * FROM files WHERE path = ?`                                                    |
| `search_name(name, filters, limit)`    | `SELECT * FROM files WHERE name LIKE ? [+ filters]`                                     |
| `search_meta(filters, limit)`          | Dynamic WHERE from MetaFilters                                                          |
| `paths_under(dir)`                     | `SELECT path FROM files WHERE path LIKE ? \|\| '/%'`                                    |
| `has_child(parent, pattern)`           | `SELECT COUNT(*) FROM files WHERE parent = ? AND name LIKE ?` (indexed)                 |
| `find_dupes(kind, conditions, limit)`  | Two-pass GROUP BY + IN query                                                            |
| `total_files()`                        | `SELECT COUNT(*) FROM files WHERE archive_path IS NULL`                                 |
| `total_archive_entries()`              | `SELECT COUNT(*) FROM files WHERE archive_path IS NOT NULL`                             |
| `remove_archive_entries(archive_path)` | Delete all inner records for a given ZIP path                                           |
| `paths_in_archive(archive_path)`       | Return all virtual paths belonging to a ZIP                                             |
| `begin_crawl_generation()`             | Increment and return `crawl_generation` counter in index_meta                           |
| `mark_seen(path, watch_root, gen)`     | `UPDATE files SET watch_root=?, crawl_generation=? WHERE path=?`                        |
| `remove_stale_for_root(root, gen)`     | `DELETE FROM files WHERE (watch_root=? OR path LIKE root \|\| '/%') AND generation < ?` |

### MetaFilters → WHERE clauses

```rust
pub struct MetaFilters {
    pub min_size:    Option<u64>,
    pub max_size:    Option<u64>,
    pub after:       Option<i64>,
    pub before:      Option<i64>,
    pub is_dir:      Option<bool>,
    pub extensions:  Option<Vec<String>>,
    pub path_prefix: Option<String>,
}
```

Each `Some` field appends an `AND` clause. The query builder accumulates
conditions and bind parameters separately to avoid SQL injection.

### attrib_bitmask Constants

```
Archive=1, Compressed=2, Directory=4, Encrypted=8,
Hidden=16, ReparsePoint=32, Normal=64, Offline=128,
Sparse=256, ReadOnly=512, System=1024, Temporary=2048
```

---

## 2.2 filesearch - Tantivy Full-Text Store (`store/tantivy_store.rs`)

### Schema Fields

| Field        | Type | Stored | Indexed | Notes                                      |
| ------------ | ---- | ------ | ------- | ------------------------------------------ |
| `path`       | Text | Yes    | Yes     | Used for dedup and retrieval               |
| `name`       | Text | Yes    | Yes     | Tokenised filename                         |
| `content`    | Text | No     | Yes     | Too large to store; fetch from disk on hit |
| `extension`  | Text | Yes    | Yes     |                                            |
| `mtime`      | U64  | Yes    | Fast    | For sorting                                |
| `size_bytes` | U64  | Yes    | Fast    | For filtering                              |

### Upsert Pattern

Since Tantivy has no native upsert:

1. Delete existing document(s) matching `path` term.
2. Add new document.
3. Do NOT commit immediately - batch commits via background task.

### Commit Strategy

```
tokio::spawn(async {
    loop {
        tokio::time::sleep(Duration::from_secs(commit_interval_secs)).await;
        store.commit()?;
        store.reload_reader()?;
    }
})
```

Also commit after bulk crawl completion.

### Snippet Generation

Use `tantivy::SnippetGenerator` on the `content` field to generate highlighted
excerpts (wrap matches in `<mark>...</mark>`).

---

## 2.3 rar2zip - Job Queue (`queue.rs`)

### Schema

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
    destinations TEXT NOT NULL,    -- JSON array of host strings
    retry_count  INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    error        TEXT
);

CREATE TABLE IF NOT EXISTS multipart_groups (
    group_id   TEXT NOT NULL,
    part_path  TEXT NOT NULL,
    arrived_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, part_path)
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS idx_jobs_hash   ON jobs(file_hash);
```

### Key Methods

| Method                                 | Purpose                                                                                               |
| -------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `enqueue(path, hash, strategy, dests)` | Insert new `pending` job, return id                                                                   |
| `set_status(id, status, error)`        | Update status + optional error text                                                                   |
| `increment_retry(id)`                  | `retry_count += 1`                                                                                    |
| `get_stale_jobs()`                     | All jobs with status `converting` or `transferring`                                                   |
| `get_failed_jobs()`                    | All jobs with status `failed`                                                                         |
| `reset_failed()`                       | Set all `failed` → `pending`, zero retries, return count                                              |
| `is_duplicate(hash)`                   | True if any `done` job with this hash exists                                                          |
| `queue_depth()`                        | `HashMap<status, count>`                                                                              |
| `recent_jobs(limit)`                   | Last N done/failed jobs by `updated_at DESC`                                                          |
| `add_part(group, path)`                | Insert multipart_groups row                                                                           |
| `get_parts(group)`                     | Return all paths for a group                                                                          |
| `clear_parts(group)`                   | Delete all rows for a group                                                                           |
| `get_expired_groups(timeout_hours)`    | Return group_ids where oldest part arrived > timeout_hours ago and no active job exists for the group |

---

## 2.4 Connection Pool Configuration

Both stores use `r2d2 + r2d2_sqlite`:

```rust
let manager = SqliteConnectionManager::file(db_path)
    .with_init(|conn| {
        conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA foreign_keys=ON;
        ")?;
        Ok(())
    });
let pool = r2d2::Pool::builder()
    .max_size(16)
    .build(manager)?;
```

---

## 2.5 Tests - Phase 2

### filesearch SqliteStore tests

```
- upsert_file + get_by_path → same record returned
- remove_file → get_by_path returns None
- remove_directory → all children removed, parent untouched
- search_name with extension filter → correct subset
- search_meta with size range → correct subset
- paths_under returns all paths under prefix
- concurrent upserts from 10 threads → all succeed, total_files correct
- has_child: insert child, query parent → true; no child → false
- upsert inner-file record (archive_path set) → get_by_path returns it
- remove_archive_entries("/a.zip") → removes only inner entries, real files untouched
- paths_in_archive("/a.zip") → returns correct virtual paths
- total_files() → excludes inner-file records (archive_path IS NOT NULL)
- total_archive_entries() → counts only inner-file records
```

### filesearch TantivyStore tests

```
- upsert + commit + search_fulltext → document found
- remove → no longer returned
- snippet contains highlighted match wrapped in <mark>
- content filter (extension) applied after Tantivy results
```

### rar2zip JobQueue tests

```
- enqueue → status=pending; set_status(converting) → get_stale_jobs returns it
- is_duplicate: enqueue, set_status(done), is_duplicate returns true
- reset_failed: enqueue 3, fail all, reset_failed returns 3, all pending
- multipart: add_part x3, get_parts → 3; clear_parts → 0
- concurrent: 10 threads enqueue simultaneously, all succeed
- queue_depth returns correct counts per status
- get_expired_groups: add parts with arrived_at = now-25h, timeout=24h → group returned
- get_expired_groups: add parts with arrived_at = now-1h, timeout=24h → empty
- get_expired_groups with timeout=0 → always empty (disabled)
```

---

## 2.6 Definition of Done

- [x] `cargo test -p filesearch store` passes.
- [x] `cargo test -p rar2zip queue` passes.
- [x] `cargo clippy -- -D warnings` clean on new files.
- [x] Schema migrations are idempotent (re-running `new()` on existing DB is
      safe).
- [x] No `unwrap()` in store code.

---

## 2.7 Implementation Notes _(added during Phase 2 implementation)_

### A. SQL typo in phase 2 spec - use ref doc schema

The phase 2 spec (§2.1) has `idx_files_size ON files(size_bytes))` - one extra
closing parenthesis. The ref doc (`01-ref-filesearch-modules.md §6.1`) is
correct. The implementation uses the ref doc version.

### B. `SqlCondition` defined with all Phase 5 variants

`store/mod.rs` defines the full set of `SqlCondition` variants upfront
(including `NameGlob`, `DepthOp`, `NameLenOp`, `AttribAll`, `AttribAny`, etc.)
rather than just the ones needed for Phase 2. This prevents breaking changes to
the store's public API when the Phase 5 query evaluator is wired up.

### C. `:memory:` SQLite is connection-private - use real temp files for concurrent tests

`SqliteConnectionManager::file(":memory:")` creates a separate in-memory
database per connection. With an r2d2 pool of 16 connections, threads picking up
different connections see empty databases. Both concurrent tests
(`concurrent_upserts_all_succeed` in `sqlite_store.rs` and
`concurrent_enqueue_all_succeed` in `queue.rs`) were updated to use
`tempfile::NamedTempFile` so all pool connections share the same real file. This
is also the realistic scenario in production.

### D. `tantivy::schema::Value` trait must be in scope

Methods like `as_str()` and `as_u64()` on `OwnedValue` are provided by the
`tantivy::schema::Value` trait, not inherent methods. Import
`use tantivy::schema::Value;` is required wherever field values are read from a
retrieved document.

### E. `MmapDirectory::open` error type is `OpenDirectoryError`, not `TantivyError`

`tantivy::directory::MmapDirectory::open(path)` returns
`Result<MmapDirectory, OpenDirectoryError>`. Since `AppError` has
`#[from] tantivy::TantivyError` (not `#[from] OpenDirectoryError`), the
conversion requires `.map_err(|e| AppError::Tantivy(e.into()))` - the extra
`.into()` converts `OpenDirectoryError` to `TantivyError` via its blanket impl.

### F. `store/mod.rs` module layout

Shared types (`FileRecord`, `SearchHit`, `MetaFilters`, `DupeKind`,
`SqlCondition`, `CmpOp`, bitmask constants) live in `store/mod.rs`. Both
`sqlite_store` and `tantivy_store` import from `super::*`. `lib.rs` exposes
`pub mod store;` - the two sub-modules are re-exported through it.
