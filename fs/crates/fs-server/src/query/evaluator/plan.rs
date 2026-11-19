use chrono::Local;

use crate::error::{AppError, Result};
use crate::query::ast::{DateExpr, Expr, Function, Modifiers, Predicate, PredicateKind, RangeExpr};
use crate::query::date::resolve_date_constant;
use crate::query::glob::{
    glob_to_like, glob_to_like_nocase, glob_to_sqlite, has_wildcards, like_escape,
    like_escape_nocase,
};
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
        tantivy_query: None,
        post_filters: Vec::new(),
        limit: None,
        sort: None,
        offset: 0,
        strategy: EvalStrategy::SqliteOnly,
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
                p.sort = p.sort.or(sp.sort);
                // Flatten `a | b | c` into one OR instead of nesting.
                match <[SqlCondition; 1]>::try_from(sp.sql_conditions) {
                    Ok([SqlCondition::Or(inner)]) if sp.post_filters.is_empty() => {
                        branches.extend(inner.into_iter().map(|sql| Branch {
                            sql,
                            post: Vec::new(),
                        }));
                    }
                    Ok(one) => branches.push(Branch {
                        sql: one.into(),
                        post: sp.post_filters,
                    }),
                    Err(sql) => branches.push(Branch {
                        sql,
                        post: sp.post_filters,
                    }),
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
            p.sort = p.sort.or(np.sort);
            if np.post_filters.is_empty() {
                p.sql_conditions.push(SqlCondition::Not(np.sql_conditions));
            } else {
                // NOT of a superset is a subset, so SQL cannot narrow here.
                p.post_filters.push(PostFilter::Not(Branch {
                    sql: np.sql_conditions,
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
        tantivy_query: None,
        post_filters: Vec::new(),
        limit: None,
        sort: None,
        offset: 0,
        strategy: EvalStrategy::SqliteOnly,
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
        Some(true) => p.sql_conditions.push(SqlCondition::IsArchiveEntry),
        Some(false) => {
            // archive_path IS NULL - not directly representable; use post-filter
            p.post_filters.push(PostFilter::ArchiveExclude);
        }
        None => {}
    }

    let now = Local::now().fixed_offset();

    match &pred.kind {
        PredicateKind::Term(s) if mods.regex == Some(true) => {
            push_regex(s, mods, p)?;
        }

        PredicateKind::Term(s) => {
            let case_sensitive = mods.case_sensitive.unwrap_or(false);
            // Like Everything, a term with a path separator searches paths.
            let match_path = mods
                .match_path
                .unwrap_or_else(|| s.contains('/') || s.contains('\\'));
            let whole_fn = mods.whole_filename.unwrap_or(false);
            let whole_word = mods.whole_word.unwrap_or(false);
            let wildcards = mods.wildcards.unwrap_or(true) && has_wildcards(s);

            if wildcards {
                // Wildcards match the whole filename, and with path: the whole path.
                let (like, widened) =
                    like_pattern(s, case_sensitive, glob_to_like, glob_to_like_nocase);
                let recheck = case_sensitive || widened;
                // SQLite's GLOB reads `[` as a character class, and its LOWER()
                // folds ASCII only, so those names go through LIKE as well.
                if match_path || widened || s.contains('[') {
                    p.sql_conditions.push(if match_path {
                        SqlCondition::PathLike(like)
                    } else {
                        SqlCondition::NameLike(like)
                    });
                    if recheck {
                        p.post_filters.push(PostFilter::Glob {
                            pattern: s.clone(),
                            on_path: match_path,
                            case_sensitive,
                        });
                    }
                } else if case_sensitive {
                    p.sql_conditions
                        .push(SqlCondition::NameGlob(glob_to_sqlite(s, true)));
                } else {
                    p.sql_conditions
                        .push(SqlCondition::NameGlobNoCase(glob_to_sqlite(s, true)));
                }
            } else {
                let (literal, widened) =
                    like_pattern(s, case_sensitive, like_escape, like_escape_nocase);
                let whole = whole_fn && !match_path;
                let pattern = if whole {
                    literal
                } else {
                    format!("%{literal}%")
                };
                if match_path {
                    p.sql_conditions.push(SqlCondition::PathLike(pattern));
                } else {
                    p.sql_conditions.push(SqlCondition::NameLike(pattern));
                }
                if case_sensitive || widened {
                    p.post_filters.push(PostFilter::Literal {
                        text: s.clone(),
                        whole,
                        on_path: match_path,
                        case_sensitive,
                    });
                }
            }

            if whole_word {
                p.post_filters
                    .push(PostFilter::WholeWord(s.clone(), case_sensitive));
            }
        }

        PredicateKind::Regex(pattern) => {
            push_regex(pattern, mods, p)?;
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
    func: &Function,
    mods: &Modifiers,
    p: &mut QueryPlan,
    now: chrono::DateTime<chrono::FixedOffset>,
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
        Function::Size(range) => match range {
            RangeExpr::Eq(v) => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Eq, *v)),
            RangeExpr::Lt(v) => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Lt, *v)),
            RangeExpr::Lte(v) => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Lte, *v)),
            RangeExpr::Gt(v) => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Gt, *v)),
            RangeExpr::Gte(v) => p.sql_conditions.push(SqlCondition::SizeOp(CmpOp::Gte, *v)),
            RangeExpr::Between(a, b) => p.sql_conditions.push(SqlCondition::SizeRange(*a, *b)),
        },
        Function::Ext(exts) => {
            p.sql_conditions.push(SqlCondition::ExtIn(exts.clone()));
        }
        Function::Path(path) => {
            p.sql_conditions
                .push(SqlCondition::PathLike(format!("{}%", like_escape(path))));
        }
        Function::Parent(path) => {
            // ParentPath covers the whole subtree (and the memory store's
            // per-directory fast path); the depth check keeps direct children.
            let dir = path.trim_end_matches(['/', '\\']);
            let depth = dir.bytes().filter(|&b| b == b'/' || b == b'\\').count();
            p.sql_conditions
                .push(SqlCondition::ParentPath(dir.to_owned()));
            p.sql_conditions
                .push(SqlCondition::DepthOp(CmpOp::Eq, depth + 1));
        }
        Function::Content(text) => {
            p.tantivy_query = Some(text.clone());
            p.strategy = EvalStrategy::TantivyThenSqlite;
        }
        Function::Dupe(kind) => {
            p.strategy = EvalStrategy::DupeSearch(*kind);
        }
        Function::Count(n) => {
            p.limit = Some(*n);
        }
        Function::Sort(sort) => {
            p.sort = Some(*sort);
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
            let case_sensitive = mods.case_sensitive == Some(true);
            let (literal, widened) =
                like_pattern(s, case_sensitive, like_escape, like_escape_nocase);
            p.sql_conditions
                .push(SqlCondition::NameLike(format!("{literal}%")));
            if case_sensitive || widened {
                p.post_filters.push(PostFilter::StartWith {
                    text: s.clone(),
                    case_sensitive,
                });
            }
        }
        Function::EndWith(s) => {
            let case_sensitive = mods.case_sensitive == Some(true);
            let (literal, widened) =
                like_pattern(s, case_sensitive, like_escape, like_escape_nocase);
            p.sql_conditions
                .push(SqlCondition::NameLike(format!("%{literal}")));
            if case_sensitive || widened {
                p.post_filters.push(PostFilter::EndWith {
                    text: s.clone(),
                    case_sensitive,
                });
            }
        }
        Function::ArchiveOf(path) if has_wildcards(path) => {
            p.sql_conditions.push(SqlCondition::IsArchiveEntry);
            p.post_filters.push(PostFilter::ArchiveGlob {
                pattern: path.clone(),
                case_sensitive: mods.case_sensitive == Some(true),
            });
        }
        Function::ArchiveOf(path) => {
            p.sql_conditions.push(SqlCondition::ArchiveOf(path.clone()));
        }
    }
    Ok(())
}

