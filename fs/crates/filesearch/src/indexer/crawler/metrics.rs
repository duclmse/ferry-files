use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

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
