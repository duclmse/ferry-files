use std::path::Path;
use std::sync::RwLock;

use crate::error::{AppError, Result};
use crate::store::is_under;
use super::state::{IndexState, Snapshot};
use super::{FileRecord, MemoryStore, RecordView};

// ---------------------------------------------------------------------------
// Binary snapshot format
//
// Version 1 (legacy):
//   [4 bytes]  magic:      b"FSID"
//   [2 bytes]  version:    1u16 LE
//   [8 bytes]  data_len:   u64 LE
//   [data_len] postcard-encoded Vec<FileRecord>
//
// Version 2 (legacy):
//   [4 bytes]  magic:          b"FSID"
//   [2 bytes]  version:        2u16 LE
//   [8 bytes]  usn_checkpoint: u64 LE - USN/inotify journal position for dirty-shutdown recovery
//   [8 bytes]  data_len:       u64 LE
//   [data_len] postcard-encoded Vec<FileRecord>
//
// Version 3 (current): v2 header, then a postcard-encoded `state::Snapshot`
//   (path arena, packed entries, extension table, side table, meta, crawl
//   generation). v1/v2 carried no `meta`, so `last_crawl_time` was lost and
//   every restart re-crawled.
// ---------------------------------------------------------------------------

const MAGIC:      &[u8; 4] = b"FSID";
const VERSION:    u16      = 3;
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
                        s.len(),
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
            2 | 3 => {
                if bytes.len() < HEADER_V2 {
                    return Err(AppError::Index("corrupt index.bin: header too short".into()));
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
        if bytes.len() - data_offset < data_len {
            return Err(AppError::Index("corrupt index.bin: truncated data".into()));
        }
        let body = &bytes[data_offset..data_offset + data_len];
        if version == 3 {
            let snap: Snapshot = postcard::from_bytes(body)
                .map_err(|e| AppError::Index(format!("snapshot decode: {e}")))?;
            return IndexState::from_snapshot(snap, usn_checkpoint)
                .map_err(|e| AppError::Index(format!("corrupt index.bin: {e}")));
        }
        let records: Vec<FileRecord> = postcard::from_bytes(body)
            .map_err(|e| AppError::Index(format!("snapshot decode: {e}")))?;
        let mut state = IndexState::from_records(records);
        state.usn_checkpoint = usn_checkpoint;
        Ok(state)
    }

    /// Serialize the index to `data_dir/index.bin` in v3 format.
    pub fn flush_to_disk(&self, data_dir: &Path) -> Result<()> {
        // Write lock only to drop arena garbage before saving it.
        self.write_state().compact();
        let state = self.read_state();
        let encoded = postcard::to_allocvec(&state.to_snapshot())
            .map_err(|e| AppError::Index(format!("snapshot encode: {e}")))?;
        let mut buf = Vec::with_capacity(HEADER + encoded.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&state.usn_checkpoint.to_le_bytes());
        buf.extend_from_slice(&(encoded.len() as u64).to_le_bytes());
        buf.extend_from_slice(&encoded);
        drop(encoded);
        // Write-then-rename so a crash mid-write keeps the previous snapshot.
        let path = data_dir.join("index.bin");
        let tmp  = data_dir.join("index.bin.tmp");
        std::fs::write(&tmp, &buf).map_err(AppError::Io)?;
        std::fs::rename(&tmp, &path).map_err(AppError::Io)?;
        tracing::info!(
            "memory store: flushed {} records (checkpoint={}) to {}",
            state.len(),
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
        self.write_state().upsert(record);
    }

    pub fn remove_file(&self, path: &str) -> bool {
        self.write_state().remove(path)
    }

    fn remove_where(&self, pred: impl Fn(&super::state::EntryRef<'_>) -> bool) -> usize {
        let mut state = self.write_state();
        let paths: Vec<String> = state.iter()
            .filter(|e| pred(e))
            .map(|e| e.path_str().to_owned())
            .collect();
        for p in &paths {
            state.remove(p);
        }
        paths.len()
    }

    pub fn remove_directory(&self, path: &str) -> usize {
        self.remove_where(|e| is_under(e.path_str(), path))
    }

    /// `tag`, when set, stamps every row with the current crawl's generation -
    /// mirrors `SqliteStore::upsert_archive_batch`'s reasoning: without it,
    /// `remove_stale_for_root`'s untagged-means-stale rule deletes archive
    /// entries right after the walk that just inserted them finishes.
    pub fn upsert_archive_batch(&self, records: &[FileRecord], tag: Option<(&str, i64)>) {
        let mut state = self.write_state();
        for r in records {
            let i = state.upsert(r);
            if let Some((_, generation)) = tag {
                state.set_generation(i, generation_u32(generation));
            }
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

    /// The watch root is not stored per entry: stale removal selects entries
    /// by path prefix, so only the generation is needed.
    pub fn batch_upsert_crawl(&self, batch: &[(FileRecord, String)], generation: i64) {
        let mut state = self.write_state();
        let generation = generation_u32(generation);
        for (record, _watch_root) in batch {
            let i = state.upsert(record);
            state.set_generation(i, generation);
        }
    }

    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> usize {
        let generation = generation_u32(generation);
        self.remove_where(|e| {
            let p = e.path_str();
            (p == watch_root || is_under(p, watch_root))
                && (e.generation() == 0 || e.generation() < generation)
        })
    }

    pub fn remove_archive_entries(&self, archive_path: &str) -> usize {
        self.remove_where(|e| e.archive_path() == Some(archive_path))
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    pub fn get_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        let state = self.read_state();
        Ok(state.find(path).map(|i| state.view(i).to_record()))
    }

    pub fn total_files(&self) -> i64 {
        let state = self.read_state();
        state.iter().filter(|e| e.archive_path().is_none()).count() as i64
    }

    pub fn total_archive_entries(&self) -> i64 {
        let state = self.read_state();
        state.iter().filter(|e| e.archive_path().is_some()).count() as i64
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

/// Generations count crawls; 4 billion crawls do not happen, so saturating
/// keeps ordering without widening every entry to 8 bytes.
fn generation_u32(generation: i64) -> u32 {
    generation.clamp(0, i64::from(u32::MAX)) as u32
}
