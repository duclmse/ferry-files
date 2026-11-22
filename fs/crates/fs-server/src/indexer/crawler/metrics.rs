use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// IndexMetrics
// ---------------------------------------------------------------------------

pub struct IndexMetrics {
    pub total_files: AtomicU64,
    pub total_dirs: AtomicU64,
    pub content_indexed: AtomicU64,
    /// Unix time the last full crawl finished; 0 before the first.
    pub last_crawl_secs: AtomicU64,
    pub events_processed: AtomicU64,
    pub archive_entries_indexed: AtomicU64,
    pub archives_crawled: AtomicU64,
}

impl IndexMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            total_files: AtomicU64::new(0),
            total_dirs: AtomicU64::new(0),
            content_indexed: AtomicU64::new(0),
            last_crawl_secs: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            archive_entries_indexed: AtomicU64::new(0),
            archives_crawled: AtomicU64::new(0),
        })
    }
}

impl IndexMetrics {
    /// Sets the file and folder counts from the store. A crawl counts up as
    /// it goes, but nothing else does: a restart, watcher events and file
    /// lists change the store without touching the counters. Blocking.
    pub fn recount(&self, store: &crate::store::FileStore) {
        use std::sync::atomic::Ordering;
        match store.entry_counts() {
            Ok((files, dirs)) => {
                self.total_files.store(files, Ordering::Relaxed);
                self.total_dirs.store(dirs, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!("counting index entries: {e}"),
        }
    }
}

impl Default for IndexMetrics {
    fn default() -> Self {
        Self {
            total_files: AtomicU64::new(0),
            total_dirs: AtomicU64::new(0),
            content_indexed: AtomicU64::new(0),
            last_crawl_secs: AtomicU64::new(0),
            events_processed: AtomicU64::new(0),
            archive_entries_indexed: AtomicU64::new(0),
            archives_crawled: AtomicU64::new(0),
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
