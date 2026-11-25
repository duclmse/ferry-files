//! Work on many books at once: their info for the library, checks, info changes, and shrinking.
//! Each works on one book's bytes; lib.rs reads and writes them through the device.

use crate::books::epub::{Book, Issue, Meta};
use image::codecs::jpeg::JpegEncoder;
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::PathBuf;

/// What the library shows for a book.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub title: String,
    pub creators: Vec<String>,
    pub language: String,
    pub publisher: String,
    pub subjects: Vec<String>,
    /// Set when the book couldn't be read.
    pub problem: Option<String>,
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ferry-files/library")
}

fn key(src: &str, path: &str, size: u64, mtime: i64) -> PathBuf {
    cache_dir().join(format!(
        "{:x}.json",
        md5::compute(format!("{src}\n{path}\n{size}\n{mtime}"))
    ))
}

pub fn cached(src: &str, path: &str, size: u64, mtime: i64) -> Option<Info> {
    serde_json::from_slice(&std::fs::read(key(src, path, size, mtime)).ok()?).ok()
}

pub fn remember(src: &str, path: &str, size: u64, mtime: i64, info: &Info) {
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(data) = serde_json::to_vec(info) {
        let _ = std::fs::write(key(src, path, size, mtime), data);
    }
}

pub fn info(data: &[u8], path: &str) -> Info {
    match Book::open(data, path.to_string(), String::new()).and_then(|b| b.meta()) {
        Ok(m) => Info {
            title: m.title,
            creators: m.creators,
            language: m.language,
            publisher: m.publisher,
            subjects: m.subjects,
            problem: None,
        },
        Err(e) => Info {
            problem: Some(e),
            ..Info::default()
        },
    }
}

/// The zip-level rules a reader expects, which `Book` doesn't see once it's opened: the first file
/// is `mimetype`, stored uncompressed, saying "application/epub+zip".
fn zip_issues(data: &[u8]) -> Vec<Issue> {
    let mut out = vec![];
    let mut zip = match zip::ZipArchive::new(Cursor::new(data)) {
        Ok(z) => z,
        Err(e) => {
            return vec![Issue {
                serious: true,
                text: format!("Not a readable zip: {e}"),
            }]
        }
    };
    let first = zip.by_index(0).ok().map(|mut f| {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut f, &mut s);
        (f.name().to_string(), f.compression(), s)
    });
    match first {
        Some((name, _, _)) if name != "mimetype" => out.push(Issue {
            serious: true,
            text: "The first file in the zip isn't “mimetype”.".into(),
        }),
        Some((_, method, text)) => {
            if method != zip::CompressionMethod::Stored {
                out.push(Issue {
                    serious: true,
                    text: "“mimetype” is compressed; it must be stored as is.".into(),
                });
            }
            if text.trim() != "application/epub+zip" {
                out.push(Issue {
                    serious: true,
                    text: "“mimetype” doesn't say application/epub+zip.".into(),
                });
            }
        }
        None => out.push(Issue {
            serious: true,
            text: "The zip is empty.".into(),
        }),
    }
    out
}

pub fn check(data: &[u8], path: &str) -> Vec<Issue> {
    let mut out = zip_issues(data);
    match Book::open(data, path.to_string(), String::new()) {
        Ok(b) => out.extend(b.check()),
        Err(e) => out.push(Issue {
            serious: true,
            text: e,
        }),
    }
    out
}

/// Fields to change in many books at once. A field left out (None, or no subjects) isn't touched.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Change {
    pub creators: Option<Vec<String>>,
    pub language: Option<String>,
    pub publisher: Option<String>,
    #[serde(default)]
    pub add_subjects: Vec<String>,
}

impl Change {
    fn apply(&self, m: &mut Meta) {
        if let Some(c) = &self.creators {
            m.creators = c.clone();
        }
        if let Some(l) = &self.language {
            m.language = l.clone();
        }
        if let Some(p) = &self.publisher {
            m.publisher = p.clone();
        }
        for s in &self.add_subjects {
            if !m.subjects.iter().any(|x| x.eq_ignore_ascii_case(s)) {
                m.subjects.push(s.clone());
            }
        }
    }
}

/// The book with its info changed, or None when nothing would change.
pub fn change(data: &[u8], path: &str, c: &Change) -> Result<Option<Vec<u8>>, String> {
    let mut b = Book::open(data, path.to_string(), String::new())?;
    let old = b.meta()?;
    let mut m = old.clone();
    c.apply(&mut m);
    if m == old {
        return Ok(None);
    }
    b.set_meta(&m)?;
    b.touch_modified();
    b.to_bytes().map(Some)
}

#[derive(Debug, Clone, Serialize)]
pub struct Shrunk {
    pub before: u64,
    pub after: u64,
    /// Images made smaller.
    pub images: u32,
}

/// Makes the book smaller: JPEG and PNG images larger than `max_side` pixels are scaled down,
/// JPEGs are saved again at `quality`, and everything is compressed. Each image keeps its type
/// and name, and is only replaced when the new one is smaller.
pub fn shrink(
    data: &[u8],
    path: &str,
    max_side: u32,
    quality: u8,
) -> Result<(Vec<u8>, Shrunk), String> {
    let mut b = Book::open(data, path.to_string(), String::new())?;
    let mut changes = vec![];
    for (name, bytes) in b.files() {
        let format = match image::guess_format(bytes) {
            Ok(f @ (ImageFormat::Jpeg | ImageFormat::Png)) => f,
            _ => continue,
        };
        let Ok(img) = image::load_from_memory_with_format(bytes, format) else {
            continue;
        };
        let big = img.width() > max_side || img.height() > max_side;
        if format == ImageFormat::Png && !big {
            continue;
        }
        let img = if big {
            img.resize(max_side, max_side, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let mut out = Vec::new();
        let ok = if format == ImageFormat::Jpeg {
            JpegEncoder::new_with_quality(&mut out, quality)
                .encode_image(&img.to_rgb8())
                .is_ok()
        } else {
            img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
                .is_ok()
        };
        if ok && out.len() < bytes.len() {
            changes.push((name.to_string(), out));
        }
    }
    let images = changes.len() as u32;
    for (name, out) in changes {
        b.put_file(&name, out);
    }
    let bytes = b.to_bytes()?;
    let s = Shrunk {
        before: data.len() as u64,
        after: bytes.len() as u64,
        images,
    };
    Ok((bytes, s))
}

/// "Book.epub" → "Book (smaller).epub".
pub fn smaller_name(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem} (smaller).{ext}"),
        None => format!("{name} (smaller)"),
    }
}
