//! A PDF in the preview: pages drawn by the backend with their text and links over them, the
//! outline beside them, or the system's own viewer.

use super::*;

mod info;
use info::*;

mod layers;
use layers::*;

/// Asks the built-in viewer for the pages only: no toolbar or side panes. Edge on Windows does as
/// asked; WebKit (macOS, Linux) ignores it and keeps its own controls.
const PDF_PLAIN: &str = "toolbar=0&navpanes=0";

/// Around and between the pages drawn, in CSS pixels.
const PDF_PAD: f64 = 12.0;
const PDF_GAP: f64 = 10.0;
/// Pages are drawn this many device pixels wider at a time, so resizing the preview draws them
/// again only now and then (they're scaled to fit in between).
const PDF_RES_STEP: f64 = 128.0;

/// How the pages are sized: as wide as the stage, a whole page in view, at their printed size
/// (72 dpi points as CSS pixels), or as zoomed. The first three are kept through a resize.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum Fit {
    Width,
    Page,
    Actual,
    Free,
}

/// The zoom limits, for the buttons and the fits alike.
const ZOOM_MIN: f64 = 0.25;
const ZOOM_MAX: f64 = 8.0;

/// The zoom that `fit` is (None for `Free`): `room` is the stage's size and `page` the page's in
/// points (unturned), `side` whether it's turned on its side. Zoom 1 is the column as wide as
/// the stage, less the padding. A whole page is never wider than the stage.
fn fit_zoom(fit: Fit, room: (f64, f64), page: [f64; 2], side: bool) -> Option<f64> {
    let col = (room.0 - 2.0 * PDF_PAD).max(50.0);
    let [pw, ph] = page.map(|v| v.max(1.0));
    let (pw, ph) = if side { (ph, pw) } else { (pw, ph) };
    let z = match fit {
        Fit::Free => return None,
        Fit::Width => 1.0,
        Fit::Page => ((room.1 - 2.0 * PDF_PAD).max(50.0) / (col * ph / pw)).min(1.0),
        Fit::Actual => pw / col,
    };
    Some(z.clamp(ZOOM_MIN, ZOOM_MAX))
}

/// Where each PDF was left, most recently read first: localStorage, under this key.
const PLACES_KEY: &str = "ferry-files.pdf-places";
const PLACES_MAX: usize = 200;

/// Where a PDF was left: `key` names the file as it was (`place_key`), the rest is how it was
/// shown: the `anchor`, the zoom and fit, and the turns.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Place {
    key: String,
    page: usize,
    down: f64,
    across: f64,
    zoom: f64,
    fit: Fit,
    turns: u8,
}

/// A PDF's key among the places: where it is, its size and when it changed, so a PDF changed
/// since starts at the top again.
fn place_key(src: &str, path: &str, size: u64, mtime: i64, inner: Option<&str>) -> String {
    match inner {
        Some(i) => format!("{src}\n{path}\n{size}\n{mtime}\n{i}"),
        None => format!("{src}\n{path}\n{size}\n{mtime}"),
    }
}

/// `p` put first among `places`, in place of the PDF's old place, and the least recently read
/// dropped past `PLACES_MAX`.
fn remember(places: &mut Vec<Place>, p: Place) {
    places.retain(|q| q.key != p.key);
    places.insert(0, p);
    places.truncate(PLACES_MAX);
}

