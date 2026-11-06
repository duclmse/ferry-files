use std::collections::HashMap;
use std::hash::BuildHasher;

use hashbrown::{DefaultHashBuilder, HashTable};
use serde::{Deserialize, Serialize};

use super::snapshot::{Snapshot, SnapshotRef};
use super::{FileRecord, RecordView};

// ---------------------------------------------------------------------------
// Packed entries
//
// Each path is stored once, in `arena`. Everything else that the crawler
// derives from the path is an offset into it: `name` is the path's tail and
// `archive_path` its head (`outer.zip!/inner` → `outer.zip`). The path index
// holds entry numbers only and hashes keys out of the arena, so the path is
// never copied a second time. See docs/plans/everything-borrowing.md, phase 3.
// ---------------------------------------------------------------------------

const F_DIR: u8 = 1;
const F_HIDDEN: u8 = 2;
const F_SYMLINK: u8 = 4;
const F_CONTENT: u8 = 8;
/// Entry has a row in `extra` (fields that are not slices of the path).
const F_EXTRA: u8 = 16;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) struct Entry {
    path_off: u64,
    path_len: u32,
    /// Byte offset of the name within the path.
    name_off: u32,
    /// Length of the archive-path prefix; 0 = not an archive entry.
    archive_len: u32,
    /// 1-based index into `exts`; 0 = no extension.
    ext: u32,
    size: u64,
    mtime: i64,
    ctime: i64,
    indexed_at: i64,
    attrib: u32,
    flags: u8,
    /// Crawl generation that last saw this entry; 0 = never tagged by a crawl.
    pub(super) generation: u32,
}

impl Entry {
    pub(super) fn path_range(&self) -> std::ops::Range<usize> {
        let start = self.path_off as usize;
        start..start + self.path_len as usize
    }

    pub(super) fn with_path_off(mut self, off: u64) -> Self {
        self.path_off = off;
        self
    }
}

/// Fields that are rare, or that do not follow the crawler's path layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Extra {
    id: Option<i64>,
    tantivy_id: Option<String>,
    name: Option<String>,
    archive_path: Option<String>,
}

impl Extra {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.tantivy_id.is_none()
            && self.name.is_none()
            && self.archive_path.is_none()
    }
}

pub(super) struct IndexState {
    arena: String,
    /// Bytes in `arena` no longer referenced by any entry.
    garbage: usize,
    entries: Vec<Entry>,
    index: HashTable<u32>,
    hasher: DefaultHashBuilder,
    exts: Vec<String>,
    ext_ids: HashMap<String, u32>,
    extra: HashMap<u32, Extra>,
    pub(super) meta: HashMap<String, String>,
    pub(super) crawl_gen: i64,
    /// USN/inotify journal position recorded at last clean shutdown.
    /// Used for dirty-shutdown recovery: replay events from this position.
    pub(super) usn_checkpoint: u64,
}

fn path_in<'a>(arena: &'a str, e: &Entry) -> &'a str {
    let start = e.path_off as usize;
    &arena[start..start + e.path_len as usize]
}

/// Borrowed view of one entry; implements [`RecordView`] for matching.
#[derive(Clone, Copy)]
pub(super) struct EntryRef<'a> {
    st: &'a IndexState,
    e: &'a Entry,
    i: u32,
}

impl IndexState {
    pub(super) fn empty() -> Self {
        Self {
            arena: String::new(),
            garbage: 0,
            entries: Vec::new(),
            index: HashTable::new(),
            hasher: DefaultHashBuilder::default(),
            exts: Vec::new(),
            ext_ids: HashMap::new(),
            extra: HashMap::new(),
            meta: HashMap::new(),
            crawl_gen: 0,
            usn_checkpoint: 0,
        }
    }

