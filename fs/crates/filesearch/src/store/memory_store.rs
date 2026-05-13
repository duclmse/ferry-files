//! In-memory file-metadata store with binary snapshot persistence.
//!
//! Designed after the approach in `docs/design.md`: records live in a flat
//! `Vec<FileRecord>` plus a `HashMap` path-index for O(1) lookups. Snapshots
//! are written as a small binary file (`index.bin`) on clean shutdown and
//! reloaded on the next start — near-zero load latency via sequential read.
//!
//! This store is single-writer: all mutating methods take an exclusive lock.
//! Use the SQLite store when you need concurrent writers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::RwLock;

use crate::error::{AppError, Result};
use super::{CmpOp, DupeKind, FileRecord, MetaFilters, SqlCondition};

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
//   [8 bytes]  usn_checkpoint: u64 LE  — USN/inotify journal position for dirty-shutdown recovery
//   [8 bytes]  data_len:       u64 LE
//   [data_len] postcard-encoded Vec<FileRecord>
// ---------------------------------------------------------------------------

const MAGIC:      &[u8; 4] = b"FSID";
const VERSION:    u16      = 2;
const HEADER_V1:  usize    = 4 + 2 + 8;      // magic + version + data_len
const HEADER_V2:  usize    = 4 + 2 + 8 + 8;  // magic + version + checkpoint + data_len
const HEADER:     usize    = HEADER_V2;

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct IndexState {
    records:        Vec<FileRecord>,
    by_path:        HashMap<String, usize>,
    meta:           HashMap<String, String>,
    /// Crawl-generation tracking: path → (generation, watch_root)
    crawl_info:     HashMap<String, (i64, String)>,
    crawl_gen:      i64,
    /// USN/inotify journal position recorded at last clean shutdown.
    /// Used for dirty-shutdown recovery: replay events from this position.
    usn_checkpoint: u64,
}

impl IndexState {
    fn empty() -> Self {
        Self {
            records:        Vec::new(),
            by_path:        HashMap::new(),
            meta:           HashMap::new(),
            crawl_info:     HashMap::new(),
            crawl_gen:      0,
            usn_checkpoint: 0,
        }
    }

    fn from_records(records: Vec<FileRecord>) -> Self {
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

    fn upsert(&mut self, record: FileRecord) {
        if let Some(&idx) = self.by_path.get(&record.path) {
            self.records[idx] = record;
        } else {
            let idx = self.records.len();
            self.by_path.insert(record.path.clone(), idx);
            self.records.push(record);
        }
    }

    /// Swap-remove so indices stay valid: moves the last element to fill the gap.
    fn remove(&mut self, path: &str) -> bool {
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

// ---------------------------------------------------------------------------

pub struct MemoryStore {
    state:    RwLock<IndexState>,
}

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
        self.state.read().unwrap().usn_checkpoint
    }

    pub fn set_usn_checkpoint(&self, checkpoint: u64) {
        self.state.write().unwrap().usn_checkpoint = checkpoint;
    }

    // -----------------------------------------------------------------------
    // Upsert / delete
    // -----------------------------------------------------------------------

    pub fn upsert_file(&self, record: &FileRecord) {
        self.state.write().unwrap().upsert(record.clone());
    }

    pub fn remove_file(&self, path: &str) -> bool {
        self.state.write().unwrap().remove(path)
    }

    pub fn remove_directory(&self, path: &str) -> usize {
        let prefix = format!("{path}/");
        let mut state = self.state.write().unwrap();
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

    pub fn upsert_archive_batch(&self, records: &[FileRecord]) {
        let mut state = self.state.write().unwrap();
        for r in records {
            state.upsert(r.clone());
        }
    }

    // -----------------------------------------------------------------------
    // Crawl generation
    // -----------------------------------------------------------------------

    pub fn begin_crawl_generation(&self) -> i64 {
        let mut state = self.state.write().unwrap();
        state.crawl_gen += 1;
        state.crawl_gen
    }

    pub fn batch_upsert_crawl(&self, batch: &[(FileRecord, String)], generation: i64) {
        let mut state = self.state.write().unwrap();
        for (record, watch_root) in batch {
            state.crawl_info.insert(record.path.clone(), (generation, watch_root.clone()));
            state.upsert(record.clone());
        }
    }

    pub fn remove_stale_for_root(&self, watch_root: &str, generation: i64) -> usize {
        let mut state = self.state.write().unwrap();
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
        let mut state = self.state.write().unwrap();
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
        let state = self.state.read().unwrap();
        Ok(state.by_path.get(path).map(|&i| state.records[i].clone()))
    }

    pub fn total_files(&self) -> i64 {
        let state = self.state.read().unwrap();
        state.records.iter().filter(|r| r.archive_path.is_none()).count() as i64
    }

    pub fn total_archive_entries(&self) -> i64 {
        let state = self.state.read().unwrap();
        state.records.iter().filter(|r| r.archive_path.is_some()).count() as i64
    }

    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter().any(|r| {
            parent_dir(&r.path) == parent_path && like_match(&r.name, name_pattern)
        }))
    }

