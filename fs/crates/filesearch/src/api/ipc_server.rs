//! Daemon-side IPC listener.
//!
//! On Unix:   binds a `UnixListener` at `cfg.api.ipc_path`.
//! On Windows: uses a named-pipe server via `tokio::net::windows::named_pipe`.
//!
//! Each connection is handled in its own task. Requests are decoded from
//! length-prefixed postcard frames, dispatched to query/metrics helpers, and
//! the response is written back as a single length-prefixed frame.

use std::sync::{
    atomic::Ordering,
    Arc,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::indexer::crawler::IndexMetrics;
use crate::query::{evaluator, parser};
use crate::store::{sqlite_store::SqliteStore, tantivy_store::TantivyStore, FileRecord};

use super::ipc_protocol::{
    decode_payload, encode_frame, DecodeError, IpcEnvelope, IpcMessage, IpcRequest, IpcResponse,
    MAX_FRAME_BYTES, SearchHitDto, SearchResponse, StatsResponse,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the IPC listener until `shutdown` is cancelled.
pub async fn run_ipc_server(
    cfg:      Arc<RwLock<Config>>,
    sqlite:   Arc<SqliteStore>,
    tantivy:  Arc<TantivyStore>,
    metrics:  Arc<IndexMetrics>,
    shutdown: CancellationToken,
) {
    let ipc_path = cfg.read().await.api.ipc_path.clone();

    #[cfg(unix)]
    {
        use tokio::net::UnixListener;

        // Remove stale socket file from a previous run.
        let _ = std::fs::remove_file(&ipc_path);

        let listener = match UnixListener::bind(&ipc_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("ipc: failed to bind {}: {e}", ipc_path.display());
                return;
            }
        };
        tracing::info!("ipc: listening on {}", ipc_path.display());

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("ipc server shutting down");
                    let _ = std::fs::remove_file(&ipc_path);
                    break;
                }
                res = listener.accept() => {
                    match res {
                        Ok((stream, _)) => {
                            let ctx = ConnCtx {
                                cfg:     Arc::clone(&cfg),
                                sqlite:  Arc::clone(&sqlite),
                                tantivy: Arc::clone(&tantivy),
                                metrics: Arc::clone(&metrics),
                            };
                            tokio::spawn(handle_connection(stream, ctx));
                        }
                        Err(e) => tracing::warn!("ipc: accept error: {e}"),
                    }
                }
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};

        let pipe_name: String = ipc_path.to_string_lossy().into_owned();

        loop {
            let server = match ServerOptions::new()
                .pipe_mode(PipeMode::Byte)
                .create(&pipe_name)
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("ipc: failed to create named pipe: {e}");
                    return;
                }
            };

            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!("ipc server shutting down");
                    break;
                }
                res = server.connect() => {
                    match res {
                        Ok(()) => {
                            let ctx = ConnCtx {
                                cfg:     Arc::clone(&cfg),
                                sqlite:  Arc::clone(&sqlite),
                                tantivy: Arc::clone(&tantivy),
                                metrics: Arc::clone(&metrics),
                            };
                            tokio::spawn(handle_connection(server, ctx));
                        }
                        Err(e) => tracing::warn!("ipc: pipe connect error: {e}"),
                    }
                }
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    tracing::warn!("ipc: not supported on this platform");
}

// ---------------------------------------------------------------------------
// Per-connection context
// ---------------------------------------------------------------------------

struct ConnCtx {
    cfg:     Arc<RwLock<Config>>,
    sqlite:  Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
}

// ---------------------------------------------------------------------------
// Connection handler — platform-agnostic over any AsyncRead+AsyncWrite
// ---------------------------------------------------------------------------

