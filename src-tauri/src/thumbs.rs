//! Thumbnails of image files and EPUB covers, for the thumbnail and column views, and photos
//! shrunk for the preview.
//!
//! A JPEG's thumbnail is the small picture the camera put in its EXIF block, when there's a good
//! one: that's in the first few KB, so the rest of the file isn't read. Otherwise the file is read
//! and shrunk to fit SIZE×SIZE. Either way the result is cached on the Mac under a key made of the
//! path, size and modified time, so a changed file gets a new one. A file that can't be shown as a
//! thumbnail is cached as "none" so it isn't read again.
//!
//! Both caches are trimmed (least recently used first) when the app starts.

use crate::device::Device;
use crate::epub::Book;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat, ImageReader};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Longest side of a thumbnail, in pixels (2× the largest size the UI shows).
const SIZE: u32 = 320;
/// Files bigger than this aren't read for a thumbnail.
pub const MAX_SOURCE: u64 = 60 * 1024 * 1024;
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp"];
/// How much of a JPEG is read for the picture in its EXIF block (which is in the first 64 KB,
/// after at most a short JFIF block).
const HEAD: u64 = 96 * 1024;
/// An EXIF picture smaller than this on its longest side is too blurry for a thumbnail.
const MIN_EMBEDDED: u32 = 160;
/// Longest side of a photo shrunk for the preview: about a large preview's on a Retina screen.
pub const FIT: u32 = 2560;
/// How big each cache may grow before the least recently used files are deleted.
const THUMBS_MAX: u64 = 300 << 20;
const PREVIEWS_MAX: u64 = 500 << 20;

fn ext(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

/// Whether a file of this name can have a thumbnail.
pub fn supported(path: &str) -> bool {
    let e = ext(path);
    e == "epub" || IMAGE_EXTS.contains(&e.as_str())
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("ferry-files/thumbs")
}

fn previews_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("ferry-files/previews")
}

/// `src` ("mac" or names the device) is part of the key: the same path can be on several. `px`
/// is the size made, so a change to it makes new ones.
fn key(src: &str, path: &str, size: u64, mtime: i64, px: u32) -> String {
    // "2": made upright since; the ones made before may be on their side.
    format!("{:x}", md5::compute(format!("{src}\n{path}\n{size}\n{mtime}\n{px}\n2")))
}

/// Marks a cached file used, for `trim`: its modified time, at most once a day.
fn touch(path: &Path) {
    let old = std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|d| d > Duration::from_secs(86400)));
    if old {
        if let Ok(f) = std::fs::File::options().append(true).open(path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }
}

/// Deletes the least recently used files of both caches past their limits, down to 80% of them.
pub fn trim_caches() {
    trim(&cache_dir(), THUMBS_MAX);
    trim(&previews_dir(), PREVIEWS_MAX);
}

