//! Streaming encode and decode of the `index.bin` v3 and v4 bodies.
//!
//! The body is the postcard encoding of [`Snapshot`]. Decoding it with
//! `postcard::from_bytes` needed the whole file in memory next to the decoded
//! arrays, and encoding built the file twice (1.5-1.6 GB peak RSS for a
//! 420 MB snapshot). These functions read and write the same bytes field by
//! field, so neither direction holds more than the index plus a small buffer.
//! The writer also drops dead entries and their tails on the way out, so
//! saving never needs to compact (copy) the index first.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::state::{Entry, Extra, NO_PARENT};

#[derive(Serialize)]
pub(super) struct SnapshotRef<'a> {
    pub(super) arena: &'a str,
    pub(super) entries: &'a [Entry],
    /// New entry numbers when dead entries are dropped on the way out
    /// (`NO_PARENT` = dropped); `None` writes every entry as is.
    #[serde(skip)]
    pub(super) renumber: Option<Vec<u32>>,
    pub(super) exts: &'a [String],
    pub(super) extra: Vec<(u32, &'a Extra)>,
    pub(super) meta: Vec<(&'a String, &'a String)>,
    pub(super) crawl_gen: i64,
}

pub(super) struct Snapshot<E> {
    pub(super) arena: String,
    pub(super) entries: Vec<E>,
    pub(super) exts: Vec<String>,
    pub(super) extra: Vec<(u32, Extra)>,
    pub(super) meta: Vec<(String, String)>,
    pub(super) crawl_gen: i64,
}

/// The fields after `entries`. Postcard encodes a struct as its fields in
/// order, so these are the same bytes as the tail of `SnapshotRef`.
#[derive(Serialize)]
struct TailRef<'a> {
    exts: &'a [String],
    extra: &'a [(u32, &'a Extra)],
    meta: &'a [(&'a String, &'a String)],
    crawl_gen: i64,
}

#[derive(Deserialize)]
struct Tail {
    exts: Vec<String>,
    extra: Vec<(u32, Extra)>,
    meta: Vec<(String, String)>,
    crawl_gen: i64,
}

/// Longest postcard encoding of one `Entry`: five 64-bit varints (10 bytes),
/// seven 32-bit varints (5 bytes) and one byte, rounded up.
const MAX_ENTRY: usize = 96;
/// Shortest encoding of one v3 `EntryV3` (one byte per field; `Entry` has
/// one field more); bounds the entry count a corrupt header can make us
/// reserve.
const MIN_ENTRY: usize = 12;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn write_varint(w: &mut impl Write, mut v: u64) -> io::Result<()> {
    let mut buf = [0u8; 10];
    let mut n = 0;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf[n] = byte;
            n += 1;
            break;
        }
        buf[n] = byte | 0x80;
        n += 1;
    }
    w.write_all(&buf[..n])
}

/// Writes the body; returns its length in bytes. The arena is written as
/// the kept entries' tails in entry order, and entry offsets and parent
/// numbers are rewritten to match.
pub(super) fn write(w: impl Write, s: &SnapshotRef<'_>) -> io::Result<u64> {
    let mut w = Counting { inner: w, n: 0 };
    let arena = s.arena.as_bytes();
    let renumber = |i: u32| match &s.renumber {
        None => i,
        Some(m) => m[i as usize],
    };
    let kept = || {
        s.entries
            .iter()
            .enumerate()
            .filter(|(i, _)| renumber(*i as u32) != NO_PARENT)
            .map(|(_, e)| e)
    };
    let live: u64 = kept().map(|e| e.tail_range().len() as u64).sum();
    write_varint(&mut w, live)?;
    for e in kept() {
        w.write_all(
            arena
                .get(e.tail_range())
                .ok_or_else(|| invalid("entry outside arena"))?,
        )?;
    }
    write_varint(&mut w, kept().count() as u64)?;
    let mut buf = [0u8; MAX_ENTRY];
    let mut off = 0u64;
    for e in kept() {
        let parent = match e.parent() {
            NO_PARENT => NO_PARENT,
            p => renumber(p),
        };
        let moved = e.moved(off, parent);
        off += e.tail_range().len() as u64;
        let used = postcard::to_slice(&moved, &mut buf).map_err(|e| invalid(e.to_string()))?;
        w.write_all(used)?;
    }
    let tail = TailRef {
        exts: s.exts,
        extra: &s.extra,
        meta: &s.meta,
        crawl_gen: s.crawl_gen,
    };
    let tail = postcard::to_allocvec(&tail).map_err(|e| invalid(e.to_string()))?;
    w.write_all(&tail)?;
    w.flush()?;
    Ok(w.n)
}

struct Counting<W> {
    inner: W,
    n: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reads a body of exactly `len` bytes.
pub(super) fn read<E: DeserializeOwned>(r: impl Read, len: u64) -> io::Result<Snapshot<E>> {
    let mut r = Chunks::new(r.take(len));
    let mut left = len;

    let arena_len = r.varint()?;
    if arena_len > left {
        return Err(invalid("arena longer than file"));
    }
    let mut arena = Vec::new();
    r.exact(arena_len as usize, &mut arena)?;
    let arena = String::from_utf8(arena).map_err(|_| invalid("arena is not UTF-8"))?;
    left -= arena_len;

    let count = r.varint()?;
    if count > left / MIN_ENTRY as u64 {
        return Err(invalid("entry count larger than file"));
    }
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        r.fill(MAX_ENTRY)?;
        let (e, rest) = postcard::take_from_bytes::<E>(r.avail())
            .map_err(|e| invalid(format!("entry: {e}")))?;
        let used = r.avail().len() - rest.len();
        r.pos += used;
        entries.push(e);
    }

