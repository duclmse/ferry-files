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
#[cfg(feature = "grpc")]
use crate::api::grpc::{self, FileSearchService};
#[cfg(feature = "ipc")]
use crate::api::ipc_server::run_ipc_server;
#[cfg(feature = "rest")]
use crate::api::rest::AppState;
use crate::config;
#[cfg(any(feature = "rest", feature = "grpc"))]
use crate::config::AuthMode;
use crate::error::AppError;
use crate::indexer::crawler::{crawl_roots, process_events, IndexEvent, IndexMetrics, Scope};
use crate::indexer::exclusions::ExclusionEngine;
#[cfg(feature = "rest")]
use crate::scheduler::{ArchiveSemaphores, ClientRateLimiter, SchedulerMetrics};
use crate::store::{tantivy_store::TantivyStore, FileStore};
use crate::watcher;
use crate::zidx::zidx_path_for;
#[cfg(feature = "grpc")]
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
pub(crate) fn instance_mutex_name(data_dir: &std::path::Path) -> String {
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
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;

    pub struct InstanceGuard(HANDLE);

    // SAFETY: the guard only closes its handle, which any thread may do; it
    // is held across awaits on the daemon's multi-threaded runtime.
    unsafe impl Send for InstanceGuard {}

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
            let handle = CreateMutexW(None, true, PCWSTR(name.as_ptr()))
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

// How long shutdown waits for a running crawl to wind down. The walk quits at
// once; a file-table read (`indexer.use_mft`) runs to its end.
const CRAWL_STOP: Duration = Duration::from_secs(20);

// Longer than the time an extension process is given to stop.
const EXTENSION_DRAIN: Duration = Duration::from_secs(35);

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run(config_path: PathBuf, opts: RunOptions) -> Result<(), AppError> {
    run_with_options(config_path, CancellationToken::new(), opts).await
}

/// How the daemon was started, beyond what the config says.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Started by the system's service manager for every user of the
    /// computer, rather than for the user who runs it.
    pub system_service: bool,
}

/// Like [`run`] but accepts an external shutdown token so tests (and
/// embedders) can cancel the daemon programmatically.
pub async fn run_with_shutdown(
    config_path: PathBuf,
    shutdown: CancellationToken,
) -> Result<(), AppError> {
    run_with_options(config_path, shutdown, RunOptions::default()).await
}

/// A server the config turns on but this build of fsd left out.
#[cfg(not(all(feature = "rest", feature = "grpc", feature = "ipc")))]
fn not_built(name: &str, feature: &str, enabled: bool, _opts: RunOptions) {
    if enabled {
        tracing::warn!(
            "{name} is enabled in the config, but this fsd was built without the `{feature}` feature"
        );
    }
}