    pub(super) fn from_records(records: Vec<FileRecord>) -> Self {
        let mut st = Self::empty();
        st.entries.reserve(records.len());
        for r in records {
            st.upsert(&r);
        }
        st
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    fn path_of(&self, e: &Entry) -> &str {
        path_in(&self.arena, e)
    }

    fn hash(&self, path: &str) -> u64 {
        self.hasher.hash_one(path)
    }

    pub(super) fn find(&self, path: &str) -> Option<u32> {
        let h = self.hash(path);
        self.index
            .find(h, |&i| self.path_of(&self.entries[i as usize]) == path)
            .copied()
    }

    pub(super) fn view(&self, i: u32) -> EntryRef<'_> {
        EntryRef {
            st: self,
            e: &self.entries[i as usize],
            i,
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = EntryRef<'_>> + '_ {
        self.entries.iter().enumerate().map(|(i, e)| EntryRef {
            st: self,
            e,
            i: i as u32,
        })
    }

    fn intern_ext(&mut self, ext: Option<&str>) -> u32 {
        let Some(ext) = ext else { return 0 };
        if let Some(&id) = self.ext_ids.get(ext) {
            return id;
        }
        self.exts.push(ext.to_owned());
        let id = self.exts.len() as u32;
        self.ext_ids.insert(ext.to_owned(), id);
        id
    }

    /// Packs `r` around an already-stored path at `path_off`; anything that
    /// is not a slice of the path goes to the returned `Extra`.
    fn pack(&mut self, r: &FileRecord, path_off: u64) -> (Entry, Extra) {
        let path = r.path.as_str();
        let mut extra = Extra {
            id: r.id,
            tantivy_id: r.tantivy_id.clone(),
            ..Extra::default()
        };

        let name_off = match path.strip_suffix(r.name.as_str()) {
            Some(head) if !r.name.is_empty() => head.len() as u32,
            _ if r.name.is_empty() => path.len() as u32,
            _ => {
                extra.name = Some(r.name.clone());
                0
            }
        };
        let archive_len = match r.archive_path.as_deref() {
            None => 0,
            Some(ap)
                if !ap.is_empty()
                    && path.len() > ap.len() + 1
                    && path.starts_with(ap)
                    && path[ap.len()..].starts_with("!/") =>
            {
                ap.len() as u32
            }
            Some(ap) => {
                extra.archive_path = Some(ap.to_owned());
                0
            }
        };

        let mut flags = 0;
        if r.is_dir {
            flags |= F_DIR;
        }
        if r.is_hidden {
            flags |= F_HIDDEN;
        }
        if r.is_symlink {
            flags |= F_SYMLINK;
        }
        if r.content_indexed {
            flags |= F_CONTENT;
        }
        if !extra.is_empty() {
            flags |= F_EXTRA;
        }

        let entry = Entry {
            path_off,
            path_len: path.len() as u32,
            name_off,
            archive_len,
            ext: self.intern_ext(r.extension.as_deref()),
            size: r.size_bytes,
            mtime: r.mtime,
            ctime: r.ctime,
            indexed_at: r.indexed_at,
            attrib: r.attrib_bitmask,
            flags,
            generation: 0,
        };
        (entry, extra)
    }

    fn set_extra(&mut self, i: u32, extra: Extra) {
        if extra.is_empty() {
            self.extra.remove(&i);
        } else {
            self.extra.insert(i, extra);
        }
    }

    /// Inserts or replaces `r`, keeping the entry's crawl generation on
    /// replace (watcher updates do not un-tag a crawled entry). Returns the
    /// entry number.
    pub(super) fn upsert(&mut self, r: &FileRecord) -> u32 {
        if let Some(i) = self.find(&r.path) {
            let old = self.entries[i as usize];
            let (mut entry, extra) = self.pack(r, old.path_off);
            entry.generation = old.generation;
            self.entries[i as usize] = entry;
            self.set_extra(i, extra);
            return i;
        }
        let path_off = self.arena.len() as u64;
        self.arena.push_str(&r.path);
        let (entry, extra) = self.pack(r, path_off);
        let i = self.entries.len() as u32;
        self.entries.push(entry);
        self.set_extra(i, extra);
        let h = self.hash(&r.path);
        let (entries, arena, hasher) = (&self.entries, &self.arena, &self.hasher);
        self.index.insert_unique(h, i, |&j| {
            hasher.hash_one(path_in(arena, &entries[j as usize]))
        });
        i
    }

    pub(super) fn set_generation(&mut self, i: u32, generation: u32) {
        self.entries[i as usize].generation = generation;
    }

    /// Swap-remove: the last entry takes the removed one's number.
    pub(super) fn remove(&mut self, path: &str) -> bool {
        let h = self.hash(path);
        let (entries, arena) = (&self.entries, &self.arena);
        let Ok(slot) = self
            .index
            .find_entry(h, |&i| path_in(arena, &entries[i as usize]) == path)
        else {
            return false;
        };
        let (i, _) = slot.remove();
        let last = (self.entries.len() - 1) as u32;
        self.garbage += self.entries[i as usize].path_len as usize;
        self.extra.remove(&i);
        if i != last {
            let moved_hash = self.hash(self.path_of(&self.entries[last as usize]));
            if let Some(slot) = self.index.find_mut(moved_hash, |&j| j == last) {
                *slot = i;
            }
            if let Some(x) = self.extra.remove(&last) {
                self.extra.insert(i, x);
            }
        }
        self.entries.swap_remove(i as usize);
        if self.garbage > 4096 && self.garbage > self.arena.len() / 2 {
            self.compact();
        }
        true
    }

    /// Rewrites the arena without the bytes of removed paths. Entry numbers
    /// do not change, so the index stays valid.
    pub(super) fn compact(&mut self) {
        if self.garbage == 0 {
            return;
        }
        let mut arena = String::with_capacity(self.arena.len() - self.garbage);
        for e in &mut self.entries {
            let s = e.path_off as usize;
            e.path_off = arena.len() as u64;
            arena.push_str(&self.arena[s..s + e.path_len as usize]);
        }
        self.arena = arena;
        self.garbage = 0;
    }

    pub(super) fn to_snapshot(&self) -> SnapshotRef<'_> {
        let mut extra: Vec<(u32, &Extra)> = self.extra.iter().map(|(&k, v)| (k, v)).collect();
        extra.sort_unstable_by_key(|(k, _)| *k);
        let mut meta: Vec<(&String, &String)> = self.meta.iter().collect();
        meta.sort_unstable();
        SnapshotRef {
            arena: &self.arena,
            entries: &self.entries,
            exts: &self.exts,
            extra,
            meta,
            crawl_gen: self.crawl_gen,
        }
    }

    pub(super) fn from_snapshot(s: Snapshot, usn_checkpoint: u64) -> Result<Self, String> {
        let mut st = Self::empty();
        for e in &s.entries {
            let end = e.path_off as usize + e.path_len as usize;
            let ok = end <= s.arena.len()
                && s.arena.is_char_boundary(e.path_off as usize)
                && s.arena.is_char_boundary(end)
                && e.name_off <= e.path_len
                && s.arena
                    .is_char_boundary(e.path_off as usize + e.name_off as usize)
                && e.archive_len <= e.path_len
                && s.arena
                    .is_char_boundary(e.path_off as usize + e.archive_len as usize)
                && e.ext as usize <= s.exts.len();
            if !ok {
                return Err("entry out of bounds".into());
            }
        }
        if s.extra.iter().any(|(k, _)| *k as usize >= s.entries.len()) {
            return Err("side-table key out of bounds".into());
        }
        st.arena = s.arena;
        st.entries = s.entries;
        st.ext_ids = s
            .exts
            .iter()
            .enumerate()
            .map(|(i, x)| (x.clone(), i as u32 + 1))
            .collect();
        st.exts = s.exts;
        st.extra = s.extra.into_iter().collect();
        st.meta = s.meta.into_iter().collect();
        st.crawl_gen = s.crawl_gen;
        st.usn_checkpoint = usn_checkpoint;
        st.index = HashTable::with_capacity(st.entries.len());
        for i in 0..st.entries.len() as u32 {
            let h = st.hash(st.path_of(&st.entries[i as usize]));
            if st
                .index
                .find(h, |&j| {
                    st.path_of(&st.entries[j as usize]) == st.path_of(&st.entries[i as usize])
                })
                .is_some()
            {
                return Err("duplicate path".into());
            }
            let (entries, arena, hasher) = (&st.entries, &st.arena, &st.hasher);
            st.index.insert_unique(h, i, |&j| {
                hasher.hash_one(path_in(arena, &entries[j as usize]))
            });
        }
        Ok(st)
    }
}

// ---------------------------------------------------------------------------
// Borrowed view
// ---------------------------------------------------------------------------

impl<'a> EntryRef<'a> {
    fn extra(&self) -> Option<&'a Extra> {
        if self.e.flags & F_EXTRA == 0 {
            return None;
        }
        self.st.extra.get(&self.i)
    }

