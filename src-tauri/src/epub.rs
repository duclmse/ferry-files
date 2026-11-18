//! An EPUB held in memory: read from a zip, edited, written back to a zip.
//! Every edit splices the original XML text (see xml.rs), so files and parts the user didn't change
//! keep their exact bytes.

use crate::xml::{self, Kind, Tok};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read, Write};

const DC_NS: &str = "http://purl.org/dc/elements/1.1/";

pub type Result<T> = std::result::Result<T, String>;

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
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
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
        self.items.iter().find(|i| i.props.split_whitespace().any(|p| p == "cover-image")).or_else(|| {
            let id = self.elems.iter().find(|e| e.local == "meta" && e.attr("name") == Some("cover"))?.attr("content")?;
            self.item_by_id(id)
        })
    }

    fn dc(&self, local: &str) -> Vec<&Elem> {
        self.elems.iter().filter(|e| e.dc && e.local == local).collect()
    }

    /// Where new metadata children go: after the last one, so the closing tag keeps its indent.
    fn meta_insert_at(&self) -> usize {
        self.elems.last().map_or(self.metadata_close, |e| e.end)
    }

    fn indent_of(&self, pos: usize) -> String {
        let line = &self.text[..pos];
        let start = line.rfind('\n').map_or(0, |k| k + 1);
        let ws: String = line[start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if ws.is_empty() { "    ".into() } else { ws }
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
        if scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
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
const VOID: &[&str] =
    &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// Non-overlapping matches of `needle` in `hay`, as byte ranges.
pub fn find_all(hay: &str, needle: &str, match_case: bool, whole_word: bool) -> Vec<(usize, usize)> {
    let h: Vec<(usize, char)> = hay.char_indices().collect();
    let n: Vec<char> = needle.chars().collect();
    let mut out = vec![];
    if n.is_empty() {
        return out;
    }
    let eq = |a: char, b: char| a == b || (!match_case && a.to_lowercase().eq(b.to_lowercase()));
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i + n.len() <= h.len() {
        if (0..n.len()).all(|k| eq(h[i + k].1, n[k])) {
            let j = i + n.len();
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

fn snippet(hay: &str, (s, e): (usize, usize)) -> Snippet {
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
    Snippet { before: b, text: hay[s..e].to_string(), after: a }
}

impl Book {
    pub fn open(data: &[u8], source: String, version: String) -> Result<Book> {
        let mut zip = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| format!("Not a readable EPUB (zip): {e}"))?;
        let mut files = Vec::with_capacity(zip.len());
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(|e| format!("Can't read the EPUB: {e}"))?;
            if f.is_dir() {
                continue;
            }
            let name = f.name().to_string();
            let mut buf = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut buf).map_err(|e| format!("Can't read {name} in the EPUB: {e}"))?;
            files.push((name, buf));
        }
        let mut book = Book { files, opf: String::new(), source, version, dirty: false, rev: 0 };
        let container = book.text("META-INF/container.xml").map_err(|_| "Not an EPUB: META-INF/container.xml is missing.".to_string())?;
        let toks = xml::tokenize(&container).map_err(|e| format!("META-INF/container.xml: {e}"))?;
        book.opf = toks
            .iter()
            .find(|t| t.is_start() && t.local() == "rootfile")
            .and_then(|t| t.attr("full-path"))
            .ok_or("Not an EPUB: container.xml names no package document.")?
            .to_string();
        if book.file(&book.opf).is_none() {
            return Err(format!("The package document {} is missing.", book.opf));
        }
        book.package()?;
        Ok(book)
    }

    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.iter().find(|(n, _)| n == path).map(|(_, d)| d.as_slice())
    }

    pub fn text(&self, path: &str) -> Result<String> {
        let data = self.file(path).ok_or_else(|| format!("{path} isn't in the book."))?;
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
            .and_then(|p| p.items.iter().find(|i| i.path == path).map(|i| i.media.clone()))
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
                    let close = xml::matching_close(&toks, i).ok_or(format!("{}: <metadata> is never closed.", self.opf))?;
                    p.metadata_close = toks[close].start;
                    // Direct children only.
                    let mut k = i + 1;
                    while k < close {
                        let c = &toks[k];
                        if c.is_start() {
                            let end = xml::matching_close(&toks, k).ok_or(format!("{}: <{}> is never closed.", self.opf, c.name))?;
                            p.elems.push(Elem {
                                name: c.name.clone(),
                                local: c.local().to_string(),
                                dc: false, // set below, once the dc prefix is known
                                start: c.start,
                                end: toks[end].end,
                                inner: if c.kind == Kind::Empty { (c.end, c.end) } else { (c.end, toks[end].start) },
                                empty: c.kind == Kind::Empty,
                                attrs: c.attrs.clone(),
                                text: xml::inner_text(&text, &toks[k + 1..end.max(k + 1)]).trim().to_string(),
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
            return Err(format!("{}: no <metadata> or <manifest> section.", self.opf));
        }
        let prefix = p.dc_prefix.clone();
        for e in &mut p.elems {
            e.dc = e.name.split_once(':').is_some_and(|(a, _)| a == prefix);
        }
        p.text = text;
        Ok(p)
    }

    // ----- Book info -----

    pub fn meta(&self) -> Result<Meta> {
        let p = self.package()?;
        let first = |l: &str| p.dc(l).first().map(|e| e.text.clone()).unwrap_or_default();
        let all = |l: &str| p.dc(l).iter().map(|e| e.text.clone()).collect();
        let ids = p.dc("identifier");
        let identifier = ids.iter().find(|e| e.attr("id") == Some(&p.unique_id)).or(ids.first()).map(|e| e.text.clone());
        Ok(Meta {
            title: first("title"),
            creators: all("creator"),
            language: first("language"),
            publisher: first("publisher"),
            date: first("date"),
            identifier: identifier.unwrap_or_default(),
            subjects: all("subject"),
            description: first("description"),
        })
    }

    pub fn set_meta(&mut self, m: &Meta) -> Result<()> {
        let clean = |v: &[String]| v.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let one = |s: &str| clean(&[s.to_string()]);
        for (v, what) in [(&m.title, "a title"), (&m.language, "a language"), (&m.identifier, "an identifier")] {
            if v.trim().is_empty() {
                return Err(format!("The book needs {what}."));
            }
        }
        let p = self.package()?;
        let mut edits: Vec<(usize, usize, String)> = vec![];
        let mut inserts: BTreeMap<usize, String> = BTreeMap::new();
        let at = p.meta_insert_at();
        let indent = p.indent_of(p.elems.first().map_or(at, |e| e.start));

        let ids = p.dc("identifier");
        let uid = ids.iter().find(|e| e.attr("id") == Some(&p.unique_id)).or(ids.first()).copied();
        let fields: Vec<(&str, Vec<&Elem>, Vec<String>)> = vec![
            ("title", p.dc("title").into_iter().take(1).collect(), one(&m.title)),
            ("creator", p.dc("creator"), clean(&m.creators)),
            ("language", p.dc("language").into_iter().take(1).collect(), one(&m.language)),
            ("publisher", p.dc("publisher").into_iter().take(1).collect(), one(&m.publisher)),
            ("date", p.dc("date").into_iter().take(1).collect(), one(&m.date)),
            ("subject", p.dc("subject"), clean(&m.subjects)),
            ("description", p.dc("description").into_iter().take(1).collect(), one(&m.description)),
            ("identifier", uid.into_iter().collect(), one(&m.identifier)),
        ];

        for (local, old, new) in fields {
            for k in 0..old.len().max(new.len()) {
                match (old.get(k), new.get(k)) {
                    (Some(e), Some(v)) if &e.text == v => {}
                    (Some(e), Some(v)) if e.empty => {
                        let open = p.text[e.start..e.end].trim_end_matches("/>").trim_end();
                        edits.push((e.start, e.end, format!("{open}>{}</{}>", xml::escape_text(v), e.name)));
                    }
                    (Some(e), Some(v)) => edits.push((e.inner.0, e.inner.1, xml::escape_text(v))),
                    (Some(e), None) => {
                        edits.push((line_start(&p.text, e.start), e.end, String::new()));
                        // EPUB 3 <meta refines="#id"> entries describe this element; drop them too.
                        if let Some(id) = e.attr("id") {
                            let target = format!("#{id}");
                            for r in p.elems.iter().filter(|r| r.attr("refines") == Some(target.as_str())) {
                                edits.push((line_start(&p.text, r.start), r.end, String::new()));
                            }
                        }
                    }
                    (None, Some(v)) => {
                        let pos = old.last().map_or(at, |e| e.end);
                        let tag = format!("{}:{local}", p.dc_prefix);
                        inserts.entry(pos).or_default().push_str(&format!("\n{indent}<{tag}>{}</{tag}>", xml::escape_text(v)));
                    }
                    (None, None) => {}
                }
            }
        }
        if edits.is_empty() && inserts.is_empty() {
            return Ok(());
        }
        edits.extend(inserts.into_iter().map(|(pos, s)| (pos, pos, s)));
        dedupe(&mut edits);
        let text = xml::splice(&p.text, edits);
        xml::check(&text).map_err(|e| format!("Updating the book info broke the package document: {e}"))?;
        let opf = self.opf.clone();
        self.set_file(&opf, text.into_bytes());
        Ok(())
    }

    /// Sets <meta property="dcterms:modified"> to now, when the book has one (EPUB 3).
    pub fn touch_modified(&mut self) {
        let Ok(p) = self.package() else { return };
        let Some(e) = p.elems.iter().find(|e| e.local == "meta" && e.attr("property") == Some("dcterms:modified")) else {
            return;
        };
        if e.empty {
            return;
        }
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let text = xml::splice(&p.text, vec![(e.inner.0, e.inner.1, now)]);
        let opf = self.opf.clone();
        self.set_file(&opf, text.into_bytes());
    }

    // ----- Chapters, cover, stats -----

    pub fn chapters(&self) -> Result<Vec<ChapterRef>> {
        let p = self.package()?;
        let labels = self.toc_labels(&p);
        Ok(p.spine
            .iter()
            .filter_map(|id| p.item_by_id(id))
            .filter(|i| i.media.contains("html") && !i.path.is_empty())
            .map(|i| ChapterRef {
                path: i.path.clone(),
                label: labels.get(&i.path).cloned().unwrap_or_else(|| file_name(&i.path).to_string()),
            })
            .collect())
    }

    /// Chapter path → its first title in the table of contents (EPUB 3 nav, else the EPUB 2 NCX).
    fn toc_labels(&self, p: &Package) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Some(nav) = p.items.iter().find(|i| i.props.split_whitespace().any(|x| x == "nav")) {
            if let Some((s, toks)) = self.text(&nav.path).ok().and_then(|s| xml::tokenize(&s).ok().map(|t| (s, t))) {
                let navs: Vec<usize> = (0..toks.len()).filter(|&k| toks[k].kind == Kind::Open && toks[k].local() == "nav").collect();
                let toc = navs
                    .iter()
                    .copied()
                    .find(|&k| toks[k].attr_local("type").is_some_and(|v| v.split_whitespace().any(|x| x == "toc")))
                    .or(navs.first().copied());
                if let Some(start) = toc {
                    let end = xml::matching_close(&toks, start).unwrap_or(toks.len() - 1);
                    for k in start..end {
                        let t = &toks[k];
                        if t.kind == Kind::Open && t.local() == "a" {
                            if let (Some(href), Some(close)) = (t.attr("href"), xml::matching_close(&toks, k)) {
                                let label = squash(&xml::inner_text(&s, &toks[k + 1..close]));
                                if let Some(path) = resolve(&nav.path, href) {
                                    if !label.is_empty() {
                                        out.entry(path).or_insert(label);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
        let ncx = p
            .toc_id
            .as_deref()
            .and_then(|id| p.item_by_id(id))
            .or_else(|| p.items.iter().find(|i| i.media == "application/x-dtbncx+xml"));
        if let Some(ncx) = ncx {
            if let Some((s, toks)) = self.text(&ncx.path).ok().and_then(|s| xml::tokenize(&s).ok().map(|t| (s, t))) {
                let mut label = String::new();
                for (k, t) in toks.iter().enumerate() {
                    if t.kind == Kind::Open && t.local() == "text" {
                        if let Some(close) = xml::matching_close(&toks, k) {
                            label = squash(&xml::inner_text(&s, &toks[k + 1..close]));
                        }
                    } else if t.is_start() && t.local() == "content" {
                        if let Some(path) = t.attr("src").and_then(|src| resolve(&ncx.path, src)) {
                            if !label.is_empty() {
                                out.entry(path).or_insert_with(|| label.clone());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    pub fn cover_path(&self) -> Option<String> {
        let p = self.package().ok()?;
        p.cover().map(|i| i.path.clone()).filter(|path| self.file(path).is_some())
    }

    /// Replaces the cover image, or adds one when the book has none.
    pub fn set_cover(&mut self, data: Vec<u8>, ext: &str) -> Result<()> {
        let media = media_for_ext(ext).ok_or("Use a JPEG, PNG, GIF, WebP or SVG image for the cover.")?;
        let p = self.package()?;
        let mut edits = vec![];
        let path = if let Some(item) = p.cover() {
            if item.media != media {
                let attrs: Vec<(String, String)> = item
                    .tok
                    .attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), if k == "media-type" { media.to_string() } else { v.clone() }))
                    .collect();
                edits.push((item.tok.start, item.tok.end, tag_text(&item.tok.name, &attrs, item.tok.kind == Kind::Empty)));
            }
            item.path.clone()
        } else {
            let ext = ext.to_ascii_lowercase();
            let base = dir_of(&self.opf).to_string();
            let mut n = 0;
            let path = loop {
                let name = if n == 0 { format!("images/cover.{ext}") } else { format!("images/cover-{n}.{ext}") };
                if self.file(&format!("{base}{name}")).is_none() {
                    break format!("{base}{name}");
                }
                n += 1;
            };
            let mut n = 0;
            let id = loop {
                let id = if n == 0 { "cover-image".to_string() } else { format!("cover-image-{n}") };
                if p.item_by_id(&id).is_none() {
                    break id;
                }
                n += 1;
            };
            let props = if p.version.starts_with('3') { r#" properties="cover-image""# } else { "" };
            let last = p.items.last().map_or(p.manifest_close, |i| i.tok.end);
            let indent = p.indent_of(p.items.last().map_or(p.manifest_close, |i| i.tok.start));
            let href = xml::escape_attr(&relative(&self.opf, &path));
            edits.push((last, last, format!(r#"{}{indent}<item id="{id}" href="{href}" media-type="{media}"{props}/>"#, "\n")));
            if !p.elems.iter().any(|e| e.local == "meta" && e.attr("name") == Some("cover")) {
                let at = p.meta_insert_at();
                let indent = p.indent_of(p.elems.first().map_or(at, |e| e.start));
                edits.push((at, at, format!("\n{indent}<meta name=\"cover\" content=\"{id}\"/>")));
            }
            path
        };
        if !edits.is_empty() {
            let text = xml::splice(&p.text, edits);
            xml::check(&text).map_err(|e| format!("Updating the cover broke the package document: {e}"))?;
            let opf = self.opf.clone();
            self.set_file(&opf, text.into_bytes());
        }
        self.set_file(&path, data);
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        let p = self.package()?;
        let count = |f: &dyn Fn(&str) -> bool| p.items.iter().filter(|i| f(&i.media)).count();
        Ok(Stats {
            version: p.version.clone(),
            chapters: self.chapters()?.len(),
            files: self.files.len(),
            bytes: self.files.iter().map(|(_, d)| d.len() as u64).sum(),
            images: count(&|m| m.starts_with("image/")),
            styles: count(&|m| m == "text/css"),
            fonts: count(&|m| m.contains("font") || m.contains("opentype")),
        })
    }

    // ----- Chapter text -----

    fn body_range(toks: &[Tok], path: &str) -> Result<(usize, usize)> {
        let open = toks
            .iter()
            .position(|t| t.kind == Kind::Open && t.local() == "body")
            .ok_or_else(|| format!("{path} has no <body>."))?;
        let close = xml::matching_close(toks, open).ok_or_else(|| format!("{path}: <body> is never closed."))?;
        Ok((toks[open].end, toks[close].start))
    }

    /// The chapter as HTML for the visual editor: self-closed non-void tags expanded, the XML
    /// declaration dropped, and `<base href=base>` added so its images and styles load.
    pub fn visual_html(&self, path: &str, base: &str) -> Result<String> {
        let text = self.text(path)?;
        xml::check(&text)?;
        let toks = xml::tokenize(&text)?;
        Self::body_range(&toks, path)?;
        let mut out = String::with_capacity(text.len() + 200);
        let mut based = false;
        for t in &toks {
            let raw = &text[t.start..t.end];
            match t.kind {
                Kind::Pi if t.start == 0 || text[..t.start].trim().is_empty() => {}
                Kind::Doctype => out.push_str("<!DOCTYPE html>"),
                Kind::Empty if !VOID.contains(&t.local().to_ascii_lowercase().as_str()) => {
                    out.push_str(raw.trim_end_matches("/>").trim_end());
                    out.push_str(&format!("></{}>", t.name));
                }
                Kind::Open if t.local() == "head" && !based => {
                    out.push_str(raw);
                    out.push_str(&format!(r#"<base href="{}">"#, xml::escape_attr(base)));
                    based = true;
                }
                _ => out.push_str(raw),
            }
        }
        if !based {
            out.insert_str(0, &format!(r#"<base href="{}">"#, xml::escape_attr(base)));
        }
        Ok(out)
    }

    pub fn body_inner(&self, path: &str) -> Result<String> {
        let text = self.text(path)?;
        let toks = xml::tokenize(&text)?;
        let (s, e) = Self::body_range(&toks, path)?;
        Ok(text[s..e].to_string())
    }

    /// Replaces the chapter's whole source after checking it's well-formed.
    pub fn set_source(&mut self, path: &str, source: &str) -> Result<()> {
        self.text(path)?;
        xml::check(source)?;
        if self.text(path)? != source {
            self.set_file(path, source.as_bytes().to_vec());
        }
        Ok(())
    }

    /// Replaces what's inside <body>, keeping the head and the <body> tag itself.
    pub fn set_body(&mut self, path: &str, inner: &str) -> Result<()> {
        let text = self.text(path)?;
        let toks = xml::tokenize(&text)?;
        let (s, e) = Self::body_range(&toks, path)?;
        if &text[s..e] == inner {
            return Ok(());
        }
        xml::check(&format!("<body>{inner}</body>")).map_err(|e| format!("The edited chapter isn't valid XHTML: {e}"))?;
        let new = format!("{}{inner}{}", &text[..s], &text[e..]);
        self.set_file(path, new.into_bytes());
        Ok(())
    }

    // ----- Find & replace -----

    /// Ranges of text tokens inside <body>, outside <script> and <style>.
    fn body_texts(text: &str, toks: &[Tok]) -> Vec<(usize, usize)> {
        let mut out = vec![];
        let (mut in_body, mut skip) = (false, 0usize);
        for t in toks {
            let l = t.local();
            match t.kind {
                Kind::Open if l == "body" => in_body = true,
                Kind::Close if l == "body" => in_body = false,
                Kind::Open if in_body && (l == "script" || l == "style") => skip += 1,
                Kind::Close if in_body && (l == "script" || l == "style") => skip = skip.saturating_sub(1),
                Kind::Text if in_body && skip == 0 && !text[t.start..t.end].trim().is_empty() => out.push((t.start, t.end)),
                _ => {}
            }
        }
        out
    }

    pub fn find(&self, q: &Query) -> Result<Found> {
        let mut found = Found::default();
        if q.find.is_empty() {
            return Ok(found);
        }
        for ch in self.chapters()? {
            let Some(toks) = self.text(&ch.path).ok().and_then(|t| xml::tokenize(&t).ok().map(|k| (t, k))) else {
                found.skipped.push(ch.label);
                continue;
            };
            let (text, toks) = toks;
            let mut hit = Hit { path: ch.path.clone(), label: ch.label.clone(), count: 0, snippets: vec![] };
            for (s, e) in Self::body_texts(&text, &toks) {
                let plain = xml::unescape(&text[s..e]);
                for m in find_all(&plain, &q.find, q.match_case, q.whole_word) {
                    hit.count += 1;
                    if hit.snippets.len() < 3 {
                        hit.snippets.push(snippet(&plain, m));
                    }
                }
            }
            if hit.count > 0 {
                found.total += hit.count;
                found.hits.push(hit);
            }
        }
        Ok(found)
    }

    /// Replaces every match in chapter text (never in markup). Returns how many were replaced.
    pub fn replace_all(&mut self, q: &Query) -> Result<usize> {
        if q.find.is_empty() {
            return Ok(0);
        }
        let mut total = 0;
        for ch in self.chapters()? {
            let Ok(text) = self.text(&ch.path) else { continue };
            let Ok(toks) = xml::tokenize(&text) else { continue };
            let mut edits = vec![];
            for (s, e) in Self::body_texts(&text, &toks) {
                let plain = xml::unescape(&text[s..e]);
                let ms = find_all(&plain, &q.find, q.match_case, q.whole_word);
                if ms.is_empty() {
                    continue;
                }
                total += ms.len();
                let mut out = String::with_capacity(plain.len());
                let mut last = 0;
                for (a, b) in ms {
                    out.push_str(&plain[last..a]);
                    out.push_str(&q.replace);
                    last = b;
                }
                out.push_str(&plain[last..]);
                edits.push((s, e, xml::escape_text(&out)));
            }
            if !edits.is_empty() {
                let new = xml::splice(&text, edits);
                self.set_file(&ch.path, new.into_bytes());
            }
        }
        Ok(total)
    }

    // ----- Writing -----

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

/// Something wrong with a book, found by `Book::check`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    /// Readers may refuse the book, or show it wrongly. Otherwise it's untidy but usually works.
    pub serious: bool,
    pub text: String,
}

impl Book {
    /// Every file in the book: (path inside the zip, bytes).
    pub fn files(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files.iter().map(|(n, d)| (n.as_str(), d.as_slice()))
    }

    /// Replaces a file's bytes (the caller keeps its type the same).
    pub fn put_file(&mut self, path: &str, data: Vec<u8>) {
        self.set_file(path, data);
    }

    /// Looks for what makes readers refuse or misshow a book: missing files and metadata, broken
    /// chapters, links to files that aren't there.
    pub fn check(&self) -> Vec<Issue> {
        let mut out = vec![];
        let mut bad = |serious: bool, text: String| out.push(Issue { serious, text });
        let p = match self.package() {
            Ok(p) => p,
            Err(e) => {
                bad(true, e);
                return out;
            }
        };
        for (what, l) in [("title", "title"), ("language", "language"), ("identifier", "identifier")] {
            if p.dc(l).iter().all(|e| e.text.is_empty()) {
                bad(true, format!("The book has no {what}."));
            }
        }
        let mut ids = std::collections::HashSet::new();
        for i in &p.items {
            if !ids.insert(i.id.as_str()) {
                bad(true, format!("Two files in the manifest have the id “{}”.", i.id));
            }
            if i.path.is_empty() || self.file(&i.path).is_none() {
                bad(true, format!("{} is listed but isn't in the book.", if i.path.is_empty() { &i.id } else { &i.path }));
            }
        }
        if p.spine.is_empty() {
            bad(true, "The reading order (spine) is empty.".into());
        }
        for id in &p.spine {
            if p.item_by_id(id).is_none() {
                bad(true, format!("The reading order names “{id}”, which isn't in the manifest."));
            }
        }
        if p.version.starts_with('3') && !p.items.iter().any(|i| i.props.split_whitespace().any(|x| x == "nav")) {
            bad(false, "No table of contents (nav document).".into());
        }
        if p.version.starts_with('2') && p.toc_id.as_deref().and_then(|id| p.item_by_id(id)).is_none() {
            bad(false, "No table of contents (NCX).".into());
        }
        if p.cover().is_none() {
            bad(false, "No cover image is marked.".into());
        }
        let listed: std::collections::HashSet<&str> = p.items.iter().map(|i| i.path.as_str()).collect();
        let unlisted: Vec<&str> = self
            .files
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| *n != "mimetype" && !n.starts_with("META-INF/") && *n != self.opf && !listed.contains(n))
            .collect();
        if !unlisted.is_empty() {
            let more = if unlisted.len() > 3 { format!(" and {} more", unlisted.len() - 3) } else { String::new() };
            bad(false, format!("Files not in the manifest: {}{more}.", unlisted[..unlisted.len().min(3)].join(", ")));
        }
        let mut missing = 0;
        for i in p.items.iter().filter(|i| i.media.contains("html")) {
            let Ok(text) = self.text(&i.path) else { continue };
            if let Err(e) = xml::check(&text) {
                bad(true, format!("{}: {e}", i.path));
                continue;
            }
            let Ok(toks) = xml::tokenize(&text) else { continue };
            for t in toks.iter().filter(|t| t.is_start()) {
                let href = match t.local() {
                    "img" => t.attr("src"),
                    "link" => t.attr("href"),
                    "image" => t.attr_local("href"),
                    "a" => t.attr("href"),
                    _ => None,
                };
                let Some(target) = href.and_then(|h| resolve(&i.path, h)) else { continue };
                if self.file(&target).is_none() {
                    missing += 1;
                    if missing <= 5 {
                        bad(true, format!("{} links to {target}, which isn't in the book.", i.path));
                    }
                }
            }
        }
        if missing > 5 {
            bad(true, format!("…and {} more links to missing files.", missing - 5));
        }
        out
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

fn tag_text(name: &str, attrs: &[(String, String)], empty: bool) -> String {
    let mut s = format!("<{name}");
    for (k, v) in attrs {
        s.push_str(&format!(r#" {k}="{}""#, xml::escape_attr(v)));
    }
    s.push_str(if empty { "/>" } else { ">" });
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPF: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:1234</dc:identifier>
    <dc:title>Old &amp; Tired</dc:title>
    <dc:creator id="c1">Ann</dc:creator>
    <meta refines="#c1" property="role">aut</meta>
    <dc:creator id="c2">Bob</dc:creator>
    <meta refines="#c2" property="role">ill</meta>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2020-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="c1" href="text/ch%201.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="text/ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/pic.png" media-type="image/png"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>
"##;

    const NAV: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Nav</title></head>
<body><nav epub:type="toc"><ol><li><a href="text/ch%201.xhtml">Chapter <b>One</b></a></li><li><a href="text/ch2.xhtml#s">Two</a></li></ol></nav></body></html>"#;

    const CH1: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title/><link rel="stylesheet" href="../style.css"/></head>
<body class="c"><p>The cat &amp; the hat.<a id="p1"/></p><img src="../images/pic.png" alt=""/><style>p{color:cat}</style></body></html>"#;

    const CH2: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>2</title></head><body><p>Concatenate the cat.</p></body></html>"#;

    fn sample() -> Book {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default();
        for (n, d) in [
            ("mimetype", "application/epub+zip"),
            ("META-INF/container.xml", r#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#),
            ("OEBPS/content.opf", OPF),
            ("OEBPS/nav.xhtml", NAV),
            ("OEBPS/text/ch 1.xhtml", CH1),
            ("OEBPS/text/ch2.xhtml", CH2),
            ("OEBPS/images/pic.png", "PNG"),
        ] {
            w.start_file(n, o).unwrap();
            w.write_all(d.as_bytes()).unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        Book::open(&bytes, "/sdcard/b.epub".into(), "v1".into()).unwrap()
    }

    #[test]
    fn checks_change_and_shrink_books() {
        let mut b = sample();
        let issues = b.check();
        let texts: Vec<&str> = issues.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("style.css") && t.contains("isn't in the book")), "{texts:?}");
        assert!(texts.contains(&"No cover image is marked."));
        assert!(!texts.iter().any(|t| t.contains("no title")));
        // A large photo, to be scaled down.
        let img = image::RgbImage::from_fn(2400, 1200, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut jpg = vec![];
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 98).encode_image(&img).unwrap();
        b.put_file("OEBPS/images/photo.jpg", jpg);
        let bytes = b.to_bytes().unwrap();
        assert!(crate::books::check(&bytes, "b.epub").iter().all(|i| !i.text.contains("mimetype")));
        let (small, s) = crate::books::shrink(&bytes, "b.epub", 1200, 75).unwrap();
        assert_eq!(s.images, 1);
        assert!(s.after < s.before / 2, "{s:?}");
        let sb = Book::open(&small, "x".into(), "".into()).unwrap();
        let photo = image::load_from_memory(sb.file("OEBPS/images/photo.jpg").unwrap()).unwrap();
        assert_eq!((photo.width(), photo.height()), (1200, 600));
        assert_eq!(sb.file("OEBPS/images/pic.png").unwrap(), b"PNG", "not an image: left alone");
        // Changing info in bulk: only the given fields.
        let c = crate::books::Change { language: Some("fr".into()), add_subjects: vec!["Poetry".into()], ..Default::default() };
        let changed = crate::books::change(&small, "x", &c).unwrap().unwrap();
        let m = Book::open(&changed, "x".into(), "".into()).unwrap().meta().unwrap();
        assert_eq!((m.language.as_str(), m.subjects.as_slice(), m.creators.len()), ("fr", &["Poetry".to_string()][..], 2));
        assert!(crate::books::change(&changed, "x", &c).unwrap().is_none(), "nothing left to change");
        assert_eq!(crate::books::smaller_name("A.b.epub"), "A.b (smaller).epub");
    }

    #[test]
    fn reads_book() {
        let b = sample();
        let m = b.meta().unwrap();
        assert_eq!(m.title, "Old & Tired");
        assert_eq!(m.creators, ["Ann", "Bob"]);
        assert_eq!(m.identifier, "urn:uuid:1234");
        let ch = b.chapters().unwrap();
        assert_eq!(ch.len(), 2);
        assert_eq!((ch[0].path.as_str(), ch[0].label.as_str()), ("OEBPS/text/ch 1.xhtml", "Chapter One"));
        assert_eq!(ch[1].label, "Two");
        assert_eq!(b.stats().unwrap().images, 1);
        assert_eq!(b.cover_path(), None);
        assert_eq!(resolve("OEBPS/text/a.xhtml", "../images/x.png#f").as_deref(), Some("OEBPS/images/x.png"));
        assert_eq!(resolve("a.xhtml", "https://x.org/"), None);
        assert_eq!(relative("OEBPS/content.opf", "OEBPS/images/c.png"), "images/c.png");
        assert_eq!(relative("OEBPS/text/a.xhtml", "OEBPS/images/c.png"), "../images/c.png");
    }

    #[test]
    fn edits_meta() {
        let mut b = sample();
        let mut m = b.meta().unwrap();
        m.title = "New <Title>".into();
        m.creators = vec!["Ann".into()];
        m.subjects = vec!["Fiction".into(), "Cats".into()];
        m.publisher = "Pub".into();
        b.set_meta(&m).unwrap();
        assert!(b.dirty);
        let opf = b.text("OEBPS/content.opf").unwrap();
        assert!(opf.contains("<dc:title>New &lt;Title&gt;</dc:title>"), "{opf}");
        assert!(!opf.contains("Bob") && !opf.contains("#c2"), "creator and its refines removed: {opf}");
        assert!(opf.contains(r##"<meta refines="#c1" property="role">aut</meta>"##));
        assert!(opf.contains("<dc:subject>Fiction</dc:subject>\n    <dc:subject>Cats</dc:subject>"), "{opf}");
        assert_eq!(b.meta().unwrap(), Meta { creators: vec!["Ann".into()], ..m.clone() });
        // Untouched lines keep their exact text.
        assert!(opf.contains(r#"<item id="c1" href="text/ch%201.xhtml" media-type="application/xhtml+xml"/>"#));
        assert!(b.set_meta(&Meta { title: " ".into(), ..m }).is_err());
        b.touch_modified();
        assert!(!b.text("OEBPS/content.opf").unwrap().contains("2020-01-01"));
    }

    #[test]
    fn finds_and_replaces_text_only() {
        let mut b = sample();
        let q = Query { find: "cat".into(), replace: "dog".into(), match_case: false, whole_word: true };
        let f = b.find(&q).unwrap();
        assert_eq!(f.total, 2, "{f:?}"); // not "Concatenate", not inside <style>
        assert_eq!(f.hits[0].snippets[0].before, "The ");
        assert_eq!(b.replace_all(&q).unwrap(), 2);
        let ch1 = b.text("OEBPS/text/ch 1.xhtml").unwrap();
        assert!(ch1.contains("<p>The dog &amp; the hat.<a id=\"p1\"/></p>"), "{ch1}");
        assert!(ch1.contains("p{color:cat}"));
        assert!(b.text("OEBPS/text/ch2.xhtml").unwrap().contains("Concatenate the dog."));
        let q = Query { find: "THE".into(), replace: String::new(), match_case: true, whole_word: false };
        assert_eq!(b.find(&q).unwrap().total, 0);
    }

    #[test]
    fn visual_and_body() {
        let mut b = sample();
        let path = "OEBPS/text/ch 1.xhtml";
        let html = b.visual_html(path, "yomi://localhost/book/1/OEBPS/text/").unwrap();
        assert!(html.starts_with("\n<!DOCTYPE html>"), "{html}");
        assert!(html.contains(r#"<head><base href="yomi://localhost/book/1/OEBPS/text/"><title></title><link"#), "{html}");
        assert!(html.contains(r#"<a id="p1"></a>"#) && html.contains(r#"alt=""/>"#));
        assert!(b.body_inner(path).unwrap().starts_with("<p>The cat"));
        b.set_body(path, "<p>New</p>").unwrap();
        assert!(b.text(path).unwrap().contains(r#"<body class="c"><p>New</p></body>"#));
        assert!(b.set_body(path, "<p>broken").is_err());
        assert!(b.set_source(path, "<html><body>").is_err());
    }

    #[test]
    fn covers_and_round_trip() {
        let mut b = sample();
        b.set_cover(b"JPG".to_vec(), "jpg").unwrap();
        assert_eq!(b.cover_path().as_deref(), Some("OEBPS/images/cover.jpg"));
        let opf = b.text("OEBPS/content.opf").unwrap();
        assert!(opf.contains(r#"<item id="cover-image" href="images/cover.jpg" media-type="image/jpeg" properties="cover-image"/>"#), "{opf}");
        assert!(opf.contains(r#"<meta name="cover" content="cover-image"/>"#));
        b.set_cover(b"PNG2".to_vec(), "png").unwrap();
        assert_eq!(b.media_type("OEBPS/images/cover.jpg"), "image/png");
        assert_eq!(b.file("OEBPS/images/cover.jpg"), Some(&b"PNG2"[..]));
        let bytes = b.to_bytes().unwrap();
        assert_eq!(&bytes[30..38], b"mimetype");
        let again = Book::open(&bytes, String::new(), String::new()).unwrap();
        assert_eq!(again.meta().unwrap(), b.meta().unwrap());
        assert_eq!(again.file("OEBPS/images/cover.jpg"), Some(&b"PNG2"[..]));
        assert_eq!(again.chapters().unwrap().len(), 2);
    }
}
