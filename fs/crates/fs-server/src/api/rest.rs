//! REST API server - Axum 0.7 + tower-http.
//!
//! Routes:
//!   GET  /search?q=&limit=&offset=
//!   GET  /stats
//!   POST /reindex  { "full": bool }
//!   GET  /health
//!   GET  /file?path=
//!   POST /open     { "path": "..." }
//!   GET    /watch_paths
//!   POST   /watch_paths         { "path": "..." }  (token)
//!   DELETE /watch_paths?path=[&forget=true]       (token)

use std::sync::{atomic::Ordering, Arc};

use axum::http::HeaderValue;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    middleware::from_fn_with_state,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, RwLock};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

use crate::api::archive::ArchiveRegistry;
use crate::config::Config;
use crate::indexer::crawler::IndexMetrics;
use crate::query::{evaluator, parser};
use crate::scheduler::{ArchiveSemaphores, ClientRateLimiter, SchedulerMetrics};
use crate::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

use super::dto::archive_entry_opt;
use super::open::{extract_and_open, open_with_os};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub sqlite: Arc<FileStore>,
    pub tantivy: Arc<TantivyStore>,
    pub config: Arc<RwLock<Config>>,
    pub metrics: Arc<IndexMetrics>,
    pub reindex_tx: watch::Sender<bool>,
    pub archive_registry: Arc<ArchiveRegistry>,
    pub archive_semaphores: Arc<ArchiveSemaphores>,
    pub scheduler_metrics: Arc<SchedulerMetrics>,
    pub rate_limiter: Arc<ClientRateLimiter>,
    pub watch_list: Arc<crate::watch_list::WatchList>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SearchHit {
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub mtime: i64,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub score: f32,
    pub snippet: Option<String>,
    pub archive_path: Option<String>,
    pub archive_entry: Option<String>,
}

#[derive(Serialize)]
pub struct SearchApiResponse {
    pub result: Vec<SearchHit>,
    pub total: usize,
    pub took_ms: u64,
    pub mode: &'static str,
}

