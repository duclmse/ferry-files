use super::state::{EntryRef, NO_PARENT};
use super::{CmpOp, DupeKind, MetaFilters, RecordView, SqlCondition};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Borrowed so grouping millions of entries allocates nothing per entry.
/// Numbers order numerically, matching SQLite's `ORDER BY` on those columns.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum DupeKey<'a> {
    Text(&'a str),
    Number(i128),
}

pub(super) fn dupe_key<'a>(e: &EntryRef<'a>, kind: DupeKind) -> DupeKey<'a> {
    let name = e.name_str();
    match kind {
        DupeKind::Name => DupeKey::Text(name),
        DupeKind::NamePart => DupeKey::Text(
            e.ext_str()
                .and_then(|x| name.strip_suffix(x)?.strip_suffix('.'))
                .unwrap_or(name),
        ),
        DupeKind::Size => DupeKey::Number(i128::from(e.size_bytes())),
        DupeKind::DateModified | DupeKind::DateAccessed => DupeKey::Number(i128::from(e.mtime())),
        DupeKind::DateCreated => DupeKey::Number(i128::from(e.ctime())),
        DupeKind::Attrib => DupeKey::Number(i128::from(e.attrib_bitmask())),
    }
}

pub(super) fn meta_filter_matches<R: RecordView + ?Sized>(r: &R, f: &MetaFilters) -> bool {
    if let Some(min) = f.min_size {
        if r.size_bytes() < min {
            return false;
        }
    }
    if let Some(max) = f.max_size {
        if r.size_bytes() > max {
            return false;
        }
    }
    if let Some(after) = f.after {
        if r.mtime() < after {
            return false;
        }
    }
    if let Some(before) = f.before {
        if r.mtime() > before {
            return false;
        }
    }
    if let Some(is_dir) = f.is_dir {
        if r.is_dir() != is_dir {
            return false;
        }
    }
    if let Some(exts) = &f.extensions {
        if !exts.is_empty() && !r.extension().is_some_and(|e| exts.iter().any(|x| x == e)) {
            return false;
        }
    }
    if let Some(prefix) = &f.path_prefix {
        if !r.path_is_under(prefix) {
            return false;
        }
    }
    true
}

/// Returns the inner literal if `pattern` is exactly `%<literal>%` with no other `%` or `_`.
fn extract_contains_pattern(pattern: &str) -> Option<&str> {
    let b = pattern.as_bytes();
    if b.len() < 3 || b[0] != b'%' || b.last() != Some(&b'%') {
        return None;
    }
    let inner = &pattern[1..pattern.len() - 1];
    if inner.contains('%') || inner.contains('_') {
        None
    } else {
        Some(inner)
    }
}

/// Returns the literal if `pattern` is exactly `<literal>%` with no other `%` or `_`.
fn extract_prefix_pattern(pattern: &str) -> Option<&str> {
    let inner = pattern.strip_suffix('%')?;
    (!inner.is_empty() && !inner.contains(['%', '_'])).then_some(inner)
}

/// Wildcard match without recursion or allocation; backtracks only to the
/// latest `many`, so a pattern with several of them stays O(text * pattern).
fn wild_match(text: &[u8], pattern: &[u8], many: u8, one: u8, fold_case: bool) -> bool {
    let same = |a: u8, b: u8| a == b || (fold_case && a.eq_ignore_ascii_case(&b));
    let (mut t, mut p) = (0, 0);
    let mut retry = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(&c) if c == many => {
                p += 1;
                retry = Some((p, t));
            }
            Some(&c) if c == one || same(c, text[t]) => {
                p += 1;
                t += 1;
            }
            _ => {
                let Some((after_many, start)) = retry else {
                    return false;
                };
                p = after_many;
                t = start + 1;
                retry = Some((after_many, t));
            }
        }
    }
    pattern[p..].iter().all(|&c| c == many)
}

