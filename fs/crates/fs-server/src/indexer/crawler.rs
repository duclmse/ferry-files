use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, mpsc, Arc};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::api::archive::ArchiveRegistry;
use crate::config::{Config, MftMode};
use crate::error::{AppError, Result};
use crate::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

use super::archive::ArchiveCrawler;
use super::content::content_limit;
use super::exclusions::ExclusionEngine;
use super::mft::MftCache;

mod crawl;
mod dzx_sidecars;
mod fsmeta;
mod metrics;
#[cfg(test)]
mod tests;

pub use metrics::{IndexEvent, IndexMetrics};

use crawl::{crawl_archive_sync, SidecarConfig};
use fsmeta::{build_file_record, is_hidden_name, record_from_mft, unix_now};

// ---------------------------------------------------------------------------
// Per-entry work beyond the metadata row
// ---------------------------------------------------------------------------

struct SideWork {
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
    arc_cfg: crate::config::ArchiveIndexConfig,
    cnt_cfg: crate::config::ContentConfig,
    zidx_dir: Option<PathBuf>,
    dzx_dir: Option<PathBuf>,
    dzx_min_entry_bytes: u64,
    dzx_max_concurrent_builds: usize,
    /// `(watch_root, generation)` during a full crawl, so archive entries
    /// survive its stale-row sweep; `None` for watcher events.
    tag: Option<(String, i64)>,
}

#[derive(Clone)]
struct Stores {
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
}

impl SideWork {
    /// `under` picks the content limits: a network mount may have none.
    fn new(cfg: &Config, under: &Path, stores: &Stores, tag: Option<(String, i64)>) -> Self {
        let service = &cfg.archive_service;
        let sidecar_dir = |dir: &Option<PathBuf>, default: &str| {
            service
                .enabled
                .then(|| dir.clone().unwrap_or_else(|| cfg.data_dir.join(default)))
        };
        Self {
            sqlite: Arc::clone(&stores.sqlite),
            tantivy: Arc::clone(&stores.tantivy),
            metrics: Arc::clone(&stores.metrics),
            registry: Arc::clone(&stores.registry),
            arc_cfg: cfg.archive_index.clone(),
            cnt_cfg: super::content::effective_config(&cfg.content, &cfg.exclusions, under),
            zidx_dir: sidecar_dir(&service.zidx_dir, "zidx"),
            dzx_dir: sidecar_dir(&service.dzx_dir, "dzx"),
            dzx_min_entry_bytes: service.dzx_min_entry_bytes,
            dzx_max_concurrent_builds: service.dzx_max_concurrent_builds,
            tag,
        }
    }

