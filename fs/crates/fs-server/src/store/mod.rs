use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod memory_store;
pub mod sqlite_store;
pub mod tantivy_store;

// ---------------------------------------------------------------------------
// Attrib bitmask constants (Windows FILE_ATTRIBUTE_* mapped to bits)
// ---------------------------------------------------------------------------

pub const ATTRIB_ARCHIVE: u32 = 1;
pub const ATTRIB_COMPRESSED: u32 = 2;
pub const ATTRIB_DIRECTORY: u32 = 4;
pub const ATTRIB_ENCRYPTED: u32 = 8;
pub const ATTRIB_HIDDEN: u32 = 16;
pub const ATTRIB_REPARSE_POINT: u32 = 32;
pub const ATTRIB_NORMAL: u32 = 64;
pub const ATTRIB_OFFLINE: u32 = 128;
pub const ATTRIB_SPARSE: u32 = 256;
pub const ATTRIB_READONLY: u32 = 512;
pub const ATTRIB_SYSTEM: u32 = 1024;
pub const ATTRIB_TEMPORARY: u32 = 2048;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A record stored in the SQLite metadata store.
/// `archive_path = None` → real filesystem file.
/// `archive_path = Some(outer_zip)` → virtual inner-archive entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: Option<i64>,
    pub path: String,
    pub name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub is_dir: bool,
    pub is_hidden: bool,
    pub is_symlink: bool,
    pub attrib_bitmask: u32,
    pub content_indexed: bool,
    pub tantivy_id: Option<String>,
    pub indexed_at: i64,
    pub archive_path: Option<String>,
}

/// Fuzzy ranker handed to the stores: takes candidate names, returns the
/// indices of the winners, best first.
pub type FuzzyRank<'a> = dyn Fn(&[&str]) -> Vec<usize> + Sync + 'a;

/// The fields predicates read, so they can run on borrowed storage (the
/// memory store's packed entries) without building a `FileRecord` per row.
/// The memory store keeps no whole paths, so path access goes through a
/// callback that may rebuild the path into a scratch buffer.
pub trait RecordView {
    fn with_path<T>(&self, f: impl FnOnce(&str) -> T) -> T;
    fn path_is_under(&self, dir: &str) -> bool {
        self.with_path(|p| is_under(p, dir))
    }
    /// Number of '/' in the path, which is what `depth:` compares.
    fn path_depth(&self) -> usize {
        self.with_path(|p| p.bytes().filter(|&b| b == b'/').count())
    }
    fn name(&self) -> &str;
    fn extension(&self) -> Option<&str>;
    fn size_bytes(&self) -> u64;
    fn mtime(&self) -> i64;
    fn ctime(&self) -> i64;
    fn is_dir(&self) -> bool;
    fn attrib_bitmask(&self) -> u32;
    fn is_archive_entry(&self) -> bool;
    fn with_archive_path<T>(&self, f: impl FnOnce(Option<&str>) -> T) -> T;
}