/// The SQL `LIKE` pattern for `s` and whether it was widened. Either way it
/// selects a superset: `LIKE` ignores ASCII case even for `case:`, and a
/// widened pattern needs a [`fold_case`](crate::query::glob::fold_case) recheck.
fn like_pattern(
    s: &str,
    case_sensitive: bool,
    exact: fn(&str) -> String,
    nocase: fn(&str) -> (String, bool),
) -> (String, bool) {
    if case_sensitive {
        (exact(s), false)
    } else {
        nocase(s)
    }
}

// ---------------------------------------------------------------------------
// Date helpers
// ---------------------------------------------------------------------------

fn date_expr_to_range(de: &DateExpr, now: chrono::DateTime<chrono::FixedOffset>) -> (i64, i64) {
    match de {
        DateExpr::Constant(c) => resolve_date_constant(c, now),
        DateExpr::Range(lo, hi) => (*lo, *hi),
        DateExpr::Comparison(op, inner) => {
            let (lo, hi) = date_expr_to_range(inner, now);
            match op {
                CmpOp::Lt => (i64::MIN, lo.saturating_sub(1)),
                CmpOp::Lte => (i64::MIN, hi),
                CmpOp::Gt => (hi.saturating_add(1), i64::MAX),
                CmpOp::Gte => (lo, i64::MAX),
                CmpOp::Eq => (lo, hi),
            }
        }
        DateExpr::Between(a, b) => (date_expr_to_range(a, now).0, date_expr_to_range(b, now).1),
    }
}

/// Like Everything, a regex matches the filename unless `path:` is given.
/// There is no SQL narrowing, so it always runs post-fetch.
fn push_regex(pattern: &str, mods: &Modifiers, p: &mut QueryPlan) -> Result<()> {
    let re = build_regex(pattern, mods.case_sensitive.unwrap_or(false))?;
    p.post_filters.push(PostFilter::Regex {
        re,
        on_path: mods.match_path.unwrap_or(false),
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Modifier merge
// ---------------------------------------------------------------------------

fn merge_mods(parent: &Modifiers, child: &Modifiers) -> Modifiers {
    Modifiers {
        case_sensitive: child.case_sensitive.or(parent.case_sensitive),
        match_path: child.match_path.or(parent.match_path),
        files_only: child.files_only.or(parent.files_only),
        folders_only: child.folders_only.or(parent.folders_only),
        whole_filename: child.whole_filename.or(parent.whole_filename),
        whole_word: child.whole_word.or(parent.whole_word),
        ignore_diacritics: child.ignore_diacritics.or(parent.ignore_diacritics),
        regex: child.regex.or(parent.regex),
        wildcards: child.wildcards.or(parent.wildcards),
        archive_only: child.archive_only.or(parent.archive_only),
    }
}
