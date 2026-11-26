//! Background loops: the stored change-log position, catching up roots that
//! come back, and keeping the snapshot, counts and file lists fresh.

use super::*;

/// How often the stored change-log position advances. It lags by one to two
/// intervals (more if the daemon ran for less than that), so a restart may
/// replay changes that were already applied; replay is idempotent.
pub(super) const JOURNAL_POSITION_INTERVAL: Duration = Duration::from_secs(60);

/// How often to check whether a running crawl has finished, so the position
/// captured before it is stored soon after (a restart within the first minutes
/// would otherwise crawl again).
pub(super) const JOURNAL_CRAWL_POLL: Duration = Duration::from_secs(2);

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
pub(super) async fn keep_journal_position(
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
pub(super) async fn catch_up_roots(
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
pub(super) async fn keep_snapshot_fresh(
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
pub(super) async fn keep_counts(
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

pub(super) async fn keep_file_lists(
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
