use crate::store::{CmpOp, DupeKind, Sort};

#[derive(Debug, Clone)]
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Predicate(Predicate),
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub kind: PredicateKind,
    pub modifiers: Modifiers,
}

#[derive(Debug, Clone)]
pub enum PredicateKind {
    Term(String),
    Regex(String),
    Function(Function),
    TypeMacro(TypeMacro),
    Fuzzy(String),
}

#[derive(Debug, Clone, Default)]
pub struct Modifiers {
    pub case_sensitive: Option<bool>,
    pub match_path: Option<bool>,
    pub files_only: Option<bool>,
    pub folders_only: Option<bool>,
    pub whole_filename: Option<bool>,
    pub whole_word: Option<bool>,
    pub ignore_diacritics: Option<bool>,
    /// Treat the term as a regular expression (`regex: a.b`).
    pub regex: Option<bool>,
    /// `Some(false)` matches `*` and `?` literally.
    pub wildcards: Option<bool>,
    /// None = include both real files and inner archive entries.
    /// Some(true)  = inner-archive entries only.
    /// Some(false) = real filesystem files only.
    pub archive_only: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum RangeExpr<T: Clone> {
    Eq(T),
    Lt(T),
    Lte(T),
    Gt(T),
    Gte(T),
    Between(T, T),
}

#[derive(Debug, Clone)]
pub enum DateConstant {
    Today,
    Yesterday,
    ThisWeek,
    LastWeek,
    ThisMonth,
    LastMonth,
    ThisYear,
    LastYear,
    LastNDays(u32),
    LastNHours(u32),
    LastNMinutes(u32),
    LastNWeeks(u32),
    LastNMonths(u32),
    /// Named month, 1-indexed (1=January, 12=December).
    Month(u32),
    /// Day-of-week, 0=Sunday … 6=Saturday.
    Weekday(u32),
}

#[derive(Debug, Clone)]
pub enum DateExpr {
    Constant(DateConstant),
    /// Absolute range of Unix seconds (lo, hi inclusive).
    Range(i64, i64),
    /// `<op> timestamp` - open-ended comparison.
    Comparison(CmpOp, i64),
}

#[derive(Debug, Clone)]
pub enum Function {
    DateModified(DateExpr),
    DateCreated(DateExpr),
    DateAccessed(DateExpr),
    Size(RangeExpr<u64>),
    Path(String),
    Parent(String),
    Ext(Vec<String>),
    Content(String),
    Dupe(DupeKind),
    Count(usize),
    Sort(Sort),
    Len(CmpOp, usize),
    Depth(CmpOp, usize),
    Attrib(u32),
    Child(String),
    StartWith(String),
    EndWith(String),
    ArchiveOf(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeMacro {
    Audio,
    Video,
    Picture,
    Document,
    Archive,
    Executable,
    Source,
    Font,
}
