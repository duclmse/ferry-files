//! Drawing a PDF's pages for the preview (`preview::PdfView`): the pixels PDFium draws
//! (`ferry://…/pdfpage`) put on canvases, a few at a time, rough ones first.
//!
//! Each page near what's shown gets a rough drawing (`ROUGH_PX` across) as soon as it comes
//! near, which is quick to draw and to send, and small. Only the pages in view are then drawn
//! sharp, and when zoomed in, only the piece of a page in view: what's in memory stays about a
//! screen's worth, however far the PDF is zoomed or scrolled.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

/// How many device pixels across a page's rough drawing is.
pub(super) const ROUGH_PX: f64 = 360.0;
/// Drawings asked for at once: more only wait in the backend, and come back too late to be the
/// ones that matter.
const RUNNING_MAX: usize = 2;

/// A drawing to fetch: `done` is given its pixels.
pub(super) struct Draw {
    pub url: String,
    pub page: usize,
    pub sharp: bool,
    pub done: Box<dyn FnOnce(Result<web_sys::ImageData, JsValue>)>,
}

#[derive(Default)]
struct Queue {
    running: usize,
    waiting: Vec<Draw>,
}

/// The drawings waiting and running, for all of a PDF's pages: rough ones go first, then sharp
/// ones in the order asked.
#[derive(Clone, Copy)]
pub(super) struct Drawer(StoredValue<Queue, LocalStorage>);

impl Drawer {
    pub fn new() -> Self {
        Drawer(StoredValue::new_local(Queue::default()))
    }

    /// Asks for a drawing, in place of any waiting for the same page and kind.
    pub fn ask(self, d: Draw) {
        let _ = self.0.try_update_value(|q| {
            q.waiting.retain(|w| (w.page, w.sharp) != (d.page, d.sharp));
            q.waiting.push(d);
        });
        self.pump();
    }

    /// Drops the drawings waiting for `page` (gone from view); the sharp one only, with `sharp`.
    pub fn forget(self, page: usize, sharp_only: bool) {
        let _ = self.0.try_update_value(|q| q.waiting.retain(|w| w.page != page || (sharp_only && !w.sharp)));
    }

    fn pump(self) {
        loop {
            let next = self
                .0
                .try_update_value(|q| {
                    if q.running >= RUNNING_MAX || q.waiting.is_empty() {
                        return None;
                    }
                    let k = q.waiting.iter().position(|w| !w.sharp).unwrap_or(0);
                    q.running += 1;
                    Some(q.waiting.remove(k))
                })
                .flatten();
            let Some(d) = next else { break };
            spawn_local(async move {
                let got = page_pixels(&d.url).await;
                let _ = self.0.try_update_value(|q| q.running -= 1);
                (d.done)(got);
                self.pump();
            });
        }
    }
}

/// Page pixels from `ferry://…/pdfpage`: its width and height (u32, little-endian), then RGBA.
async fn page_pixels(url: &str) -> Result<web_sys::ImageData, JsValue> {
    let r: web_sys::Response = JsFuture::from(window().fetch_with_str(url)).await?.dyn_into()?;
    if !r.ok() {
        let why = JsFuture::from(r.text()?).await.ok().and_then(|t| t.as_string()).unwrap_or_default();
        return Err(format!("{} {why}", r.status()).into());
    }
    let buf: js_sys::ArrayBuffer = JsFuture::from(r.array_buffer()?).await?.dyn_into()?;
    if buf.byte_length() < 8 {
        return Err("No page drawn.".into());
    }
    let head = js_sys::DataView::new(&buf, 0, 8);
    let (w, h) = (head.get_uint32_endian(0, true), head.get_uint32_endian(4, true));
    let len = w as u64 * h as u64 * 4;
    if w == 0 || h == 0 || buf.byte_length() as u64 != 8 + len {
        return Err("The page drawn is the wrong size.".into());
    }
    let px = js_sys::Uint8ClampedArray::new_with_byte_offset_and_length(&buf, 8, len as u32);
    web_sys::ImageData::new_with_js_u8_clamped_array_and_sh(&px, w, h)
}

pub(super) fn put_pixels(canvas: &web_sys::HtmlCanvasElement, img: &web_sys::ImageData) -> Result<(), JsValue> {
    let pen: web_sys::CanvasRenderingContext2d = canvas.get_context("2d")?.ok_or("No canvas.")?.dyn_into()?;
    // Sized only now the pixels are in: resizing clears the canvas, and what was drawn before
    // stays up (stretched) until then.
    canvas.set_width(img.width());
    canvas.set_height(img.height());
    pen.put_image_data(img, 0.0, 0.0)
}

/// Lets a canvas's pixels go now, not when it's collected.
pub(super) fn clear(canvas: &web_sys::HtmlCanvasElement) {
    canvas.set_width(0);
    canvas.set_height(0);
}

/// A box in a page's leaf (the turned page as laid out, `w` × `h`: x0, y0, x1, y1) in the
/// unturned page's own box, as `pdf_sheet` turns it.
pub(super) fn leaf_to_sheet(t: u8, w: f64, h: f64, [x0, y0, x1, y1]: [f64; 4]) -> [f64; 4] {
    let to = |x: f64, y: f64| match t % 4 {
        1 => (y, w - x),
        2 => (w - x, h - y),
        3 => (h - y, x),
        _ => (x, y),
    };
    let ((a, b), (c, d)) = (to(x0, y0), to(x1, y1));
    [a.min(c), b.min(d), a.max(c), b.max(d)]
}

#[cfg(test)]
mod tests {
    use super::leaf_to_sheet;

    #[test]
    fn leaf_boxes_in_the_unturned_page() {
        // A 100 × 200 page; its leaf is 200 × 100 when on its side.
        let top_left_of_leaf = [0.0, 0.0, 10.0, 20.0];
        assert_eq!(leaf_to_sheet(0, 100.0, 200.0, top_left_of_leaf), [0.0, 0.0, 10.0, 20.0]);
        // Turned right, the leaf's top left is the page's bottom left.
        assert_eq!(leaf_to_sheet(1, 200.0, 100.0, top_left_of_leaf), [0.0, 190.0, 20.0, 200.0]);
        assert_eq!(leaf_to_sheet(2, 100.0, 200.0, top_left_of_leaf), [90.0, 180.0, 100.0, 200.0]);
        // Turned left, it's the page's top right.
        assert_eq!(leaf_to_sheet(3, 200.0, 100.0, top_left_of_leaf), [80.0, 0.0, 100.0, 10.0]);
    }
}
