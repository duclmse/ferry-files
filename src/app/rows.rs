//! Long lists render only the items in view and a little either side, with empty space standing
//! in for the rest, so a folder of thousands of files costs about what one of a hundred does.
//! The list, thumbnail and column views use it; their rows (or rows of tiles) are all one height.

use super::*;
use std::cell::RefCell;
use wasm_bindgen::closure::Closure;

/// A long list's layout, read off what's rendered: a row's height (with the gap after it), the
/// items in a row, and how far down the scroller's content the first row starts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Rows {
    pub(super) h: f64,
    pub(super) per: usize,
    pub(super) top: f64,
}

impl Rows {
    fn count(self, len: usize) -> usize {
        len.div_ceil(self.per.max(1))
    }

    /// The space standing in for rows `from..to`.
    pub(super) fn span(self, from: usize, to: usize) -> f64 {
        to.saturating_sub(from) as f64 * self.h
    }
}

thread_local! {
    /// Each pane's list in view (its list, thumbnails, or current column) as last laid out.
    static SHOWN: RefCell<HashMap<usize, Rows>> = RefCell::new(HashMap::new());
}

/// Which items to render, as (first, end), and their layout, kept up to date as the scroller
/// scrolls, resizes, or `len` changes.
#[derive(Clone, Copy)]
pub(super) struct Window {
    pub(super) range: RwSignal<(usize, usize)>,
    pub(super) rows: RwSignal<Rows>,
    scroller: NodeRef<html::Div>,
    len: Signal<usize>,
    measure: fn(&web_sys::Element) -> Option<Rows>,
    pane: Signal<Option<usize>>,
}

impl Window {
    /// `measure` reads the layout once something is rendered; `guess` stands until then. While
    /// `pane` is some pane, this is the list `reveal` scrolls for it.
    pub(super) fn new(
        scroller: NodeRef<html::Div>,
        len: Signal<usize>,
        guess: Rows,
        measure: fn(&web_sys::Element) -> Option<Rows>,
        pane: Signal<Option<usize>>,
    ) -> Self {
        let w = Window {
            range: RwSignal::new((0, len.get_untracked().min(60))),
            rows: RwSignal::new(guess),
            scroller,
            len,
            measure,
            pane,
        };
        // Again once what's newly rendered can be measured.
        Effect::new(move |_| {
            w.len.track();
            w.range.track();
            w.update();
            request_animation_frame(move || w.update());
        });
        let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
        Effect::new(move |_| {
            let Some(el) = scroller.get() else { return };
            let cb = Closure::<dyn Fn()>::new(move || w.update());
            if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
                ro.observe(&el);
                if let Some((old, _)) = watcher.try_update_value(|o| o.replace((ro, cb))).flatten()
                {
                    old.disconnect();
                }
            }
        });
        on_cleanup(move || {
            if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
                ro.disconnect();
            }
            if let Some(id) = pane.try_get_untracked().flatten() {
                SHOWN.with(|s| s.borrow_mut().remove(&id));
            }
        });
        w
    }

    /// Measures again and moves the range to what's in view; for the scroller's scroll event too.
    pub(super) fn update(self) {
        let Some(el) = self.scroller.try_get_untracked().flatten() else {
            return;
        };
        let el: &web_sys::Element = &el;
        let Some(old) = self.rows.try_get_untracked() else {
            return;
        };
        let r = (self.measure)(el).filter(|r| r.h > 0.0).unwrap_or(old);
        if r != old {
            self.rows.set(r);
        }
        if let Some(id) = self.pane.try_get_untracked().flatten() {
            SHOWN.with(|s| s.borrow_mut().insert(id, r));
        }
        let n = self.len.get_untracked();
        let (top, height) = (el.scroll_top() as f64, el.client_height() as f64);
        // Half a screenful either side, so scrolling a little shows rows already there.
        let pad = ((height / r.h / 2.0).ceil() as usize).max(4);
        let first = (((top - r.top) / r.h).floor().max(0.0) as usize).saturating_sub(pad);
        let end = (((top + height - r.top) / r.h).ceil().max(0.0) as usize + pad).min(r.count(n));
        let range = (
            (first * r.per).min(n),
            (end * r.per).min(n).max((first * r.per).min(n)),
        );
        if self.range.get_untracked() != range {
            self.range.set(range);
        }
    }

    /// The rows above and below what's rendered, as heights for the space standing in for them.
    pub(super) fn gaps(self) -> (f64, f64) {
        let (a, b) = self.range.get();
        let r = self.rows.get();
        let rows = r.count(self.len.get());
        let (a, b) = (a / r.per.max(1), b.div_ceil(r.per.max(1)));
        (r.span(0, a), r.span(b, rows))
    }

    /// `items`' slice in the range.
    pub(super) fn slice<T: Clone>(self, items: &[T]) -> Vec<T> {
        let (a, b) = self.range.get();
        items[a.min(items.len())..b.min(items.len())].to_vec()
    }
}

