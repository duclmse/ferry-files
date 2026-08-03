use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::store::{FileRecord, FileStore, tantivy_store::TantivyStore};

use super::postfilter::{apply_post_filters, fuzzy_score};
use super::types::{EvalStrategy, QueryPlan, QueryResult};

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