impl RecordView for FileRecord {
    fn with_path<T>(&self, f: impl FnOnce(&str) -> T) -> T {
        f(&self.path)
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn extension(&self) -> Option<&str> {
        self.extension.as_deref()
    }
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
    fn mtime(&self) -> i64 {
        self.mtime
    }
    fn ctime(&self) -> i64 {
        self.ctime
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn attrib_bitmask(&self) -> u32 {
        self.attrib_bitmask
    }
    fn is_archive_entry(&self) -> bool {
        self.archive_path.is_some()
    }
    fn with_archive_path<T>(&self, f: impl FnOnce(Option<&str>) -> T) -> T {
        f(self.archive_path.as_deref())
    }
}

/// `path` is strictly inside directory `dir`, without allocating `"{dir}/"`.
pub(crate) fn is_under(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'/'
}

/// A search hit returned by the Tantivy full-text store.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub score: f32,
    pub snippet: Option<String>,
}

// ---------------------------------------------------------------------------
// MetaFilters - dynamic WHERE clause builder
// ---------------------------------------------------------------------------

/// Optional filters applied on top of a primary query predicate.
/// Each `Some` field appends an `AND` clause; params are accumulated
/// separately to prevent SQL injection.
#[derive(Debug, Clone, Default)]
pub struct MetaFilters {
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub is_dir: Option<bool>,
    pub extensions: Option<Vec<String>>,
    pub path_prefix: Option<String>,
}

impl MetaFilters {
    /// Build a WHERE fragment and the corresponding bind values.
    /// The returned string starts with `AND` if non-empty (or is empty string).
    /// Caller appends it to a base `WHERE 1=1` or similar.
    pub fn build_where(&self) -> (String, Vec<rusqlite::types::Value>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(v) = self.min_size {
            clauses.push("size_bytes >= ?".into());
            params.push(rusqlite::types::Value::Integer(v as i64));
        }
        if let Some(v) = self.max_size {
            clauses.push("size_bytes <= ?".into());
            params.push(rusqlite::types::Value::Integer(v as i64));
        }
        if let Some(v) = self.after {
            clauses.push("mtime >= ?".into());
            params.push(rusqlite::types::Value::Integer(v));
        }
        if let Some(v) = self.before {
            clauses.push("mtime <= ?".into());
            params.push(rusqlite::types::Value::Integer(v));
        }
        if let Some(v) = self.is_dir {
            clauses.push("is_dir = ?".into());
            params.push(rusqlite::types::Value::Integer(i64::from(v)));
        }
        if let Some(exts) = &self.extensions {
            if !exts.is_empty() {
                let placeholders = exts.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                clauses.push(format!("extension IN ({placeholders})"));
                for e in exts {
                    params.push(rusqlite::types::Value::Text(e.clone()));
                }
            }
        }
        if let Some(prefix) = &self.path_prefix {
            clauses.push("path LIKE ? || '/%'".into());
            params.push(rusqlite::types::Value::Text(prefix.clone()));
        }

        if clauses.is_empty() {
            (String::new(), params)
        } else {
            (format!("AND {}", clauses.join(" AND ")), params)
        }
    }
}

// ---------------------------------------------------------------------------
// DupeKind
// ---------------------------------------------------------------------------

/// Which property to group by when searching for duplicate files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupeKind {
    Name,
    NamePart,
    Size,
    DateModified,
    DateCreated,
    DateAccessed,
    Attrib,
}

// ---------------------------------------------------------------------------
// CmpOp - comparison operators for numeric SqlConditions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
}

impl CmpOp {
    pub fn as_sql(&self) -> &'static str {
        match self {
            CmpOp::Lt => "<",
            CmpOp::Lte => "<=",
            CmpOp::Gt => ">",
            CmpOp::Gte => ">=",
            CmpOp::Eq => "=",
        }
    }
}

// ---------------------------------------------------------------------------
// SqlCondition - typed SQL predicates (used by query evaluator, Phase 5)
// ---------------------------------------------------------------------------

/// A typed SQL predicate that can be passed to store methods.
/// All variants are defined now to avoid breaking the store API in Phase 5.
#[derive(Debug, Clone)]
pub enum SqlCondition {
    /// `name LIKE ?`  (% wildcards must be pre-added by caller)
    NameLike(String),
    /// `name GLOB ?`  (SQLite GLOB, case-sensitive)
    NameGlob(String),
    /// `path LIKE ?`
    PathLike(String),
    /// `is_dir = 1` or `is_dir = 0`
    IsDir(bool),
    /// `mtime BETWEEN a AND b` (inclusive)
    MtimeRange(i64, i64),
    /// `ctime BETWEEN a AND b` (inclusive)
    CtimeRange(i64, i64),
    /// `size_bytes BETWEEN a AND b` (inclusive)
    SizeRange(u64, u64),
    /// `size_bytes <op> value`
    SizeOp(CmpOp, u64),
    /// `extension IN (...)`
    ExtIn(Vec<String>),
    /// `path LIKE ? || '/%'` (all files under a directory)
    ParentPath(String),
    /// `LENGTH(name) <op> value`
    NameLenOp(CmpOp, usize),
    /// Depth relative to a root: `(LENGTH(path) - LENGTH(REPLACE(path, '/', ''))) <op> depth`
    DepthOp(CmpOp, usize),
    /// `archive_path IS NOT NULL` (inner-archive entries only)
    IsArchiveEntry,
    /// `archive_path = ?` (all inner entries of a specific archive)
    ArchiveOf(String),
    /// `attrib_bitmask & mask = mask` (all specified bits set)
    AttribAll(u32),
    /// `attrib_bitmask & mask != 0` (any of the specified bits set)
    AttribAny(u32),
    /// True when any branch matches; each branch is an AND of its conditions.
    Or(Vec<Vec<SqlCondition>>),
    /// True when the AND of the conditions does not match.
    Not(Vec<SqlCondition>),
}

