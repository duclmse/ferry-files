use std::convert::Infallible;
use std::path::Path;

use axum::{
    extract::{Query, Request, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

use crate::api::rest::AppState;

use super::common::{
    bad_request, internal, not_found, validate_path, OkResponse, PathParam, ProgressEvent,
    StreamParam,
};

// ---------------------------------------------------------------------------
// PUT /fs/write?path=
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WriteResponse {
    written: u64,
}

pub(super) async fn write_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
    request: Request,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p) => p,
        Err(e) => return bad_request(e).into_response(),
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return internal(format!("could not create parent directories: {e}")).into_response();
        }
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return internal(format!("body read error: {e}")).into_response(),
    };
    let written = bytes.len() as u64;

    if let Err(e) = tokio::fs::write(&path, &bytes).await {
        return internal(e.to_string()).into_response();
    }

    Json(WriteResponse { written }).into_response()
}

// ---------------------------------------------------------------------------
// SSE progress helpers
// ---------------------------------------------------------------------------

type ProgressTx = tokio::sync::mpsc::Sender<ProgressEvent>;

fn send_progress(tx: &ProgressTx, ev: ProgressEvent) {
    // Called from spawn_blocking threads; blocking_send is the right API.
    let _ = tx.blocking_send(ev);
}

fn make_sse(rx: tokio::sync::mpsc::Receiver<ProgressEvent>) -> Response {
    let stream = ReceiverStream::new(rx).map(|ev| {
        Ok::<Event, Infallible>(
            Event::default()
                .json_data(&ev)
                .unwrap_or_else(|_| Event::default().data("progress")),
        )
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /fs/copy[?stream=true]  { "from": "...", "to": "..." }
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct FromToBody {
    from: String,
    to: String,
}

pub(super) async fn copy_handler(
    State(_state): State<AppState>,
    Query(qp): Query<StreamParam>,
    Json(body): Json<FromToBody>,
) -> Response {
    let src = match validate_path(&body.from) {
        Ok(p) => p,
        Err(e) => return bad_request(format!("'from': {e}")).into_response(),
    };
    let dst = match validate_path(&body.to) {
        Ok(p) => p,
        Err(e) => return bad_request(format!("'to': {e}")).into_response(),
    };

    if qp.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
        tokio::task::spawn_blocking(move || {
            let total = count_entries(&src);
            send_progress(
                &tx,
                ProgressEvent {
                    processed: 0,
                    total,
                    current: String::new(),
                    done: false,
                    error: None,
                },
            );
            let mut processed = 0;
            match copy_recursive_tracked(&src, &dst, &tx, &mut processed, total) {
                Ok(()) => send_progress(
                    &tx,
                    ProgressEvent {
                        processed: total,
                        total,
                        current: String::new(),
                        done: true,
                        error: None,
                    },
                ),
                Err(e) => send_progress(
                    &tx,
                    ProgressEvent {
                        processed,
                        total,
                        current: String::new(),
                        done: true,
                        error: Some(e),
                    },
                ),
            }
        });
        make_sse(rx)
    } else {
        let r = tokio::task::spawn_blocking(move || copy_recursive(&src, &dst))
            .await
            .map_err(|e| e.to_string());
        match r {
            Ok(Ok(())) => Json(OkResponse { ok: true }).into_response(),
            Ok(Err(e)) => internal(e).into_response(),
            Err(e) => internal(e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// POST /fs/move[?stream=true]  { "from": "...", "to": "..." }
// ---------------------------------------------------------------------------

pub(super) async fn move_handler(
    State(_state): State<AppState>,
    Query(qp): Query<StreamParam>,
    Json(body): Json<FromToBody>,
) -> Response {
    let src = match validate_path(&body.from) {
        Ok(p) => p,
        Err(e) => return bad_request(format!("'from': {e}")).into_response(),
    };
    let dst = match validate_path(&body.to) {
        Ok(p) => p,
        Err(e) => return bad_request(format!("'to': {e}")).into_response(),
    };

    if let Some(parent) = dst.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return internal(format!("cannot create destination directory: {e}")).into_response();
        }
    }

    if qp.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
        tokio::task::spawn_blocking(move || {
            // Try atomic rename first (O(1), same filesystem).
            if std::fs::rename(&src, &dst).is_ok() {
                send_progress(
                    &tx,
                    ProgressEvent {
                        processed: 1,
                        total: 1,
                        current: src.display().to_string(),
                        done: true,
                        error: None,
                    },
                );
                return;
            }
            // Fall back to copy+delete across filesystems.
            let total = count_entries(&src);
            send_progress(
                &tx,
                ProgressEvent {
                    processed: 0,
                    total,
                    current: String::new(),
                    done: false,
                    error: None,
                },
            );
            let mut processed = 0;
            let r = copy_recursive_tracked(&src, &dst, &tx, &mut processed, total).and_then(|()| {
                std::fs::remove_dir_all(&src).map_err(|e| format!("remove source: {e}"))
            });
            match r {
                Ok(()) => send_progress(
                    &tx,
                    ProgressEvent {
                        processed: total,
                        total,
                        current: String::new(),
                        done: true,
                        error: None,
                    },
                ),
                Err(e) => send_progress(
                    &tx,
                    ProgressEvent {
                        processed,
                        total,
                        current: String::new(),
                        done: true,
                        error: Some(e),
                    },
                ),
            }
        });
        make_sse(rx)
    } else {
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => return Json(OkResponse { ok: true }).into_response(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return not_found(format!("source not found: {}", src.display())).into_response()
            }
            Err(_) => {} // fallthrough: cross-filesystem rename
        }
        let r = tokio::task::spawn_blocking(move || {
            copy_recursive(&src, &dst)?;
            std::fs::remove_dir_all(&src).map_err(|e| format!("remove source: {e}"))
        })
        .await
        .map_err(|e| e.to_string());
        match r {
            Ok(Ok(())) => Json(OkResponse { ok: true }).into_response(),
            Ok(Err(e)) => internal(e).into_response(),
            Err(e) => internal(e).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// DELETE /fs/delete?path=[&stream=true]
// ---------------------------------------------------------------------------

pub(super) async fn delete_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p) => p,
        Err(e) => return bad_request(e).into_response(),
    };

    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return not_found(format!("path not found: {raw}")).into_response()
        }
        Err(e) => return internal(e.to_string()).into_response(),
    };

    if params.stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
        tokio::task::spawn_blocking(move || {
            let total = count_entries(&path);
            send_progress(
                &tx,
                ProgressEvent {
                    processed: 0,
                    total,
                    current: String::new(),
                    done: false,
                    error: None,
                },
            );
            let mut processed = 0;
            match delete_recursive_tracked(&path, &tx, &mut processed, total) {
                Ok(()) => send_progress(
                    &tx,
                    ProgressEvent {
                        processed: total,
                        total,
                        current: String::new(),
                        done: true,
                        error: None,
                    },
                ),
                Err(e) => send_progress(
                    &tx,
                    ProgressEvent {
                        processed,
                        total,
                        current: String::new(),
                        done: true,
                        error: Some(e),
                    },
                ),
            }
        });
        make_sse(rx)
    } else {
        let r = if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        match r {
            Ok(()) => Json(OkResponse { ok: true }).into_response(),
            Err(e) => internal(e.to_string()).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// POST /fs/mkdir?path=
// ---------------------------------------------------------------------------

pub(super) async fn mkdir_handler(
    State(_state): State<AppState>,
    Query(params): Query<PathParam>,
) -> Response {
    let raw = match params.path {
        Some(p) if !p.is_empty() => p,
        _ => return bad_request("missing 'path' parameter").into_response(),
    };
    let path = match validate_path(&raw) {
        Ok(p) => p,
        Err(e) => return bad_request(e).into_response(),
    };

    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Blocking helpers - called only from spawn_blocking
// ---------------------------------------------------------------------------

/// Count all files under `root` (directories not counted; they have no size
/// progress to report).  Iterative to avoid stack overflow on deep trees.
fn count_entries(root: &Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_file() {
            count += 1;
        } else if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    count
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("cannot read source: {e}"))?;
    if meta.is_file() {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| format!("cannot create destination directory: {e}"))?;
        }
        std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| format!("copy failed: {e}"))
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("cannot create destination directory: {e}"))?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        Err("source is neither a file nor a directory".into())
    }
}

fn copy_recursive_tracked(
    src: &Path,
    dst: &Path,
    tx: &ProgressTx,
    processed: &mut usize,
    total: usize,
) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("cannot read source: {e}"))?;
    if meta.is_file() {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        std::fs::copy(src, dst).map_err(|e| format!("copy failed: {e}"))?;
        *processed += 1;
        send_progress(
            tx,
            ProgressEvent {
                processed: *processed,
                total,
                current: src.display().to_string(),
                done: false,
                error: None,
            },
        );
    } else if meta.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_recursive_tracked(
                &entry.path(),
                &dst.join(entry.file_name()),
                tx,
                processed,
                total,
            )?;
        }
    }
    Ok(())
}

fn delete_recursive_tracked(
    path: &Path,
    tx: &ProgressTx,
    processed: &mut usize,
    total: usize,
) -> Result<(), String> {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            delete_recursive_tracked(&entry.path(), tx, processed, total)?;
        }
        std::fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
        *processed += 1;
        send_progress(
            tx,
            ProgressEvent {
                processed: *processed,
                total,
                current: path.display().to_string(),
                done: false,
                error: None,
            },
        );
        Ok(())
    }
}