pub async fn run_with_options(
    config_path: PathBuf,
    shutdown: CancellationToken,
    opts: RunOptions,
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
    #[cfg(windows)]
    if let Err(e) = crate::stop::listen(&cfg.data_dir, shutdown.clone()) {
        tracing::warn!("`fsd stop` will not reach this daemon: {e}");
    }

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
    crate::indexer::exclusions::log_own_dirs_under(&cfg);
    // Read once: the APIs change it (`config_edit`), nothing follows the file.
    let shared_cfg = Arc::new(RwLock::new(cfg));
    let watch_list = crate::watch_list::WatchList::with_editor(
        Arc::clone(&shared_cfg),
        crate::config_edit::ConfigEditor::new(Arc::clone(&shared_cfg), Arc::clone(&extensions)),
    );

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

    // ── 13. Filesystem watcher → IndexEvent channel ────────────────────────
    let (ev_tx, mut ev_rx) = mpsc::channel::<IndexEvent>(1024);
    let catchup_tx = ev_tx.clone();
    let crawler = Crawler {
        active: Arc::clone(&crawl_active),
        sqlite: Arc::clone(&sqlite),
        tantivy: Arc::clone(&tantivy),
        metrics: Arc::clone(&metrics),
        registry: Arc::clone(&archive_registry),
        cfg: Arc::clone(&shared_cfg),
    };
    tokio::spawn(follow_watch_paths(
        crawler.clone(),
        Arc::clone(&watch_list),
        ev_tx,
        reindex_tx.clone(),
        shutdown.clone(),
    ));

    // ── 14. IPC server ─────────────────────────────────────────────────────
    #[cfg(not(feature = "ipc"))]
    not_built("IPC", "ipc", shared_cfg.read().await.api.ipc_enabled, opts);
    #[cfg(feature = "ipc")]
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
            let watch_list_c = Arc::clone(&watch_list);
            tokio::spawn(async move {
                run_ipc_server(
                    cfg_c,
                    sqlite_c,
                    tantivy_c,
                    metrics_c,
                    reindex_ipc,
                    extensions_c,
                    watch_list_c,
                    opts.system_service,
                    shutdown_c,
                )
                .await;
            });
        } else {
            tracing::info!("IPC server disabled");
        }
    }

    // ── 15. REST server ────────────────────────────────────────────────────
    #[cfg(not(feature = "rest"))]
    not_built(
        "REST",
        "rest",
        shared_cfg.read().await.api.rest_enabled,
        opts,
    );
    #[cfg(feature = "rest")]
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
                watch_list: Arc::clone(&watch_list),
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
    #[cfg(not(feature = "grpc"))]
    not_built(
        "gRPC",
        "grpc",
        shared_cfg.read().await.api.grpc_enabled,
        opts,
    );
    #[cfg(feature = "grpc")]
    {
        let api_cfg = shared_cfg.read().await.api.clone();
        if api_cfg.grpc_enabled {
            let svc = FileSearchService {
                sqlite: Arc::clone(&sqlite),
                tantivy: Arc::clone(&tantivy),
                config: Arc::clone(&shared_cfg),
                metrics: Arc::clone(&metrics),
                reindex_tx: reindex_tx.clone(),
                watch_list: Arc::clone(&watch_list),
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
        if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
            tracing::debug!("sd_notify ready skipped: {e}");
        }
    }

    // ── 17. Initial crawl, or catch up on changes made while stopped ───────
    // The live watcher only sees changes from now on; anything that happened
    // while the daemon was down comes from the OS change log or a timestamp
    // scan (journal.rs), root by root.
    {
        let needs_crawl = sqlite
            .get_meta("last_crawl_time")
            .map(|v| v.is_none())
            .unwrap_or(true); // on error assume uncrawled
        let roots = shared_cfg.read().await.watch_paths.clone();
        for p in crate::config::missing_watch_paths(&*shared_cfg.read().await) {
            tracing::warn!("watch path {} is offline; its entries stay", p.display());
        }
        // A crawled index starts with its roots ready, as its last crawl left
        // them; a crawl or catch-up that follows moves them on.
        for root in &roots {
            let path = root.to_string_lossy();
            if !root.is_dir() {
                metrics.set_root(&path, crate::indexer::crawler::RootState::Offline, None);
            } else if !needs_crawl {
                let entries = crate::indexer::crawler::last_entries(&sqlite, &path);
                metrics.set_root(
                    &path,
                    crate::indexer::crawler::RootState::Ready,
                    Some(entries),
                );
            }
        }
        let crawl_active = Arc::clone(&crawl_active);
        let sqlite = Arc::clone(&sqlite);
        let cfg = Arc::clone(&shared_cfg);
        let crawler = crawler.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if needs_crawl {
                tracing::info!("full crawl: index has never been built");
                crawler.spawn(None, shutdown.clone());
            } else {
                let present: Vec<PathBuf> = roots.into_iter().filter(|r| r.is_dir()).collect();
                let crawl = catch_up_roots(&crawler, present, &catchup_tx).await;
                if !crawl.is_empty() {
                    crawler.spawn(Some(crawl), shutdown.clone());
                }
            }
            keep_journal_position(sqlite, cfg, crawl_active, shutdown).await;
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
                    crawler.spawn(None, shutdown.clone());
                } else {
                    tracing::info!("reindex ignored: crawl already active");
                }
            }
        }
    }

    // ── 19. Graceful shutdown ──────────────────────────────────────────────
    #[cfg(target_os = "linux")]
    {
        let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
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

    // A crawl's writer may still be adding rows; wait for it to stop (its
    // walk quits on the token), so the save below holds all it wrote and
    // nothing writes after it.
    let crawl_deadline = tokio::time::Instant::now() + CRAWL_STOP;
    while crawl_active.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= crawl_deadline {
            tracing::warn!("crawl did not stop within {CRAWL_STOP:?}; saving without it");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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

/// Periodically stores each present root's position captured one interval
/// earlier. By then every event before it has passed the debouncer and been
/// applied, so the stored position is safe even after a crash. A root that
/// is offline keeps the position it had, so it can catch up when it returns.
///
/// Nothing is stored while a crawl runs: changes made before the crawl
/// started (while the daemon was stopped, say) are only in the index once it
/// finishes, so an interrupted crawl must leave the old position (or none)
/// for the next start. The pending positions are kept, not discarded: they
/// were captured before the crawl started, so once the crawl is done
/// everything before them has been crawled and everything after is in the
/// log.
async fn keep_journal_position(
    store: Arc<FileStore>,
    cfg: Arc<RwLock<crate::config::Config>>,
    crawl_active: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    let capture = |roots: Vec<PathBuf>| async move {
        tokio::task::spawn_blocking(move || {
            roots
                .into_iter()
                .filter_map(|r| {
                    let p = watcher::journal::current_position(std::slice::from_ref(&r))?;
                    Some((r.to_string_lossy().into_owned(), p))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default()
    };
    let store_c = Arc::clone(&store);
    if let Err(e) =
        tokio::task::spawn_blocking(move || watcher::journal::split_shared(&store_c)).await
    {
        tracing::warn!("splitting the stored change-log position: {e}");
    }
    let mut pending = capture(cfg.read().await.watch_paths.clone()).await;
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
        for (root, p) in pending.drain(..) {
            if let Err(e) = watcher::journal::store_for(&store, &root, &p) {
                tracing::warn!("store change-log position of {root}: {e}");
            }
        }
        pending = capture(cfg.read().await.watch_paths.clone()).await;
    }
}

/// Catches up `roots`, which exist, each from its own stored position:
/// replays what changed while it was not watched into `tx`, and returns
/// those that need a crawl instead (no position stored, a different volume,
/// or history that cannot be replayed).
async fn catch_up_roots(
    c: &Crawler,
    roots: Vec<PathBuf>,
    tx: &mpsc::Sender<IndexEvent>,
) -> Vec<PathBuf> {
    use watcher::journal::CatchUp;
    if roots.is_empty() {
        return roots;
    }
    let (formats, exclusions) = {
        let cfg = c.cfg.read().await;
        (
            cfg.archive_index.formats.clone(),
            ExclusionEngine::for_config(&cfg),
        )
    };
    let excl = match exclusions {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("catch-up: exclusions: {e}; crawling instead");
            return roots;
        }
    };
    let store = Arc::clone(&c.sqlite);
    let start = std::time::Instant::now();
    let plan = tokio::task::spawn_blocking(move || {
        let skip = |p: &std::path::Path, hidden: bool| excl.should_exclude(p, hidden);
        let (mut events, mut crawl) = (Vec::new(), Vec::new());
        for root in roots {
            let path = root.to_string_lossy().into_owned();
            let Some(now) = watcher::journal::current_position(std::slice::from_ref(&root)) else {
                tracing::info!(
                    "{path}: no change log here, so changes made while it was not \
                     watched are not picked up (use the Reindex API)"
                );
                continue;
            };
            let Some(stored) = watcher::journal::stored_for(&store, &path) else {
                tracing::info!("{path}: no change-log position stored; crawling it");
                crawl.push(root);
                continue;
            };
            match watcher::journal::catch_up(&stored, &now, &formats, &skip, &*store) {
                CatchUp::UpToDate => {}
                CatchUp::Events(found) => events.extend(found),
                CatchUp::FullCrawl(why) => {
                    tracing::info!("{path}: {why}; crawling it");
                    crawl.push(root);
                }
            }
        }
        (events, crawl)
    })
    .await;
    let (events, crawl) = match plan {
        Ok(plan) => plan,
        Err(e) => {
            tracing::warn!("catch-up failed: {e}");
            return Vec::new();
        }
    };
    if events.is_empty() {
        tracing::info!("catch-up: no changes while not watched");
    } else {
        tracing::info!(
            "catch-up: replaying {} changes made while not watched ({:.1}s)",
            events.len(),
            start.elapsed().as_secs_f64(),
        );
    }
    for ev in events {
        if tx.send(ev).await.is_err() {
            break;
        }
    }
    crawl
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
        // At 0, keep polling so a changed interval takes effect.
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

/// Loads `[file_lists]` and keeps them current, following config changes. It
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
// Watch paths
// ---------------------------------------------------------------------------

/// How often the watch paths are compared with the config, which a config change
/// replaces.
const WATCH_PATHS_POLL: Duration = Duration::from_secs(1);

/// What a crawl needs.
#[derive(Clone)]
struct Crawler {
    active: Arc<AtomicBool>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
    cfg: Arc<RwLock<crate::config::Config>>,
}

/// Runs the watcher over the config's watch paths (none when there are no
/// paths), and starts it again when they change, crawling the added ones.
/// The rows of a removed path stay, unless it was removed with `forget`.
///
/// The new watcher starts before the old one stops, and the old one gets a
/// debounce window and more to deliver what it holds: changes in between
/// are seen twice, which applying them tolerates, rather than not at all.
async fn follow_watch_paths(
    crawler: Crawler,
    watch_list: Arc<crate::watch_list::WatchList>,
    tx: mpsc::Sender<IndexEvent>,
    reindex: watch::Sender<bool>,
    shutdown: CancellationToken,
) {
    use crate::indexer::crawler::RootState;
    let start = |roots: &[PathBuf]| -> Option<CancellationToken> {
        if roots.is_empty() {
            tracing::info!("no watch path to follow: nothing is watched");
            return None;
        }
        let stop = shutdown.child_token();
        let (cfg, roots, tx, index, reindex, token) = (
            Arc::clone(&crawler.cfg),
            roots.to_vec(),
            tx.clone(),
            Arc::clone(&crawler.sqlite),
            reindex.clone(),
            stop.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = watcher::run_watcher(cfg, roots, tx, index, reindex, token).await {
                tracing::error!("watcher error: {e}");
            }
        });
        Some(stop)
    };
    // Polled roots get no watcher: a timestamp scan every `poll_secs` finds
    // their changes instead.
    let watched = |online: &[PathBuf], polled: &[(PathBuf, u64)]| -> Vec<PathBuf> {
        online
            .iter()
            .filter(|r| !polled.iter().any(|(p, _)| p == *r))
            .cloned()
            .collect()
    };
    let (mut roots, polled) = {
        let c = crawler.cfg.read().await;
        (c.watch_paths.clone(), c.polled())
    };
    let mut online = present(&roots, &roots).await;
    let mut following = watched(&online, &polled);
    let mut running = start(&following);
    let mut polls = Polls::default();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(WATCH_PATHS_POLL) => {}
        }
        let (now, debounce_ms, polled) = {
            let c = crawler.cfg.read().await;
            (c.watch_paths.clone(), c.watcher.debounce_ms, c.polled())
        };
        let available = present(&now, &online).await;
        polls.run(&crawler, &polled, &available, &tx, &shutdown);
        let follow = watched(&available, &polled);
        if now == roots && available == online && follow == following {
            continue;
        }
        let added: Vec<PathBuf> = now.iter().filter(|p| !roots.contains(p)).cloned().collect();
        let removed: Vec<PathBuf> = roots.iter().filter(|p| !now.contains(p)).cloned().collect();
        // Watch paths an added one took in: watched live until now, so their
        // entries are current and its crawl need not walk them again.
        let keep: Vec<PathBuf> = removed
            .iter()
            .filter(|r| following.contains(r) && added.iter().any(|a| r.starts_with(a)))
            .cloned()
            .collect();
        // Listed before and after, but not reachable before and after.
        let back: Vec<PathBuf> = available
            .iter()
            .filter(|p| roots.contains(p) && !online.contains(p))
            .cloned()
            .collect();
        let gone: Vec<PathBuf> = now
            .iter()
            .filter(|p| !available.contains(p) && (online.contains(p) || added.contains(p)))
            .cloned()
            .collect();
        if !added.is_empty() || !removed.is_empty() {
            tracing::info!(
                "watch_paths changed: {} added, {} removed",
                added.len(),
                removed.len()
            );
            crawler
                .metrics
                .notify(crate::indexer::crawler::Notice::WatchPaths {
                    roots: now
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect(),
                });
        }
        crawler.metrics.keep_roots(&now);
        for root in &gone {
            tracing::warn!("watch path {} is offline; its entries stay", root.display());
            crawler
                .metrics
                .set_root(&root.to_string_lossy(), RootState::Offline, None);
        }
        for root in &back {
            tracing::info!("watch path {} is back; catching it up", root.display());
        }
        let added: Vec<PathBuf> = added
            .into_iter()
            .filter(|p| available.contains(p))
            .collect();
        for root in &added {
            crawler
                .metrics
                .set_root(&root.to_string_lossy(), RootState::Pending, None);
        }
        let forget: Vec<PathBuf> = watch_list
            .take_forgotten()
            .into_iter()
            .filter(|p| removed.contains(p))
            .collect();
        roots = now;
        online = available;
        if follow != following {
            following = follow;
            if let Some(old) = std::mem::replace(&mut running, start(&following)) {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(2 * debounce_ms + 500)).await;
                    old.cancel();
                });
            }
        }
        let mut crawl = added;
        if !back.is_empty() {
            // Started above, the watcher already follows these roots, so what
            // changes while they catch up is not lost.
            let again = catch_up_roots(&crawler, back.clone(), &tx).await;
            for root in back.iter().filter(|r| !again.contains(r)) {
                let path = root.to_string_lossy();
                let entries = crate::indexer::crawler::last_entries(&crawler.sqlite, &path);
                crawler
                    .metrics
                    .set_root(&path, RootState::Ready, Some(entries));
            }
            crawl.extend(again);
        }
        if crawl.is_empty() && forget.is_empty() {
            continue;
        }
        // One crawl or removal at a time. A full crawl already running may
        // have read the paths before the change, so the added ones are
        // crawled after it.
        while crawler
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(JOURNAL_CRAWL_POLL) => {}
            }
        }
        if !forget.is_empty() {
            let (c, kept) = (crawler.clone(), roots.clone());
            match tokio::task::spawn_blocking(move || forget_roots(&c, &forget, &kept)).await {
                Ok(again) => crawl.extend(again.into_iter().filter(|r| online.contains(r))),
                Err(e) => tracing::warn!("forgetting removed watch paths failed: {e}"),
            }
        }
        if crawl.is_empty() {
            crawler.active.store(false, Ordering::Release);
        } else {
            crawler.spawn_keeping(crawl, keep, shutdown.clone());
        }
    }
}

