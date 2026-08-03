use std::collections::HashMap;

use super::FileRecord;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

pub(super) struct IndexState {
    pub(super) records:        Vec<FileRecord>,
    pub(super) by_path:        HashMap<String, usize>,
    pub(super) meta:           HashMap<String, String>,
    /// Crawl-generation tracking: path → (generation, watch_root)
    pub(super) crawl_info:     HashMap<String, (i64, String)>,
    pub(super) crawl_gen:      i64,
    /// USN/inotify journal position recorded at last clean shutdown.
    /// Used for dirty-shutdown recovery: replay events from this position.
    pub(super) usn_checkpoint: u64,
}

impl IndexState {
    pub(super) fn empty() -> Self {
        Self {
            records:        Vec::new(),
            by_path:        HashMap::new(),
            meta:           HashMap::new(),
            crawl_info:     HashMap::new(),
            crawl_gen:      0,
            usn_checkpoint: 0,
        }
    }

    pub(super) fn from_records(records: Vec<FileRecord>) -> Self {
        let mut by_path = HashMap::with_capacity(records.len());
        for (i, r) in records.iter().enumerate() {
            by_path.insert(r.path.clone(), i);
        }
        Self {
            records,
            by_path,
            meta:           HashMap::new(),
            crawl_info:     HashMap::new(),
            crawl_gen:      0,
            usn_checkpoint: 0,
        }
    }

    pub(super) fn upsert(&mut self, record: FileRecord) {
        if let Some(&idx) = self.by_path.get(&record.path) {
            self.records[idx] = record;
        } else {
            let idx = self.records.len();
            self.by_path.insert(record.path.clone(), idx);
            self.records.push(record);
        }
    }

    /// Swap-remove so indices stay valid: moves the last element to fill the gap.
    pub(super) fn remove(&mut self, path: &str) -> bool {
        if let Some(idx) = self.by_path.remove(path) {
            let last = self.records.len() - 1;
            if idx == last {
                self.records.pop();
            } else {
                self.records.swap_remove(idx);
                let moved_path = self.records[idx].path.clone();
                self.by_path.insert(moved_path, idx);
            }
            true
        } else {
            false
        }
    }
}
