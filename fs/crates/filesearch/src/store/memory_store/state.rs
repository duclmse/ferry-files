use std::cell::Cell;
use std::collections::HashMap;
use std::hash::BuildHasher;

use hashbrown::{DefaultHashBuilder, HashTable};
use serde::{Deserialize, Serialize};

use super::snapshot::{Snapshot, SnapshotRef};
use super::{FileRecord, RecordView};

// ---------------------------------------------------------------------------
// Packed entries
//
// As in Everything, an entry stores only the part of its path below its
// parent entry (`/name`; `!/inner` for an archive's top-level members; the
// whole path when the parent is not indexed) and the parent's entry number.
// Full paths are rebuilt by walking parents; names are slices of the tail.
// The path index holds entry numbers only. A removed entry that live entries
// still extend stays as a dead "ghost" so their paths keep resolving, until
// compaction drops it. See docs/plans/everything-borrowing.md, phase 3.
// ---------------------------------------------------------------------------

const F_DIR: u8 = 1;
const F_HIDDEN: u8 = 2;
const F_SYMLINK: u8 = 4;
const F_CONTENT: u8 = 8;
/// Entry has a row in `extra` (fields that are not slices of the path).
const F_EXTRA: u8 = 16;
/// Removed; kept only while live entries name it as an ancestor.
const F_DEAD: u8 = 32;

pub(super) const NO_PARENT: u32 = u32::MAX;

/// Dead entries tolerated before compaction, as a fraction of all entries.
const COMPACT_MIN_DEAD: usize = 4096;
const COMPACT_DEAD_DIVISOR: usize = 4;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) struct Entry {
    tail_off: u64,
    tail_len: u32,
    /// Entry whose path this one extends; `NO_PARENT` means the tail is the
    /// whole path. Always lower than the entry's own number, so walks end.
    parent: u32,
    /// Byte offset of the name within the tail.
    name_off: u32,
    /// Length of the archive-path prefix of the full path; 0 = not an
    /// archive entry.
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
    pub(super) fn tail_range(&self) -> std::ops::Range<usize> {
        let start = self.tail_off as usize;
        start..start + self.tail_len as usize
    }

    pub(super) fn parent(&self) -> u32 {
        self.parent
    }

    pub(super) fn moved(mut self, tail_off: u64, parent: u32) -> Self {
        self.tail_off = tail_off;
        self.parent = parent;
        self
    }

    fn dead(&self) -> bool {
        self.flags & F_DEAD != 0
    }
}

/// The v3 snapshot entry: a whole path per entry in a shared arena. Only
/// read, to upgrade old snapshots.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) struct EntryV3 {
    pub(super) path_off: u64,
    pub(super) path_len: u32,
    pub(super) name_off: u32,
    pub(super) archive_len: u32,
    pub(super) ext: u32,
    pub(super) size: u64,
    pub(super) mtime: i64,
    pub(super) ctime: i64,
    pub(super) indexed_at: i64,
    pub(super) attrib: u32,
    pub(super) flags: u8,
    pub(super) generation: u32,
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
    tails: String,
    entries: Vec<Entry>,
    /// Entries flagged `F_DEAD`.
    dead: usize,
    /// Dead entries that survived the last compaction (ancestors of live
    /// ones); compaction cannot drop them, so they do not count towards the
    /// next one.
    pinned: usize,
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

// ---------------------------------------------------------------------------
// Path reconstruction
//
// Free functions over the two arrays, so the hash table's rehash callback can
// borrow them while the table itself is borrowed mutably.
// ---------------------------------------------------------------------------

thread_local! {
    static PATH_BUF: Cell<String> = const { Cell::new(String::new()) };
}

/// Runs `f` with an empty per-thread buffer, so scans that need full paths
/// do not allocate one per entry.
fn with_path_buf<T>(f: impl FnOnce(&mut String) -> T) -> T {
    PATH_BUF.with(|c| {
        let mut buf = c.take();
        buf.clear();
        let out = f(&mut buf);
        c.set(buf);
        out
    })
}

fn tail_in<'a>(tails: &'a str, e: &Entry) -> &'a str {
    &tails[e.tail_range()]
}

