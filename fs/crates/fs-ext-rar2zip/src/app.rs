use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::alerts::{notify, AlertLevel, AlertPayload};
use crate::config::Config;
use crate::convert::convert;
use crate::error::{AppError, Result};
use crate::intake::IntakeProcessor;
use crate::metrics::Metrics;
use crate::queue::{Job, JobQueue, Status, Strategy};
use crate::transfer::{stream_convert_and_transfer, transfer_all};
use crate::watcher::run_watcher;

// ---------------------------------------------------------------------------

pub async fn run(
    config: Config,
    config_path: std::path::PathBuf,
    shutdown: CancellationToken,
) -> Result<()> {
    // Open job queue; DB lives beside the watch folder
    let db_path = config.watch_folder.join(".rar2zip.db");
    let queue = Arc::new(JobQueue::new(&db_path)?);

    // Recover jobs that were in-flight when the process last died
    recover_stale_jobs(&queue)?;

    // Expire any multipart groups whose assembly window lapsed while we were down
    expire_multipart(&queue, &config);

    // Task tracker - lets us wait for all spawned process_job tasks to finish
    // before fully exiting (up to 30 s).
    let tracker = TaskTracker::new();

    // Shared mutable config (for hot-reload)
    let cfg_lock: Arc<RwLock<Config>> = Arc::new(RwLock::new(config.clone()));

    // Shared state
    let metrics = Metrics::new();
    let sem = Arc::new(Semaphore::new(config.transfer.max_concurrent));

    // Config hot-reload watcher (best-effort; errors are logged inside)
    {
        let cfg_arc = Arc::clone(&cfg_lock);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            crate::config::watch_config(config_path, cfg_arc, shutdown).await;
        });
    }

    // Metrics HTTP server in a background thread
    let metrics_flag = Arc::new(AtomicBool::new(false));
    {
        let m = Arc::clone(&metrics);
        let q = Arc::clone(&queue);
        let flag = Arc::clone(&metrics_flag);
        let bind = config.observability.bind;
        let port = config.observability.metrics_port;
        std::thread::spawn(move || crate::metrics::serve(bind, port, m, q, flag));
    }

    // Dashboard HTTP server in a separate background thread
    {
        let m = Arc::clone(&metrics);
        let q = Arc::clone(&queue);
        let flag = Arc::new(AtomicBool::new(false)); // shares lifecycle with metrics_flag below
        let bind = config.observability.bind;
        let port = config.observability.dashboard_port;
        // Reuse the same serve() function on the dashboard port; /ui handles the HTML
        std::thread::spawn(move || crate::metrics::serve(bind, port, m, q, flag));
    }

    // Watcher → intake channel
    let (watcher_tx, mut watcher_rx) = tokio::sync::mpsc::channel::<std::path::PathBuf>(256);
    let intake = Arc::new(IntakeProcessor::new(
        Arc::clone(&queue),
        Arc::clone(&cfg_lock),
    ));

    // Spawn filesystem watcher
    {
        let cfg = Arc::clone(&cfg_lock);
        let tx = watcher_tx.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = run_watcher(cfg, tx, shutdown).await {
                tracing::error!("watcher error: {e}");
            }
        });
    }

    // Multipart expiry task - runs once per hour
    {
        let q = Arc::clone(&queue);
        let cfg = config.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = interval.tick() => expire_multipart(&q, &cfg),
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }

    // Queue-depth alert task - runs every 60 s
    {
        let q = Arc::clone(&queue);
        let cfg = config.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let threshold = cfg.observability.alert_queue_depth_threshold;
            let mut last = Instant::now() - Duration::from_secs(300); // allow immediate first fire
            let mut iv = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = iv.tick() => {
                        if threshold == 0 { continue; }
                        let depth: i64 = q.queue_depth()
                            .map(|m| m.get("pending").copied().unwrap_or(0))
                            .unwrap_or(0);
                        if depth >= threshold as i64 && last.elapsed() >= Duration::from_secs(300) {
                            last = Instant::now();
                            notify(
                                AlertPayload {
                                    job_id:  "queue".into(),
                                    message: format!("queue depth {depth} exceeds threshold {threshold}"),
                                    level:   AlertLevel::Warning,
                                },
                                &cfg.observability,
                                &cfg.smtp,
                            ).await;
                        }
                    }
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }

    // Job processor tick
    let mut drain_iv = tokio::time::interval(Duration::from_secs(5));

    tracing::info!("rar2zip running");

    loop {
        tokio::select! {
            // New file detected by the watcher
            path = watcher_rx.recv() => {
                if let Some(p) = path {
                    let intake = Arc::clone(&intake);
                    tracker.spawn(async move {
                        if let Err(e) = intake.process(p.clone()).await {
                            tracing::error!("intake error for {}: {e}", p.display());
                        }
                    });
                }
            }

            // Periodic drain of the pending queue (handles startup backlog and
            // jobs that were enqueued while the processor was busy)
            _ = drain_iv.tick() => {
                drain_pending(
                    Arc::clone(&queue),
                    Arc::clone(&cfg_lock),
                    Arc::clone(&metrics),
                    Arc::clone(&sem),
                    shutdown.clone(),
                    &tracker,
                )
                .await;
            }

            _ = shutdown.cancelled() => {
                tracing::info!("shutdown signal received, stopping");
                break;
            }
        }
    }

    // Close the tracker so no new tasks can be spawned, then wait up to 30 s
    // for in-flight process_job tasks to finish before tearing down stores.
    tracker.close();
    match tokio::time::timeout(Duration::from_secs(30), tracker.wait()).await {
        Ok(()) => {}
        Err(_) => tracing::warn!("graceful shutdown timed out; some jobs may not have completed"),
    }

    // Signal the metrics thread to stop
    metrics_flag.store(true, Ordering::Relaxed);
    Ok(())
}

