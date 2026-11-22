//! Daemon lifecycle - startup, API server orchestration, graceful shutdown.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::{mpsc, watch, RwLock};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::api::archive::ArchiveRegistry;
use crate::api::grpc::{self, FileSearchService};
use crate::api::ipc_server::run_ipc_server;
use crate::api::rest::AppState;
use crate::config;
use crate::config::AuthMode;
use crate::error::AppError;
use crate::indexer::crawler::{full_crawl, process_events, IndexEvent, IndexMetrics};
use crate::indexer::exclusions::ExclusionEngine;
use crate::scheduler::{ArchiveSemaphores, ClientRateLimiter, SchedulerMetrics};
use crate::store::{tantivy_store::TantivyStore, FileStore};
use crate::watcher;
use crate::zidx::zidx_path_for;
use tonic::codegen::InterceptedService;

// ---------------------------------------------------------------------------
// Windows single-instance guard via named mutex
// ---------------------------------------------------------------------------

/// The Windows instance mutex for `data_dir`. One daemon per data folder,
/// as the PID file gives on Unix: what must not be shared is the index and
/// its Tantivy writer. A single machine-wide name refused a daemon for any
/// other folder, and a test daemon still shutting down blocked the next one.
/// Lowercased because Windows paths ignore case.
#[cfg_attr(not(windows), allow(dead_code))]
fn instance_mutex_name(data_dir: &std::path::Path) -> String {
    let dir = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    let key = dir.to_string_lossy().to_lowercase();
    let hash = blake3::hash(key.as_bytes()).to_hex();
    format!("Global\\fs_daemon_{}", &hash[..16])
}

#[cfg(windows)]
mod win_guard {
    use super::AppError;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, HANDLE,
    };
    use windows::Win32::System::Threading::CreateMutexW;

    pub struct InstanceGuard(HANDLE);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub fn acquire(data_dir: &std::path::Path) -> Result<InstanceGuard, AppError> {
        let name: Vec<u16> = super::instance_mutex_name(data_dir)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let handle = CreateMutexW(None, BOOL(1), PCWSTR(name.as_ptr()))
                .map_err(|e| AppError::Config(format!("CreateMutexW: {e}")))?;
            // A valid handle with ERROR_ALREADY_EXISTS means another instance owns the mutex.
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return Err(AppError::Config(format!(
                    "fs daemon is already running for {}",
                    data_dir.display()
                )));
            }
            Ok(InstanceGuard(handle))
        }
    }
}

// Maximum concurrent filesystem-event processing tasks.
// Beyond this, new events wait for a running task to finish rather than
// spawning unboundedly. Keeps memory and SQLite-pool contention bounded
// under inotify/FSEvents bursts (e.g. large `git checkout`).

// Longer than the time an extension process is given to stop.
const EXTENSION_DRAIN: Duration = Duration::from_secs(35);

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run(config_path: PathBuf) -> Result<(), AppError> {
    run_with_shutdown(config_path, CancellationToken::new()).await
}

