use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use super::snapshot;
use super::state::IndexState;
use super::{FileRecord, MemoryStore, RecordView};
use crate::error::{AppError, Result};

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
// Version 3 (legacy): v2 header, then a postcard-encoded `snapshot::Snapshot`
//   (path arena, packed entries, extension table, side table, meta, crawl
//   generation). v1/v2 carried no `meta`, so `last_crawl_time` was lost and
//   every restart re-crawled.
//
// Version 4: as v3, but the arena holds path tails and each entry
//   names its parent entry (`state::Entry`) instead of holding a whole path.
//
// Version 5 (current): the v4 body in one zstd frame. The header adds the
//   uncompressed length before data_len, which is the compressed length.
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"FSID";
const VERSION: u16 = 5;
const HEADER_V1: usize = 4 + 2 + 8; // magic + version + data_len
const HEADER_V2: usize = 4 + 2 + 8 + 8; // magic + version + checkpoint + data_len
const HEADER_V5: usize = HEADER_V2 + 8; // ... + checkpoint + raw_len + data_len
const HEADER: usize = HEADER_V5;
/// Paths repeat their folders' words, so the fastest level already gets most
/// of the gain.
const ZSTD_LEVEL: i32 = 1;

fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// Like `read_exact`, but a short file is not an error: returns the bytes read.
fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

impl MemoryStore {
    /// See `IndexState::reserve`.
    pub fn reserve(&self, entries: usize, tail_bytes: usize) {
        self.write_state().reserve(entries, tail_bytes);
    }

    pub fn heap_summary(&self) -> String {
        self.read_state().heap_summary()
    }

