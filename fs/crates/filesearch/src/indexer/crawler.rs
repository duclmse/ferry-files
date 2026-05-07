use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::store::{
    sqlite_store::SqliteStore,
    tantivy_store::TantivyStore,
    FileRecord,
};

use super::archive::{ArchiveCrawler, archive_entry_to_record};
use super::content::{content_limit, extract_content};
use super::exclusions::ExclusionEngine;

// ---------------------------------------------------------------------------
// IndexMetrics
// ---------------------------------------------------------------------------

pub struct IndexMetrics {
    pub total_files:             AtomicU64,
    pub total_dirs:              AtomicU64,
    pub content_indexed:         AtomicU64,
    pub last_crawl_secs:         AtomicU64,
    pub events_processed:        AtomicU64,
    pub archive_entries_indexed: AtomicU64,
    pub archives_crawled:        AtomicU64,
}

impl IndexMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            total_files:             AtomicU64::new(0),
            total_dirs:              AtomicU64::new(0),
            content_indexed:         AtomicU64::new(0),
            last_crawl_secs:         AtomicU64::new(0),
            events_processed:        AtomicU64::new(0),
            archive_entries_indexed: AtomicU64::new(0),
            archives_crawled:        AtomicU64::new(0),
        })
    }
}

impl Default for IndexMetrics {
    fn default() -> Self {
        Self {
            total_files:             AtomicU64::new(0),
            total_dirs:              AtomicU64::new(0),
            content_indexed:         AtomicU64::new(0),
            last_crawl_secs:         AtomicU64::new(0),
            events_processed:        AtomicU64::new(0),
            archive_entries_indexed: AtomicU64::new(0),
            archives_crawled:        AtomicU64::new(0),
        }
    }
}

// ---------------------------------------------------------------------------
// IndexEvent
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum IndexEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
    ArchiveModified(PathBuf),
    ArchiveDeleted(PathBuf),
}

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn is_hidden_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// Build platform-specific attribute bitmask from filesystem metadata.
#[cfg_attr(windows, allow(unused_variables))]
pub fn attrib_from_metadata(meta: &std::fs::Metadata, path: &Path) -> u32 {
    use crate::store::{
        ATTRIB_ARCHIVE, ATTRIB_COMPRESSED, ATTRIB_DIRECTORY, ATTRIB_ENCRYPTED,
        ATTRIB_HIDDEN, ATTRIB_NORMAL, ATTRIB_OFFLINE, ATTRIB_READONLY,
        ATTRIB_REPARSE_POINT, ATTRIB_SPARSE, ATTRIB_SYSTEM, ATTRIB_TEMPORARY,
    };

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let a = meta.file_attributes();
        let mut m: u32 = 0;
        if a & 0x0020 != 0 { m |= ATTRIB_ARCHIVE; }
        if a & 0x0800 != 0 { m |= ATTRIB_COMPRESSED; }
        if a & 0x0010 != 0 { m |= ATTRIB_DIRECTORY; }
        if a & 0x4000 != 0 { m |= ATTRIB_ENCRYPTED; }
        if a & 0x0002 != 0 { m |= ATTRIB_HIDDEN; }
        if a & 0x0400 != 0 { m |= ATTRIB_REPARSE_POINT; }
        if a & 0x0080 != 0 { m |= ATTRIB_NORMAL; }
        if a & 0x1000 != 0 { m |= ATTRIB_OFFLINE; }
        if a & 0x0200 != 0 { m |= ATTRIB_SPARSE; }
        if a & 0x0001 != 0 { m |= ATTRIB_READONLY; }
        if a & 0x0004 != 0 { m |= ATTRIB_SYSTEM; }
        if a & 0x0100 != 0 { m |= ATTRIB_TEMPORARY; }
        return m;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut m: u32 = 0;
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
        {
            m |= ATTRIB_HIDDEN;
        }
        if meta.mode() & 0o200 == 0 {
            m |= ATTRIB_READONLY;
        }
        return m;
    }

    #[allow(unreachable_code)]
    {
        let _ = (meta, path, ATTRIB_HIDDEN, ATTRIB_READONLY);
        0
    }
}

fn build_file_record(path: &Path, meta: &std::fs::Metadata, is_hidden: bool, now: i64) -> FileRecord {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let ctime = {
        #[cfg(unix)]
        { use std::os::unix::fs::MetadataExt; meta.ctime() }
        #[cfg(windows)]
        {
            meta.created()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(mtime)
        }
        #[cfg(not(any(unix, windows)))]
        { mtime }
    };

    let is_dir    = meta.is_dir();
    let is_symlink = meta.file_type().is_symlink();
    let size_bytes = if is_dir { 0 } else { meta.len() };
    let name       = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
    let extension  = if is_dir {
        None
    } else {
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase())
    };

    FileRecord {
        id:              None,
        path:            path.to_string_lossy().into_owned(),
        name,
        extension,
        size_bytes,
        mtime,
        ctime,
        is_dir,
        is_hidden,
        is_symlink,
        attrib_bitmask:  attrib_from_metadata(meta, path),
        content_indexed: false,
        tantivy_id:      None,
        indexed_at:      now,
        archive_path:    None,
    }
}

