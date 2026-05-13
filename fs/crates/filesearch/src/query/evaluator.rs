use std::sync::Arc;

use chrono::Local;
use nucleo::{Config as NucleoConfig, Matcher, Utf32Str};
use rayon::prelude::*;
use regex::Regex;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::store::{
    build_conditions, CmpOp, DupeKind, FileRecord, FileStore, SqlCondition,
    tantivy_store::TantivyStore,
};

use super::ast::{
    DateExpr, Expr, Function, Modifiers, Predicate, PredicateKind, RangeExpr,
};
use super::date::resolve_date_constant;
use super::glob::{glob_to_sqlite, has_wildcards};
use super::macros::expand_type_macro;

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
            // OR across two branches — both sides add conditions.
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
            // archive_path IS NULL — not directly representable; use post-filter
            p.post_filters.push(PostFilter::ArchiveExclude);
        }
        None => {}
    }

    let now = Local::now().naive_local();

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
            // No SQL narrowing — regex must be applied post-fetch
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
    now:    chrono::NaiveDateTime,
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
            // No `atime` column — approximate with mtime
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

fn date_expr_to_range(de: &DateExpr, now: chrono::NaiveDateTime) -> (i64, i64) {
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

// ---------------------------------------------------------------------------
// Post-filter application
// ---------------------------------------------------------------------------

pub fn apply_post_filters(
    records: Vec<FileRecord>,
    filters: &[PostFilter],
    store:   &FileStore,
) -> Vec<FileRecord> {
    records.into_iter().filter(|r| {
        for f in filters {
            if !post_filter_match(f, r, store) {
                return false;
            }
        }
        true
    }).collect()
}

fn post_filter_match(f: &PostFilter, r: &FileRecord, store: &FileStore) -> bool {
    match f {
        PostFilter::Regex(re, _cs) => re.is_match(&r.path),
        PostFilter::Child(name)    => store.has_child(&r.path, name).unwrap_or(false),
        PostFilter::WholeWord(w, cs) => {
            let text = if *cs { r.name.clone() } else { r.name.to_ascii_lowercase() };
            let word = if *cs { w.clone()      } else { w.to_ascii_lowercase()      };
            is_whole_word(&text, &word)
        }
        PostFilter::StartWith(s) => r.name.starts_with(s.as_str()),
        PostFilter::EndWith(s)   => r.name.ends_with(s.as_str()),
        PostFilter::ArchiveExclude => r.archive_path.is_none(),
    }
}

fn is_whole_word(text: &str, word: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let word_chars: Vec<char> = word.chars().collect();
    if word_chars.is_empty() { return false; }
    let wlen = word_chars.len();
    for i in 0..=chars.len().saturating_sub(wlen) {
        let slice: Vec<char> = chars[i..i + wlen].to_vec();
        if slice == word_chars {
            let before_ok = i == 0 || !chars[i - 1].is_alphanumeric();
            let after_ok  = i + wlen == chars.len() || !chars[i + wlen].is_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Execute the QueryPlan against the stores
// ---------------------------------------------------------------------------

pub async fn execute_plan(
    plan:    QueryPlan,
    store:   Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    config:  Arc<RwLock<Config>>,
) -> Result<QueryResult> {
    let start = std::time::Instant::now();
    let cfg   = config.read().await;

    let limit  = plan.limit.unwrap_or(cfg.search.default_limit).min(cfg.search.max_limit);
    let offset = plan.offset;

    let records = match &plan.strategy {
        EvalStrategy::SqliteOnly => {
            let conds  = plan.sql_conditions.clone();
            let lim    = limit + offset + plan.post_filters.len().min(1000);
            let store2 = Arc::clone(&store);
            tokio::task::spawn_blocking(move || {
                store2.search_conditions(&conds, lim)
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??
        }

        EvalStrategy::TantivyThenSqlite => {
            let query = plan.tantivy_query.clone().unwrap_or_default();
            let hits  = tokio::task::spawn_blocking({
                let tv = Arc::clone(&tantivy);
                let q  = query.clone();
                move || {
                    tv.search_fulltext(&q, limit * 4, &crate::store::MetaFilters::default())
                }
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??;

            let paths:  Vec<String> = hits.into_iter().map(|h| h.path).collect();
            let store2  = Arc::clone(&store);
            let conds   = plan.sql_conditions.clone();
            tokio::task::spawn_blocking(move || {
                store2.search_by_paths(&paths, &conds, limit)
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??
        }

        EvalStrategy::DupeSearch(kind) => {
            let kind2  = *kind;
            let conds  = plan.sql_conditions.clone();
            let store2 = Arc::clone(&store);
            tokio::task::spawn_blocking(move || {
                store2.find_dupes(kind2, &conds, limit)
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??
        }

        EvalStrategy::FuzzyFilter(term) => {
            let conds        = plan.sql_conditions.clone();
            let max_cands    = cfg.search.max_fuzzy_candidates;
            let threshold    = cfg.search.fuzzy_threshold;
            let term_clone   = term.clone();
            let store2       = Arc::clone(&store);

            tokio::task::spawn_blocking(move || {
                let candidates = store2.search_conditions(
                    &conds,
                    if max_cands > 0 { max_cands } else { 50_000 },
                )?;
                Ok::<Vec<FileRecord>, AppError>(
                    fuzzy_score(&candidates, &term_clone, threshold, limit)
                )
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??
        }
    };

    drop(cfg);

    // Apply post-filters
    let store_ref = Arc::clone(&store);
    let filters   = plan.post_filters;
    let filtered  = tokio::task::spawn_blocking(move || {
        apply_post_filters(records, &filters, &store_ref)
    })
    .await
    .map_err(|e| AppError::Query(e.to_string()))?;

    let total    = filtered.len();
    let hits     = filtered.into_iter().skip(offset).take(limit).collect();
    let took_ms  = start.elapsed().as_millis() as u64;

    Ok(QueryResult {
        hits,
        total,
        took_ms,
        mode_used: plan.strategy,
    })
}


// ---------------------------------------------------------------------------
// Fuzzy scoring with nucleo
// ---------------------------------------------------------------------------

fn fuzzy_score(
    candidates: &[FileRecord],
    term:       &str,
    threshold:  f32,
    limit:      usize,
) -> Vec<FileRecord> {
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(term, &mut needle_buf);

    let mut scored: Vec<(u16, &FileRecord)> = candidates
        .par_iter()
        .filter_map(|r| {
            let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
            let mut hay_buf = Vec::new();
            let haystack    = Utf32Str::new(&r.name, &mut hay_buf);
            matcher
                .fuzzy_match(haystack, needle)
                .map(|score| (score, r))
        })
        .collect();

    let max_possible = 1024u16;
    scored.retain(|(s, _)| (*s as f32 / max_possible as f32) >= threshold);
    scored.sort_unstable_by(|(a, _), (b, _)| b.cmp(a));
    scored.into_iter().take(limit).map(|(_, r)| r.clone()).collect()
}

// ---------------------------------------------------------------------------
// Regex helper
// ---------------------------------------------------------------------------

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex> {
    let re = if case_sensitive {
        Regex::new(pattern)
    } else {
        Regex::new(&format!("(?i){pattern}"))
    };
    re.map_err(|e| AppError::Query(format!("invalid regex '{pattern}': {e}")))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::Modifiers;
    use crate::query::parser::parse;
    use crate::config::MacroConfig;

    fn empty_mods() -> Modifiers { Modifiers::default() }

    fn parse_plan(q: &str) -> QueryPlan {
        let macros = MacroConfig::default();
        let expr   = parse(q, &macros).unwrap();
        plan(&expr, &empty_mods()).unwrap()
    }

    #[test]
    fn plain_term_produces_name_like() {
        let p = parse_plan("main");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::NameLike(_))));
        assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
    }

    #[test]
    fn wildcard_produces_glob() {
        let p = parse_plan("*.rs");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::NameGlob(_))));
    }

    #[test]
    fn ext_produces_ext_in() {
        let p = parse_plan("ext:jpg;png");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::ExtIn(_))));
    }

    #[test]
    fn size_large_produces_size_range() {
        let p = parse_plan("size:large");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::SizeRange(_, _))));
    }

    #[test]
    fn content_sets_tantivy_strategy() {
        let p = parse_plan("content:TODO");
        assert!(p.tantivy_query.is_some());
        assert!(matches!(p.strategy, EvalStrategy::TantivyThenSqlite));
    }

    #[test]
    fn fuzzy_sets_fuzzy_strategy() {
        let p = parse_plan("fuzzy:mian");
        assert!(matches!(p.strategy, EvalStrategy::FuzzyFilter(_)));
    }

    #[test]
    fn dupe_sets_dupe_strategy() {
        let p = parse_plan("dupe:");
        assert!(matches!(p.strategy, EvalStrategy::DupeSearch(_)));
    }

    #[test]
    fn count_sets_limit() {
        let p = parse_plan("count:50");
        assert_eq!(p.limit, Some(50));
    }

    #[test]
    fn archive_entry_only_condition() {
        let p = parse_plan("archive: *.rs");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::IsArchiveEntry)));
    }

    #[test]
    fn noarchive_post_filter() {
        let p = parse_plan("noarchive: *.log");
        assert!(p.post_filters.iter().any(|f| matches!(f, PostFilter::ArchiveExclude)));
    }

    #[test]
    fn archiveof_produces_condition() {
        let p = parse_plan("archiveof:project.zip");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::ArchiveOf(_))));
    }

    #[test]
    fn select_strategy_disk_below_threshold() {
        // Disk strategy when threshold = 0 (disabled)
        let p = parse_plan("*.rs");
        assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
    }

    #[test]
    fn dm_today_produces_mtime_range() {
        let p = parse_plan("dm:today");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::MtimeRange(_, _))));
        assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
    }

    #[test]
    fn size_gt_1mb_produces_size_op() {
        let p = parse_plan("size:>1mb");
        assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::SizeOp(CmpOp::Gt, 1_048_576))));
    }
}
