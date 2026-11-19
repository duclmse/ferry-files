use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, mpsc, Arc};

use ignore::WalkBuilder;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::api::archive::ArchiveRegistry;
use crate::config::{Config, MftMode};
use crate::error::{AppError, Result};
use crate::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

use super::archive::ArchiveCrawler;
use super::content::{content_limit, extract_content};
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

            let walker = WalkBuilder::new(watch_path)
                .hidden(false)
                .git_ignore(false)
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
                    for item in rx {
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
                })
            };

            let (side_tx, side_rx) = mpsc::sync_channel::<FileRecord>(SIDE_QUEUE);
            let side_workers =
                spawn_side_workers(content_threads, &side_work, side_rx, &shutdown_w);

            let try_mft = match use_mft {
                MftMode::On => true,
                MftMode::Off => false,
                MftMode::Auto => {
                    super::mft::is_volume_root(watch_path) || mft_cache.has_drive_of(watch_path)
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
            let _ = writer_thread.join();

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
    let under = sqlite.paths_under(path).unwrap_or_default();
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
    let walker = WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(false)
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
        IndexEvent::Created(path) | IndexEvent::Modified(path) => {
            let Ok(meta) = std::fs::metadata(&path) else {
                // FSEvents reports an unpaired rename's old side as a
                // modification of a path that is gone.
                if matches!(std::fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
                {
                    let path_str = path.to_string_lossy().into_owned();
                    tokio::task::spawn_blocking(move || remove_path(&sqlite, &tantivy, &path_str))
                        .await
                        .ok();
                    metrics.events_processed.fetch_add(1, Ordering::Relaxed);
                }
                return;
            };
            let is_hidden = is_hidden_name(&path);
            let now = unix_now();
            let record = build_file_record(&path, &meta, is_hidden, now);

            // upsert_file/children_of/remove hold the SQLite write lock and can
            // block for up to busy_timeout under crawl-writer contention; run
            // them on a blocking-pool thread rather than the tokio worker.
            let sqlite_b = Arc::clone(&sqlite);
            let tantivy_b = Arc::clone(&tantivy);
            let record_b = record.clone();
            let path_b = path.clone();
            // A folder moved in, or renamed without a pairing the debouncer
            // saw, is reported alone; its contents arrive with no events.
            let subtree = meta.is_dir().then(|| {
                let stores = Stores {
                    sqlite: Arc::clone(&sqlite),
                    tantivy: Arc::clone(&tantivy),
                    metrics: Arc::clone(&metrics),
                    registry: Arc::clone(&registry),
                };
                (
                    SideWork::new(&cfg, &path, &stores, None),
                    ExclusionEngine::new(&cfg.exclusions).map(Arc::new),
                )
            });
            let upserted = tokio::task::spawn_blocking(move || {
                let unseen_dir =
                    subtree.is_some() && matches!(sqlite_b.get_by_path(&record_b.path), Ok(None));
                if let Err(e) = sqlite_b.upsert_file(&record_b) {
                    tracing::warn!("process_event upsert: {e}");
                    return false;
                }
                if let (true, Some((side_work, Ok(excl)))) = (unseen_dir, &subtree) {
                    let children = records_under(&path_b, excl);
                    if !children.is_empty() {
                        if let Err(e) = sqlite_b.upsert_files(&children) {
                            tracing::warn!("process_event subtree upsert: {e}");
                        }
                        for child in &children {
                            side_work.run(child);
                        }
                    }
                }

                // Stale-sibling reconciliation. notify-debouncer-full's FSEvents
                // rename pairing can silently swallow the "old path" side of a
                // rename entirely (its was_created() heuristic misfires when
                // FSEvents replays the file's original creation flags alongside
                // the rename-from flag), so a renamed-away file's row can be left
                // in the store forever with no Deleted/Renamed event ever firing
                // for it. Piggyback on every Created/Modified event to sweep
                // recorded siblings under the same parent that no longer exist
                // on disk, using the existing indexed `parent` column.
                if let Some(parent) = path_b.parent() {
                    let parent_str = parent.to_string_lossy().into_owned();
                    let siblings = if sweep_due(&parent_str) {
                        sqlite_b.children_of(&parent_str)
                    } else {
                        Ok(Vec::new())
                    };
                    if let Ok(siblings) = siblings {
                        for sibling in siblings {
                            if sibling == record_b.path {
                                continue;
                            }
                            if std::fs::symlink_metadata(&sibling).is_err() {
                                if let Err(e) = sqlite_b.remove_file(&sibling) {
                                    tracing::warn!("process_event stale-sibling remove: {e}");
                                }
                                if let Err(e) = tantivy_b.remove(&sibling) {
                                    tracing::warn!(
                                        "process_event stale-sibling tantivy remove: {e}"
                                    );
                                }
                            }
                        }
                    }
                }
                true
            })
            .await
            .unwrap_or(false);

            if !upserted {
                return;
            }

            // Content
            let cnt_cfg = super::content::effective_config(&cfg.content, &cfg.exclusions, &path);
            let climit = content_limit(&path, &cnt_cfg);
            if let Some(max_bytes) = climit {
                let tantivy = Arc::clone(&tantivy);
                let metrics = Arc::clone(&metrics);
                let mut r = record.clone();
                tokio::spawn(async move {
                    if let Ok(Some(text)) = extract_content(&path, max_bytes).await {
                        r.content_indexed = true;
                        // The writer's mutex is held for the length of a
                        // commit; waiting on it here held a runtime worker.
                        let indexed = tokio::task::spawn_blocking(move || {
                            tantivy.upsert(&r, Some(&text)).is_ok()
                        })
                        .await
                        .unwrap_or(false);
                        if indexed {
                            metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }

            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::Deleted(path) => {
            let path_str = path.to_string_lossy().into_owned();
            let sqlite_b = Arc::clone(&sqlite);
            let tantivy_b = Arc::clone(&tantivy);
            tokio::task::spawn_blocking(move || remove_path(&sqlite_b, &tantivy_b, &path_str))
                .await
                .ok();
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::Renamed { from, to } => {
            let stores = Stores {
                sqlite: Arc::clone(&sqlite),
                tantivy: Arc::clone(&tantivy),
                metrics: Arc::clone(&metrics),
                registry: Arc::clone(&registry),
            };
            let side_work = SideWork::new(&cfg, &to, &stores, None);
            let excl = ExclusionEngine::new(&cfg.exclusions).map(Arc::new);
            tokio::task::spawn_blocking(move || {
                let from_str = from.to_string_lossy().into_owned();
                // Full-text documents are keyed by path, so everything under
                // the old name goes, archive entries included.
                let old = sqlite.paths_under(&from_str).unwrap_or_default();
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