#[derive(Serialize)]
pub struct StatsApiResponse {
    pub total_files: u64,
    pub total_dirs: u64,
    pub content_indexed: u64,
    pub archive_entries_indexed: u64,
    pub archives_crawled: u64,
    pub events_processed: u64,
    pub last_crawl_secs: u64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ReindexAccepted {
    accepted: bool,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

fn api_error(status: StatusCode, msg: impl Into<String>) -> impl IntoResponse {
    (status, Json(ErrorResponse { error: msg.into() }))
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    let mut r = Router::new()
        .route("/search", get(search_handler))
        .route("/stats", get(stats_handler))
        .route("/reindex", post(reindex_handler))
        .route("/metrics", get(metrics_handler))
        .route("/file", get(file_handler))
        .route("/open", post(open_handler))
        .route("/watch_paths", get(watch_paths_handler))
        .merge(watch_changes(state.clone()))
        .merge(crate::api::fs::router(state.clone()));

    if state.archive_registry.is_enabled() {
        r = r.merge(crate::api::archive::router(state.clone()));
    }

    // After the layer, so health probes need no token.
    r.route_layer(from_fn_with_state(
        state.clone(),
        crate::api::auth::require_token_in_all_mode,
    ))
    .route("/health", get(health_handler))
    .with_state(state)
}

/// Builds the CORS policy from `api.rest_cors_origins`. An empty list adds no
/// CORS headers, so browsers keep cross-origin pages from reading responses.
pub fn cors_layer(origins: &[String]) -> CorsLayer {
    if origins.iter().any(|o| o == "*") {
        return CorsLayer::permissive();
    }
    if origins.is_empty() {
        return CorsLayer::new();
    }
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| match HeaderValue::from_str(origin) {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::warn!("ignoring invalid CORS origin {origin:?}: {e}");
                None
            }
        })
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed))
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SearchParams {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let q = match params.q {
        Some(q) if !q.is_empty() => q,
        _ => {
            return api_error(StatusCode::BAD_REQUEST, "missing query parameter 'q'")
                .into_response()
        }
    };

    // Single config lock - reads both macros and search limits in one pass.
    let (macros, default_limit) = {
        let cfg = state.config.read().await;
        (cfg.macros.clone(), cfg.search.default_limit)
    };

    let expr = match parser::parse(&q, &macros) {
        Ok(e) => e,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let mut plan = match evaluator::plan(&expr, &Default::default()) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    plan.limit = Some(params.limit.unwrap_or(default_limit));
    plan.offset = params.offset.unwrap_or(0);

    let mode = eval_strategy_mode(&plan.strategy);

    match evaluator::execute_plan(plan, state.sqlite, state.tantivy, state.config).await {
        Ok(result) => {
            let hits = result.hits.iter().map(file_record_to_hit).collect();
            Json(SearchApiResponse {
                result: hits,
                total: result.total,
                took_ms: result.took_ms,
                mode,
            })
            .into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    // All fields read from in-process atomics - no DB round-trip.
    let m = &state.metrics;
    Json(StatsApiResponse {
        total_files: m.total_files.load(Ordering::Relaxed),
        total_dirs: m.total_dirs.load(Ordering::Relaxed),
        content_indexed: m.content_indexed.load(Ordering::Relaxed),
        archive_entries_indexed: m.archive_entries_indexed.load(Ordering::Relaxed),
        archives_crawled: m.archives_crawled.load(Ordering::Relaxed),
        events_processed: m.events_processed.load(Ordering::Relaxed),
        last_crawl_secs: m.last_crawl_secs.load(Ordering::Relaxed),
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct ReindexBody {
    #[serde(default)]
    full: bool,
}

async fn reindex_handler(
    State(state): State<AppState>,
    Json(body): Json<ReindexBody>,
) -> impl IntoResponse {
    let _ = state.reindex_tx.send(body.full);
    Json(ReindexAccepted { accepted: true })
}

/// Changing the watch paths needs the token whatever `api.auth_mode` is:
/// the daemon would index, and show the names of, whatever its account
/// can read.
fn watch_changes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/watch_paths",
            post(watch_add_handler).delete(watch_remove_handler),
        )
        .route_layer(from_fn_with_state(state, crate::api::auth::require_token))
}

#[derive(Serialize)]
struct WatchPathsResponse {
    watch_paths: Vec<String>,
    /// False until a full crawl has finished once.
    crawled: bool,
    /// For a change: whether it changed anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<bool>,
    /// For an add: the watch paths inside it, which it replaced.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    absorbed: Vec<String>,
    /// Each watch path's crawl, in `watch_paths` order.
    roots: Vec<RootJson>,
}

#[derive(Serialize)]
struct RootJson {
    path: String,
    /// "pending", "crawling", "ready" or "offline".
    state: &'static str,
    /// Entries its last or running crawl wrote.
    entries: u64,
    /// Unix seconds its last crawl started and finished here; 0 if none did.
    started: u64,
    finished: u64,
}

#[derive(Deserialize)]
struct WatchPathBody {
    path: String,
}

#[derive(Deserialize)]
struct WatchRemoveParams {
    path: String,
    /// Remove the path's entries from the index too.
    #[serde(default)]
    forget: bool,
}

async fn watch_paths_handler(State(state): State<AppState>) -> impl IntoResponse {
    let paths = state.config.read().await.watch_paths.clone();
    Json(watch_paths_response(&state, &paths, None))
}

fn watch_paths_response(
    state: &AppState,
    paths: &[std::path::PathBuf],
    changed: Option<bool>,
) -> WatchPathsResponse {
    let roots: Vec<RootJson> = state
        .metrics
        .roots(paths)
        .into_iter()
        .map(|r| RootJson {
            state: r.state.label(),
            entries: r.entries,
            started: r.started,
            finished: r.finished,
            path: r.path,
        })
        .collect();
    WatchPathsResponse {
        watch_paths: roots.iter().map(|r| r.path.clone()).collect(),
        crawled: state.metrics.last_crawl_secs.load(Ordering::Relaxed) != 0,
        changed,
        absorbed: Vec::new(),
        roots,
    }
}

async fn watch_add_handler(
    State(state): State<AppState>,
    Json(body): Json<WatchPathBody>,
) -> axum::response::Response {
    let changed = state.watch_list.add(&body.path).await;
    watch_change_response(&state, changed)
}

async fn watch_remove_handler(
    State(state): State<AppState>,
    Query(params): Query<WatchRemoveParams>,
) -> axum::response::Response {
    let changed = state.watch_list.remove(&params.path, params.forget).await;
    watch_change_response(&state, changed)
}

fn watch_change_response(
    state: &AppState,
    changed: Result<crate::watch_list::Outcome, crate::watch_list::WatchListError>,
) -> axum::response::Response {
    use crate::watch_list::WatchListError;
    match changed {
        Ok(o) => {
            let mut body = watch_paths_response(state, &o.watch_paths, Some(o.changed));
            body.absorbed = o
                .absorbed
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            Json(body).into_response()
        }
        Err(e) => {
            let status = match e {
                WatchListError::Invalid(_) => StatusCode::BAD_REQUEST,
                WatchListError::NotFound(_) => StatusCode::NOT_FOUND,
                WatchListError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            api_error(status, e.to_string()).into_response()
        }
    }
}

async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    use axum::http::header::CONTENT_TYPE;
    let body = state.scheduler_metrics.prometheus_text();
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

#[derive(Deserialize)]
pub struct FileParams {
    path: Option<String>,
}

async fn file_handler(
    State(state): State<AppState>,
    Query(params): Query<FileParams>,
) -> impl IntoResponse {
    let path = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return api_error(StatusCode::BAD_REQUEST, "missing 'path' parameter").into_response(),
    };

    match state.sqlite.get_by_path(&path) {
        Ok(Some(r)) => Json(file_record_to_hit(&r)).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "path not in index").into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct OpenBody {
    path: String,
}

#[derive(Serialize)]
pub struct OpenResponse {
    opened: bool,
    temp_path: Option<String>,
}

async fn open_handler(
    State(state): State<AppState>,
    Json(body): Json<OpenBody>,
) -> impl IntoResponse {
    // Verify path exists in index.
    match state.sqlite.get_by_path(&body.path) {
        Ok(None) => return api_error(StatusCode::BAD_REQUEST, "path not in index").into_response(),
        Err(e) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Ok(Some(_)) => {}
    }

    if body.path.contains("!/") {
        match extract_and_open(&body.path).await {
            Ok(temp_path) => Json(OpenResponse {
                opened: true,
                temp_path: Some(temp_path),
            })
            .into_response(),
            Err(e) => api_error(StatusCode::BAD_REQUEST, e).into_response(),
        }
    } else {
        match open_with_os(&body.path).await {
            Ok(()) => Json(OpenResponse {
                opened: true,
                temp_path: None,
            })
            .into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// Mapping helpers
// ---------------------------------------------------------------------------

fn file_record_to_hit(r: &FileRecord) -> SearchHit {
    SearchHit {
        path: r.path.clone(),
        name: r.name.clone(),
        size_bytes: r.size_bytes,
        mtime: r.mtime,
        kind: if r.is_dir { "dir" } else { "file" },
        score: 0.0,
        snippet: None,
        archive_path: r.archive_path.clone(),
        archive_entry: archive_entry_opt(r),
    }
}

fn eval_strategy_mode(strategy: &evaluator::EvalStrategy) -> &'static str {
    match strategy {
        evaluator::EvalStrategy::SqliteOnly => "exact",
        evaluator::EvalStrategy::TantivyThenSqlite => "content",
        evaluator::EvalStrategy::DupeSearch(_) => "dupes",
        evaluator::EvalStrategy::FuzzyFilter(_) => "fuzzy",
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn cors_follows_the_configured_origins() {
        use axum::{body::Body, http::Request, routing::get, Router};
        use tower::ServiceExt;

        async fn allowed(origins: &[&str], origin: &str) -> Option<String> {
            let origins: Vec<String> = origins.iter().map(|o| o.to_string()).collect();
            let app = Router::new()
                .route("/x", get(|| async { "ok" }))
                .layer(super::cors_layer(&origins));
            let request = Request::builder()
                .uri("/x")
                .header("origin", origin)
                .body(Body::empty())
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            response
                .headers()
                .get("access-control-allow-origin")
                .map(|v| v.to_str().unwrap().to_owned())
        }

        let site = "https://evil.example";
        let ui = "https://ui.example";
        assert_eq!(allowed(&[], site).await, None);
        assert_eq!(allowed(&[ui], site).await, None);
        assert_eq!(allowed(&[ui], ui).await.as_deref(), Some(ui));
        assert_eq!(allowed(&["*"], site).await.as_deref(), Some("*"));
    }

    use super::*;
    use crate::query::evaluator::EvalStrategy;
    use crate::store::{DupeKind, FileRecord};

    fn make_record(path: &str, is_dir: bool, archive_path: Option<&str>) -> FileRecord {
        FileRecord {
            id: None,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            extension: Some("rs".into()),
            size_bytes: 4096,
            mtime: 1_700_000_000,
            ctime: 1_700_000_000,
            is_dir,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: archive_path.map(str::to_string),
        }
    }

    #[test]
    fn file_record_to_hit_real_file() {
        let r = make_record("/home/user/main.rs", false, None);
        let hit = file_record_to_hit(&r);
        assert_eq!(hit.path, "/home/user/main.rs");
        assert_eq!(hit.name, "main.rs");
        assert_eq!(hit.kind, "file");
        assert_eq!(hit.score, 0.0);
        assert_eq!(hit.archive_path, None);
        assert_eq!(hit.archive_entry, None);
    }

    #[test]
    fn file_record_to_hit_directory() {
        let r = make_record("/home/user/src", true, None);
        let hit = file_record_to_hit(&r);
        assert_eq!(hit.kind, "dir");
    }

    #[test]
    fn file_record_to_hit_archive_entry() {
        let r = make_record(
            "/data/archive.zip!/src/main.rs",
            false,
            Some("/data/archive.zip"),
        );
        let hit = file_record_to_hit(&r);
        assert_eq!(hit.archive_path, Some("/data/archive.zip".into()));
        assert_eq!(hit.archive_entry, Some("src/main.rs".into()));
    }

    #[test]
    fn eval_strategy_mode_all_variants() {
        assert_eq!(eval_strategy_mode(&EvalStrategy::SqliteOnly), "exact");
        assert_eq!(
            eval_strategy_mode(&EvalStrategy::TantivyThenSqlite),
            "content"
        );
        assert_eq!(
            eval_strategy_mode(&EvalStrategy::DupeSearch(DupeKind::Name)),
            "dupes"
        );
        assert_eq!(
            eval_strategy_mode(&EvalStrategy::FuzzyFilter("x".into())),
            "fuzzy"
        );
    }
}
