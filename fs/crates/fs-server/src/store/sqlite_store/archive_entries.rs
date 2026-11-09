use crate::error::{AppError, Result};
use crate::store::FileRecord;

use super::SqliteStore;

impl SqliteStore {
    // -----------------------------------------------------------------------
    // Archive inner-file methods
    // -----------------------------------------------------------------------

    /// Upsert a batch of archive inner-file records in a single transaction.
    /// Replaces the former pattern of calling `get_conn()` + `upsert_file_conn()` in a loop.
    ///
    /// `tag`, when set to `(watch_root, generation)`, stamps every row with the
    /// enclosing full crawl's generation - see `upsert_archive_entry_conn` for
    /// why this matters. Event-driven archive (re)indexing outside a crawl
    /// (rename, ArchiveModified) passes `None`, matching how per-event file
    /// upserts also leave these columns NULL.
    pub fn upsert_archive_batch(
        &self,
        records: &[FileRecord],
        tag: Option<(&str, i64)>,
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let conn = self.get_conn()?;
        conn.execute_batch("BEGIN").map_err(AppError::Database)?;
        let result: Result<()> = (|| {
            for record in records {
                match tag {
                    Some((watch_root, generation)) => {
                        self.upsert_archive_entry_conn(&conn, record, watch_root, generation)?;
                    }
                    None => {
                        self.upsert_file_conn(&conn, record)?;
                    }
                }
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

    pub fn remove_archive_entries(&self, archive_path: &str) -> Result<usize> {
        let conn = self.get_conn()?;
        let n = conn.execute("DELETE FROM files WHERE archive_path = ?", [archive_path])?;
        Ok(n)
    }

    pub fn paths_in_archive(&self, archive_path: &str) -> Result<Vec<String>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare("SELECT path FROM files WHERE archive_path = ?")?;
        let rows = stmt.query_map([archive_path], |row| row.get(0))?;
        rows.map(|r| r.map_err(AppError::Database)).collect()
    }

    pub fn total_archive_entries(&self) -> Result<i64> {
        let conn = self.get_conn()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE archive_path IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    pub fn total_files(&self) -> Result<i64> {
        let conn = self.get_conn()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE archive_path IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(n)
    }

    /// Return paths of all real ZIP files in the index (not inner archive entries).
    pub fn list_archive_paths(&self) -> Result<Vec<String>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT path FROM files
             WHERE archive_path IS NULL AND is_dir = 0 AND extension = 'zip'",
        )?;
        let paths = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }
}
