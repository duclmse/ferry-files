use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, mpsc, Arc};

use ignore::WalkBuilder;
use rayon::prelude::*;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::api::archive::ArchiveRegistry;
use crate::config::Config;
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
    arc_cfg: Arc<crate::config::ArchiveIndexConfig>,
    cnt_cfg: Arc<crate::config::ContentConfig>,
    zidx_dir: Arc<Option<PathBuf>>,
    dzx_dir: Arc<Option<PathBuf>>,
    registry: Arc<ArchiveRegistry>,
    dzx_min_entry_bytes: u64,
    dzx_max_concurrent_builds: usize,
    generation: i64,
    root: String,
}

impl SideWork {
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

    fn run(&self, path: &Path, is_dir: bool, len: u64, record: &FileRecord) {
        if is_dir {
            return;
        }
        // Content indexing - sync read for small files (≤1MB) in the walk closure
        if let Some(max_bytes) = content_limit(path, &self.cnt_cfg) {
            if len <= 1024 * 1024 {
                if let Ok(raw) = std::fs::read(path) {
                    let end = (max_bytes as usize).min(raw.len());
                    if let Some(text) = super::content::extract_content_from_bytes(&raw[..end]) {
                        let mut r = record.clone();
                        r.content_indexed = true;
                        if self.tantivy.upsert(&r, Some(&text)).is_ok() {
                            self.metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        // Archive indexing - has its own BEGIN/COMMIT, stays inline.
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
                Some((self.root.as_str(), self.generation)),
            ) {
                tracing::warn!("archive crawl error for {}: {e}", path.display());
            }
        }
    }
}

/// Returns false, having sent nothing, if the file table cannot be used.
///
/// Rows go out in walk order because the memory store shares a parent's path
/// only with entries added after it. The slower per-file work runs in
/// parallel afterwards, as it does on the walker's threads.
fn crawl_from_mft(
    root: &Path,
    cache: &mut MftCache,
    excl: &ExclusionEngine,
    tx: &mpsc::SyncSender<(FileRecord, String)>,
    root_str: &str,
    side_work: &SideWork,
) -> bool {
    let index = match cache.for_root(root) {
        Ok(index) => index,
        Err(e) => {
            tracing::warn!("file table unavailable, walking {}: {e}", root.display());
            return false;
        }
    };
    let now = unix_now();
    let mut extra = Vec::new();
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
        if side_work.wanted(Path::new(&row.path), row.is_dir) {
            extra.push(row.clone());
        }
        let _ = tx.send((row, root_str.to_owned()));
    }
    extra.par_iter().for_each(|row| {
        side_work.run(Path::new(&row.path), row.is_dir, row.size_bytes, row);
    });
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
    let archive_cfg = Arc::new(cfg.archive_index.clone());
    let watch_paths = cfg.watch_paths.clone();
    let zidx_dir: Arc<Option<PathBuf>> = Arc::new(if cfg.archive_service.enabled {
        Some(
            cfg.archive_service
                .zidx_dir
                .clone()
                .unwrap_or_else(|| cfg.data_dir.join("zidx")),
        )
    } else {
        None
    });
    let dzx_dir: Arc<Option<PathBuf>> = Arc::new(if cfg.archive_service.enabled {
        Some(
            cfg.archive_service
                .dzx_dir
                .clone()
                .unwrap_or_else(|| cfg.data_dir.join("dzx")),
        )
    } else {
        None
    });
    let dzx_min_entry_bytes = cfg.archive_service.dzx_min_entry_bytes;
    let dzx_max_concurrent_builds = cfg.archive_service.dzx_max_concurrent_builds;

    // One generation per full crawl. Each upserted row is marked with the
    // watch_root it was found under and this generation. After each root
    // finishes, rows with an older generation are stale and deleted in SQL -
    // no full path list is materialised in Rust memory.
    let generation = sqlite.begin_crawl_generation()?;

    let sqlite_w = Arc::clone(&sqlite);
    let tantivy_w = Arc::clone(&tantivy);
    let metrics_w = Arc::clone(&metrics);
    let excl_w = Arc::clone(&exclusions);
    let arc_cfg_w = Arc::clone(&archive_cfg);
    let cnt_cfg_w = Arc::new(cfg.content.clone());
    let zidx_dir_w = Arc::clone(&zidx_dir);
    let dzx_dir_w = Arc::clone(&dzx_dir);
    let registry_w = Arc::clone(&registry);
    let shutdown_w = shutdown.clone();

    let use_mft = cfg.indexer.use_mft;

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
                .build_parallel();

            let side_work = Arc::new(SideWork {
                sqlite: Arc::clone(&sqlite_w),
                tantivy: Arc::clone(&tantivy_w),
                metrics: Arc::clone(&metrics_w),
                arc_cfg: Arc::clone(&arc_cfg_w),
                cnt_cfg: Arc::clone(&cnt_cfg_w),
                zidx_dir: Arc::clone(&zidx_dir_w),
                dzx_dir: Arc::clone(&dzx_dir_w),
                registry: Arc::clone(&registry_w),
                dzx_min_entry_bytes,
                dzx_max_concurrent_builds,
                generation,
                root: watch_root.clone(),
            });
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

            let from_mft = use_mft
                && crawl_from_mft(
                    watch_path,
                    &mut mft_cache,
                    &excl,
                    &tx,
                    &root_str,
                    &side_work,
                );
            if !from_mft {
                walker.run(|| {
                    let tx_w = tx.clone();
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
                        let _ = tx_w.send((record.clone(), root_str.clone()));
                        side_work.run(path, meta.is_dir(), meta.len(), &record);
                        ignore::WalkState::Continue
                    })
                });
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
            let upserted = tokio::task::spawn_blocking(move || {
                if let Err(e) = sqlite_b.upsert_file(&record_b) {
                    tracing::warn!("process_event upsert: {e}");
                    return false;
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
            let climit = content_limit(&path, &cfg.content);
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
            tokio::task::spawn_blocking(move || {
                if let Err(e) = sqlite_b.remove_file(&path_str) {
                    tracing::warn!("process_event remove: {e}");
                }
                if let Err(e) = tantivy_b.remove(&path_str) {
                    tracing::warn!("tantivy remove: {e}");
                }
            })
            .await
            .ok();
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::Renamed { from, to } => {
            let from_str = from.to_string_lossy().into_owned();
            let sqlite_b = Arc::clone(&sqlite);
            let tantivy_b = Arc::clone(&tantivy);
            let to_b = to.clone();
            let to_exists = tokio::task::spawn_blocking(move || {
                // Remove old record(s)
                let _ = sqlite_b.remove_file(&from_str);
                let _ = tantivy_b.remove(&from_str);
                // If directory rename, remove all children under old prefix
                let _ = sqlite_b.remove_directory(&from_str);

                // Upsert new path
                let Ok(meta) = std::fs::metadata(&to_b) else {
                    return false;
                };
                let is_hidden = is_hidden_name(&to_b);
                let now = unix_now();
                let record = build_file_record(&to_b, &meta, is_hidden, now);
                let _ = sqlite_b.upsert_file(&record);
                true
            })
            .await
            .unwrap_or(false);

            if to_exists {
                // Archive handling
                let is_archive = to
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| {
                        cfg.archive_index
                            .formats
                            .iter()
                            .any(|f| f.eq_ignore_ascii_case(e))
                    })
                    .unwrap_or(false);
                if cfg.archive_index.enabled && is_archive {
                    let sqlite2 = Arc::clone(&sqlite);
                    let tantivy2 = Arc::clone(&tantivy);
                    let metrics2 = Arc::clone(&metrics);
                    let registry2 = Arc::clone(&registry);
                    let arc_cfg = cfg.archive_index.clone();
                    let cnt_cfg = cfg.content.clone();
                    let zidx_dir = if cfg.archive_service.enabled {
                        Some(
                            cfg.archive_service
                                .zidx_dir
                                .clone()
                                .unwrap_or_else(|| cfg.data_dir.join("zidx")),
                        )
                    } else {
                        None
                    };
                    let dzx_dir = if cfg.archive_service.enabled {
                        Some(
                            cfg.archive_service
                                .dzx_dir
                                .clone()
                                .unwrap_or_else(|| cfg.data_dir.join("dzx")),
                        )
                    } else {
                        None
                    };
                    let dzx_min_entry_bytes = cfg.archive_service.dzx_min_entry_bytes;
                    let dzx_max_concurrent_builds = cfg.archive_service.dzx_max_concurrent_builds;
                    let to2 = to.clone();
                    tokio::task::spawn_blocking(move || {
                        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
                        let _ = crawl_archive_sync(
                            &to2,
                            &crawler,
                            sqlite2.as_ref(),
                            &tantivy2,
                            &metrics2,
                            SidecarConfig {
                                zidx_dir: zidx_dir.as_deref(),
                                registry: Some(&registry2),
                                dzx: dzx_dir
                                    .as_deref()
                                    .map(|d| (d, dzx_min_entry_bytes, dzx_max_concurrent_builds)),
                            },
                            None,
                        );
                    });
                }
            }
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::ArchiveModified(path) => {
            let arc_cfg = cfg.archive_index.clone();
            let cnt_cfg = cfg.content.clone();
            let sqlite2 = Arc::clone(&sqlite);
            let tantivy2 = Arc::clone(&tantivy);
            let metrics2 = Arc::clone(&metrics);
            let registry2 = Arc::clone(&registry);
            let zidx_dir = if cfg.archive_service.enabled {
                Some(
                    cfg.archive_service
                        .zidx_dir
                        .clone()
                        .unwrap_or_else(|| cfg.data_dir.join("zidx")),
                )
            } else {
                None
            };
            let dzx_dir = if cfg.archive_service.enabled {
                Some(
                    cfg.archive_service
                        .dzx_dir
                        .clone()
                        .unwrap_or_else(|| cfg.data_dir.join("dzx")),
                )
            } else {
                None
            };
            let dzx_min_entry_bytes = cfg.archive_service.dzx_min_entry_bytes;
            let dzx_max_concurrent_builds = cfg.archive_service.dzx_max_concurrent_builds;
            tokio::task::spawn_blocking(move || {
                let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
                if let Err(e) = crawl_archive_sync(
                    &path,
                    &crawler,
                    sqlite2.as_ref(),
                    &tantivy2,
                    &metrics2,
                    SidecarConfig {
                        zidx_dir: zidx_dir.as_deref(),
                        registry: Some(&registry2),
                        dzx: dzx_dir
                            .as_deref()
                            .map(|d| (d, dzx_min_entry_bytes, dzx_max_concurrent_builds)),
                    },
                    None,
                ) {
                    tracing::warn!("ArchiveModified crawl error: {e}");
                }
            });
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::ArchiveDeleted(path) => {
            registry.unregister(&path);
            let path_str = path.to_string_lossy().into_owned();
            if let Err(e) = sqlite.remove_archive_entries(&path_str) {
                tracing::warn!("ArchiveDeleted remove_archive_entries: {e}");
            }
            if let Err(e) = sqlite.remove_file(&path_str) {
                tracing::warn!("ArchiveDeleted remove_file: {e}");
            }
            if let Err(e) = tantivy.remove(&path_str) {
                tracing::warn!("ArchiveDeleted tantivy remove: {e}");
            }
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