    let mut tail = Vec::new();
    r.rest(&mut tail)?;
    let t: Tail = postcard::from_bytes(&tail).map_err(|e| invalid(format!("tail: {e}")))?;
    Ok(Snapshot {
        arena,
        entries,
        exts: t.exts,
        extra: t.extra,
        meta: t.meta,
        crawl_gen: t.crawl_gen,
    })
}

/// A read buffer that can guarantee a minimum lookahead, so a postcard value
/// never has to be decoded across a refill.
struct Chunks<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
}

impl<R: Read> Chunks<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: vec![0; 1 << 20].into_boxed_slice(),
            pos: 0,
            end: 0,
        }
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.pos..self.end]
    }

    /// Buffers at least `n` bytes unless the input ends first.
    fn fill(&mut self, n: usize) -> io::Result<()> {
        if self.end - self.pos >= n {
            return Ok(());
        }
        self.buf.copy_within(self.pos..self.end, 0);
        self.end -= self.pos;
        self.pos = 0;
        while self.end < n {
            let got = self.inner.read(&mut self.buf[self.end..])?;
            if got == 0 {
                break;
            }
            self.end += got;
        }
        Ok(())
    }

    fn varint(&mut self) -> io::Result<u64> {
        self.fill(10)?;
        let mut v = 0u64;
        for (i, &b) in self.avail().iter().take(10).enumerate() {
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                self.pos += i + 1;
                return Ok(v);
            }
        }
        Err(invalid("bad or truncated varint"))
    }

    /// Reads exactly `n` bytes into `out`; bytes past the buffer go straight
    /// from the input into `out`.
    fn exact(&mut self, n: usize, out: &mut Vec<u8>) -> io::Result<()> {
        out.reserve_exact(n);
        let have = (self.end - self.pos).min(n);
        out.extend_from_slice(&self.buf[self.pos..self.pos + have]);
        self.pos += have;
        let need = n - have;
        if need > 0 {
            let start = out.len();
            out.resize(start + need, 0);
            self.inner.read_exact(&mut out[start..])?;
        }
        Ok(())
    }

    fn rest(&mut self, out: &mut Vec<u8>) -> io::Result<()> {
        out.extend_from_slice(self.avail());
        self.pos = self.end;
        self.inner.read_to_end(out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory_store::state::IndexState;
    use crate::store::FileRecord;

    fn rec(i: usize) -> FileRecord {
        FileRecord {
            id: None,
            path: format!("/dir/sub/file{i}.rs"),
            name: format!("file{i}.rs"),
            extension: Some("rs".into()),
            size_bytes: i as u64,
            mtime: -(i as i64),
            ctime: 0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: i64::MAX,
            archive_path: None,
        }
    }

    fn encode(st: &IndexState) -> Vec<u8> {
        let mut out = Vec::new();
        let n = write(&mut out, &st.to_snapshot()).unwrap();
        assert_eq!(n as usize, out.len());
        out
    }

    #[test]
    fn stream_matches_postcard_and_round_trips() {
        let mut st = IndexState::from_records((0..5000).map(rec).collect());
        st.meta.insert("last_crawl_time".into(), "9".into());
        st.crawl_gen = 4;
        let out = encode(&st);
        // Same bytes as the old whole-struct encoding, so existing snapshots load.
        assert_eq!(out, postcard::to_allocvec(&st.to_snapshot()).unwrap());

        let back = IndexState::from_snapshot(read(&out[..], out.len() as u64).unwrap(), 0).unwrap();
        assert_eq!(back.len(), 5000);
        assert_eq!(
            back.meta.get("last_crawl_time").map(String::as_str),
            Some("9")
        );
        assert_eq!(back.crawl_gen, 4);
        let i = back.find("/dir/sub/file4321.rs").unwrap();
        assert_eq!(
            format!("{:?}", back.view(i).to_record()),
            format!("{:?}", rec(4321))
        );
    }

    #[test]
    fn removed_paths_are_not_written() {
        let mut st = IndexState::from_records((0..100).map(rec).collect());
        let full = encode(&st).len();
        // Few enough removals that the store does not compact by itself.
        for i in (0..100).step_by(10) {
            assert!(st.remove(&rec(i).path));
        }
        let out = encode(&st);
        assert!(out.len() < full - 10 * "/dir/sub/file0.rs".len());
        let back = IndexState::from_snapshot(read(&out[..], out.len() as u64).unwrap(), 0).unwrap();
        assert_eq!(back.len(), 90);
        for i in 0..100 {
            assert_eq!(back.find(&rec(i).path).is_some(), i % 10 != 0, "{i}");
        }
        let i = back.find("/dir/sub/file99.rs").unwrap();
        assert_eq!(
            format!("{:?}", back.view(i).to_record()),
            format!("{:?}", rec(99))
        );
    }

    #[test]
    fn truncated_or_lying_body_is_an_error() {
        let out = encode(&IndexState::from_records((0..500).map(rec).collect()));
        for cut in [1, out.len() / 2, out.len() - 1] {
            assert!(
                read::<Entry>(&out[..cut], cut as u64).is_err(),
                "cut at {cut}"
            );
        }
        // A huge entry count must not be trusted for the allocation.
        let mut lie = Vec::new();
        write_varint(&mut lie, 0).unwrap();
        write_varint(&mut lie, u64::MAX / 2).unwrap();
        assert!(read::<Entry>(&lie[..], lie.len() as u64).is_err());
    }
}