fn conjunction_sql(conds: &[SqlCondition]) -> (String, Vec<rusqlite::types::Value>) {
    if conds.is_empty() {
        return ("1".into(), Vec::new());
    }
    let mut parts = Vec::with_capacity(conds.len());
    let mut params = Vec::new();
    for c in conds {
        let (sql, p) = c.to_sql();
        parts.push(sql);
        params.extend(p);
    }
    (format!("({})", parts.join(" AND ")), params)
}

impl SqlCondition {
    /// Render this condition to a SQL fragment and its bind values.
    pub fn to_sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        match self {
            SqlCondition::NameLike(p) => ("name LIKE ?".into(), vec![Value::Text(p.clone())]),
            SqlCondition::NameGlob(p) => ("name GLOB ?".into(), vec![Value::Text(p.clone())]),
            SqlCondition::PathLike(p) => ("path LIKE ?".into(), vec![Value::Text(p.clone())]),
            SqlCondition::IsDir(v) => ("is_dir = ?".into(), vec![Value::Integer(i64::from(*v))]),
            SqlCondition::MtimeRange(a, b) => (
                "mtime BETWEEN ? AND ?".into(),
                vec![Value::Integer(*a), Value::Integer(*b)],
            ),
            SqlCondition::CtimeRange(a, b) => (
                "ctime BETWEEN ? AND ?".into(),
                vec![Value::Integer(*a), Value::Integer(*b)],
            ),
            SqlCondition::SizeRange(a, b) => (
                "size_bytes BETWEEN ? AND ?".into(),
                vec![Value::Integer(*a as i64), Value::Integer(*b as i64)],
            ),
            SqlCondition::SizeOp(op, v) => (
                format!("size_bytes {} ?", op.as_sql()),
                vec![Value::Integer(*v as i64)],
            ),
            SqlCondition::ExtIn(exts) => {
                let ph = exts.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                let vals = exts.iter().map(|e| Value::Text(e.clone())).collect();
                (format!("extension IN ({ph})"), vals)
            }
            SqlCondition::ParentPath(p) => {
                ("path LIKE ? || '/%'".into(), vec![Value::Text(p.clone())])
            }
            SqlCondition::NameLenOp(op, n) => (
                format!("LENGTH(name) {} ?", op.as_sql()),
                vec![Value::Integer(*n as i64)],
            ),
            SqlCondition::DepthOp(op, d) => (
                format!(
                    "(LENGTH(path) - LENGTH(REPLACE(path, '/', ''))) {} ?",
                    op.as_sql()
                ),
                vec![Value::Integer(*d as i64)],
            ),
            SqlCondition::IsArchiveEntry => ("archive_path IS NOT NULL".into(), vec![]),
            SqlCondition::ArchiveOf(ap) => {
                ("archive_path = ?".into(), vec![Value::Text(ap.clone())])
            }
            SqlCondition::AttribAll(mask) => {
                (format!("(attrib_bitmask & {mask}) = {mask}"), vec![])
            }
            SqlCondition::AttribAny(mask) => (format!("(attrib_bitmask & {mask}) != 0"), vec![]),
            SqlCondition::Or(branches) => {
                if branches.is_empty() {
                    return ("0".into(), Vec::new());
                }
                let mut parts = Vec::with_capacity(branches.len());
                let mut params = Vec::new();
                for b in branches {
                    let (sql, p) = conjunction_sql(b);
                    parts.push(sql);
                    params.extend(p);
                }
                (format!("({})", parts.join(" OR ")), params)
            }
            SqlCondition::Not(conds) => {
                // NULL columns (extension, archive_path) make the inner
                // expression NULL, and a bare NOT NULL would drop those rows;
                // `!ext:rs` must still return files without an extension.
                let (sql, p) = conjunction_sql(conds);
                (format!("NOT COALESCE({sql}, 0)"), p)
            }
        }
    }
}

/// Build a complete AND clause from a slice of `SqlCondition`s.
/// Returns `("")` if the slice is empty, else `" AND cond1 AND cond2 ..."`.
pub fn build_conditions(conditions: &[SqlCondition]) -> (String, Vec<rusqlite::types::Value>) {
    if conditions.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut parts = Vec::new();
    let mut all_params = Vec::new();
    for cond in conditions {
        let (sql, params) = cond.to_sql();
        parts.push(sql);
        all_params.extend(params);
    }
    (format!(" AND {}", parts.join(" AND ")), all_params)
}

// ---------------------------------------------------------------------------
// FileStore - dispatch enum over concrete store backends
// ---------------------------------------------------------------------------