/// Calls `f` with the entry numbers from `i` up to its root, leaf first.
fn with_chain<T>(entries: &[Entry], i: u32, f: impl FnOnce(&[u32]) -> T) -> T {
    let mut arr = [0u32; 64];
    let mut n = 0;
    let mut j = i;
    loop {
        if n == arr.len() {
            let mut v = arr.to_vec();
            loop {
                v.push(j);
                match entries[j as usize].parent {
                    NO_PARENT => return f(&v),
                    p => j = p,
                }
            }
        }
        arr[n] = j;
        n += 1;
        match entries[j as usize].parent {
            NO_PARENT => return f(&arr[..n]),
            p => j = p,
        }
    }
}

fn write_path(entries: &[Entry], tails: &str, i: u32, buf: &mut String) {
    with_chain(entries, i, |chain| {
        for &j in chain.iter().rev() {
            buf.push_str(tail_in(tails, &entries[j as usize]));
        }
    });
}

/// Compares without rebuilding: strips each tail off the end of `path`.
fn path_eq(entries: &[Entry], tails: &str, i: u32, path: &str) -> bool {
    let mut rest = path;
    let mut j = i;
    loop {
        let e = &entries[j as usize];
        let Some(head) = rest.strip_suffix(tail_in(tails, e)) else {
            return false;
        };
        rest = head;
        match e.parent {
            NO_PARENT => return rest.is_empty(),
            p => j = p,
        }
    }
}

fn hash_entry(entries: &[Entry], tails: &str, hasher: &DefaultHashBuilder, i: u32) -> u64 {
    with_path_buf(|buf| {
        write_path(entries, tails, i, buf);
        hasher.hash_one(buf.as_str())
    })
}