    pub fn list_archive_paths(&self) -> Result<Vec<String>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.archive_path.is_none() && !r.is_dir
                && r.extension.as_deref() == Some("zip"))
            .map(|r| r.path.clone())
            .collect())
    }

    pub fn search_meta(&self, filters: &MetaFilters, limit: usize) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| meta_filter_matches(r, filters))
            .take(limit)
            .cloned()
            .collect())
    }

    /// Mirror of `SqliteStore::search_name`: `name LIKE pattern` + optional meta-filters.
    pub fn search_name(
        &self,
        name:    &str,
        filters: &MetaFilters,
        limit:   usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| like_match(&r.name, name) && meta_filter_matches(r, filters))
            .take(limit)
            .cloned()
            .collect())
    }

    /// Mirror of `SqliteStore::search_extension`: exact extension match + optional meta-filters.
    pub fn search_extension(
        &self,
        ext:     &str,
        filters: &MetaFilters,
        limit:   usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.extension.as_deref() == Some(ext) && meta_filter_matches(r, filters))
            .take(limit)
            .cloned()
            .collect())
    }

    /// Mirror of `SqliteStore::paths_under`: all paths recursively under `dir`.
    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let prefix = format!("{dir}/");
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.path.starts_with(&prefix))
            .map(|r| r.path.clone())
            .collect())
    }

    pub fn search_conditions(&self, conds: &[SqlCondition], limit: usize) -> Result<Vec<FileRecord>> {
        if conds.is_empty() {
            return self.search_meta(&MetaFilters::default(), limit);
        }
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| conds.iter().all(|c| c.matches(r)))
            .take(limit)
            .cloned()
            .collect())
    }

    pub fn search_by_paths(
        &self,
        paths: &[String],
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        let mut results = Vec::new();
        for path in paths.iter().take(limit * 4) {
            if let Some(&idx) = state.by_path.get(path) {
                let r = &state.records[idx];
                if conds.is_empty() || conds.iter().all(|c| c.matches(r)) {
                    results.push(r.clone());
                }
            }
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn find_dupes(
        &self,
        kind:  DupeKind,
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();

        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in state.records.iter().enumerate() {
            if r.archive_path.is_some() { continue; }
            if !conds.is_empty() && !conds.iter().all(|c| c.matches(r)) { continue; }
            let key = dupe_key(r, kind);
            groups.entry(key).or_default().push(i);
        }

        let mut keys: Vec<&String> = groups.keys().collect();
        keys.sort();
        let mut result = Vec::new();
        'outer: for key in keys {
            let indices = &groups[key];
            if indices.len() <= 1 { continue; }
            for &idx in indices {
                result.push(state.records[idx].clone());
                if result.len() >= limit { break 'outer; }
            }
        }
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Metadata (key-value)
    // -----------------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.state.read().unwrap().meta.get(key).cloned()
    }

    pub fn set_meta(&self, key: &str, value: &str) {
        self.state.write().unwrap().meta.insert(key.to_owned(), value.to_owned());
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        None | Some(0) => "",
        Some(i) => &path[..i],
    }
}

fn dupe_key(r: &FileRecord, kind: DupeKind) -> String {
    match kind {
        DupeKind::Name => r.name.clone(),
        DupeKind::NamePart => r.extension.as_ref()
            .map(|e| {
                let suffix = format!(".{e}");
                r.name.strip_suffix(&suffix).unwrap_or(&r.name).to_owned()
            })
            .unwrap_or_else(|| r.name.clone()),
        DupeKind::Size         => r.size_bytes.to_string(),
        DupeKind::DateModified |
        DupeKind::DateAccessed => r.mtime.to_string(),
        DupeKind::DateCreated  => r.ctime.to_string(),
        DupeKind::Attrib       => r.attrib_bitmask.to_string(),
    }
}

fn meta_filter_matches(r: &FileRecord, f: &MetaFilters) -> bool {
    if let Some(min) = f.min_size  { if r.size_bytes < min { return false; } }
    if let Some(max) = f.max_size  { if r.size_bytes > max { return false; } }
    if let Some(after)  = f.after  { if r.mtime < after    { return false; } }
    if let Some(before) = f.before { if r.mtime > before   { return false; } }
    if let Some(is_dir) = f.is_dir { if r.is_dir != is_dir { return false; } }
    if let Some(exts) = &f.extensions {
        if !exts.is_empty() && !r.extension.as_ref().map(|e| exts.contains(e)).unwrap_or(false) {
            return false;
        }
    }
    if let Some(prefix) = &f.path_prefix {
        if !r.path.starts_with(&format!("{prefix}/")) { return false; }
    }
    true
}

/// SQLite LIKE semantics: case-insensitive ASCII, `%` = any sequence, `_` = any char.
///
/// Fast path: `%term%` (pure contains, no other wildcards) uses SIMD via `memchr::memmem`.
pub(crate) fn like_match(text: &str, pattern: &str) -> bool {
    if let Some(needle) = extract_contains_pattern(pattern) {
        let text_lower   = text.to_ascii_lowercase();
        let needle_lower = needle.to_ascii_lowercase();
        return memchr::memmem::find(text_lower.as_bytes(), needle_lower.as_bytes()).is_some();
    }
    like_inner(
        text.to_ascii_lowercase().as_bytes(),
        pattern.to_ascii_lowercase().as_bytes(),
    )
}

/// Returns the inner literal if `pattern` is exactly `%<literal>%` with no other `%` or `_`.
fn extract_contains_pattern(pattern: &str) -> Option<&str> {
    let b = pattern.as_bytes();
    if b.len() < 3 || b[0] != b'%' || *b.last().unwrap() != b'%' {
        return None;
    }
    let inner = &pattern[1..pattern.len() - 1];
    if inner.contains('%') || inner.contains('_') { None } else { Some(inner) }
}

fn like_inner(text: &[u8], pattern: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'%', rest @ ..] => (0..=text.len()).any(|i| like_inner(&text[i..], rest)),
        [b'_', rest @ ..] => !text.is_empty() && like_inner(&text[1..], rest),
        [c, rest @ ..] => !text.is_empty() && text[0] == *c && like_inner(&text[1..], rest),
    }
}