    pub(super) fn path_str(&self) -> &'a str {
        self.st.path_of(self.e)
    }

    pub(super) fn name_str(&self) -> &'a str {
        if let Some(n) = self.extra().and_then(|x| x.name.as_deref()) {
            return n;
        }
        &self.path_str()[self.e.name_off as usize..]
    }

    pub(super) fn ext_str(&self) -> Option<&'a str> {
        match self.e.ext {
            0 => None,
            id => Some(self.st.exts[id as usize - 1].as_str()),
        }
    }

    pub(super) fn archive_str(&self) -> Option<&'a str> {
        if self.e.archive_len > 0 {
            return Some(&self.path_str()[..self.e.archive_len as usize]);
        }
        self.extra().and_then(|x| x.archive_path.as_deref())
    }

    pub(super) fn generation(&self) -> u32 {
        self.e.generation
    }

    pub(super) fn to_record(self) -> FileRecord {
        let extra = self.extra();
        FileRecord {
            id: extra.and_then(|x| x.id),
            path: self.path_str().to_owned(),
            name: self.name_str().to_owned(),
            extension: self.ext_str().map(str::to_owned),
            size_bytes: self.e.size,
            mtime: self.e.mtime,
            ctime: self.e.ctime,
            is_dir: self.e.flags & F_DIR != 0,
            is_hidden: self.e.flags & F_HIDDEN != 0,
            is_symlink: self.e.flags & F_SYMLINK != 0,
            attrib_bitmask: self.e.attrib,
            content_indexed: self.e.flags & F_CONTENT != 0,
            tantivy_id: extra.and_then(|x| x.tantivy_id.clone()),
            indexed_at: self.e.indexed_at,
            archive_path: self.archive_str().map(str::to_owned),
        }
    }
}

impl RecordView for EntryRef<'_> {
    fn path(&self) -> &str {
        self.path_str()
    }
    fn name(&self) -> &str {
        self.name_str()
    }
    fn extension(&self) -> Option<&str> {
        self.ext_str()
    }
    fn size_bytes(&self) -> u64 {
        self.e.size
    }
    fn mtime(&self) -> i64 {
        self.e.mtime
    }
    fn ctime(&self) -> i64 {
        self.e.ctime
    }
    fn is_dir(&self) -> bool {
        self.e.flags & F_DIR != 0
    }
    fn attrib_bitmask(&self) -> u32 {
        self.e.attrib
    }
    fn archive_path(&self) -> Option<&str> {
        self.archive_str()
    }
}
