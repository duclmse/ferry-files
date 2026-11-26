use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::store::{tantivy_store::TantivyStore, FileRecord, FileStore, Sort, SqlCondition};

use super::postfilter::{apply_post_filters, fuzzy_rank, FuzzyPrefilter};
use super::types::{EvalStrategy, PostFilter, QueryPlan, QueryResult};

// ---------------------------------------------------------------------------
// Execute the QueryPlan against the stores
// ---------------------------------------------------------------------------

pub async fn execute_plan(
    plan: QueryPlan,
    store: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    config: Arc<RwLock<Config>>,
) -> Result<QueryResult> {
    let start = std::time::Instant::now();
    let cfg = config.read().await;

    let limit = plan
        .limit
        .unwrap_or(cfg.search.default_limit)
        .min(cfg.search.max_limit);
    let offset = plan.offset;
    let sort = plan.sort;

    let mut post_filters = plan.post_filters;
    let records = match &plan.strategy {
        EvalStrategy::SqliteOnly => {
            let conds = plan.sql_conditions.clone();
            let want = limit + offset;
            let cap = cfg.search.max_filter_candidates;
            let filters = std::mem::take(&mut post_filters);
            let store2 = Arc::clone(&store);
            tokio::task::spawn_blocking(move || {
                fetch_filtered(&store2, &conds, sort, &filters, want, cap)
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??
        }

        EvalStrategy::TantivyThenSqlite => {
            let query = plan.tantivy_query.clone().unwrap_or_default();
            // The page is cut from these below, so they run to its end.
            let want = limit + offset;
            let hits = tokio::task::spawn_blocking({
                let tv = Arc::clone(&tantivy);
                let q = query.clone();
                move || tv.search_fulltext(&q, want * 4, &crate::store::MetaFilters::default())
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??;

            let paths: Vec<String> = hits.into_iter().map(|h| h.path).collect();
            let store2 = Arc::clone(&store);
            let conds = plan.sql_conditions.clone();
            tokio::task::spawn_blocking(move || store2.search_by_paths(&paths, &conds, want))
                .await
                .map_err(|e| AppError::Query(e.to_string()))??
        }

        EvalStrategy::DupeSearch(kind) => {
            let kind2 = *kind;
            let conds = plan.sql_conditions.clone();
            let store2 = Arc::clone(&store);
            // The page is cut from these below, so they run to its end.
            let want = limit + offset;
            tokio::task::spawn_blocking(move || store2.find_dupes(kind2, &conds, want))
                .await
                .map_err(|e| AppError::Query(e.to_string()))??
        }

        EvalStrategy::FuzzyFilter(term) => {
            let conds = plan.sql_conditions.clone();
            let max_cands = cfg.search.max_fuzzy_candidates;
            let threshold = cfg.search.fuzzy_threshold;
            let term_clone = term.clone();
            let store2 = Arc::clone(&store);
            let prefilter = FuzzyPrefilter::new(term, threshold);

            tokio::task::spawn_blocking(move || {
                store2.fuzzy_search(
                    &conds,
                    if max_cands > 0 { max_cands } else { 50_000 },
                    &|name: &str| prefilter.could_match(name),
                    &|names: &[&str]| fuzzy_rank(names, &term_clone, threshold, limit),
                )
            })
            .await
            .map_err(|e| AppError::Query(e.to_string()))??
        }
    };

    drop(cfg);

    // Apply post-filters
    let store_ref = Arc::clone(&store);
    let filters = post_filters;
    let filtered =
        tokio::task::spawn_blocking(move || apply_post_filters(records, &filters, &store_ref))
            .await
            .map_err(|e| AppError::Query(e.to_string()))?;

    // The other strategies pick their rows by relevance or grouping, so only
    // the rows they chose can be ordered.
    let mut filtered = filtered;
    if let (Some(sort), false) = (sort, matches!(plan.strategy, EvalStrategy::SqliteOnly)) {
        filtered.sort_by(|a, b| sort.compare(a, b));
    }

    let total = filtered.len();
    let hits = filtered.into_iter().skip(offset).take(limit).collect();
    let took_ms = start.elapsed().as_millis() as u64;

    Ok(QueryResult {
        hits,
        total,
        took_ms,
        mode_used: plan.strategy,
    })
}

/// Fetches rows and applies `filters`, growing the fetch until `want` rows
/// survive, the store runs out, or `cap` rows (0 = no cap) have been read.
///
/// Filters that run in Rust can reject most fetched rows (`!regex:x` on a
/// large index), so fetching exactly `want` would return short pages.
pub(super) fn fetch_filtered(
    store: &FileStore,
    conds: &[SqlCondition],
    sort: Option<Sort>,
    filters: &[PostFilter],
    want: usize,
    cap: usize,
) -> Result<Vec<FileRecord>> {
    let fetch_rows = |n: usize| match sort {
        Some(sort) => store.search_sorted(conds, sort, n),
        None => store.search_conditions(conds, n),
    };
    if filters.is_empty() {
        return fetch_rows(want);
    }
    let cap = if cap == 0 { usize::MAX } else { cap.max(want) };
    let mut fetch = want.max(FIRST_FILTERED_FETCH).min(cap);
    loop {
        let rows = fetch_rows(fetch)?;
        let exhausted = rows.len() < fetch;
        let kept = apply_post_filters(rows, filters, store);
        if kept.len() >= want || exhausted || fetch >= cap {
            return Ok(kept);
        }
        // Each round refetches from the start; growing 4x keeps the total
        // work within ~1.33x of the final fetch.
        fetch = fetch.saturating_mul(4).min(cap);
    }
}

const FIRST_FILTERED_FETCH: usize = 256;
