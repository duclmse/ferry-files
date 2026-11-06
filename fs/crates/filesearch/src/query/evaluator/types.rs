use regex::Regex;

use crate::store::{DupeKind, FileRecord, SqlCondition};

// ---------------------------------------------------------------------------
// Public result type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub hits:      Vec<FileRecord>,
    pub total:     usize,
    pub took_ms:   u64,
    pub mode_used: EvalStrategy,
}

// ---------------------------------------------------------------------------
// QueryPlan
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct QueryPlan {
    pub sql_conditions: Vec<SqlCondition>,
    pub tantivy_query:  Option<String>,
    pub post_filters:   Vec<PostFilter>,
    pub limit:          Option<usize>,
    pub offset:         usize,
    pub strategy:       EvalStrategy,
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
    Regex(Regex, bool),
    Child(String),
    WholeWord(String, bool),
    StartWith(String),
    EndWith(String),
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
    pub sql:  Vec<SqlCondition>,
    pub post: Vec<PostFilter>,
}