/// Scrolls `scroller`, pane `pane`'s list, so its item `idx` shows, when that item isn't rendered
/// (`scrollIntoView` does it when it is).
pub(super) fn reveal(pane: usize, scroller: &web_sys::Element, idx: usize) {
    let Some(r) = SHOWN.with(|s| s.borrow().get(&pane).copied()) else {
        return;
    };
    let y = r.top + (idx / r.per.max(1)) as f64 * r.h;
    let (top, height) = (
        scroller.scroll_top() as f64,
        scroller.client_height() as f64,
    );
    if y - r.top < top {
        scroller.set_scroll_top((y - r.top) as i32);
    } else if y + r.h > top + height {
        scroller.set_scroll_top((y + r.h - height).ceil() as i32);
    }
}

/// The height of `el`'s first descendant matching `selector`, in px.
fn height_of(el: &web_sys::Element, selector: &str) -> Option<f64> {
    // Not offsetHeight: that's rounded, and rows can be a fraction of a pixel high.
    Some(
        el.query_selector(selector)
            .ok()
            .flatten()?
            .get_bounding_client_rect()
            .height(),
    )
}

/// How far apart the rows of `el`'s descendants matching `selector` are, in px: from the first
/// row to the last over the rows between, so a row a little off (the one after the space at the
/// top loses half a border in a table) doesn't skew it. One row: its height plus `gap`.
fn pitch(el: &web_sys::Element, selector: &str, gap: f64) -> Option<f64> {
    let found = el.query_selector_all(selector).ok()?;
    let mut tops: Vec<f64> = (0..found.length())
        .filter_map(|n| found.item(n)?.dyn_into::<web_sys::Element>().ok())
        .map(|e| e.get_bounding_client_rect().top())
        .collect();
    tops.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    match tops.as_slice() {
        [] => None,
        [_] => Some(height_of(el, selector)? + gap),
        [first, .., last] => Some((last - first) / (tops.len() - 1) as f64),
    }
}

/// A length property of `el` as laid out, in px.
fn px(el: &web_sys::Element, prop: &str) -> f64 {
    window()
        .get_computed_style(el)
        .ok()
        .flatten()
        .and_then(|s| s.get_property_value(prop).ok())
        .and_then(|v| v.trim_end_matches("px").trim().parse().ok())
        .unwrap_or(0.0)
}

/// The list view: rows under the header.
pub(super) fn list_rows(el: &web_sys::Element) -> Option<Rows> {
    Some(Rows {
        h: pitch(el, "tbody tr[data-row]", 0.0)?,
        per: 1,
        top: height_of(el, "thead")?,
    })
}

/// The thumbnail view: rows of tiles in a grid, after the space for the rows above.
pub(super) fn tile_rows(el: &web_sys::Element) -> Option<Rows> {
    let grid = el.query_selector(".thumbs").ok().flatten()?;
    let per = window()
        .get_computed_style(&grid)
        .ok()
        .flatten()
        .and_then(|s| s.get_property_value("grid-template-columns").ok())
        .map_or(1, |v| v.split_whitespace().count().max(1));
    Some(Rows {
        h: pitch(&grid, ".tile", px(&grid, "row-gap"))?,
        per,
        top: px(&grid, "padding-top"),
    })
}

/// A column of the column view.
pub(super) fn column_rows(el: &web_sys::Element) -> Option<Rows> {
    Some(Rows {
        h: pitch(el, ".row", 0.0)?,
        per: 1,
        top: px(el, "padding-top"),
    })
}
