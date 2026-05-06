use std::path::Path;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params_from_iter;

use crate::error::{AppError, Result};
use super::{DupeKind, FileRecord, MetaFilters, SqlCondition, build_conditions};

// ---------------------------------------------------------------------------

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA cache_size=-65536;

CREATE TABLE IF NOT EXISTS files (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    path            TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,
    extension       TEXT,
    size_bytes      INTEGER NOT NULL,
    mtime           INTEGER NOT NULL,
    ctime           INTEGER NOT NULL,
    is_dir          INTEGER NOT NULL,
    is_hidden       INTEGER NOT NULL,
    is_symlink      INTEGER NOT NULL,
    attrib_bitmask  INTEGER NOT NULL DEFAULT 0,
    content_indexed INTEGER NOT NULL DEFAULT 0,
    tantivy_id      TEXT,
    indexed_at      INTEGER NOT NULL,
    archive_path    TEXT DEFAULT NULL
);

CREATE INDEX IF NOT EXISTS idx_files_name      ON files(name);
CREATE INDEX IF NOT EXISTS idx_files_extension ON files(extension);
CREATE INDEX IF NOT EXISTS idx_files_mtime     ON files(mtime);
CREATE INDEX IF NOT EXISTS idx_files_size      ON files(size_bytes);
CREATE INDEX IF NOT EXISTS idx_files_parent    ON files(path);
CREATE INDEX IF NOT EXISTS idx_files_attrib    ON files(attrib_bitmask);
CREATE INDEX IF NOT EXISTS idx_files_archive   ON files(archive_path)
    WHERE archive_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS index_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

// ---------------------------------------------------------------------------

pub struct SqliteStore {
    pool: Pool<SqliteConnectionManager>,
}

