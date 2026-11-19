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
    // failed_folder may sit inside the watched tree; moving a file there must
    // not queue it again.
    if path.starts_with(&cfg.failed_folder) {
        return false;
    }

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

/// Files appear by being created or by being renamed or moved in; most
/// downloaders write `x.part` and rename it when complete.
fn is_arrival(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
    )
}

/// Sends `path` on once it is old enough, or schedules a recheck.
async fn submit(path: PathBuf, cfg: &Config, tx: &mpsc::Sender<PathBuf>) {
    if !passes_filters(&path, cfg) {
        return;
    }
    let age = file_age_secs(&path);
    if age >= cfg.min_file_age_secs {
        let _ = tx.send(path).await;
        return;
    }
    let remaining = cfg.min_file_age_secs - age;
    let tx = tx.clone();
    let cfg = cfg.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(remaining + 1)).await;
        if path.exists() && passes_filters(&path, &cfg) {
            let _ = tx.send(path).await;
        }
    });
}

fn files_under(root: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(entry.path()),
                Ok(t) if t.is_file() => files.push(entry.path()),
                _ => {}
            }
        }
    }
    files.sort();
    files
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

    // Files that arrived while rar2zip was not running raise no event. Intake
    // skips any whose content is already queued or done.
    {
        let cfg = config.read().await.clone();
        let root = cfg.watch_folder.clone();
        let existing = tokio::task::spawn_blocking(move || files_under(&root))
            .await
            .map_err(|e| AppError::Config(format!("scan watch folder: {e}")))?;
        let count = existing.len();
        for path in existing {
            submit(path, &cfg, &tx).await;
        }
        tracing::info!("startup scan: {count} file(s) in the watch folder");
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
                    if !is_arrival(&ev.kind) {
                        continue;
                    }
                    // A rename lists the old path too; it no longer exists.
                    for path in ev.paths.iter().filter(|p| p.is_file()).cloned() {
                        submit(path, &cfg, &tx).await;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConversionConfig, ObservabilityConfig, TransferConfig};
    use std::path::Path;

    fn config(watch: &Path) -> Config {
        Config {
            watch_folder: watch.to_path_buf(),
            failed_folder: watch.join("failed"),
            exclude_patterns: vec!["*.part".into()],
            include_extensions: vec!["rar".into()],
            watch_subdirs: None,
            min_file_age_secs: 0,
            conversion: ConversionConfig::default(),
            destinations: vec![],
            transfer: TransferConfig::default(),
            observability: ObservabilityConfig::default(),
            smtp: None,
        }
    }

    async fn next(rx: &mut mpsc::Receiver<PathBuf>) -> Option<PathBuf> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn existing_renamed_in_files_arrive_and_failed_ones_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let watch = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(watch.join("failed")).unwrap();
        std::fs::create_dir_all(watch.join("sub")).unwrap();
        std::fs::write(watch.join("sub/before.rar"), b"a").unwrap();
        std::fs::write(watch.join("failed/old.rar"), b"b").unwrap();
        std::fs::write(watch.join("note.txt"), b"c").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let stop = CancellationToken::new();
        let cfg = Arc::new(RwLock::new(config(&watch)));
        let task = tokio::spawn(run_watcher(cfg, tx, stop.clone()));

        assert_eq!(next(&mut rx).await.unwrap(), watch.join("sub/before.rar"));

        // Let the platform watcher settle before the events that matter.
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(watch.join("dl.rar.part"), b"d").unwrap();
        std::fs::rename(watch.join("dl.rar.part"), watch.join("dl.rar")).unwrap();
        assert_eq!(next(&mut rx).await.unwrap(), watch.join("dl.rar"));

        std::fs::rename(watch.join("dl.rar"), watch.join("failed/dl.rar")).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            rx.try_recv().is_err(),
            "a move into failed/ is not an arrival"
        );

        stop.cancel();
        task.await.unwrap().unwrap();
    }
}
