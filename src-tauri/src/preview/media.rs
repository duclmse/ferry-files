//! Width, height and length of pictures and videos, from the file's header: only the first part
//! of the file is read (and, for an MP4 or MOV, its `moov` box, wherever it is).

use serde::Serialize;

#[derive(Serialize, Default, Debug, PartialEq)]
pub struct MediaInfo {
    /// As shown: a photo or video turned a quarter has its width and height swapped.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Seconds.
    pub duration: Option<f64>,
}

/// Bytes read from the start for a picture's header. A JPEG's size comes after its EXIF block,
/// which can hold a thumbnail of up to 64 KB.
pub const HEAD: u64 = 128 * 1024;
/// The biggest `moov` box read; a long video's is a few MB.
const MOOV_MAX: u64 = 32 * 1024 * 1024;

/// Reads `len` bytes from `start` (fewer at the end of the file).
pub type Read<'a> = dyn FnMut(u64, u64) -> Result<Vec<u8>, String> + 'a;

pub fn info(name: &str, read: &mut Read) -> Result<MediaInfo, String> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, x)| x.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "3gp" | "m4a" => movie(read),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" | "heif" | "avif" => {
            let head = read(0, HEAD)?;
            Ok(match picture(&head) {
                Some((w, h)) => MediaInfo {
                    width: Some(w),
                    height: Some(h),
                    duration: None,
                },
                None => MediaInfo::default(),
            })
        }
        _ => Ok(MediaInfo::default()),
    }
}

/// A picture's size from the start of its file.
pub fn picture(head: &[u8]) -> Option<(u32, u32)> {
    // HEIC and AVIF: ISO boxes, like MP4.
    if head.get(4..8) == Some(b"ftyp") {
        return heif(head);
    }
    let u16le = |at: usize| Some(u16::from_le_bytes(head.get(at..at + 2)?.try_into().ok()?) as u32);
    let u32le = |at: usize| Some(u32::from_le_bytes(head.get(at..at + 4)?.try_into().ok()?));
    let u24le = |at: usize| {
        Some(u32::from_le_bytes([
            *head.get(at)?,
            *head.get(at + 1)?,
            *head.get(at + 2)?,
            0,
        ]))
    };
    if head.starts_with(&[0xFF, 0xD8]) {
        jpeg(head)
    } else if head.starts_with(b"\x89PNG") {
        // IHDR, the first chunk.
        Some((u32_at(head, 16)?, u32_at(head, 20)?))
    } else if head.starts_with(b"GIF8") {
        Some((u16le(6)?, u16le(8)?))
    } else if head.starts_with(b"BM") {
        // The height is negative when rows go top to bottom.
        Some((u32le(18)?, (u32le(22)? as i32).unsigned_abs()))
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        match head.get(12..16)? {
            b"VP8X" => Some((u24le(24)? + 1, u24le(27)? + 1)),
            b"VP8L" => {
                let b = u32le(21)?;
                Some(((b & 0x3FFF) + 1, ((b >> 14) & 0x3FFF) + 1))
            }
            b"VP8 " => Some((u16le(26)? & 0x3FFF, u16le(28)? & 0x3FFF)),
            _ => None,
        }
    } else {
        None
    }
}

/// One ISO box: its type and contents.
struct Bx<'a> {
    kind: &'a [u8],
    body: &'a [u8],
}

/// The boxes one after another in `data`. A box cut off at the end is given as far as it goes.
fn boxes(data: &[u8]) -> impl Iterator<Item = Bx<'_>> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        let (size, head) = box_head(data.get(at..)?)?;
        let end = if size == 0 {
            data.len()
        } else {
            at.checked_add(size as usize)?.min(data.len())
        };
        let b = Bx {
            kind: &data[at + 4..at + 8],
            body: data.get(at + head..end)?,
        };
        at = if size == 0 {
            data.len()
        } else {
            at + size as usize
        };
        Some(b)
    })
}

