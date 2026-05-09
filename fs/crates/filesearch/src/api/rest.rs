//! REST API server — Axum 0.7 + tower-http.
//!
//! Routes:
//!   GET  /search?q=&limit=&offset=
//!   GET  /stats
//!   POST /reindex  { "full": bool }
//!   GET  /health
//!   GET  /file?path=
//!   POST /open     { "path": "..." }

use std::sync::{atomic::Ordering, Arc};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, RwLock};
use tower_http::cors::CorsLayer;

use crate::config::Config;
use crate::indexer::crawler::IndexMetrics;
use crate::query::{evaluator, parser};
use crate::store::{sqlite_store::SqliteStore, tantivy_store::TantivyStore, FileRecord};

use super::dto::archive_entry_opt;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub sqlite:     Arc<SqliteStore>,
    pub tantivy:    Arc<TantivyStore>,
    pub config:     Arc<RwLock<Config>>,
    pub metrics:    Arc<IndexMetrics>,
    pub reindex_tx: watch::Sender<bool>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SearchHit {
    pub path:          String,
    pub name:          String,
    pub size_bytes:    u64,
    pub mtime:         i64,
    #[serde(rename = "type")]
    pub kind:          &'static str,
    pub score:         f32,
    pub snippet:       Option<String>,
    pub archive_path:  Option<String>,
    pub archive_entry: Option<String>,
}

#[derive(Serialize)]
pub struct SearchApiResponse {
    pub result:  Vec<SearchHit>,
    pub total:   usize,
    pub took_ms: u64,
    pub mode:    &'static str,
}

