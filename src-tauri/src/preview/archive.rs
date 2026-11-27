//! What's in a ZIP file (and APK, JAR…), from its central directory at the end: only the end of
//! the file and that directory are read, never the files packed in it.

use crate::preview::media::Read;
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub struct Packed {
    pub name: String,
    /// Unpacked.
    pub size: u64,
    pub dir: bool,
    /// Packed without compression or encryption: it can be read in place, a range at a time.
    pub stored: bool,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Listing {
    /// The first `LIST_MAX`.
    pub items: Vec<Packed>,
    /// How many there are in all.
    pub total: u64,
}

const LIST_MAX: usize = 5000;
/// The biggest central directory read.
const DIR_MAX: u64 = 32 * 1024 * 1024;
/// End-of-directory record (22 bytes) and a comment of up to 64 KB.
const TAIL: u64 = 22 + 65535;

fn u16_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?) as u64)
}
fn u32_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?) as u64)
}
fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

pub fn list(size: u64, read: &mut Read) -> Result<Listing, String> {
    let bad = || "Not a ZIP file, or a damaged one.".to_string();
    let tail_at = size.saturating_sub(TAIL);
    let tail = read(tail_at, size - tail_at)?;
    // The last end-of-directory signature.
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| tail[i..i + 4] == [0x50, 0x4B, 0x05, 0x06])
        .ok_or_else(bad)?;
    let (mut total, mut dir_size, mut dir_at) = (
        u16_at(&tail, end + 10),
        u32_at(&tail, end + 12),
        u32_at(&tail, end + 16),
    );
    let mut zip64 = false;
    // ZIP64: the real numbers are in another record, found through the locator just before.
    if [total, dir_size, dir_at]
        .iter()
        .any(|v| matches!(v, Some(0xFFFF) | Some(0xFFFF_FFFF)))
        && end >= 20
        && tail[end - 20..end - 16] == [0x50, 0x4B, 0x06, 0x07]
    {
        let rec_at = u64_at(&tail, end - 12).ok_or_else(bad)?;
        let rec = read(rec_at, 56)?;
        if rec.get(0..4) == Some(&[0x50, 0x4B, 0x06, 0x06]) {
            (total, dir_size, dir_at) = (u64_at(&rec, 32), u64_at(&rec, 40), u64_at(&rec, 48));
            zip64 = true;
        }
    }
    let (Some(total), Some(dir_size), Some(mut dir_at)) = (total, dir_size, dir_at) else {
        return Err(bad());
    };
    // With something before the ZIP (a self-extracting one), offsets are from the ZIP's start:
    // the directory ends where the end record starts.
    let end_at = tail_at + end as u64;
    if !zip64 && dir_at + dir_size < end_at {
        dir_at = end_at - dir_size;
    }
    let dir = read(dir_at, dir_size.min(DIR_MAX))?;
    let mut items = Vec::new();
    let mut at = 0usize;
    while items.len() < LIST_MAX && dir.get(at..at + 4) == Some(&[0x50, 0x4B, 0x01, 0x02]) {
        let stored =
            u16_at(&dir, at + 10) == Some(0) && u16_at(&dir, at + 8).is_some_and(|f| f & 1 == 0);
        let (Some(mut size), Some(name_len), Some(extra_len), Some(note_len)) = (
            u32_at(&dir, at + 24),
            u16_at(&dir, at + 28),
            u16_at(&dir, at + 30),
            u16_at(&dir, at + 32),
        ) else {
            break;
        };
        let name_at = at + 46;
        let Some(name) = dir.get(name_at..name_at + name_len as usize) else {
            break;
        };
        // Some Windows tools write \ between folders.
        let name = String::from_utf8_lossy(name).replace('\\', "/");
        if size == 0xFFFF_FFFF {
            // The ZIP64 extra field starts with the unpacked size when it didn't fit.
            let extra = dir
                .get(name_at + name_len as usize..name_at + (name_len + extra_len) as usize)
                .unwrap_or(&[]);
            let mut e = 0;
            while let (Some(id), Some(len)) = (u16_at(extra, e), u16_at(extra, e + 2)) {
                if id == 1 {
                    size = u64_at(extra, e + 4).unwrap_or(size);
                    break;
                }
                e += 4 + len as usize;
            }
        }
        items.push(Packed {
            dir: name.ends_with('/'),
            name,
            size,
            stored,
        });
        at = name_at + (name_len + extra_len + note_len) as usize;
    }
    Ok(Listing { items, total })
}