/// When each polled root was last scanned, from when, and whether a scan of
/// it is running.
#[derive(Default)]
struct Polls {
    roots: std::collections::HashMap<PathBuf, Poll>,
}

struct Poll {
    last: std::time::Instant,
    /// Unix seconds the next scan looks for changes after.
    since: u64,
    busy: Arc<AtomicBool>,
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Polls {
    /// Starts a scan of each polled root that is online, due and not
    /// already being scanned. A root starts its clock when first seen:
    /// catch-up or its crawl covered what came before.
    fn run(
        &mut self,
        c: &Crawler,
        polled: &[(PathBuf, u64)],
        online: &[PathBuf],
        tx: &mpsc::Sender<IndexEvent>,
        shutdown: &CancellationToken,
    ) {
        self.roots.retain(|r, _| polled.iter().any(|(p, _)| p == r));
        for (root, secs) in polled.iter().filter(|(r, _)| online.contains(r)) {
            let poll = self.roots.entry(root.clone()).or_insert_with(|| Poll {
                last: std::time::Instant::now(),
                since: unix_secs(),
                busy: Arc::new(AtomicBool::new(false)),
            });
            if poll.last.elapsed() < Duration::from_secs(*secs)
                || poll.busy.swap(true, Ordering::AcqRel)
            {
                continue;
            }
            // A second's overlap: a change in the second the last scan
            // started may not have shown in it.
            let since = std::mem::replace(&mut poll.since, unix_secs().saturating_sub(1));
            poll.last = std::time::Instant::now();
            let (c, root, tx, busy, shutdown) = (
                c.clone(),
                root.clone(),
                tx.clone(),
                Arc::clone(&poll.busy),
                shutdown.clone(),
            );
            tokio::spawn(async move {
                poll_root(&c, &root, since, &tx, &shutdown).await;
                busy.store(false, Ordering::Release);
            });
        }
    }
}

/// One scan of a polled root for what changed since `since`.
async fn poll_root(
    c: &Crawler,
    root: &std::path::Path,
    since: u64,
    tx: &mpsc::Sender<IndexEvent>,
    shutdown: &CancellationToken,
) {
    use watcher::journal::CatchUp;
    let (formats, exclusions) = {
        let cfg = c.cfg.read().await;
        (
            cfg.archive_index.formats.clone(),
            ExclusionEngine::for_config(&cfg),
        )
    };
    let excl = match exclusions {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("poll {}: exclusions: {e}", root.display());
            return;
        }
    };
    let (store, scan_root, start) = (
        Arc::clone(&c.sqlite),
        root.to_path_buf(),
        std::time::Instant::now(),
    );
    let plan = tokio::task::spawn_blocking(move || {
        let skip = |p: &std::path::Path, hidden: bool| excl.should_exclude(p, hidden);
        watcher::journal::changes_since(&[scan_root], since, &formats, &skip, &*store)
    })
    .await;
    match plan {
        Ok(CatchUp::UpToDate) => {}
        Ok(CatchUp::Events(found)) => {
            tracing::info!(
                "poll {}: {} changes ({:.1}s)",
                root.display(),
                found.len(),
                start.elapsed().as_secs_f64()
            );
            for ev in found {
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
        }
        Ok(CatchUp::FullCrawl(why)) => {
            tracing::info!("poll {}: {why}; crawling it", root.display());
            while c
                .active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(JOURNAL_CRAWL_POLL) => {}
                }
            }
            c.spawn(Some(vec![root.to_path_buf()]), shutdown.clone());
        }
        Err(e) => tracing::warn!("poll {} failed: {e}", root.display()),
    }
}

