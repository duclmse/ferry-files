use rusqlite::params_from_iter;

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

CREATE INDEX IF NOT EXISTS idx_files_name      ON files(name);
CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
CREATE INDEX IF NOT EXISTS idx_files_mtime     ON files(mtime);
CREATE INDEX IF NOT EXISTS idx_files_size      ON files(size_bytes);
CREATE INDEX IF NOT EXISTS idx_files_parent    ON files(parent);
CREATE INDEX IF NOT EXISTS idx_files_attrib    ON files(attrib_bitmask);
CREATE INDEX IF NOT EXISTS idx_files_archive   ON files(archive_path)
    WHERE archive_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS index_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

// Migration: add columns if this is an existing database that pre-dates them.
// SQLite does not support ADD COLUMN IF NOT EXISTS before 3.37, so we catch the
// "duplicate column name" error and ignore it.
pub(super) const MIGRATIONS: &[&str] = &[
    "ALTER TABLE files ADD COLUMN parent           TEXT DEFAULT NULL",
    "ALTER TABLE files ADD COLUMN watch_root       TEXT DEFAULT NULL",
    "ALTER TABLE files ADD COLUMN crawl_generation INTEGER DEFAULT NULL",
    // Re-point the parent index to the actual parent column.
    "DROP INDEX IF EXISTS idx_files_parent",
    "CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent)",
];

pub(super) fn apply_migrations(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    for sql in MIGRATIONS {
        match conn.execute_batch(sql) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive the parent directory path from a file path.
/// Works for both real paths (`/a/b/c.rs` → `/a/b`) and virtual archive paths
/// (`/a/arc.zip!/src/c.rs` → `/a/arc.zip!/src`).
pub(super) fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        None | Some(0) => String::new(),
        Some(i) => path[..i].to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Row mapper
// ---------------------------------------------------------------------------

pub(super) fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id:              Some(row.get(0)?),
        path:            row.get(1)?,
        name:            row.get(2)?,
        extension:       row.get(3)?,
        size_bytes:      row.get::<_, i64>(4)? as u64,
        mtime:           row.get(5)?,
        ctime:           row.get(6)?,
        is_dir:          row.get::<_, i64>(7)? != 0,
        is_hidden:       row.get::<_, i64>(8)? != 0,
        is_symlink:      row.get::<_, i64>(9)? != 0,
        attrib_bitmask:  row.get::<_, i64>(10)? as u32,
        content_indexed: row.get::<_, i64>(11)? != 0,
        tantivy_id:      row.get(12)?,
        indexed_at:      row.get(13)?,
        archive_path:    row.get(14)?,
    })
}

pub(super) fn query_records(
    stmt: &mut rusqlite::Statement<'_>,
    params: Vec<rusqlite::types::Value>,
) -> Result<Vec<FileRecord>> {
    let rows = stmt.query_map(params_from_iter(params.iter()), row_to_record)?;
    rows.map(|r| r.map_err(AppError::Database)).collect()
}
