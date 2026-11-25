//! PDF pages drawn by PDFium, for the preview's own PDF viewer: each page's size, a page as a
//! picture at the width it's shown at, and the text on a page with where each letter is, for
//! finding text, marking what's found and laying the text over the page to select.
//!
//! PDFium is loaded when first needed, from the app bundle (or `src-tauri/libs/pdfium`, put there
//! by `scripts/fetch-pdfium.sh`, in a debug build). Without it the preview falls back to the web
//! view's own PDF viewer, with no outline or search.

use pdfium_render::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

static PDFIUM: OnceLock<Option<Pdfium>> = OnceLock::new();

/// Where PDFium may be: in the bundle first, then the source tree (debug builds), then the
/// system's library path.
fn candidates() -> Vec<PathBuf> {
    let name = Pdfium::pdfium_platform_library_name();
    let mut out = vec![];
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) {
        #[cfg(target_os = "macos")]
        out.push(dir.join("../Frameworks").join(&name));
        // Linux packages put the bundle's resources in /usr/lib/<product name>.
        #[cfg(all(unix, not(target_os = "macos")))]
        out.push(dir.join("../lib/Ferry Files").join(&name));
        out.push(dir.join(&name));
    }
    #[cfg(debug_assertions)]
    out.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("libs/pdfium").join(&name));
    out.push(PathBuf::from(name));
    out
}

/// PDFium, or None when it isn't there (or won't load). Tried once.
pub fn pdfium() -> Option<&'static Pdfium> {
    PDFIUM.get_or_init(|| candidates().into_iter().find_map(|c| Pdfium::bind_to_library(&c).ok()).map(Pdfium::new)).as_ref()
}

/// Which file a PDF's bytes came from. The size and modification time are in it, so a changed
/// file is read again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Key {
    pub src: String,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub inner: Option<String>,
}

/// A PDF opened by PDFium, which keeps its bytes.
pub type Doc = PdfDocument<'static>;

/// The last PDFs opened, so each page drawn, text read or search doesn't read the file (a
/// phone's is slow to read) and parse it again.
const KEPT: usize = 2;
static DOCS: Mutex<Vec<(Key, Arc<Doc>)>> = Mutex::new(Vec::new());

/// The PDF opened, from the bytes `read` gives unless it's kept. Held locked while reading, so
/// the pages asked for at once open the file once.
pub fn doc(key: Key, read: impl FnOnce() -> Result<Vec<u8>, String>) -> Result<Arc<Doc>, String> {
    let pdfium = pdfium().ok_or_else(|| NO_PDFIUM.to_string())?;
    let mut docs = DOCS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(k) = docs.iter().position(|(k, _)| *k == key) {
        let kept = docs.remove(k);
        let doc = kept.1.clone();
        docs.push(kept);
        return Ok(doc);
    }
    let doc = Arc::new(pdfium.load_pdf_from_byte_vec(read()?, None).map_err(unread)?);
    if docs.len() >= KEPT {
        docs.remove(0);
    }
    docs.push((key, doc.clone()));
    Ok(doc)
}

pub const NO_PDFIUM: &str = "PDFium isn't there to read the PDF.";

fn unread(e: PdfiumError) -> String {
    match e {
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => "This PDF is locked with a password.".to_string(),
        e => format!("Couldn't read the PDF: {e}"),
    }
}

pub fn open<'a>(pdfium: &'a Pdfium, data: &'a [u8]) -> Result<PdfDocument<'a>, String> {
    pdfium.load_pdf_from_byte_slice(data, None).map_err(unread)
}

pub fn page<'a>(doc: &'a PdfDocument, n: usize) -> Result<PdfPage<'a>, String> {
    let i = PdfPageIndex::try_from(n).map_err(|_| "No such page.".to_string())?;
    doc.pages().get(i).map_err(|_| "No such page.".to_string())
}

