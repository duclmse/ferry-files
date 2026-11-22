//! Live change sources of our own, chosen by `watcher.backend`, and the
//! recovery every source shares.
//!
//! notify's Windows backend reads ReadDirectoryChangesW into a 16 KiB
//! buffer and ignores the overflow that Windows reports, so a burst of more
//! than a few hundred changes (deleting a folder of 1,000 files) loses
//! events without a trace; a lost delete leaves its rows in the index for
//! good. The sources here never lose changes silently: `native` reads
//! ReadDirectoryChangesW with a large buffer and reports overflow, and `usn`
//! reads the NTFS change journal, which keeps every change on disk until it
//! is read. When a source does learn that it lost changes, the timestamp
//! scan (`journal::changes_since`) finds them, as it does for changes made
//! while the daemon was stopped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch, RwLock};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::indexer::crawler::IndexEvent;
use crate::indexer::exclusions::ExclusionEngine;
use crate::store::FileStore;

use super::journal::{self, CatchUp};

mod rdcw;
mod usn;

/// What a source thread hands the async side.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) enum Msg {
    Events(Vec<IndexEvent>),
    /// Changes made since `since` (Unix seconds) may be missing.
    Lost {
        since: u64,
        reason: String,
    },
}

/// Changes collected over one debounce window, in arrival order. A path
/// changed several times keeps only its last change, unless a rename
/// touching it came in between: the rename moves rows, so what came before
/// and after it are different files.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) struct Batch {
    events: Vec<Option<IndexEvent>>,
    latest: HashMap<PathBuf, usize>,
    opened: Option<Instant>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Batch {
    pub(super) fn push(&mut self, event: IndexEvent) {
        self.opened.get_or_insert_with(Instant::now);
        match &event {
            IndexEvent::Renamed { from, to } => {
                self.latest.remove(from);
                self.latest.remove(to);
            }
            IndexEvent::Created(p)
            | IndexEvent::Modified(p)
            | IndexEvent::Deleted(p)
            | IndexEvent::ArchiveModified(p)
            | IndexEvent::ArchiveDeleted(p) => {
                if let Some(i) = self.latest.insert(p.clone(), self.events.len()) {
                    self.events[i] = None;
                }
            }
        }
        self.events.push(Some(event));
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.opened.is_none()
    }

    /// When the batch is to be applied; `None` while it is empty.
    pub(super) fn deadline(&self, window: Duration) -> Option<Instant> {
        self.opened.map(|t| t + window)
    }

    pub(super) fn take(&mut self) -> Vec<IndexEvent> {
        self.latest.clear();
        self.opened = None;
        std::mem::take(&mut self.events)
            .into_iter()
            .flatten()
            .collect()
    }
}

/// How long a source thread waits for the next change before it checks
/// for shutdown and a due batch.
#[cfg_attr(not(windows), allow(dead_code))]
const POLL: Duration = Duration::from_millis(50);

/// How long a source waits before trying again after its watch failed.
#[cfg_attr(not(windows), allow(dead_code))]
const RETRY: Duration = Duration::from_secs(5);

#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How long until `batch` is due, at most [`POLL`].
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn wait_for(batch: &Batch, window: Duration) -> Duration {
    batch.deadline(window).map_or(POLL, |d| {
        d.saturating_duration_since(Instant::now()).min(POLL)
    })
}

/// Sleeps for `d` in [`POLL`] steps; false when shutdown was asked for.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn pause(d: Duration, stop: &std::sync::atomic::AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(POLL.min(end.saturating_duration_since(Instant::now())));
    }
    !stop.load(std::sync::atomic::Ordering::Relaxed)
}

/// Runs the timestamp scan for each "changes lost since" it is sent.
/// Requests that arrive while a scan runs are merged into one more scan
/// from the earliest of them: the scan reads the tree as it is when it
/// runs, so one pass covers every loss before it started.
#[derive(Clone)]
pub(super) struct Recovery {
    tx: mpsc::UnboundedSender<(u64, String)>,
}

impl Recovery {
    pub(super) fn spawn(
        config: Arc<RwLock<Config>>,
        index: Arc<FileStore>,
        reindex: watch::Sender<bool>,
        events: mpsc::Sender<IndexEvent>,
        shutdown: CancellationToken,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, String)>();
        tokio::spawn(async move {
            loop {
                let (mut since, reason) = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    next = rx.recv() => match next {
                        Some(next) => next,
                        None => return,
                    },
                };
                let mut merged = 1;
                while let Ok((s, _)) = rx.try_recv() {
                    since = since.min(s);
                    merged += 1;
                }
                tracing::warn!(
                    "watcher lost changes ({reason}{}); scanning for changes since {since}",
                    if merged > 1 {
                        format!(", and {} more times", merged - 1)
                    } else {
                        String::new()
                    }
                );
                let (roots, formats, exclusions) = {
                    let c = config.read().await;
                    (
                        c.watch_paths.clone(),
                        c.archive_index.formats.clone(),
                        c.exclusions.clone(),
                    )
                };
                let index = Arc::clone(&index);
                let start = Instant::now();
                let plan = tokio::task::spawn_blocking(move || {
                    let excl = match ExclusionEngine::new(&exclusions) {
                        Ok(e) => e,
                        Err(e) => return CatchUp::FullCrawl(format!("exclusions: {e}")),
                    };
                    let skip = |p: &std::path::Path, hidden: bool| excl.should_exclude(p, hidden);
                    journal::changes_since(&roots, since, &formats, &skip, &*index)
                })
                .await;
                match plan {
                    Ok(CatchUp::UpToDate) => tracing::info!(
                        "recovery scan: nothing missing ({:.1}s)",
                        start.elapsed().as_secs_f64()
                    ),
                    Ok(CatchUp::Events(found)) => {
                        tracing::info!(
                            "recovery scan: applying {} changes ({:.1}s)",
                            found.len(),
                            start.elapsed().as_secs_f64()
                        );
                        for ev in found {
                            if events.send(ev).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(CatchUp::FullCrawl(why)) => {
                        tracing::warn!("recovery scan: {why}; reindexing");
                        let _ = reindex.send(true);
                    }
                    Err(e) => tracing::warn!("recovery scan failed: {e}"),
                }
            }
        });
        Self { tx }
    }

