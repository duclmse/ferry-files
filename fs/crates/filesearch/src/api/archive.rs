//! Archive Service API - REST handlers and ArchiveRegistry.
//!
//! Routes (mounted only when archive_service.enabled = true):
//!   GET  /archive - list all registered archives
//!   GET  /archive/:id/index - raw .zidx binary
//!   GET  /archive/:id/dir?node=`<u32>` - directory listing
//!   GET  /archive/:id/file?node=`<u32>` - stream file content; `Range:` → 206
//!        for stored entries and deflate entries with a `.dzx` sidecar
//!   GET  /archive/:id/search?q=&limit=&offset= - archive-scoped search

use std::net::{IpAddr, SocketAddr};

use axum::{
    body::Body,
    extract::{ConnectInfo, Path as AxumPath, Query, State},
    http::{header, Request, Response, StatusCode},
    middleware::Next,
    response::IntoResponse,
    Json, Router,
    routing::get,
};
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use crate::api::rest::AppState;
use crate::query::{evaluator, parser};
use crate::query::ast::{Expr, Function, Modifiers, Predicate, PredicateKind};
use crate::archive::StreamExtractor;

mod registry;
mod handlers;
#[cfg(test)]
mod tests;

pub use registry::{ArchiveRegistry, ArchiveSummary, archive_id};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/archive",            get(handlers::list_archives))
        .route("/archive/:id/index",  get(handlers::get_index))
        .route("/archive/:id/dir",    get(handlers::list_dir))
        .route("/archive/:id/file",   get(handlers::stream_file))
        .route("/archive/:id/search", get(handlers::search_archive))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Rate-limit middleware (Step 2)
// ---------------------------------------------------------------------------

async fn rate_limit_middleware(
    State(state): State<AppState>,
    req:          Request<Body>,
    next:         Next,
) -> axum::response::Response {
    let enabled = {
        let cfg = state.config.read().await;
        cfg.archive_service.scheduler.rate_limit_enabled
    };
    if !enabled {
        return next.run(req).await;
    }

    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));

    if state.rate_limiter.check(ip, 1).is_err() {
        state.scheduler_metrics.inc_rejected();
        return (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded")
            .into_response();
    }

    next.run(req).await
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id:    Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node:  Option<u32>,
}

fn err_not_found(id: &str) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::NOT_FOUND, Json(ErrorBody {
        error: "archive not found".into(),
        id:    Some(id.to_string()),
        node:  None,
    }))
}

fn err_bad_node(msg: impl Into<String>, node: u32) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::BAD_REQUEST, Json(ErrorBody {
        error: msg.into(),
        id:    None,
        node:  Some(node),
    }))
}

fn err_internal(msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody {
        error: msg.into(),
        id:    None,
        node:  None,
    }))
}