/// `crate::store::is_under(path, dir)`, plus whether the path is a direct
/// child, streamed root-first over the tails so most entries are rejected
/// within their first segment.
fn under(entries: &[Entry], tails: &str, i: u32, dir: &str) -> Option<bool> {
    let d = dir.as_bytes();
    // The prefix to match is `dir` followed by '/'.
    let want = d.len() + 1;
    with_chain(entries, i, |chain| {
        let mut k = 0;
        let mut direct = true;
        for &j in chain.iter().rev() {
            let t = tail_in(tails, &entries[j as usize]).as_bytes();
            let mut s = 0;
            if k < want {
                let n = (want - k).min(t.len());
                for &b in &t[..n] {
                    let expect = d.get(k).copied().unwrap_or(b'/');
                    if b != expect {
                        return None;
                    }
                    k += 1;
                }
                s = n;
            }
            if k == want && t[s..].contains(&b'/') {
                direct = false;
            }
        }
        (k == want).then_some(direct)
    })
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
            tails: String::new(),
            entries: Vec::new(),
            dead: 0,
            pinned: 0,
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

    /// Live entries.
    pub(super) fn len(&self) -> usize {
        self.entries.len() - self.dead
    }

    fn hash(&self, path: &str) -> u64 {
        self.hasher.hash_one(path)
    }

    /// Includes dead entries.
    fn find_any(&self, path: &str) -> Option<u32> {
        let h = self.hash(path);
        self.index
            .find(h, |&i| path_eq(&self.entries, &self.tails, i, path))
            .copied()
    }

    pub(super) fn find(&self, path: &str) -> Option<u32> {
        self.find_any(path)
            .filter(|&i| !self.entries[i as usize].dead())
    }

    pub(super) fn view(&self, i: u32) -> EntryRef<'_> {
        EntryRef {
            st: self,
            e: &self.entries[i as usize],
            i,
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = EntryRef<'_>> + '_ {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.dead())
            .map(|(i, e)| EntryRef {
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

    /// The entry a new `path` should extend, and where its tail starts. The
    /// parent directory if it is indexed; for `outer.zip!/top`, the archive.
    fn parent_of(&self, path: &str) -> (u32, usize) {
        let cut = match path.rfind(['/', '\\']) {
            None | Some(0) => return (NO_PARENT, 0),
            Some(c) => c,
        };
        let head = &path[..cut];
        if let Some(p) = self.find_any(head) {
            return (p, cut);
        }
        if let Some(archive) = head.strip_suffix('!') {
            if let Some(p) = self.find_any(archive) {
                return (p, archive.len());
            }
        }
        (NO_PARENT, 0)
    }

    /// Packs `r` around a tail that is `r.path[cut..]`, already stored at
    /// `tail_off`; anything that is not a slice of the path goes to the
    /// returned `Extra`.
    fn pack(&mut self, r: &FileRecord, tail_off: u64, cut: usize, parent: u32) -> (Entry, Extra) {
        let path = r.path.as_str();
        let tail_len = path.len() - cut;
        let mut extra = Extra {
            id: r.id,
            tantivy_id: r.tantivy_id.clone(),
            ..Extra::default()
        };

        let name_off = match path.strip_suffix(r.name.as_str()) {
            _ if r.name.is_empty() => tail_len as u32,
            Some(head) if head.len() >= cut => (head.len() - cut) as u32,
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
            tail_off,
            tail_len: tail_len as u32,
            parent,
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
        if let Some(i) = self.find_any(&r.path) {
            let old = self.entries[i as usize];
            let cut = r.path.len() - old.tail_len as usize;
            let (mut entry, extra) = self.pack(r, old.tail_off, cut, old.parent);
            if old.dead() {
                self.dead -= 1;
                self.pinned = self.pinned.min(self.dead);
            } else {
                entry.generation = old.generation;
            }
            self.entries[i as usize] = entry;
            self.set_extra(i, extra);
            return i;
        }
        let (parent, cut) = self.parent_of(&r.path);
        let tail_off = self.tails.len() as u64;
        self.tails.push_str(&r.path[cut..]);
        let (entry, extra) = self.pack(r, tail_off, cut, parent);
        let i = self.entries.len() as u32;
        self.entries.push(entry);
        self.set_extra(i, extra);
        let h = self.hash(&r.path);
        let (entries, tails, hasher) = (&self.entries, &self.tails, &self.hasher);
        self.index
            .insert_unique(h, i, |&j| hash_entry(entries, tails, hasher, j));
        i
    }

    pub(super) fn set_generation(&mut self, i: u32, generation: u32) {
        self.entries[i as usize].generation = generation;
    }

    pub(super) fn remove(&mut self, path: &str) -> bool {
        let Some(i) = self.find(path) else {
            return false;
        };
        self.remove_at(i);
        self.maybe_compact();
        true
    }

    /// Marks entry `i` dead. Entry numbers stay valid until `maybe_compact`.
    pub(super) fn remove_at(&mut self, i: u32) {
        let e = &mut self.entries[i as usize];
        if e.dead() {
            return;
        }
        e.flags = F_DEAD;
        e.generation = 0;
        self.extra.remove(&i);
        self.dead += 1;
    }

    pub(super) fn maybe_compact(&mut self) {
        let unpinned = self.dead.saturating_sub(self.pinned);
        if unpinned > COMPACT_MIN_DEAD && unpinned * COMPACT_DEAD_DIVISOR > self.entries.len() {
            self.compact();
        }
    }

    /// For each entry, its number after dropping dead entries that no live
    /// entry extends, or `NO_PARENT` if it is dropped. Parents stay below
    /// children because the order is kept.
    fn renumbering(&self) -> Vec<u32> {
        let n = self.entries.len();
        let mut keep = vec![false; n];
        for i in (0..n).rev() {
            let e = &self.entries[i];
            if !e.dead() {
                keep[i] = true;
            }
            if keep[i] && e.parent != NO_PARENT {
                keep[e.parent as usize] = true;
            }
        }
        let mut next = 0u32;
        keep.iter()
            .map(|&k| {
                if k {
                    next += 1;
                    next - 1
                } else {
                    NO_PARENT
                }
            })
            .collect()
    }

    fn remapped_parent(map: &[u32], parent: u32) -> u32 {
        match parent {
            NO_PARENT => NO_PARENT,
            p => map[p as usize],
        }
    }

    /// Drops dead entries that nothing extends, and their tails.
    pub(super) fn compact(&mut self) {
        let map = self.renumbering();
        let kept = map.iter().filter(|&&m| m != NO_PARENT).count();
        let mut tails = String::with_capacity(
            self.entries
                .iter()
                .zip(&map)
                .filter(|(_, &m)| m != NO_PARENT)
                .map(|(e, _)| e.tail_len as usize)
                .sum(),
        );
        let mut entries = Vec::with_capacity(kept);
        for (e, &m) in self.entries.iter().zip(&map) {
            if m == NO_PARENT {
                continue;
            }
            let off = tails.len() as u64;
            tails.push_str(tail_in(&self.tails, e));
            entries.push(e.moved(off, Self::remapped_parent(&map, e.parent)));
        }
        self.index.retain(|i| match map[*i as usize] {
            NO_PARENT => false,
            m => {
                *i = m;
                true
            }
        });
        self.extra = std::mem::take(&mut self.extra)
            .into_iter()
            .filter_map(|(k, x)| match map[k as usize] {
                NO_PARENT => None,
                m => Some((m, x)),
            })
            .collect();
        self.tails = tails;
        self.entries = entries;
        self.dead = self.entries.iter().filter(|e| e.dead()).count();
        self.pinned = self.dead;
    }

    pub(super) fn to_snapshot(&self) -> SnapshotRef<'_> {
        let map = (self.dead > 0).then(|| self.renumbering());
        let renumber = |k: u32| match &map {
            None => Some(k),
            Some(m) => Some(m[k as usize]).filter(|&m| m != NO_PARENT),
        };
        let mut extra: Vec<(u32, &Extra)> = self
            .extra
            .iter()
            .filter_map(|(&k, v)| Some((renumber(k)?, v)))
            .collect();
        extra.sort_unstable_by_key(|(k, _)| *k);
        let mut meta: Vec<(&String, &String)> = self.meta.iter().collect();
        meta.sort_unstable();
        SnapshotRef {
            arena: &self.tails,
            entries: &self.entries,
            renumber: map,
            exts: &self.exts,
            extra,
            meta,
            crawl_gen: self.crawl_gen,
        }
    }

    pub(super) fn from_snapshot(s: Snapshot<Entry>, usn_checkpoint: u64) -> Result<Self, String> {
        for (i, e) in s.entries.iter().enumerate() {
            let r = e.tail_range();
            let ok = r.end <= s.arena.len()
                && s.arena.is_char_boundary(r.start)
                && s.arena.is_char_boundary(r.end)
                && e.name_off <= e.tail_len
                && s.arena.is_char_boundary(r.start + e.name_off as usize)
                && (e.parent == NO_PARENT || (e.parent as usize) < i)
                && e.ext as usize <= s.exts.len();
            if !ok {
                return Err("entry out of bounds".into());
            }
        }
        if s.extra.iter().any(|(k, _)| *k as usize >= s.entries.len()) {
            return Err("side-table key out of bounds".into());
        }
        let mut st = Self::empty();
        st.tails = s.arena;
        st.entries = s.entries;
        st.dead = st.entries.iter().filter(|e| e.dead()).count();
        st.pinned = st.dead;
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
            let (entries, tails, hasher) = (&st.entries, &st.tails, &st.hasher);
            let duplicate = with_path_buf(|buf| {
                write_path(entries, tails, i, buf);
                let h = hasher.hash_one(buf.as_str());
                let dup = st
                    .index
                    .find(h, |&j| path_eq(entries, tails, j, buf))
                    .is_some();
                (dup, h)
            });
            if duplicate.0 {
                return Err("duplicate path".into());
            }
            st.index
                .insert_unique(duplicate.1, i, |&j| hash_entry(entries, tails, hasher, j));
        }
        Ok(st)
    }

    /// Upgrades a v3 snapshot (one whole path per entry). Shorter paths go
    /// first so parents exist before the entries that extend them.
    pub(super) fn from_v3(s: Snapshot<EntryV3>, usn_checkpoint: u64) -> Result<Self, String> {
        for e in &s.entries {
            let start = e.path_off as usize;
            let end = start + e.path_len as usize;
            let ok = end <= s.arena.len()
                && s.arena.is_char_boundary(start)
                && s.arena.is_char_boundary(end)
                && e.name_off <= e.path_len
                && s.arena.is_char_boundary(start + e.name_off as usize)
                && e.archive_len <= e.path_len
                && s.arena.is_char_boundary(start + e.archive_len as usize)
                && e.ext as usize <= s.exts.len();
            if !ok {
                return Err("entry out of bounds".into());
            }
        }
        let mut extra: HashMap<u32, Extra> = s.extra.into_iter().collect();
        let mut order: Vec<u32> = (0..s.entries.len() as u32).collect();
        order.sort_by_key(|&i| s.entries[i as usize].path_len);
        let mut st = Self::empty();
        st.entries.reserve(s.entries.len());
        for i in order {
            let e = &s.entries[i as usize];
            let x = if e.flags & F_EXTRA != 0 { extra.remove(&i) } else { None };
            let x = x.unwrap_or_default();
            let start = e.path_off as usize;
            let path = &s.arena[start..start + e.path_len as usize];
            let r = FileRecord {
                id: x.id,
                path: path.to_owned(),
                name: x
                    .name
                    .unwrap_or_else(|| path[e.name_off as usize..].to_owned()),
                extension: match e.ext {
                    0 => None,
                    id => Some(s.exts[id as usize - 1].clone()),
                },
                size_bytes: e.size,
                mtime: e.mtime,
                ctime: e.ctime,
                is_dir: e.flags & F_DIR != 0,
                is_hidden: e.flags & F_HIDDEN != 0,
                is_symlink: e.flags & F_SYMLINK != 0,
                attrib_bitmask: e.attrib,
                content_indexed: e.flags & F_CONTENT != 0,
                tantivy_id: x.tantivy_id,
                indexed_at: e.indexed_at,
                archive_path: match e.archive_len {
                    0 => x.archive_path,
                    n => Some(path[..n as usize].to_owned()),
                },
            };
            if st.find(&r.path).is_some() {
                return Err("duplicate path".into());
            }
            let j = st.upsert(&r);
            st.set_generation(j, e.generation);
        }
        st.meta = s.meta.into_iter().collect();
        st.crawl_gen = s.crawl_gen;
        st.usn_checkpoint = usn_checkpoint;
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

    pub(super) fn index(&self) -> u32 {
        self.i
    }

    fn tail(&self) -> &'a str {
        tail_in(&self.st.tails, self.e)
    }

    pub(super) fn write_path(&self, buf: &mut String) {
        write_path(&self.st.entries, &self.st.tails, self.i, buf);
    }

    pub(super) fn path_string(&self) -> String {
        let mut s = String::new();
        self.write_path(&mut s);
        s
    }

    pub(super) fn path_is(&self, path: &str) -> bool {
        path_eq(&self.st.entries, &self.st.tails, self.i, path)
    }

    /// `None` if the path is not under `dir`; otherwise whether it is a
    /// direct child.
    pub(super) fn under(&self, dir: &str) -> Option<bool> {
        under(&self.st.entries, &self.st.tails, self.i, dir)
    }

    pub(super) fn name_str(&self) -> &'a str {
        if let Some(n) = self.extra().and_then(|x| x.name.as_deref()) {
            return n;
        }
        self.tail().get(self.e.name_off as usize..).unwrap_or_default()
    }

    pub(super) fn ext_str(&self) -> Option<&'a str> {
        match self.e.ext {
            0 => None,
            id => Some(self.st.exts[id as usize - 1].as_str()),
        }
    }

    pub(super) fn is_archive(&self) -> bool {
        self.e.archive_len > 0 || self.extra().is_some_and(|x| x.archive_path.is_some())
    }

    pub(super) fn generation(&self) -> u32 {
        self.e.generation
    }

    pub(super) fn to_record(self) -> FileRecord {
        let extra = self.extra();
        let path = self.path_string();
        let archive_path = match self.e.archive_len {
            0 => extra.and_then(|x| x.archive_path.clone()),
            n => path.get(..n as usize).map(str::to_owned),
        };
        FileRecord {
            id: extra.and_then(|x| x.id),
            name: self.name_str().to_owned(),
            path,
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
            archive_path,
        }
    }
}

impl RecordView for EntryRef<'_> {
    fn with_path<T>(&self, f: impl FnOnce(&str) -> T) -> T {
        with_path_buf(|buf| {
            self.write_path(buf);
            f(buf)
        })
    }
    fn path_is_under(&self, dir: &str) -> bool {
        self.under(dir).is_some()
    }
    fn path_depth(&self) -> usize {
        let mut n = 0;
        let mut j = self.i;
        loop {
            let e = &self.st.entries[j as usize];
            n += tail_in(&self.st.tails, e).bytes().filter(|&b| b == b'/').count();
            match e.parent {
                NO_PARENT => return n,
                p => j = p,
            }
        }
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
    fn is_archive_entry(&self) -> bool {
        self.is_archive()
    }
    fn with_archive_path<T>(&self, f: impl FnOnce(Option<&str>) -> T) -> T {
        match self.e.archive_len {
            0 => f(self.extra().and_then(|x| x.archive_path.as_deref())),
            n => self.with_path(|p| f(p.get(..n as usize))),
        }
    }
}