/// A box's size (0: to the end of the file) and header length.
fn box_head(d: &[u8]) -> Option<(u64, usize)> {
    let size = u32::from_be_bytes(d.get(0..4)?.try_into().ok()?) as u64;
    d.get(4..8)?;
    if size == 1 {
        let big = u64::from_be_bytes(d.get(8..16)?.try_into().ok()?);
        return (big >= 16).then_some((big, 16));
    }
    (size == 0 || size >= 8).then_some((size, 8))
}

fn find<'a>(data: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
    boxes(data).find(|b| b.kind == kind).map(|b| b.body)
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

/// HEIC or AVIF: the biggest `ispe` (the whole picture, not one of its tiles), turned by `irot`.
fn heif(head: &[u8]) -> Option<(u32, u32)> {
    let meta = find(head, b"meta")?.get(4..)?;
    let ipco = find(find(meta, b"iprp")?, b"ipco")?;
    let mut size = None::<(u32, u32)>;
    let mut turned = false;
    for b in boxes(ipco) {
        match b.kind {
            b"ispe" => {
                let s = (u32_at(b.body, 4)?, u32_at(b.body, 8)?);
                if size.is_none_or(|(w, h)| s.0 as u64 * s.1 as u64 > w as u64 * h as u64) {
                    size = Some(s);
                }
            }
            b"irot" => turned |= b.body.first().is_some_and(|a| a & 1 == 1),
            _ => {}
        }
    }
    size.map(|(w, h)| if turned { (h, w) } else { (w, h) })
}

/// A JPEG's size from its frame header (SOFn), swapped when its EXIF orientation turns it a
/// quarter (5 to 8). The image crate's decoder wants the whole file, so the segments are walked
/// here.
fn jpeg(d: &[u8]) -> Option<(u32, u32)> {
    let mut turned = false;
    let mut at = 2;
    loop {
        if *d.get(at)? != 0xFF {
            return None;
        }
        let m = *d.get(at + 1)?;
        // Fill bytes, and markers without a length.
        if m == 0xFF {
            at += 1;
            continue;
        }
        if m == 0x01 || (0xD0..=0xD7).contains(&m) {
            at += 2;
            continue;
        }
        let len = u16::from_be_bytes([*d.get(at + 2)?, *d.get(at + 3)?]) as usize;
        let seg = d.get(at + 4..(at + 2 + len).min(d.len()))?;
        match m {
            0xE1 if seg.starts_with(b"Exif\0\0") => {
                turned = exif_orientation(&seg[6..]).is_some_and(|o| (5..=8).contains(&o))
            }
            // SOF0 to SOF15, apart from DHT, JPG and DAC.
            0xC0..=0xCF if !matches!(m, 0xC4 | 0xC8 | 0xCC) => {
                let h = u16::from_be_bytes([*seg.get(1)?, *seg.get(2)?]) as u32;
                let w = u16::from_be_bytes([*seg.get(3)?, *seg.get(4)?]) as u32;
                return Some(if turned { (h, w) } else { (w, h) });
            }
            // Start of scan: the picture data, and no frame header before it.
            0xDA => return None,
            _ => {}
        }
        at += 2 + len;
    }
}

/// The orientation tag (0x0112) in IFD0 of a TIFF block.
fn exif_orientation(t: &[u8]) -> Option<u16> {
    let le = match t.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_ = |at: usize| {
        t.get(at..at + 2).map(|b| {
            if le {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                u16::from_be_bytes([b[0], b[1]])
            }
        })
    };
    let u32_ = |at: usize| {
        t.get(at..at + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            }
        })
    };
    let ifd = u32_(4)? as usize;
    let n = u16_(ifd)? as usize;
    (0..n)
        .map(|i| ifd + 2 + i * 12)
        .find(|&e| u16_(e) == Some(0x0112))
        .and_then(|e| u16_(e + 8))
}

