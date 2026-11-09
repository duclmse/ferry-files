pub mod journal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use notify::{EventKind, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use std::sync::Arc;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::indexer::crawler::IndexEvent;

// ---------------------------------------------------------------------------

/// Dedup priority within a batch: Delete > Rename > Modify > Create.
/// Higher number = higher priority.
fn event_priority(kind: &EventKind) -> u8 {
    match kind {
        EventKind::Remove(_) => 4,
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => 3,
        EventKind::Modify(_) => 2,
        EventKind::Create(_) => 1,
        _ => 0,
    }
}

fn is_archive_ext(path: &std::path::Path, formats: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| formats.iter().any(|f| f.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Convert a batch of debounced notify events to `IndexEvent`s, applying
/// dedup priority (Delete > Rename > Modify > Create) per path.
fn batch_to_events(
    events: Vec<notify_debouncer_full::DebouncedEvent>,
    formats: &[String],
) -> Vec<IndexEvent> {
    // Dedup: for each path keep only the highest-priority event
    let mut best: HashMap<PathBuf, (u8, notify_debouncer_full::DebouncedEvent)> = HashMap::new();

    for ev in events {
        for path in &ev.paths {
            let prio = event_priority(&ev.kind);
            let entry = best.entry(path.clone()).or_insert((0, ev.clone()));
            if prio > entry.0 {
                *entry = (prio, ev.clone());
            }
        }
    }

    let mut out = Vec::with_capacity(best.len());
    for (path, (_, ev)) in best {
        match &ev.kind {
            EventKind::Create(_) => {
                if is_archive_ext(&path, formats) {
                    out.push(IndexEvent::ArchiveModified(path));
                } else {
                    out.push(IndexEvent::Created(path));
                }
            }
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both)) => {
                if ev.paths.len() >= 2 {
                    out.push(IndexEvent::Renamed {
                        from: ev.paths[0].clone(),
                        to: ev.paths[1].clone(),
                    });
                }
            }
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::From)) => {
                // The debouncer couldn't pair this with a matching `To` event
                // (no tracker cookie / file-id match - routine on macOS's
                // FSEvents backend). The path is gone, so treat it as a
                // delete: falling through to the generic `Modify` arm below
                // would call `process_event`'s Created/Modified handler,
                // whose `fs::metadata` read fails on a vanished path and
                // silently no-ops, leaving the stale row in the index forever.
                if is_archive_ext(&path, formats) {
                    out.push(IndexEvent::ArchiveDeleted(path));
                } else {
                    out.push(IndexEvent::Deleted(path));
                }
            }
            EventKind::Modify(_) => {
                if is_archive_ext(&path, formats) {
                    out.push(IndexEvent::ArchiveModified(path));
                } else {
                    out.push(IndexEvent::Modified(path));
                }
            }
            EventKind::Remove(_) => {
                if is_archive_ext(&path, formats) {
                    out.push(IndexEvent::ArchiveDeleted(path));
                } else {
                    out.push(IndexEvent::Deleted(path));
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------

pub async fn run_watcher(
    config: Arc<RwLock<Config>>,
    tx: mpsc::Sender<IndexEvent>,
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
    .map_err(|e| AppError::Index(format!("debouncer create: {e}")))?;

    // Watch all configured paths
    {
        let cfg = config.read().await;
        for path in &cfg.watch_paths {
            debouncer
                .watcher()
                .watch(path, notify::RecursiveMode::Recursive)
                .map_err(|e| AppError::Index(format!("watch {}: {e}", path.display())))?;
        }
    }

    tracing::info!("filesystem watcher started");

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("watcher shutting down");
                break;
            }
            Some(result) = debounce_rx.recv() => {
                // Re-read config to pick up hot-reloaded watch_paths
                let cfg = config.read().await.clone();
                let formats = cfg.archive_index.formats.clone();
                drop(cfg);

                match result {
                    Ok(events) => {
                        let index_events = batch_to_events(events, &formats);
                        for ev in index_events {
                            if tx.send(ev).await.is_err() {
                                tracing::warn!("watcher event channel closed");
                                return Ok(());
                            }
                        }
                    }
                    Err(errs) => {
                        for e in errs {
                            tracing::warn!("watcher error: {e}");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