/// SQLite GLOB semantics: case-sensitive, `*` = any sequence, `?` = any char.
pub(crate) fn glob_match(text: &str, pattern: &str) -> bool {
    glob_inner(text.as_bytes(), pattern.as_bytes())
}

fn glob_inner(text: &[u8], pattern: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'*', rest @ ..] => (0..=text.len()).any(|i| glob_inner(&text[i..], rest)),
        [b'?', rest @ ..] => !text.is_empty() && glob_inner(&text[1..], rest),
        [c, rest @ ..] => !text.is_empty() && text[0] == *c && glob_inner(&text[1..], rest),
    }
}

fn count_slashes(path: &str) -> usize {
    path.bytes().filter(|&b| b == b'/').count()
}

// ---------------------------------------------------------------------------

impl SqlCondition {
    /// Evaluate this condition against a `FileRecord` in memory.
    /// Mirrors the SQLite semantics of [`SqlCondition::to_sql`].
    pub fn matches(&self, r: &FileRecord) -> bool {
        match self {
            SqlCondition::NameLike(p)      => like_match(&r.name, p),
            SqlCondition::NameGlob(p)      => glob_match(&r.name, p),
            SqlCondition::PathLike(p)      => like_match(&r.path, p),
            SqlCondition::IsDir(v)         => r.is_dir == *v,
            SqlCondition::MtimeRange(a, b) => r.mtime >= *a && r.mtime <= *b,
            SqlCondition::CtimeRange(a, b) => r.ctime >= *a && r.ctime <= *b,
            SqlCondition::SizeRange(a, b)  => r.size_bytes >= *a && r.size_bytes <= *b,
            SqlCondition::SizeOp(op, v)    => cmp_op_u64(*op, r.size_bytes, *v),
            SqlCondition::ExtIn(exts)      =>
                r.extension.as_ref().map(|e| exts.contains(e)).unwrap_or(false),
            SqlCondition::ParentPath(p)    => r.path.starts_with(&format!("{p}/")),
            SqlCondition::NameLenOp(op, n) => cmp_op_usize(*op, r.name.len(), *n),
            SqlCondition::DepthOp(op, d)   => cmp_op_usize(*op, count_slashes(&r.path), *d),
            SqlCondition::IsArchiveEntry   => r.archive_path.is_some(),
            SqlCondition::ArchiveOf(ap)    => r.archive_path.as_deref() == Some(ap.as_str()),
            SqlCondition::AttribAll(mask)  => r.attrib_bitmask & mask == *mask,
            SqlCondition::AttribAny(mask)  => r.attrib_bitmask & mask != 0,
        }
    }
}