impl SqliteStore {
    pub fn new(db_path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;",
            )?;
            Ok(())
        });
        let pool = r2d2::Pool::builder()
            .max_size(16)
            .build(manager)
            .map_err(|e| AppError::Index(e.to_string()))?;

        // Apply schema
        let conn = pool.get().map_err(|e| AppError::Index(e.to_string()))?;
        conn.execute_batch(SCHEMA)?;

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
        conn.execute(
            "INSERT INTO files (
                path, name, extension, size_bytes, mtime, ctime,
                is_dir, is_hidden, is_symlink, attrib_bitmask,
                content_indexed, tantivy_id, indexed_at, archive_path
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(path) DO UPDATE SET
                name            = excluded.name,
                extension       = excluded.extension,
                size_bytes      = excluded.size_bytes,
                mtime           = excluded.mtime,
                ctime           = excluded.ctime,
                is_dir          = excluded.is_dir,
                is_hidden       = excluded.is_hidden,
                is_symlink      = excluded.is_symlink,
                attrib_bitmask  = excluded.attrib_bitmask,
                content_indexed = excluded.content_indexed,
                tantivy_id      = excluded.tantivy_id,
                indexed_at      = excluded.indexed_at,
                archive_path    = excluded.archive_path",
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

    pub fn search_name(
        &self,
        name: &str,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let (filter_sql, filter_params) = filters.build_where();
        let sql = format!(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files WHERE name LIKE ? {filter_sql} LIMIT ?"
        );
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&sql)?;

        let mut params: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Text(name.to_owned())];
        params.extend(filter_params);
        params.push(rusqlite::types::Value::Integer(limit as i64));

        query_records(&mut stmt, params)
    }

    pub fn search_extension(
        &self,
        ext: &str,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let (filter_sql, filter_params) = filters.build_where();
        let sql = format!(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files WHERE extension = ? {filter_sql} LIMIT ?"
        );
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&sql)?;

        let mut params: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Text(ext.to_owned())];
        params.extend(filter_params);
        params.push(rusqlite::types::Value::Integer(limit as i64));

        query_records(&mut stmt, params)
    }

    pub fn search_meta(
        &self,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let (filter_sql, filter_params) = filters.build_where();
        // Strip leading "AND " — base is "WHERE 1=1"
        let where_extra = if let Some(stripped) = filter_sql.strip_prefix("AND ") {
            format!("AND {stripped}")
        } else {
            filter_sql
        };
        let sql = format!(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files WHERE 1=1 {where_extra} LIMIT ?"
        );
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&sql)?;

        let mut params = filter_params;
        params.push(rusqlite::types::Value::Integer(limit as i64));

        query_records(&mut stmt, params)
    }

    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let conn = self.get_conn()?;
        let mut stmt =
            conn.prepare("SELECT path FROM files WHERE path LIKE ? || '/%'")?;
        let rows = stmt.query_map([dir], |row| row.get(0))?;
        rows.map(|r| r.map_err(AppError::Database)).collect()
    }

    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        let conn = self.get_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files
             WHERE path LIKE ? || '/%'
               AND name LIKE ?
               AND path NOT LIKE ? || '/%/%'",
            rusqlite::params![parent_path, name_pattern, parent_path],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    // -----------------------------------------------------------------------
    // Duplicate detection
    // -----------------------------------------------------------------------

    pub fn find_dupes(
        &self,
        kind: DupeKind,
        extra_conditions: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let (extra_sql, extra_params) = build_conditions(extra_conditions);

        let group_col = match kind {
            DupeKind::Name         => "name",
            DupeKind::NamePart     => "name",
            DupeKind::Size         => "size_bytes",
            DupeKind::DateModified => "mtime",
            DupeKind::DateCreated  => "ctime",
            DupeKind::DateAccessed => "mtime", // no atime in schema, fall back to mtime
            DupeKind::Attrib       => "attrib_bitmask",
        };

        // First pass: find values that appear more than once
        let pass1 = format!(
            "SELECT {group_col} FROM files
             WHERE archive_path IS NULL {extra_sql}
             GROUP BY {group_col} HAVING COUNT(*) > 1"
        );
        // Second pass: fetch all rows that match those values
        let sql = format!(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files
             WHERE {group_col} IN ({pass1})
               AND archive_path IS NULL {extra_sql}
             ORDER BY {group_col}
             LIMIT ?"
        );
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&sql)?;

        // extra_params appear twice (once per pass)
        let mut params = extra_params.clone();
        params.extend(extra_params);
        params.push(rusqlite::types::Value::Integer(limit as i64));

        query_records(&mut stmt, params)
    }

    // -----------------------------------------------------------------------
    // Archive inner-file methods
    // -----------------------------------------------------------------------

    pub fn remove_archive_entries(&self, archive_path: &str) -> Result<usize> {
        let conn = self.get_conn()?;
        let n = conn.execute(
            "DELETE FROM files WHERE archive_path = ?",
            [archive_path],
        )?;
        Ok(n)
    }

    pub fn paths_in_archive(&self, archive_path: &str) -> Result<Vec<String>> {
        let conn = self.get_conn()?;
        let mut stmt =
            conn.prepare("SELECT path FROM files WHERE archive_path = ?")?;
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

// ---------------------------------------------------------------------------
// Row mapper
// ---------------------------------------------------------------------------

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
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

fn query_records(
    stmt: &mut rusqlite::Statement<'_>,
    params: Vec<rusqlite::types::Value>,
) -> Result<Vec<FileRecord>> {
    let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
        row_to_record(row)
    })?;
    rows.map(|r| r.map_err(AppError::Database)).collect()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_store() -> SqliteStore {
        SqliteStore::new(Path::new(":memory:")).unwrap()
    }

    fn sample_record(path: &str) -> FileRecord {
        FileRecord {
            id:              None,
            path:            path.to_owned(),
            name:            path.split('/').last().unwrap_or(path).to_owned(),
            extension:       Some("rs".into()),
            size_bytes:      1024,
            mtime:           1_000_000,
            ctime:           1_000_000,
            is_dir:          false,
            is_hidden:       false,
            is_symlink:      false,
            attrib_bitmask:  0,
            content_indexed: false,
            tantivy_id:      None,
            indexed_at:      1_000_000,
            archive_path:    None,
        }
    }

    #[test]
    fn upsert_and_get_by_path() {
        let store = make_store();
        let rec = sample_record("/tmp/foo.rs");
        store.upsert_file(&rec).unwrap();
        let got = store.get_by_path("/tmp/foo.rs").unwrap().unwrap();
        assert_eq!(got.path, "/tmp/foo.rs");
        assert_eq!(got.size_bytes, 1024);
    }

    #[test]
    fn remove_file_returns_none() {
        let store = make_store();
        store.upsert_file(&sample_record("/tmp/foo.rs")).unwrap();
        store.remove_file("/tmp/foo.rs").unwrap();
        assert!(store.get_by_path("/tmp/foo.rs").unwrap().is_none());
    }

    #[test]
    fn remove_directory_removes_children() {
        let store = make_store();
        store.upsert_file(&sample_record("/proj/src/a.rs")).unwrap();
        store.upsert_file(&sample_record("/proj/src/b.rs")).unwrap();
        store.upsert_file(&sample_record("/proj/README.md")).unwrap();
        let removed = store.remove_directory("/proj/src").unwrap();
        assert_eq!(removed, 2);
        assert!(store.get_by_path("/proj/src/a.rs").unwrap().is_none());
        assert!(store.get_by_path("/proj/README.md").unwrap().is_some());
    }

    #[test]
    fn search_name_with_extension_filter() {
        let store = make_store();
        store.upsert_file(&sample_record("/a/foo.rs")).unwrap();
        let mut rec2 = sample_record("/a/bar.toml");
        rec2.extension = Some("toml".into());
        store.upsert_file(&rec2).unwrap();

        let filters = MetaFilters {
            extensions: Some(vec!["rs".into()]),
            ..Default::default()
        };
        let results = store.search_name("%", &filters, 100).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "/a/foo.rs");
    }

    #[test]
    fn search_meta_size_range() {
        let store = make_store();
        let mut small = sample_record("/a/small.rs");
        small.size_bytes = 100;
        let mut large = sample_record("/a/large.rs");
        large.size_bytes = 5000;
        store.upsert_file(&small).unwrap();
        store.upsert_file(&large).unwrap();

        let filters = MetaFilters {
            min_size: Some(1000),
            ..Default::default()
        };
        let results = store.search_meta(&filters, 100).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "/a/large.rs");
    }

    #[test]
    fn paths_under_returns_correct_subset() {
        let store = make_store();
        store.upsert_file(&sample_record("/root/a/b.rs")).unwrap();
        store.upsert_file(&sample_record("/root/a/c.rs")).unwrap();
        store.upsert_file(&sample_record("/other/x.rs")).unwrap();

        let paths = store.paths_under("/root/a").unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"/root/a/b.rs".to_owned()));
        assert!(paths.contains(&"/root/a/c.rs".to_owned()));
    }

    #[test]
    fn concurrent_upserts_all_succeed() {
        // :memory: gives each pool connection its own private database.
        // Use a real temp file so all connections share the same schema.
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let store = Arc::new(SqliteStore::new(db_file.path()).unwrap());
        let mut handles = Vec::new();
        for i in 0..10_usize {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let rec = sample_record(&format!("/tmp/file_{i}.rs"));
                s.upsert_file(&rec).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(store.total_files().unwrap(), 10);
    }

    #[test]
    fn has_child_true_and_false() {
        let store = make_store();
        store.upsert_file(&sample_record("/proj/src/main.rs")).unwrap();
        assert!(store.has_child("/proj/src", "main.rs").unwrap());
        assert!(!store.has_child("/proj/src", "other.rs").unwrap());
    }

    #[test]
    fn upsert_inner_file_and_get() {
        let store = make_store();
        let mut rec = sample_record("/archive.zip!/inner/file.rs");
        rec.archive_path = Some("/archive.zip".into());
        store.upsert_file(&rec).unwrap();
        let got = store.get_by_path("/archive.zip!/inner/file.rs").unwrap().unwrap();
        assert_eq!(got.archive_path, Some("/archive.zip".into()));
    }

    #[test]
    fn remove_archive_entries_leaves_real_files() {
        let store = make_store();
        let mut inner1 = sample_record("/a.zip!/x.rs");
        inner1.archive_path = Some("/a.zip".into());
        let mut inner2 = sample_record("/a.zip!/y.rs");
        inner2.archive_path = Some("/a.zip".into());
        let real = sample_record("/a.zip");
        store.upsert_file(&inner1).unwrap();
        store.upsert_file(&inner2).unwrap();
        store.upsert_file(&real).unwrap();

        let removed = store.remove_archive_entries("/a.zip").unwrap();
        assert_eq!(removed, 2);
        assert!(store.get_by_path("/a.zip").unwrap().is_some());
    }

    #[test]
    fn paths_in_archive_returns_correct_paths() {
        let store = make_store();
        let mut inner = sample_record("/b.zip!/file.rs");
        inner.archive_path = Some("/b.zip".into());
        store.upsert_file(&inner).unwrap();
        let paths = store.paths_in_archive("/b.zip").unwrap();
        assert_eq!(paths, vec!["/b.zip!/file.rs".to_owned()]);
    }

    #[test]
    fn total_files_excludes_archive_entries() {
        let store = make_store();
        store.upsert_file(&sample_record("/real.rs")).unwrap();
        let mut inner = sample_record("/z.zip!/inner.rs");
        inner.archive_path = Some("/z.zip".into());
        store.upsert_file(&inner).unwrap();
        assert_eq!(store.total_files().unwrap(), 1);
    }

    #[test]
    fn total_archive_entries_counts_only_inner() {
        let store = make_store();
        store.upsert_file(&sample_record("/real.rs")).unwrap();
        let mut inner = sample_record("/z.zip!/inner.rs");
        inner.archive_path = Some("/z.zip".into());
        store.upsert_file(&inner).unwrap();
        assert_eq!(store.total_archive_entries().unwrap(), 1);
    }
}
