use crate::error::{AppError, Result};
use crate::store::{DupeKind, FileRecord, MetaFilters, SqlCondition, build_conditions};

use super::SqliteStore;
use super::schema::{query_records, row_to_record};

impl SqliteStore {
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

    /// List all paths under `dir` (recursive). Only use for small subtrees or
    /// non-crawl purposes; full-crawl stale detection should use generation methods.
    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let conn = self.get_conn()?;
        let mut stmt =
            conn.prepare("SELECT path FROM files WHERE path LIKE ? || '/%'")?;
        let rows = stmt.query_map([dir], |row| row.get(0))?;
        rows.map(|r| r.map_err(AppError::Database)).collect()
    }

    /// Return true if `parent_path` has a direct child matching `name_pattern`.
    /// Uses the indexed `parent` column for O(log n) lookup.
    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        let conn = self.get_conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE parent = ? AND name LIKE ?",
            rusqlite::params![parent_path, name_pattern],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Direct (non-recursive) children of `parent_path` that are real files,
    /// i.e. not archive-inner entries. Uses the indexed `parent` column.
    pub fn children_of(&self, parent_path: &str) -> Result<Vec<String>> {
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(
            "SELECT path FROM files WHERE parent = ? AND archive_path IS NULL",
        )?;
        let rows = stmt.query_map([parent_path], |row| row.get(0))?;
        rows.map(|r| r.map_err(AppError::Database)).collect()
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
            DupeKind::NamePart     => "COALESCE(REPLACE(name, '.' || extension, ''), name)",
            DupeKind::Size         => "size_bytes",
            DupeKind::DateModified => "mtime",
            DupeKind::DateCreated  => "ctime",
            DupeKind::DateAccessed => "mtime",
            DupeKind::Attrib       => "attrib_bitmask",
        };

        let pass1 = format!(
            "SELECT {group_col} FROM files
             WHERE archive_path IS NULL {extra_sql}
             GROUP BY {group_col} HAVING COUNT(*) > 1"
        );
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

        let mut params = extra_params.clone();
        params.extend(extra_params);
        params.push(rusqlite::types::Value::Integer(limit as i64));

        query_records(&mut stmt, params)
    }

    /// Execute arbitrary SqlConditions and return matching records.
    /// Used by `FileStore::search_conditions` dispatch.
    pub fn search_conditions(&self, conds: &[SqlCondition], limit: usize) -> Result<Vec<FileRecord>> {
        if conds.is_empty() {
            return self.search_meta(&MetaFilters::default(), limit);
        }
        let (where_extra, params) = build_conditions(conds);
        let sql = format!(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files WHERE 1=1 {where_extra} LIMIT ?"
        );
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let mut all_params = params;
        all_params.push(rusqlite::types::Value::Integer(limit as i64));
        query_records(&mut stmt, all_params)
    }

    /// Look up `paths` in order, applying `conds` as additional filters.
    /// Used by `FileStore::search_by_paths` for the Tantivy → SQLite path.
    pub fn search_by_paths(
        &self,
        paths:  &[String],
        conds:  &[SqlCondition],
        limit:  usize,
    ) -> Result<Vec<FileRecord>> {
        let mut results = Vec::with_capacity(paths.len().min(limit));
        let (where_extra, extra_params) = build_conditions(conds);
        let sql = format!(
            "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                    is_dir, is_hidden, is_symlink, attrib_bitmask,
                    content_indexed, tantivy_id, indexed_at, archive_path
             FROM files WHERE path = ? {where_extra}"
        );
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&sql)?;
        for path in paths.iter().take(limit * 4) {
            let mut params: Vec<rusqlite::types::Value> =
                vec![rusqlite::types::Value::Text(path.clone())];
            params.extend(extra_params.clone());
            let row = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    row_to_record(row)
                })?
                .next();
            if let Some(Ok(rec)) = row {
                results.push(rec);
            }
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }
}