    fn is_archive(&self, path: &Path) -> bool {
        self.arc_cfg.enabled
            && path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                self.arc_cfg
                    .formats
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(e))
            })
    }

    fn wanted(&self, path: &Path, is_dir: bool) -> bool {
        !is_dir && (self.is_archive(path) || content_limit(path, &self.cnt_cfg).is_some())
    }

    fn run(&self, record: &FileRecord) {
        if record.is_dir {
            return;
        }
        let path = Path::new(&record.path);
        if let Some(max_bytes) = content_limit(path, &self.cnt_cfg) {
            if let Some(text) = super::content::read_content(path, max_bytes) {
                let mut r = record.clone();
                r.content_indexed = true;
                if self.tantivy.upsert(&r, Some(&text)).is_ok() {
                    self.metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        // Archive indexing has its own BEGIN/COMMIT.
        if self.is_archive(path) {
            let crawler = ArchiveCrawler::new(&self.arc_cfg, &self.cnt_cfg);
            if let Err(e) = crawl_archive_sync(
                path,
                &crawler,
                self.sqlite.as_ref(),
                &self.tantivy,
                &self.metrics,
                SidecarConfig {
                    zidx_dir: self.zidx_dir.as_deref(),
                    registry: Some(&self.registry),
                    dzx: self
                        .dzx_dir
                        .as_deref()
                        .map(|d| (d, self.dzx_min_entry_bytes, self.dzx_max_concurrent_builds)),
                },
                self.tag.as_ref().map(|(root, gen)| (root.as_str(), *gen)),
            ) {
                tracing::warn!("archive crawl error for {}: {e}", path.display());
            }
        }
    }
}

/// With `use_mft = "auto"`, a root whose last crawl found at least this many
/// entries reads the file table even if it is not a whole volume. On the
/// 2-core CI runner the table cost 9-10 s for all of C: whatever the root,
/// and the walk managed 2,200-10,000 entries a second, so the break-even
/// lies between about 22,000 and 100,000 entries; this leaves a margin.
/// Measured on one machine with a warm cache: revisit with cold numbers.
const MFT_AUTO_MIN_ENTRIES: u64 = 200_000;

fn entries_key(root: &str) -> String {
    format!("root_entries:{root}")
}

/// Entries the last full crawl found under `root`; 0 if never crawled.
fn last_entries(store: &FileStore, root: &str) -> u64 {
    store
        .get_meta(&entries_key(root))
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Bounds the files waiting for content or archive work, so a walk that
/// outruns the workers waits for them instead of queueing the whole tree.
const SIDE_QUEUE: usize = 1024;

/// Reads content and indexes archives for the files the walk hands over, on
/// `content_threads` threads apart from the walkers, so the walk is not held
/// up by file reads. After a cancel the queue is drained without work.
fn spawn_side_workers(
    threads: usize,
    side_work: &Arc<SideWork>,
    rx: mpsc::Receiver<FileRecord>,
    shutdown: &CancellationToken,
) -> Vec<std::thread::JoinHandle<()>> {
    let rx = Arc::new(std::sync::Mutex::new(rx));
    (0..threads.max(1))
        .map(|_| {
            let rx = Arc::clone(&rx);
            let side_work = Arc::clone(side_work);
            let shutdown = shutdown.clone();
            std::thread::spawn(move || loop {
                let next = match rx.lock() {
                    Ok(rx) => rx.recv(),
                    Err(_) => return,
                };
                let Ok(record) = next else { return };
                if !shutdown.is_cancelled() {
                    side_work.run(&record);
                }
            })
        })
        .collect()
}

/// Returns false, having sent nothing, if the file table cannot be used.
///
/// Rows go out in walk order because the memory store shares a parent's path
/// only with entries added after it.
fn crawl_from_mft(
    root: &Path,
    cache: &mut MftCache,
    excl: &ExclusionEngine,
    tx: &mpsc::SyncSender<(FileRecord, String)>,
    root_str: &str,
    side_work: &SideWork,
    side_tx: &mpsc::SyncSender<FileRecord>,
) -> bool {
    let index = match cache.for_root(root) {
        Ok(index) => index,
        Err(e) => {
            tracing::warn!("file table unavailable, walking {}: {e}", root.display());
            return false;
        }
    };
    let now = unix_now();
    let mut rows = Vec::new();
    let walked = index.walk(root, |path, file| {
        let is_hidden = is_hidden_name(path);
        if excl.should_exclude(path, is_hidden) {
            return false;
        }
        rows.push(record_from_mft(path, file, is_hidden, now));
        true
    });
    if let Err(e) = walked {
        tracing::warn!("file table unusable, walking {}: {e}", root.display());
        return false;
    }
    tracing::info!(
        "file table: {} entries under {}",
        rows.len(),
        root.display()
    );
    for row in rows {
        let side = side_work
            .wanted(Path::new(&row.path), row.is_dir)
            .then(|| row.clone());
        let _ = tx.send((row, root_str.to_owned()));
        if let Some(side) = side {
            let _ = side_tx.send(side);
        }
    }
    true
}

// ---------------------------------------------------------------------------
// full_crawl
// ---------------------------------------------------------------------------

pub async fn full_crawl(
    config: Arc<RwLock<Config>>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
    shutdown: CancellationToken,
) -> Result<()> {
    let cfg = config.read().await.clone();

    let exclusions = Arc::new(ExclusionEngine::new(&cfg.exclusions)?);
    let watch_paths = cfg.watch_paths.clone();

    // One generation per full crawl. Each upserted row is marked with the
    // watch_root it was found under and this generation. After each root
    // finishes, rows with an older generation are stale and deleted in SQL -
    // no full path list is materialised in Rust memory.
    let generation = sqlite.begin_crawl_generation()?;

    let stores = Stores {
        sqlite: Arc::clone(&sqlite),
        tantivy: Arc::clone(&tantivy),
        metrics: Arc::clone(&metrics),
        registry: Arc::clone(&registry),
    };
    let sqlite_w = Arc::clone(&sqlite);
    let metrics_w = Arc::clone(&metrics);
    let excl_w = Arc::clone(&exclusions);
    let shutdown_w = shutdown.clone();
    let cfg_w = cfg.clone();

    let use_mft = cfg.indexer.use_mft;
    let crawl_threads = cfg.indexer.crawl_threads;
    let content_threads = cfg.indexer.content_threads;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut mft_cache = MftCache::default();
        for watch_path in &watch_paths {
            if shutdown_w.is_cancelled() {
                break;
            }

            let watch_root = watch_path.to_string_lossy().into_owned();

            let walker = super::walk_builder(watch_path)
                .threads(crawl_threads)
                .build_parallel();
            let side_work = Arc::new(SideWork::new(
                &cfg_w,
                watch_path,
                &stores,
                Some((watch_root.clone(), generation)),
            ));
            if side_work.cnt_cfg.extension_limits.is_empty()
                && !cfg_w.content.extension_limits.is_empty()
            {
                tracing::info!(
                    "no content indexed under {}: network mount",
                    watch_path.display()
                );
            }
            let excl = Arc::clone(&excl_w);
            let root_str = watch_root.clone();

            // Dedicated writer thread batches upserts in groups of 500
            // inside a single BEGIN/COMMIT, reducing write transactions to ⌈N/500⌉.
            // Metrics are incremented here (not in the walker) so that
            // `total_files` reflects rows actually committed to SQLite - tests
            // and the REST /stats endpoint use it as a crawl-completion signal.
            // Benchmarked (scripts/bench.sh) against a 615k-file / 88k-dir tree:
            // 500 -> ~40s, 4000 -> ~21s, 10000 -> ~18s, 25000+ -> ~16s (plateau,
            // walker/stat-syscall-bound). 10000 captures most of the win while
            // keeping the per-crash replay window (unwritten buffered rows) small.
            const BATCH_SIZE: usize = 10000;
            // Room for several batches: with a channel smaller than one batch
            // the walkers stalled for every commit and the writer then waited
            // for them to refill it (crawl 52-57 s -> 42-45 s on 2.4M files).
            let (tx, rx) = mpsc::sync_channel::<(FileRecord, String)>(4 * BATCH_SIZE);
            let writer_thread = {
                let sqlite_b = Arc::clone(&sqlite_w);
                let metrics_b = Arc::clone(&metrics_w);
                let gen = generation;
                std::thread::spawn(move || {
                    let mut buf: Vec<(FileRecord, String)> = Vec::with_capacity(BATCH_SIZE);
                    let mut entries: u64 = 0;
                    for item in rx {
                        entries += 1;
                        buf.push(item);
                        if buf.len() >= BATCH_SIZE {
                            if let Err(e) = sqlite_b.batch_upsert_crawl(&buf, gen) {
                                tracing::warn!("batch_upsert_crawl error: {e}");
                            } else {
                                for (rec, _) in &buf {
                                    if rec.is_dir {
                                        metrics_b.total_dirs.fetch_add(1, Ordering::Relaxed);
                                    } else {
                                        metrics_b.total_files.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            buf.clear();
                        }
                    }
                    if !buf.is_empty() {
                        if let Err(e) = sqlite_b.batch_upsert_crawl(&buf, gen) {
                            tracing::warn!("batch_upsert_crawl (final) error: {e}");
                        } else {
                            for (rec, _) in &buf {
                                if rec.is_dir {
                                    metrics_b.total_dirs.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    metrics_b.total_files.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    entries
                })
            };

            let (side_tx, side_rx) = mpsc::sync_channel::<FileRecord>(SIDE_QUEUE);
            let side_workers =
                spawn_side_workers(content_threads, &side_work, side_rx, &shutdown_w);

            let try_mft = match use_mft {
                MftMode::On => true,
                MftMode::Off => false,
                MftMode::Auto => {
                    super::mft::is_volume_root(watch_path)
                        || mft_cache.has_drive_of(watch_path)
                        || last_entries(&sqlite_w, &watch_root) >= MFT_AUTO_MIN_ENTRIES
                }
            };
            let from_mft = try_mft
                && crawl_from_mft(
                    watch_path,
                    &mut mft_cache,
                    &excl,
                    &tx,
                    &root_str,
                    &side_work,
                    &side_tx,
                );
            if !from_mft {
                walker.run(|| {
                    let tx_w = tx.clone();
                    let side_tx = side_tx.clone();
                    let excl = Arc::clone(&excl);
                    let side_work = Arc::clone(&side_work);
                    let root_str = root_str.clone();

                    Box::new(move |result| {
                        let entry = match result {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!("walk error: {e}");
                                return ignore::WalkState::Continue;
                            }
                        };

                        let path = entry.path();
                        let Ok(meta) = entry.metadata() else {
                            return ignore::WalkState::Continue;
                        };

                        let is_hidden = is_hidden_name(path);
                        if excl.should_exclude(path, is_hidden) {
                            return if meta.is_dir() {
                                ignore::WalkState::Skip
                            } else {
                                ignore::WalkState::Continue
                            };
                        }

                        let record = build_file_record(path, &meta, is_hidden, unix_now());
                        let side = side_work
                            .wanted(path, meta.is_dir())
                            .then(|| record.clone());
                        let _ = tx_w.send((record, root_str.clone()));
                        if let Some(side) = side {
                            let _ = side_tx.send(side);
                        }
                        ignore::WalkState::Continue
                    })
                });
            }

            // Archive entries carry this generation too, so the workers finish
            // before the stale rows go.
            drop(side_tx);
            for worker in side_workers {
                let _ = worker.join();
            }

            // Closing tx signals the writer thread that no more records are coming.
            // Join before removing stale rows so all crawl_generation marks are committed.
            drop(tx);
            if let Ok(entries) = writer_thread.join() {
                let _ = sqlite_w.set_meta(&entries_key(&watch_root), &entries.to_string());
            }

            // Remove stale rows for this watch root in SQL - no Rust Vec of paths.
            let removed = sqlite_w.remove_stale_for_root(&watch_root, generation)?;
            if removed > 0 {
                tracing::info!("reconcile: removed {removed} stale entries under {watch_root}");
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| AppError::Index(e.to_string()))??;

    // Blocking: the commit drains Tantivy's indexing queue and fsyncs, and
    // total_files counts every row.
    let now = unix_now();
    metrics.last_crawl_secs.store(now as u64, Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = tantivy.commit() {
            tracing::error!("final tantivy commit: {e}");
        }
        if let Err(e) = tantivy.reload_reader() {
            tracing::error!("tantivy reload: {e}");
        }
        let _ = sqlite.set_meta("last_crawl_time", &now.to_string());
        let total = sqlite.total_files().unwrap_or(0);
        let _ = sqlite.set_meta("total_files", &total.to_string());
    })
    .await
    .map_err(|e| AppError::Index(e.to_string()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// process_event - incremental update from watcher
// ---------------------------------------------------------------------------

/// Whether a Created/Modified event under `parent` should sweep its
/// siblings. The sweep stats every indexed sibling, thousands per event in a
/// folder like System32, and it exists for FSEvents' lost rename sides, so
/// macOS sweeps on every event. Elsewhere renames are reported in full and
/// the sweep is only a backstop for lost notifications, so each folder is
/// swept at most once per `SWEEP_EVERY`.
fn sweep_due(parent: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant};

    const SWEEP_EVERY: Duration = Duration::from_secs(30);
    static SWEPT: LazyLock<Mutex<HashMap<String, Instant>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    if cfg!(target_os = "macos") {
        return true;
    }
    let Ok(mut swept) = SWEPT.lock() else {
        return true;
    };
    let now = Instant::now();
    if swept.len() > 4096 {
        swept.retain(|_, at| now.duration_since(*at) < SWEEP_EVERY);
    }
    match swept.get(parent) {
        Some(at) if now.duration_since(*at) < SWEEP_EVERY => false,
        _ => {
            swept.insert(parent.to_owned(), now);
            true
        }
    }
}

/// Removes `path` and, for a folder, everything indexed under it: a folder
/// moved out of the tree is reported alone, and its contents went with it.
fn remove_path(sqlite: &FileStore, tantivy: &TantivyStore, path: &str) {
    let under = if indexed_dir(sqlite, path) {
        sqlite.paths_under(path).unwrap_or_default()
    } else {
        Vec::new()
    };
    for p in under
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(path))
    {
        if let Err(e) = tantivy.remove(p) {
            tracing::warn!("tantivy remove: {e}");
        }
    }
    if !under.is_empty() {
        if let Err(e) = sqlite.remove_directory(path) {
            tracing::warn!("remove subtree {path}: {e}");
        }
    }
    if let Err(e) = sqlite.remove_file(path) {
        tracing::warn!("remove {path}: {e}");
    }
}

fn log_if_slow(start: std::time::Instant, what: &str, path: &str) {
    let took = start.elapsed();
    if took > SLOW {
        tracing::info!("{what} {path} in {:.1}s", took.as_secs_f64());
    }
}

fn parent_indexed(cfg: &Config, sqlite: &FileStore, batch: &FileBatch, path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if cfg.watch_paths.iter().any(|root| root == parent) {
        return true;
    }
    let parent = parent.to_string_lossy();
    batch.has(&parent) || indexed_dir(sqlite, &parent)
}

/// Whether `path` is indexed as a folder: one row lookup, where asking for
/// the subtree of every deleted or renamed file cost a query each.
fn indexed_dir(sqlite: &FileStore, path: &str) -> bool {
    matches!(sqlite.get_by_path(path), Ok(Some(r)) if r.is_dir)
}

/// Created, modified and deleted events, applied in order: plain files are
/// collected per path, last event winning, and written in one transaction;
/// a folder's subtree work flushes them first so it sees them in order.
fn apply_file_events(events: Vec<IndexEvent>, cfg: &Config, stores: &Stores) {
    let mut batch = FileBatch::default();
    let count = events.len();
    for event in events {
        let (path, deleted) = match event {
            IndexEvent::Created(p) | IndexEvent::Modified(p) => (p, false),
            IndexEvent::Deleted(p) => (p, true),
            _ => continue,
        };
        let path_str = path.to_string_lossy().into_owned();
        let meta = if deleted {
            None
        } else {
            match std::fs::metadata(&path) {
                Ok(meta) => Some(meta),
                // FSEvents reports an unpaired rename's old side, or a
                // deletion, as a modification of a path that is gone.
                Err(_) if std::fs::symlink_metadata(&path).is_err() => None,
                Err(_) => continue,
            }
        };
        let Some(meta) = meta else {
            if indexed_dir(&stores.sqlite, &path_str) {
                batch.flush(cfg, stores);
                let start = std::time::Instant::now();
                remove_path(&stores.sqlite, &stores.tantivy, &path_str);
                log_if_slow(start, "removed folder", &path_str);
            } else {
                batch.remove(path_str);
            }
            continue;
        };
        let record = build_file_record(&path, &meta, is_hidden_name(&path), unix_now());
        // A folder moved in, or renamed without a pairing the debouncer saw,
        // is reported alone; its contents arrive with no events. Its parent
        // is indexed; a path that names an indexed folder another way (an
        // alias) has an unindexed parent too, and must not be walked again.
        if meta.is_dir()
            && !batch.has(&record.path)
            && matches!(stores.sqlite.get_by_path(&record.path), Ok(None))
            && parent_indexed(cfg, &stores.sqlite, &batch, &path)
        {
            match ExclusionEngine::shared(&cfg.exclusions) {
                Ok(excl) => {
                    let start = std::time::Instant::now();
                    for child in records_under(&path, &excl) {
                        batch.upsert(child);
                    }
                    log_if_slow(start, "walked unseen folder", &record.path);
                }
                Err(e) => tracing::warn!("exclusions: {e}"),
            }
        }
        if let Some(parent) = path.parent() {
            batch.parents.insert(parent.to_string_lossy().into_owned());
        }
        batch.upsert(record);
    }
    batch.flush(cfg, stores);
    batch.sweep(stores);
    stores
        .metrics
        .events_processed
        .fetch_add(count as u64, Ordering::Relaxed);
}

#[derive(Default)]
struct FileBatch {
    /// The latest change per path: an upsert, or `None` for a removal.
    changes: std::collections::HashMap<String, Option<FileRecord>>,
    parents: std::collections::BTreeSet<String>,
}

impl FileBatch {
    fn upsert(&mut self, record: FileRecord) {
        self.changes.insert(record.path.clone(), Some(record));
    }

    fn remove(&mut self, path: String) {
        self.changes.insert(path, None);
    }

    fn has(&self, path: &str) -> bool {
        matches!(self.changes.get(path), Some(Some(_)))
    }

    fn flush(&mut self, cfg: &Config, stores: &Stores) {
        let mut upserts = Vec::new();
        let mut removes = Vec::new();
        for (path, change) in self.changes.drain() {
            match change {
                Some(record) => upserts.push(record),
                None => removes.push(path),
            }
        }
        if let Err(e) = stores.sqlite.apply_changes(&upserts, &removes) {
            tracing::warn!("watcher batch of {}: {e}", upserts.len() + removes.len());
            return;
        }
        for path in &removes {
            if let Err(e) = stores.tantivy.remove(path) {
                tracing::warn!("tantivy remove: {e}");
            }
        }
        // Content limits depend on the watch root (network mounts).
        let mut side: std::collections::HashMap<PathBuf, SideWork> =
            std::collections::HashMap::new();
        for record in upserts.iter().filter(|r| !r.is_dir) {
            let path = Path::new(&record.path);
            let root = cfg
                .watch_paths
                .iter()
                .filter(|root| path.starts_with(root))
                .max_by_key(|root| root.as_os_str().len())
                .cloned()
                .unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf());
            side.entry(root)
                .or_insert_with_key(|root| SideWork::new(cfg, root, stores, None))
                .run(record);
        }
    }

    /// Stale-sibling reconciliation. notify-debouncer-full's FSEvents rename
    /// pairing can silently swallow the "old path" side of a rename (its
    /// was_created() heuristic misfires when FSEvents replays the file's
    /// original creation flags alongside the rename-from flag), so a
    /// renamed-away file's row could stay with no event ever firing for it.
    /// Each touched folder's recorded children are checked on disk.
    fn sweep(&mut self, stores: &Stores) {
        for parent in std::mem::take(&mut self.parents) {
            if !sweep_due(&parent) {
                continue;
            }
            let Ok(children) = stores.sqlite.children_of(&parent) else {
                continue;
            };
            for child in children {
                if std::fs::symlink_metadata(&child).is_err() {
                    if let Err(e) = stores.sqlite.remove_file(&child) {
                        tracing::warn!("stale-sibling remove: {e}");
                    }
                    if let Err(e) = stores.tantivy.remove(&child) {
                        tracing::warn!("stale-sibling tantivy remove: {e}");
                    }
                }
            }
        }
    }
}

/// Watcher events in order, `event_batch_size` at most: runs of created,
/// modified and deleted events are applied together, the rest one by one
/// where they fall. One caller at a time keeps each path's events in order.
pub async fn process_events(
    events: Vec<IndexEvent>,
    config: Arc<RwLock<Config>>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
) {
    let cfg = Arc::new(config.read().await.clone());
    let stores = Stores {
        sqlite,
        tantivy,
        metrics,
        registry,
    };
    let start = std::time::Instant::now();
    let received = events.len();
    let events = match ExclusionEngine::shared(&cfg.exclusions) {
        Ok(excl) => without_excluded(events, &excl),
        Err(e) => {
            tracing::warn!("exclusions: {e}");
            events
        }
    };
    let kept = events.len();
    let mut run = Vec::new();
    for event in events {
        if matches!(
            event,
            IndexEvent::Created(_) | IndexEvent::Modified(_) | IndexEvent::Deleted(_)
        ) {
            run.push(event);
            continue;
        }
        flush_run(&mut run, &cfg, &stores).await;
        process_event(
            event,
            Arc::clone(&config),
            Arc::clone(&stores.sqlite),
            Arc::clone(&stores.tantivy),
            Arc::clone(&stores.metrics),
            Arc::clone(&stores.registry),
        )
        .await;
    }
    flush_run(&mut run, &cfg, &stores).await;
    let took = start.elapsed();
    if took > SLOW {
        tracing::info!(
            "slow event batch: {kept} of {received} events applied in {:.1}s",
            took.as_secs_f64()
        );
    }
}

/// An event batch or folder operation slower than this is logged, so a
/// stall in the single event consumer can be traced to its cause.
const SLOW: std::time::Duration = std::time::Duration::from_secs(1);

/// Drops events for paths the crawl excludes, as the crawl does. Without
/// this, a data folder inside a watched tree (`C:\` as a root) fed its own
/// database and index writes back in as events, each causing more writes.
/// A rename across the boundary becomes the side that stays visible.
fn without_excluded(events: Vec<IndexEvent>, excl: &ExclusionEngine) -> Vec<IndexEvent> {
    let out = |p: &Path| excl.should_exclude(p, is_hidden_name(p));
    events
        .into_iter()
        .filter_map(|event| match event {
            IndexEvent::Renamed { from, to } => match (out(&from), out(&to)) {
                (false, false) => Some(IndexEvent::Renamed { from, to }),
                (false, true) => Some(IndexEvent::Deleted(from)),
                (true, false) => Some(IndexEvent::Created(to)),
                (true, true) => None,
            },
            IndexEvent::Created(ref p)
            | IndexEvent::Modified(ref p)
            | IndexEvent::Deleted(ref p)
            | IndexEvent::ArchiveModified(ref p)
            | IndexEvent::ArchiveDeleted(ref p) => (!out(p)).then_some(event),
        })
        .collect()
}

async fn flush_run(run: &mut Vec<IndexEvent>, cfg: &Arc<Config>, stores: &Stores) {
    if run.is_empty() {
        return;
    }
    let events = std::mem::take(run);
    let (cfg, stores) = (Arc::clone(cfg), stores.clone());
    tokio::task::spawn_blocking(move || apply_file_events(events, &cfg, &stores))
        .await
        .ok();
}

/// Removes an archive's row and its entries, from both stores.
fn remove_archive(sqlite: &FileStore, tantivy: &TantivyStore, path: &str) {
    let inner = sqlite.paths_in_archive(path).unwrap_or_default();
    for p in inner
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(path))
    {
        if let Err(e) = tantivy.remove(p) {
            tracing::warn!("tantivy remove: {e}");
        }
    }
    if let Err(e) = sqlite.remove_archive_entries(path) {
        tracing::warn!("remove entries of {path}: {e}");
    }
    if let Err(e) = sqlite.remove_file(path) {
        tracing::warn!("remove {path}: {e}");
    }
}

/// Everything under `dir`, as the crawl would record it.
fn records_under(dir: &Path, excl: &Arc<ExclusionEngine>) -> Vec<FileRecord> {
    let prune = Arc::clone(excl);
    let walker = super::walk_builder(dir)
        .filter_entry(move |e| !prune.should_exclude(e.path(), is_hidden_name(e.path())))
        .build();
    walker
        .skip(1)
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let meta = entry.metadata().ok()?;
            let path = entry.path();
            Some(build_file_record(
                path,
                &meta,
                is_hidden_name(path),
                unix_now(),
            ))
        })
        .collect()
}

pub async fn process_event(
    event: IndexEvent,
    config: Arc<RwLock<Config>>,
    sqlite: Arc<FileStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
    registry: Arc<ArchiveRegistry>,
) {
    let cfg = config.read().await.clone();

    match event {
        event @ (IndexEvent::Created(_) | IndexEvent::Modified(_) | IndexEvent::Deleted(_)) => {
            let stores = Stores {
                sqlite,
                tantivy,
                metrics,
                registry,
            };
            tokio::task::spawn_blocking(move || apply_file_events(vec![event], &cfg, &stores))
                .await
                .ok();
        }

        IndexEvent::Renamed { from, to } => {
            let stores = Stores {
                sqlite: Arc::clone(&sqlite),
                tantivy: Arc::clone(&tantivy),
                metrics: Arc::clone(&metrics),
                registry: Arc::clone(&registry),
            };
            let side_work = SideWork::new(&cfg, &to, &stores, None);
            let excl = ExclusionEngine::shared(&cfg.exclusions);
            tokio::task::spawn_blocking(move || {
                let from_str = from.to_string_lossy().into_owned();
                // Full-text documents are keyed by path, so everything under
                // the old name goes, archive entries included.
                let old = if indexed_dir(&sqlite, &from_str) {
                    sqlite.paths_under(&from_str).unwrap_or_default()
                } else {
                    Vec::new()
                };
                for path in old.iter().chain(std::iter::once(&from_str)) {
                    let _ = tantivy.remove(path);
                }
                let _ = sqlite.remove_archive_entries(&from_str);
                let _ = sqlite.remove_file(&from_str);
                let _ = sqlite.remove_directory(&from_str);
                registry.unregister(&from);

                let Ok(meta) = std::fs::metadata(&to) else {
                    return;
                };
                let mut moved = vec![build_file_record(
                    &to,
                    &meta,
                    is_hidden_name(&to),
                    unix_now(),
                )];
                // A directory rename is one event: nothing is reported for
                // its contents, which now live under the new name.
                if meta.is_dir() {
                    match &excl {
                        Ok(excl) => moved.extend(records_under(&to, excl)),
                        Err(e) => tracing::warn!("rename: exclusions: {e}"),
                    }
                }
                if let Err(e) = sqlite.upsert_files(&moved) {
                    tracing::warn!("rename upsert under {}: {e}", to.display());
                    return;
                }
                for record in &moved {
                    side_work.run(record);
                }
            })
            .await
            .ok();
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::ArchiveModified(path) => {
            let stores = Stores {
                sqlite: Arc::clone(&sqlite),
                tantivy: Arc::clone(&tantivy),
                metrics: Arc::clone(&metrics),
                registry: Arc::clone(&registry),
            };
            let side_work = SideWork::new(&cfg, &path, &stores, None);
            // Not awaited: a large archive takes a while, and the next event
            // need not wait for it.
            tokio::task::spawn_blocking(move || {
                let Ok(meta) = std::fs::metadata(&path) else {
                    // FSEvents can report a deletion as a modification.
                    let path_str = path.to_string_lossy();
                    remove_archive(&side_work.sqlite, &side_work.tantivy, &path_str);
                    side_work.registry.unregister(&path);
                    return;
                };
                let record = build_file_record(&path, &meta, is_hidden_name(&path), unix_now());
                if let Err(e) = side_work.sqlite.upsert_file(&record) {
                    tracing::warn!("ArchiveModified upsert: {e}");
                    return;
                }
                side_work.run(&record);
                // Deleted while this ran: the delete's cleanup may have come
                // before these rows, so they go now.
                if !path.exists() {
                    remove_archive(&side_work.sqlite, &side_work.tantivy, &record.path);
                }
            });
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::ArchiveDeleted(path) => {
            registry.unregister(&path);
            let path_str = path.to_string_lossy().into_owned();
            tokio::task::spawn_blocking(move || remove_archive(&sqlite, &tantivy, &path_str))
                .await
                .ok();
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
