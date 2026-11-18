//! Thumbnails of image files and EPUB covers, for the thumbnail and column views.
//!
//! The file is read from the phone and shrunk to fit SIZE×SIZE, and the result is cached on the
//! Mac under a key made of the path, size and modified time, so a changed file gets a new one.
//! A file that can't be shown as a thumbnail is cached as "none" so it isn't read again.

use crate::device::Device;
use crate::epub::Book;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use std::path::PathBuf;

/// Longest side of a thumbnail, in pixels (2× the largest size the UI shows).
const SIZE: u32 = 320;
/// Files bigger than this aren't read for a thumbnail.
pub const MAX_SOURCE: u64 = 60 * 1024 * 1024;
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp"];

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
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("yomi-files/thumbs")
}

/// `src` ("device" or "mac") is part of the key: the same path can be on both.
fn key(src: &str, path: &str, size: u64, mtime: i64) -> String {
    format!("{:x}", md5::compute(format!("{src}\n{path}\n{size}\n{mtime}\n{SIZE}")))
}

pub enum Thumb {
    Image(Vec<u8>, &'static str),
    /// Not an image, too big, or can't be decoded.
    None,
}

/// The cached thumbnail, if there is one.
pub fn cached(src: &str, path: &str, size: u64, mtime: i64) -> Option<Thumb> {
    let base = cache_dir().join(key(src, path, size, mtime));
    for (ext, mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        if let Ok(data) = std::fs::read(base.with_extension(ext)) {
            return Some(Thumb::Image(data, mime));
        }
    }
    base.with_extension("none").exists().then_some(Thumb::None)
}

fn encode(img: DynamicImage) -> Result<(Vec<u8>, &'static str), String> {
    let img = img.thumbnail(SIZE, SIZE);
    let mut out = Vec::new();
    if img.color().has_alpha() {
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).map_err(|e| e.to_string())?;
        Ok((out, "image/png"))
    } else {
        JpegEncoder::new_with_quality(&mut out, 82).encode_image(&img.to_rgb8()).map_err(|e| e.to_string())?;
        Ok((out, "image/jpeg"))
    }
}

/// The picture inside the file: the image itself, or an EPUB's cover.
fn picture(path: &str, data: &[u8]) -> Option<DynamicImage> {
    if ext(path) == "epub" {
        let book = Book::open(data, path.to_string(), String::new()).ok()?;
        let cover = book.cover_path()?;
        image::load_from_memory(book.file(&cover)?).ok()
    } else {
        image::load_from_memory(data).ok()
    }
}

/// Reads the file (from `dev`, the phone or the Mac), makes the thumbnail and caches it.
/// `size` and `mtime` are what the listing showed; they're only used for the cache key.
pub fn make(src: &str, dev: &Device, path: &str, size: u64, mtime: i64) -> Result<Thumb, String> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let base = dir.join(key(src, path, size, mtime));
    let thumb = if !supported(path) || size > MAX_SOURCE {
        None
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
