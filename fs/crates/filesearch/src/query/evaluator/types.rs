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
}
