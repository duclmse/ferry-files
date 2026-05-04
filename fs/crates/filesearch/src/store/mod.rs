use serde::{Deserialize, Serialize};

pub mod sqlite_store;
pub mod tantivy_store;

// ---------------------------------------------------------------------------
// Attrib bitmask constants (Windows FILE_ATTRIBUTE_* mapped to bits)
// ---------------------------------------------------------------------------

pub const ATTRIB_ARCHIVE:       u32 = 1;
pub const ATTRIB_COMPRESSED:    u32 = 2;
pub const ATTRIB_DIRECTORY:     u32 = 4;
pub const ATTRIB_ENCRYPTED:     u32 = 8;
pub const ATTRIB_HIDDEN:        u32 = 16;
pub const ATTRIB_REPARSE_POINT: u32 = 32;
pub const ATTRIB_NORMAL:        u32 = 64;
pub const ATTRIB_OFFLINE:       u32 = 128;
pub const ATTRIB_SPARSE:        u32 = 256;
pub const ATTRIB_READONLY:      u32 = 512;
pub const ATTRIB_SYSTEM:        u32 = 1024;
pub const ATTRIB_TEMPORARY:     u32 = 2048;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A record stored in the SQLite metadata store.
/// `archive_path = None` → real filesystem file.
/// `archive_path = Some(outer_zip)` → virtual inner-archive entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id:              Option<i64>,
    pub path:            String,
    pub name:            String,
    pub extension:       Option<String>,
    pub size_bytes:      u64,
    pub mtime:           i64,
    pub ctime:           i64,
    pub is_dir:          bool,
    pub is_hidden:       bool,
    pub is_symlink:      bool,
    pub attrib_bitmask:  u32,
    pub content_indexed: bool,
    pub tantivy_id:      Option<String>,
    pub indexed_at:      i64,
    pub archive_path:    Option<String>,
}

/// A search hit returned by the Tantivy full-text store.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path:    String,
    pub score:   f32,
    pub snippet: Option<String>,
}

// ---------------------------------------------------------------------------
// MetaFilters — dynamic WHERE clause builder
// ---------------------------------------------------------------------------

/// Optional filters applied on top of a primary query predicate.
/// Each `Some` field appends an `AND` clause; params are accumulated
/// separately to prevent SQL injection.
#[derive(Debug, Clone, Default)]
pub struct MetaFilters {
    pub min_size:    Option<u64>,
    pub max_size:    Option<u64>,
    pub after:       Option<i64>,
    pub before:      Option<i64>,
    pub is_dir:      Option<bool>,
    pub extensions:  Option<Vec<String>>,
    pub path_prefix: Option<String>,
}

impl MetaFilters {
    /// Build a WHERE fragment and the corresponding bind values.
    /// The returned string starts with `AND` if non-empty (or is empty string).
    /// Caller appends it to a base `WHERE 1=1` or similar.
    pub fn build_where(&self) -> (String, Vec<rusqlite::types::Value>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut params:  Vec<rusqlite::types::Value> = Vec::new();

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
// CmpOp — comparison operators for numeric SqlConditions
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
            CmpOp::Lt  => "<",
            CmpOp::Lte => "<=",
            CmpOp::Gt  => ">",
            CmpOp::Gte => ">=",
            CmpOp::Eq  => "=",
        }
    }
}

// ---------------------------------------------------------------------------
// SqlCondition — typed SQL predicates (used by query evaluator, Phase 5)
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
}

impl SqlCondition {
    /// Render this condition to a SQL fragment and its bind values.
    pub fn to_sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        match self {
            SqlCondition::NameLike(p) =>
                ("name LIKE ?".into(), vec![Value::Text(p.clone())]),
            SqlCondition::NameGlob(p) =>
                ("name GLOB ?".into(), vec![Value::Text(p.clone())]),
            SqlCondition::PathLike(p) =>
                ("path LIKE ?".into(), vec![Value::Text(p.clone())]),
            SqlCondition::IsDir(v) =>
                ("is_dir = ?".into(), vec![Value::Integer(i64::from(*v))]),
            SqlCondition::MtimeRange(a, b) =>
                ("mtime BETWEEN ? AND ?".into(), vec![Value::Integer(*a), Value::Integer(*b)]),
            SqlCondition::CtimeRange(a, b) =>
                ("ctime BETWEEN ? AND ?".into(), vec![Value::Integer(*a), Value::Integer(*b)]),
            SqlCondition::SizeRange(a, b) =>
                ("size_bytes BETWEEN ? AND ?".into(),
                 vec![Value::Integer(*a as i64), Value::Integer(*b as i64)]),
            SqlCondition::SizeOp(op, v) =>
                (format!("size_bytes {} ?", op.as_sql()),
                 vec![Value::Integer(*v as i64)]),
            SqlCondition::ExtIn(exts) => {
                let ph = exts.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                let vals = exts.iter().map(|e| Value::Text(e.clone())).collect();
                (format!("extension IN ({ph})"), vals)
            }
            SqlCondition::ParentPath(p) =>
                ("path LIKE ? || '/%'".into(), vec![Value::Text(p.clone())]),
            SqlCondition::NameLenOp(op, n) =>
                (format!("LENGTH(name) {} ?", op.as_sql()),
                 vec![Value::Integer(*n as i64)]),
            SqlCondition::DepthOp(op, d) =>
                (format!(
                    "(LENGTH(path) - LENGTH(REPLACE(path, '/', ''))) {} ?",
                    op.as_sql()
                ), vec![Value::Integer(*d as i64)]),
            SqlCondition::IsArchiveEntry =>
                ("archive_path IS NOT NULL".into(), vec![]),
            SqlCondition::ArchiveOf(ap) =>
                ("archive_path = ?".into(), vec![Value::Text(ap.clone())]),
            SqlCondition::AttribAll(mask) =>
                (format!("(attrib_bitmask & {mask}) = {mask}"), vec![]),
            SqlCondition::AttribAny(mask) =>
                (format!("(attrib_bitmask & {mask}) != 0"), vec![]),
        }
    }
}

/// Build a complete AND clause from a slice of `SqlCondition`s.
/// Returns `("")` if the slice is empty, else `" AND cond1 AND cond2 ..."`.
pub fn build_conditions(
    conditions: &[SqlCondition],
) -> (String, Vec<rusqlite::types::Value>) {
    if conditions.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut parts   = Vec::new();
    let mut all_params = Vec::new();
    for cond in conditions {
        let (sql, params) = cond.to_sql();
        parts.push(sql);
        all_params.extend(params);
    }
    (format!(" AND {}", parts.join(" AND ")), all_params)
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
        let f = MetaFilters { min_size: Some(100), ..Default::default() };
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