/// Each page's width and height in points, as it's shown (its own turn applied).
pub fn sizes(doc: &PdfDocument) -> Vec<[f32; 2]> {
    let pages = doc.pages();
    // Without loading each page, which took most of a second for a long PDF.
    pages
        .as_range()
        .map(|i| match pages.page_size(i) {
            Ok(r) => [r.width().value, r.height().value],
            Err(_) => pages.get(i).map_or([612.0, 792.0], |p| [p.width().value, p.height().value]),
        })
        .collect()
}

/// Most pixels in a page drawn whole (about 8000 × 5000): past that, it's drawn smaller.
const PIXELS_MAX: f32 = 40e6;
/// Widest a page is drawn a piece at a time (zoomed in): only the piece is in memory.
const TILED_WIDTH_MAX: u32 = 30_000;

/// Page `n` (from 0) drawn about `width` pixels wide on white, for the preview to put on a
/// canvas as it is: its width and height (u32, little-endian), then its pixels, RGBA. No PNG to
/// make and decode, which took longer than drawing the page.
///
/// With `tile` (left, top, width, height in pixels of the page drawn `width` wide), only that
/// piece of it is drawn, so a page zoomed in isn't drawn whole.
pub fn render(doc: &PdfDocument, n: usize, width: u32, tile: Option<[u32; 4]>) -> Result<Vec<u8>, String> {
    let page = page(doc, n)?;
    let (pw, ph) = (page.width().value.max(1.0), page.height().value.max(1.0));
    let w = match tile {
        None => (width.clamp(16, 10_000) as f32).min((PIXELS_MAX * pw / ph).sqrt()).max(1.0).round() as Pixels,
        Some(_) => width.clamp(16, TILED_WIDTH_MAX) as Pixels,
    };
    // PDFium swaps its BGRA to RGBA as it draws, so the pixels need no converting after.
    let config = PdfRenderConfig::new()
        .set_target_width(w)
        .set_format(PdfBitmapFormat::BGRA)
        .set_reverse_byte_order(true)
        .set_clear_color(PdfColor::WHITE)
        .render_form_data(true)
        .render_annotations(true);
    let failed = |e: PdfiumError| format!("Couldn't draw the page: {e}");
    let bitmap = match tile {
        None => page.render_with_config(&config).map_err(failed)?,
        Some([x, y, tw, th]) => {
            let h = (w as f32 * ph / pw).round() as u32;
            let (x, y) = (x.min(w as u32 - 1), y.min(h.saturating_sub(1)));
            let (tw, th) = (tw.clamp(1, w as u32 - x), th.clamp(1, h.saturating_sub(y).max(1)));
            if tw as f32 * th as f32 > PIXELS_MAX {
                return Err("Too big a piece to draw.".into());
            }
            let mut bitmap = PdfBitmap::empty(tw as Pixels, th as Pixels, PdfBitmapFormat::BGRA).map_err(failed)?;
            // The whole page laid out, moved so the piece is at the corner: PDFium draws only
            // what falls in the bitmap.
            page.render_into_bitmap_with_config(&mut bitmap, &config.set_origin(-(x as Pixels), -(y as Pixels))).map_err(failed)?;
            bitmap
        }
    };
    let (bw, bh) = (bitmap.width() as usize, bitmap.height() as usize);
    let px = bitmap.as_rgba_bytes();
    let row = bw * 4;
    let stride = if bh > 0 { px.len() / bh } else { 0 };
    if stride < row {
        return Err("Couldn't draw the page.".into());
    }
    let mut out = Vec::with_capacity(8 + row * bh);
    out.extend_from_slice(&(bw as u32).to_le_bytes());
    out.extend_from_slice(&(bh as u32).to_le_bytes());
    if stride == row {
        out.extend_from_slice(&px[..row * bh]);
    } else {
        for r in px.chunks(stride).take(bh) {
            out.extend_from_slice(&r[..row]);
        }
    }
    Ok(out)
}