// ---------------------------------------------------------------------------
// crawl_archive — wraps all inner upserts in a single SQLite transaction
// ---------------------------------------------------------------------------

fn crawl_archive_sync(
    archive_path: &Path,
    crawler:      &ArchiveCrawler<'_>,
    sqlite:       &SqliteStore,
    tantivy:      &TantivyStore,
    metrics:      &IndexMetrics,
) -> Result<()> {
    let archive_str = archive_path.to_str().unwrap_or_default();
    sqlite.remove_archive_entries(archive_str)?;

    let conn = sqlite.get_conn()?;
    conn.execute_batch("BEGIN").map_err(AppError::Database)?;

    for entry_result in crawler.crawl(archive_path, 0)? {
        let entry = match entry_result {
            Ok(e)  => e,
            Err(e) => {
                tracing::warn!("archive entry error in {}: {e}", archive_path.display());
                continue;
            }
        };
        let record = archive_entry_to_record(&entry);
        sqlite.upsert_file_conn(&conn, &record)?;
        if let Some(content) = &entry.content {
            if let Err(e) = tantivy.upsert(&record, Some(content)) {
                tracing::warn!("tantivy upsert error: {e}");
            }
            metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
        }
        metrics.archive_entries_indexed.fetch_add(1, Ordering::Relaxed);
    }

    conn.execute_batch("COMMIT").map_err(AppError::Database)?;
    metrics.archives_crawled.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

// ---------------------------------------------------------------------------
// full_crawl
// ---------------------------------------------------------------------------

pub async fn full_crawl(
    config:   Arc<RwLock<Config>>,
    sqlite:   Arc<SqliteStore>,
    tantivy:  Arc<TantivyStore>,
    metrics:  Arc<IndexMetrics>,
    shutdown: CancellationToken,
) -> Result<()> {
    let cfg = config.read().await.clone();

    let exclusions  = Arc::new(ExclusionEngine::new(&cfg.exclusions)?);
    let archive_cfg = Arc::new(cfg.archive_index.clone());
    let watch_paths = cfg.watch_paths.clone();

    // One generation per full crawl. Each upserted row is marked with the
    // watch_root it was found under and this generation. After each root
    // finishes, rows with an older generation are stale and deleted in SQL —
    // no full path list is materialised in Rust memory.
    let generation = sqlite.begin_crawl_generation()?;

    let sqlite_w    = Arc::clone(&sqlite);
    let tantivy_w   = Arc::clone(&tantivy);
    let metrics_w   = Arc::clone(&metrics);
    let excl_w      = Arc::clone(&exclusions);
    let arc_cfg_w   = Arc::clone(&archive_cfg);
    let cnt_cfg_w   = Arc::new(cfg.content.clone());
    let shutdown_w  = shutdown.clone();

    tokio::task::spawn_blocking(move || -> Result<()> {
        for watch_path in &watch_paths {
            if shutdown_w.is_cancelled() {
                break;
            }

            let watch_root = watch_path.to_string_lossy().into_owned();

            let walker = WalkBuilder::new(watch_path)
                .hidden(false)
                .git_ignore(false)
                .build_parallel();

            let sqlite  = Arc::clone(&sqlite_w);
            let tantivy = Arc::clone(&tantivy_w);
            let metrics = Arc::clone(&metrics_w);
            let excl    = Arc::clone(&excl_w);
            let arc_cfg = Arc::clone(&arc_cfg_w);
            let cnt_cfg = Arc::clone(&cnt_cfg_w);
            let root_str = watch_root.clone();

            walker.run(|| {
                let sqlite   = Arc::clone(&sqlite);
                let tantivy  = Arc::clone(&tantivy);
                let metrics  = Arc::clone(&metrics);
                let excl     = Arc::clone(&excl);
                let arc_cfg  = Arc::clone(&arc_cfg);
                let cnt_cfg  = Arc::clone(&cnt_cfg);
                let root_str = root_str.clone();

                Box::new(move |result| {
                    let entry = match result {
                        Ok(e)  => e,
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

                    let now    = unix_now();
                    let record = build_file_record(path, &meta, is_hidden, now);
                    let path_str = record.path.clone();

                    match sqlite.upsert_file(&record) {
                        Ok(_) => {
                            // Mark as seen in this generation so stale cleanup
                            // can skip it.
                            if let Err(e) = sqlite.mark_seen(&path_str, &root_str, generation) {
                                tracing::warn!("mark_seen error for {path_str}: {e}");
                            }
                            if record.is_dir {
                                metrics.total_dirs.fetch_add(1, Ordering::Relaxed);
                            } else {
                                metrics.total_files.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(e) => tracing::warn!("upsert error for {}: {e}", path.display()),
                    }

                    // Content indexing — sync read for small files (≤1MB) in the walk closure
                    if !meta.is_dir() {
                        if let Some(max_bytes) = content_limit(path, &cnt_cfg) {
                            if meta.len() <= 1024 * 1024 {
                                if let Ok(raw) = std::fs::read(path) {
                                    let end = (max_bytes as usize).min(raw.len());
                                    if let Some(text) = super::content::extract_content_from_bytes(&raw[..end]) {
                                        let mut r = record.clone();
                                        r.content_indexed = true;
                                        if tantivy.upsert(&r, Some(&text)).is_ok() {
                                            metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Archive indexing
                    if !meta.is_dir() && arc_cfg.enabled {
                        let is_archive = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| arc_cfg.formats.iter().any(|f| f.eq_ignore_ascii_case(e)))
                            .unwrap_or(false);

                        if is_archive {
                            let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
                            if let Err(e) =
                                crawl_archive_sync(path, &crawler, &sqlite, &tantivy, &metrics)
                            {
                                tracing::warn!("archive crawl error for {}: {e}", path.display());
                            }
                        }
                    }

                    ignore::WalkState::Continue
                })
            });

            // Remove stale rows for this watch root in SQL — no Rust Vec of paths.
            let removed = sqlite_w.remove_stale_for_root(&watch_root, generation)?;
            if removed > 0 {
                tracing::info!("reconcile: removed {removed} stale entries under {watch_root}");
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| AppError::Index(e.to_string()))??;

    // Final commit
    if let Err(e) = tantivy.commit() {
        tracing::error!("final tantivy commit: {e}");
    }
    if let Err(e) = tantivy.reload_reader() {
        tracing::error!("tantivy reload: {e}");
    }

    let now = unix_now();
    metrics.last_crawl_secs.store(now as u64, Ordering::Relaxed);
    let _ = sqlite.set_meta("last_crawl_time", &now.to_string());
    let total = sqlite.total_files().unwrap_or(0);
    let _ = sqlite.set_meta("total_files", &total.to_string());

    Ok(())
}

// ---------------------------------------------------------------------------
// process_event — incremental update from watcher
// ---------------------------------------------------------------------------

pub async fn process_event(
    event:   IndexEvent,
    config:  Arc<RwLock<Config>>,
    sqlite:  Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    metrics: Arc<IndexMetrics>,
) {
    let cfg = config.read().await.clone();

    match event {
        IndexEvent::Created(path) | IndexEvent::Modified(path) => {
            let Ok(meta) = std::fs::metadata(&path) else { return; };
            let is_hidden = is_hidden_name(&path);
            let now = unix_now();
            let record = build_file_record(&path, &meta, is_hidden, now);

            if let Err(e) = sqlite.upsert_file(&record) {
                tracing::warn!("process_event upsert: {e}");
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
                        if tantivy.upsert(&r, Some(&text)).is_ok() {
                            metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }

            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::Deleted(path) => {
            let path_str = path.to_string_lossy().into_owned();
            if let Err(e) = sqlite.remove_file(&path_str) {
                tracing::warn!("process_event remove: {e}");
            }
            if let Err(e) = tantivy.remove(&path_str) {
                tracing::warn!("tantivy remove: {e}");
            }
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::Renamed { from, to } => {
            let from_str = from.to_string_lossy().into_owned();
            // Remove old record(s)
            let _ = sqlite.remove_file(&from_str);
            let _ = tantivy.remove(&from_str);
            // If directory rename, remove all children under old prefix
            let _ = sqlite.remove_directory(&from_str);

            // Upsert new path
            if let Ok(meta) = std::fs::metadata(&to) {
                let is_hidden = is_hidden_name(&to);
                let now = unix_now();
                let record = build_file_record(&to, &meta, is_hidden, now);
                let _ = sqlite.upsert_file(&record);

                // Archive handling
                let is_archive = to
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| cfg.archive_index.formats.iter().any(|f| f.eq_ignore_ascii_case(e)))
                    .unwrap_or(false);
                if cfg.archive_index.enabled && is_archive {
                    let sqlite2  = Arc::clone(&sqlite);
                    let tantivy2 = Arc::clone(&tantivy);
                    let metrics2 = Arc::clone(&metrics);
                    let arc_cfg  = cfg.archive_index.clone();
                    let cnt_cfg  = cfg.content.clone();
                    let to2      = to.clone();
                    tokio::task::spawn_blocking(move || {
                        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
                        let _ = crawl_archive_sync(&to2, &crawler, &sqlite2, &tantivy2, &metrics2);
                    });
                }
            }
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::ArchiveModified(path) => {
            let arc_cfg = cfg.archive_index.clone();
            let cnt_cfg = cfg.content.clone();
            let sqlite2  = Arc::clone(&sqlite);
            let tantivy2 = Arc::clone(&tantivy);
            let metrics2 = Arc::clone(&metrics);
            tokio::task::spawn_blocking(move || {
                let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
                if let Err(e) = crawl_archive_sync(&path, &crawler, &sqlite2, &tantivy2, &metrics2) {
                    tracing::warn!("ArchiveModified crawl error: {e}");
                }
            });
            metrics.events_processed.fetch_add(1, Ordering::Relaxed);
        }

        IndexEvent::ArchiveDeleted(path) => {
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

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_sqlite(dir: &TempDir) -> Arc<SqliteStore> {
        Arc::new(SqliteStore::new(&dir.path().join("test.db")).unwrap())
    }

    fn make_tantivy(_dir: &TempDir) -> Arc<TantivyStore> {
        Arc::new(TantivyStore::new(Path::new(":memory:")).unwrap())
    }

    fn make_config(watch: &Path) -> Arc<RwLock<Config>> {
        use crate::config::*;
        Arc::new(RwLock::new(Config {
            watch_paths: vec![watch.to_path_buf()],
            data_dir: watch.join("data"),
            log_dir: watch.join("logs"),
            exclusions: ExclusionConfig::default(),
            content: ContentConfig { extension_limits: [("txt".to_string(), 65536)].into() },
            api: ApiConfig::default(),
            search: SearchConfig::default(),
            indexer: IndexerConfig::default(),
            archive_index: ArchiveIndexConfig::default(),
            macros: MacroConfig::default(),
        }))
    }

    #[tokio::test]
    async fn full_crawl_indexes_all_files() {
        let dir = TempDir::new().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("{i}.txt")), b"hello").unwrap();
        }
        let sqlite  = make_sqlite(&dir);
        let tantivy = make_tantivy(&dir);
        let metrics = IndexMetrics::new();
        let config  = make_config(dir.path());
        let token   = CancellationToken::new();

        full_crawl(config, sqlite.clone(), tantivy, metrics, token).await.unwrap();

        let total = sqlite.total_files().unwrap();
        assert!(total >= 5, "expected ≥5 files, got {total}");
    }

    #[tokio::test]
    async fn excluded_pattern_not_indexed() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("skip.log"), b"should be excluded").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"should be indexed").unwrap();

        use crate::config::*;
        let config = Arc::new(RwLock::new(Config {
            watch_paths: vec![dir.path().to_path_buf()],
            data_dir: dir.path().join("data"),
            log_dir: dir.path().join("logs"),
            exclusions: ExclusionConfig {
                path_patterns: vec!["**/*.log".into()],
                exclude_hidden: false,
                max_content_size_bytes: 0,
                skip_network_content: false,
            },
            content: ContentConfig { extension_limits: Default::default() },
            api: ApiConfig::default(),
            search: SearchConfig::default(),
            indexer: IndexerConfig::default(),
            archive_index: ArchiveIndexConfig::default(),
            macros: MacroConfig::default(),
        }));

        let tmpdir2 = TempDir::new().unwrap();
        let sqlite  = make_sqlite(&tmpdir2);
        let tantivy = make_tantivy(&tmpdir2);
        let metrics = IndexMetrics::new();
        let token   = CancellationToken::new();

        full_crawl(config, sqlite.clone(), tantivy, metrics, token).await.unwrap();

        let skip_path = dir.path().join("skip.log").to_string_lossy().into_owned();
        assert!(sqlite.get_by_path(&skip_path).unwrap().is_none(), "excluded file should not be indexed");
    }

    #[tokio::test]
    async fn delete_event_removes_record() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("victim.txt");
        std::fs::write(&file, b"bye").unwrap();

        let tmpdir2 = TempDir::new().unwrap();
        let sqlite  = make_sqlite(&tmpdir2);
        let tantivy = make_tantivy(&tmpdir2);
        let metrics = IndexMetrics::new();
        let config  = make_config(dir.path());

        // Insert the record manually
        let meta = std::fs::metadata(&file).unwrap();
        let record = build_file_record(&file, &meta, false, unix_now());
        sqlite.upsert_file(&record).unwrap();

        // Now fire Delete event
        process_event(
            IndexEvent::Deleted(file.clone()),
            config,
            sqlite.clone(),
            tantivy,
            metrics,
        ).await;

        let path_str = file.to_string_lossy().into_owned();
        assert!(sqlite.get_by_path(&path_str).unwrap().is_none());
    }
}
