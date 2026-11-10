use regex::Regex;

use crate::store::{DupeKind, FileRecord, Sort, SqlCondition};

// ---------------------------------------------------------------------------
// Public result type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub hits: Vec<FileRecord>,
    pub total: usize,
    pub took_ms: u64,
    pub mode_used: EvalStrategy,
}

// ---------------------------------------------------------------------------
// QueryPlan
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct QueryPlan {
    pub sql_conditions: Vec<SqlCondition>,
    pub tantivy_query: Option<String>,
    pub post_filters: Vec<PostFilter>,
    pub limit: Option<usize>,
    pub sort: Option<Sort>,
    pub offset: usize,
    pub strategy: EvalStrategy,
}

#[derive(Debug, Clone)]
pub enum EvalStrategy {
    SqliteOnly,
    TantivyThenSqlite,
    DupeSearch(DupeKind),
    FuzzyFilter(String),
}

#[derive(Debug)]
pub enum PostFilter {
    Regex {
        re: Regex,
        on_path: bool,
    },
    Child(String),
    WholeWord(String, bool),
    /// Rechecks of text that SQL matched as a superset: SQL either ignored
    /// case under `case:`, or widened non-ASCII letters it cannot fold.
    StartWith {
        text: String,
        case_sensitive: bool,
    },
    EndWith {
        text: String,
        case_sensitive: bool,
    },
    Literal {
        text: String,
        whole: bool,
        on_path: bool,
        case_sensitive: bool,
    },
    /// `archiveof:` with wildcards, matched against the outer archive's path.
    ArchiveGlob {
        pattern: String,
        case_sensitive: bool,
    },
    /// `*`/`?` match over the whole name or path.
    Glob {
        pattern: String,
        on_path: bool,
        case_sensitive: bool,
    },
    ArchiveExclude,
    /// OR over branches that need post-fetch checks; SQL only narrows to a superset.
    Any(Vec<Branch>),
    /// NOT of a branch that needs post-fetch checks.
    Not(Branch),
}

/// One side of an OR / the inside of a NOT: matches when all of its SQL
/// conditions and all of its post-filters match.
#[derive(Debug)]
pub struct Branch {
    pub sql: Vec<SqlCondition>,
    pub post: Vec<PostFilter>,
}
