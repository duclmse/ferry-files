//! Daemon lifecycle — startup, API server orchestration, graceful shutdown.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::sync::{mpsc, watch, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::api::archive::ArchiveRegistry;
use crate::api::grpc::{self, FileSearchService};
use crate::api::ipc_server::run_ipc_server;
use crate::api::rest::AppState;
use crate::config;
use crate::error::AppError;
use crate::indexer::crawler::{full_crawl, process_event, IndexEvent, IndexMetrics};
use crate::store::{FileStore, tantivy_store::TantivyStore};
use crate::watcher;
use crate::zidx::zidx_path_for;

// ---------------------------------------------------------------------------
// Windows single-instance guard via named mutex
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win_guard {
    use super::AppError;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    pub struct InstanceGuard(HANDLE);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            unsafe { let _ = CloseHandle(self.0); }
        }
    }

    pub fn acquire() -> Result<InstanceGuard, AppError> {
        let name: Vec<u16> = "Global\\filesearch_daemon\0".encode_utf16().collect();
        unsafe {
            let handle = CreateMutexW(None, BOOL(1), PCWSTR(name.as_ptr()))
                .map_err(|e| AppError::Config(format!("CreateMutexW: {e}")))?;
            // A valid handle with ERROR_ALREADY_EXISTS means another instance owns the mutex.
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return Err(AppError::Config("filesearch daemon is already running".into()));
            }
            Ok(InstanceGuard(handle))
        }
    }
}