/// MP4, MOV and the like: finds the `moov` box among the top-level ones, reading only their
/// headers, then reads it for the length (`mvhd`) and the picture size (`tkhd`).
fn movie(read: &mut Read) -> Result<MediaInfo, String> {
    Ok(match moov_at(read)? {
        Some((at, len)) => moov(&read(at, len)?),
        None => MediaInfo::default(),
    })
}

fn moov(moov: &[u8]) -> MediaInfo {
    let mut info = MediaInfo::default();
    if let Some(m) = find(moov, b"mvhd") {
        // Version 1 has 64-bit times and length.
        let (scale, len) = if m.first() == Some(&1) {
            (u32_at(m, 20), u64_at(m, 24))
        } else {
            (u32_at(m, 12), u32_at(m, 16).map(u64::from))
        };
        if let (Some(scale), Some(len)) = (scale, len) {
            if scale > 0 && len > 0 && len != u32::MAX as u64 && len != u64::MAX {
                info.duration = Some(len as f64 / scale as f64);
            }
        }
    }
    // The first track with a picture.
    for t in boxes(moov).filter(|b| b.kind == b"trak") {
        let Some(k) = find(t.body, b"tkhd") else {
            continue;
        };
        let at = if k.first() == Some(&1) { 52 } else { 40 };
        let (Some(a), Some(b), Some(w), Some(h)) = (
            u32_at(k, at),
            u32_at(k, at + 4),
            u32_at(k, at + 36),
            u32_at(k, at + 40),
        ) else {
            continue;
        };
        // 16.16 fixed point.
        let (w, h) = (w >> 16, h >> 16);
        if w == 0 || h == 0 {
            continue;
        }
        // The matrix turns it a quarter when its first entry is 0 and the second isn't.
        let turned = a == 0 && b != 0;
        (info.width, info.height) = if turned {
            (Some(h), Some(w))
        } else {
            (Some(w), Some(h))
        };
        break;
    }
    info
}

/// When a photo or video was taken, as (year, month, day): EXIF `DateTimeOriginal` in a JPEG or
/// HEIC (else its `DateTime`), or an MP4 or MOV's creation time. None when the file doesn't say.
pub fn taken(name: &str, read: &mut Read) -> Option<(i32, u32, u32)> {
    let ext = name
        .rsplit_once('.')
        .map(|(_, x)| x.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "m4v" | "mov" | "3gp" => movie_date(read),
        "jpg" | "jpeg" => {
            let head = read(0, HEAD).ok()?;
            exif_date(jpeg_exif(&head)?)
        }
        "heic" | "heif" | "avif" => {
            let head = read(0, HEAD).ok()?;
            let (at, len) = heif_exif(&head)?;
            let item = if at + len <= head.len() as u64 {
                head[at as usize..(at + len) as usize].to_vec()
            } else {
                read(at, len.min(HEAD)).ok()?
            };
            // The item starts with where the TIFF block begins, after "Exif\0\0".
            let skip = u32_at(&item, 0)? as usize;
            exif_date(item.get(4 + skip..)?)
        }
        _ => None,
    }
}

/// The TIFF block of a JPEG's EXIF segment.
fn jpeg_exif(d: &[u8]) -> Option<&[u8]> {
    let mut at = 2;
    loop {
        if *d.get(at)? != 0xFF {
            return None;
        }
        let m = *d.get(at + 1)?;
        if m == 0xFF {
            at += 1;
            continue;
        }
        if m == 0x01 || (0xD0..=0xD7).contains(&m) {
            at += 2;
            continue;
        }
        if m == 0xDA {
            return None;
        }
        let len = u16::from_be_bytes([*d.get(at + 2)?, *d.get(at + 3)?]) as usize;
        let seg = d.get(at + 4..(at + 2 + len).min(d.len()))?;
        if m == 0xE1 && seg.starts_with(b"Exif\0\0") {
            return Some(&seg[6..]);
        }
        at += 2 + len;
    }
}

