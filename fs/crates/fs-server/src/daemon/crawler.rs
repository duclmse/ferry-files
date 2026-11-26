//! The crawl spawner: starts a full crawl as a background task, for the initial
//! crawl and for API-triggered reindexes, and never two at once.
//!
//! The caller (or `spawn_crawl`, for the initial crawl) sets `active` before
//! calling it; the spawned task clears it when it finishes.

use super::*;

/// What a crawl needs.
#[derive(Clone)]
pub(super) struct Crawler {
    pub(super) active: Arc<AtomicBool>,
    pub(super) sqlite: Arc<FileStore>,
    pub(super) tantivy: Arc<TantivyStore>,
    pub(super) metrics: Arc<IndexMetrics>,
    pub(super) registry: Arc<ArchiveRegistry>,
    pub(super) cfg: Arc<RwLock<crate::config::Config>>,
}

impl Crawler {
    /// Crawls `roots`, or every watch path, in the background.
    pub(super) fn spawn(&self, roots: Option<Vec<PathBuf>>, shutdown: CancellationToken) {
        let scope = Scope {
            roots,
            keep: Vec::new(),
        };
        spawn_crawl(self.clone(), scope, shutdown);
    }

    /// `spawn`, keeping the entries of `keep` instead of walking them
    /// (`Scope::keep`).
    pub(super) fn spawn_keeping(
        &self,
        roots: Vec<PathBuf>,
        keep: Vec<PathBuf>,
        shutdown: CancellationToken,
    ) {
        let scope = Scope {
            roots: Some(roots),
            keep,
        };
        spawn_crawl(self.clone(), scope, shutdown);
    }
}

pub(super) fn spawn_crawl(c: Crawler, scope: Scope, shutdown: CancellationToken) {
    let roots = scope.roots.clone();
    let Crawler {
        active: ca,
        sqlite,
        tantivy,
        metrics,
        registry,
        cfg,
    } = c;
    // Set the flag so concurrent reindex requests are rejected until done.
    ca.store(true, Ordering::Release);
    tokio::spawn(async move {
        let what = match &roots {
            None => "full crawl".to_owned(),
            Some(r) => format!(
                "crawl of {}",
                r.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        tracing::info!("{what} starting");
        let store = Arc::clone(&sqlite);
        // Bulk load drops the indexes, which only pays when most rows are new.
        let whole = roots.is_none();
        let bulk = tokio::task::spawn_blocking(move || -> crate::error::Result<bool> {
            Ok(whole && store.begin_bulk_load()?)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
        .unwrap_or_else(|e| {
            tracing::warn!("bulk load setup failed, crawling with indexes: {e}");
            false
        });
        let store = Arc::clone(&sqlite);
        let cancelled = shutdown.clone();
        if let Err(e) = crawl_roots(cfg, scope, sqlite, tantivy, metrics, registry, shutdown).await
        {
            tracing::error!("{what} error: {e}");
        } else if cancelled.is_cancelled() {
            tracing::info!("{what} stopped for shutdown");
        } else {
            tracing::info!("{what} complete");
            let warm = Arc::clone(&store);
            if let Err(e) = tokio::task::spawn_blocking(move || warm.warm_search()).await {
                tracing::warn!("search warm-up panicked: {e}");
            }
            if let Some(summary) = store.heap_summary() {
                tracing::info!("memory store: {summary}");
            }
        }
        // On shutdown, skip the rebuild: it can take tens of seconds, and
        // SqliteStore::with_indexes recreates missing indexes on the next start.
        if bulk && !cancelled.is_cancelled() {
            match tokio::task::spawn_blocking(move || store.end_bulk_load()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!("bulk load index rebuild failed: {e}"),
                Err(e) => tracing::error!("bulk load index rebuild panicked: {e}"),
            }
        }
        ca.store(false, Ordering::Release);
    });
}

// ---------------------------------------------------------------------------
