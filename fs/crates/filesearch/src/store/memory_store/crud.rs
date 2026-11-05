use std::path::Path;
use std::sync::RwLock;

use crate::error::{AppError, Result};
use super::state::IndexState;
use super::{FileRecord, MemoryStore};

// ---------------------------------------------------------------------------
// Binary snapshot format
//
// Version 1 (legacy):
//   [4 bytes]  magic:      b"FSID"
//   [2 bytes]  version:    1u16 LE
//   [8 bytes]  data_len:   u64 LE
//   [data_len] postcard-encoded Vec<FileRecord>
//
// Version 2 (current):
//   [4 bytes]  magic:          b"FSID"
//   [2 bytes]  version:        2u16 LE
//   [8 bytes]  usn_checkpoint: u64 LE - USN/inotify journal position for dirty-shutdown recovery
//   [8 bytes]  data_len:       u64 LE
//   [data_len] postcard-encoded Vec<FileRecord>
// ---------------------------------------------------------------------------

const MAGIC:      &[u8; 4] = b"FSID";
const VERSION:    u16      = 2;
const HEADER_V1:  usize    = 4 + 2 + 8;      // magic + version + data_len
const HEADER_V2:  usize    = 4 + 2 + 8 + 8;  // magic + version + checkpoint + data_len
const HEADER:     usize    = HEADER_V2;

