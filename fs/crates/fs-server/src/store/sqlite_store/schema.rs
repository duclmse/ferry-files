use rusqlite::params_from_iter;

use crate::config::SqliteIndex;
use crate::error::{AppError, Result};
use crate::store::FileRecord;

// ---------------------------------------------------------------------------

pub(super) const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA cache_size=-65536;

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
    attrib_bitmask   INTEGER NOT NULL DEFAULT 0,
    content_indexed  INTEGER NOT NULL DEFAULT 0,
    tantivy_id       TEXT,
    indexed_at       INTEGER NOT NULL,
    archive_path     TEXT DEFAULT NULL,
    parent           TEXT DEFAULT NULL,
    watch_root       TEXT DEFAULT NULL,
    crawl_generation INTEGER DEFAULT NULL
);

CREATE INDEX IF NOT EXISTS idx_files_parent    ON files(parent COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_files_archive   ON files(archive_path)
    WHERE archive_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS index_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// The `[store] indexes` choices: config name, index name, what follows
/// `ON`. Kept out of `SCHEMA`, which runs on every start and would rebuild an
/// index the config turned off.
pub(super) const OPTIONAL_INDEXES: &[(SqliteIndex, &str, &str)] = &[
    (SqliteIndex::Name, "idx_files_name", "files(name)"),
    (
        SqliteIndex::Extension,
        "idx_files_extension",
        "files(extension)",
    ),
    // A `*.ext` glob also matches names without an extension (every folder,
    // `x.rs/`, `.rs`). Checking those through `idx_files_extension` fetched
    // each row; this holds just their names, so the check reads the index.
    (
        SqliteIndex::Extension,
        NOEXT_NAME_INDEX,
        "files(name) WHERE extension IS NULL",
    ),
    (SqliteIndex::Mtime, "idx_files_mtime", "files(mtime)"),
    (SqliteIndex::Size, "idx_files_size", "files(size_bytes)"),
    (
        SqliteIndex::Attrib,
        "idx_files_attrib",
        "files(attrib_bitmask)",
    ),
];

pub(super) const NOEXT_NAME_INDEX: &str = "idx_files_noext_name";

// Migration: add columns if this is an existing database that pre-dates them.
// SQLite does not support ADD COLUMN IF NOT EXISTS before 3.37, so we catch the
// "duplicate column name" error and ignore it.
pub(super) const MIGRATIONS: &[&str] = &[
    "ALTER TABLE files ADD COLUMN parent           TEXT DEFAULT NULL",
    "ALTER TABLE files ADD COLUMN watch_root       TEXT DEFAULT NULL",
    "ALTER TABLE files ADD COLUMN crawl_generation INTEGER DEFAULT NULL",
];

pub(super) fn apply_migrations(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    for sql in MIGRATIONS {
        match conn.execute_batch(sql) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e),
        }
    }
    // Old databases had `idx_files_parent` on another column, or on
    // `parent` without NOCASE, which `parent:` cannot use. Rebuilding it
    // unconditionally cost a large share of a 40-100 s restart at 1.4M rows,
    // so only a wrong index is rebuilt.
    let columns: Vec<(String, String)> = conn
        .prepare("SELECT name, coll FROM pragma_index_xinfo('idx_files_parent') WHERE key = 1")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if columns != [("parent".to_owned(), "NOCASE".to_owned())] {
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_files_parent;
             CREATE INDEX idx_files_parent ON files(parent COLLATE NOCASE);",
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive the parent directory path from a file path.
/// Works for both real paths (`/a/b/c.rs` → `/a/b`, `C:\a\b.rs` → `C:\a`)
/// and virtual archive paths (`/a/arc.zip!/src/c.rs` → `/a/arc.zip!/src`).
/// Splits on `\` as well, like the memory store: Windows paths use it, and
/// a root written with `/` gets children joined with `\`.
pub(super) fn parent_of(path: &str) -> String {
    crate::store::parent_text(path).to_owned()
}

/// `path` is under the directory bound to `?1`, which a Windows path joins
/// with `\` and every other path with `/`. Written as two key ranges (`0`
/// follows `/` and `]` follows `\`) so the unique index on `path` answers
/// it: the `LIKE` form was case-insensitive, so it scanned the table, 99 ms
/// per call at 1M rows, and read `_` and `%` in a folder name as wildcards.
pub(super) const UNDER_DIR_1: &str = "((path > ?1 || '/' AND path < ?1 || '0') \
     OR (path > ?1 || '\\' AND path < ?1 || ']'))";

const PARENT_REPAIR_KEY: &str = "parent_splits_backslash";

/// Rows written while `parent_of` split on `/` only have an empty or wrong
/// `parent` for any path with `\`, which hid their children from directory
/// listings and from the catch-up scan's deletion check. Runs once.
pub(super) fn repair_backslash_parents(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;
    let done = conn
        .query_row(
            "SELECT 1 FROM index_meta WHERE key = ?",
            [PARENT_REPAIR_KEY],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if done {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    let rows: Vec<(i64, String)> = tx
        .prepare("SELECT id, path FROM files WHERE instr(path, '\\') > 0")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    {
        let mut update = tx.prepare("UPDATE files SET parent = ?1 WHERE id = ?2")?;
        for (id, path) in &rows {
            update.execute(rusqlite::params![parent_of(path), id])?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO index_meta (key, value) VALUES (?1, '1')",
        [PARENT_REPAIR_KEY],
    )?;
    tx.commit()
}

// ---------------------------------------------------------------------------
// Row mapper
// ---------------------------------------------------------------------------

pub(super) fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: Some(row.get(0)?),
        path: row.get(1)?,
        name: row.get(2)?,
        extension: row.get(3)?,
        size_bytes: row.get::<_, i64>(4)? as u64,
        mtime: row.get(5)?,
        ctime: row.get(6)?,
        is_dir: row.get::<_, i64>(7)? != 0,
        is_hidden: row.get::<_, i64>(8)? != 0,
        is_symlink: row.get::<_, i64>(9)? != 0,
        attrib_bitmask: row.get::<_, i64>(10)? as u32,
        content_indexed: row.get::<_, i64>(11)? != 0,
        tantivy_id: row.get(12)?,
        indexed_at: row.get(13)?,
        archive_path: row.get(14)?,
    })
}

pub(super) fn query_records(
    stmt: &mut rusqlite::Statement<'_>,
    params: Vec<rusqlite::types::Value>,
) -> Result<Vec<FileRecord>> {
    let rows = stmt.query_map(params_from_iter(params.iter()), row_to_record)?;
    rows.map(|r| r.map_err(AppError::Database)).collect()
}