#[derive(Serialize)]
pub struct StatsApiResponse {
    pub total_files:             u64,
    pub total_dirs:              u64,
    pub content_indexed:         u64,
    pub archive_entries_indexed: u64,
    pub archives_crawled:        u64,
    pub events_processed:        u64,
    pub last_crawl_secs:         u64,
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
    Router::new()
        .route("/search",  get(search_handler))
        .route("/stats",   get(stats_handler))
        .route("/reindex", post(reindex_handler))
        .route("/health",  get(health_handler))
        .route("/file",    get(file_handler))
        .route("/open",    post(open_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SearchParams {
    q:      Option<String>,
    limit:  Option<usize>,
    offset: Option<usize>,
}

async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let q = match params.q {
        Some(q) if !q.is_empty() => q,
        _ => return api_error(StatusCode::BAD_REQUEST, "missing query parameter 'q'")
            .into_response(),
    };

    // Single config lock — reads both macros and search limits in one pass.
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
    plan.limit  = Some(params.limit.unwrap_or(default_limit));
    plan.offset = params.offset.unwrap_or(0);

    let mode = eval_strategy_mode(&plan.strategy);

    match evaluator::execute_plan(plan, state.sqlite, state.tantivy, state.config).await {
        Ok(result) => {
            let hits = result.hits.iter().map(file_record_to_hit).collect();
            Json(SearchApiResponse {
                result:  hits,
                total:   result.total,
                took_ms: result.took_ms,
                mode,
            })
            .into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    // All fields read from in-process atomics — no DB round-trip.
    let m = &state.metrics;
    Json(StatsApiResponse {
        total_files:             m.total_files.load(Ordering::Relaxed),
        total_dirs:              m.total_dirs.load(Ordering::Relaxed),
        content_indexed:         m.content_indexed.load(Ordering::Relaxed),
        archive_entries_indexed: m.archive_entries_indexed.load(Ordering::Relaxed),
        archives_crawled:        m.archives_crawled.load(Ordering::Relaxed),
        events_processed:        m.events_processed.load(Ordering::Relaxed),
        last_crawl_secs:         m.last_crawl_secs.load(Ordering::Relaxed),
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

async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
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
        Ok(None)    => api_error(StatusCode::NOT_FOUND, "path not in index").into_response(),
        Err(e)      => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct OpenBody {
    path: String,
}

#[derive(Serialize)]
pub struct OpenResponse {
    opened:    bool,
    temp_path: Option<String>,
}

async fn open_handler(
    State(state): State<AppState>,
    Json(body): Json<OpenBody>,
) -> impl IntoResponse {
    // Verify path exists in index.
    match state.sqlite.get_by_path(&body.path) {
        Ok(None)    => return api_error(StatusCode::BAD_REQUEST, "path not in index").into_response(),
        Err(e)      => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Ok(Some(_)) => {}
    }

    if body.path.contains("!/") {
        match extract_and_open(&body.path).await {
            Ok(temp_path) => Json(OpenResponse { opened: true, temp_path: Some(temp_path) })
                .into_response(),
            Err(e) => api_error(StatusCode::BAD_REQUEST, e).into_response(),
        }
    } else {
        match open_with_os(&body.path).await {
            Ok(()) => Json(OpenResponse { opened: true, temp_path: None }).into_response(),
            Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// Open helpers
// ---------------------------------------------------------------------------

pub(crate) async fn open_with_os(path: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let (prog, args): (&str, &[&str]) = ("open", &[path]);
    #[cfg(target_os = "linux")]
    let (prog, args): (&str, &[&str]) = ("xdg-open", &[path]);
    #[cfg(target_os = "windows")]
    let (prog, args): (&str, &[&str]) = ("cmd", &["/C", "start", "", path]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return Err("open not supported on this platform".into());

    #[allow(unreachable_code)]
    tokio::process::Command::new(prog)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(crate) async fn extract_and_open(virtual_path: &str) -> Result<String, String> {
    let sep = virtual_path
        .find("!/")
        .ok_or_else(|| "invalid virtual path".to_owned())?;
    let archive_path = virtual_path[..sep].to_owned();
    let entry_name   = virtual_path[sep + 2..].to_owned();

    let temp_path = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let file  = std::fs::File::open(&archive_path).map_err(|e| e.to_string())?;
        let mut zip   = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        let mut entry = zip.by_name(&entry_name)
            .map_err(|_| "entry not found in archive".to_owned())?;

        let basename = std::path::Path::new(&entry_name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("extracted_file");

        let mut tmp = tempfile::Builder::new()
            .prefix("filesearch_")
            .suffix(&format!("_{basename}"))
            .tempfile()
            .map_err(|e| e.to_string())?;

        std::io::copy(&mut entry, &mut tmp).map_err(|e| e.to_string())?;
        let (_, path) = tmp.keep().map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())??;

    let cleanup_path = temp_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let _ = tokio::fs::remove_file(&cleanup_path).await;
    });

    open_with_os(&temp_path).await?;
    Ok(temp_path)
}

// ---------------------------------------------------------------------------
// Mapping helpers
// ---------------------------------------------------------------------------

fn file_record_to_hit(r: &FileRecord) -> SearchHit {
    SearchHit {
        path:          r.path.clone(),
        name:          r.name.clone(),
        size_bytes:    r.size_bytes,
        mtime:         r.mtime,
        kind:          if r.is_dir { "dir" } else { "file" },
        score:         0.0,
        snippet:       None,
        archive_path:  r.archive_path.clone(),
        archive_entry: archive_entry_opt(r),
    }
}

fn eval_strategy_mode(strategy: &evaluator::EvalStrategy) -> &'static str {
    match strategy {
        evaluator::EvalStrategy::SqliteOnly        => "exact",
        evaluator::EvalStrategy::TantivyThenSqlite => "content",
        evaluator::EvalStrategy::DupeSearch(_)     => "dupes",
        evaluator::EvalStrategy::FuzzyFilter(_)    => "fuzzy",
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::evaluator::EvalStrategy;
    use crate::store::{DupeKind, FileRecord};

    fn make_record(path: &str, is_dir: bool, archive_path: Option<&str>) -> FileRecord {
        FileRecord {
            id:              None,
            path:            path.into(),
            name:            path.rsplit('/').next().unwrap_or(path).into(),
            extension:       Some("rs".into()),
            size_bytes:      4096,
            mtime:           1_700_000_000,
            ctime:           1_700_000_000,
            is_dir,
            is_hidden:       false,
            is_symlink:      false,
            attrib_bitmask:  0,
            content_indexed: false,
            tantivy_id:      None,
            indexed_at:      0,
            archive_path:    archive_path.map(str::to_string),
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
        let r = make_record("/data/archive.zip!/src/main.rs", false, Some("/data/archive.zip"));
        let hit = file_record_to_hit(&r);
        assert_eq!(hit.archive_path, Some("/data/archive.zip".into()));
        assert_eq!(hit.archive_entry, Some("src/main.rs".into()));
    }

    #[test]
    fn eval_strategy_mode_all_variants() {
        assert_eq!(eval_strategy_mode(&EvalStrategy::SqliteOnly),        "exact");
        assert_eq!(eval_strategy_mode(&EvalStrategy::TantivyThenSqlite), "content");
        assert_eq!(eval_strategy_mode(&EvalStrategy::DupeSearch(DupeKind::Name)), "dupes");
        assert_eq!(eval_strategy_mode(&EvalStrategy::FuzzyFilter("x".into())),   "fuzzy");
    }

    #[tokio::test]
    async fn extract_and_open_no_separator_returns_err() {
        let result = extract_and_open("no-separator-here").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid virtual path"));
    }
}
