//! What's in a ZIP file (and APK, JAR…), from its central directory at the end: only the end of
//! the file and that directory are read, never the files packed in it.

use crate::media::Read;
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub struct Packed {
    pub name: String,
    /// Unpacked.
    pub size: u64,
    pub dir: bool,
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
    let end = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..i + 4] == [0x50, 0x4B, 0x05, 0x06]).ok_or_else(bad)?;
    let (mut total, mut dir_size, mut dir_at) = (u16_at(&tail, end + 10), u32_at(&tail, end + 12), u32_at(&tail, end + 16));
    // ZIP64: the real numbers are in another record, found through the locator just before.
    if [total, dir_size, dir_at].iter().any(|v| matches!(v, Some(0xFFFF) | Some(0xFFFF_FFFF))) && end >= 20 && tail[end - 20..end - 16] == [0x50, 0x4B, 0x06, 0x07] {
        let rec_at = u64_at(&tail, end - 12).ok_or_else(bad)?;
        let rec = read(rec_at, 56)?;
        if rec.get(0..4) == Some(&[0x50, 0x4B, 0x06, 0x06]) {
            (total, dir_size, dir_at) = (u64_at(&rec, 32), u64_at(&rec, 40), u64_at(&rec, 48));
        }
    }
    let (Some(total), Some(dir_size), Some(dir_at)) = (total, dir_size, dir_at) else { return Err(bad()) };
    let dir = read(dir_at, dir_size.min(DIR_MAX))?;
    let mut items = Vec::new();
    let mut at = 0usize;
    while items.len() < LIST_MAX && dir.get(at..at + 4) == Some(&[0x50, 0x4B, 0x01, 0x02]) {
        let (Some(mut size), Some(name_len), Some(extra_len), Some(note_len)) = (u32_at(&dir, at + 24), u16_at(&dir, at + 28), u16_at(&dir, at + 30), u16_at(&dir, at + 32)) else { break };
        let name_at = at + 46;
        let Some(name) = dir.get(name_at..name_at + name_len as usize) else { break };
        let name = String::from_utf8_lossy(name).into_owned();
        if size == 0xFFFF_FFFF {
            // The ZIP64 extra field starts with the unpacked size when it didn't fit.
            let extra = dir.get(name_at + name_len as usize..name_at + (name_len + extra_len) as usize).unwrap_or(&[]);
            let mut e = 0;
            while let (Some(id), Some(len)) = (u16_at(extra, e), u16_at(extra, e + 2)) {
                if id == 1 {
                    size = u64_at(extra, e + 4).unwrap_or(size);
                    break;
                }
                e += 4 + len as usize;
            }
        }
        items.push(Packed { dir: name.ends_with('/'), name, size });
        at = name_at + (name_len + extra_len + note_len) as usize;
    }
    Ok(Listing { items, total })
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    #[test]
    fn lists_without_reading_the_files() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            z.add_directory("pics/", o).unwrap();
            z.start_file("pics/a.txt", o).unwrap();
            z.write_all(&[b'x'; 100_000]).unwrap();
            z.start_file("é.txt", o).unwrap();
            z.write_all(b"hi").unwrap();
            z.set_comment("a comment");
            z.finish().unwrap();
        }
        let file = buf.into_inner();
        let mut read_bytes = 0;
        let mut read = |at: u64, len: u64| {
            let v: Vec<u8> = file.iter().skip(at as usize).take(len as usize).copied().collect();
            read_bytes += v.len();
            Ok(v)
        };
        let l = super::list(file.len() as u64, &mut read).unwrap();
        assert_eq!(l.total, 3);
        let got: Vec<_> = l.items.iter().map(|p| (p.name.as_str(), p.size, p.dir)).collect();
        assert_eq!(got, [("pics/", 0, true), ("pics/a.txt", 100_000, false), ("é.txt", 2, false)]);
        // The end of the file (the last 64 KB and a bit) and the directory, not the 100 KB file.
        assert!(read_bytes < 66_000, "{read_bytes}");

        let mut read = |_: u64, _: u64| Ok(b"not a zip at all, just some text".to_vec());
        assert!(super::list(32, &mut read).is_err());
    }
}
