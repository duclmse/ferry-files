use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use fs_protocol::ipc_protocol::RootState;

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
    /// Each watch path's crawl state, by path as the index stores it.
    roots: RwLock<HashMap<String, Root>>,
}

struct Root {
    state: RootState,
    /// Entries its last or running crawl wrote; the crawl writer adds to it
    /// per batch.
    entries: Arc<AtomicU64>,
    /// Unix seconds its last crawl started and finished; 0 if none did here.
    started: u64,
    finished: u64,
}

/// A watch path's crawl, as the APIs report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootProgress {
    pub path: String,
    pub state: RootState,
    pub entries: u64,
    pub started: u64,
    pub finished: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl IndexMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sets `root`'s state. `Crawling` starts its count from zero and
    /// returns the counter the crawl adds to; `Ready` with `entries` sets
    /// the count the crawl ended with.
    pub fn set_root(&self, root: &str, state: RootState, entries: Option<u64>) -> Arc<AtomicU64> {
        let Ok(mut roots) = self.roots.write() else {
            return Arc::new(AtomicU64::new(0));
        };
        let r = roots.entry(root.to_owned()).or_insert_with(|| Root {
            state,
            entries: Arc::new(AtomicU64::new(0)),
            started: 0,
            finished: 0,
        });
        match state {
            RootState::Crawling => {
                r.entries.store(0, Ordering::Relaxed);
                r.started = unix_now();
            }
            RootState::Ready if r.state == RootState::Crawling => r.finished = unix_now(),
            _ => {}
        }
        if let Some(n) = entries {
            r.entries.store(n, Ordering::Relaxed);
        }
        r.state = state;
        Arc::clone(&r.entries)
    }

    /// Forgets the state of watch paths that are no longer listed.
    pub fn keep_roots(&self, roots: &[PathBuf]) {
        let keep: Vec<String> = roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        if let Ok(mut map) = self.roots.write() {
            map.retain(|k, _| keep.contains(k));
        }
    }

    /// The progress of each of `roots`, in that order. A path with no state
    /// yet is pending.
    pub fn roots(&self, roots: &[PathBuf]) -> Vec<RootProgress> {
        let map = self.roots.read().ok();
        roots
            .iter()
            .map(|r| {
                let path = r.to_string_lossy().into_owned();
                match map.as_ref().and_then(|m| m.get(&path)) {
                    Some(s) => RootProgress {
                        state: s.state,
                        entries: s.entries.load(Ordering::Relaxed),
                        started: s.started,
                        finished: s.finished,
                        path,
                    },
                    None => RootProgress {
                        path,
                        state: RootState::Pending,
                        entries: 0,
                        started: 0,
                        finished: 0,
                    },
                }
            })
            .collect()
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
            roots: RwLock::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_moves_from_pending_through_crawling_to_ready() {
        let m = IndexMetrics::default();
        let roots = [PathBuf::from("/a"), PathBuf::from("/b")];
        assert!(m
            .roots(&roots)
            .iter()
            .all(|r| r.state == RootState::Pending));
        let count = m.set_root("/a", RootState::Crawling, None);
        count.fetch_add(10, Ordering::Relaxed);
        let now = m.roots(&roots);
        assert_eq!((now[0].state, now[0].entries), (RootState::Crawling, 10));
        assert!(now[0].started > 0);
        m.set_root("/a", RootState::Ready, Some(12));
        let now = m.roots(&roots);
        assert_eq!((now[0].state, now[0].entries), (RootState::Ready, 12));
        assert!(now[0].finished >= now[0].started);
        assert_eq!(now[1].state, RootState::Pending);
        m.keep_roots(&roots[1..]);
        assert_eq!(m.roots(&roots)[0].state, RootState::Pending);
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
