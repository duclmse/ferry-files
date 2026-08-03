use crate::error::Result;

use super::SqliteStore;

impl SqliteStore {
    // -----------------------------------------------------------------------
    // Benchmark / test helpers
    // -----------------------------------------------------------------------

    /// Insert `count` synthetic file records in a single transaction.
    /// Used by `benches/search.rs` to pre-populate the index quickly.
    ///
    /// Each record has a deterministic path based on its index so the function
    /// is idempotent (repeated calls with the same count are no-ops due to
    /// `INSERT OR IGNORE`).
    pub fn populate_for_bench(&self, count: usize) -> Result<()> {
        const EXTENSIONS: &[&str] = &["rs", "toml", "md", "txt", "py", "go", "ts", "json"];
        let conn = self.get_conn()?;
        let now  = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO files
                 (path, name, extension, size_bytes, mtime, ctime,
                  is_dir, is_hidden, is_symlink, attrib_bitmask,
                  content_indexed, indexed_at, parent)
                 VALUES (?1,?2,?3,?4,?5,?6,0,0,0,0,0,?7,?8)",
            )?;

            for i in 0..count {
                let ext = EXTENSIONS[i % EXTENSIONS.len()];
                let dir = i % 10_000;
                let name = format!("file_{i}.{ext}");
                let path = format!("/bench/dir_{dir}/{name}");
                let parent = format!("/bench/dir_{dir}");
                let size: i64 = ((i % 1_000) as i64 + 1) * 512;
                stmt.execute(rusqlite::params![
                    path, name, ext, size, now, now, now, parent,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