use crate::error::Result;
use memory_store::MemoryStore;
use sqlite_store::SqliteStore;

/// Runtime-selectable metadata store. Replace `Arc<SqliteStore>` with
/// `Arc<FileStore>` throughout the daemon to support both backends.
pub enum FileStore {
    Sqlite(Arc<SqliteStore>),
    Memory(Arc<MemoryStore>),
}

impl FileStore {
    /// Create a `FileStore` from config.
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        match cfg.store.mode {
            crate::config::StoreMode::Sqlite => Ok(FileStore::Sqlite(Arc::new(
                SqliteStore::with_indexes(&cfg.data_dir.join("index.db"), &cfg.store.indexes)?,
            ))),
            crate::config::StoreMode::Memory => Ok(FileStore::Memory(Arc::new(MemoryStore::new(
                &cfg.data_dir,
            )?))),
        }
    }

    // ---- upsert / delete ---------------------------------------------------

    pub fn upsert_file(&self, record: &FileRecord) -> Result<i64> {
        match self {
            FileStore::Sqlite(s) => s.upsert_file(record),
            FileStore::Memory(m) => {
                m.upsert_file(record);
                Ok(0)
            }
        }
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        match self {
            FileStore::Sqlite(s) => s.remove_file(path),
            FileStore::Memory(m) => {
                m.remove_file(path);
                Ok(())
            }
        }
    }

    pub fn remove_directory(&self, path: &str) -> Result<usize> {
        match self {
            FileStore::Sqlite(s) => s.remove_directory(path),
            FileStore::Memory(m) => Ok(m.remove_directory(path)),
        }
    }

    pub fn upsert_archive_batch(
        &self,
        records: &[FileRecord],
        tag: Option<(&str, i64)>,
    ) -> Result<()> {
        match self {
            FileStore::Sqlite(s) => s.upsert_archive_batch(records, tag),
            FileStore::Memory(m) => {
                m.upsert_archive_batch(records, tag);
                Ok(())
            }
        }
    }

    pub fn remove_archive_entries(&self, archive_path: &str) -> Result<usize> {
        match self {
            FileStore::Sqlite(s) => s.remove_archive_entries(archive_path),
            FileStore::Memory(m) => Ok(m.remove_archive_entries(archive_path)),
        }
    }

    // ---- crawl generation --------------------------------------------------

    pub fn begin_crawl_generation(&self) -> Result<i64> {
        match self {
            FileStore::Sqlite(s) => s.begin_crawl_generation(),
            FileStore::Memory(m) => Ok(m.begin_crawl_generation()),
        }
    }

    pub fn batch_upsert_crawl(
        &self,
        batch: &[(FileRecord, String)],
        generation: i64,
    ) -> Result<()> {
        match self {
            FileStore::Sqlite(s) => s.batch_upsert_crawl(batch, generation),
            FileStore::Memory(m) => {
                m.batch_upsert_crawl(batch, generation);
                Ok(())
            }
        }
    }

    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> Result<usize> {
        match self {
            FileStore::Sqlite(s) => s.remove_stale_for_root(watch_root, generation),
            FileStore::Memory(m) => Ok(m.remove_stale_for_root(watch_root, generation)),
        }
    }

    // ---- queries -----------------------------------------------------------

    pub fn get_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.get_by_path(path),
            FileStore::Memory(m) => m.get_by_path(path),
        }
    }

    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        match self {
            FileStore::Sqlite(s) => s.has_child(parent_path, name_pattern),
            FileStore::Memory(m) => m.has_child(parent_path, name_pattern),
        }
    }

    pub fn children_of(&self, parent_path: &str) -> Result<Vec<String>> {
        match self {
            FileStore::Sqlite(s) => s.children_of(parent_path),
            FileStore::Memory(m) => m.children_of(parent_path),
        }
    }

    pub fn total_files(&self) -> Result<i64> {
        match self {
            FileStore::Sqlite(s) => s.total_files(),
            FileStore::Memory(m) => Ok(m.total_files()),
        }
    }

    pub fn total_archive_entries(&self) -> Result<i64> {
        match self {
            FileStore::Sqlite(s) => s.total_archive_entries(),
            FileStore::Memory(m) => Ok(m.total_archive_entries()),
        }
    }

    pub fn search_meta(&self, filters: &MetaFilters, limit: usize) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.search_meta(filters, limit),
            FileStore::Memory(m) => m.search_meta(filters, limit),
        }
    }

    pub fn search_name(
        &self,
        name: &str,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.search_name(name, filters, limit),
            FileStore::Memory(m) => m.search_name(name, filters, limit),
        }
    }

    pub fn search_extension(
        &self,
        ext: &str,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.search_extension(ext, filters, limit),
            FileStore::Memory(m) => m.search_extension(ext, filters, limit),
        }
    }

    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        match self {
            FileStore::Sqlite(s) => s.paths_under(dir),
            FileStore::Memory(m) => m.paths_under(dir),
        }
    }

    pub fn search_conditions(
        &self,
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.search_conditions(conds, limit),
            FileStore::Memory(m) => m.search_conditions(conds, limit),
        }
    }

    /// Fuzzy search: `rank` scores the names of up to `cap` rows matching
    /// `conds`; only the winners are materialized as `FileRecord`s.
    pub fn fuzzy_search(
        &self,
        conds: &[SqlCondition],
        cap: usize,
        rank: &FuzzyRank<'_>,
    ) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.fuzzy_search(conds, cap, rank),
            FileStore::Memory(m) => m.fuzzy_search(conds, cap, rank),
        }
    }

    pub fn search_by_paths(
        &self,
        paths: &[String],
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.search_by_paths(paths, conds, limit),
            FileStore::Memory(m) => m.search_by_paths(paths, conds, limit),
        }
    }

    pub fn find_dupes(
        &self,
        kind: DupeKind,
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        match self {
            FileStore::Sqlite(s) => s.find_dupes(kind, conds, limit),
            FileStore::Memory(m) => m.find_dupes(kind, conds, limit),
        }
    }

    pub fn list_archive_paths(&self) -> Result<Vec<String>> {
        match self {
            FileStore::Sqlite(s) => s.list_archive_paths(),
            FileStore::Memory(m) => m.list_archive_paths(),
        }
    }

    // ---- metadata (key-value) ----------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        match self {
            FileStore::Sqlite(s) => s.get_meta(key),
            FileStore::Memory(m) => Ok(m.get_meta(key)),
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        match self {
            FileStore::Sqlite(s) => s.set_meta(key, value),
            FileStore::Memory(m) => {
                m.set_meta(key, value);
                Ok(())
            }
        }
    }

    // ---- persistence -------------------------------------------------------

    /// Write the in-memory index to disk. No-op for the SQLite backend.
    /// Starts an initial-crawl bulk load (SQLite only, and only when the
    /// index is empty). Returns whether `end_bulk_load` has work to do.
    pub fn begin_bulk_load(&self) -> Result<bool> {
        match self {
            FileStore::Sqlite(s) => s.begin_bulk_load(),
            FileStore::Memory(_) => Ok(false),
        }
    }

    pub fn end_bulk_load(&self) -> Result<()> {
        match self {
            FileStore::Sqlite(s) => s.end_bulk_load(),
            FileStore::Memory(_) => Ok(()),
        }
    }

    pub fn flush_to_disk(&self, data_dir: &Path) -> Result<()> {
        match self {
            FileStore::Sqlite(_) => Ok(()),
            FileStore::Memory(m) => m.flush_to_disk(data_dir),
        }
    }

    // ---- USN/inotify checkpoint --------------------------------------------

    /// Return the last recorded journal position (0 if never set or SQLite mode).
    pub fn get_usn_checkpoint(&self) -> u64 {
        match self {
            FileStore::Sqlite(_) => 0,
            FileStore::Memory(m) => m.get_usn_checkpoint(),
        }
    }

    /// Persist the journal position so dirty-shutdown recovery can replay deltas.
    /// No-op for the SQLite backend (SQLite is always consistent on disk).
    pub fn set_usn_checkpoint(&self, checkpoint: u64) {
        if let FileStore::Memory(m) = self {
            m.set_usn_checkpoint(checkpoint);
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_filters_empty() {
        let f = MetaFilters::default();
        let (sql, params) = f.build_where();
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }

    #[test]
    fn meta_filters_min_size() {
        let f = MetaFilters {
            min_size: Some(100),
            ..Default::default()
        };
        let (sql, params) = f.build_where();
        assert!(sql.contains("size_bytes >="));
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn sql_condition_ext_in() {
        let cond = SqlCondition::ExtIn(vec!["rs".into(), "toml".into()]);
        let (sql, params) = cond.to_sql();
        assert!(sql.contains("extension IN"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn build_conditions_empty() {
        let (sql, params) = build_conditions(&[]);
        assert!(sql.is_empty());
        assert!(params.is_empty());
    }
}