/// Reads a file through `read` a block at a time, for `zip::ZipArchive`.
struct Ranged<'r, 'a> {
    read: &'r mut Read<'a>,
    size: u64,
    at: u64,
    /// The block last read, and where it starts.
    block: (u64, Vec<u8>),
}

const BLOCK: u64 = 256 * 1024;

impl std::io::Read for Ranged<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let (start, data) = &self.block;
        if self.at < *start || self.at >= start + data.len() as u64 {
            let len = BLOCK.max(buf.len() as u64).min(self.size - self.at);
            let data = (self.read)(self.at, len).map_err(std::io::Error::other)?;
            if data.is_empty() {
                return Ok(0);
            }
            self.block = (self.at, data);
        }
        let (start, data) = &self.block;
        let from = (self.at - start) as usize;
        let n = buf.len().min(data.len() - from);
        buf[..n].copy_from_slice(&data[from..from + n]);
        self.at += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for Ranged<'_, '_> {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let at = match to {
            std::io::SeekFrom::Start(n) => n as i64,
            std::io::SeekFrom::End(n) => self.size as i64 + n,
            std::io::SeekFrom::Current(n) => self.at as i64 + n,
        };
        if at < 0 {
            return Err(std::io::Error::other("Seek before the start."));
        }
        self.at = at as u64;
        Ok(self.at)
    }
}

/// Where the file `name` lies in the ZIP, as (offset, length), when it's packed without
/// compression or encryption: it can then be read in place. Only the directory is read.
pub fn stored_span(size: u64, read: &mut Read, name: &str) -> Result<Option<(u64, u64)>, String> {
    let r = Ranged {
        read,
        size,
        at: 0,
        block: (0, Vec::new()),
    };
    let mut zip =
        zip::ZipArchive::new(r).map_err(|e| format!("Not a ZIP file, or a damaged one ({e})."))?;
    let i = zip
        .index_for_name(name)
        .ok_or_else(|| format!("{name}: not in the ZIP."))?;
    let f = zip.by_index_raw(i).map_err(|e| format!("{name}: {e}"))?;
    // by_index_raw has read the local header, so the offset is known.
    let start = f
        .data_start()
        .ok_or_else(|| format!("{name}: where its data starts is unknown."))?;
    Ok(
        (f.compression() == zip::CompressionMethod::Stored && !f.encrypted())
            .then(|| (start, f.size())),
    )
}

/// One file packed in the ZIP, unpacked; only its part of the ZIP (and the directory) is read.
/// Refused when it unpacks to more than `max` bytes.
pub fn unpack(size: u64, read: &mut Read, name: &str, max: u64) -> Result<Vec<u8>, String> {
    unpack_each(size, read, max, |get| get(name))?
}

/// Unpacks the files `with` asks `get` for, reading the directory once. Each is refused when it
/// unpacks to more than `max` bytes.
pub fn unpack_each<T>(
    size: u64,
    read: &mut Read,
    max: u64,
    with: impl FnOnce(&mut dyn FnMut(&str) -> Result<Vec<u8>, String>) -> T,
) -> Result<T, String> {
    let r = Ranged {
        read,
        size,
        at: 0,
        block: (0, Vec::new()),
    };
    let mut zip =
        zip::ZipArchive::new(r).map_err(|e| format!("Not a ZIP file, or a damaged one ({e})."))?;
    let mut get = |name: &str| {
        let mut f = zip.by_name(name).map_err(|e| format!("{name}: {e}"))?;
        if f.size() > max {
            return Err(format!(
                "Too big to show ({} MB unpacked).",
                f.size() / (1024 * 1024)
            ));
        }
        let mut out = Vec::with_capacity(f.size() as usize);
        std::io::Read::read_to_end(&mut f, &mut out).map_err(|e| format!("{name}: {e}"))?;
        Ok(out)
    };
    Ok(with(&mut get))
}

