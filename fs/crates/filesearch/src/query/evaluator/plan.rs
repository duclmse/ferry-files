use chrono::Local;

use crate::error::{AppError, Result};
use crate::query::ast::{
    DateExpr, Expr, Function, Modifiers, Predicate, PredicateKind, RangeExpr,
};
use crate::query::date::resolve_date_constant;
use crate::query::glob::{glob_to_sqlite, has_wildcards};
use crate::query::macros::expand_type_macro;
use crate::store::{CmpOp, SqlCondition};

use super::postfilter::build_regex;
use super::types::{Branch, EvalStrategy, PostFilter, QueryPlan};

// ---------------------------------------------------------------------------
// Plan builder
// ---------------------------------------------------------------------------

pub fn plan(expr: &Expr, default_mods: &Modifiers) -> Result<QueryPlan> {
    let mut p = QueryPlan {
        sql_conditions: Vec::new(),
        tantivy_query:  None,
        post_filters:   Vec::new(),
        limit:          None,
        offset:         0,
        strategy:       EvalStrategy::SqliteOnly,
    };
    walk(expr, default_mods, &mut p)?;
    Ok(p)
}

fn walk(expr: &Expr, inherited: &Modifiers, p: &mut QueryPlan) -> Result<()> {
    match expr {
        Expr::And(l, r) => {
            walk(l, inherited, p)?;
            walk(r, inherited, p)?;
        }
        Expr::Or(l, r) => {
            let mut branches = Vec::new();
            for side in [l, r] {
                let sp = sub_plan(side, inherited)?;
                require_plain(&sp, "OR")?;
                p.limit = p.limit.or(sp.limit);
                // Flatten `a | b | c` into one OR instead of nesting.
                match <[SqlCondition; 1]>::try_from(sp.sql_conditions) {
                    Ok([SqlCondition::Or(inner)]) if sp.post_filters.is_empty() => {
                        branches.extend(inner.into_iter().map(|sql| Branch { sql, post: Vec::new() }));
                    }
                    Ok(one) => branches.push(Branch { sql: one.into(), post: sp.post_filters }),
                    Err(sql) => branches.push(Branch { sql, post: sp.post_filters }),
                }
            }
            // Each branch's SQL alone is a superset of the branch, so the OR of
            // them is a superset of the whole OR; post-filters then make it exact.
            p.sql_conditions.push(SqlCondition::Or(
                branches.iter().map(|b| b.sql.clone()).collect(),
            ));
            if branches.iter().any(|b| !b.post.is_empty()) {
                p.post_filters.push(PostFilter::Any(branches));
            }
        }
        Expr::Not(inner) => {
            let np = sub_plan(inner, inherited)?;
            require_plain(&np, "NOT")?;
            p.limit = p.limit.or(np.limit);
            if np.post_filters.is_empty() {
                p.sql_conditions.push(SqlCondition::Not(np.sql_conditions));
            } else {
                // NOT of a superset is a subset, so SQL cannot narrow here.
                p.post_filters.push(PostFilter::Not(Branch {
                    sql:  np.sql_conditions,
                    post: np.post_filters,
                }));
            }
        }
        Expr::Predicate(pred) => {
            let mods = merge_mods(inherited, &pred.modifiers);
            emit_predicate(pred, &mods, p)?;
        }
    }
    Ok(())
}

fn sub_plan(expr: &Expr, inherited: &Modifiers) -> Result<QueryPlan> {
    let mut p = QueryPlan {
        sql_conditions: Vec::new(),
        tantivy_query:  None,
        post_filters:   Vec::new(),
        limit:          None,
        offset:         0,
        strategy:       EvalStrategy::SqliteOnly,
    };
    walk(expr, inherited, &mut p)?;
    Ok(p)
}

/// `content:`, `fuzzy:` and `dupe:` pick a whole-query execution strategy, so
/// they cannot be combined per-branch; rejecting beats silently AND-ing them.
fn require_plain(sp: &QueryPlan, op: &str) -> Result<()> {
    if matches!(sp.strategy, EvalStrategy::SqliteOnly) && sp.tantivy_query.is_none() {
        Ok(())
    } else {
        Err(AppError::Query(format!(
            "content:, fuzzy: and dupe: cannot be used inside {op}"
        )))
    }
}

// ---------------------------------------------------------------------------
// Predicate → SqlConditions
// ---------------------------------------------------------------------------

fn emit_predicate(pred: &Predicate, mods: &Modifiers, p: &mut QueryPlan) -> Result<()> {
    // Global modifier flags
    if mods.files_only == Some(true) {
        p.sql_conditions.push(SqlCondition::IsDir(false));
    }
    if mods.folders_only == Some(true) {
        p.sql_conditions.push(SqlCondition::IsDir(true));
    }
    match mods.archive_only {
        Some(true)  => p.sql_conditions.push(SqlCondition::IsArchiveEntry),
        Some(false) => {
            // archive_path IS NULL - not directly representable; use post-filter
            p.post_filters.push(PostFilter::ArchiveExclude);
        }
        None => {}
    }

    let now = Local::now().fixed_offset();

    match &pred.kind {
        PredicateKind::Term(s) => {
            let case_sensitive = mods.case_sensitive.unwrap_or(false);
            let match_path     = mods.match_path.unwrap_or(false);
            let whole_fn       = mods.whole_filename.unwrap_or(false);
            let whole_word     = mods.whole_word.unwrap_or(false);

            if has_wildcards(s) {
                let glob = glob_to_sqlite(s, whole_fn || has_wildcards(s));
                if match_path {
                    p.sql_conditions.push(SqlCondition::PathLike(format!("%{s}%")));
                } else if case_sensitive {
                    p.sql_conditions.push(SqlCondition::NameGlob(glob));
                } else {
                    p.sql_conditions.push(SqlCondition::NameGlob(glob.to_ascii_lowercase()));
                }
            } else {
                let pattern = if match_path {
                    format!("%{s}%")
                } else if whole_fn {
                    s.clone()
                } else {
                    format!("%{s}%")
                };
                if match_path {
                    p.sql_conditions.push(SqlCondition::PathLike(pattern));
                } else {
                    p.sql_conditions.push(SqlCondition::NameLike(pattern));
                }
            }

            if whole_word {
                p.post_filters.push(PostFilter::WholeWord(s.clone(), case_sensitive));
            }
        }

        PredicateKind::Regex(pattern) => {
            let case_sensitive = mods.case_sensitive.unwrap_or(false);
            let re = build_regex(pattern, case_sensitive)?;
            // No SQL narrowing - regex must be applied post-fetch
            p.post_filters.push(PostFilter::Regex(re, case_sensitive));
        }

        PredicateKind::Fuzzy(term) => {
            p.strategy = EvalStrategy::FuzzyFilter(term.clone());
        }

        PredicateKind::TypeMacro(tm) => {
            let exts = expand_type_macro(*tm);
            p.sql_conditions.push(SqlCondition::ExtIn(exts));
        }

        PredicateKind::Function(func) => {
            emit_function(func, mods, p, now)?;
        }
    }

    Ok(())
}

