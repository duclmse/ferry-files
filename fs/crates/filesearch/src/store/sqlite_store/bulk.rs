//! Initial-crawl bulk load (docs/plans/everything-borrowing.md, phase 4).
//!
//! Maintaining five secondary B-trees row by row, in random key order, made
//! the first crawl write-bound. Dropping them while an empty database is
//! filled and rebuilding them afterwards sorts each index once instead.

use rusqlite::OptionalExtension;

use crate::error::{AppError, Result};

use super::SqliteStore;

/// Indexes dropped during the bulk load. `idx_files_parent` stays because the
/// watcher's stale-sibling sweep (`children_of`) runs during the crawl and
/// would otherwise scan the whole table per event; `idx_files_archive` is a
/// small partial index used by archive indexing during the crawl.
const BULK_DROPPED: &[&str] = &[
    "idx_files_name",
    "idx_files_extension",
    "idx_files_mtime",
    "idx_files_size",
    "idx_files_attrib",
];

/// WAL auto-checkpoint threshold (pages) for crawl batches during a bulk
/// load. The default of 1000 pages (~4 MB) checkpointed after nearly every
/// 10,000-row batch, two fsyncs each, and the crawl writer spent a third of
/// its time in fsync. The WAL may grow to ~400 MB until `end_bulk_load`
/// truncates it.
pub(super) const BULK_AUTOCHECKPOINT: &str = "PRAGMA wal_autocheckpoint=100000";
/// SQLite's default; restored so pooled connections leave the crawl unchanged.
pub(super) const DEFAULT_AUTOCHECKPOINT: &str = "PRAGMA wal_autocheckpoint=1000";

/// `index_meta` key holding the `CREATE INDEX` statements to replay. Kept in
/// the database rather than in memory so the statements match whatever the
/// schema was when they were dropped; if the daemon dies mid-crawl,
/// `SqliteStore::new` recreates them from `SCHEMA` on the next start anyway.
const META_KEY: &str = "bulk_load_indexes";

impl SqliteStore {
    /// Drops the secondary indexes if the `files` table is empty. Returns
    /// whether a bulk load started; `end_bulk_load` is a no-op otherwise.
    pub fn begin_bulk_load(&self) -> Result<bool> {
        let mut conn = self.get_conn()?;
        let tx = conn.transaction()?;
        let empty: bool =
            tx.query_row("SELECT NOT EXISTS (SELECT 1 FROM files)", [], |r| r.get(0))?;
        if !empty {
            return Ok(false);
        }
        let mut sqls = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1 AND sql IS NOT NULL",
            )?;
            for name in BULK_DROPPED {
                if let Some(sql) = stmt
                    .query_row([name], |r| r.get::<_, String>(0))
                    .optional()?
                {
                    sqls.push(sql);
                }
            }
        }
        if sqls.is_empty() {
            return Ok(false);
        }
        let saved = serde_json::to_string(&sqls).map_err(|e| AppError::Index(e.to_string()))?;
        tx.execute(
            "INSERT INTO index_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [META_KEY, &saved],
        )?;
        for name in BULK_DROPPED {
            tx.execute_batch(&format!("DROP INDEX IF EXISTS {name}"))?;
        }
        // Raised before the drop is visible so no query plans against the
        // missing index; a failed commit only costs a few slower scans.
        self.set_bulk_loading(true)?;
        tx.commit()?;
        tracing::info!(
            "bulk load: dropped {} secondary indexes for the initial crawl",
            sqls.len()
        );
        Ok(true)
    }

    /// Recreates the indexes dropped by `begin_bulk_load`.
    pub fn end_bulk_load(&self) -> Result<()> {
        let conn = self.get_conn()?;
        let saved: Option<String> = conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = ?1",
                [META_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let Some(saved) = saved else { return Ok(()) };
        let sqls: Vec<String> =
            serde_json::from_str(&saved).map_err(|e| AppError::Index(e.to_string()))?;
        let start = std::time::Instant::now();
        for sql in &sqls {
            // Statements come from sqlite_master; IF NOT EXISTS makes a replay
            // after a partial rebuild harmless.
            let sql = sql.replacen("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ", 1);
            conn.execute_batch(&sql)?;
        }
        conn.execute("DELETE FROM index_meta WHERE key = ?1", [META_KEY])?;
        self.set_bulk_loading(false)?;
        // The rebuild leaves hundreds of MB of index pages in the WAL; until
        // a checkpoint moves them, every read consults the WAL index first.
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        tracing::info!(
            "bulk load: rebuilt {} indexes in {:.1}s",
            sqls.len(),
            start.elapsed().as_secs_f64(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FileRecord, SqlCondition};

    fn rec(path: &str) -> FileRecord {
        FileRecord {
            id: None,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            extension: Some("rs".into()),
            size_bytes: 1,
            mtime: 0,
            ctime: 0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: None,
        }
    }

    fn indexes(s: &SqliteStore) -> Vec<String> {
        let conn = s.get_conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn empty_db_drops_then_restores_indexes() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        let before = indexes(&s);
        assert!(s.begin_bulk_load().unwrap());
        let during = indexes(&s);
        assert!(during.contains(&"idx_files_parent".to_owned()));
        assert!(!during.contains(&"idx_files_name".to_owned()));

        let generation = s.begin_crawl_generation().unwrap();
        s.batch_upsert_crawl(&[(rec("/w/a.rs"), "/w".into())], generation)
            .unwrap();
        // Name queries must keep working while idx_files_name is gone.
        let like = vec![SqlCondition::NameLike("%a%".into())];
        assert_eq!(s.search_conditions(&like, 10).unwrap().len(), 1);
        s.end_bulk_load().unwrap();
        assert_eq!(s.search_conditions(&like, 10).unwrap().len(), 1);
        assert_eq!(indexes(&s), before);
        assert!(s.get_meta(META_KEY).unwrap().is_none());
        // A second end is a no-op.
        s.end_bulk_load().unwrap();
    }

    #[test]
    fn non_empty_db_keeps_indexes() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        s.upsert_file(&rec("/w/a.rs")).unwrap();
        let before = indexes(&s);
        assert!(!s.begin_bulk_load().unwrap());
        assert_eq!(indexes(&s), before);
    }

    #[test]
    fn restart_mid_bulk_load_recreates_indexes() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("i.db");
        let before = {
            let s = SqliteStore::new(&db).unwrap();
            let before = indexes(&s);
            assert!(s.begin_bulk_load().unwrap());
            before
        };
        let s = SqliteStore::new(&db).unwrap();
        assert_eq!(indexes(&s), before);
        s.end_bulk_load().unwrap();
        assert_eq!(indexes(&s), before);
    }
}
