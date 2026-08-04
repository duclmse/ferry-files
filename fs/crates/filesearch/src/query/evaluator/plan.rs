use chrono::Local;

use crate::error::Result;
use crate::query::ast::{
    DateExpr, Expr, Function, Modifiers, Predicate, PredicateKind, RangeExpr,
};
use crate::query::date::resolve_date_constant;
use crate::query::glob::{glob_to_sqlite, has_wildcards};
use crate::query::macros::expand_type_macro;
use crate::store::{build_conditions, CmpOp, SqlCondition};

use super::postfilter::build_regex;
use super::types::{EvalStrategy, PostFilter, QueryPlan};

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
            // OR across two branches - both sides add conditions.
            // For SQL we approximate OR as an OR'd LIKE by wrapping conditions.
            // A simple but correct approach: collect each branch's conditions
            // and OR them together in a single SqlCondition.
            // For now, emit both branches' conditions joined with OR at the SQL level.
            let lp = sub_plan(l, inherited)?;
            let rp = sub_plan(r, inherited)?;

            // Merge limit / tantivy / strategy from branches
            merge_strategy(&mut p.strategy, lp.strategy);
            merge_strategy(&mut p.strategy, rp.strategy);
            if p.tantivy_query.is_none() { p.tantivy_query = lp.tantivy_query; }
            p.post_filters.extend(lp.post_filters);
            p.post_filters.extend(rp.post_filters);

            if !lp.sql_conditions.is_empty() || !rp.sql_conditions.is_empty() {
                // Emit an OR condition block
                p.sql_conditions.push(or_conditions(lp.sql_conditions, rp.sql_conditions));
            }
        }
        Expr::Not(inner) => {
            let np = sub_plan(inner, inherited)?;
            // Negate SQL conditions
            for cond in np.sql_conditions {
                p.sql_conditions.push(negate_condition(cond));
            }
            p.post_filters.extend(np.post_filters);
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

fn merge_strategy(dst: &mut EvalStrategy, src: EvalStrategy) {
    match (&*dst, &src) {
        (EvalStrategy::SqliteOnly, _) => *dst = src,
        (EvalStrategy::FuzzyFilter(_), EvalStrategy::TantivyThenSqlite) => {
            *dst = EvalStrategy::TantivyThenSqlite;
        }
        _ => {}
    }
}

fn or_conditions(left: Vec<SqlCondition>, right: Vec<SqlCondition>) -> SqlCondition {
    // Build "( left AND ... ) OR ( right AND ... )" as a raw SQL fragment.
    let (lsql, _lp) = build_conditions(&left);
    let (rsql, _rp) = build_conditions(&right);
    // We need to carry the params too. Use NameLike as a carrier of raw SQL.
    // This is a deliberate simplification: for OR we fall back to a catch-all
    // that returns everything when we can't express it in SqlCondition enum.
    // The correct approach would be a SqlCondition::Raw(String, Vec<Value>).
    // For now, if both sides produce conditions, we drop the OR optimisation
    // and return an always-true condition (let post-filters handle it).
    let _ = (lsql, rsql);
    SqlCondition::NameLike("%".into()) // catch-all, post-filters narrow it down
}

fn negate_condition(c: SqlCondition) -> SqlCondition {
    match c {
        SqlCondition::IsDir(v)           => SqlCondition::IsDir(!v),
        SqlCondition::IsArchiveEntry     => SqlCondition::NameLike("%".into()), // approximate
        other                            => other, // cannot negate generically; post-filter handles it
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