/// Up to `len` bytes from `start` of the file `name` packed in the ZIP, and how big the file is
/// unpacked. One stored as it is is read in place; a compressed one is unpacked up to there, the
/// part before `start` let go of as it goes. Refused when the bytes asked for are more than `max`.
pub fn unpack_part(
    size: u64,
    read: &mut Read,
    name: &str,
    start: u64,
    len: u64,
    max: u64,
) -> Result<(Vec<u8>, u64), String> {
    use std::io::Read as _;
    let r = Ranged {
        read,
        size,
        at: 0,
        block: (0, Vec::new()),
    };
    let mut zip =
        zip::ZipArchive::new(r).map_err(|e| format!("Not a ZIP file, or a damaged one ({e})."))?;
    let i = zip
        .index_for_name(name)
        .ok_or_else(|| format!("{name}: not in the ZIP."))?;
    let (full, span) = {
        let f = zip.by_index_raw(i).map_err(|e| format!("{name}: {e}"))?;
        if f.is_dir() {
            return Err(format!("{name}: a folder in the ZIP, not a file."));
        }
        let stored = f.compression() == zip::CompressionMethod::Stored && !f.encrypted();
        (f.size(), f.data_start().filter(|_| stored))
    };
    let want = len.min(full.saturating_sub(start));
    if want > max {
        return Err(format!(
            "{name}: that's {} MB, more than the {} MB read at once.",
            want.div_ceil(1024 * 1024),
            max / 1024 / 1024
        ));
    }
    if want == 0 {
        return Ok((Vec::new(), full));
    }
    if let Some(at) = span {
        let r = zip.into_inner();
        return Ok(((r.read)(at + start, want)?, full));
    }
    let mut f = zip.by_index(i).map_err(|e| format!("{name}: {e}"))?;
    let bad = |e: std::io::Error| format!("{name}: {e}");
    std::io::copy(&mut (&mut f).take(start), &mut std::io::sink()).map_err(bad)?;
    let mut out = Vec::with_capacity(want as usize);
    (&mut f).take(want).read_to_end(&mut out).map_err(bad)?;
    Ok((out, full))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    #[test]
    fn lists_without_reading_the_files() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            z.add_directory("pics/", o).unwrap();
            z.start_file("pics/a.txt", o).unwrap();
            z.write_all(&[b'x'; 100_000]).unwrap();
            z.start_file("é.txt", o).unwrap();
            z.write_all(b"hi").unwrap();
            z.set_comment("a comment").unwrap();
            z.finish().unwrap();
        }
        let file = buf.into_inner();
        let mut read_bytes = 0;
        let mut read = |at: u64, len: u64| {
            let v: Vec<u8> = file
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect();
            read_bytes += v.len();
            Ok(v)
        };
        let l = super::list(file.len() as u64, &mut read).unwrap();
        assert_eq!(l.total, 3);
        let got: Vec<_> = l
            .items
            .iter()
            .map(|p| (p.name.as_str(), p.size, p.dir))
            .collect();
        assert_eq!(
            got,
            [
                ("pics/", 0, true),
                ("pics/a.txt", 100_000, false),
                ("é.txt", 2, false)
            ]
        );
        // The end of the file (the last 64 KB and a bit) and the directory, not the 100 KB file.
        assert!(read_bytes < 66_000, "{read_bytes}");

        let mut read = |_: u64, _: u64| Ok(b"not a zip at all, just some text".to_vec());
        assert!(super::list(32, &mut read).is_err());
    }

    #[test]
    fn reads_part_of_a_packed_file() {
        let mut buf = Cursor::new(Vec::new());
        let text: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            for (name, how) in [
                ("stored.bin", zip::CompressionMethod::Stored),
                ("packed.bin", zip::CompressionMethod::Deflated),
            ] {
                let o = zip::write::SimpleFileOptions::default().compression_method(how);
                z.start_file(name, o).unwrap();
                z.write_all(&text).unwrap();
            }
            z.add_directory("d/", zip::write::SimpleFileOptions::default())
                .unwrap();
            z.finish().unwrap();
        }
        let file = buf.into_inner();
        let mut read = |at: u64, len: u64| {
            let from = (at as usize).min(file.len());
            let to = (at.saturating_add(len) as usize).min(file.len());
            Ok(file[from..to].to_vec())
        };
        let size = file.len() as u64;
        for name in ["stored.bin", "packed.bin"] {
            let (got, full) =
                super::unpack_part(size, &mut read, name, 150_000, 10, 1 << 20).unwrap();
            assert_eq!(
                (got.as_slice(), full),
                (&text[150_000..150_010], 200_000),
                "{name}"
            );
            // Past the end: what's left, then nothing.
            let (got, _) =
                super::unpack_part(size, &mut read, name, 199_995, 100, 1 << 20).unwrap();
            assert_eq!(got, &text[199_995..]);
            let (got, _) =
                super::unpack_part(size, &mut read, name, 300_000, 100, 1 << 20).unwrap();
            assert!(got.is_empty());
        }
        // More than max, a folder, and a name that isn't there are refused.
        assert!(
            super::unpack_part(size, &mut read, "packed.bin", 0, u64::MAX, 1000)
                .unwrap_err()
                .contains("more than")
        );
        assert!(super::unpack_part(size, &mut read, "d/", 0, 10, 1000).is_err());
        assert!(super::unpack_part(size, &mut read, "nope", 0, 10, 1000).is_err());
    }

    #[test]
    fn unpacks_one_file() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            z.start_file("big.bin", o).unwrap();
            z.write_all(&[7u8; 600_000]).unwrap();
            z.start_file("dir/hi.txt", o).unwrap();
            z.write_all(b"hello").unwrap();
            z.finish().unwrap();
        }
        let file = buf.into_inner();
        let mut read = |at: u64, len: u64| {
            Ok(file
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect())
        };
        assert_eq!(
            super::unpack(file.len() as u64, &mut read, "dir/hi.txt", 100).unwrap(),
            b"hello"
        );
        assert_eq!(
            super::unpack(file.len() as u64, &mut read, "big.bin", 1 << 20)
                .unwrap()
                .len(),
            600_000
        );
        assert!(super::unpack(file.len() as u64, &mut read, "big.bin", 1000).is_err());
        assert!(super::unpack(file.len() as u64, &mut read, "nope", 1000).is_err());
    }

    #[test]
    fn lists_a_zip_with_something_before_it() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            z.start_file("a\\b.txt", o).unwrap();
            z.write_all(b"hi").unwrap();
            z.finish().unwrap();
        }
        let mut file = vec![b'#'; 1000];
        file.extend(buf.into_inner());
        let mut read = |at: u64, len: u64| {
            Ok(file
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect())
        };
        let l = super::list(file.len() as u64, &mut read).unwrap();
        let got: Vec<_> = l.items.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(got, ["a/b.txt"]);
    }

    #[test]
    fn finds_stored_files_in_place() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let packed = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            z.start_file("small.txt", packed).unwrap();
            z.write_all(&[b'a'; 5000]).unwrap();
            z.start_file("clip.mp4", stored).unwrap();
            z.write_all(&(0..300_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>())
                .unwrap();
            z.finish().unwrap();
        }
        // With something before the ZIP, the offset is still where the bytes are in the file.
        let mut file = vec![b'#'; 700];
        file.extend(buf.into_inner());
        let mut read = |at: u64, len: u64| {
            Ok(file
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect())
        };
        let l = super::list(file.len() as u64, &mut read).unwrap();
        let got: Vec<_> = l
            .items
            .iter()
            .map(|p| (p.name.as_str(), p.stored))
            .collect();
        assert_eq!(got, [("small.txt", false), ("clip.mp4", true)]);
        assert_eq!(
            super::stored_span(file.len() as u64, &mut read, "small.txt").unwrap(),
            None
        );
        let (at, len) = super::stored_span(file.len() as u64, &mut read, "clip.mp4")
            .unwrap()
            .unwrap();
        assert_eq!(len, 300_000);
        assert_eq!(
            &file[at as usize + 1000..at as usize + 1004],
            &[1000 % 251, 1001 % 251, 1002 % 251, 1003 % 251].map(|v| v as u8)
        );
        assert!(super::stored_span(file.len() as u64, &mut read, "nope").is_err());
    }
}