/// How long a check that the watch paths exist may take. An unreachable
/// network share can block for tens of seconds.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `roots` that exist now, as folders. When the check takes too long,
/// the answer is `last`, what was known before, limited to `roots`.
async fn present(roots: &[PathBuf], last: &[PathBuf]) -> Vec<PathBuf> {
    let check = roots.to_vec();
    let found = tokio::time::timeout(
        PRESENCE_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            check.into_iter().filter(|r| r.is_dir()).collect::<Vec<_>>()
        }),
    )
    .await;
    match found {
        Ok(Ok(found)) => found,
        _ => roots.iter().filter(|r| last.contains(r)).cloned().collect(),
    }
}

/// Removes the entries of `gone`, removed watch paths, from both stores and
/// the archive registry. A removed path inside a kept one keeps its entries,
/// which the kept one still covers. Returns the kept paths inside a removed
/// one, which lost their entries with it and need crawling again. Blocking.
fn forget_roots(c: &Crawler, gone: &[PathBuf], kept: &[PathBuf]) -> Vec<PathBuf> {
    let mut again = Vec::new();
    for root in gone {
        if let Some(outer) = kept.iter().find(|k| root.starts_with(k)) {
            tracing::info!(
                "{} is inside watch path {}; its entries stay",
                root.display(),
                outer.display()
            );
            continue;
        }
        let start = std::time::Instant::now();
        let path = root.to_string_lossy();
        let removed = c.sqlite.remove_directory(&path).unwrap_or_else(|e| {
            tracing::warn!("forget {path}: {e}");
            0
        });
        if let Err(e) = c.sqlite.remove_file(&path) {
            tracing::warn!("forget {path}: {e}");
        }
        if let Err(e) = c.tantivy.remove_under(&path) {
            tracing::warn!("forget {path} (full text): {e}");
        }
        c.registry.unregister_under(root);
        crate::indexer::crawler::forget_root_meta(&c.sqlite, &path);
        tracing::info!(
            "forgot {path}: {removed} entries ({:.1}s)",
            start.elapsed().as_secs_f64()
        );
        again.extend(kept.iter().filter(|k| k.starts_with(root)).cloned());
    }
    c.metrics.recount(&c.sqlite);
    again
}

