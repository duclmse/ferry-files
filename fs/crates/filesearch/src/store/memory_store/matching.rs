use super::{CmpOp, DupeKind, MetaFilters, RecordView, SqlCondition};
use crate::store::is_under;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(super) fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        None | Some(0) => "",
        Some(i) => &path[..i],
    }
}

pub(super) fn dupe_key<R: RecordView + ?Sized>(r: &R, kind: DupeKind) -> String {
    let name = r.name();
    match kind {
        DupeKind::Name => name.to_owned(),
        DupeKind::NamePart => r
            .extension()
            .and_then(|e| name.strip_suffix(e)?.strip_suffix('.'))
            .unwrap_or(name)
            .to_owned(),
        DupeKind::Size => r.size_bytes().to_string(),
        DupeKind::DateModified | DupeKind::DateAccessed => r.mtime().to_string(),
        DupeKind::DateCreated => r.ctime().to_string(),
        DupeKind::Attrib => r.attrib_bitmask().to_string(),
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
        if !is_under(r.path(), prefix) {
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

/// SQLite LIKE semantics: case-insensitive ASCII, `%` = any sequence, `_` = any char.
///
/// Fast path: `%term%` (pure contains, no other wildcards) skips the backtracking matcher.
pub(super) fn like_match(text: &str, pattern: &str) -> bool {
    // Called once per record per query, so no per-call lowercase copies:
    // allocating two Strings per record dominated a 1M-record scan.
    if let Some(needle) = extract_contains_pattern(pattern) {
        let needle = needle.as_bytes();
        return needle.is_empty()
            || text
                .as_bytes()
                .windows(needle.len())
                .any(|w| w.eq_ignore_ascii_case(needle));
    }
    like_inner(
        text.to_ascii_lowercase().as_bytes(),
        pattern.to_ascii_lowercase().as_bytes(),
    )
}

fn like_inner(text: &[u8], pattern: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'%', rest @ ..] => (0..=text.len()).any(|i| like_inner(&text[i..], rest)),
        [b'_', rest @ ..] => !text.is_empty() && like_inner(&text[1..], rest),
        [c, rest @ ..] => !text.is_empty() && text[0] == *c && like_inner(&text[1..], rest),
    }
}

/// SQLite GLOB semantics: case-sensitive, `*` = any sequence, `?` = any char.
pub(super) fn glob_match(text: &str, pattern: &str) -> bool {
    glob_inner(text.as_bytes(), pattern.as_bytes())
}

fn glob_inner(text: &[u8], pattern: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'*', rest @ ..] => (0..=text.len()).any(|i| glob_inner(&text[i..], rest)),
        [b'?', rest @ ..] => !text.is_empty() && glob_inner(&text[1..], rest),
        [c, rest @ ..] => !text.is_empty() && text[0] == *c && glob_inner(&text[1..], rest),
    }
}

fn count_slashes(path: &str) -> usize {
    path.bytes().filter(|&b| b == b'/').count()
}

// ---------------------------------------------------------------------------

impl SqlCondition {
    /// Evaluate this condition against an in-memory record.
    /// Mirrors the SQLite semantics of [`SqlCondition::to_sql`].
    pub fn matches<R: RecordView + ?Sized>(&self, r: &R) -> bool {
        match self {
            SqlCondition::NameLike(p) => like_match(r.name(), p),
            SqlCondition::NameGlob(p) => glob_match(r.name(), p),
            SqlCondition::PathLike(p) => like_match(r.path(), p),
            SqlCondition::IsDir(v) => r.is_dir() == *v,
            SqlCondition::MtimeRange(a, b) => r.mtime() >= *a && r.mtime() <= *b,
            SqlCondition::CtimeRange(a, b) => r.ctime() >= *a && r.ctime() <= *b,
            SqlCondition::SizeRange(a, b) => r.size_bytes() >= *a && r.size_bytes() <= *b,
            SqlCondition::SizeOp(op, v) => cmp_op_u64(*op, r.size_bytes(), *v),
            SqlCondition::ExtIn(exts) => r.extension().is_some_and(|e| exts.iter().any(|x| x == e)),
            SqlCondition::ParentPath(p) => is_under(r.path(), p),
            SqlCondition::NameLenOp(op, n) => cmp_op_usize(*op, r.name().len(), *n),
            SqlCondition::DepthOp(op, d) => cmp_op_usize(*op, count_slashes(r.path()), *d),
            SqlCondition::IsArchiveEntry => r.archive_path().is_some(),
            SqlCondition::ArchiveOf(ap) => r.archive_path() == Some(ap.as_str()),
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