/// Where a HEIC's EXIF item is in the file, from its `meta` box (`iinf` names it, `iloc` places it).
fn heif_exif(head: &[u8]) -> Option<(u64, u64)> {
    let meta = find(head, b"meta")?.get(4..)?;
    let iinf = find(meta, b"iinf")?;
    let entries = if iinf.first() == Some(&0) {
        iinf.get(6..)?
    } else {
        iinf.get(8..)?
    };
    let id = boxes(entries).filter(|b| b.kind == b"infe").find_map(|b| {
        let v = *b.body.first()?;
        let (id, kind_at) = match v {
            2 => (
                u16::from_be_bytes(b.body.get(4..6)?.try_into().ok()?) as u32,
                8,
            ),
            3 => (u32_at(b.body, 4)?, 10),
            _ => return None,
        };
        (b.body.get(kind_at..kind_at + 4)? == b"Exif").then_some(id)
    })?;
    let l = find(meta, b"iloc")?;
    let v = *l.first()?;
    let (off_size, len_size) = ((l.get(4)? >> 4) as usize, (l.get(4)? & 15) as usize);
    let (base_size, index_size) = (
        (l.get(5)? >> 4) as usize,
        if v > 0 { (l.get(5)? & 15) as usize } else { 0 },
    );
    let num = |at: usize, n: usize| -> Option<u64> {
        (0..n).try_fold(0u64, |acc, i| Some(acc << 8 | *l.get(at + i)? as u64))
    };
    let (count, mut at) = if v < 2 {
        (num(6, 2)?, 8)
    } else {
        (num(6, 4)?, 10)
    };
    for _ in 0..count {
        let id_size = if v < 2 { 2 } else { 4 };
        let item = num(at, id_size)? as u32;
        at += id_size;
        if v > 0 {
            at += 2; // construction method
        }
        at += 2; // data reference index
        let base = num(at, base_size)?;
        at += base_size;
        let extents = num(at, 2)?;
        at += 2;
        let mut first = None;
        for _ in 0..extents {
            at += index_size;
            let off = num(at, off_size)?;
            let len = num(at + off_size, len_size)?;
            at += off_size + len_size;
            first.get_or_insert((base + off, len));
        }
        if item == id {
            return first;
        }
    }
    None
}

