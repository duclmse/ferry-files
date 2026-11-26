//! Polled roots, which get no watcher: when each was last scanned, and
//! scanning one when it is due.

use super::*;

/// When each polled root was last scanned, from when, and whether a scan of
/// it is running.
#[derive(Default)]
pub(super) struct Polls {
    pub(super) roots: std::collections::HashMap<PathBuf, Poll>,
}

pub(super) struct Poll {
    pub(super) last: std::time::Instant,
    /// Unix seconds the next scan looks for changes after.
    pub(super) since: u64,
    pub(super) busy: Arc<AtomicBool>,
}

pub(super) fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Polls {
    /// Starts a scan of each polled root that is online, due and not
    /// already being scanned. A root starts its clock when first seen:
    /// catch-up or its crawl covered what came before.
    pub(super) fn run(
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
pub(super) async fn poll_root(
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
pub(super) const PRESENCE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `roots` that exist now, as folders. When the check takes too long,
/// the answer is `last`, what was known before, limited to `roots`.
pub(super) async fn present(roots: &[PathBuf], last: &[PathBuf]) -> Vec<PathBuf> {
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