fn places() -> Vec<Place> {
    crate::app::stored(PLACES_KEY)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Where the side pane's width is kept.
const SIDE_W_KEY: &str = "ferry-files.pdf-side-w";

/// How often the pages in view are drawn sharp while the reader keeps scrolling.
const SHARPEN_MS: f64 = 300.0;

/// Night: the pages drawn light on dark (their colours turned round, hues kept), for every PDF.
const NIGHT_KEY: &str = "ferry-files.pdf-night";

thread_local! {
    static NIGHT: ArcRwSignal<bool> = ArcRwSignal::new(crate::app::stored_flag(NIGHT_KEY, false));
}

fn night() -> ArcRwSignal<bool> {
    NIGHT.with(Clone::clone)
}

type FitKey = std::rc::Rc<dyn Fn(Fit) -> bool>;

thread_local! {
    /// The PDF in each pane's preview that takes ⌘0, ⌘1 and ⌘2: (pane, viewer, handler).
    static FIT_KEYS: std::cell::RefCell<Vec<(usize, u64, FitKey)>> = const { std::cell::RefCell::new(Vec::new()) };
    static NEXT_VIEWER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// ⌘0, ⌘1 or ⌘2 for the PDF in `pane`'s preview; false when there's none, or it's in the web
/// view's own viewer.
pub(crate) fn pdf_fit_key(pane: usize, fit: Fit) -> bool {
    let f = FIT_KEYS.with(|k| {
        k.borrow()
            .iter()
            .rev()
            .find(|(p, _, _)| *p == pane)
            .map(|(_, _, f)| f.clone())
    });
    f.is_some_and(|f| f(fit))
}

/// Where a page drawn sits (from its top) and how it's turned: the unturned page's box (width,
/// height) and the CSS transform that puts it in the box the turned page takes up, `w` × `h`.
fn pdf_sheet(t: u8, w: f64, h: f64) -> String {
    let (iw, ih) = if t % 2 == 1 { (h, w) } else { (w, h) };
    let turn = match t {
        1 => format!("translateX({w}px) rotate(90deg)"),
        2 => format!("translate({w}px, {h}px) rotate(180deg)"),
        3 => format!("translateY({h}px) rotate(-90deg)"),
        _ => "none".to_string(),
    };
    format!("width: {iw}px; height: {ih}px; --ih: {ih}px; transform: {turn}")
}

/// A PDF with a bar above it: the outline, find in its text, go to a page, zoom, turn; and its
/// info in the side pane.
///
/// The pages are drawn by PDFium in the backend (`ferry://…/pdfpage`, pixels put straight on a
/// canvas, `pdf`): roughly those near what's scrolled to, then sharp those in view, at the
/// width they're shown at (only the piece in view, zoomed in): zoom makes the pages bigger, not the viewer, and a
/// resized preview draws them again, sharp. Matches found are marked on the pages, and their links
/// go to their page or open in the browser. Without PDFium,
/// the web view's own viewer shows the PDF instead; it's out of the app's reach, so a page is gone
/// to through the address (#page=), zoom scales the whole viewer, and matches aren't marked.
#[component]
pub(super) fn PdfView(
    pane: Pane,
    src: Src,
    entry: Entry,
    url: String,
    #[prop(optional)] inner: Option<Inner>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let frame = NodeRef::<html::Iframe>::new();
    let (path, size, mtime) = (entry.path.clone(), entry.size, entry.mtime);
    let what = StoredValue::new((path.clone(), inner));
    let page_url = StoredValue::new(url.clone());
    let outline = RwSignal::new(None::<Result<PdfOutline, String>>);
    // The pane beside the pages, hidden until asked for, every time: its Outline tab, or its
    // Search tab (`search_tab`) or its Info tab (`info_tab`).
    let show = RwSignal::new(false);
    let search_tab = RwSignal::new(false);
    // The Info tab, instead of the outline (search_tab false).
    let info_tab = RwSignal::new(false);
    let info = RwSignal::new(None::<Result<PdfInfo, String>>);
    // The side pane's width, dragged, for every PDF (0: the layout's own).
    let side_w = stored_px(SIDE_W_KEY);
    let typed = RwSignal::new(String::new());
    let page_input = NodeRef::<html::Input>::new();
    let args = move || {
        let (path, inner) = what.get_value();
        match &inner {
            Some(i) => {
                json!({ "src": src.key(), "path": i.zip_path, "size": size, "mtime": mtime, "inner": i.name, "zip_size": i.zip_size })
            }
            None => json!({ "src": src.key(), "path": path, "size": size, "mtime": mtime }),
        }
    };
    // Each page's size in points, when PDFium draws them: None while asked for, Some(None) when
    // the web view's viewer shows the PDF (no PDFium, or a PDF it couldn't read).
    let sizes = RwSignal::new(None::<Option<Vec<[f64; 2]>>>);
    // Why PDFium couldn't read the PDF, when it couldn't: said, rather than just falling back.
    let unread = RwSignal::new(None::<String>);
    {
        let args = args();
        spawn_local(async move {
            let r = call::<Option<Vec<[f64; 2]>>>("pdf_pages", args).await;
            if let Err(e) = &r {
                let _ = unread.try_set(Some(e.clone()));
            }
            let _ = sizes.try_set(Some(r.ok().flatten().filter(|s| !s.is_empty())));
        });
    }
    let drawn = move || sizes.with(|s| matches!(s, Some(Some(_))));
    let load = move || {
        if outline.with_untracked(|o| matches!(o, Some(Ok(_)))) {
            return;
        }
        outline.set(None);
        let args = args();
        spawn_local(async move {
            let r = call::<PdfOutline>("pdf_outline", args).await;
            let _ = outline.try_set(Some(r));
        });
    };
    let show_outline = move || {
        if show.get_untracked() && !search_tab.get_untracked() && !info_tab.get_untracked() {
            show.set(false);
        } else {
            search_tab.set(false);
            info_tab.set(false);
            show.set(true);
            load();
        }
    };
    let show_info = move || {
        search_tab.set(false);
        info_tab.set(true);
        show.set(true);
        if info.with_untracked(|i| matches!(i, Some(Ok(_)))) {
            return;
        }
        info.set(None);
        let args = args();
        spawn_local(async move {
            let r = call::<PdfInfo>("pdf_info", args).await;
            let _ = info.try_set(Some(r));
        });
    };
    let pages = move || {
        sizes
            .with(|s| s.as_ref().and_then(|s| s.as_ref()).map(Vec::len))
            .or_else(|| outline.with(|o| o.as_ref().and_then(|o| o.as_ref().ok()).map(|o| o.pages)))
    };
    let zoom = RwSignal::new(1.0f64);
    let fit = RwSignal::new(Fit::Width);
    let turns = RwSignal::new(0u8);
    let stage = NodeRef::<html::Div>::new();
    let room = RwSignal::new(None::<(f64, f64)>);
    // The stage scrolled to (top, height shown), for which pages to draw.
    let scrolled = RwSignal::new((0.0f64, 0.0f64));
    // The pages drawn: their width, each one's top and height, and the height of them all.
    let layout = Memo::new(move |_| {
        let Some((w, _)) = room.get() else {
            return (0.0, vec![], 0.0);
        };
        let cw = (w - 2.0 * PDF_PAD).max(50.0) * zoom.get();
        let odd = turns.get() % 2 == 1;
        let mut y = PDF_PAD;
        let mut at = vec![];
        sizes.with(|s| {
            for &[pw, ph] in s
                .as_ref()
                .and_then(|s| s.as_ref())
                .map_or(&[][..], |s| &s[..])
            {
                let (pw, ph) = (pw.max(1.0), ph.max(1.0));
                let h = cw * if odd { pw / ph } else { ph / pw };
                at.push((y, h));
                y += h + PDF_GAP;
            }
        });
        (cw, at, y - PDF_GAP + PDF_PAD)
    });
    // Pages from a screen above what's shown to a screen below it: (first, past the last).
    let near = Memo::new(move |_| {
        let (top, ch) = scrolled.get();
        let (lo, hi) = (top - ch, top + 2.0 * ch);
        layout.with(|(_, at, _)| {
            (
                at.partition_point(|&(y, h)| y + h < lo),
                at.partition_point(|&(y, _)| y <= hi),
            )
        })
    });
    // Pages from two screens above what's shown to two below: those already made stay made.
    let far = Memo::new(move |_| {
        let (top, ch) = scrolled.get();
        let (lo, hi) = (top - 2.0 * ch, top + 3.0 * ch);
        layout.with(|(_, at, _)| {
            (
                at.partition_point(|&(y, h)| y + h < lo),
                at.partition_point(|&(y, _)| y <= hi),
            )
        })
    });
    // The page at the top third of what's shown, from 0.
    let here = Memo::new(move |_| {
        let (top, ch) = scrolled.get();
        layout.with(|(_, at, _)| {
            at.partition_point(|&(y, h)| y + h < top + ch / 3.0)
                .min(at.len().saturating_sub(1))
        })
    });
    Effect::new(move |_| {
        let n = here.get();
        if !drawn() {
            return;
        }
        let typing = page_input
            .get_untracked()
            .is_some_and(|i| document().active_element().is_some_and(|a| a == **i));
        if !typing {
            typed.set((n + 1).to_string());
        }
    });
    let go = move |n: usize| {
        let n = pages().map_or(n, |last| n.min(last)).max(1);
        if drawn() {
            if let (Some(s), Some(y)) = (
                stage.get_untracked(),
                layout.with_untracked(|(_, at, _)| at.get(n - 1).map(|a| a.0)),
            ) {
                s.set_scroll_top((y - PDF_PAD / 2.0).max(0.0) as i32);
            }
        } else if let Some(f) = frame.get_untracked() {
            // Set again even when it's the same, so a second click goes back to the page.
            let _ = f.set_attribute(
                "src",
                &format!("{}#page={n}&{PDF_PLAIN}", page_url.get_value()),
            );
        }
    };
    // Find: the PDF's text is read page by page in the backend, and a match picked goes to its
    // page, where it's marked (only on pages drawn by PDFium).
    let fq = RwSignal::new(Query::default());
    let found = RwSignal::new(None::<Result<Found, String>>);
    let searching = RwSignal::new(false);
    let find_focus = RwSignal::new(0u32);
    let current = RwSignal::new(None::<(String, usize)>);
    // Where the matches are on each page (from 1) asked for, for the last search; and the search
    // they're for, so an answer for an older one is let go.
    let marks = RwSignal::new(std::collections::HashMap::<usize, Vec<Vec<[f64; 4]>>>::new());
    let marks_for = StoredValue::new(0u32);
    // A match picked is scrolled to once its page's marks are in.
    let reveal_mark = StoredValue::new(false);
    let open_find = move || {
        if !drawn() {
            return;
        }
        search_tab.set(true);
        info_tab.set(false);
        show.set(true);
        find_focus.update(|n| *n += 1);
    };
    let close_find = move || {
        search_tab.set(false);
        info_tab.set(false);
        load();
    };
    // A search still running when another starts is let go: only the last one's results show.
    let asked_search = StoredValue::new(0u32);
    let timer = StoredValue::new_local(None::<TimeoutHandle>);
    let run_search = move || {
        let query = fq.get_untracked();
        let n = asked_search.get_value() + 1;
        asked_search.set_value(n);
        current.set(None);
        marks.set(Default::default());
        if query.find.trim().is_empty() {
            searching.set(false);
            found.set(None);
            return;
        }
        searching.set(true);
        let mut args = args();
        args["query"] = json!(query);
        spawn_local(async move {
            let r = call::<Found>("pdf_find", args).await;
            if asked_search.try_get_value() == Some(n) {
                // Before `found`, which asks for the marks of the pages shown.
                marks_for.set_value(n);
                let _ = searching.try_set(false);
                let _ = found.try_set(Some(r));
            }
        });
    };
    let search = move |now: bool| {
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        if now {
            run_search();
        } else {
            timer.set_value(
                set_timeout_with_handle(run_search, std::time::Duration::from_millis(300)).ok(),
            );
        }
    };
    on_cleanup(move || {
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    let has_hits = move |page: usize| {
        found.with(|f| {
            f.as_ref()
                .and_then(|f| f.as_ref().ok())
                .is_some_and(|f| f.hits.iter().any(|h| h.path == page.to_string()))
        })
    };
    // The marks of `page` (from 1), asked for once a search.
    let ask_marks = move |page: usize| {
        if !has_hits(page) || marks.with_untracked(|m| m.contains_key(&page)) {
            return;
        }
        let n = marks_for.get_value();
        marks.update(|m| {
            m.insert(page, vec![]);
        });
        let mut args = args();
        args["page"] = json!(page);
        args["query"] = json!(fq.get_untracked());
        spawn_local(async move {
            let r = call::<Vec<Vec<[f64; 4]>>>("pdf_marks", args)
                .await
                .unwrap_or_default();
            if marks_for.try_get_value() == Some(n) {
                marks.update(|m| {
                    m.insert(page, r);
                });
            }
        });
    };
    let pick = move |page: String, k: usize| {
        if let Ok(n) = page.parse::<usize>() {
            current.set(Some((page, k)));
            reveal_mark.set_value(true);
            go(n);
        }
    };
    // The match picked, scrolled to the middle once it's marked.
    Effect::new(move |_| {
        current.track();
        marks.track();
        if !reveal_mark.get_value() {
            return;
        }
        request_animation_frame(move || {
            let Some(el) = stage
                .get_untracked()
                .and_then(|s| s.query_selector(".pdf-mark.here").ok().flatten())
            else {
                return;
            };
            reveal_mark.set_value(false);
            let opts = web_sys::ScrollIntoViewOptions::new();
            opts.set_block(web_sys::ScrollLogicalPosition::Center);
            opts.set_inline(web_sys::ScrollLogicalPosition::Nearest);
            el.scroll_into_view_with_scroll_into_view_options(&opts);
        });
    });
    // ⌘F from the app's window, with the focus in this preview (app.rs).
    let asked_find = StoredValue::new(ctx.read_find.get_untracked().1);
    Effect::new(move |_| {
        let (id, n) = ctx.read_find.get();
        if n != asked_find.get_value() {
            asked_find.set_value(n);
            if id == p.id {
                open_find();
            }
        }
    });
    let on_key = move |ev: ev::KeyboardEvent| {
        // Typed digits aren't the app's shortcuts.
        ev.stop_propagation();
        if ev.key() == "Enter" {
            if let Ok(n) = typed.get_untracked().trim().parse::<usize>() {
                go(n);
            }
        }
    };
    // The width the web view's viewer lays its pages out at: the stage's when first measured.
    // Resizing the preview then scales that layout to the new width rather than resizing the
    // viewer, which keeps its page scale (and leaves a gap or cuts the pages off) and can't be
    // told to refit.
    let base = RwSignal::new(None::<f64>);
    let measure = move || {
        if let Some(s) = stage.get_untracked() {
            let (w, h) = (s.client_width() as f64, s.client_height() as f64);
            if base.with_untracked(Option::is_none) && w >= 50.0 {
                let _ = base.try_set(Some(w));
            }
            let _ = room.try_set(Some((w, h)));
            let _ = scrolled.try_set((s.scroll_top() as f64, h));
        }
    };
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(s) = stage.get() else { return };
        let cb = Closure::<dyn Fn()>::new(measure);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&s);
            if let Some((old, _)) = watcher.try_update_value(|w| w.replace((ro, cb))).flatten() {
                old.disconnect();
            }
        }
    });
    on_cleanup(move || {
        if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
            ro.disconnect();
        }
    });
    // The device pixels a page is drawn across, in steps (`PDF_RES_STEP`), and only once the
    // preview has stopped changing size for a moment: the pages are scaled to fit until then.
    let res = RwSignal::new(0.0f64);
    let res_timer = StoredValue::new_local(None::<TimeoutHandle>);
    Effect::new(move |_| {
        let cw = layout.with(|l| l.0);
        if cw <= 0.0 {
            return;
        }
        let want =
            (cw * window().device_pixel_ratio().max(1.0) / PDF_RES_STEP).ceil() * PDF_RES_STEP;
        if let Some(h) = res_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        if res.get_untracked() == 0.0 {
            res.set(want);
        } else if want != res.get_untracked() {
            res_timer.set_value(
                set_timeout_with_handle(
                    move || {
                        let _ = res.try_set(want);
                    },
                    std::time::Duration::from_millis(200),
                )
                .ok(),
            );
        }
    });
    on_cleanup(move || {
        if let Some(h) = res_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    // Zoomed or turned, the same part of the PDF stays in the middle of what's shown.
    let keep_place = move |change: &dyn Fn()| {
        let Some(s) = stage.get_untracked() else {
            return change();
        };
        let (sh, ch) = (s.scroll_height() as f64, s.client_height() as f64);
        let mid = if sh > 0.0 {
            (s.scroll_top() as f64 + ch / 2.0) / sh
        } else {
            0.0
        };
        change();
        request_animation_frame(move || {
            if let Some(s) = stage.get_untracked() {
                s.set_scroll_top(
                    (mid * s.scroll_height() as f64 - s.client_height() as f64 / 2.0).max(0.0)
                        as i32,
                );
                s.set_scroll_left((s.scroll_width() - s.client_width()).max(0) / 2);
            }
        });
    };
    // Drawn pages keep their place through `anchor` instead.
    let set_zoom = move |z: f64| {
        let z = z.clamp(ZOOM_MIN, ZOOM_MAX);
        fit.set(Fit::Free);
        if drawn() {
            zoom.set(z)
        } else {
            keep_place(&|| zoom.set(z))
        }
    };
    // A fit's zoom, again whenever the stage is resized or the pages turned. Fit page fits the
    // page in view when it's picked or the stage changes, not every page scrolled past.
    Effect::new(move |_| {
        let f = fit.get();
        let Some(room) = room.get() else { return };
        let side = turns.get() % 2 == 1;
        let Some(page) = sizes.with(|s| {
            let s = s.as_ref()?.as_ref()?;
            s.get(here.get_untracked()).or(s.first()).copied()
        }) else {
            return;
        };
        if let Some(z) = fit_zoom(f, room, page, side) {
            if (zoom.get_untracked() - z).abs() > 1e-6 {
                zoom.set(z);
            }
        }
    });
    let pick_fit = move |f: Fit| {
        if !drawn() {
            return false;
        }
        // Set even when it's on, so Fit page picked again fits the page now in view.
        fit.set(f);
        true
    };
    {
        let id = NEXT_VIEWER.with(|n| {
            let id = n.get();
            n.set(id + 1);
            id
        });
        let pane_id = p.id;
        FIT_KEYS.with(|k| {
            k.borrow_mut()
                .push((pane_id, id, std::rc::Rc::new(pick_fit)))
        });
        on_cleanup(move || FIT_KEYS.with(|k| k.borrow_mut().retain(|(_, i, _)| *i != id)));
    }
    let turn = move |by: u8| {
        if drawn() {
            turns.update(|t| *t = (*t + by) % 4)
        } else {
            keep_place(&|| turns.update(|t| *t = (*t + by) % 4))
        }
    };
    // (The box it takes up, the viewer's own size and how it's moved and turned into that box.)
    let placed = move || {
        let Some((w, h)) = room.get() else {
            return (String::new(), "width: 100%; height: 100%".to_string());
        };
        let z = zoom.get();
        let t = turns.get();
        // The viewer fills the stage (its sides swapped when turned on its side) at the width it
        // was first laid out at, scaled to fit, then zoomed: a real zoom, not a wider page column.
        let (own_w, own_h) = if t % 2 == 1 { (h, w) } else { (w, h) };
        let laid_w = base.get().unwrap_or(own_w);
        let fit = if laid_w > 0.0 && own_w > 0.0 {
            own_w / laid_w
        } else {
            1.0
        };
        let (big_w, big_h) = (own_w * z, own_h * z);
        let (box_w, box_h) = if t % 2 == 1 {
            (big_h, big_w)
        } else {
            (big_w, big_h)
        };
        let turn = match t {
            1 => format!("translateX({big_h}px) rotate(90deg) "),
            2 => format!("translate({big_w}px, {big_h}px) rotate(180deg) "),
            3 => format!("translateY({big_w}px) rotate(-90deg) "),
            _ => String::new(),
        };
        let (laid_h, s) = (own_h / fit, z * fit);
        (
            format!("width: {box_w}px; height: {box_h}px"),
            format!("width: {laid_w}px; height: {laid_h}px; transform: {turn}scale({s})"),
        )
    };
    // Where the reader is, kept through zooming, turning and resizing, which move everything
    // else: the page at the middle of what's shown (from 0), how far down it that is (a fraction
    // of its height), and how far across the column (a fraction of its width).
    let anchor = StoredValue::new(None::<(usize, f64, f64)>);
    // While the place is put back: the scrolls that makes aren't the reader's.
    let restoring = StoredValue::new(false);
    let note_anchor = move |s: &web_sys::HtmlElement| {
        let (top, ch) = (s.scroll_top() as f64, s.client_height() as f64);
        let across = (s.scroll_left() as f64 + s.client_width() as f64 / 2.0)
            / (s.scroll_width() as f64).max(1.0);
        let at = layout.with_untracked(|(_, at, _)| {
            let mid = top + ch / 2.0;
            let i = at
                .partition_point(|&(y, h)| y + h < mid)
                .min(at.len().checked_sub(1)?);
            let (y, h) = at[i];
            Some((
                i,
                ((mid - y) / h.max(1.0)).clamp(0.0, 1.0),
                across.clamp(0.0, 1.0),
            ))
        });
        if at.is_some() {
            anchor.set_value(at);
        }
    };
    let put_back = move || {
        let Some((i, down, across)) = anchor.get_value() else {
            return;
        };
        restoring.set_value(true);
        // After the pages are laid out again, so the stage is as tall as they are.
        request_animation_frame(move || {
            if let Some(s) = stage.get_untracked() {
                if let Some((y, h)) = layout
                    .try_with_untracked(|(_, at, _)| at.get(i).copied())
                    .flatten()
                {
                    let ch = s.client_height() as f64;
                    s.set_scroll_top((y + down * h - ch / 2.0).max(0.0).round() as i32);
                    let cw = s.client_width() as f64;
                    s.set_scroll_left(
                        (across * s.scroll_width() as f64 - cw / 2.0)
                            .max(0.0)
                            .round() as i32,
                    );
                    let _ = scrolled.try_set((s.scroll_top() as f64, ch));
                }
            }
            // The scroll events it makes come before the next frame.
            request_animation_frame(move || {
                let _ = restoring.try_set_value(false);
            });
        });
    };
    Effect::new(move |_| {
        layout.track();
        put_back();
    });
    // Where this PDF was left, the last time it was read: put back once its pages are in, before
    // they're drawn sharp; and kept each time the reader stops (`settled`).
    let key = {
        let (path, inner) = what.get_value();
        match &inner {
            Some(i) => place_key(
                &src.key(),
                &i.zip_path,
                size,
                mtime,
                Some(&format!("{}\n{}", i.zip_size, i.name)),
            ),
            None => place_key(&src.key(), &path, size, mtime, None),
        }
    };
    let key = StoredValue::new(key);
    let restored = StoredValue::new(false);
    Effect::new(move |_| {
        if !drawn() || restored.get_value() {
            return;
        }
        restored.set_value(true);
        let Some(p) = key.with_value(|k| places().into_iter().find(|p| &p.key == k)) else {
            return;
        };
        let n = sizes.with_untracked(|s| s.as_ref().and_then(|s| s.as_ref()).map_or(0, Vec::len));
        if p.page >= n {
            return;
        }
        anchor.set_value(Some((
            p.page,
            p.down.clamp(0.0, 1.0),
            p.across.clamp(0.0, 1.0),
        )));
        turns.set(p.turns % 4);
        fit.set(p.fit);
        if p.fit == Fit::Free {
            zoom.set(p.zoom.clamp(ZOOM_MIN, ZOOM_MAX));
        }
        put_back();
    });
    let keep = move || {
        let Some((page, down, across)) = anchor.try_get_value().flatten() else {
            return;
        };
        let Some(p) = key.try_with_value(|key| Place {
            key: key.clone(),
            page,
            down,
            across,
            zoom: zoom.get_untracked(),
            fit: fit.get_untracked(),
            turns: turns.get_untracked(),
        }) else {
            return;
        };
        let mut all = places();
        if all.first() != Some(&p) {
            remember(&mut all, p);
            if let Ok(s) = serde_json::to_string(&all) {
                crate::app::store(PLACES_KEY, &s);
            }
        }
    };
    // Bumped once scrolling, zooming or resizing has stopped for a moment: the pages in view
    // are drawn sharp then, not on every step; and every `SHARPEN_MS` during a long scroll, so
    // the pages coming into view don't stay rough all the way.
    let settled = RwSignal::new(0u32);
    let settle_timer = StoredValue::new_local(None::<TimeoutHandle>);
    let last_settled = StoredValue::new(0.0f64);
    Effect::new(move |_| {
        scrolled.track();
        layout.track();
        res.track();
        if let Some(h) = settle_timer.try_update_value(Option::take).flatten() {
            h.clear();
            let now = js_sys::Date::now();
            if now - last_settled.get_value() >= SHARPEN_MS {
                last_settled.set_value(now);
                settled.update(|n| *n += 1);
            }
        }
        let h = set_timeout_with_handle(
            move || {
                let _ = last_settled.try_set_value(js_sys::Date::now());
                settled.try_update(|n| *n += 1);
                keep();
            },
            std::time::Duration::from_millis(120),
        );
        settle_timer.set_value(h.ok());
    });
    on_cleanup(move || {
        if let Some(h) = settle_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    let drawer = super::pdf::Drawer::new();
    // Each page's text and links (from 1) once read, kept while the PDF is shown.
    let texts = StoredValue::new(HashMap::<usize, std::sync::Arc<PdfLayer>>::new());
    let picture_url = move |n: usize, px: u32, tile: Option<[u32; 4]>| {
        let (path, inner) = what.get_value();
        let at = match &inner {
            Some(i) => format!(
                "p={}&s={size}&m={mtime}&e={}&z={}",
                uri_part(&i.zip_path),
                uri_part(&i.name),
                i.zip_size
            ),
            None => format!("p={}&s={size}&m={mtime}", uri_part(&path)),
        };
        let tile = tile
            .map(|[x, y, w, h]| format!("&t={x},{y},{w},{h}"))
            .unwrap_or_default();
        crate::os::served(&format!(
            "pdfpage?src={}&{at}&n={n}&w={px}{tile}",
            src.key()
        ))
    };
    // Page `i` (from 0): a box of its size, with the page drawn in it when it's near what's shown.
    let page_view = move |i: usize| {
        // Dropped only once it's further off than `near` reaches again (`far`), so a page at the
        // edge isn't made and dropped (and drawn rough again) on every small scroll back and forth.
        let shown = Memo::new(move |was: Option<&bool>| {
            let (a, b) = if was == Some(&true) {
                far.get()
            } else {
                near.get()
            };
            i >= a && i < b
        });
        let place = move || {
            layout.with(|(cw, at, _)| {
                at.get(i)
                    .map(|&(y, h)| {
                        format!("top: {y}px; left: {PDF_PAD}px; width: {cw}px; height: {h}px")
                    })
                    .unwrap_or_default()
            })
        };
        let sheet = move || {
            layout.with(|(cw, at, _)| {
                at.get(i)
                    .map(|&(_, h)| pdf_sheet(turns.get(), *cw, h))
                    .unwrap_or_default()
            })
        };
        // Device pixels across the unturned page: the step's, its sides swapped when it's turned.
        let page = i + 1;
        let aspect = move || {
            let [pw, ph] = sizes.with(|s| {
                s.as_ref()
                    .and_then(|s| s.as_ref())
                    .and_then(|s| s.get(i).copied())
                    .unwrap_or([1.0, 1.0])
            });
            pw.max(1.0) / ph.max(1.0)
        };
        view! {
            <div class="pdf-leaf" style=place>
                {move || shown.get().then(|| {
                    Effect::new(move |_| {
                        found.track();
                        ask_marks(page);
                    });
                    // Its own signal, so another page's text coming in doesn't lay this one's
                    // again (and lose what's selected in it).
                    let runs = RwSignal::new(texts.with_value(|t| t.get(&page).cloned()));
                    if runs.with_untracked(Option::is_none) {
                        let mut args = args();
                        args["page"] = json!(page);
                        spawn_local(async move {
                            let r = std::sync::Arc::new(call::<PdfLayer>("pdf_text", args).await.unwrap_or_default());
                            let _ = texts.try_update_value(|t| t.insert(page, r.clone()));
                            let _ = runs.try_set(Some(r));
                        });
                    }
                    let rough = NodeRef::<html::Canvas>::new();
                    let sharp = NodeRef::<html::Canvas>::new();
                    // Where the sharp drawing is on the page (fractions: left, top, width, height).
                    let sharp_at = RwSignal::new([0.0f64, 0.0, 1.0, 1.0]);
                    // The sharp drawing shown or coming: its pixels across the page, and the part
                    // of the page it covers (pixels: x0, y0, x1, y1).
                    let have = StoredValue::new(None::<(u32, [f64; 4])>);
                    // Which drawing of each kind was asked for last, so one that comes in late is
                    // let go.
                    let asked = StoredValue::new((0u32, 0u32));
                    Effect::new(move |_| {
                        let Some(c) = rough.get() else { return };
                        let [pw, ph] = sizes.with(|s| s.as_ref().and_then(|s| s.as_ref()).and_then(|s| s.get(i).copied()).unwrap_or([1.0, 1.0]));
                        let across = if turns.get() % 2 == 1 { super::pdf::ROUGH_PX * pw.max(1.0) / ph.max(1.0) } else { super::pdf::ROUGH_PX };
                        let Some(n) = asked.try_update_value(|a| { a.0 += 1; a.0 }) else { return };
                        drawer.ask(super::pdf::Draw {
                            url: picture_url(page, across.round() as u32, None),
                            page,
                            sharp: false,
                            done: Box::new(move |got| {
                                if asked.try_get_value().map(|a| a.0) != Some(n) {
                                    return;
                                }
                                if let Err(e) = got.and_then(|img| super::pdf::put_pixels(&c, &img)) {
                                    web_sys::console::warn_2(&format!("Page {page} not drawn:").into(), &e);
                                }
                            }),
                        });
                    });
                    Effect::new(move |_| {
                        settled.track();
                        let Some(c) = sharp.get() else { return };
                        let Some(s) = stage.get_untracked() else { return };
                        let (cw, leaf) = layout.with_untracked(|(cw, at, _)| (*cw, at.get(i).copied()));
                        let Some((ly, lh)) = leaf else { return };
                        let (top, ch) = (s.scroll_top() as f64, s.client_height() as f64);
                        let (left, sw) = (s.scroll_left() as f64, s.client_width() as f64);
                        let lx = ((sw - (cw + 2.0 * PDF_PAD)) / 2.0).max(0.0) + PDF_PAD;
                        // What of the leaf is in view, and a little more, so a small scroll shows
                        // no edge.
                        let view = |m: f64| [
                            (left - m * sw - lx).max(0.0), (top - m * ch - ly).max(0.0),
                            (left + (1.0 + m) * sw - lx).min(cw), (top + (1.0 + m) * ch - ly).min(lh),
                        ];
                        // A sharp drawing is let go only a screen out of view, so one scrolled
                        // just past and back isn't blurred and drawn again; and pages about to
                        // come into view are drawn sharp already.
                        let keep_to = view(1.0);
                        if keep_to[2] <= keep_to[0] || keep_to[3] <= keep_to[1] {
                            drawer.forget(page, true);
                            let _ = asked.try_update_value(|a| a.1 += 1);
                            if have.try_get_value().flatten().is_some() {
                                have.set_value(None);
                                super::pdf::clear(&c);
                            }
                            return;
                        }
                        let seen = view(0.25);
                        if seen[2] <= seen[0] || seen[3] <= seen[1] {
                            drawer.forget(page, true);
                            return;
                        }
                        let t = turns.get_untracked();
                        let (iw, ih) = if t % 2 == 1 { (lh, cw) } else { (cw, lh) };
                        let px = {
                            let r = res.get_untracked();
                            if r <= 0.0 { return }
                            let [pw, ph] = sizes.with_untracked(|s| s.as_ref().and_then(|s| s.as_ref()).and_then(|s| s.get(i).copied()).unwrap_or([1.0, 1.0]));
                            (if t % 2 == 1 { r * pw.max(1.0) / ph.max(1.0) } else { r }).round().max(16.0)
                        };
                        let k = px / iw.max(1.0);
                        let dpr = window().device_pixel_ratio().max(1.0);
                        // Drawn whole unless that's much more than a screenful of pixels.
                        let whole = px * ih * k <= 3.0 * sw * ch * dpr * dpr;
                        let to_px = |b: [f64; 4]| {
                            let [x0, y0, x1, y1] = super::pdf::leaf_to_sheet(t, cw, lh, b);
                            [(x0 * k).floor(), (y0 * k).floor(), (x1 * k).ceil().min(px), (y1 * k).ceil().min((ih * k).round())]
                        };
                        let need = if whole { [0.0, 0.0, px, (ih * k).round()] } else { to_px(seen) };
                        if let Some((hp, hb)) = have.try_get_value().flatten() {
                            if hp == px as u32 && hb[0] <= need[0] && hb[1] <= need[1] && hb[2] >= need[2] && hb[3] >= need[3] {
                                return;
                            }
                        }
                        let get = if whole { need } else { to_px(view(0.5)) };
                        have.set_value(Some((px as u32, get)));
                        let tile = (!whole).then(|| [get[0] as u32, get[1] as u32, (get[2] - get[0]) as u32, (get[3] - get[1]) as u32]);
                        let (fw, fh) = (px, (ih * k).round().max(1.0));
                        let place = [get[0] / fw, get[1] / fh, (get[2] - get[0]) / fw, (get[3] - get[1]) / fh];
                        let Some(n) = asked.try_update_value(|a| { a.1 += 1; a.1 }) else { return };
                        drawer.ask(super::pdf::Draw {
                            url: picture_url(page, px as u32, tile),
                            page,
                            sharp: true,
                            done: Box::new(move |got| {
                                if asked.try_get_value().map(|a| a.1) != Some(n) {
                                    return;
                                }
                                match got.and_then(|img| super::pdf::put_pixels(&c, &img)) {
                                    Ok(()) => {
                                        let _ = sharp_at.try_set(place);
                                    }
                                    Err(e) => {
                                        let _ = have.try_set_value(None);
                                        web_sys::console::warn_2(&format!("Page {page} not drawn sharp:").into(), &e);
                                    }
                                }
                            }),
                        });
                    });
                    on_cleanup(move || drawer.forget(page, false));
                    let sharp_style = move || {
                        let [x, y, w, h] = sharp_at.get();
                        format!("left: {}%; top: {}%; width: {}%; height: {}%", x * 100.0, y * 100.0, w * 100.0, h * 100.0)
                    };
                    view! {
                        <div class="pdf-sheet" style=sheet>
                            <canvas class="rough" node_ref=rough aria-hidden="true"></canvas>
                            <canvas class="sharp" node_ref=sharp style=sharp_style aria-hidden="true"></canvas>
                            {move || marks.with(|m| m.get(&page).cloned()).map(|ms| {
                                ms.into_iter().enumerate().flat_map(|(k, boxes)| boxes.into_iter().map(move |b| (k, b))).map(|(k, b)| {
                                    let at = format!("left: {}%; top: {}%; width: {}%; height: {}%", b[0] * 100.0, b[1] * 100.0, b[2] * 100.0, b[3] * 100.0);
                                    let here = move || current.with(|c| c.as_ref().is_some_and(|(pg, kk)| *pg == page.to_string() && *kk == k));
                                    view! { <span class="pdf-mark" class:here=here style=at></span> }
                                }).collect_view()
                            })}
                            {move || runs.get().map(|r| view! { {pdf_text_layer(&r.runs, aspect())} {pdf_link_layer(&r.links, go)} })}
                        </div>
                    }
                })}
            </div>
        }
    };
    // The page shown (from 1): the one scrolled to, or the one typed in the web view's viewer.
    let page_now = move || {
        if drawn() {
            here.get_untracked() + 1
        } else {
            typed.get_untracked().trim().parse().unwrap_or(1)
        }
    };
    // Right-click on the zoom: the fits. Closed by a click anywhere else.
    let fits_open = RwSignal::new(false);
    let zoom_menu = NodeRef::<html::Span>::new();
    let away = window_event_listener(ev::mousedown, move |e| {
        let inside = zoom_menu
            .get_untracked()
            .zip(e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()))
            .is_some_and(|(m, t)| m.contains(Some(&t)));
        if !inside {
            fits_open.set(false);
        }
    });
    on_cleanup(move || away.remove());
    // Narrow, the bar drops the zoom percentage, then the page box, then the zoom (as the
    // book's bar does).
    let bar = NodeRef::<html::Div>::new();
    fold_bar(bar, 3);
    let heading_list = move || match outline.get() {
        None => view! { <span class="hint">"Loading…"</span> }.into_any(),
        Some(Err(e)) => view! { <span class="hint">{e}</span> }.into_any(),
        Some(Ok(o)) if o.headings.is_empty() => {
            view! { <span class="hint">"This PDF has no outline."</span> }.into_any()
        }
        Some(Ok(o)) => o
            .headings
            .into_iter()
            .map(|h| {
                let indent = format!(
                    "padding-left: {}px",
                    8 + 12 * h.level.saturating_sub(1).min(8)
                );
                let (page, tip) = (h.page, h.title.clone());
                view! {
                    <button class="pdf-head" style=indent title=tip on:click=move |_| go(page)>
                        <span class="name">{h.title}</span>
                        <span class="pg">{page}</span>
                    </button>
                }
            })
            .collect_view()
            .into_any(),
    };
    let on_scroll = move |_| {
        if let Some(s) = stage.get_untracked() {
            scrolled.set((s.scroll_top() as f64, s.client_height() as f64));
            if !restoring.get_value() {
                note_anchor(&s);
            }
        }
    };
    let stage_body = move || {
        match sizes.get().map(|s| s.map(|s| s.len())) {
        None => view! { <span class="hint pdf-wait">"Loading…"</span> }.into_any(),
        Some(Some(n)) => view! {
            <div class="pdf-col" style=move || layout.with(|(cw, _, total)| format!("width: {}px; height: {total}px", cw + 2.0 * PDF_PAD))>
                {(0..n).map(page_view).collect_view()}
            </div>
        }
        .into_any(),
        Some(None) => view! {
            <div class="pdf-box" style=move || placed().0>
                <iframe class="pdf" node_ref=frame src=format!("{}#{PDF_PLAIN}", page_url.get_value()) title="PDF" style=move || placed().1></iframe>
            </div>
        }
        .into_any(),
    }
    };
    view! {
        <div class="viewer-bar pdf-bar pdf-reader" node_ref=bar>
            // Rendered only with PDFium: `hidden` loses to `.btn-group`'s display.
            {move || drawn().then(|| view! {
                <span class="btn-group" role="group">
                    <button class="btn" class:active=move || show.get() && !search_tab.get() && !info_tab.get() title="Outline" aria-label="Outline"
                        on:click=move |_| show_outline()>{ico(Icon::Contents)}</button>
                    <button class="btn" class:active=move || show.get() && search_tab.get() title=crate::os::keys("Find in the PDF (⌘F)") aria-label="Find in the PDF"
                        aria-pressed=move || (show.get() && search_tab.get()).to_string()
                        on:click=move |_| if show.get_untracked() && search_tab.get_untracked() { show.set(false) } else { open_find() }>{ico(Icon::Search)}</button>
                </span>
            })}
            <span class="btn-group pdf-zoom" role="group" node_ref=zoom_menu>
                <button class="btn" title="Zoom out" aria-label="Zoom out" on:click=move |_| set_zoom(zoom.get_untracked() / 1.25)>{ico(Icon::ZoomOut)}</button>
                <button class="btn zoom" class:active=fits_open aria-haspopup="menu" aria-expanded=move || fits_open.get().to_string()
                    title=move || if drawn() { crate::os::keys("Fit the width (⌘1). Right-click for the other fits.").to_string() } else { "Fit the width".to_string() }
                    on:click=move |_| { fits_open.set(false); if !pick_fit(Fit::Width) { set_zoom(1.0) } }
                    on:contextmenu=move |e: web_sys::MouseEvent| if drawn() { e.prevent_default(); fits_open.update(|o| *o = !*o); }>
                    {move || format!("{:.0}%", zoom.get() * 100.0)}</button>
                <button class="btn" title="Zoom in" aria-label="Zoom in" on:click=move |_| set_zoom(zoom.get_untracked() * 1.25)>{ico(Icon::ZoomIn)}</button>
                {move || fits_open.get().then(|| {
                    let item = move |f: Fit, what: &'static str, key: &'static str| view! {
                        <button role="menuitemradio" aria-checked=move || (fit.get() == f).to_string()
                            on:click=move |_| { fits_open.set(false); pick_fit(f); }>
                            <span class="tick" class:on=move || fit.get() == f>{ico(Icon::Check)}</span>
                            <span class="what">{what}</span>
                            <span class="key">{crate::os::keys(key)}</span>
                        </button>
                    };
                    view! {
                        <span class="zoom-menu" role="menu">
                            {item(Fit::Width, "Fit the width", "⌘1")}
                            {item(Fit::Page, "Fit the page", "⌘2")}
                            {item(Fit::Actual, "Actual size", "⌘0")}
                        </span>
                    }
                })}
            </span>
            {move || unread.get().map(|e| view! { <span class="hint pdf-unread" title=e.clone()>{format!("No outline or search: {e}")}</span> })}
            <span class="pdf-fill"></span>
            <input class="pdf-page" node_ref=page_input type="text" inputmode="numeric" placeholder="Page" aria-label="Go to page"
                prop:value=move || typed.get() on:input=move |ev| typed.set(event_target_value(&ev)) on:keydown=on_key/>
            {move || pages().map(|n| view! { <span class="pdf-pages">{format!("of {n}")}</span> })}
            <span class="btn-group" role="group">
                <button class="btn" title="Previous page" aria-label="Previous page" disabled=move || drawn() && here.get() == 0
                    on:click=move |_| go(page_now().saturating_sub(1))>{ico(Icon::Prev)}</button>
                <button class="btn" title="Next page" aria-label="Next page" disabled=move || drawn() && pages().is_some_and(|n| here.get() + 1 >= n)
                    on:click=move |_| go(page_now() + 1)>{ico(Icon::Next)}</button>
            </span>
            {move || drawn().then(|| {
                let on = night();
                let flip = {
                    let on = on.clone();
                    move |_| {
                        let v = !on.get_untracked();
                        on.set(v);
                        crate::app::store(NIGHT_KEY, if v { "1" } else { "0" });
                    }
                };
                let (a, b) = (on.clone(), on);
                view! {
                    <button class="btn" class:active=move || a.get() aria-pressed=move || b.get().to_string()
                        title="Night: light pages on dark" aria-label="Night: light pages on dark" on:click=flip>{ico(Icon::Night)}</button>
                }
            })}
            <span class="btn-group" role="group">
                <button class="btn" title="Turn left" aria-label="Turn left" on:click=move |_| turn(3)>{ico(Icon::TurnLeft)}</button>
                <button class="btn" title="Turn right" aria-label="Turn right" on:click=move |_| turn(1)>{ico(Icon::TurnRight)}</button>
            </span>
        </div>
        <div class="pdf-body">
            {move || (show.get() && drawn()).then(|| view! {
                <nav class="pdf-outline epub-outline" aria-label="Outline and search"
                    style=move || match side_w.get() { w if w > 0.0 => format!("flex-basis: {w}px; max-width: none"), _ => String::new() }>
                    <div class="outline-tabs many" role="tablist">
                        {tab_pick(vec![
                            ("Outline", Signal::derive(move || !search_tab.get() && !info_tab.get()), Callback::new(move |_| close_find())),
                            ("Search", search_tab.into(), Callback::new(move |_| open_find())),
                            ("Info", info_tab.into(), Callback::new(move |_| show_info())),
                        ])}
                        <button role="tab" class:on=move || !search_tab.get() && !info_tab.get() aria-selected=move || (!search_tab.get() && !info_tab.get()).to_string()
                            title="Outline" on:click=move |_| close_find()>{ico(Icon::Contents)}"Outline"</button>
                        <button role="tab" class:on=search_tab aria-selected=move || search_tab.get().to_string()
                            title=crate::os::keys("Search the PDF (⌘F)") on:click=move |_| open_find()>{ico(Icon::Search)}"Search"</button>
                        <button role="tab" class:on=info_tab aria-selected=move || info_tab.get().to_string()
                            title="About the PDF" on:click=move |_| show_info()>{ico(Icon::Info)}"Info"</button>
                    </div>
                    {move || if info_tab.get() {
                        info_list(info).into_any()
                    } else if search_tab.get() {
                        super::reading::find_bar(super::reading::PDF_WORDS, fq, found, searching, find_focus, search, pick, current, close_find).into_any()
                    } else {
                        heading_list().into_any()
                    }}
                </nav>
                <Grip size=side_w key=SIDE_W_KEY.to_string() min=140.0 max=640.0 room=200.0 target=Target::Prev class="grip pdf-grip"/>
            })}
            <div class="pdf-stage" class:drawn=drawn class:night={let n = night(); move || n.get()} node_ref=stage on:scroll=on_scroll>{stage_body}</div>
        </div>
    }
}

#[cfg(test)]
mod tests;