// Maximum concurrent filesystem-event processing tasks.
// Beyond this, new events wait for a running task to finish rather than
// spawning unboundedly. Keeps memory and SQLite-pool contention bounded
// under inotify/FSEvents bursts (e.g. large `git checkout`).
const MAX_EVENT_TASKS: usize = 16;

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
    shutdown:    CancellationToken,
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
    let file_appender = tracing_appender::rolling::daily(&cfg.log_dir, "filesearch.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    // Ignore error when already initialised (happens in integration tests that
    // run multiple daemon instances within the same process).
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .try_init();

    tracing::info!("filesearch daemon starting");

    // ── 3. Instance guard: PID file (Unix) / named mutex (Windows) ────────
    #[cfg(unix)]
    let pid_path = cfg.data_dir.join("filesearch.pid");
    #[cfg(unix)]
    write_pid_file(&pid_path)?;
    #[cfg(windows)]
    let _instance_guard = win_guard::acquire()?;

    // ── 4-5. Stores ────────────────────────────────────────────────────────
    tracing::info!("store mode: {:?}", cfg.store.mode);
    let sqlite  = Arc::new(FileStore::from_config(&cfg)?);
    let tantivy = Arc::new(TantivyStore::new(&cfg.data_dir.join("tantivy"))?);
    let metrics = IndexMetrics::new();

    // ── 5b. Archive registry ───────────────────────────────────────────────
    let archive_registry: Arc<ArchiveRegistry> = {
        if cfg.archive_service.enabled {
            let zidx_dir = cfg.archive_service.zidx_dir.clone()
                .unwrap_or_else(|| cfg.data_dir.join("zidx"));
            std::fs::create_dir_all(&zidx_dir).map_err(AppError::Io)?;
            let cache_size = cfg.archive_service.index_cache_size;
            let reg = Arc::new(ArchiveRegistry::new(zidx_dir.clone(), cache_size));
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

    // ── 9. Event-task semaphore ────────────────────────────────────────────
    // Caps the number of concurrently running process_event tasks to
    // MAX_EVENT_TASKS. Provides backpressure during watcher bursts without
    // spawning an unbounded number of tokio tasks.
    let event_sem = Arc::new(Semaphore::new(MAX_EVENT_TASKS));

    // ── 10. Shutdown signal → cancel token ────────────────────────────────
    {
        let shutdown_c = shutdown.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            shutdown_c.cancel();
        });
    }

    // ── 11. Tantivy commit loop ────────────────────────────────────────────
    {
        let tantivy_c  = Arc::clone(&tantivy);
        let shutdown_c = shutdown.clone();
        let cfg_c      = Arc::clone(&shared_cfg);
        tokio::spawn(async move {
            loop {
                let interval_secs = cfg_c.read().await.indexer.commit_interval_secs;
                tokio::select! {
                    _ = shutdown_c.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {
                        if let Err(e) = tantivy_c.commit() {
                            tracing::warn!("tantivy commit error: {e}");
                        }
                    }
                }
            }
        });
    }

    // ── 12. Config hot-reload watcher ──────────────────────────────────────
    {
        let shutdown_c  = shutdown.clone();
        let shared_cfg2 = Arc::clone(&shared_cfg);
        tokio::spawn(config::watch_config(config_path, shared_cfg2, shutdown_c));
    }

    // ── 13. Filesystem watcher → IndexEvent channel ────────────────────────
    let (ev_tx, mut ev_rx) = mpsc::channel::<IndexEvent>(1024);
    {
        let shutdown_c = shutdown.clone();
        let cfg_c      = Arc::clone(&shared_cfg);
        tokio::spawn(async move {
            if let Err(e) = watcher::run_watcher(cfg_c, ev_tx, shutdown_c).await {
                tracing::error!("watcher error: {e}");
            }
        });
    }

    // ── 14. IPC server ─────────────────────────────────────────────────────
    {
        let api_cfg = shared_cfg.read().await.api.clone();
        if api_cfg.ipc_enabled {
            let shutdown_c  = shutdown.clone();
            let cfg_c       = Arc::clone(&shared_cfg);
            let sqlite_c    = Arc::clone(&sqlite);
            let tantivy_c   = Arc::clone(&tantivy);
            let metrics_c   = Arc::clone(&metrics);
            let reindex_ipc = reindex_tx.clone();
            tokio::spawn(async move {
                run_ipc_server(cfg_c, sqlite_c, tantivy_c, metrics_c, reindex_ipc, shutdown_c).await;
            });
        } else {
            tracing::info!("IPC server disabled");
        }
    }

    // ── 15. REST server ────────────────────────────────────────────────────
    {
        let api_cfg = shared_cfg.read().await.api.clone();
        if api_cfg.rest_enabled {
            let app_state = AppState {
                sqlite:           Arc::clone(&sqlite),
                tantivy:          Arc::clone(&tantivy),
                config:           Arc::clone(&shared_cfg),
                metrics:          Arc::clone(&metrics),
                reindex_tx:       reindex_tx.clone(),
                archive_registry: Arc::clone(&archive_registry),
            };
            let shutdown_c = shutdown.clone();
            let rest_port  = api_cfg.rest_port;
            tokio::spawn(async move {
                let router = crate::api::rest::router(app_state);
                let addr   = std::net::SocketAddr::from(([0, 0, 0, 0], rest_port));
                tracing::info!("REST listening on {addr}");
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => { tracing::error!("REST bind error: {e}"); return; }
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
                sqlite:     Arc::clone(&sqlite),
                tantivy:    Arc::clone(&tantivy),
                config:     Arc::clone(&shared_cfg),
                metrics:    Arc::clone(&metrics),
                reindex_tx: reindex_tx.clone(),
            };
            let reflection = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(grpc::proto::FILE_DESCRIPTOR_SET)
                .build()
                .expect("gRPC reflection service build");
            let shutdown_c = shutdown.clone();
            let grpc_port  = api_cfg.grpc_port;
            tokio::spawn(async move {
                let addr = std::net::SocketAddr::from(([0, 0, 0, 0], grpc_port));
                tracing::info!("gRPC listening on {addr}");
                if let Err(e) = tonic::transport::Server::builder()
                    .add_service(grpc::service(svc))
                    .add_service(reflection)
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

    // ── 17. Initial full crawl (only if the index has never been built) ───────
    // If last_crawl_time is present the index is warm; the watcher covers
    // changes that happened during downtime via the startup backlog.
    let needs_crawl = sqlite.get_meta("last_crawl_time")
        .map(|v| v.is_none())
        .unwrap_or(true); // on error assume uncrawled
    if needs_crawl {
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
        tracing::info!("index already built, skipping initial crawl (use Reindex API to force)");
    }

    // ── 18. Main event loop ────────────────────────────────────────────────
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            Some(event) = ev_rx.recv() => {
                // Acquire a semaphore permit before spawning. When MAX_EVENT_TASKS
                // are already running this yields here (backpressure) rather than
                // allocating another task.
                let permit    = Arc::clone(&event_sem).acquire_owned().await.unwrap();
                let sqlite_e  = Arc::clone(&sqlite);
                let tantivy_e = Arc::clone(&tantivy);
                let metrics_e = Arc::clone(&metrics);
                let cfg_e     = Arc::clone(&shared_cfg);
                let reg_e     = Arc::clone(&archive_registry);
                tokio::spawn(async move {
                    process_event(event, cfg_e, sqlite_e, tantivy_e, metrics_e, reg_e).await;
                    drop(permit); // release when done
                });
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
    while tokio::time::Instant::now() < drain_deadline {
        match ev_rx.try_recv() {
            Ok(event) => {
                let sqlite_e  = Arc::clone(&sqlite);
                let tantivy_e = Arc::clone(&tantivy);
                let metrics_e = Arc::clone(&metrics);
                let cfg_e     = Arc::clone(&shared_cfg);
                let reg_e     = Arc::clone(&archive_registry);
                process_event(event, cfg_e, sqlite_e, tantivy_e, metrics_e, reg_e).await;
            }
            Err(_) => break,
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

    #[cfg(unix)]
    { let _ = std::fs::remove_file(&pid_path); }

    tracing::info!("filesearch daemon stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Crawl spawner — deduplicates the "start a full crawl as a background task"
// pattern used both for the initial crawl and API-triggered reindexes.
//
// The `crawl_active` flag is set to `true` before calling this function by
// the caller (or here for the initial crawl). It is cleared to `false` when
// the spawned task finishes.
// ---------------------------------------------------------------------------

fn spawn_crawl(
    crawl_active: &Arc<AtomicBool>,
    sqlite:       Arc<FileStore>,
    tantivy:      Arc<TantivyStore>,
    metrics:      Arc<IndexMetrics>,
    registry:     Arc<ArchiveRegistry>,
    cfg:          Arc<RwLock<crate::config::Config>>,
    shutdown:     CancellationToken,
) {
    // Set the flag so concurrent reindex requests are rejected until done.
    crawl_active.store(true, Ordering::Release);
    let ca = Arc::clone(crawl_active);
    tokio::spawn(async move {
        tracing::info!("full crawl starting");
        if let Err(e) = full_crawl(cfg, sqlite, tantivy, metrics, registry, shutdown).await {
            tracing::error!("full_crawl error: {e}");
        } else {
            tracing::info!("full crawl complete");
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
        use tokio::signal::unix::{SignalKind, signal};
        match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
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