/// A page's text, and for each character in it (by its byte offset), its box on the page as
/// fractions of the page shown: (left, top, width, height). Characters PDFium adds itself
/// (spaces and line ends, "\r\n", between runs of text) have no box; a space in a search matches
/// any run of white space, so a phrase across a line end is still found.
pub struct PageText {
    pub text: String,
    boxes: Vec<(usize, Option<[f32; 4]>)>,
}

/// Points to fractions of the page shown: a fixed size this big, as PDFium maps page points to
/// pixels with the page's own turn.
const UNIT: Pixels = 10_000;

/// A box on the page in points as fractions of the page shown (left, top, width, height), with
/// `config` a `UNIT` square.
fn shown_box(page: &PdfPage, r: &PdfRect, config: &PdfRenderConfig) -> Option<[f32; 4]> {
    let (x0, y0) = page.points_to_pixels(r.left(), r.top(), config).ok()?;
    let (x1, y1) = page.points_to_pixels(r.right(), r.bottom(), config).ok()?;
    let (l, t, rr, b) = (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1));
    let u = UNIT as f32;
    (rr > l && b > t).then(|| [l as f32 / u, t as f32 / u, (rr - l) as f32 / u, (b - t) as f32 / u])
}

/// A link on a page: its box (as `PageText`'s), and where it goes: a page in the PDF (from 1),
/// or a web or mail address (`openable`).
#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Link {
    pub b: [f32; 4],
    pub page: Option<usize>,
    pub uri: Option<String>,
}

/// Addresses a link may open: web and mail ones, not files or apps.
pub fn openable(uri: &str) -> bool {
    let scheme = uri.split_once(':').map_or("", |(s, _)| s);
    ["http", "https", "mailto"].iter().any(|s| s.eq_ignore_ascii_case(scheme))
}

/// The links on a page that go somewhere the preview can take you.
pub fn links(page: &PdfPage) -> Vec<Link> {
    let config = PdfRenderConfig::new().set_fixed_size(UNIT, UNIT);
    page.links()
        .iter()
        .filter_map(|l| {
            let b = l.rect().ok().and_then(|r| shown_box(page, &r, &config))?;
            let to = l.destination().and_then(|d| d.page_index().ok()).or_else(|| {
                let a = l.action()?;
                let d = a.as_local_destination_action()?.destination().ok()?;
                d.page_index().ok()
            });
            let page = to.and_then(|i| usize::try_from(i).ok()).map(|i| i + 1);
            let uri = match page {
                Some(_) => None,
                None => l.action().and_then(|a| a.as_uri_action().and_then(|u| u.uri().ok())).map(|u| u.trim().to_string()).filter(|u| openable(u)),
            };
            (page.is_some() || uri.is_some()).then_some(Link { b, page, uri })
        })
        .collect()
}

pub fn page_text(page: &PdfPage) -> Result<PageText, String> {
    let text = page.text().map_err(|e| format!("Couldn't read the page's text: {e}"))?;
    let config = PdfRenderConfig::new().set_fixed_size(UNIT, UNIT);
    let mut out = PageText { text: String::new(), boxes: vec![] };
    for c in text.chars().iter() {
        let ch = c.unicode_char().unwrap_or('\u{FFFD}');
        let generated = c.is_generated().unwrap_or(false);
        let at = if generated { None } else { c.loose_bounds().ok().and_then(|r| shown_box(page, &r, &config)) };
        out.boxes.push((out.text.len(), at));
        out.text.push(ch);
    }
    Ok(out)
}

/// On the same line: overlapping a half of each other's height.
fn same_line(a: &[f32; 4], b: &[f32; 4]) -> bool {
    (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]) > 0.5 * a[3].min(b[3])
}

fn join(a: &mut [f32; 4], b: &[f32; 4]) {
    let (x0, y0) = (a[0].min(b[0]), a[1].min(b[1]));
    let (x1, y1) = ((a[0] + a[2]).max(b[0] + b[2]), (a[1] + a[3]).max(b[1] + b[3]));
    *a = [x0, y0, x1 - x0, y1 - y0];
}

