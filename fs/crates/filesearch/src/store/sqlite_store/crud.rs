use std::path::Path;

use r2d2_sqlite::SqliteConnectionManager;

use crate::error::{AppError, Result};
use crate::store::FileRecord;

use super::SqliteStore;
use super::schema::{SCHEMA, apply_migrations, parent_of, row_to_record};

impl SqliteStore {
    pub fn new(db_path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA busy_timeout=5000;",
            )?;
            Ok(())
        });
        let pool = r2d2::Pool::builder()
            .max_size(16)
            .build(manager)
            .map_err(|e| AppError::Index(e.to_string()))?;

        let conn = pool.get().map_err(|e| AppError::Index(e.to_string()))?;
        conn.execute_batch(SCHEMA)?;
        apply_migrations(&conn)?;

        Ok(Self { pool })
    }

    pub(crate) fn get_conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.pool.get().map_err(|e| AppError::Index(e.to_string()))
    }

    // -----------------------------------------------------------------------
    // Upsert / delete
    // -----------------------------------------------------------------------

    pub fn upsert_file(&self, file: &FileRecord) -> Result<i64> {
        let conn = self.get_conn()?;
        self.upsert_file_conn(&conn, file)
    }

    pub fn upsert_file_conn(
        &self,
        conn: &rusqlite::Connection,
        file: &FileRecord,
    ) -> Result<i64> {
        let parent = parent_of(&file.path);
        conn.execute(
            "INSERT INTO files (
                path, name, extension, size_bytes, mtime, ctime,
                is_dir, is_hidden, is_symlink, attrib_bitmask,
                content_indexed, tantivy_id, indexed_at, archive_path, parent
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
             ON CONFLICT(path) DO UPDATE SET
                name             = excluded.name,
                extension        = excluded.extension,
                size_bytes       = excluded.size_bytes,
                mtime            = excluded.mtime,
                ctime            = excluded.ctime,
                is_dir           = excluded.is_dir,
                is_hidden        = excluded.is_hidden,
                is_symlink       = excluded.is_symlink,
                attrib_bitmask   = excluded.attrib_bitmask,
                content_indexed  = excluded.content_indexed,
                tantivy_id       = excluded.tantivy_id,
                indexed_at       = excluded.indexed_at,
                archive_path     = excluded.archive_path,
                parent           = excluded.parent",
            rusqlite::params![
                &file.path,
                &file.name,
                &file.extension,
                file.size_bytes as i64,
                file.mtime,
                file.ctime,
                i64::from(file.is_dir),
                i64::from(file.is_hidden),
                i64::from(file.is_symlink),
                file.attrib_bitmask,
                i64::from(file.content_indexed),
                &file.tantivy_id,
                file.indexed_at,
                &file.archive_path,
                parent,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Upsert a batch of crawl records in a single transaction.
    /// Combines the former `upsert_file` + `mark_seen` into one SQL round-trip per
    /// record, and wraps the whole batch in a single BEGIN/COMMIT - turning 2N
    /// individual transactions into ⌈N/batch_size⌉ transactions.
    pub fn batch_upsert_crawl(
        &self,
        batch:      &[(FileRecord, String)],
        generation: i64,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let conn = self.get_conn()?;
        conn.execute_batch("BEGIN").map_err(AppError::Database)?;

        let result: Result<()> = (|| {
            let mut stmt = conn.prepare(
                "INSERT INTO files (
                    path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path, parent,
                    watch_root, crawl_generation
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
                 ON CONFLICT(path) DO UPDATE SET
                    name             = excluded.name,
                    extension        = excluded.extension,
                    size_bytes       = excluded.size_bytes,
                    mtime            = excluded.mtime,
                    ctime            = excluded.ctime,
                    is_dir           = excluded.is_dir,
                    is_hidden        = excluded.is_hidden,
                    is_symlink       = excluded.is_symlink,
                    attrib_bitmask   = excluded.attrib_bitmask,
                    content_indexed  = excluded.content_indexed,
                    tantivy_id       = excluded.tantivy_id,
                    indexed_at       = excluded.indexed_at,
                    archive_path     = excluded.archive_path,
                    parent           = excluded.parent,
                    watch_root       = excluded.watch_root,
                    crawl_generation = excluded.crawl_generation",
            ).map_err(AppError::Database)?;

            for (record, watch_root) in batch {
                let parent = parent_of(&record.path);
                stmt.execute(rusqlite::params![
                    &record.path,
                    &record.name,
                    &record.extension,
                    record.size_bytes as i64,
                    record.mtime,
                    record.ctime,
                    i64::from(record.is_dir),
                    i64::from(record.is_hidden),
                    i64::from(record.is_symlink),
                    record.attrib_bitmask,
                    i64::from(record.content_indexed),
                    &record.tantivy_id,
                    record.indexed_at,
                    &record.archive_path,
                    parent,
                    watch_root,
                    generation,
                ]).map_err(AppError::Database)?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => conn.execute_batch("COMMIT").map_err(AppError::Database),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Same as `upsert_file_conn` but also stamps `watch_root`/`crawl_generation`.
    /// Used for archive-entry rows produced during a full crawl so they carry the
    /// current generation, exactly like real files inserted via `batch_upsert_crawl`.
    /// Without this, entries land with `crawl_generation IS NULL`, which
    /// `remove_stale_for_root`'s legacy-row fallback treats as untracked and
    /// deletes immediately after the walk that just inserted them finishes.
    pub(super) fn upsert_archive_entry_conn(
        &self,
        conn: &rusqlite::Connection,
        file: &FileRecord,
        watch_root: &str,
        generation: i64,
    ) -> Result<i64> {
        let parent = parent_of(&file.path);
        conn.execute(
            "INSERT INTO files (
                path, name, extension, size_bytes, mtime, ctime,
                is_dir, is_hidden, is_symlink, attrib_bitmask,
                content_indexed, tantivy_id, indexed_at, archive_path, parent,
                watch_root, crawl_generation
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
             ON CONFLICT(path) DO UPDATE SET
                name             = excluded.name,
                extension        = excluded.extension,
                size_bytes       = excluded.size_bytes,
                mtime            = excluded.mtime,
                ctime            = excluded.ctime,
                is_dir           = excluded.is_dir,
                is_hidden        = excluded.is_hidden,
                is_symlink       = excluded.is_symlink,
                attrib_bitmask   = excluded.attrib_bitmask,
                content_indexed  = excluded.content_indexed,
                tantivy_id       = excluded.tantivy_id,
                indexed_at       = excluded.indexed_at,
                archive_path     = excluded.archive_path,
                parent           = excluded.parent,
                watch_root       = excluded.watch_root,
                crawl_generation = excluded.crawl_generation",
            rusqlite::params![
                &file.path,
                &file.name,
                &file.extension,
                file.size_bytes as i64,
                file.mtime,
                file.ctime,
                i64::from(file.is_dir),
                i64::from(file.is_hidden),
                i64::from(file.is_symlink),
                file.attrib_bitmask,
                i64::from(file.content_indexed),
                &file.tantivy_id,
                file.indexed_at,
                &file.archive_path,
                parent,
                watch_root,
                generation,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        let conn = self.get_conn()?;
        conn.execute("DELETE FROM files WHERE path = ?", [path])?;
        Ok(())
    }

    pub fn remove_directory(&self, path: &str) -> Result<usize> {
        let conn = self.get_conn()?;
        let n = conn.execute(
            "DELETE FROM files WHERE path LIKE ? || '/%'",
            [path],
        )?;
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Crawl generation management (mark-and-sweep reconciliation)
    // -----------------------------------------------------------------------

    /// Atomically increment the generation counter and return the new value.
    /// The counter is stored as `crawl_generation` in `index_meta`.
    pub fn begin_crawl_generation(&self) -> Result<i64> {
        let conn = self.get_conn()?;
        conn.execute(
            "INSERT INTO index_meta (key, value) VALUES ('crawl_generation', '1')
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            [],
        )?;
        let gen: i64 = conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM index_meta WHERE key = 'crawl_generation'",
            [],
            |row| row.get(0),
        )?;
        Ok(gen)
    }

    /// Delete all rows under `watch_root` whose `crawl_generation` is older than
    /// `generation`. Uses a path-prefix predicate so rows that were present
    /// before watch_root tracking was introduced are also cleaned up. No paths
    /// are loaded into Rust memory.
    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> Result<usize> {
        let conn = self.get_conn()?;
        // Rows that are newly upserted in this crawl get watch_root set by
        // mark_seen(). Rows never marked have watch_root = NULL, so we cover
        // both cases with a path prefix OR watch_root predicate.
        let n = conn.execute(
            "DELETE FROM files
             WHERE (watch_root = ?1 OR (watch_root IS NULL AND path LIKE ?1 || '/%'))
               AND (crawl_generation IS NULL OR crawl_generation < ?2)",
            rusqlite::params![watch_root, generation],
        )?;
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    pub fn get_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files WHERE path = ?",
        )?;
        let mut rows = stmt.query([path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_record(row)?))
        } else {
            Ok(None)
        }
    }

    // -----------------------------------------------------------------------
    // Index metadata (key-value)
    // -----------------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.get_conn()?;
        let result = conn.query_row(
            "SELECT value FROM index_meta WHERE key = ?",
            [key],
            |row| row.get(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AppError::Database(e)),
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.get_conn()?;
        conn.execute(
            "INSERT INTO index_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }
}