/// Like [`run`] but accepts an external shutdown token so tests (and
/// embedders) can cancel the daemon programmatically.
pub async fn run_with_shutdown(
    config_path: PathBuf,
    shutdown: CancellationToken,
) -> Result<(), AppError> {
    // ── 1. Load & validate config ──────────────────────────────────────────
    let cfg = config::load(&config_path)?;
    let errors = config::validate(&cfg);
    if !errors.is_empty() {
        for e in &errors {
            eprintln!("Config error: {e}");
        }
        return Err(AppError::Config("invalid configuration".into()));
    }

    std::fs::create_dir_all(&cfg.data_dir).map_err(AppError::Io)?;
    std::fs::create_dir_all(&cfg.log_dir).map_err(AppError::Io)?;

    // ── 2. Logging ─────────────────────────────────────────────────────────
    let file_appender = tracing_appender::rolling::daily(&cfg.log_dir, "fs.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Ignore error when already initialised (happens in integration tests that
    // run multiple daemon instances within the same process).
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .try_init();

    tracing::info!("fs daemon starting");

    let loaded_extensions = crate::extensions::load(&config_path)?;
    for problem in &loaded_extensions.problems {
        tracing::error!("extensions: {problem}");
    }
    let extensions = Arc::new(loaded_extensions.extensions);

    // ── 3. Instance guard: PID file (Unix) / named mutex (Windows) ────────
    #[cfg(unix)]
    let pid_path = cfg.data_dir.join("fs.pid");
    #[cfg(unix)]
    write_pid_file(&pid_path)?;
    #[cfg(windows)]
    let _instance_guard = win_guard::acquire(&cfg.data_dir)?;

    // ── 4-5. Stores ────────────────────────────────────────────────────────
    let sqlite = Arc::new(FileStore::from_config(&cfg)?);
    {
        let store = Arc::clone(&sqlite);
        tokio::task::spawn_blocking(move || store.warm_search());
    }
    let t_tantivy = std::time::Instant::now();
    let tantivy = Arc::new(TantivyStore::new(&cfg.data_dir.join("tantivy"))?);
    tracing::info!(
        "full-text index opened in {:.1}s",
        t_tantivy.elapsed().as_secs_f64()
    );
    let metrics = IndexMetrics::new();
    // A warm start does not crawl, so the time comes from the last one.
    if let Some(t) = sqlite
        .get_meta("last_crawl_time")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok())
    {
        metrics.last_crawl_secs.store(t, Ordering::Relaxed);
    }

    // ── 5b. Archive registry ───────────────────────────────────────────────
    let archive_registry: Arc<ArchiveRegistry> = {
        if cfg.archive_service.enabled {
            let zidx_dir = cfg
                .archive_service
                .zidx_dir
                .clone()
                .unwrap_or_else(|| cfg.data_dir.join("zidx"));
            std::fs::create_dir_all(&zidx_dir).map_err(AppError::Io)?;
            let cache_size = cfg.archive_service.index_cache_size;
            let dzx_dir = cfg
                .archive_service
                .dzx_dir
                .clone()
                .unwrap_or_else(|| cfg.data_dir.join("dzx"));
            let reg =
                Arc::new(ArchiveRegistry::new(zidx_dir.clone(), cache_size).with_dzx_dir(dzx_dir));
            // Warm the registry from existing .zidx files without a full re-crawl.
            if let Ok(zip_paths) = sqlite.list_archive_paths() {
                for zip_str in zip_paths {
                    let zip_path = std::path::PathBuf::from(&zip_str);
                    let zidx_path = zidx_path_for(&zidx_dir, &zip_path);
                    if zidx_path.exists() {
                        reg.register(zip_path);
                    }
                }
            }
            reg
        } else {
            Arc::new(ArchiveRegistry::disabled())
        }
    };

    // ── 6. Shared config ───────────────────────────────────────────────────
    let shared_cfg = Arc::new(RwLock::new(cfg));

    // ── 7. Crawl-in-progress guard ─────────────────────────────────────────
    // Prevents two concurrent full crawls (initial + API-triggered reindex)
    // from fighting over the SQLite write connection.
    let crawl_active = Arc::new(AtomicBool::new(false));

    // ── 8. Reindex watch channel ───────────────────────────────────────────
    let (reindex_tx, mut reindex_rx) = watch::channel(false);

    // ── 10. Shutdown signal → cancel token ────────────────────────────────
    {
        let shutdown_c = shutdown.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            shutdown_c.cancel();
        });
    }

    // ── 10b. Extensions ─────────────────────────────────────────────────────
    let mut extension_tasks = JoinSet::new();
    for ext in extensions.iter() {
        let name = ext.name().to_owned();
        let run = ext.run(shutdown.clone());
        tracing::info!("starting extension {name}");
        // An extension failing must not take file search down with it.
        extension_tasks.spawn(async move {
            if let Err(e) = run.await {
                tracing::error!("extension {name} stopped: {e:#}");
            }
        });
    }

    // ── 11. Tantivy commit loop ────────────────────────────────────────────
    {
        let tantivy_c = Arc::clone(&tantivy);
        let shutdown_c = shutdown.clone();
        let cfg_c = Arc::clone(&shared_cfg);
        tokio::spawn(async move {
            loop {
                let interval_secs = cfg_c.read().await.indexer.commit_interval_secs;
                tokio::select! {
                    _ = shutdown_c.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {
                        // A commit waits for every queued document to be
                        // indexed and then fsyncs; on a runtime worker that
                        // stalled every request (seconds on a 2-core host
                        // after a crawl queued thousands of documents).
                        // The reader reloads only on request, so searches
                        // see the commit only after the reload.
                        let t = Arc::clone(&tantivy_c);
                        match tokio::task::spawn_blocking(move || t.commit_if_changed()).await {
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => tracing::warn!("tantivy commit error: {e}"),
                            Err(e) => tracing::warn!("tantivy commit panicked: {e}"),
                        }
                    }
                }
            }
        });
    }

    // ── 11b. Periodic memory-store snapshot ────────────────────────────────
    if matches!(*sqlite, FileStore::Memory(_)) {
        let store_c = Arc::clone(&sqlite);
        let cfg_c = Arc::clone(&shared_cfg);
        let crawl_c = Arc::clone(&crawl_active);
        let shutdown_c = shutdown.clone();
        tokio::spawn(keep_snapshot_fresh(store_c, cfg_c, crawl_c, shutdown_c));
    }

    // ── 11c. Everything file lists ─────────────────────────────────────────
    tokio::spawn(keep_file_lists(
        Arc::clone(&sqlite),
        Arc::clone(&metrics),
        Arc::clone(&shared_cfg),
        Arc::clone(&crawl_active),
        shutdown.clone(),
    ));

    // ── 11d. File and folder counts for stats ──────────────────────────────
    tokio::spawn(keep_counts(
        Arc::clone(&sqlite),
        Arc::clone(&metrics),
        Arc::clone(&crawl_active),
        shutdown.clone(),
    ));

    // ── 12. Config hot-reload watcher ──────────────────────────────────────
    {
        let shutdown_c = shutdown.clone();
        let shared_cfg2 = Arc::clone(&shared_cfg);
        tokio::spawn(config::watch_config(config_path, shared_cfg2, shutdown_c));
    }

    // ── 13. Filesystem watcher → IndexEvent channel ────────────────────────
    let (ev_tx, mut ev_rx) = mpsc::channel::<IndexEvent>(1024);
    let catchup_tx = ev_tx.clone();
    {
        let shutdown_c = shutdown.clone();
        let cfg_c = Arc::clone(&shared_cfg);
        let index = Arc::clone(&sqlite);
        let reindex = reindex_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = watcher::run_watcher(cfg_c, ev_tx, index, reindex, shutdown_c).await {
                tracing::error!("watcher error: {e}");
            }
        });
    }

    // ── 14. IPC server ─────────────────────────────────────────────────────
    {
        let api_cfg = shared_cfg.read().await.api.clone();
        if api_cfg.ipc_enabled {
            let shutdown_c = shutdown.clone();
            let cfg_c = Arc::clone(&shared_cfg);
            let sqlite_c = Arc::clone(&sqlite);
            let tantivy_c = Arc::clone(&tantivy);
            let metrics_c = Arc::clone(&metrics);
            let reindex_ipc = reindex_tx.clone();
            let extensions_c = Arc::clone(&extensions);
            tokio::spawn(async move {
                run_ipc_server(
                    cfg_c,
                    sqlite_c,
                    tantivy_c,
                    metrics_c,
                    reindex_ipc,
                    extensions_c,
                    shutdown_c,
                )
                .await;
            });
        } else {
            tracing::info!("IPC server disabled");
        }
    }

    // ── 15. REST server ────────────────────────────────────────────────────
    {
        let api_cfg = shared_cfg.read().await.api.clone();
        if api_cfg.rest_enabled {
            let (sched_max_per_archive, rate_per_client, burst_per_client) = {
                let cfg = shared_cfg.read().await;
                let s = &cfg.archive_service.scheduler;
                (s.max_per_archive, s.rate_per_client, s.burst_per_client)
            };
            let scheduler_metrics = SchedulerMetrics::new();
            let archive_semaphores = ArchiveSemaphores::new(sched_max_per_archive);
            let rate_limiter = Arc::new(ClientRateLimiter::new(rate_per_client, burst_per_client));

            let app_state = AppState {
                sqlite: Arc::clone(&sqlite),
                tantivy: Arc::clone(&tantivy),
                config: Arc::clone(&shared_cfg),
                metrics: Arc::clone(&metrics),
                reindex_tx: reindex_tx.clone(),
                archive_registry: Arc::clone(&archive_registry),
                archive_semaphores,
                scheduler_metrics,
                rate_limiter,
            };
            let shutdown_c = shutdown.clone();
            let rest_port = api_cfg.rest_port;
            let rest_bind = api_cfg.rest_bind;
            let cors_origins = api_cfg.rest_cors_origins.clone();
            let auth_mode = api_cfg.auth_mode;
            tokio::spawn(async move {
                let router = crate::api::rest::router(app_state)
                    .layer(crate::api::rest::cors_layer(&cors_origins));
                let addr = std::net::SocketAddr::new(rest_bind, rest_port);
                tracing::info!("REST listening on {addr}");
                if !rest_bind.is_loopback() && auth_mode != AuthMode::All {
                    tracing::warn!(
                        "REST is reachable from the network and only file changes need a token; \
                         set api.auth_mode = \"all\""
                    );
                }
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("REST bind error: {e}");
                        return;
                    }
                };
                if let Err(e) = axum::serve(listener, router)
                    .with_graceful_shutdown(async move { shutdown_c.cancelled().await })
                    .await
                {
                    tracing::error!("REST server error: {e}");
                }
            });
        } else {
            tracing::info!("REST server disabled");
        }
    }

    // ── 16. gRPC server ────────────────────────────────────────────────────
    {
        let api_cfg = shared_cfg.read().await.api.clone();
        if api_cfg.grpc_enabled {
            let svc = FileSearchService {
                sqlite: Arc::clone(&sqlite),
                tantivy: Arc::clone(&tantivy),
                config: Arc::clone(&shared_cfg),
                metrics: Arc::clone(&metrics),
                reindex_tx: reindex_tx.clone(),
            };
            let reflection = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(grpc::proto::FILE_DESCRIPTOR_SET)
                .build()
                .expect("gRPC reflection service build");
            let shutdown_c = shutdown.clone();
            let grpc_port = api_cfg.grpc_port;
            let grpc_bind = api_cfg.grpc_bind;
            let auth_mode = api_cfg.auth_mode;
            let interceptor = crate::api::auth::grpc_interceptor(Arc::clone(&shared_cfg));
            tokio::spawn(async move {
                let addr = std::net::SocketAddr::new(grpc_bind, grpc_port);
                tracing::info!("gRPC listening on {addr}");
                if !grpc_bind.is_loopback() && auth_mode != AuthMode::All {
                    tracing::warn!(
                        "gRPC is reachable from the network without a token; \
                         set api.auth_mode = \"all\""
                    );
                }
                if let Err(e) = tonic::transport::Server::builder()
                    .add_service(grpc::service(svc, interceptor.clone()))
                    .add_service(InterceptedService::new(reflection, interceptor))
                    .serve_with_shutdown(addr, shutdown_c.cancelled())
                    .await
                {
                    tracing::error!("gRPC server error: {e}");
                }
            });
        } else {
            tracing::info!("gRPC server disabled");
        }
    }

    // ── 17a. Notify systemd the daemon is ready ────────────────────────────
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
            tracing::debug!("sd_notify ready skipped: {e}");
        }
    }

    // ── 17. Initial crawl, or catch up on changes made while stopped ───────
    // The live watcher only sees changes from now on; anything that happened
    // while the daemon was down comes from the OS change log or a timestamp
    // scan (journal.rs).
    {
        let needs_crawl = sqlite
            .get_meta("last_crawl_time")
            .map(|v| v.is_none())
            .unwrap_or(true); // on error assume uncrawled
        let crawl_active = Arc::clone(&crawl_active);
        let sqlite = Arc::clone(&sqlite);
        let tantivy = Arc::clone(&tantivy);
        let metrics = Arc::clone(&metrics);
        let registry = Arc::clone(&archive_registry);
        let cfg = Arc::clone(&shared_cfg);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let crawl = |reason: &str| {
                tracing::info!("full crawl: {reason}");
                spawn_crawl(
                    &crawl_active,
                    Arc::clone(&sqlite),
                    Arc::clone(&tantivy),
                    Arc::clone(&metrics),
                    Arc::clone(&registry),
                    Arc::clone(&cfg),
                    shutdown.clone(),
                );
            };
            let (roots, formats, exclusions) = {
                let c = cfg.read().await;
                (
                    c.watch_paths.clone(),
                    c.archive_index.formats.clone(),
                    c.exclusions.clone(),
                )
            };
            let now = {
                let roots = roots.clone();
                tokio::task::spawn_blocking(move || watcher::journal::current_position(&roots))
                    .await
                    .ok()
                    .flatten()
            };
            let stored = sqlite
                .get_meta(watcher::journal::META_KEY)
                .ok()
                .flatten()
                .and_then(|s| watcher::journal::Position::from_json(&s));

            match (needs_crawl, &now, stored) {
                (true, _, _) => crawl("index has never been built"),
                (false, None, _) => tracing::info!(
                    "index already built; no change log here, so changes made while \
                     stopped are not picked up (use the Reindex API)"
                ),
                (false, Some(_), None) => crawl("no change-log position stored yet"),
                (false, Some(now), Some(stored)) => {
                    let store = Arc::clone(&sqlite);
                    let (now, start) = (now.clone(), std::time::Instant::now());
                    let plan = tokio::task::spawn_blocking(move || {
                        let excl = match ExclusionEngine::new(&exclusions) {
                            Ok(e) => e,
                            Err(e) => {
                                return watcher::journal::CatchUp::FullCrawl(format!(
                                    "exclusions: {e}"
                                ))
                            }
                        };
                        let skip =
                            |p: &std::path::Path, hidden: bool| excl.should_exclude(p, hidden);
                        watcher::journal::catch_up(&stored, &now, &formats, &skip, &*store)
                    })
                    .await;
                    match plan {
                        Ok(watcher::journal::CatchUp::UpToDate) => {
                            tracing::info!("catch-up: no changes while stopped");
                        }
                        Ok(watcher::journal::CatchUp::Events(events)) => {
                            tracing::info!(
                                "catch-up: replaying {} changes made while stopped ({:.1}s)",
                                events.len(),
                                start.elapsed().as_secs_f64(),
                            );
                            for ev in events {
                                if catchup_tx.send(ev).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(watcher::journal::CatchUp::FullCrawl(reason)) => crawl(&reason),
                        Err(e) => crawl(&format!("catch-up failed: {e}")),
                    }
                }
            }
            if let Some(now) = now {
                keep_journal_position(sqlite, roots, now, crawl_active, shutdown).await;
            }
        });
    }

    // ── 18. Main event loop ────────────────────────────────────────────────
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            Some(event) = ev_rx.recv() => {
                // One batch at a time, in arrival order: events for one path
                // apply in the order they happened, and a batch shares one
                // write transaction. The bounded channel holds the watcher
                // back while a batch runs.
                let limit = shared_cfg.read().await.indexer.event_batch_size.max(1);
                let mut batch = vec![event];
                while batch.len() < limit {
                    match ev_rx.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }
                process_events(
                    batch,
                    Arc::clone(&shared_cfg),
                    Arc::clone(&sqlite),
                    Arc::clone(&tantivy),
                    Arc::clone(&metrics),
                    Arc::clone(&archive_registry),
                )
                .await;
            }
            Ok(()) = reindex_rx.changed() => {
                let full = *reindex_rx.borrow();
                if crawl_active
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    tracing::info!("reindex requested (full={full})");
                    spawn_crawl(
                        &crawl_active,
                        Arc::clone(&sqlite),
                        Arc::clone(&tantivy),
                        Arc::clone(&metrics),
                        Arc::clone(&archive_registry),
                        Arc::clone(&shared_cfg),
                        shutdown.clone(),
                    );
                } else {
                    tracing::info!("reindex ignored: crawl already active");
                }
            }
        }
    }

    // ── 19. Graceful shutdown ──────────────────────────────────────────────
    #[cfg(target_os = "linux")]
    {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    }
    tracing::info!("draining in-flight events (max 10s)…");
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut pending = Vec::new();
    while let Ok(event) = ev_rx.try_recv() {
        pending.push(event);
    }
    if !pending.is_empty() {
        let drained = process_events(
            pending,
            Arc::clone(&shared_cfg),
            Arc::clone(&sqlite),
            Arc::clone(&tantivy),
            Arc::clone(&metrics),
            Arc::clone(&archive_registry),
        );
        if tokio::time::timeout_at(drain_deadline, drained)
            .await
            .is_err()
        {
            tracing::warn!("in-flight events not all applied before the deadline");
        }
    }

    tracing::info!("final tantivy commit…");
    let _ = tantivy.commit();

    // Flush in-memory store snapshot on clean shutdown (no-op for SQLite).
    {
        let data_dir = shared_cfg.read().await.data_dir.clone();
        if let Err(e) = sqlite.flush_to_disk(&data_dir) {
            tracing::warn!("store flush error: {e}");
        }
    }

    // Last, so index flushing is not held up by a long extension drain.
    let drain = async { while extension_tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(EXTENSION_DRAIN, drain).await.is_err() {
        tracing::warn!("extensions did not stop within {EXTENSION_DRAIN:?}");
    }

    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&pid_path);
    }

    tracing::info!("fs daemon stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Change-log position
// ---------------------------------------------------------------------------

/// How often the stored change-log position advances. It lags by one to two
/// intervals (more if the daemon ran for less than that), so a restart may
/// replay changes that were already applied; replay is idempotent.
const JOURNAL_POSITION_INTERVAL: Duration = Duration::from_secs(60);

/// How often to check whether a running crawl has finished, so the position
/// captured before it is stored soon after (a restart within the first minutes
/// would otherwise crawl again).
const JOURNAL_CRAWL_POLL: Duration = Duration::from_secs(2);

/// Periodically stores the position captured one interval earlier. By then
/// every event before it has passed the debouncer and been applied, so the
/// stored position is safe even after a crash.
///
/// Nothing is stored while a crawl runs: changes made before the crawl
/// started (while the daemon was stopped, say) are only in the index once it
/// finishes, so an interrupted crawl must leave the old position (or none)
/// for the next start. The pending position is kept, not discarded: it was
/// captured before the crawl started, so once the crawl is done everything
/// before it has been crawled and everything after it is in the log.
async fn keep_journal_position(
    store: Arc<FileStore>,
    roots: Vec<PathBuf>,
    first: watcher::journal::Position,
    crawl_active: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    let mut prev = Some(first);
    loop {
        let wait = if crawl_active.load(Ordering::Acquire) {
            JOURNAL_CRAWL_POLL
        } else {
            JOURNAL_POSITION_INTERVAL
        };
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        if crawl_active.load(Ordering::Acquire) {
            continue;
        }
        if let Some(p) = prev.take() {
            if let Err(e) = store.set_meta(watcher::journal::META_KEY, &p.to_json()) {
                tracing::warn!("store change-log position: {e}");
            }
        }
        let roots = roots.clone();
        prev = tokio::task::spawn_blocking(move || watcher::journal::current_position(&roots))
            .await
            .ok()
            .flatten();
    }
}

/// Saves the memory store periodically so a crash does not cost a full crawl.
///
/// Skipped during a crawl: a half-crawled index is not worth the seconds the
/// save holds off the crawl's writer, and the stored change-log position only
/// advances once the crawl is done (see [`keep_journal_position`]), so a
/// snapshot taken afterwards is the first one recovery can build on.
async fn keep_snapshot_fresh(
    store: Arc<FileStore>,
    cfg: Arc<RwLock<crate::config::Config>>,
    crawl_active: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    loop {
        let (interval, data_dir) = {
            let c = cfg.read().await;
            (c.store.snapshot_interval_secs, c.data_dir.clone())
        };
        // At 0, keep polling so a hot-reloaded interval takes effect.
        let wait = Duration::from_secs(if interval == 0 { 60 } else { interval });
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        if interval == 0 || crawl_active.load(Ordering::Acquire) {
            continue;
        }
        let store = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || store.flush_if_changed(&data_dir)).await {
            Ok(Err(e)) => tracing::warn!("periodic store save: {e}"),
            Err(e) => tracing::warn!("periodic store save: {e}"),
            Ok(Ok(_)) => {}
        }
    }
}