// ---------------------------------------------------------------------------
// Crawl spawner - deduplicates the "start a full crawl as a background task"
// pattern used both for the initial crawl and API-triggered reindexes.
//
// The `crawl_active` flag is set to `true` before calling this function by
// the caller (or here for the initial crawl). It is cleared to `false` when
// the spawned task finishes.
// ---------------------------------------------------------------------------

impl Crawler {
    /// Crawls `roots`, or every watch path, in the background.
    fn spawn(&self, roots: Option<Vec<PathBuf>>, shutdown: CancellationToken) {
        let scope = Scope {
            roots,
            keep: Vec::new(),
        };
        spawn_crawl(self.clone(), scope, shutdown);
    }

    /// `spawn`, keeping the entries of `keep` instead of walking them
    /// (`Scope::keep`).
    fn spawn_keeping(&self, roots: Vec<PathBuf>, keep: Vec<PathBuf>, shutdown: CancellationToken) {
        let scope = Scope {
            roots: Some(roots),
            keep,
        };
        spawn_crawl(self.clone(), scope, shutdown);
    }
}

fn spawn_crawl(c: Crawler, scope: Scope, shutdown: CancellationToken) {
    let roots = scope.roots.clone();
    let Crawler {
        active: ca,
        sqlite,
        tantivy,
        metrics,
        registry,
        cfg,
    } = c;
    // Set the flag so concurrent reindex requests are rejected until done.
    ca.store(true, Ordering::Release);
    tokio::spawn(async move {
        let what = match &roots {
            None => "full crawl".to_owned(),
            Some(r) => format!(
                "crawl of {}",
                r.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        tracing::info!("{what} starting");
        let store = Arc::clone(&sqlite);
        // Bulk load drops the indexes, which only pays when most rows are new.
        let whole = roots.is_none();
        let bulk = tokio::task::spawn_blocking(move || -> crate::error::Result<bool> {
            Ok(whole && store.begin_bulk_load()?)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
        .unwrap_or_else(|e| {
            tracing::warn!("bulk load setup failed, crawling with indexes: {e}");
            false
        });
        let store = Arc::clone(&sqlite);
        let cancelled = shutdown.clone();
        if let Err(e) = crawl_roots(cfg, scope, sqlite, tantivy, metrics, registry, shutdown).await
        {
            tracing::error!("{what} error: {e}");
        } else if cancelled.is_cancelled() {
            tracing::info!("{what} stopped for shutdown");
        } else {
            tracing::info!("{what} complete");
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
    #[cfg(windows)]
    {
        // Closing the console window gives the process about 5 s before
        // Windows ends it: enough to save the index, which a kill loses.
        use tokio::signal::windows::{ctrl_break, ctrl_close};
        match (ctrl_break(), ctrl_close()) {
            (Ok(mut brk), Ok(mut close)) => {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => {
                        r.ok();
                        tracing::info!("ctrl-c received, shutting down");
                    }
                    _ = brk.recv() => tracing::info!("ctrl-break received, shutting down"),
                    _ = close.recv() => tracing::info!("console closed, shutting down"),
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::error!("failed to register console handlers: {e}; ctrl-c only");
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("ctrl-c received, shutting down");
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
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
