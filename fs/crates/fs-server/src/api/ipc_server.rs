//! Daemon-side IPC listener.
//!
//! On Unix:   binds a `UnixListener` at `cfg.api.ipc_path`.
//! On Windows: uses a named-pipe server via `tokio::net::windows::named_pipe`.
//!
//! Each connection is handled in its own task. Requests are decoded from
//! length-prefixed postcard frames, dispatched to query/metrics helpers, and
//! the response is written back as a single length-prefixed frame.
//!
//! A daemon run as a system service (`shared`) serves every local user: the
//! socket or pipe lets them all connect, and requests that would act as the
//! daemon's own account on their behalf (`Open`, extension commands) are
//! refused. Watch path changes are refused too, unless the client is an
//! administrator (`client_is_admin`).

use std::sync::{atomic::Ordering, Arc};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, RwLock};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::extensions::Extensions;
use crate::indexer::crawler::IndexMetrics;
use crate::query::{evaluator, parser};
use crate::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

use fs_protocol::ipc_protocol::{
    decode_payload, encode_frame, DecodeError, IpcEnvelope, IpcMessage, IpcRequest, IpcResponse,
    NoticeKind, RootStatus, RootsResponse, SearchHitDto, SearchResponse, StatsResponse,
    MAX_FRAME_BYTES,
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the IPC listener until `shutdown` is cancelled.
#[allow(clippy::too_many_arguments)]
pub async fn run_ipc_server(
    cfg: Arc<RwLock<Config>>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    reindex_tx: watch::Sender<bool>,
    extensions: Arc<Extensions>,
    watch_list: Arc<crate::watch_list::WatchList>,
    shared: bool,
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
        if shared {
            use std::os::unix::fs::PermissionsExt;
            // The daemon runs as root here and its umask leaves the socket
            // unwritable for others, so no user could connect.
            if let Err(e) =
                std::fs::set_permissions(&ipc_path, std::fs::Permissions::from_mode(0o666))
            {
                tracing::error!("ipc: cannot open {} to all users: {e}", ipc_path.display());
            }
        }
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
                            // getsockopt only; any group lookup waits
                            // for a request that needs it.
                            let peer = match stream.peer_cred() {
                                Ok(c) if shared => Peer::Unix { uid: c.uid(), gid: c.gid() },
                                _ => Peer::Unknown,
                            };
                            let ctx = ConnCtx {
                                cfg:        Arc::clone(&cfg),
                                sqlite:     Arc::clone(&sqlite),
                                tantivy:    Arc::clone(&tantivy),
                                metrics:    Arc::clone(&metrics),
                                reindex_tx: reindex_tx.clone(),
                                extensions: Arc::clone(&extensions),
                                watch_list: Arc::clone(&watch_list),
                                shared,
                                peer,
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
        let security = if shared {
            match pipe_security::AllUsers::new() {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::error!("ipc: cannot build the pipe's access list: {e}");
                    return;
                }
            }
        } else {
            None
        };
        let mut first = true;

        loop {
            let mut options = ServerOptions::new();
            options.pipe_mode(PipeMode::Byte);
            // Fails if someone else already made a pipe of this name, which
            // could otherwise answer the daemon's clients.
            options.first_pipe_instance(first && shared);
            let created = match &security {
                Some(sec) => sec.create(&options, &pipe_name),
                None => options.create(&pipe_name),
            };
            first = false;
            let server = match created {
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
                            let peer = Peer::Windows {
                                elevated: shared && client_is_admin(&server),
                            };
                            let ctx = ConnCtx {
                                cfg:        Arc::clone(&cfg),
                                sqlite:     Arc::clone(&sqlite),
                                tantivy:    Arc::clone(&tantivy),
                                metrics:    Arc::clone(&metrics),
                                reindex_tx: reindex_tx.clone(),
                                extensions: Arc::clone(&extensions),
                                watch_list: Arc::clone(&watch_list),
                                shared,
                                peer,
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
    cfg: Arc<RwLock<Config>>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    reindex_tx: watch::Sender<bool>,
    extensions: Arc<Extensions>,
    watch_list: Arc<crate::watch_list::WatchList>,
    shared: bool,
    /// Who the client is, for `peer_is_admin`. Looked up only for `shared`.
    peer: Peer,
}

/// Who is on the other end of a connection, as far as the administrator
/// check needs.
#[derive(Clone, Copy)]
enum Peer {
    /// Unix peer credentials.
    #[cfg(unix)]
    Unix { uid: u32, gid: u32 },
    /// Unix: no credentials, or not a shared daemon.
    #[cfg(unix)]
    Unknown,
    /// Windows: whether the client's token is elevated.
    #[cfg(windows)]
    Windows { elevated: bool },
}

// ---------------------------------------------------------------------------
// Connection handler - platform-agnostic over any AsyncRead+AsyncWrite
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
                    code: "FRAME_TOO_LARGE".into(),
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
        if let IpcMessage::Request(IpcRequest::Subscribe { kinds }) = env.message {
            return subscribe(stream, request_id, kinds, &ctx).await;
        }
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

/// Sends the subscriber the notices it asked for until it hangs up. What it
/// sends meanwhile is ignored: the connection is a stream now.
async fn subscribe<S>(stream: S, request_id: u128, kinds: Vec<NoticeKind>, ctx: &ConnCtx)
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut rx = ctx.metrics.subscribe();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let send = |message: IpcResponse| {
        encode_frame(&IpcEnvelope::new(request_id, IpcMessage::Response(message)))
    };
    let Ok(frame) = send(IpcResponse::Subscribed) else {
        return;
    };
    if wr.write_all(&frame).await.is_err() {
        return;
    }
    let mut probe = [0u8; 64];
    loop {
        tokio::select! {
            read = rd.read(&mut probe) => match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            },
            notice = crate::indexer::crawler::next_notice(&mut rx) => match notice {
                None => return,
                Some(n) if n.wanted(&kinds) => {
                    let Ok(frame) = send(IpcResponse::Notice(n)) else { continue };
                    if wr.write_all(&frame).await.is_err() {
                        return;
                    }
                }
                Some(_) => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

async fn dispatch(req: IpcRequest, request_id: u128, ctx: &ConnCtx) -> IpcEnvelope {
    // The daemon's account may read folders the client cannot, and the
    // index would then show their names to every user.
    if matches!(
        req,
        IpcRequest::WatchAdd { .. } | IpcRequest::WatchRemove { .. }
    ) && !may_change_watch_paths(ctx.shared, ctx.shared && peer_is_admin(ctx).await)
    {
        return error_response(request_id, "NOT_ALLOWED", SHARED_WATCH_REFUSAL);
    }
    // The config may hold passwords (archive and extension settings), and
    // a change acts as the daemon's account; the same rule as watch paths.
    if matches!(
        req,
        IpcRequest::ConfigGet { .. } | IpcRequest::ConfigChange { .. }
    ) && !may_change_watch_paths(ctx.shared, ctx.shared && peer_is_admin(ctx).await)
    {
        return error_response(request_id, "NOT_ALLOWED", SHARED_CONFIG_REFUSAL);
    }
    match req {
        IpcRequest::Search {
            query,
            limit,
            offset,
        } => search_handler(request_id, query, limit, offset, ctx).await,
        IpcRequest::Stats => stats_handler(request_id, ctx),
        IpcRequest::Reindex { full } => reindex_handler(request_id, full, ctx),
        IpcRequest::Open { .. } | IpcRequest::Extension { .. } if ctx.shared => {
            error_response(request_id, "NOT_ALLOWED", SHARED_REFUSAL)
        }
        IpcRequest::Open { path } => open_handler(request_id, path, ctx).await,
        IpcRequest::Extension { name, command } => {
            extension_handler(request_id, &name, command, &ctx.extensions).await
        }
        IpcRequest::Roots => roots_handler(request_id, ctx).await,
        // Handled by `handle_connection`, which turns the connection over.
        IpcRequest::Subscribe { .. } => error_response(
            request_id,
            "UNEXPECTED_SUBSCRIBE",
            "Subscribe is answered with a stream",
        ),
        IpcRequest::WatchAdd { path } => {
            let changed = ctx.watch_list.add(&path).await;
            watch_handler(request_id, changed, ctx).await
        }
        IpcRequest::WatchRemove { path, forget } => {
            let changed = ctx.watch_list.remove(&path, forget).await;
            watch_handler(request_id, changed, ctx).await
        }
        IpcRequest::ConfigGet { key } => {
            let got = ctx.watch_list.config().get(key.as_deref()).await;
            config_response(request_id, got.map(IpcResponse::Config))
        }
        IpcRequest::ConfigChange { change } => {
            let applied = ctx.watch_list.config().change(change).await;
            config_response(request_id, applied.map(IpcResponse::ConfigApplied))
        }
    }
}

fn config_response(
    request_id: u128,
    answer: Result<IpcResponse, crate::config_edit::ConfigError>,
) -> IpcEnvelope {
    use crate::config_edit::ConfigError;
    match answer {
        Ok(r) => IpcEnvelope::new(request_id, IpcMessage::Response(r)),
        Err(e) => {
            let code = match e {
                ConfigError::Invalid(_) => "INVALID_CONFIG",
                ConfigError::NotFound(_) => "NOT_FOUND",
                ConfigError::Refused(_) => "NOT_ALLOWED",
                ConfigError::Io(_) => "CONFIG_ERROR",
            };
            error_response(request_id, code, e.to_string())
        }
    }
}

const SHARED_REFUSAL: &str = "fsd runs as a system service for all users, so it does not open files or run extension commands for a client; open the file yourself";

const SHARED_CONFIG_REFUSAL: &str = "fsd runs as a system service for all users, so only an administrator reads or changes its config over IPC (root or api.admin_group on Unix, an elevated prompt on Windows); or use REST or gRPC with the token";

const SHARED_WATCH_REFUSAL: &str = "fsd runs as a system service for all users, so only an administrator changes its watch paths over IPC (root or api.admin_group on Unix, an elevated prompt on Windows); or edit its config file, or use REST or gRPC with the token";

/// Changing the watch paths makes the daemon's account read what it may
/// read and show its names to every user, so a shared daemon takes it only
/// from an administrator. Decided per request.
fn may_change_watch_paths(shared: bool, admin: bool) -> bool {
    !shared || admin
}

/// Whether the client is an administrator: root or a member of
/// `api.admin_group` on Unix, an elevated token on Windows.
async fn peer_is_admin(ctx: &ConnCtx) -> bool {
    match ctx.peer {
        #[cfg(unix)]
        Peer::Unix { uid: 0, .. } => true,
        #[cfg(unix)]
        Peer::Unix { uid, gid } => {
            let Some(group) = ctx.cfg.read().await.api.admin_group.clone() else {
                return false;
            };
            // getgrnam and getpwuid may ask a directory service over the
            // network, so they stay off the runtime's threads.
            tokio::task::spawn_blocking(move || in_group(uid, gid, &group))
                .await
                .unwrap_or(false)
        }
        #[cfg(unix)]
        Peer::Unknown => false,
        #[cfg(windows)]
        Peer::Windows { elevated } => elevated,
    }
}

/// `uid` (primary group `gid`) belongs to `group`. Blocking.
#[cfg(unix)]
fn in_group(uid: u32, gid: u32, group: &str) -> bool {
    use nix::unistd::{Group, Uid, User};
    let Ok(Some(group)) = Group::from_name(group) else {
        return false;
    };
    gid == group.gid.as_raw()
        || User::from_uid(Uid::from_raw(uid))
            .ok()
            .flatten()
            .is_some_and(|u| group.mem.contains(&u.name))
}

/// An elevated token: an administrator who confirmed UAC, or SYSTEM. A
/// filtered administrator token is not elevated.
#[cfg(windows)]
fn client_is_admin(pipe: &tokio::net::windows::named_pipe::NamedPipeServer) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the pipe handle is valid while `pipe` lives; the process and
    // token handles are closed before returning; `elevation` is the size
    // passed.
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle() as isize), &mut pid).is_err() {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(process);
        if opened.is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let read = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        read.is_ok() && elevation.TokenIsElevated != 0
    }
}

async fn watch_handler(
    request_id: u128,
    changed: Result<crate::watch_list::Outcome, crate::watch_list::WatchListError>,
    ctx: &ConnCtx,
) -> IpcEnvelope {
    use crate::watch_list::WatchListError;
    match changed {
        Ok(_) => roots_handler(request_id, ctx).await,
        Err(e) => {
            let code = match e {
                WatchListError::Invalid(_) => "INVALID_PATH",
                WatchListError::NotFound(_) => "NOT_FOUND",
                WatchListError::Config(_) => "CONFIG_ERROR",
            };
            error_response(request_id, code, e.to_string())
        }
    }
}

async fn roots_handler(request_id: u128, ctx: &ConnCtx) -> IpcEnvelope {
    let paths = ctx.cfg.read().await.watch_paths.clone();
    let progress = ctx.metrics.roots(&paths);
    let roots = progress.iter().map(|r| r.path.clone()).collect();
    let status = progress
        .iter()
        .map(|r| RootStatus {
            state: r.state,
            entries: r.entries,
        })
        .collect();
    let crawled = ctx.metrics.last_crawl_secs.load(Ordering::Relaxed) != 0;
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Roots(RootsResponse {
            roots,
            crawled,
            status,
        })),
    )
}

async fn search_handler(
    request_id: u128,
    query_str: String,
    limit: u32,
    offset: u32,
    ctx: &ConnCtx,
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
    plan.limit = Some(limit as usize);
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
            total_files: m.total_files.load(Ordering::Relaxed),
            total_dirs: m.total_dirs.load(Ordering::Relaxed),
            content_indexed: m.content_indexed.load(Ordering::Relaxed),
            archive_entries_indexed: m.archive_entries_indexed.load(Ordering::Relaxed),
            archives_crawled: m.archives_crawled.load(Ordering::Relaxed),
            events_processed: m.events_processed.load(Ordering::Relaxed),
            last_crawl_secs: m.last_crawl_secs.load(Ordering::Relaxed),
        })),
    )
}

