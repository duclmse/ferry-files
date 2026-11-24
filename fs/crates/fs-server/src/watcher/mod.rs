pub mod journal;
mod live;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use notify::{EventKind, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tokio::sync::mpsc;
use tokio::sync::{watch, RwLock};
use tokio_util::sync::CancellationToken;

use std::sync::Arc;

use crate::config::{Config, WatcherBackend};
use crate::error::{AppError, Result};
use crate::indexer::crawler::IndexEvent;
use crate::store::FileStore;

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
    // A paired rename is the best event for both of its paths.
    let mut renames = std::collections::HashSet::new();
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
                if ev.paths.len() >= 2 && renames.insert(ev.paths[..2].to_vec()) {
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
    out.into_iter().map(long_names).collect()
}

/// The event with its paths in long form on Windows, where the change
/// notifications name a path by its 8.3 alias (`RUNNER~1`) when that is
/// how it was opened. The crawl and the journal replay use long names, so
/// an alias indexed a second copy of whatever it named, which no later
/// event under the long name removed.
fn long_names(event: IndexEvent) -> IndexEvent {
    if !cfg!(windows) {
        return event;
    }
    match event {
        IndexEvent::Created(p) => IndexEvent::Created(long_path(p)),
        IndexEvent::Modified(p) => IndexEvent::Modified(long_path(p)),
        IndexEvent::Deleted(p) => IndexEvent::Deleted(long_path(p)),
        IndexEvent::ArchiveModified(p) => IndexEvent::ArchiveModified(long_path(p)),
        IndexEvent::ArchiveDeleted(p) => IndexEvent::ArchiveDeleted(long_path(p)),
        IndexEvent::Renamed { from, to } => IndexEvent::Renamed {
            from: long_path(from),
            to: long_path(to),
        },
    }
}

/// Resolves an 8.3 alias through the file system: the path itself if it
/// still exists, else its folder (a deleted entry's own short name cannot
/// be resolved, and is kept).
pub(super) fn long_path(path: PathBuf) -> PathBuf {
    if !path.to_string_lossy().contains('~') {
        return path;
    }
    if let Ok(long) = std::fs::canonicalize(&path) {
        return crate::config::without_verbatim(long);
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
            Ok(long) => crate::config::without_verbatim(long).join(name),
            Err(_) => path,
        },
        _ => path,
    }
}

// ---------------------------------------------------------------------------

/// Follows changes under the watch paths until shutdown, with the backend
/// `watcher.backend` names. When the backend reports lost changes, the
/// timestamp scan finds them (`live::Recovery`); `reindex` asks for a crawl
/// when that is too many to apply one by one.
///
/// `roots` are the watch paths to follow: those that exist now. The daemon
/// starts the watcher again when they change.
pub async fn run_watcher(
    config: Arc<RwLock<Config>>,
    roots: Vec<PathBuf>,
    tx: mpsc::Sender<IndexEvent>,
    index: Arc<FileStore>,
    reindex: watch::Sender<bool>,
    shutdown: CancellationToken,
) -> Result<()> {
    let recovery = live::Recovery::spawn(
        Arc::clone(&config),
        roots.clone(),
        index,
        reindex,
        tx.clone(),
        shutdown.clone(),
    );
    let (backend, debounce_ms) = {
        let c = config.read().await;
        (c.watcher.backend, c.watcher.debounce_ms)
    };
    if backend != WatcherBackend::Notify {
        match live::run(
            Arc::clone(&config),
            roots.clone(),
            tx.clone(),
            recovery.clone(),
            shutdown.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(why) => tracing::info!("watcher.backend {backend:?}: {why}; using notify"),
        }
    }
    run_notify(
        config,
        &roots,
        tx,
        recovery,
        Duration::from_millis(debounce_ms),
        shutdown,
    )
    .await
}

async fn run_notify(
    config: Arc<RwLock<Config>>,
    roots: &[PathBuf],
    tx: mpsc::Sender<IndexEvent>,
    recovery: live::Recovery,
    debounce: Duration,
    shutdown: CancellationToken,
) -> Result<()> {
    let (debounce_tx, mut debounce_rx) =
        tokio::sync::mpsc::unbounded_channel::<DebounceEventResult>();

    let debounce_tx_clone = debounce_tx.clone();
    let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
        let _ = debounce_tx_clone.send(result);
    })
    .map_err(|e| AppError::Index(format!("debouncer create: {e}")))?;

    for path in roots {
        debouncer
            .watcher()
            .watch(path, notify::RecursiveMode::Recursive)
            .map_err(|e| AppError::Index(format!("watch {}: {e}", path.display())))?;
    }

    tracing::info!(
        "filesystem watcher started (notify, {} ms window)",
        debounce.as_millis()
    );

    // Every change before this has been delivered. A batch carries changes
    // up to one debounce window older than its arrival.
    let lag = debounce.as_secs() + 1;
    let mut since = live::unix_now();
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
                        // inotify's queue overflowed, or FSEvents dropped
                        // events: notify says so with a rescan flag and
                        // nothing else.
                        if events.iter().any(|e| e.need_rescan()) {
                            recovery.request(since, "notify asked for a rescan".into());
                        }
                        since = live::unix_now().saturating_sub(lag);
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{ModifyKind, RenameMode};
    use notify_debouncer_full::DebouncedEvent;

    #[test]
    fn a_paired_rename_becomes_one_event() {
        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/w/old"))
            .add_path(PathBuf::from("/w/new"));
        let out = batch_to_events(
            vec![DebouncedEvent::new(event, std::time::Instant::now())],
            &[],
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(
            &out[0],
            IndexEvent::Renamed { from, to } if from.ends_with("old") && to.ends_with("new")
        ));
    }
}