// ---------------------------------------------------------------------------
// Stale-job recovery
// ---------------------------------------------------------------------------

fn recover_stale_jobs(queue: &JobQueue) -> Result<()> {
    let stale = queue.get_stale_jobs()?;
    if !stale.is_empty() {
        tracing::warn!("recovering {} stale job(s)", stale.len());
        for job in stale {
            queue.set_status(&job.id, Status::Pending, None)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pending-queue drain
// ---------------------------------------------------------------------------

async fn drain_pending(
    queue: Arc<JobQueue>,
    cfg_lock: Arc<RwLock<Config>>,
    metrics: Arc<Metrics>,
    sem: Arc<Semaphore>,
    shutdown: CancellationToken,
    tracker: &TaskTracker,
) {
    let config = cfg_lock.read().await.clone();
    let pending = match queue.get_pending_jobs(32) {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!("get_pending_jobs: {e}");
            return;
        }
    };

    for job in pending {
        if shutdown.is_cancelled() {
            break;
        }
        let q = Arc::clone(&queue);
        let m = Arc::clone(&metrics);
        let s = Arc::clone(&sem);
        let c = config.clone();
        let sd = shutdown.clone();
        tracker.spawn(async move {
            if let Err(e) = process_job(job, q, c, m, s, sd).await {
                tracing::error!("job processing error: {e}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Single-job processing
// ---------------------------------------------------------------------------

async fn process_job(
    job: Job,
    queue: Arc<JobQueue>,
    config: Config,
    metrics: Arc<Metrics>,
    sem: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()> {
    let id = job.id.clone();

    match job.strategy {
        Strategy::Disk => process_disk_job(job, &id, queue, config, metrics, sem, shutdown).await,
        Strategy::Stream => process_stream_job(job, &id, queue, config, metrics, shutdown).await,
    }
}

async fn process_disk_job(
    job: Job,
    id: &str,
    queue: Arc<JobQueue>,
    config: Config,
    metrics: Arc<Metrics>,
    sem: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()> {
    // --- Convert ---
    queue.set_status(id, Status::Converting, None)?;

    let zip_path = match convert(&job, &config.conversion).await {
        Ok(p) => p,
        Err(e) => {
            metrics
                .conversion_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let msg = e.to_string();
            queue.increment_retry(id)?;
            queue.set_status(id, Status::Failed, Some(&msg))?;
            if job.retry_count + 1 >= config.transfer.max_retries {
                fire_alert(id, &msg, AlertLevel::Error, &config).await;
            }
            return Err(e);
        }
    };

    // --- Transfer ---
    queue.set_status(id, Status::Transferring, None)?;

    let transfer_result = transfer_all(
        &zip_path,
        &job,
        &config,
        Arc::clone(&metrics),
        sem,
        shutdown,
    )
    .await;

    // Always remove the local zip regardless of transfer outcome
    if let Err(e) = tokio::fs::remove_file(&zip_path).await {
        tracing::warn!("could not remove zip {}: {e}", zip_path.display());
    }

    match transfer_result {
        Ok(()) => {
            delete_source_files(&job, &queue).await;
            queue.set_status(id, Status::Done, None)?;
            metrics
                .files_processed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Err(e) => {
            let msg = e.to_string();
            queue.set_status(id, Status::Failed, Some(&msg))?;
            move_to_failed(&job.file_path, &config.failed_folder).await;
            fire_alert(id, &msg, AlertLevel::Error, &config).await;
            return Err(e);
        }
    }
    Ok(())
}

async fn process_stream_job(
    job: Job,
    id: &str,
    queue: Arc<JobQueue>,
    config: Config,
    metrics: Arc<Metrics>,
    shutdown: CancellationToken,
) -> Result<()> {
    let dest = match config.destinations.first() {
        Some(d) => d.clone(),
        None => return Err(AppError::Config("no destinations configured".into())),
    };

    queue.set_status(id, Status::Transferring, None)?;

    let result = stream_convert_and_transfer(&job.file_path, &dest, &config, shutdown).await;

    match result {
        Ok(()) => {
            delete_source_files(&job, &queue).await;
            queue.set_status(id, Status::Done, None)?;
            metrics
                .files_processed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Err(e) => {
            let msg = e.to_string();
            queue.set_status(id, Status::Failed, Some(&msg))?;
            move_to_failed(&job.file_path, &config.failed_folder).await;
            fire_alert(id, &msg, AlertLevel::Error, &config).await;
            return Err(e);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn delete_source_files(job: &Job, queue: &JobQueue) {
    // Remove the primary source file
    if let Err(e) = tokio::fs::remove_file(&job.file_path).await {
        tracing::warn!("remove source {}: {e}", job.file_path.display());
    }

    // Remove any recorded multipart siblings
    let dir = job.file_path.parent().unwrap_or(std::path::Path::new("."));
    let stem = job
        .file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let group_id = format!("{}/{stem}", dir.display());

    if let Ok(parts) = queue.get_parts(&group_id) {
        for part in parts {
            if part != job.file_path {
                if let Err(e) = tokio::fs::remove_file(&part).await {
                    tracing::warn!("remove part {}: {e}", part.display());
                }
            }
        }
        let _ = queue.clear_parts(&group_id);
    }
}

async fn move_to_failed(src: &std::path::Path, failed_dir: &std::path::Path) {
    if let Err(e) = tokio::fs::create_dir_all(failed_dir).await {
        tracing::warn!("create failed_folder {}: {e}", failed_dir.display());
        return;
    }
    if let Some(name) = src.file_name() {
        let dst = failed_dir.join(name);
        if let Err(e) = tokio::fs::rename(src, &dst).await {
            tracing::warn!("move to failed/ {}: {e}", src.display());
        }
    }
}

async fn fire_alert(job_id: &str, message: &str, level: AlertLevel, config: &Config) {
    notify(
        AlertPayload {
            job_id: job_id.to_string(),
            message: message.to_string(),
            level,
        },
        &config.observability,
        &config.smtp,
    )
    .await;
}

fn expire_multipart(queue: &JobQueue, config: &Config) {
    let timeout = config.conversion.multipart_timeout_hours;
    let expired = match queue.get_expired_groups(timeout) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("get_expired_groups: {e}");
            return;
        }
    };

    for group_id in expired {
        tracing::warn!("expiring timed-out multipart group: {group_id}");
        let parts = queue.get_parts(&group_id).unwrap_or_default();
        for part in parts {
            if let Ok(name) = part.file_name().map(|n| n.to_os_string()).ok_or(()) {
                let dst = config.failed_folder.join(name);
                if let Err(e) = std::fs::rename(&part, &dst) {
                    tracing::warn!("expire: move {}: {e}", part.display());
                }
            }
        }
        let _ = queue.clear_parts(&group_id);
    }
}