    pub(super) fn request(&self, since: u64, reason: String) {
        let _ = self.tx.send((since, reason));
    }
}

/// The event with the archive variant for archive files, as notify's path
/// produces them.
fn with_formats(event: IndexEvent, formats: &[String]) -> IndexEvent {
    let archive = |p: &PathBuf| super::is_archive_ext(p, formats);
    match event {
        IndexEvent::Created(p) | IndexEvent::Modified(p) if archive(&p) => {
            IndexEvent::ArchiveModified(p)
        }
        IndexEvent::Deleted(p) if archive(&p) => IndexEvent::ArchiveDeleted(p),
        other => other,
    }
}

/// Runs the configured native sources until shutdown. `Err` when this
/// platform has none, for the caller to use notify instead.
pub(super) async fn run(
    config: Arc<RwLock<Config>>,
    tx: mpsc::Sender<IndexEvent>,
    recovery: Recovery,
    shutdown: CancellationToken,
) -> std::result::Result<(), String> {
    let (roots, settings) = {
        let c = config.read().await;
        (c.watch_paths.clone(), c.watcher.clone())
    };
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<Msg>();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let threads = start(&roots, &settings, msg_tx, &stop)?;
    tracing::info!(
        "filesystem watcher started ({:?}, {} ms window)",
        settings.backend,
        settings.debounce_ms
    );

    'run: loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            msg = msg_rx.recv() => match msg {
                Some(Msg::Events(events)) => {
                    let formats = config.read().await.archive_index.formats.clone();
                    for ev in events {
                        if tx.send(with_formats(ev, &formats)).await.is_err() {
                            tracing::warn!("watcher event channel closed");
                            break 'run;
                        }
                    }
                }
                Some(Msg::Lost { since, reason }) => recovery.request(since, reason),
                None => break,
            },
        }
    }
    tracing::info!("watcher shutting down");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || {
        for t in threads {
            let _ = t.join();
        }
    })
    .await;
    Ok(())
}

#[cfg(windows)]
fn start(
    roots: &[PathBuf],
    settings: &crate::config::WatcherConfig,
    tx: mpsc::UnboundedSender<Msg>,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> std::result::Result<Vec<std::thread::JoinHandle<()>>, String> {
    use crate::config::WatcherBackend;

    let window = Duration::from_millis(settings.debounce_ms);
    let mut threads = Vec::new();
    let mut rest: Vec<PathBuf> = roots.to_vec();
    if settings.backend == WatcherBackend::Usn {
        let (journaled, threads_usn) = usn::start(roots, window, &tx, stop);
        rest.retain(|r| !journaled.contains(r));
        threads.extend(threads_usn);
    }
    let buffer = (settings.buffer_kib.max(4) as usize) * 1024;
    for root in rest {
        threads.push(rdcw::start(
            root,
            window,
            buffer,
            tx.clone(),
            Arc::clone(stop),
        )?);
    }
    Ok(threads)
}

#[cfg(not(windows))]
fn start(
    _roots: &[PathBuf],
    _settings: &crate::config::WatcherConfig,
    _tx: mpsc::UnboundedSender<Msg>,
    _stop: &Arc<std::sync::atomic::AtomicBool>,
) -> std::result::Result<Vec<std::thread::JoinHandle<()>>, String> {
    Err("only Windows has native watcher backends".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn a_batch_keeps_each_paths_last_change_around_renames() {
        let mut b = Batch::default();
        assert!(b.is_empty());
        b.push(IndexEvent::Created(p("/w/a")));
        b.push(IndexEvent::Modified(p("/w/a")));
        b.push(IndexEvent::Created(p("/w/tmp")));
        b.push(IndexEvent::Renamed {
            from: p("/w/tmp"),
            to: p("/w/b"),
        });
        b.push(IndexEvent::Modified(p("/w/b")));
        b.push(IndexEvent::Deleted(p("/w/a")));
        assert!(!b.is_empty());
        let got: Vec<String> = b.take().iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(
            got,
            vec![
                format!("{:?}", IndexEvent::Created(p("/w/tmp"))),
                format!(
                    "{:?}",
                    IndexEvent::Renamed {
                        from: p("/w/tmp"),
                        to: p("/w/b")
                    }
                ),
                format!("{:?}", IndexEvent::Modified(p("/w/b"))),
                format!("{:?}", IndexEvent::Deleted(p("/w/a"))),
            ]
        );
        assert!(b.is_empty());
        assert!(b.take().is_empty());
    }

    #[test]
    fn archives_get_their_own_events() {
        let formats = vec!["zip".to_owned()];
        assert!(matches!(
            with_formats(IndexEvent::Created(p("/w/a.ZIP")), &formats),
            IndexEvent::ArchiveModified(_)
        ));
        assert!(matches!(
            with_formats(IndexEvent::Deleted(p("/w/a.zip")), &formats),
            IndexEvent::ArchiveDeleted(_)
        ));
        assert!(matches!(
            with_formats(IndexEvent::Deleted(p("/w/a.txt")), &formats),
            IndexEvent::Deleted(_)
        ));
    }
}
