//! Following the watch paths: restarting the watcher when they change,
//! crawling the added ones, and forgetting removed ones.

use super::*;

/// How often the watch paths are compared with the config, which a config change
/// replaces.
pub(super) const WATCH_PATHS_POLL: Duration = Duration::from_secs(1);

/// Runs the watcher over the config's watch paths (none when there are no
/// paths), and starts it again when they change, crawling the added ones.
/// The rows of a removed path stay, unless it was removed with `forget`.
///
/// The new watcher starts before the old one stops, and the old one gets a
/// debounce window and more to deliver what it holds: changes in between
/// are seen twice, which applying them tolerates, rather than not at all.
pub(super) async fn follow_watch_paths(
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

/// Removes the entries of `gone`, removed watch paths, from both stores and
/// the archive registry. A removed path inside a kept one keeps its entries,
/// which the kept one still covers. Returns the kept paths inside a removed
/// one, which lost their entries with it and need crawling again. Blocking.
pub(super) fn forget_roots(c: &Crawler, gone: &[PathBuf], kept: &[PathBuf]) -> Vec<PathBuf> {
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