/// Loads `[file_lists]` and keeps them current, following hot reloads. It
/// waits for the first crawl: with the store still empty, SQLite crawls
/// with its indexes dropped, and rows from a list would make it keep them.
/// Keeps the stats' file and folder counts true to the store: once at start,
/// then after watcher events. Counting is a pass over every entry, so it runs
/// at most every `EVERY` and not while a crawl keeps its own tally.
async fn keep_counts(
    store: Arc<FileStore>,
    metrics: Arc<IndexMetrics>,
    crawl_active: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    const EVERY: Duration = Duration::from_secs(10);
    let mut counted_at: Option<u64> = None;
    loop {
        let events = metrics.events_processed.load(Ordering::Relaxed);
        if counted_at != Some(events) && !crawl_active.load(Ordering::Acquire) {
            let (store, m) = (Arc::clone(&store), Arc::clone(&metrics));
            if tokio::task::spawn_blocking(move || m.recount(&store))
                .await
                .is_ok()
            {
                counted_at = Some(events);
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(EVERY) => {}
        }
    }
}

async fn keep_file_lists(
    store: Arc<FileStore>,
    metrics: Arc<IndexMetrics>,
    cfg: Arc<RwLock<crate::config::Config>>,
    crawl_active: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    // Logged once per problem, not on every poll.
    let mut unreadable: std::collections::HashMap<PathBuf, String> = Default::default();
    loop {
        let (lists, roots, data_dir, poll) = {
            let c = cfg.read().await;
            (
                c.file_lists.paths.clone(),
                c.watch_paths.clone(),
                c.data_dir.clone(),
                c.file_lists.poll_secs.max(1),
            )
        };
        let crawled = store.get_meta("last_crawl_time").ok().flatten().is_some();
        let ready = crawled && !crawl_active.load(Ordering::Acquire);
        if ready {
            let syncing = Arc::clone(&store);
            let configured = lists;
            let report = tokio::task::spawn_blocking(move || {
                crate::indexer::file_lists::sync(&syncing, &configured, &roots, &data_dir)
            })
            .await;
            match report {
                Ok(Ok(report)) => {
                    if !report.loaded.is_empty() || !report.dropped.is_empty() {
                        let (store, m) = (Arc::clone(&store), Arc::clone(&metrics));
                        let _ = tokio::task::spawn_blocking(move || m.recount(&store)).await;
                    }
                    for l in &report.loaded {
                        tracing::info!(
                            "file list {}: {} entries ({} removed, {} under watch paths left out)",
                            l.list.display(),
                            l.entries,
                            l.removed,
                            l.under_watch_paths,
                        );
                        if l.skipped > 0 {
                            tracing::warn!(
                                "file list {}: skipped {} unreadable rows; first: {}",
                                l.list.display(),
                                l.skipped,
                                l.first_problem.as_deref().unwrap_or("?"),
                            );
                        }
                    }
                    for (list, removed) in &report.dropped {
                        tracing::info!(
                            "file list {} left the config: removed {removed} entries",
                            list.display()
                        );
                    }
                    let now_unreadable: std::collections::HashMap<_, _> =
                        report.unreadable.into_iter().collect();
                    for (list, why) in &now_unreadable {
                        if unreadable.get(list) != Some(why) {
                            tracing::warn!(
                                "file list {}: {why}; its entries are kept",
                                list.display()
                            );
                        }
                    }
                    unreadable = now_unreadable;
                }
                Ok(Err(e)) => tracing::warn!("file lists: {e}"),
                Err(e) => tracing::warn!("file lists: {e}"),
            }
        }
        let wait = if ready { poll } else { 2 };
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Crawl spawner - deduplicates the "start a full crawl as a background task"
// pattern used both for the initial crawl and API-triggered reindexes.
//
// The `crawl_active` flag is set to `true` before calling this function by
// the caller (or here for the initial crawl). It is cleared to `false` when
// the spawned task finishes.
// ---------------------------------------------------------------------------

fn spawn_crawl(
    crawl_active: &Arc<AtomicBool>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
    cfg: Arc<RwLock<crate::config::Config>>,
    shutdown: CancellationToken,
) {
    // Set the flag so concurrent reindex requests are rejected until done.
    crawl_active.store(true, Ordering::Release);
    let ca = Arc::clone(crawl_active);
    tokio::spawn(async move {
        tracing::info!("full crawl starting");
        let store = Arc::clone(&sqlite);
        let bulk = tokio::task::spawn_blocking(move || store.begin_bulk_load())
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()))
            .unwrap_or_else(|e| {
                tracing::warn!("bulk load setup failed, crawling with indexes: {e}");
                false
            });
        let store = Arc::clone(&sqlite);
        let cancelled = shutdown.clone();
        if let Err(e) = full_crawl(cfg, sqlite, tantivy, metrics, registry, shutdown).await {
            tracing::error!("full_crawl error: {e}");
        } else {
            tracing::info!("full crawl complete");
            let warm = Arc::clone(&store);
            if let Err(e) = tokio::task::spawn_blocking(move || warm.warm_search()).await {
                tracing::warn!("search warm-up panicked: {e}");
            }
            if let Some(summary) = store.heap_summary() {
                tracing::info!("memory store: {summary}");
            }
        }
        // On shutdown, skip the rebuild: it can take tens of seconds, and
        // SqliteStore::with_indexes recreates missing indexes on the next start.
        if bulk && !cancelled.is_cancelled() {
            match tokio::task::spawn_blocking(move || store.end_bulk_load()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!("bulk load index rebuild failed: {e}"),
                Err(e) => tracing::error!("bulk load index rebuild panicked: {e}"),
            }
        }
        ca.store(false, Ordering::Release);
    });
}

// ---------------------------------------------------------------------------
// Cross-platform shutdown signal handler
//
// On Unix: waits for SIGTERM or SIGINT (covers systemd, kill, Ctrl-C).
// On Windows (non-service): waits for Ctrl-C; the Windows Service path
//   receives PRESHUTDOWN/STOP via the SCM handler in service.rs instead.
// ---------------------------------------------------------------------------

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut sigterm), Ok(mut sigint)) => {
                tokio::select! {
                    _ = sigterm.recv() => tracing::info!("SIGTERM received, shutting down"),
                    _ = sigint.recv()  => tracing::info!("SIGINT received, shutting down"),
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!("failed to register Unix signal handler: {e}; ctrl-c only");
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("ctrl-c received, shutting down");
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("ctrl-c received, shutting down");
    }
}

// ---------------------------------------------------------------------------
// PID file helpers (Unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn write_pid_file(path: &std::path::Path) -> Result<(), AppError> {
    use std::io::Write;
    let pid = std::process::id();
    let mut f = std::fs::File::create(path).map_err(AppError::Io)?;
    writeln!(f, "{pid}").map_err(AppError::Io)?;
    tracing::debug!("PID file written: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::instance_mutex_name;

    #[test]
    fn instance_mutex_is_per_data_folder() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let name = instance_mutex_name(a.path());
        assert!(name.starts_with(r"Global\fs_daemon_"), "{name}");
        assert!(
            !name[7..].contains('\\'),
            "only the namespace may hold a backslash"
        );
        assert_eq!(name, instance_mutex_name(a.path()));
        assert_ne!(name, instance_mutex_name(b.path()));
    }
}
