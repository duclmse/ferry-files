//! An EPUB held in memory: read from a zip, edited, written back to a zip.
//! Every edit splices the original XML text (see xml.rs), so files and parts the user didn't change
//! keep their exact bytes.

use crate::books::xml::{self, Kind, Tok};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read, Write};

const DC_NS: &str = "http://purl.org/dc/elements/1.1/";

pub type Result<T> = std::result::Result<T, String>;

mod chapter;
mod check;
mod meta;
mod part;
mod search;
mod toc;

pub use check::Issue;
pub use part::{Cut, Moved};
pub use toc::{Level, TocEntry};

pub struct Book {
    /// (path inside the zip, bytes), in the zip's order.
    files: Vec<(String, Vec<u8>)>,
    /// Path of the package document (.opf).
    opf: String,
    /// Where the book lives on the device, and the version we opened (for the save conflict check).
    pub source: String,
    pub version: String,
    pub dirty: bool,
    /// Bumped on every change; used to bust caches of resource URLs.
    pub rev: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub title: String,
    pub creators: Vec<String>,
    pub language: String,
    pub publisher: String,
    pub date: String,
    pub identifier: String,
    pub subjects: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChapterRef {
    pub path: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub version: String,
    pub chapters: usize,
    pub files: usize,
    pub bytes: u64,
    pub images: usize,
    pub styles: usize,
    pub fonts: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Query {
    pub find: String,
    #[serde(default)]
    pub replace: String,
    #[serde(default)]
    pub match_case: bool,
    #[serde(default)]
    pub whole_word: bool,
    /// `find` is a regular expression.
    #[serde(default)]
    pub regex: bool,
    /// The replacement takes each match's case: Foo becomes Bar, FOO becomes BAR.
    #[serde(default)]
    pub preserve_case: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snippet {
    pub before: String,
    pub text: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub label: String,
    pub count: usize,
    pub snippets: Vec<Snippet>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Found {
    pub total: usize,
    pub hits: Vec<Hit>,
    /// Chapters that couldn't be searched because their XHTML is broken.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
struct Item {
    id: String,
    path: String,
    media: String,
    props: String,
    tok: Tok,
}

/// An element that's a direct child of <metadata>.
#[derive(Debug, Clone)]
struct Elem {
    name: String,
    local: String,
    dc: bool,
    start: usize,
    end: usize,
    inner: (usize, usize),
    empty: bool,
    attrs: Vec<(String, String)>,
    text: String,
}

impl Elem {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
}

struct Package {
    text: String,
    version: String,
    unique_id: String,
    dc_prefix: String,
    items: Vec<Item>,
    spine: Vec<String>,
    toc_id: Option<String>,
    elems: Vec<Elem>,
    /// Start of </metadata> and of </manifest>.
    metadata_close: usize,
    manifest_close: usize,
}

impl Package {
    fn item_by_id(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    fn cover(&self) -> Option<&Item> {
        self.items
            .iter()
            .find(|i| i.props.split_whitespace().any(|p| p == "cover-image"))
            .or_else(|| {
                let id = self
                    .elems
                    .iter()
                    .find(|e| e.local == "meta" && e.attr("name") == Some("cover"))?
                    .attr("content")?;
                self.item_by_id(id)
            })
    }

    fn dc(&self, local: &str) -> Vec<&Elem> {
        self.elems
            .iter()
            .filter(|e| e.dc && e.local == local)
            .collect()
    }

    /// Where new metadata children go: after the last one, so the closing tag keeps its indent.
    fn meta_insert_at(&self) -> usize {
        self.elems.last().map_or(self.metadata_close, |e| e.end)
    }

    fn indent_of(&self, pos: usize) -> String {
        let line = &self.text[..pos];
        let start = line.rfind('\n').map_or(0, |k| k + 1);
        let ws: String = line[start..]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if ws.is_empty() {
            "    ".into()
        } else {
            ws
        }
    }
}

fn dir_of(p: &str) -> &str {
    p.rfind('/').map_or("", |k| &p[..=k])
}

pub fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Resolves an href found in file `base` to a path inside the zip. None for external or fragment-only links.
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.split('#').next()?.split('?').next()?;
    if href.is_empty() {
        return None;
    }
    if let Some(colon) = href.find(':') {
        let scheme = &href[..colon];
        if scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return None;
        }
    }
    let href = percent_encoding::percent_decode_str(href).decode_utf8_lossy();
    let joined = match href.strip_prefix('/') {
        Some(h) => h.to_string(),
        None => format!("{}{}", dir_of(base), href),
    };
    let mut out: Vec<&str> = vec![];
    for seg in joined.split('/') {
        match seg {
            ".." => {
                out.pop();
            }
            "" | "." => {}
            s => out.push(s),
        }
    }
    Some(out.join("/"))
}

/// The relative href from file `from` to zip path `to`.
fn relative(from: &str, to: &str) -> String {
    let fd: Vec<&str> = dir_of(from).split('/').filter(|s| !s.is_empty()).collect();
    let tp: Vec<&str> = to.split('/').collect();
    let common = fd.iter().zip(&tp).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; fd.len() - common];
    parts.extend(&tp[common..]);
    parts.join("/")
}

/// `href` (as written) with its file name's extension replaced by `tail` (".png", "-1.png");
/// the folder, the name before the extension and any `#fragment` keep their bytes.
fn renamed_href(href: &str, tail: &str) -> String {
    let end = href.find(['#', '?']).unwrap_or(href.len());
    let seg = href[..end].rfind('/').map_or(0, |k| k + 1);
    let dot = href[seg..end].rfind('.').map_or(end, |k| seg + k);
    format!("{}{tail}{}", &href[..dot], &href[end..])
}

/// Where the addresses in the `url(…)`s of CSS text `css` are written (without quotes), offset
/// by `base`.
fn css_urls(css: &str, base: usize) -> Vec<(usize, usize)> {
    let lower = css.to_ascii_lowercase();
    let mut out = vec![];
    let mut from = 0;
    while let Some(k) = lower[from..].find("url(").map(|k| from + k + 4) {
        let Some(close) = css[k..].find(')').map(|c| k + c) else {
            break;
        };
        let inner = &css[k..close];
        let lead = inner.len() - inner.trim_start().len();
        let v = inner.trim();
        let (s, e) = match v.chars().next() {
            Some(q @ ('"' | '\'')) if v.len() >= 2 && v.ends_with(q) => {
                (k + lead + 1, k + lead + v.len() - 1)
            }
            _ => (k + lead, k + lead + v.len()),
        };
        if s < e {
            out.push((base + s, base + e));
        }
        from = close + 1;
    }
    out
}

fn media_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// A guess from the extension, for files the manifest doesn't list.
pub fn media_guess(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    media_for_ext(&ext).unwrap_or(match ext.as_str() {
        "xhtml" | "xht" => "application/xhtml+xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "xml" | "opf" | "ncx" => "application/xml",
        _ => "application/octet-stream",
    })
}

/// Elements that can't have content in HTML. Others written as <x/> must become <x></x>
/// before the chapter is parsed as HTML in the visual editor.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Non-overlapping matches of `needle` in `hay`, as byte ranges. A space in `needle` matches
/// any run of white space (a line break and indent in the XHTML shows as one space).
pub fn find_all(
    hay: &str,
    needle: &str,
    match_case: bool,
    whole_word: bool,
) -> Vec<(usize, usize)> {
    let h: Vec<(usize, char)> = hay.char_indices().collect();
    let mut n: Vec<char> = vec![];
    for c in needle.trim().chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if !(c == ' ' && n.last() == Some(&' ')) {
            n.push(c);
        }
    }
    let mut out = vec![];
    if n.is_empty() {
        return out;
    }
    let eq = |a: char, b: char| a == b || (!match_case && a.to_lowercase().eq(b.to_lowercase()));
    let word = |c: char| c.is_alphanumeric() || c == '_';
    // Where a match starting at `i` ends, if one does.
    let at = |i: usize| -> Option<usize> {
        let mut j = i;
        for &c in &n {
            if c == ' ' {
                let k = j;
                while j < h.len() && h[j].1.is_whitespace() {
                    j += 1;
                }
                if j == k {
                    return None;
                }
            } else if j < h.len() && eq(h[j].1, c) {
                j += 1;
            } else {
                return None;
            }
        }
        Some(j)
    };
    let mut i = 0;
    while i < h.len() {
        if let Some(j) = at(i) {
            let bounded = (i == 0 || !word(h[i - 1].1)) && (j == h.len() || !word(h[j].1));
            if !whole_word || bounded {
                out.push((h[i].0, h.get(j).map_or(hay.len(), |x| x.0)));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn snippet(hay: &str, (s, e): (usize, usize)) -> Snippet {
    let before: Vec<char> = hay[..s].chars().rev().take(48).collect();
    let before: String = before.into_iter().rev().collect();
    let after: String = hay[e..].chars().take(48).collect();
    let mut b = squash(&before);
    if !b.is_empty() && before.ends_with(char::is_whitespace) {
        b.push(' ');
    }
    let mut a = squash(&after);
    if !a.is_empty() && after.starts_with(char::is_whitespace) {
        a.insert(0, ' ');
    }
    Snippet {
        before: b,
        text: hay[s..e].to_string(),
        after: a,
    }
}

impl Book {
    pub fn open(data: &[u8], source: String, version: String) -> Result<Book> {
        let mut zip = zip::ZipArchive::new(Cursor::new(data))
            .map_err(|e| format!("Not a readable EPUB (zip): {e}"))?;
        let mut files = Vec::with_capacity(zip.len());
        for i in 0..zip.len() {
            let mut f = zip
                .by_index(i)
                .map_err(|e| format!("Can't read the EPUB: {e}"))?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().to_string();
            let mut buf = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut buf)
                .map_err(|e| format!("Can't read {name} in the EPUB: {e}"))?;
            files.push((name, buf));
        }
        let mut book = Book {
            files,
            opf: String::new(),
            source,
            version,
            dirty: false,
            rev: 0,
        };
        book.opf = book.find_opf()?;
        if book.file(&book.opf).is_none() {
            return Err(format!("The package document {} is missing.", book.opf));
        }
        book.package()?;
        Ok(book)
    }

    fn find_opf(&self) -> Result<String> {
        let container = self
            .text("META-INF/container.xml")
            .map_err(|_| "Not an EPUB: META-INF/container.xml is missing.".to_string())?;
        let toks = xml::tokenize(&container).map_err(|e| format!("META-INF/container.xml: {e}"))?;
        Ok(toks
            .iter()
            .find(|t| t.is_start() && t.local() == "rootfile")
            .and_then(|t| t.attr("full-path"))
            .ok_or("Not an EPUB: container.xml names no package document.")?
            .to_string())
    }

    /// A book of only what lists its chapters: container.xml, the package document and the
    /// table of contents, each asked of `get`. For `chapters` and `meta`; nothing else is read.
    pub fn contents_only(get: &mut dyn FnMut(&str) -> Result<Vec<u8>>) -> Result<Book> {
        let container = get("META-INF/container.xml")
            .map_err(|_| "Not an EPUB: META-INF/container.xml is missing.".to_string())?;
        let mut book = Book {
            files: vec![("META-INF/container.xml".into(), container)],
            opf: String::new(),
            source: String::new(),
            version: String::new(),
            dirty: false,
            rev: 0,
        };
        book.opf = book.find_opf()?;
        let opf =
            get(&book.opf).map_err(|_| format!("The package document {} is missing.", book.opf))?;
        book.files.push((book.opf.clone(), opf));
        let p = book.package()?;
        let toc = p.items.iter().filter(|i| {
            i.props.split_whitespace().any(|x| x == "nav")
                || i.media == "application/x-dtbncx+xml"
                || p.toc_id.as_deref() == Some(i.id.as_str())
        });
        for path in toc.map(|i| i.path.clone()).collect::<Vec<_>>() {
            if let Ok(data) = get(&path) {
                book.files.push((path, data));
            }
        }
        Ok(book)
    }

    /// What `contents_only` reads, and every chapter's text too (not their pictures or styles):
    /// enough to search the book without reading all of it.
    pub fn with_chapters(get: &mut dyn FnMut(&str) -> Result<Vec<u8>>) -> Result<Book> {
        let mut book = Book::contents_only(get)?;
        for ch in book.chapters()? {
            if book.file(&ch.path).is_none() {
                if let Ok(data) = get(&ch.path) {
                    book.files.push((ch.path, data));
                }
            }
        }
        Ok(book)
    }

    /// A chapter as `visual_html` makes it, from its own bytes alone.
    pub fn chapter_alone(path: &str, data: Vec<u8>, base: &str) -> Result<String> {
        let book = Book {
            files: vec![(path.to_string(), data)],
            opf: String::new(),
            source: String::new(),
            version: String::new(),
            dirty: false,
            rev: 0,
        };
        book.visual_html(path, base)
    }

    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(n, _)| n == path)
            .map(|(_, d)| d.as_slice())
    }

    pub fn text(&self, path: &str) -> Result<String> {
        let data = self
            .file(path)
            .ok_or_else(|| format!("{path} isn't in the book."))?;
        let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
        String::from_utf8(data.to_vec()).map_err(|_| format!("{path} isn't UTF-8 text."))
    }

    fn set_file(&mut self, path: &str, data: Vec<u8>) {
        match self.files.iter_mut().find(|(n, _)| n == path) {
            Some((_, d)) => *d = data,
            None => self.files.push((path.to_string(), data)),
        }
        self.dirty = true;
        self.rev += 1;
    }

    pub fn media_type(&self, path: &str) -> String {
        self.package()
            .ok()
            .and_then(|p| {
                p.items
                    .iter()
                    .find(|i| i.path == path)
                    .map(|i| i.media.clone())
            })
            .unwrap_or_else(|| media_guess(path).to_string())
    }

    fn package(&self) -> Result<Package> {
        let text = self.text(&self.opf)?;
        let toks = xml::tokenize(&text).map_err(|e| format!("{}: {e}", self.opf))?;
        let mut p = Package {
            text: String::new(),
            version: String::new(),
            unique_id: String::new(),
            dc_prefix: "dc".into(),
            items: vec![],
            spine: vec![],
            toc_id: None,
            elems: vec![],
            metadata_close: 0,
            manifest_close: 0,
        };
        let (mut in_manifest, mut in_spine) = (false, false);
        let mut i = 0;
        while i < toks.len() {
            let t = &toks[i];
            for (k, v) in &t.attrs {
                if v == DC_NS {
                    if let Some(prefix) = k.strip_prefix("xmlns:") {
                        p.dc_prefix = prefix.to_string();
                    }
                }
            }
            match (t.kind, t.local()) {
                (Kind::Open | Kind::Empty, "package") => {
                    p.version = t.attr("version").unwrap_or("").to_string();
                    p.unique_id = t.attr("unique-identifier").unwrap_or("").to_string();
                }
                (Kind::Open, "metadata") => {
                    let close = xml::matching_close(&toks, i)
                        .ok_or(format!("{}: <metadata> is never closed.", self.opf))?;
                    p.metadata_close = toks[close].start;
                    // Direct children only.
                    let mut k = i + 1;
                    while k < close {
                        let c = &toks[k];
                        if c.is_start() {
                            let end = xml::matching_close(&toks, k)
                                .ok_or(format!("{}: <{}> is never closed.", self.opf, c.name))?;
                            p.elems.push(Elem {
                                name: c.name.clone(),
                                local: c.local().to_string(),
                                dc: false, // set below, once the dc prefix is known
                                start: c.start,
                                end: toks[end].end,
                                inner: if c.kind == Kind::Empty {
                                    (c.end, c.end)
                                } else {
                                    (c.end, toks[end].start)
                                },
                                empty: c.kind == Kind::Empty,
                                attrs: c.attrs.clone(),
                                text: xml::inner_text(&text, &toks[k + 1..end.max(k + 1)])
                                    .trim()
                                    .to_string(),
                            });
                            k = end + 1;
                        } else {
                            k += 1;
                        }
                    }
                    i = close;
                    continue;
                }
                (Kind::Open, "manifest") => in_manifest = true,
                (Kind::Close, "manifest") => {
                    in_manifest = false;
                    p.manifest_close = t.start;
                }
                (Kind::Open | Kind::Empty, "item") if in_manifest => {
                    let href = t.attr("href").unwrap_or("");
                    p.items.push(Item {
                        id: t.attr("id").unwrap_or("").to_string(),
                        path: resolve(&self.opf, href).unwrap_or_default(),
                        media: t.attr("media-type").unwrap_or("").to_string(),
                        props: t.attr("properties").unwrap_or("").to_string(),
                        tok: t.clone(),
                    });
                }
                (Kind::Open | Kind::Empty, "spine") => {
                    in_spine = t.kind == Kind::Open;
                    p.toc_id = t.attr("toc").map(String::from);
                }
                (Kind::Close, "spine") => in_spine = false,
                (Kind::Open | Kind::Empty, "itemref") if in_spine => {
                    if let Some(id) = t.attr("idref") {
                        p.spine.push(id.to_string());
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if p.metadata_close == 0 || p.manifest_close == 0 {
            return Err(format!(
                "{}: no <metadata> or <manifest> section.",
                self.opf
            ));
        }
        let prefix = p.dc_prefix.clone();
        for e in &mut p.elems {
            e.dc = e.name.split_once(':').is_some_and(|(a, _)| a == prefix);
        }
        p.text = text;
        Ok(p)
    }

    /// The book as an EPUB zip: `mimetype` first and stored, everything else deflated, in the original order.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        use zip::write::SimpleFileOptions;
        use zip::CompressionMethod;
        let err = |e: &dyn std::fmt::Display| format!("Can't build the EPUB: {e}");
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file("mimetype", stored).map_err(|e| err(&e))?;
        w.write_all(b"application/epub+zip").map_err(|e| err(&e))?;
        for (name, data) in &self.files {
            if name == "mimetype" {
                continue;
            }
            w.start_file(name.as_str(), deflated).map_err(|e| err(&e))?;
            w.write_all(data).map_err(|e| err(&e))?;
        }
        Ok(w.finish().map_err(|e| err(&e))?.into_inner())
    }
}

/// Start of the line `pos` is on, when only spaces precede it there, so removing an element
/// also removes its indent and line break.
fn line_start(s: &str, pos: usize) -> usize {
    let b = s.as_bytes();
    let mut k = pos;
    while k > 0 && matches!(b[k - 1], b' ' | b'\t') {
        k -= 1;
    }
    if k > 0 && b[k - 1] == b'\n' {
        k -= 1;
        if k > 0 && b[k - 1] == b'\r' {
            k -= 1;
        }
        k
    } else {
        pos
    }
}

/// Drops removals that sit inside another removal (a refines meta removed twice).
fn dedupe(edits: &mut Vec<(usize, usize, String)>) {
    edits.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut out: Vec<(usize, usize, String)> = vec![];
    for e in edits.drain(..) {
        if let Some(last) = out.last() {
            if e.0 < last.1 && e.1 <= last.1 && e.0 >= last.0 && e.0 != e.1 {
                continue;
            }
        }
        out.push(e);
    }
    *edits = out;
}

#[cfg(test)]
mod tests;