/// Text that runs along one line, for the preview to lay over the page drawn (transparent, so it
/// can be selected): `t`, stretched across the box `b` (as `PageText`'s), then `e`, what comes
/// between it and the next run when copied ("", a space or a line end).
#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Run {
    pub t: String,
    pub b: [f32; 4],
    pub e: &'static str,
}

impl PageText {
    /// The boxes a match (byte offsets in `text`) covers, joined into one box a line.
    pub fn boxes(&self, (s, e): (usize, usize)) -> Vec<[f32; 4]> {
        let mut out: Vec<[f32; 4]> = vec![];
        let from = self.boxes.partition_point(|(at, _)| *at < s);
        for (_, b) in self.boxes[from..].iter().take_while(|(at, _)| *at < e) {
            let Some(b) = *b else { continue };
            match out.last_mut() {
                Some(l) if same_line(l, &b) => join(l, &b),
                _ => out.push(b),
            }
        }
        out
    }

    /// The text in runs along its lines. A run ends at a line end, or where the next letter
    /// isn't just after it on the same line (another column, or text drawn out of order).
    pub fn runs(&self) -> Vec<Run> {
        fn close(out: &mut Vec<Run>, run: &mut Option<Run>, e: &'static str) {
            if let Some(mut r) = run.take() {
                let kept = r.t.trim_end().len();
                let e = if e.is_empty() && kept < r.t.len() { " " } else { e };
                r.t.truncate(kept);
                r.e = e;
                if !r.t.is_empty() {
                    out.push(r);
                }
            }
        }
        let mut out = vec![];
        let mut run: Option<Run> = None;
        for (k, &(at, b)) in self.boxes.iter().enumerate() {
            let end = self.boxes.get(k + 1).map_or(self.text.len(), |n| n.0);
            let ch = &self.text[at..end];
            match b {
                None if ch.contains(['\r', '\n']) => close(&mut out, &mut run, "\n"),
                None => {
                    if let Some(r) = &mut run {
                        r.t.push(' ');
                    }
                }
                Some(b) => {
                    let goes_on =
                        run.as_ref().is_some_and(|r| same_line(&r.b, &b) && b[0] >= r.b[0] + r.b[2] - 0.5 * b[3] && b[0] - (r.b[0] + r.b[2]) < 2.0 * b[3]);
                    if !goes_on {
                        let next_line = run.as_ref().is_some_and(|r| !same_line(&r.b, &b));
                        close(&mut out, &mut run, if next_line { "\n" } else { " " });
                    }
                    match &mut run {
                        Some(r) => {
                            r.t.push_str(ch);
                            join(&mut r.b, &b);
                        }
                        None => run = Some(Run { t: ch.to_string(), b, e: "" }),
                    }
                }
            }
        }
        close(&mut out, &mut run, "");
        out
    }
}

/// Every page's text, a page at a time (`each(page from 0, text)`). A page whose text can't be
/// read is passed `Err`.
pub fn each_text(doc: &PdfDocument, mut each: impl FnMut(usize, Result<PageText, String>)) {
    for (n, p) in doc.pages().iter().enumerate() {
        each(n, page_text(&p));
    }
}

/// What's laid over page `n` (from 0) drawn: its text in runs, and its links.
#[derive(serde::Serialize, Debug)]
pub struct Layer {
    pub runs: Vec<Run>,
    pub links: Vec<Link>,
}

pub fn layer_of(doc: &PdfDocument, n: usize) -> Result<Layer, String> {
    let p = page(doc, n)?;
    Ok(Layer { runs: page_text(&p)?.runs(), links: links(&p) })
}

/// The text of page `n` (from 0).
pub fn text_of(doc: &PdfDocument, n: usize) -> Result<PageText, String> {
    page_text(&page(doc, n)?)
}