async fn handle_connection<S>(mut stream: S, ctx: ConnCtx)
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    loop {
        // Read 4-byte LE length prefix.
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                tracing::debug!("ipc: read len error: {e}");
                break;
            }
        }
        let payload_len = u32::from_le_bytes(len_buf);

        if payload_len > MAX_FRAME_BYTES {
            tracing::warn!("ipc: frame too large ({payload_len} bytes), closing");
            let err_env = IpcEnvelope::new(
                0,
                IpcMessage::Response(IpcResponse::Error {
                    code:    "FRAME_TOO_LARGE".into(),
                    message: format!("frame {payload_len} exceeds max {MAX_FRAME_BYTES}"),
                }),
            );
            if let Ok(frame) = encode_frame(&err_env) {
                let _ = stream.write_all(&frame).await;
            }
            break;
        }

        // Read payload.
        let mut payload = vec![0u8; payload_len as usize];
        if let Err(e) = stream.read_exact(&mut payload).await {
            tracing::debug!("ipc: read payload error: {e}");
            break;
        }

        // Decode envelope.
        let env = match decode_payload(&payload) {
            Ok(e) => e,
            Err(e) => {
                let resp = error_response(0, decode_err_code(&e), e.to_string());
                if let Ok(frame) = encode_frame(&resp) {
                    let _ = stream.write_all(&frame).await;
                }
                continue;
            }
        };

        let request_id = env.request_id;
        let response_env = match env.message {
            IpcMessage::Request(req) => dispatch(req, request_id, &ctx).await,
            IpcMessage::Response(_) => error_response(
                request_id,
                "UNEXPECTED_RESPONSE",
                "server received a Response frame",
            ),
        };

        match encode_frame(&response_env) {
            Ok(frame) => {
                if let Err(e) = stream.write_all(&frame).await {
                    tracing::debug!("ipc: write error: {e}");
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("ipc: encode error: {e}");
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

async fn dispatch(req: IpcRequest, request_id: u128, ctx: &ConnCtx) -> IpcEnvelope {
    match req {
        IpcRequest::Search { query, limit, offset } => {
            search_handler(request_id, query, limit, offset, ctx).await
        }
        IpcRequest::Stats => stats_handler(request_id, ctx),
        IpcRequest::Reindex { full } => reindex_handler(request_id, full),
        IpcRequest::Open { path } => open_handler(request_id, path),
    }
}

async fn search_handler(
    request_id: u128,
    query_str:  String,
    limit:      u32,
    offset:     u32,
    ctx:        &ConnCtx,
) -> IpcEnvelope {
    let macros = ctx.cfg.read().await.macros.clone();
    let expr = match parser::parse(&query_str, &macros) {
        Ok(e) => e,
        Err(e) => return error_response(request_id, "PARSE_ERROR", e.to_string()),
    };

    let mut plan = match evaluator::plan(&expr, &Default::default()) {
        Ok(p) => p,
        Err(e) => return error_response(request_id, "PLAN_ERROR", e.to_string()),
    };
    plan.limit  = Some(limit as usize);
    plan.offset = offset as usize;

    match evaluator::execute_plan(
        plan,
        Arc::clone(&ctx.sqlite),
        Arc::clone(&ctx.tantivy),
        Arc::clone(&ctx.cfg),
    )
    .await
    {
        Ok(result) => {
            let hits: Vec<SearchHitDto> = result.hits.iter().map(file_record_to_dto).collect();
            let total_hits = result.total as u64;
            IpcEnvelope::new(
                request_id,
                IpcMessage::Response(IpcResponse::Search(SearchResponse {
                    hits,
                    total_hits,
                    limit,
                    offset,
                    capped: false,
                })),
            )
        }
        Err(e) => error_response(request_id, "SEARCH_ERROR", e.to_string()),
    }
}

fn stats_handler(request_id: u128, ctx: &ConnCtx) -> IpcEnvelope {
    let m = &ctx.metrics;
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Stats(StatsResponse {
            total_files:             m.total_files.load(Ordering::Relaxed),
            total_dirs:              m.total_dirs.load(Ordering::Relaxed),
            content_indexed:         m.content_indexed.load(Ordering::Relaxed),
            archive_entries_indexed: m.archive_entries_indexed.load(Ordering::Relaxed),
            archives_crawled:        m.archives_crawled.load(Ordering::Relaxed),
            events_processed:        m.events_processed.load(Ordering::Relaxed),
            last_crawl_secs:         m.last_crawl_secs.load(Ordering::Relaxed),
        })),
    )
}

fn reindex_handler(request_id: u128, _full: bool) -> IpcEnvelope {
    // Reindex signalling is implemented by the daemon main loop; for now,
    // acknowledge the request. Full wiring is a Phase 7 task.
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Reindex { accepted: true }),
    )
}

fn open_handler(request_id: u128, _path: String) -> IpcEnvelope {
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Open { opened: false, temp_path: None }),
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_record_to_dto(r: &FileRecord) -> SearchHitDto {
    SearchHitDto {
        path:         r.path.clone(),
        name:         r.name.clone(),
        extension:    r.extension.clone(),
        size_bytes:   r.size_bytes,
        mtime:        r.mtime,
        is_dir:       r.is_dir,
        archive_path: r.archive_path.clone(),
        snippet:      None,
    }
}

fn error_response(request_id: u128, code: impl Into<String>, msg: impl Into<String>) -> IpcEnvelope {
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Error { code: code.into(), message: msg.into() }),
    )
}

fn decode_err_code(e: &DecodeError) -> &'static str {
    match e {
        DecodeError::Postcard(_)        => "DECODE_ERROR",
        DecodeError::FrameTooLarge(_)   => "FRAME_TOO_LARGE",
        DecodeError::BadMagic(_)        => "BAD_MAGIC",
        DecodeError::VersionMismatch(_) => "VERSION_MISMATCH",
        DecodeError::Io(_)              => "IO_ERROR",
    }
}