fn emit_function(
    func:   &Function,
    _mods:  &Modifiers,
    p:      &mut QueryPlan,
    now:    chrono::DateTime<chrono::FixedOffset>,
) -> Result<()> {
    match func {
        Function::DateModified(de) => {
            let (lo, hi) = date_expr_to_range(de, now);
            p.sql_conditions.push(SqlCondition::MtimeRange(lo, hi));
        }
        Function::DateCreated(de) => {
            let (lo, hi) = date_expr_to_range(de, now);
            p.sql_conditions.push(SqlCondition::CtimeRange(lo, hi));
        }
        Function::DateAccessed(de) => {
            // No `atime` column - approximate with mtime
            let (lo, hi) = date_expr_to_range(de, now);
            p.sql_conditions.push(SqlCondition::MtimeRange(lo, hi));
        }
        Function::Size(range) => {
            match range {
                RangeExpr::Eq(v)       => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Eq, *v)),
                RangeExpr::Lt(v)       => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Lt, *v)),
                RangeExpr::Lte(v)      => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Lte, *v)),
                RangeExpr::Gt(v)       => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Gt, *v)),
                RangeExpr::Gte(v)      => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Gte, *v)),
                RangeExpr::Between(a, b) => p.sql_conditions.push(SqlCondition::SizeRange(*a, *b)),
            }
        }
        Function::Ext(exts) => {
            p.sql_conditions.push(SqlCondition::ExtIn(exts.clone()));
        }
        Function::Path(path) => {
            p.sql_conditions.push(SqlCondition::PathLike(format!("{path}%")));
        }
        Function::Parent(path) => {
            p.sql_conditions.push(SqlCondition::ParentPath(path.clone()));
        }
        Function::Content(text) => {
            p.tantivy_query = Some(text.clone());
            p.strategy      = EvalStrategy::TantivyThenSqlite;
        }
        Function::Dupe(kind) => {
            p.strategy = EvalStrategy::DupeSearch(*kind);
        }
        Function::Count(n) => {
            p.limit = Some(*n);
        }
        Function::Len(op, n) => {
            p.sql_conditions.push(SqlCondition::NameLenOp(*op, *n));
        }
        Function::Depth(op, n) => {
            p.sql_conditions.push(SqlCondition::DepthOp(*op, *n));
        }
        Function::Attrib(mask) => {
            p.sql_conditions.push(SqlCondition::AttribAll(*mask));
        }
        Function::Child(name) => {
            p.post_filters.push(PostFilter::Child(name.clone()));
        }
        Function::StartWith(s) => {
            p.post_filters.push(PostFilter::StartWith(s.clone()));
        }
        Function::EndWith(s) => {
            p.post_filters.push(PostFilter::EndWith(s.clone()));
        }
        Function::ArchiveOf(path) => {
            p.sql_conditions.push(SqlCondition::ArchiveOf(path.clone()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Date helpers
// ---------------------------------------------------------------------------

fn date_expr_to_range(de: &DateExpr, now: chrono::DateTime<chrono::FixedOffset>) -> (i64, i64) {
    match de {
        DateExpr::Constant(c)        => resolve_date_constant(c, now),
        DateExpr::Range(lo, hi)      => (*lo, *hi),
        DateExpr::Comparison(op, ts) => match op {
            CmpOp::Lt  => (i64::MIN, *ts - 1),
            CmpOp::Lte => (i64::MIN, *ts),
            CmpOp::Gt  => (*ts + 1, i64::MAX),
            CmpOp::Gte => (*ts, i64::MAX),
            CmpOp::Eq  => (*ts, *ts),
        },
    }
}

// ---------------------------------------------------------------------------
// Modifier merge
// ---------------------------------------------------------------------------

fn merge_mods(parent: &Modifiers, child: &Modifiers) -> Modifiers {
    Modifiers {
        case_sensitive:    child.case_sensitive.or(parent.case_sensitive),
        match_path:        child.match_path.or(parent.match_path),
        files_only:        child.files_only.or(parent.files_only),
        folders_only:      child.folders_only.or(parent.folders_only),
        whole_filename:    child.whole_filename.or(parent.whole_filename),
        whole_word:        child.whole_word.or(parent.whole_word),
        ignore_diacritics: child.ignore_diacritics.or(parent.ignore_diacritics),
        archive_only:      child.archive_only.or(parent.archive_only),
    }
}