fn reindex_handler(request_id: u128, full: bool, ctx: &ConnCtx) -> IpcEnvelope {
    let _ = ctx.reindex_tx.send(full);
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Reindex { accepted: true }),
    )
}

async fn open_handler(request_id: u128, path: String, ctx: &ConnCtx) -> IpcEnvelope {
    match ctx.sqlite.get_by_path(&path) {
        Ok(None) => return error_response(request_id, "NOT_FOUND", "path not in index"),
        Err(e) => return error_response(request_id, "STORE_ERROR", e.to_string()),
        Ok(Some(_)) => {}
    }

    if path.contains("!/") {
        match super::open::extract_and_open(&path).await {
            Ok(temp) => IpcEnvelope::new(
                request_id,
                IpcMessage::Response(IpcResponse::Open {
                    opened: true,
                    temp_path: Some(temp),
                }),
            ),
            Err(e) => error_response(request_id, "OPEN_ERROR", e),
        }
    } else {
        match super::open::open_with_os(&path).await {
            Ok(()) => IpcEnvelope::new(
                request_id,
                IpcMessage::Response(IpcResponse::Open {
                    opened: true,
                    temp_path: None,
                }),
            ),
            Err(e) => error_response(request_id, "OPEN_ERROR", e),
        }
    }
}