fn trim(dir: &Path, max: u64) {
    let Ok(list) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = list
        .flatten()
        .filter_map(|f| {
            let m = f.metadata().ok().filter(|m| m.is_file())?;
            Some((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), f.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max {
        return;
    }
    files.sort();
    for (_, len, path) in files {
        if total <= max / 10 * 8 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

pub enum Thumb {
    Image(Vec<u8>, &'static str),
    /// Not an image, too big, or can't be decoded.
    None,
}

/// The cached thumbnail, if there is one.
pub fn cached(src: &str, path: &str, size: u64, mtime: i64) -> Option<Thumb> {
    let base = cache_dir().join(key(src, path, size, mtime, SIZE));
    for (ext, mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        let at = base.with_extension(ext);
        if let Ok(data) = std::fs::read(&at) {
            touch(&at);
            return Some(Thumb::Image(data, mime));
        }
    }
    let none = base.with_extension("none");
    none.exists().then(|| {
        touch(&none);
        Thumb::None
    })
}

fn encode(img: DynamicImage) -> Result<(Vec<u8>, &'static str), String> {
    // Shrunk, never enlarged.
    let img = if img.width() > SIZE || img.height() > SIZE { img.thumbnail(SIZE, SIZE) } else { img };
    to_bytes(img, 82)
}

fn to_bytes(img: DynamicImage, quality: u8) -> Result<(Vec<u8>, &'static str), String> {
    let mut out = Vec::new();
    if img.color().has_alpha() {
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).map_err(|e| e.to_string())?;
        Ok((out, "image/png"))
    } else {
        JpegEncoder::new_with_quality(&mut out, quality).encode_image(&img.to_rgb8()).map_err(|e| e.to_string())?;
        Ok((out, "image/jpeg"))
    }
}

/// The picture inside the file: the image itself (upright), or an EPUB's cover.
fn picture(path: &str, data: &[u8]) -> Option<DynamicImage> {
    if ext(path) == "epub" {
        let book = Book::open(data, path.to_string(), String::new()).ok()?;
        let cover = book.cover_path()?;
        image::load_from_memory(book.file(&cover)?).ok()
    } else {
        let img = image::load_from_memory(data).ok()?;
        Some(upright(img, exif(data).map_or(1, |x| x.orientation)))
    }
}

/// Turned (or flipped) the way EXIF orientation `o` says the camera was held, as the web view
/// shows the file itself.
fn upright(mut img: DynamicImage, o: u8) -> DynamicImage {
    if let Some(o) = Orientation::from_exif(o) {
        img.apply_orientation(o);
    }
    img
}

/// What a JPEG's EXIF block says: where its embedded picture is in the file (start, length), the
/// photo's size in pixels as stored (before turning), and its orientation (1 when upright).
#[derive(Debug, Default, PartialEq)]
struct Exif {
    thumb: Option<(usize, usize)>,
    size: Option<(u32, u32)>,
    orientation: u8,
}

/// The EXIF block of the JPEG starting `data`, if there's one before the image data.
fn exif(data: &[u8]) -> Option<Exif> {
    if data.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    loop {
        if *data.get(at)? != 0xFF {
            return None;
        }
        let marker = *data.get(at + 1)?;
        match marker {
            0xFF => at += 1, // Padding.
            0x01 | 0xD0..=0xD7 => at += 2,
            0xD9 | 0xDA => return None, // The image itself: no EXIF before it.
            _ => {
                let len = u16::from_be_bytes([*data.get(at + 2)?, *data.get(at + 3)?]) as usize;
                if marker == 0xE1 {
                    let body = data.get(at + 4..at + 2 + len)?;
                    if let Some(tiff) = body.strip_prefix(b"Exif\0\0") {
                        return read_tiff(tiff, at + 10);
                    }
                }
                at += 2 + len;
            }
        }
    }
}

/// The EXIF fields `Exif` has, from its TIFF structure `t`, which starts at `base` in the file.
fn read_tiff(t: &[u8], base: usize) -> Option<Exif> {
    let le = match t.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| t.get(o..o + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) });
    let u32_at = |o: usize| {
        t.get(o..o + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
        })
    };
    // A directory's (tag, value) pairs (a SHORT's or LONG's value; offsets for the rest), and
    // where the next directory is (0 for none).
    let dir = |o: usize| -> Option<(Vec<(u16, u32)>, usize)> {
        let n = u16_at(o)? as usize;
        let fields = (0..n.min(512))
            .filter_map(|i| {
                let f = o + 2 + i * 12;
                let v = if u16_at(f + 2)? == 3 { u16_at(f + 8)? as u32 } else { u32_at(f + 8)? };
                Some((u16_at(f)?, v))
            })
            .collect();
        Some((fields, u32_at(o + 2 + n * 12).unwrap_or(0) as usize))
    };
    let get = |fields: &[(u16, u32)], tag: u16| fields.iter().find(|f| f.0 == tag).map(|f| f.1);
    let (ifd0, next) = dir(u32_at(4)? as usize)?;
    let mut x = Exif { orientation: get(&ifd0, 0x0112).map_or(1, |o| o as u8), ..Exif::default() };
    if let Some((sub, _)) = get(&ifd0, 0x8769).and_then(|o| dir(o as usize)) {
        x.size = get(&sub, 0xA002).zip(get(&sub, 0xA003)).filter(|&(w, h)| w > 0 && h > 0);
    }
    if next != 0 {
        if let Some((ifd1, _)) = dir(next) {
            if let Some((o, n)) = get(&ifd1, 0x0201).zip(get(&ifd1, 0x0202)) {
                let (o, n) = (o as usize, n as usize);
                if n > 0 && t.get(o..o + n).is_some() {
                    x.thumb = Some((base + o, n));
                }
            }
        }
    }
    Some(x)
}

/// The picture a camera put in a JPEG's EXIF block, upright, read from the start of the file
/// alone, when it's big enough and the photo's shape (one padded with black bars isn't).
fn embedded(dev: &Device, path: &str, size: u64) -> Option<DynamicImage> {
    if !matches!(ext(path).as_str(), "jpg" | "jpeg") {
        return None;
    }
    let head = dev.read_range(path, 0, size.min(HEAD)).ok()?;
    from_head(&head)
}

fn from_head(head: &[u8]) -> Option<DynamicImage> {
    let x = exif(head)?;
    let (at, n) = x.thumb?;
    let img = image::load_from_memory(head.get(at..at + n)?).ok()?;
    let (w, h) = (img.width(), img.height());
    if w.max(h) < MIN_EMBEDDED || h == 0 {
        return None;
    }
    if let Some((pw, ph)) = x.size {
        if ((w as f64 / h as f64) / (pw as f64 / ph as f64) - 1.0).abs() > 0.03 {
            return None;
        }
    }
    Some(upright(img, x.orientation))
}

/// A photo for the preview: shrunk to fit FIT×FIT and upright, as a JPEG (PNG with transparency),
/// or the file itself when it's no bigger than that and upright already.
pub enum Fitted {
    Smaller(Vec<u8>, &'static str),
    Original(Vec<u8>),
}

/// The photo `path` for the preview (see `Fitted`), cached like thumbnails.
pub fn fitted(src: &str, dev: &Device, path: &str, size: u64, mtime: i64) -> Result<Fitted, String> {
    let dir = previews_dir();
    let base = dir.join(key(src, path, size, mtime, FIT));
    for (ext, mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        let at = base.with_extension(ext);
        if let Ok(data) = std::fs::read(&at) {
            touch(&at);
            return Ok(Fitted::Smaller(data, mime));
        }
    }
    let (data, _) = dev.read(path).map_err(|e| e.to_string())?;
    let orientation = exif(&data).map_or(1, |x| x.orientation);
    // The size is in the header: no need to decode a photo that's small enough.
    let dims = ImageReader::new(Cursor::new(&data)).with_guessed_format().ok().and_then(|r| r.into_dimensions().ok());
    if dims.is_none_or(|(w, h)| w.max(h) <= FIT) && orientation <= 1 {
        return Ok(Fitted::Original(data));
    }
    let Ok(img) = image::load_from_memory(&data) else { return Ok(Fitted::Original(data)) };
    let img = if img.width().max(img.height()) > FIT { img.resize(FIT, FIT, FilterType::Triangle) } else { img };
    let (out, mime) = to_bytes(upright(img, orientation), 85)?;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(base.with_extension(if mime == "image/png" { "png" } else { "jpg" }), &out);
    Ok(Fitted::Smaller(out, mime))
}

/// An EPUB's cover as it is in the book, and its type, for the preview when it's bigger than the
/// thumbnail. `None` when the book has none (or is too big to read for it).
pub fn cover(dev: &Device, path: &str, size: u64) -> Result<Option<(Vec<u8>, String)>, String> {
    if ext(path) != "epub" || size > MAX_SOURCE {
        return Ok(None);
    }
    let (data, _) = dev.read(path).map_err(|e| e.to_string())?;
    let Ok(book) = Book::open(&data, path.to_string(), String::new()) else { return Ok(None) };
    let Some(at) = book.cover_path() else { return Ok(None) };
    Ok(book.file(&at).map(|d| (d.to_vec(), book.media_type(&at))))
}

/// Reads the file (from `dev`, the phone or the Mac), makes the thumbnail and caches it.
/// `size` and `mtime` are what the listing showed; they're only used for the cache key.
pub fn make(src: &str, dev: &Device, path: &str, size: u64, mtime: i64) -> Result<Thumb, String> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let base = dir.join(key(src, path, size, mtime, SIZE));
    let thumb = if !supported(path) || size > MAX_SOURCE {
        None
    } else if let Some(img) = embedded(dev, path, size) {
        Some(encode(img)?)
    } else {
        let (data, _) = dev.read(path).map_err(|e| e.to_string())?;
        picture(path, &data).map(encode).transpose()?
    };
    match thumb {
        Some((data, mime)) => {
            let ext = if mime == "image/png" { "png" } else { "jpg" };
            let _ = std::fs::write(base.with_extension(ext), &data);
            Ok(Thumb::Image(data, mime))
        }
        None => {
            let _ = std::fs::write(base.with_extension("none"), b"");
            Ok(Thumb::None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG's start: an EXIF block (orientation `o`, photo size `size`) holding `thumb`.
    fn exif_jpeg(o: u16, size: (u32, u32), thumb: &[u8]) -> Vec<u8> {
        let mut t: Vec<u8> = b"II*\0".to_vec();
        t.extend(8u32.to_le_bytes());
        let field = |t: &mut Vec<u8>, tag: u16, kind: u16, v: u32| {
            t.extend(tag.to_le_bytes());
            t.extend(kind.to_le_bytes());
            t.extend(1u32.to_le_bytes());
            t.extend(v.to_le_bytes());
        };
        // IFD0 at 8 (30 bytes), the EXIF IFD at 38, IFD1 at 68, the picture at 98.
        t.extend(2u16.to_le_bytes());
        field(&mut t, 0x0112, 3, o as u32);
        field(&mut t, 0x8769, 4, 38);
        t.extend(68u32.to_le_bytes());
        t.extend(2u16.to_le_bytes());
        field(&mut t, 0xA002, 4, size.0);
        field(&mut t, 0xA003, 4, size.1);
        t.extend(0u32.to_le_bytes());
        t.extend(2u16.to_le_bytes());
        field(&mut t, 0x0201, 4, 98);
        field(&mut t, 0x0202, 4, thumb.len() as u32);
        t.extend(0u32.to_le_bytes());
        t.extend(thumb);
        let mut f = vec![0xFF, 0xD8, 0xFF, 0xE1];
        f.extend(((t.len() + 8) as u16).to_be_bytes());
        f.extend(b"Exif\0\0");
        f.extend(t);
        f.extend([0xFF, 0xDA, 0, 2]);
        f
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        to_bytes(DynamicImage::new_rgb8(w, h), 80).unwrap().0
    }

    #[test]
    fn reads_the_exif_picture() {
        let small = jpeg(200, 150);
        let f = exif_jpeg(6, (4000, 3000), &small);
        let x = exif(&f).unwrap();
        assert_eq!((x.orientation, x.size), (6, Some((4000, 3000))));
        assert_eq!(x.thumb, Some((12 + 98, small.len())));
        // Turned a quarter, as the camera was held.
        let img = from_head(&f).unwrap();
        assert_eq!((img.width(), img.height()), (150, 200));
        // Padded to another shape, or too small: not used.
        assert!(from_head(&exif_jpeg(1, (4000, 2000), &small)).is_none());
        assert!(from_head(&exif_jpeg(1, (4000, 3000), &jpeg(120, 90))).is_none());
        // Cut short (the picture isn't all in what was read), or no EXIF at all.
        assert!(from_head(&f[..f.len() - 50]).is_none());
        assert!(exif(&small).is_none_or(|x| x.thumb.is_none()));
    }

    #[test]
    fn shrinks_and_keeps_alpha() {
        let rgb = DynamicImage::new_rgb8(1200, 800);
        let (data, mime) = encode(rgb).unwrap();
        assert_eq!(mime, "image/jpeg");
        let back = image::load_from_memory(&data).unwrap();
        assert_eq!((back.width(), back.height()), (320, 213));
        let (_, mime) = encode(DynamicImage::new_rgba8(10, 10)).unwrap();
        assert_eq!(mime, "image/png");
        assert!(supported("/a/B.JPG") && supported("/a/b.epub") && !supported("/a/b.txt"));
    }
}
