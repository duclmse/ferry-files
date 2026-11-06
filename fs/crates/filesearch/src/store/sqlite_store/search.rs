use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use rusqlite::types::Value;

use crate::error::{AppError, Result};
use crate::store::{DupeKind, FileRecord, FuzzyRank, MetaFilters, SqlCondition, build_conditions};

use super::SqliteStore;
use super::schema::{query_records, row_to_record};

/// One sampled name per 512 rowids (mask + 1).
const NAME_SAMPLE_MASK: i64 = 511;
/// 16 samples ≈ 8k rows: below that a range is not worth a connection.
const MIN_SAMPLES_PER_PARTITION: usize = 16;
const MAX_NAME_PARTITIONS: usize = 8;
const NAME_BOUNDS_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct NameBounds {
    sampled_at: Option<Instant>,
    bounds:     Arc<Vec<String>>,
    /// Set while `bulk.rs` has the secondary indexes dropped; the `INDEXED BY`
    /// statements below would fail with "no such index" until they are rebuilt.
    bulk_loading: bool,
}

fn is_name_condition(c: &SqlCondition) -> bool {
    matches!(
        c,
        SqlCondition::NameLike(_) | SqlCondition::NameGlob(_) | SqlCondition::NameLenOp(..)
    )
}

/// The planner emits `NameLike("%")` as a catch-all for OR / NOT.
fn is_match_all(c: &SqlCondition) -> bool {
    matches!(c, SqlCondition::NameLike(p) if p == "%")
}

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
        let (name_conds, other_conds): (Vec<SqlCondition>, Vec<SqlCondition>) =
            conds.iter().cloned().partition(is_name_condition);
        if name_conds.iter().all(is_match_all) || self.bulk_loading()? {
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
            all_params.push(Value::Integer(limit as i64));
            return query_records(&mut stmt, all_params);
        }
        self.search_by_name_index(&name_conds, &other_conds, limit)
    }

    /// Fuzzy candidate pass: reads only `(id, name)` for up to `cap` rows
    /// matching `conds`, lets `rank` pick the winners by name, then loads full
    /// rows for those ids alone, in rank order. Loading every candidate's full
    /// row only to score its name cost ~70 MB of transient allocations per
    /// query at the default cap.
    pub fn fuzzy_search(
        &self,
        conds: &[SqlCondition],
        cap:   usize,
        rank:  &FuzzyRank<'_>,
    ) -> Result<Vec<FileRecord>> {
        let (where_extra, mut params) = build_conditions(conds);
        params.push(Value::Integer(cap as i64));
        let conn = self.get_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, name FROM files WHERE 1=1 {where_extra} LIMIT ?"
        ))?;
        let candidates = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<(i64, String)>>>()?;
        let names: Vec<&str> = candidates.iter().map(|(_, n)| n.as_str()).collect();
        let ids: Vec<i64> = rank(&names).into_iter().map(|i| candidates[i].0).collect();
        drop(names);
        drop(candidates);

        let mut by_id: std::collections::HashMap<i64, FileRecord> =
            std::collections::HashMap::with_capacity(ids.len());
        // Stay well under SQLITE_MAX_VARIABLE_NUMBER on older builds (999).
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                        is_dir, is_hidden, is_symlink, attrib_bitmask,
                        content_indexed, tantivy_id, indexed_at, archive_path
                 FROM files WHERE id IN ({marks})"
            ))?;
            let params = chunk.iter().map(|&id| Value::Integer(id)).collect();
            for r in query_records(&mut stmt, params)? {
                if let Some(id) = r.id {
                    by_id.insert(id, r);
                }
            }
        }
        // A row deleted between the two statements is simply dropped.
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    /// Name predicates like `%term%` can't use an index, so they force a scan.
    /// Scanning the table reads every full row (paths dominate its size); the
    /// covering `idx_files_name` index holds only `(name, id)` and is several
    /// times smaller, so the name predicates run against the index and only
    /// matching rows are fetched. Large indexes are additionally split into
    /// name ranges scanned in parallel, one pooled connection each.
    ///
    /// Results come back in name order rather than rowid order; neither is
    /// part of the API contract (there is no ORDER BY either way).
    fn search_by_name_index(
        &self,
        name_conds:  &[SqlCondition],
        other_conds: &[SqlCondition],
        limit:       usize,
    ) -> Result<Vec<FileRecord>> {
        let (name_sql, name_params)   = build_conditions(name_conds);
        let (other_sql, other_params) = build_conditions(other_conds);
        let bounds = self.name_bounds()?;

        let run = |lo: Option<&String>, hi: Option<&String>| -> Result<Vec<FileRecord>> {
            let mut params = name_params.clone();
            let mut range  = String::new();
            if let Some(lo) = lo {
                range.push_str(" AND name >= ?");
                params.push(Value::Text(lo.clone()));
            }
            if let Some(hi) = hi {
                range.push_str(" AND name < ?");
                params.push(Value::Text(hi.clone()));
            }
            params.extend(other_params.iter().cloned());
            params.push(Value::Integer(limit as i64));
            // CROSS JOIN pins the index scan as the outer loop so LIMIT can
            // stop it early; `n` exposes only `nid`, so the unqualified column
            // names in the select list and `other_sql` resolve to `files`.
            let sql = format!(
                "SELECT id, path, name, extension, size_bytes, mtime, ctime,
                        is_dir, is_hidden, is_symlink, attrib_bitmask,
                        content_indexed, tantivy_id, indexed_at, archive_path
                 FROM (SELECT id AS nid FROM files INDEXED BY idx_files_name
                       WHERE 1=1 {name_sql}{range}) AS n
                 CROSS JOIN files ON files.id = n.nid
                 WHERE 1=1 {other_sql} LIMIT ?"
            );
            let conn = self.get_conn()?;
            let mut stmt = conn.prepare_cached(&sql)?;
            query_records(&mut stmt, params)
        };

        if bounds.is_empty() {
            return run(None, None);
        }
        let ranges: Vec<(Option<&String>, Option<&String>)> = (0..=bounds.len())
            .map(|i| (i.checked_sub(1).map(|j| &bounds[j]), bounds.get(i)))
            .collect();
        let parts = ranges
            .par_iter()
            .map(|(lo, hi)| run(*lo, *hi))
            .collect::<Result<Vec<_>>>()?;
        let mut out: Vec<FileRecord> = parts.into_iter().flatten().collect();
        out.truncate(limit);
        Ok(out)
    }

    pub(super) fn bulk_loading(&self) -> Result<bool> {
        Ok(self.lock_name_bounds()?.bulk_loading)
    }

    pub(super) fn set_bulk_loading(&self, on: bool) -> Result<()> {
        let mut cache = self.lock_name_bounds()?;
        cache.bulk_loading = on;
        cache.sampled_at = None;
        Ok(())
    }

    fn lock_name_bounds(&self) -> Result<std::sync::MutexGuard<'_, NameBounds>> {
        self.name_bounds
            .lock()
            .map_err(|_| AppError::Index("name bounds lock poisoned".into()))
    }

    /// Range split points for `search_by_name_index`, sampled from the name
    /// index and cached. Stale points only unbalance the ranges: they always
    /// cover the whole keyspace, so results stay exact.
    fn name_bounds(&self) -> Result<Arc<Vec<String>>> {
        let mut cache = self.lock_name_bounds()?;
        if cache.sampled_at.is_some_and(|t| t.elapsed() < NAME_BOUNDS_TTL) {
            return Ok(Arc::clone(&cache.bounds));
        }

        let conn = self.get_conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT name FROM files INDEXED BY idx_files_name
             WHERE (id & ?1) = 0 ORDER BY name",
        )?;
        let sample = stmt
            .query_map([NAME_SAMPLE_MASK], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let parts = std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .min(MAX_NAME_PARTITIONS)
            .min(sample.len() / MIN_SAMPLES_PER_PARTITION)
            .max(1);
        let mut bounds: Vec<String> = (1..parts)
            .map(|i| sample[i * sample.len() / parts].clone())
            .collect();
        bounds.dedup();

        cache.sampled_at = Some(Instant::now());
        cache.bounds     = Arc::new(bounds);
        Ok(Arc::clone(&cache.bounds))
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
