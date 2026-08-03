//! Archive Service REST handlers — list/index/dir/file/search endpoints.

use super::*;

// ---------------------------------------------------------------------------
// Handler: GET /archive
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ArchiveListItem {
    id:   String,
    path: String,
}

pub(super) async fn list_archives(State(state): State<AppState>) -> impl IntoResponse {
    let items: Vec<ArchiveListItem> = state.archive_registry
        .list()
        .into_iter()
        .map(|s| ArchiveListItem {
            id:   s.id,
            path: s.zip_path.to_string_lossy().into_owned(),
        })
        .collect();
    Json(items)
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/index
// ---------------------------------------------------------------------------

pub(super) async fn get_index(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    req_headers:  axum::http::HeaderMap,
) -> impl IntoResponse {
    let (zidx_path, etag) = match state.archive_registry.entries.get(&id) {
        Some(e) => (e.zidx_path.clone(), e.etag.clone()),
        None    => return err_not_found(&id).into_response(),
    };

    // Conditional GET: skip re-reading the file when client has a fresh copy.
    if req_headers.get(header::IF_NONE_MATCH).map(|v| v.as_bytes()) == Some(etag.as_bytes()) {
        return axum::http::Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap()
            .into_response();
    }

    let file = match tokio::fs::File::open(&zidx_path).await {
        Ok(f)  => f,
        Err(_) => return err_not_found(&id).into_response(),
    };
    let len = match file.metadata().await {
        Ok(m)  => m.len(),
        Err(e) => return err_internal(e.to_string()).into_response(),
    };

    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE,   "application/octet-stream")
        .header(header::CACHE_CONTROL,  "private, max-age=3600")
        .header(header::CONTENT_LENGTH, len)
        .header(header::ETAG,           &etag)
        .body(Body::from_stream(ReaderStream::new(file)))
    {
        Ok(r)  => r.into_response(),
        Err(e) => err_internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/dir?node=<u32>
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct NodeParam {
    node: Option<u32>,
}

#[derive(Serialize)]
struct DirEntry {
    id:   u32,
    name: String,
    kind: &'static str,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    compressed_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compression:     Option<u16>,
}

pub(super) async fn list_dir(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<NodeParam>,
) -> impl IntoResponse {
    let node_id = params.node.unwrap_or(0);

    let idx = match state.archive_registry.index(&id) {
        Some(i) => i,
        None    => return err_not_found(&id).into_response(),
    };

    let node = match idx.node(node_id) {
        Ok(n)  => n,
        Err(_) => return err_bad_node("node out of range", node_id).into_response(),
    };

    if node.kind != 1 {
        return err_bad_node("node is not a directory", node_id).into_response();
    }

    let mut entries = Vec::new();
    for child_result in idx.children(node_id) {
        let child_id = match child_result {
            Ok(id) => id,
            Err(e) => return err_internal(e.to_string()).into_response(),
        };
        let child = match idx.node(child_id) {
            Ok(n)  => n,
            Err(e) => return err_internal(e.to_string()).into_response(),
        };
        let name = match idx.name(&child) {
            Ok(n)  => n.to_owned(),
            Err(e) => return err_internal(e.to_string()).into_response(),
        };
        let (kind, compressed_size, compression) = if child.kind == 1 {
            ("dir", None, None)
        } else {
            ("file", Some(child.compressed_size), Some(child.compression))
        };
        entries.push(DirEntry {
            id:   child_id,
            name,
            kind,
            size: child.uncompressed_size,
            compressed_size,
            compression,
        });
    }

    Json(entries).into_response()
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/file?node=<u32>
// ---------------------------------------------------------------------------

pub(super) async fn stream_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<NodeParam>,
) -> impl IntoResponse {
    let node_id = params.node.unwrap_or(0);

    let idx = match state.archive_registry.index(&id) {
        Some(i) => i,
        None    => return err_not_found(&id).into_response(),
    };

    let node = match idx.node(node_id) {
        Ok(n)  => n,
        Err(_) => return err_bad_node("node out of range", node_id).into_response(),
    };

    if node.kind != 0 {
        return err_bad_node("node is a directory", node_id).into_response();
    }

    let meta = crate::archive::EntryMeta {
        compression:         node.compression,
        compressed_size:     node.compressed_size,
        uncompressed_size:   node.uncompressed_size,
        local_header_offset: node.local_header_offset,
    };
    let source = match state.archive_registry.source(&id) {
        Some(s) => s,
        None    => return err_not_found(&id).into_response(),
    };

    // ── Per-archive concurrency limit (semaphore) ─────────────────────────
    // 30 s timeout: avoids a slow upstream holding permits forever. The permit
    // is moved into on_finish so it releases when the decode task actually
    // exits, not when this handler function returns (the response body is
    // streamed by a task spawned inside `stream()`, after this fn returns).
    let permit = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.archive_semaphores.acquire(&id),
    ).await {
        Ok(p)  => p,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "semaphore timeout").into_response(),
    };
    state.scheduler_metrics.inc_inflight();

    let extractor      = crate::archive::ZipStreamExtractor { source };
    let metrics        = state.scheduler_metrics.clone();
    let on_finish: Option<Box<dyn FnOnce() + Send>> = Some(Box::new(move || {
        drop(permit);
        metrics.dec_inflight();
    }));
    let stream    = match extractor.stream(&meta, on_finish) {
        Ok(s)  => s,
        Err(e) => return err_internal(e.to_string()).into_response(),
    };

    match axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, node.uncompressed_size)
        .body(axum::body::Body::from_stream(stream))
    {
        Ok(r)  => r.into_response(),
        Err(e) => err_internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Handler: GET /archive/:id/search?q=&limit=&offset=
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct SearchParams {
    q:      Option<String>,
    limit:  Option<usize>,
    offset: Option<usize>,
}

pub(super) async fn search_archive(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let zip_path = match state.archive_registry.zip_path(&id) {
        Some(p) => p,
        None    => return err_not_found(&id).into_response(),
    };

    let q = params.q.unwrap_or_default();

    let (macros, default_limit) = {
        let cfg = state.config.read().await;
        (cfg.macros.clone(), cfg.search.default_limit)
    };

    // Build the archiveof: predicate directly from the PathBuf so spaces and
    // special characters in the path never corrupt the query string.
    let zip_str = zip_path.to_string_lossy().replace('\\', "/");
    let archive_pred = Expr::Predicate(Predicate {
        kind:      PredicateKind::Function(Function::ArchiveOf(zip_str)),
        modifiers: Modifiers::default(),
    });

    let expr = if q.is_empty() {
        archive_pred
    } else {
        match parser::parse(&q, &macros) {
            Ok(user_expr) => Expr::And(Box::new(archive_pred), Box::new(user_expr)),
            Err(e)        => return (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody { error: e.to_string(), id: None, node: None }),
            ).into_response(),
        }
    };

    let mut plan = match evaluator::plan(&expr, &Default::default()) {
        Ok(p)  => p,
        Err(e) => return err_internal(e.to_string()).into_response(),
    };
    plan.limit  = Some(params.limit.unwrap_or(default_limit));
    plan.offset = params.offset.unwrap_or(0);

    match evaluator::execute_plan(plan, state.sqlite, state.tantivy, state.config).await {
        Ok(result) => {
            use crate::api::rest::SearchApiResponse;
            let hits = result.hits.iter().map(|r| crate::api::rest::SearchHit {
                path:          r.path.clone(),
                name:          r.name.clone(),
                size_bytes:    r.size_bytes,
                mtime:         r.mtime,
                kind:          if r.is_dir { "dir" } else { "file" },
                score:         0.0,
                snippet:       None,
                archive_path:  r.archive_path.clone(),
                archive_entry: crate::api::dto::archive_entry_opt(r),
            }).collect();
            Json(SearchApiResponse {
                result:  hits,
                total:   result.total,
                took_ms: result.took_ms,
                mode:    "exact",
            })
            .into_response()
        }
        Err(e) => err_internal(e.to_string()).into_response(),
    }
}
