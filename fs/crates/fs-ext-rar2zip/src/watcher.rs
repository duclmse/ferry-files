use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use notify::{EventKind, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use std::sync::Arc;

use crate::config::Config;
use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------

fn file_age_secs(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn passes_filters(path: &std::path::Path, cfg: &Config) -> bool {
    // 1. Extension check (case-insensitive)
    if !cfg.includes_extension(path) {
        return false;
    }

    // 2. Exclude pattern check
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    for pat in &cfg.exclude_patterns {
        if glob::Pattern::new(pat)
            .map(|p| p.matches(name))
            .unwrap_or(false)
        {
            return false;
        }
    }

    // 3. Subdir filter
    if let Some(subdirs) = &cfg.watch_subdirs {
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        if let Some(parent_name) = parent {
            if !subdirs.iter().any(|s| s == parent_name) {
                return false;
            }
        } else {
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------

pub async fn run_watcher(
    config: Arc<RwLock<Config>>,
    tx: mpsc::Sender<PathBuf>,
    shutdown: CancellationToken,
) -> Result<()> {
    let (debounce_tx, mut debounce_rx) =
        tokio::sync::mpsc::unbounded_channel::<DebounceEventResult>();

    let debounce_tx_clone = debounce_tx.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        None,
        move |result: DebounceEventResult| {
            let _ = debounce_tx_clone.send(result);
        },
    )
    .map_err(|e| AppError::Config(format!("rar2zip debouncer: {e}")))?;

    {
        let cfg = config.read().await;
        debouncer
            .watcher()
            .watch(&cfg.watch_folder, notify::RecursiveMode::Recursive)
            .map_err(|e| AppError::Config(format!("watch folder: {e}")))?;
        tracing::info!("watching folder: {}", cfg.watch_folder.display());
    }

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("rar2zip watcher shutting down");
                break;
            }
            Some(result) = debounce_rx.recv() => {
                let cfg = config.read().await.clone();

                let events = match result {
                    Ok(ev) => ev,
                    Err(errs) => {
                        for e in errs { tracing::warn!("watcher error: {e}"); }
                        continue;
                    }
                };

                for ev in events {
                    // Only process file-create events
                    if !matches!(ev.kind, EventKind::Create(_)) {
                        continue;
                    }
                    for path in ev.paths.iter().cloned() {
                        if path.is_dir() { continue; }
                        if !passes_filters(&path, &cfg) { continue; }

                        let age = file_age_secs(&path);
                        if age >= cfg.min_file_age_secs {
                            let _ = tx.send(path).await;
                        } else {
                            // Schedule delayed re-check
                            let remaining = cfg.min_file_age_secs - age;
                            let tx2  = tx.clone();
                            let cfg2 = cfg.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_secs(remaining + 1)).await;
                                if path.exists() && passes_filters(&path, &cfg2) {
                                    let _ = tx2.send(path).await;
                                }
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