fn contains_ignore_case(text: &[u8], needle: &[u8]) -> bool {
    let Some(last) = text.len().checked_sub(needle.len()) else {
        return false;
    };
    let Some(&first) = needle.first() else {
        return true;
    };
    let (lower, upper) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
    memchr::memchr2_iter(lower, upper, &text[..=last])
        .any(|i| text[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn starts_with_ignore_case(text: &[u8], literal: &[u8]) -> bool {
    text.get(..literal.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(literal))
}

/// A SQLite `LIKE` pattern, classified once per query rather than per entry.
/// Case-insensitive for ASCII; `%` = any sequence, `_` = any byte.
pub(super) enum Like<'a> {
    Prefix(&'a [u8]),
    Contains(&'a [u8]),
    General(&'a [u8]),
}

impl<'a> Like<'a> {
    pub(super) fn new(pattern: &'a str) -> Self {
        if let Some(needle) = extract_contains_pattern(pattern) {
            return Self::Contains(needle.as_bytes());
        }
        match extract_prefix_pattern(pattern) {
            Some(literal) => Self::Prefix(literal.as_bytes()),
            None => Self::General(pattern.as_bytes()),
        }
    }

    pub(super) fn matches(&self, text: &str) -> bool {
        match self {
            Self::Prefix(literal) => starts_with_ignore_case(text.as_bytes(), literal),
            Self::Contains(needle) => contains_ignore_case(text.as_bytes(), needle),
            Self::General(pattern) => wild_match(text.as_bytes(), pattern, b'%', b'_', true),
        }
    }
}

/// Which fast path a `GLOB` pattern qualifies for.
enum GlobShape<'a> {
    Prefix(&'a str),
    Suffix(&'a str),
    Contains(&'a str),
    General(&'a str),
}

impl<'a> GlobShape<'a> {
    fn of(pattern: &'a str) -> Self {
        let literal = |s: &str| !s.is_empty() && !s.contains(['*', '?']);
        match (pattern.strip_prefix('*'), pattern.strip_suffix('*')) {
            (Some(rest), Some(_)) if rest.ends_with('*') && literal(&rest[..rest.len() - 1]) => {
                Self::Contains(&rest[..rest.len() - 1])
            }
            (Some(rest), None) if literal(rest) => Self::Suffix(rest),
            (None, Some(rest)) if literal(rest) => Self::Prefix(rest),
            _ => Self::General(pattern),
        }
    }
}

/// A SQLite `GLOB` pattern, classified once per query rather than per entry.
/// Case-sensitive; `*` = any sequence, `?` = any byte.
pub(super) enum Glob<'a> {
    Prefix(&'a [u8]),
    Suffix(&'a [u8]),
    Contains(memchr::memmem::Finder<'a>),
    General(&'a [u8]),
}

impl<'a> Glob<'a> {
    pub(super) fn new(pattern: &'a str) -> Self {
        match GlobShape::of(pattern) {
            GlobShape::Prefix(s) => Self::Prefix(s.as_bytes()),
            GlobShape::Suffix(s) => Self::Suffix(s.as_bytes()),
            GlobShape::Contains(s) => Self::Contains(memchr::memmem::Finder::new(s)),
            GlobShape::General(s) => Self::General(s.as_bytes()),
        }
    }

    pub(super) fn matches(&self, text: &str) -> bool {
        let text = text.as_bytes();
        match self {
            Self::Prefix(literal) => text.starts_with(literal),
            Self::Suffix(literal) => text.ends_with(literal),
            Self::Contains(finder) => finder.find(text).is_some(),
            Self::General(pattern) => wild_match(text, pattern, b'*', b'?', false),
        }
    }
}

/// [`Glob`] ignoring ASCII case, like SQLite's `LOWER(x) GLOB LOWER(y)`.
pub(super) struct GlobNoCase<'a>(GlobShape<'a>);

impl<'a> GlobNoCase<'a> {
    pub(super) fn new(pattern: &'a str) -> Self {
        Self(GlobShape::of(pattern))
    }

    pub(super) fn matches(&self, text: &str) -> bool {
        let text = text.as_bytes();
        match self.0 {
            GlobShape::Prefix(literal) => starts_with_ignore_case(text, literal.as_bytes()),
            GlobShape::Suffix(literal) => text
                .len()
                .checked_sub(literal.len())
                .is_some_and(|start| text[start..].eq_ignore_ascii_case(literal.as_bytes())),
            GlobShape::Contains(literal) => contains_ignore_case(text, literal.as_bytes()),
            GlobShape::General(pattern) => wild_match(text, pattern.as_bytes(), b'*', b'?', true),
        }
    }
}

/// SQLite LIKE semantics for a single comparison.
pub(super) fn like_match(text: &str, pattern: &str) -> bool {
    Like::new(pattern).matches(text)
}

/// SQLite GLOB semantics for a single comparison.
pub(super) fn glob_match(text: &str, pattern: &str) -> bool {
    Glob::new(pattern).matches(text)
}

/// SQLite `LOWER(x) GLOB LOWER(y)` semantics for a single comparison.
pub(super) fn glob_nocase_match(text: &str, pattern: &str) -> bool {
    GlobNoCase::new(pattern).matches(text)
}

/// A query's conditions with their text patterns prepared, for scans that
/// test every entry. Anything else defers to [`SqlCondition::matches`].
pub(super) enum Compiled<'a> {
    NameLike(Like<'a>),
    NameGlob(Glob<'a>),
    NameGlobNoCase(GlobNoCase<'a>),
    /// The `usize` is this condition's slot in [`PathMemo::verdicts`].
    PathLike(Like<'a>, usize),
    Or(Vec<Vec<Compiled<'a>>>),
    Not(Vec<Compiled<'a>>),
    Other(&'a SqlCondition),
}

pub(super) fn compile(conds: &[SqlCondition]) -> Vec<Compiled<'_>> {
    compile_with(conds, &mut 0)
}

fn compile_with<'a>(conds: &'a [SqlCondition], slots: &mut usize) -> Vec<Compiled<'a>> {
    conds
        .iter()
        .map(|c| match c {
            SqlCondition::NameLike(p) => Compiled::NameLike(Like::new(p)),
            SqlCondition::NameGlob(p) => Compiled::NameGlob(Glob::new(p)),
            SqlCondition::NameGlobNoCase(p) => Compiled::NameGlobNoCase(GlobNoCase::new(p)),
            SqlCondition::PathLike(p) => {
                *slots += 1;
                Compiled::PathLike(Like::new(p), *slots - 1)
            }
            SqlCondition::Or(branches) => {
                Compiled::Or(branches.iter().map(|b| compile_with(b, slots)).collect())
            }
            SqlCondition::Not(inner) => Compiled::Not(compile_with(inner, slots)),
            other => Compiled::Other(other),
        })
        .collect()
}

impl Compiled<'_> {
    pub(super) fn matches(&self, e: &EntryRef<'_>, memo: &mut PathMemo) -> bool {
        match self {
            Self::NameLike(like) => like.matches(e.name_str()),
            Self::NameGlob(glob) => glob.matches(e.name_str()),
            Self::NameGlobNoCase(glob) => glob.matches(e.name_str()),
            Self::PathLike(like, slot) => memo.path_matches(e, like, *slot),
            Self::Or(branches) => branches
                .iter()
                .any(|b| b.iter().all(|c| c.matches(e, memo))),
            Self::Not(conds) => !conds.iter().all(|c| c.matches(e, memo)),
            Self::Other(condition) => condition.matches(e),
        }
    }
}

/// The directory most recently seen by one scan worker, with what is already
/// known about its path. Siblings are nearly always adjacent, so each
/// directory's path is built and tested once and its files inherit the
/// answer. The first full-path walk per entry was the bulk of a `path:` scan.
pub(super) struct PathMemo {
    parent: u32,
    path: String,
    parent_len: usize,
    verdicts: Vec<Option<bool>>,
}

impl PathMemo {
    pub(super) fn new() -> Self {
        Self {
            parent: NO_PARENT,
            path: String::new(),
            parent_len: 0,
            verdicts: Vec::new(),
        }
    }

    fn path_matches(&mut self, e: &EntryRef<'_>, like: &Like<'_>, slot: usize) -> bool {
        let parent = e.parent_index();
        if parent == NO_PARENT {
            // The tail is the whole path, and there is no directory to share.
            return like.matches(e.tail());
        }
        if parent != self.parent {
            self.parent = parent;
            self.path.clear();
            e.write_parent_path(&mut self.path);
            self.parent_len = self.path.len();
            self.verdicts.fill(None);
        }
        if slot >= self.verdicts.len() {
            self.verdicts.resize(slot + 1, None);
        }
        self.path.truncate(self.parent_len);
        self.path.push_str(e.tail());
        let path = self.path.as_bytes();
        let (dir, verdict) = (&path[..self.parent_len], &mut self.verdicts[slot]);
        match like {
            Like::Contains(needle) => {
                *verdict.get_or_insert_with(|| contains_ignore_case(dir, needle)) || {
                    // Only a match that reaches past the directory is left.
                    let start = self.parent_len.saturating_sub(needle.len() - 1);
                    contains_ignore_case(&path[start..], needle)
                }
            }
            Like::Prefix(literal) if dir.len() >= literal.len() => {
                *verdict.get_or_insert_with(|| starts_with_ignore_case(dir, literal))
            }
            _ => like.matches(&self.path),
        }
    }
}

// ---------------------------------------------------------------------------

impl SqlCondition {
    /// Evaluate this condition against an in-memory record.
    /// Mirrors the SQLite semantics of [`SqlCondition::to_sql`].
    pub fn matches<R: RecordView + ?Sized>(&self, r: &R) -> bool {
        match self {
            SqlCondition::NameLike(p) => like_match(r.name(), p),
            SqlCondition::NameGlob(p) => glob_match(r.name(), p),
            SqlCondition::NameGlobNoCase(p) => glob_nocase_match(r.name(), p),
            SqlCondition::PathLike(p) => r.with_path(|path| like_match(path, p)),
            SqlCondition::IsDir(v) => r.is_dir() == *v,
            SqlCondition::MtimeRange(a, b) => r.mtime() >= *a && r.mtime() <= *b,
            SqlCondition::CtimeRange(a, b) => r.ctime() >= *a && r.ctime() <= *b,
            SqlCondition::SizeRange(a, b) => r.size_bytes() >= *a && r.size_bytes() <= *b,
            SqlCondition::SizeOp(op, v) => cmp_op_u64(*op, r.size_bytes(), *v),
            SqlCondition::ExtIn(exts) => r.extension().is_some_and(|e| exts.iter().any(|x| x == e)),
            SqlCondition::ParentPath(p) => r.path_is_under(p),
            SqlCondition::NameLenOp(op, n) => cmp_op_usize(*op, r.name().len(), *n),
            SqlCondition::DepthOp(op, d) => cmp_op_usize(*op, r.path_depth(), *d),
            SqlCondition::IsArchiveEntry => r.is_archive_entry(),
            SqlCondition::ArchiveOf(ap) => r.with_archive_path(|a| a == Some(ap.as_str())),
            SqlCondition::AttribAll(mask) => r.attrib_bitmask() & mask == *mask,
            SqlCondition::AttribAny(mask) => r.attrib_bitmask() & mask != 0,
            SqlCondition::Or(branches) => branches.iter().any(|b| b.iter().all(|c| c.matches(r))),
            SqlCondition::Not(conds) => !conds.iter().all(|c| c.matches(r)),
        }
    }
}

fn cmp_op_u64(op: CmpOp, lhs: u64, rhs: u64) -> bool {
    match op {
        CmpOp::Lt => lhs < rhs,
        CmpOp::Lte => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Gte => lhs >= rhs,
        CmpOp::Eq => lhs == rhs,
    }
}

fn cmp_op_usize(op: CmpOp, lhs: usize, rhs: usize) -> bool {
    match op {
        CmpOp::Lt => lhs < rhs,
        CmpOp::Lte => lhs <= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Gte => lhs >= rhs,
        CmpOp::Eq => lhs == rhs,
    }
}