async fn extension_handler(
    request_id: u128,
    name: &str,
    command: String,
    extensions: &Extensions,
) -> IpcEnvelope {
    let Some(ext) = extensions.iter().find(|e| e.name() == name).cloned() else {
        return error_response(
            request_id,
            "UNKNOWN_EXTENSION",
            format!("extension not enabled: {name}"),
        );
    };
    match tokio::task::spawn_blocking(move || ext.command(&command)).await {
        Ok(Ok(output)) => IpcEnvelope::new(
            request_id,
            IpcMessage::Response(IpcResponse::Extension { output }),
        ),
        Ok(Err(e)) => error_response(request_id, "EXTENSION_ERROR", format!("{e:#}")),
        Err(e) => error_response(request_id, "EXTENSION_ERROR", e.to_string()),
    }
}

/// The named pipe's access list when the daemon serves every user. Without
/// one, a pipe made by a service gets the default list, which lets other
/// users read but not write, so they could not send a request.
#[cfg(windows)]
mod pipe_security {
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    /// SYSTEM and administrators: full control. Signed-in users: read and
    /// write, which is what a client's open asks for.
    const SDDL: windows::core::PCWSTR = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)");

    pub(super) struct AllUsers(PSECURITY_DESCRIPTOR);

    // The descriptor is only read after it is built.
    unsafe impl Send for AllUsers {}
    unsafe impl Sync for AllUsers {}

    impl AllUsers {
        pub(super) fn new() -> windows::core::Result<Self> {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    SDDL,
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )?;
            }
            Ok(Self(sd))
        }

        pub(super) fn create(
            &self,
            options: &ServerOptions,
            name: &str,
        ) -> std::io::Result<NamedPipeServer> {
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0 .0,
                bInheritHandle: false.into(),
            };
            // SAFETY: `attrs` and the descriptor it points to outlive the call.
            unsafe {
                options.create_with_security_attributes_raw(
                    name,
                    &mut attrs as *mut SECURITY_ATTRIBUTES as *mut std::ffi::c_void,
                )
            }
        }
    }

    impl Drop for AllUsers {
        fn drop(&mut self) {
            unsafe {
                let _ = LocalFree(HLOCAL(self.0 .0 as _));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn file_record_to_dto(r: &FileRecord) -> SearchHitDto {
    SearchHitDto {
        path: r.path.clone(),
        name: r.name.clone(),
        extension: r.extension.clone(),
        size_bytes: r.size_bytes,
        mtime: r.mtime,
        ctime: r.ctime,
        is_dir: r.is_dir,
        attrib_bitmask: r.attrib_bitmask,
        archive_path: r.archive_path.clone(),
        snippet: None,
    }
}

fn error_response(
    request_id: u128,
    code: impl Into<String>,
    msg: impl Into<String>,
) -> IpcEnvelope {
    IpcEnvelope::new(
        request_id,
        IpcMessage::Response(IpcResponse::Error {
            code: code.into(),
            message: msg.into(),
        }),
    )
}

fn decode_err_code(e: &DecodeError) -> &'static str {
    match e {
        DecodeError::Postcard(_) => "DECODE_ERROR",
        DecodeError::FrameTooLarge(_) => "FRAME_TOO_LARGE",
        DecodeError::BadMagic(_) => "BAD_MAGIC",
        DecodeError::VersionMismatch(_) => "VERSION_MISMATCH",
        DecodeError::Io(_) => "IO_ERROR",
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeExtension;

    impl fs_common::extension::Extension for FakeExtension {
        fn name(&self) -> &str {
            "fake"
        }
        fn validate(&self) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _shutdown: CancellationToken) -> fs_common::extension::ExtensionFuture {
            Box::pin(async { Ok(()) })
        }
        fn command(&self, command: &str) -> anyhow::Result<String> {
            match command {
                "ok" => Ok("done".into()),
                other => anyhow::bail!("bad command {other}"),
            }
        }
    }

    async fn call_extension(name: &str, command: &str) -> IpcResponse {
        let extensions: Extensions = vec![Arc::new(FakeExtension)];
        let env = extension_handler(7, name, command.into(), &extensions).await;
        assert_eq!(env.request_id, 7);
        match env.message {
            IpcMessage::Response(r) => r,
            IpcMessage::Request(_) => panic!("expected a response"),
        }
    }

    #[tokio::test]
    async fn extension_command_returns_output() {
        match call_extension("fake", "ok").await {
            IpcResponse::Extension { output } => assert_eq!(output, "done"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn extension_failure_maps_to_error() {
        match call_extension("fake", "nope").await {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, "EXTENSION_ERROR");
                assert!(message.contains("bad command nope"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_extension_maps_to_unknown() {
        match call_extension("missing", "ok").await {
            IpcResponse::Error { code, .. } => assert_eq!(code, "UNKNOWN_EXTENSION"),
            other => panic!("unexpected {other:?}"),
        }
    }
    use crate::store::FileRecord;

    fn make_record(path: &str, archive_path: Option<&str>) -> FileRecord {
        FileRecord {
            id: None,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            extension: Some("rs".into()),
            size_bytes: 2048,
            mtime: 1_700_000_000,
            ctime: 1_700_000_000,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: archive_path.map(str::to_string),
        }
    }

    /// root is in every group check's place; a missing group holds no one.
    #[cfg(unix)]
    #[test]
    fn group_membership_is_read_from_the_system() {
        let me = nix::unistd::getuid().as_raw();
        let my_gid = nix::unistd::getgid().as_raw();
        let primary = nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(my_gid))
            .unwrap()
            .unwrap();
        assert!(in_group(me, my_gid, &primary.name));
        assert!(!in_group(me, my_gid, "fs-no-such-group-here"));
    }

    #[test]
    fn only_an_administrator_changes_a_shared_daemons_watch_paths() {
        assert!(may_change_watch_paths(false, false));
        assert!(may_change_watch_paths(true, true));
        assert!(!may_change_watch_paths(true, false));
    }

    /// This test process, run from an elevated prompt or not, is told apart
    /// the same way `whoami /groups` would.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_pipe_client_is_an_administrator_only_when_elevated() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        let name = format!(r"\\.\pipe\fs_admin_test_{}", std::process::id());
        let server = ServerOptions::new().create(&name).unwrap();
        let _client = ClientOptions::new().open(&name).unwrap();
        server.connect().await.unwrap();
        let elevated = std::process::Command::new("net")
            .arg("session")
            .output()
            .is_ok_and(|o| o.status.success());
        assert_eq!(client_is_admin(&server), elevated);
    }

    #[test]
    fn error_response_sets_correct_fields() {
        let env = error_response(99u128, "MY_CODE", "bad things");
        assert_eq!(env.request_id, 99u128);
        assert!(matches!(
            env.message,
            IpcMessage::Response(IpcResponse::Error { ref code, ref message })
            if code == "MY_CODE" && message == "bad things"
        ));
    }

    #[test]
    fn decode_err_code_all_variants() {
        use fs_protocol::ipc_protocol::DecodeError;
        assert_eq!(
            decode_err_code(&DecodeError::Postcard(postcard::Error::WontImplement)),
            "DECODE_ERROR"
        );
        assert_eq!(
            decode_err_code(&DecodeError::FrameTooLarge(99)),
            "FRAME_TOO_LARGE"
        );
        assert_eq!(
            decode_err_code(&DecodeError::BadMagic(*b"XXXX")),
            "BAD_MAGIC"
        );
        assert_eq!(
            decode_err_code(&DecodeError::VersionMismatch(99)),
            "VERSION_MISMATCH"
        );
        assert_eq!(
            decode_err_code(&DecodeError::Io(std::io::Error::other("io"))),
            "IO_ERROR"
        );
    }

    #[test]
    fn file_record_to_dto_maps_all_fields() {
        let r = make_record("/data/archive.zip!/src/main.rs", Some("/data/archive.zip"));
        let dto = file_record_to_dto(&r);
        assert_eq!(dto.path, r.path);
        assert_eq!(dto.name, r.name);
        assert_eq!(dto.extension, r.extension);
        assert_eq!(dto.size_bytes, 2048);
        assert_eq!(dto.mtime, 1_700_000_000);
        assert!(!dto.is_dir);
        assert_eq!(dto.archive_path, Some("/data/archive.zip".into()));
        assert_eq!(dto.snippet, None);
    }

    #[test]
    fn file_record_to_dto_real_file_has_no_archive_path() {
        let r = make_record("/home/user/main.rs", None);
        let dto = file_record_to_dto(&r);
        assert_eq!(dto.archive_path, None);
    }
}