impl MemoryStore {
    /// Create a new store, loading a snapshot from `data_dir/index.bin` when one exists.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let bin = data_dir.join("index.bin");
        let state = if bin.exists() {
            match Self::load_snapshot(&bin) {
                Ok(s) => {
                    tracing::info!(
                        "memory store: loaded {} records from {}",
                        s.records.len(),
                        bin.display()
                    );
                    s
                }
                Err(e) => {
                    tracing::warn!("memory store: snapshot load failed ({e}), starting empty");
                    IndexState::empty()
                }
            }
        } else {
            IndexState::empty()
        };
        Ok(Self { state: RwLock::new(state) })
    }

    fn load_snapshot(path: &Path) -> Result<IndexState> {
        let bytes = std::fs::read(path).map_err(AppError::Io)?;
        if bytes.len() < HEADER_V1 {
            return Err(AppError::Index("corrupt index.bin: too short".into()));
        }
        if &bytes[..4] != MAGIC {
            return Err(AppError::Index("corrupt index.bin: bad magic".into()));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        let (data_offset, usn_checkpoint) = match version {
            1 => (HEADER_V1, 0u64),
            2 => {
                if bytes.len() < HEADER_V2 {
                    return Err(AppError::Index("corrupt index.bin: too short for v2".into()));
                }
                let cp = u64::from_le_bytes(
                    bytes[6..14].try_into()
                        .map_err(|_| AppError::Index("corrupt checkpoint field".into()))?,
                );
                (HEADER_V2, cp)
            }
            v => return Err(AppError::Index(format!("unsupported index version {v}"))),
        };
        let data_len_start = data_offset - 8; // data_len is always the last 8 bytes of the header
        let data_len = u64::from_le_bytes(
            bytes[data_len_start..data_offset].try_into()
                .map_err(|_| AppError::Index("corrupt data_len field".into()))?,
        ) as usize;
        if bytes.len() < data_offset + data_len {
            return Err(AppError::Index("corrupt index.bin: truncated data".into()));
        }
        let records: Vec<FileRecord> =
            postcard::from_bytes(&bytes[data_offset..data_offset + data_len])
                .map_err(|e| AppError::Index(format!("snapshot decode: {e}")))?;
        let mut state = IndexState::from_records(records);
        state.usn_checkpoint = usn_checkpoint;
        Ok(state)
    }

    /// Serialize all records to `data_dir/index.bin` in v2 format.
    pub fn flush_to_disk(&self, data_dir: &Path) -> Result<()> {
        let state = self.state.read().map_err(|_| AppError::Index("lock poisoned".into()))?;
        let encoded = postcard::to_allocvec(&state.records)
            .map_err(|e| AppError::Index(format!("snapshot encode: {e}")))?;
        let mut buf = Vec::with_capacity(HEADER + encoded.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&state.usn_checkpoint.to_le_bytes());
        buf.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
        buf.extend_from_slice(&encoded);
        let path = data_dir.join("index.bin");
        std::fs::write(&path, &buf).map_err(AppError::Io)?;
        tracing::info!(
            "memory store: flushed {} records (checkpoint={}) to {}",
            state.records.len(),
            state.usn_checkpoint,
            path.display(),
        );
        Ok(())
    }

    pub fn get_usn_checkpoint(&self) -> u64 {
        self.read_state().usn_checkpoint
    }

    pub fn set_usn_checkpoint(&self, checkpoint: u64) {
        self.write_state().usn_checkpoint = checkpoint;
    }

    // -----------------------------------------------------------------------
    // Upsert / delete
    // -----------------------------------------------------------------------

    pub fn upsert_file(&self, record: &FileRecord) {
        self.write_state().upsert(record.clone());
    }

    pub fn remove_file(&self, path: &str) -> bool {
        self.write_state().remove(path)
    }

    pub fn remove_directory(&self, path: &str) -> usize {
        let prefix = format!("{path}/");
        let mut state = self.write_state();
        let paths: Vec<String> = state.records.iter()
            .filter(|r| r.path.starts_with(&prefix))
            .map(|r| r.path.clone())
            .collect();
        let n = paths.len();
        for p in &paths {
            state.remove(p);
        }
        n
    }

    /// `tag`, when set, stamps every row with the current crawl's generation - 
    /// mirrors `SqliteStore::upsert_archive_batch`'s reasoning: without it,
    /// `remove_stale_for_root`'s `None => true` fallback deletes archive
    /// entries right after the walk that just inserted them finishes.
    pub fn upsert_archive_batch(&self, records: &[FileRecord], tag: Option<(&str, i64)>) {
        let mut state = self.write_state();
        for r in records {
            if let Some((watch_root, generation)) = tag {
                state.crawl_info.insert(r.path.clone(), (generation, watch_root.to_string()));
            }
            state.upsert(r.clone());
        }
    }

    // -----------------------------------------------------------------------
    // Crawl generation
    // -----------------------------------------------------------------------

    pub fn begin_crawl_generation(&self) -> i64 {
        let mut state = self.write_state();
        state.crawl_gen += 1;
        state.crawl_gen
    }

    pub fn batch_upsert_crawl(&self, batch: &[(FileRecord, String)], generation: i64) {
        let mut state = self.write_state();
        for (record, watch_root) in batch {
            state.crawl_info.insert(record.path.clone(), (generation, watch_root.clone()));
            state.upsert(record.clone());
        }
    }

    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> usize {
        let mut state = self.write_state();
        let stale: Vec<String> = state.records.iter()
            .filter(|r| {
                let under_root = r.path.starts_with(&format!("{watch_root}/"))
                    || r.path == watch_root;
                if !under_root { return false; }
                match state.crawl_info.get(&r.path) {
                    Some((gen, _)) => *gen < generation,
                    None => true,
                }
            })
            .map(|r| r.path.clone())
            .collect();
        let n = stale.len();
        for p in &stale {
            state.remove(p);
            state.crawl_info.remove(p);
        }
        n
    }

    pub fn remove_archive_entries(&self, archive_path: &str) -> usize {
        let mut state = self.write_state();
        let paths: Vec<String> = state.records.iter()
            .filter(|r| r.archive_path.as_deref() == Some(archive_path))
            .map(|r| r.path.clone())
            .collect();
        let n = paths.len();
        for p in &paths {
            state.remove(p);
        }
        n
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    pub fn get_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        let state = self.read_state();
        Ok(state.by_path.get(path).map(|&i| state.records[i].clone()))
    }

    pub fn total_files(&self) -> i64 {
        let state = self.read_state();
        state.records.iter().filter(|r| r.archive_path.is_none()).count() as i64
    }

    pub fn total_archive_entries(&self) -> i64 {
        let state = self.read_state();
        state.records.iter().filter(|r| r.archive_path.is_some()).count() as i64
    }

    // -----------------------------------------------------------------------
    // Metadata (key-value)
    // -----------------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.read_state().meta.get(key).cloned()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        self.write_state().meta.insert(key.to_owned(), value.to_owned());
    }
}