    /// Create a new store, loading a snapshot from `data_dir/index.bin` when one exists.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let bin = data_dir.join("index.bin");
        let state = if bin.exists() {
            match Self::load_snapshot(&bin) {
                Ok(s) => {
                    tracing::info!("memory store: loaded {}", bin.display());
                    tracing::info!("memory store: {}", s.heap_summary());
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
        Ok(Self {
            state: RwLock::new(state),
            saved_revision: AtomicU64::new(0),
            saving: Mutex::new(()),
        })
    }

    fn load_snapshot(path: &Path) -> Result<IndexState> {
        let mut file = File::open(path).map_err(AppError::Io)?;
        let file_len = file.metadata().map_err(AppError::Io)?.len();
        let mut header = [0u8; HEADER_V5];
        let got = read_up_to(&mut file, &mut header).map_err(AppError::Io)?;
        if got < HEADER_V1 {
            return Err(AppError::Index("corrupt index.bin: too short".into()));
        }
        if &header[..4] != MAGIC {
            return Err(AppError::Index("corrupt index.bin: bad magic".into()));
        }
        let version = u16::from_le_bytes([header[4], header[5]]);
        let short = || AppError::Index("corrupt index.bin: header too short".into());
        let (data_offset, usn_checkpoint) = match version {
            1 => (HEADER_V1, 0u64),
            2..=4 if got < HEADER_V2 => return Err(short()),
            2..=4 => (HEADER_V2, le_u64(&header[6..14])),
            5 if got < HEADER_V5 => return Err(short()),
            5 => (HEADER_V5, le_u64(&header[6..14])),
            v => return Err(AppError::Index(format!("unsupported index version {v}"))),
        };
        // data_len is always the last 8 bytes of the header.
        let data_len = le_u64(&header[data_offset - 8..data_offset]);
        if file_len - (data_offset as u64) < data_len {
            return Err(AppError::Index("corrupt index.bin: truncated data".into()));
        }
        file.seek(SeekFrom::Start(data_offset as u64))
            .map_err(AppError::Io)?;
        let decode = |e: std::io::Error| AppError::Index(format!("snapshot decode: {e}"));
        let corrupt = |e: String| AppError::Index(format!("corrupt index.bin: {e}"));
        if version == 5 {
            let raw_len = le_u64(&header[HEADER_V5 - 16..HEADER_V5 - 8]);
            let snap = read_compressed(file.take(data_len), raw_len).map_err(decode)?;
            return IndexState::from_snapshot(snap, usn_checkpoint).map_err(corrupt);
        }
        if version == 4 {
            let snap = snapshot::read(&mut file, data_len).map_err(decode)?;
            return IndexState::from_snapshot(snap, usn_checkpoint).map_err(corrupt);
        }
        if version == 3 {
            let snap = snapshot::read(&mut file, data_len).map_err(decode)?;
            return IndexState::from_v3(snap, usn_checkpoint).map_err(corrupt);
        }
        let mut body = Vec::new();
        file.take(data_len)
            .read_to_end(&mut body)
            .map_err(AppError::Io)?;
        let records: Vec<FileRecord> = postcard::from_bytes(&body)
            .map_err(|e| AppError::Index(format!("snapshot decode: {e}")))?;
        let mut state = IndexState::from_records(records);
        state.usn_checkpoint = usn_checkpoint;
        Ok(state)
    }

    /// Serialize the index to `data_dir/index.bin` in v5 format.
    pub fn flush_to_disk(&self, data_dir: &Path) -> Result<()> {
        let _saving = self.saving.lock().unwrap_or_else(PoisonError::into_inner);
        let state = self.read_state();
        // Write-then-rename so a crash mid-write keeps the previous snapshot.
        let path = data_dir.join("index.bin");
        let tmp = data_dir.join("index.bin.tmp");
        let write = || -> std::io::Result<()> {
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
            w.write_all(MAGIC)?;
            w.write_all(&VERSION.to_le_bytes())?;
            w.write_all(&state.usn_checkpoint.to_le_bytes())?;
            // raw_len and data_len are patched in once the body is out.
            w.write_all(&[0u8; 16])?;
            let mut z = zstd::Encoder::new(&mut w, ZSTD_LEVEL)?;
            let raw_len = snapshot::write(&mut z, &state.to_snapshot())?;
            z.finish()?;
            let mut file = w.into_inner().map_err(|e| e.into_error())?;
            let data_len = file.stream_position()? - HEADER as u64;
            file.seek(SeekFrom::Start((HEADER - 16) as u64))?;
            file.write_all(&raw_len.to_le_bytes())?;
            file.write_all(&data_len.to_le_bytes())?;
            file.sync_all()
        };
        write().map_err(AppError::Io)?;
        std::fs::rename(&tmp, &path).map_err(AppError::Io)?;
        self.saved_revision
            .store(state.revision(), Ordering::Release);
        tracing::info!(
            "memory store: flushed {} records (checkpoint={}) to {}",
            state.len(),
            state.usn_checkpoint,
            path.display(),
        );
        Ok(())
    }

    /// Saves only if an entry changed since the last save. Metadata alone
    /// does not count: the change-log position moves every minute even on an
    /// idle tree, and an older position only means replaying an empty range.
    pub fn flush_if_changed(&self, data_dir: &Path) -> Result<bool> {
        let current = self.read_state().revision();
        if current == self.saved_revision.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.flush_to_disk(data_dir)?;
        Ok(true)
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
        let hits: Vec<u32> = state
            .iter()
            .filter(|e| pred(e))
            .map(|e| e.index())
            .collect();
        for &i in &hits {
            state.remove_at(i);
        }
        state.maybe_compact();
        hits.len()
    }

    pub fn remove_directory(&self, path: &str) -> usize {
        self.remove_where(|e| e.under(path).is_some())
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

    /// The memory store sweeps by path, so only the generation changes.
    pub fn retag_under(&self, dir: &str, generation: i64) -> usize {
        let generation = generation_u32(generation);
        let mut state = self.write_state();
        let hits: Vec<u32> = state
            .iter()
            .filter(|e| e.under(dir).is_some() || e.path_is(dir))
            .map(|e| e.index())
            .collect();
        for &i in &hits {
            state.set_generation(i, generation);
        }
        hits.len()
    }

    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> usize {
        let generation = generation_u32(generation);
        self.remove_where(|e| {
            (e.generation() == 0 || e.generation() < generation)
                && (e.under(watch_root).is_some() || e.path_is(watch_root))
        })
    }

    pub fn remove_archive_entries(&self, archive_path: &str) -> usize {
        self.remove_where(|e| e.is_archive() && e.with_archive_path(|a| a == Some(archive_path)))
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
        state.iter().filter(|e| !e.is_archive()).count() as i64
    }

    /// Real (not archive) files and folders.
    pub fn entry_counts(&self) -> (u64, u64) {
        let state = self.read_state();
        state
            .iter()
            .filter(|e| !e.is_archive())
            .fold(
                (0, 0),
                |(f, d), e| if e.is_dir() { (f, d + 1) } else { (f + 1, d) },
            )
    }

    pub fn total_archive_entries(&self) -> i64 {
        let state = self.read_state();
        state.iter().filter(|e| e.is_archive()).count() as i64
    }

    // -----------------------------------------------------------------------
    // Metadata (key-value)
    // -----------------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.read_state().meta.get(key).cloned()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        self.write_state()
            .meta
            .insert(key.to_owned(), value.to_owned());
    }
}

/// Generations count crawls; 4 billion crawls do not happen, so saturating
/// keeps ordering without widening every entry to 8 bytes.
fn generation_u32(generation: i64) -> u32 {
    generation.clamp(0, i64::from(u32::MAX)) as u32
}

/// Decompresses on a second thread while this one decodes: each took about
/// 85 ms of a 270 ms load at 2M entries, one after the other.
fn read_compressed(
    compressed: impl Read + Send,
    raw_len: u64,
) -> std::io::Result<snapshot::Snapshot<super::state::Entry>> {
    use std::sync::mpsc;
    const CHUNK: u64 = 1 << 20;
    let mut decoder = zstd::Decoder::new(compressed)?;
    // A few chunks ahead is enough to keep both threads busy.
    let (tx, rx) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(4);
    std::thread::scope(|scope| {
        scope.spawn(move || loop {
            let mut chunk = Vec::with_capacity(CHUNK as usize);
            match (&mut decoder).take(CHUNK).read_to_end(&mut chunk) {
                Ok(0) => return,
                Ok(_) => {
                    if tx.send(Ok(chunk)).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        });
        let body = ChunkReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
        };
        // `read` consumes `body`, receiver included, so a decode error also
        // ends the decompressing thread's next send before the scope joins.
        snapshot::read(body, raw_len)
    })
}

/// Reads the chunks `read_compressed` sends, in order; the sender hanging up
/// is the end of the data.
struct ChunkReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.chunk.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => (self.chunk, self.pos) = (chunk, 0),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.chunk.len() - self.pos);
        out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