fn cmp_op_u64(op: CmpOp, lhs: u64, rhs: u64) -> bool {
    match op {
        CmpOp::Lt  => lhs <  rhs,
        CmpOp::Lte => lhs <= rhs,
        CmpOp::Gt  => lhs >  rhs,
        CmpOp::Gte => lhs >= rhs,
        CmpOp::Eq  => lhs == rhs,
    }
}

fn cmp_op_usize(op: CmpOp, lhs: usize, rhs: usize) -> bool {
    match op {
        CmpOp::Lt  => lhs <  rhs,
        CmpOp::Lte => lhs <= rhs,
        CmpOp::Gt  => lhs >  rhs,
        CmpOp::Gte => lhs >= rhs,
        CmpOp::Eq  => lhs == rhs,
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(path: &str) -> FileRecord {
        FileRecord {
            id:              None,
            path:            path.to_owned(),
            name:            path.split('/').last().unwrap_or(path).to_owned(),
            extension:       Some("rs".into()),
            size_bytes:      1024,
            mtime:           1_000_000,
            ctime:           1_000_000,
            is_dir:          false,
            is_hidden:       false,
            is_symlink:      false,
            attrib_bitmask:  0,
            content_indexed: false,
            tantivy_id:      None,
            indexed_at:      1_000_000,
            archive_path:    None,
        }
    }

    fn make_store() -> MemoryStore {
        MemoryStore::new(std::path::Path::new(":memory-test:")).unwrap()
    }

    #[test]
    fn upsert_and_get() {
        let store = make_store();
        store.upsert_file(&sample("/a/b.rs"));
        assert!(store.get_by_path("/a/b.rs").unwrap().is_some());
        assert!(store.get_by_path("/a/c.rs").unwrap().is_none());
    }

    #[test]
    fn remove_updates_index() {
        let store = make_store();
        store.upsert_file(&sample("/a/b.rs"));
        store.upsert_file(&sample("/a/c.rs"));
        store.remove_file("/a/b.rs");
        assert!(store.get_by_path("/a/b.rs").unwrap().is_none());
        assert!(store.get_by_path("/a/c.rs").unwrap().is_some());
    }

    #[test]
    fn total_files_excludes_archive_entries() {
        let store = make_store();
        store.upsert_file(&sample("/real.rs"));
        let mut inner = sample("/z.zip!/inner.rs");
        inner.archive_path = Some("/z.zip".into());
        store.upsert_file(&inner);
        assert_eq!(store.total_files(), 1);
        assert_eq!(store.total_archive_entries(), 1);
    }

    #[test]
    fn search_conditions_name_like() {
        let store = make_store();
        store.upsert_file(&sample("/a/foo.rs"));
        store.upsert_file(&sample("/a/bar.toml"));
        let results = store.search_conditions(
            &[SqlCondition::NameLike("%foo%".into())], 100,
        ).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "/a/foo.rs");
    }

    #[test]
    fn crawl_generation_remove_stale() {
        let store = make_store();
        store.upsert_file(&sample("/w/keep.rs"));
        store.upsert_file(&sample("/w/stale.rs"));
        let gen = store.begin_crawl_generation();
        store.batch_upsert_crawl(&[(sample("/w/keep.rs"), "/w".into())], gen);
        let removed = store.remove_stale_for_root("/w", gen);
        assert_eq!(removed, 1);
        assert!(store.get_by_path("/w/keep.rs").unwrap().is_some());
        assert!(store.get_by_path("/w/stale.rs").unwrap().is_none());
    }

    #[test]
    fn flush_and_reload() {
        let dir = TempDir::new().unwrap();
        {
            let store = MemoryStore::new(dir.path()).unwrap();
            store.upsert_file(&sample("/a/b.rs"));
            store.flush_to_disk(dir.path()).unwrap();
        }
        let store2 = MemoryStore::new(dir.path()).unwrap();
        assert!(store2.get_by_path("/a/b.rs").unwrap().is_some());
    }

    #[test]
    fn search_name_like() {
        let store = make_store();
        store.upsert_file(&sample("/a/foo.rs"));
        store.upsert_file(&sample("/a/bar.toml"));
        let results = store.search_name("%foo%", &MetaFilters::default(), 100).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "foo.rs");
        let all = store.search_name("%", &MetaFilters::default(), 100).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn search_extension_exact() {
        let store = make_store();
        store.upsert_file(&sample("/a/foo.rs"));
        let mut toml = sample("/a/bar.toml");
        toml.extension = Some("toml".into());
        store.upsert_file(&toml);
        let rs = store.search_extension("rs", &MetaFilters::default(), 100).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].name, "foo.rs");
    }

    #[test]
    fn paths_under_returns_subtree() {
        let store = make_store();
        store.upsert_file(&sample("/root/a/x.rs"));
        store.upsert_file(&sample("/root/a/b/y.rs"));
        store.upsert_file(&sample("/root/c/z.rs"));
        store.upsert_file(&sample("/other/w.rs"));
        let mut paths = store.paths_under("/root/a").unwrap();
        paths.sort();
        assert_eq!(paths, vec!["/root/a/b/y.rs", "/root/a/x.rs"]);
    }

    #[test]
    fn like_match_wildcards() {
        assert!(like_match("hello world", "%world"));
        assert!(like_match("hello world", "hello%"));
        assert!(like_match("hello world", "%ello%"));
        assert!(!like_match("hello world", "%xyz%"));
        assert!(like_match("Hello", "hello")); // case-insensitive
    }

    #[test]
    fn glob_match_wildcards() {
        assert!(glob_match("file.rs", "*.rs"));
        assert!(glob_match("file.rs", "file.*"));
        assert!(!glob_match("FILE.rs", "file.*")); // case-sensitive
        assert!(glob_match("file.rs", "????.rs"));
    }
}