/// The date in a TIFF block: `DateTimeOriginal` (0x9003) in the EXIF IFD, else `DateTime` (0x0132).
fn exif_date(t: &[u8]) -> Option<(i32, u32, u32)> {
    let le = match t.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_ = |at: usize| {
        t.get(at..at + 2).map(|b| {
            if le {
                u16::from_le_bytes([b[0], b[1]])
            } else {
                u16::from_be_bytes([b[0], b[1]])
            }
        })
    };
    let u32_ = |at: usize| {
        t.get(at..at + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            }
        })
    };
    // Where the value of `tag` in the IFD at `ifd` is.
    let entry = |ifd: usize, tag: u16| -> Option<usize> {
        let n = u16_(ifd)? as usize;
        (0..n.min(512))
            .map(|i| ifd + 2 + i * 12)
            .find(|&e| u16_(e) == Some(tag))
    };
    // "YYYY:MM:DD HH:MM:SS", always more than 4 bytes, so its value is an offset.
    let date = |e: usize| -> Option<(i32, u32, u32)> {
        let at = u32_(e + 8)? as usize;
        let s = std::str::from_utf8(t.get(at..at + 10)?).ok()?;
        let (y, m, d) = (
            s.get(0..4)?.parse().ok()?,
            s.get(5..7)?.parse().ok()?,
            s.get(8..10)?.parse().ok()?,
        );
        (y > 1900 && (1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
    };
    let ifd0 = u32_(4)? as usize;
    let original = entry(ifd0, 0x8769)
        .and_then(|e| u32_(e + 8))
        .and_then(|exif| entry(exif as usize, 0x9003))
        .and_then(date);
    original.or_else(|| entry(ifd0, 0x0132).and_then(date))
}

/// Where a movie's `moov` box is: its contents' start, and length.
fn moov_at(read: &mut Read) -> Result<Option<(u64, u64)>, String> {
    let mut at = 0u64;
    // A file has a handful of top-level boxes; the limit stops a broken one going on forever.
    for _ in 0..64 {
        let head = read(at, 16)?;
        let Some((size, hl)) = box_head(&head) else {
            break;
        };
        if &head[4..8] == b"moov" {
            let len = if size == 0 {
                MOOV_MAX
            } else {
                (size - hl as u64).min(MOOV_MAX)
            };
            return Ok(Some((at + hl as u64, len)));
        }
        if size == 0 {
            break;
        }
        at += size;
    }
    Ok(None)
}

/// A movie's creation time (`mvhd`, seconds since 1904, UTC), as a date.
fn movie_date(read: &mut Read) -> Option<(i32, u32, u32)> {
    let (at, len) = moov_at(read).ok()??;
    // `mvhd` comes first in `moov`; only its start is read.
    let start = read(at, len.min(4096)).ok()?;
    let m = find(&start, b"mvhd")?;
    let secs = if m.first() == Some(&1) {
        u64_at(m, 4)?
    } else {
        u32_at(m, 4)? as u64
    };
    const TO_1970: i64 = 2_082_844_800;
    let t = secs as i64 - TO_1970;
    if t <= 0 {
        return None;
    }
    use chrono::Datelike;
    let d = chrono::DateTime::from_timestamp(t, 0)?.date_naive();
    Some((d.year(), d.month(), d.day()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn tkhd(w: u32, h: u32, turned: bool) -> Vec<u8> {
        let mut k = vec![0u8; 84];
        let (a, b) = if turned {
            (0u32, 0x10000u32)
        } else {
            (0x10000, 0)
        };
        k[40..44].copy_from_slice(&a.to_be_bytes());
        k[44..48].copy_from_slice(&b.to_be_bytes());
        k[76..80].copy_from_slice(&(w << 16).to_be_bytes());
        k[80..84].copy_from_slice(&(h << 16).to_be_bytes());
        bx(b"tkhd", &k)
    }

    #[test]
    fn mp4_size_and_length_with_moov_at_the_end() {
        let mut mvhd = vec![0u8; 20];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&90_500u32.to_be_bytes());
        let sound = bx(b"trak", &tkhd(0, 0, false));
        let picture = bx(b"trak", &tkhd(1920, 1080, true));
        let moov = bx(b"moov", &[bx(b"mvhd", &mvhd), sound, picture].concat());
        let file = [bx(b"ftyp", b"isom"), bx(b"mdat", &[7u8; 5000]), moov].concat();
        let mut reads = 0;
        let mut read = |at: u64, len: u64| {
            reads += 1;
            Ok(file
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect())
        };
        let i = info("clip.MP4", &mut read).unwrap();
        assert_eq!(
            i,
            MediaInfo {
                width: Some(1080),
                height: Some(1920),
                duration: Some(90.5)
            }
        );
        // Three box headers, then the moov box; the media data isn't read.
        assert_eq!(reads, 4);
    }

    #[test]
    fn heic_takes_the_whole_picture_not_a_tile() {
        let ispe = |w: u32, h: u32| {
            bx(
                b"ispe",
                &[&[0u8; 4][..], &w.to_be_bytes(), &h.to_be_bytes()].concat(),
            )
        };
        let ipco = bx(
            b"ipco",
            &[ispe(512, 512), ispe(4032, 3024), bx(b"irot", &[1])].concat(),
        );
        let meta = bx(b"meta", &[&[0u8; 4][..], &bx(b"iprp", &ipco)].concat());
        let file = [bx(b"ftyp", b"heic"), meta].concat();
        assert_eq!(picture(&file), Some((3024, 4032)));
    }

    #[test]
    fn png_and_turned_jpeg() {
        let mut png = Vec::new();
        image::RgbImage::new(30, 20)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(picture(&png), Some((30, 20)));

        let mut jpeg = Vec::new();
        image::RgbImage::new(30, 20)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        assert_eq!(picture(&jpeg), Some((30, 20)));
        // An EXIF block saying "turned 90°" (orientation 6), big-endian, right after the start.
        let mut tiff = b"MM\0\x2A\0\0\0\x08\0\x01".to_vec();
        tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]);
        tiff.extend_from_slice(&[0, 0, 0, 0]);
        let seg = [&b"Exif\0\0"[..], &tiff].concat();
        let mut turned = vec![0xFF, 0xD8, 0xFF, 0xE1];
        turned.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
        turned.extend_from_slice(&seg);
        turned.extend_from_slice(&jpeg[2..]);
        assert_eq!(picture(&turned), Some((20, 30)));
        // Only the header is needed.
        assert_eq!(picture(&turned[..turned.len() / 2]), Some((20, 30)));
        assert_eq!(picture(&png[..40]), Some((30, 20)));
        for f in [
            image::ImageFormat::Gif,
            image::ImageFormat::Bmp,
            image::ImageFormat::WebP,
        ] {
            let mut v = Vec::new();
            image::RgbaImage::new(30, 20)
                .write_to(&mut Cursor::new(&mut v), f)
                .unwrap();
            assert_eq!(picture(&v[..40]), Some((30, 20)), "{f:?}");
        }
    }

    fn tiff_with_date(date: &str) -> Vec<u8> {
        // Big-endian: IFD0 at 8 with one entry, the EXIF IFD pointer; the EXIF IFD at 26 with
        // DateTimeOriginal pointing at the text at 44.
        let mut t = b"MM\0*".to_vec();
        t.extend(8u32.to_be_bytes());
        t.extend(1u16.to_be_bytes());
        t.extend([0x87, 0x69, 0, 4, 0, 0, 0, 1]);
        t.extend(26u32.to_be_bytes());
        t.extend(0u32.to_be_bytes());
        t.extend(1u16.to_be_bytes());
        t.extend([0x90, 0x03, 0, 2, 0, 0, 0, 20]);
        t.extend(44u32.to_be_bytes());
        t.extend(0u32.to_be_bytes());
        assert_eq!(t.len(), 44);
        t.extend(date.as_bytes());
        t.push(0);
        t
    }

    #[test]
    fn dates_taken() {
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        let exif = [b"Exif\0\0".to_vec(), tiff_with_date("2021:07:04 10:20:30")].concat();
        jpg.extend(((exif.len() + 2) as u16).to_be_bytes());
        jpg.extend(&exif);
        jpg.extend([0xFF, 0xDA, 0, 2]);
        let mut read = |at: u64, len: u64| {
            Ok(jpg
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect())
        };
        assert_eq!(taken("IMG_1.JPG", &mut read), Some((2021, 7, 4)));

        // 2020-01-02 03:04:05 UTC, in seconds since 1904.
        let mut mvhd = vec![0u8; 20];
        mvhd[4..8].copy_from_slice(&((1_577_934_245u64 + 2_082_844_800) as u32).to_be_bytes());
        let mp4 = [bx(b"ftyp", b"isom"), bx(b"moov", &bx(b"mvhd", &mvhd))].concat();
        let mut read = |at: u64, len: u64| {
            Ok(mp4
                .iter()
                .skip(at as usize)
                .take(len as usize)
                .copied()
                .collect())
        };
        assert_eq!(taken("VID.mp4", &mut read), Some((2020, 1, 2)));
        let mut read = |_: u64, _: u64| Ok(vec![]);
        assert_eq!(taken("x.png", &mut read), None);
    }
}
