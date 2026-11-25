//! The preview to the right of the files: pictures (zoom, turn, pan), video and audio
//! (video.rs), text and hex (loaded as they're scrolled), PDFs, fonts,
//! what's in a ZIP, and details.

use super::sidebar::chev;
use super::*;
use crate::api::{Bytes, EpubContents, Listing, MediaInfo, Packed, PdfLayer, PdfLink, PdfOutline, PdfRun, Peek};
use crate::api::{Found, Query};
use wasm_bindgen::closure::Closure;

/// A file shown another way than its type says, from "Open as…" or the context menu.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum PeekAs {
    Text,
    Hex,
    Image,
    Video,
    Zip,
}

impl PeekAs {
    fn label(self) -> &'static str {
        match self {
            PeekAs::Text => "Text",
            PeekAs::Hex => "Binary",
            PeekAs::Image => "Picture",
            PeekAs::Video => "Video",
            PeekAs::Zip => "ZIP",
        }
    }
}

/// Under a file shown another way: back to how its type shows, or another way.
fn peek_as_buttons(now: PeekAs, zip: bool, set: impl Fn(Option<PeekAs>) + Clone + Send + Sync + 'static) -> impl IntoView {
    view! {
        <div class="peek-as">
            <OpenAs now=now zip small=true pick=move |h| set(Some(h))/>
        </div>
    }
}

/// "Open as…": opens as text; its arrow lists the other ways (text, binary, picture, video, and
/// ZIP when `zip`). `now`, the way shown, is marked.
#[component]
fn OpenAs(
    #[prop(into)] pick: Callback<PeekAs>,
    #[prop(optional)] now: Option<PeekAs>,
    #[prop(optional)] zip: bool,
    #[prop(optional)] small: bool,
) -> impl IntoView {
    // Where the list is, under the arrow; None while closed.
    let at = RwSignal::new(None::<(f64, f64)>);
    let ways: Vec<PeekAs> = [PeekAs::Text, PeekAs::Hex, PeekAs::Image, PeekAs::Video].into_iter().chain(zip.then_some(PeekAs::Zip)).collect();
    let rows = ways.len();
    let wrap = NodeRef::<html::Span>::new();
    // Under the whole button, or over it when there's no room below; kept in the window.
    let toggle = move |_| {
        if at.get_untracked().is_some() {
            at.set(None);
            return;
        }
        let Some(el) = wrap.get_untracked() else {
            return;
        };
        let r = el.get_bounding_client_rect();
        let size = |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
        let (vw, vh) = (size(window().inner_width()), size(window().inner_height()));
        let (w, h) = (140.0_f64.max(r.width()), rows as f64 * 26.0 + 12.0);
        let x = r.left().min(vw - w - 4.0).max(4.0);
        let y = if r.bottom() + 4.0 + h <= vh - 4.0 { r.bottom() + 4.0 } else { (r.top() - 4.0 - h).max(4.0) };
        at.set(Some((x, y)));
    };
    view! {
        <span class="open-as" class:small=small node_ref=wrap>
            <button class="btn open-text" class:small=small title="Open as text" on:click=move |_| pick.run(PeekAs::Text)>{ico(Icon::Text)}"Open as…"</button>
            <button class="btn open-list" class:small=small title="Open as…" aria-label="Other ways to open it" aria-haspopup="menu"
                aria-expanded=move || at.get().is_some().to_string() on:click=toggle>{ico(Icon::Chevron)}</button>
            {move || at.get().map(|(x, y)| {
                let ways = ways.clone();
                view! {
                    <div class="menu-backdrop" on:click=move |_| at.set(None) on:contextmenu=move |ev| { ev.prevent_default(); at.set(None) }></div>
                    <div class="menu open-as-list" role="menu" style=format!("left: {x}px; top: {y}px; min-width: {}px", wrap.get_untracked().map_or(140.0, |w| w.get_bounding_client_rect().width().max(140.0)))
                        on:keydown=move |ev| if ev.key() == "Escape" { at.set(None) }>
                        {ways.into_iter().map(|w| view! {
                            <button role="menuitemradio" aria-checked=(now == Some(w)).to_string() class:on=now == Some(w)
                                on:click=move |_| { at.set(None); pick.run(w) }>{w.label()}</button>
                        }).collect_view()}
                    </div>
                }
            })}
        </span>
    }
}

const PDF_EXTS: &[&str] = &["pdf"];
const FONT_EXTS: &[&str] = &["ttf", "otf", "woff", "woff2"];
pub(super) const ZIP_EXTS: &[&str] = &["zip", "apk", "jar", "cbz", "xapk", "ipa", "aar", "war", "whl", "vsix", "nupkg"];

/// Asks the built-in viewer for the pages only: no toolbar or side panes. Edge on Windows does as
/// asked; WebKit (macOS, Linux) ignores it and keeps its own controls.
const PDF_PLAIN: &str = "toolbar=0&navpanes=0";

/// Around and between the pages drawn, in CSS pixels.
const PDF_PAD: f64 = 12.0;
const PDF_GAP: f64 = 10.0;
/// Pages are drawn this many device pixels wider at a time, so resizing the preview draws them
/// again only now and then (they're scaled to fit in between).
const PDF_RES_STEP: f64 = 128.0;

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

/// How wide `t` is in the text over a PDF page (its font, `.pdf-text`) at 100px.
fn run_width(t: &str) -> f64 {
    thread_local! {
        static PEN: Option<web_sys::CanvasRenderingContext2d> = document()
            .create_element("canvas")
            .ok()
            .and_then(|c| c.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .and_then(|c| c.get_context("2d").ok().flatten())
            .and_then(|c| c.dyn_into().ok())
            .inspect(|c: &web_sys::CanvasRenderingContext2d| c.set_font("100px sans-serif"));
    }
    PEN.with(|p| p.as_ref().and_then(|p| p.measure_text(t).ok()).map_or(0.0, |m| m.width()))
}

/// A page's text laid over it, transparent: each run in its box, its font as tall as the box and
/// stretched to its width, so what's selected is what's under it. `aspect`: the page's width over
/// its height, as the box is in fractions of each.
fn pdf_text_layer(runs: &[PdfRun], aspect: f64) -> impl IntoView {
    let spans = runs
        .iter()
        .map(|r| {
            let [x, y, w, h] = r.b;
            let m = run_width(&r.t);
            let k = if m > 0.0 && h > 0.0 { w * 100.0 * aspect / (m * h) } else { 1.0 };
            let at = format!("left: {}%; top: {}%; font-size: calc(var(--ih) * {h}); transform: scaleX({k})", x * 100.0, y * 100.0);
            view! { <span style=at>{format!("{}{}", r.t, r.e)}</span> }
        })
        .collect_view();
    view! { <div class="pdf-text">{spans}</div> }
}

/// A page's links over its text: one goes to its page (`go`), or opens in the browser or mail app.
fn pdf_link_layer(links: &[PdfLink], go: impl Fn(usize) + Copy + 'static) -> impl IntoView {
    links
        .iter()
        .map(|l| {
            let [x, y, w, h] = l.b;
            let at = format!("left: {}%; top: {}%; width: {}%; height: {}%", x * 100.0, y * 100.0, w * 100.0, h * 100.0);
            let (page, uri) = (l.page, l.uri.clone());
            let tip = match (&page, &uri) {
                (Some(n), _) => format!("Page {n}"),
                (_, Some(u)) => u.clone(),
                _ => String::new(),
            };
            let click = move |ev: web_sys::MouseEvent| {
                ev.prevent_default();
                if let Some(n) = page {
                    go(n);
                } else if let Some(u) = uri.clone() {
                    spawn_local(async move {
                        if let Err(e) = call::<()>("open_link", json!({ "url": u })).await {
                            web_sys::console::warn_1(&e.into());
                        }
                    });
                }
            };
            view! { <a class="pdf-link" href="#" style=at title=tip.clone() aria-label=tip on:click=click></a> }
        })
        .collect_view()
}

/// A PDF with a bar above it: the outline, find in its text, go to a page, zoom, turn, open it in
/// its app, show it in its folder.
///
/// The pages are drawn by PDFium in the backend (`ferry://…/pdfpage`, pixels put straight on a
/// canvas, `pdf`): roughly those near what's scrolled to, then sharp those in view, at the
/// width they're shown at (only the piece in view, zoomed in): zoom makes the pages bigger, not the viewer, and a
/// resized preview draws them again, sharp. Matches found are marked on the pages, and their links
/// go to their page or open in the browser. Without PDFium,
/// the web view's own viewer shows the PDF instead; it's out of the app's reach, so a page is gone
/// to through the address (#page=), zoom scales the whole viewer, and matches aren't marked.
#[component]
fn PdfView(pane: Pane, src: Src, entry: Entry, url: String, #[prop(optional)] inner: Option<Inner>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let frame = NodeRef::<html::Iframe>::new();
    let in_zip = inner.is_some();
    let (path, size, mtime) = (entry.path.clone(), entry.size, entry.mtime);
    let what = StoredValue::new((path.clone(), inner));
    let page_url = StoredValue::new(url.clone());
    let outline = RwSignal::new(None::<Result<PdfOutline, String>>);
    // The pane beside the pages, hidden until asked for, every time: its Outline tab, or its
    // Search tab (`search_tab`).
    let show = RwSignal::new(false);
    let search_tab = RwSignal::new(false);
    let typed = RwSignal::new(String::new());
    let page_input = NodeRef::<html::Input>::new();
    let args = move || {
        let (path, inner) = what.get_value();
        match &inner {
            Some(i) => json!({ "src": src.key(), "path": i.zip_path, "size": size, "mtime": mtime, "inner": i.name, "zip_size": i.zip_size }),
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
        if show.get_untracked() && !search_tab.get_untracked() {
            show.set(false);
        } else {
            search_tab.set(false);
            show.set(true);
            load();
        }
    };
    let pages = move || {
        sizes
            .with(|s| s.as_ref().and_then(|s| s.as_ref()).map(Vec::len))
            .or_else(|| outline.with(|o| o.as_ref().and_then(|o| o.as_ref().ok()).map(|o| o.pages)))
    };
    let zoom = RwSignal::new(1.0f64);
    let turns = RwSignal::new(0u8);
    let stage = NodeRef::<html::Div>::new();
    let room = RwSignal::new(None::<(f64, f64)>);
    // The stage scrolled to (top, height shown), for which pages to draw.
    let scrolled = RwSignal::new((0.0f64, 0.0f64));
    // The pages drawn: their width, each one's top and height, and the height of them all.
    let layout = Memo::new(move |_| {
        let Some((w, _)) = room.get() else { return (0.0, vec![], 0.0) };
        let cw = (w - 2.0 * PDF_PAD).max(50.0) * zoom.get();
        let odd = turns.get() % 2 == 1;
        let mut y = PDF_PAD;
        let mut at = vec![];
        sizes.with(|s| {
            for &[pw, ph] in s.as_ref().and_then(|s| s.as_ref()).map_or(&[][..], |s| &s[..]) {
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
        layout.with(|(_, at, _)| (at.partition_point(|&(y, h)| y + h < lo), at.partition_point(|&(y, _)| y <= hi)))
    });
    // The page at the top third of what's shown, from 0.
    let here = Memo::new(move |_| {
        let (top, ch) = scrolled.get();
        layout.with(|(_, at, _)| at.partition_point(|&(y, h)| y + h < top + ch / 3.0).min(at.len().saturating_sub(1)))
    });
    Effect::new(move |_| {
        let n = here.get();
        if !drawn() {
            return;
        }
        let typing = page_input.get_untracked().is_some_and(|i| document().active_element().is_some_and(|a| a == **i));
        if !typing {
            typed.set((n + 1).to_string());
        }
    });
    let go = move |n: usize| {
        let n = pages().map_or(n, |last| n.min(last)).max(1);
        if drawn() {
            if let (Some(s), Some(y)) = (stage.get_untracked(), layout.with_untracked(|(_, at, _)| at.get(n - 1).map(|a| a.0))) {
                s.set_scroll_top((y - PDF_PAD / 2.0).max(0.0) as i32);
            }
        } else if let Some(f) = frame.get_untracked() {
            // Set again even when it's the same, so a second click goes back to the page.
            let _ = f.set_attribute("src", &format!("{}#page={n}&{PDF_PLAIN}", page_url.get_value()));
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
        show.set(true);
        find_focus.update(|n| *n += 1);
    };
    let close_find = move || {
        search_tab.set(false);
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
            timer.set_value(set_timeout_with_handle(run_search, std::time::Duration::from_millis(300)).ok());
        }
    };
    on_cleanup(move || {
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    let has_hits =
        move |page: usize| found.with(|f| f.as_ref().and_then(|f| f.as_ref().ok()).is_some_and(|f| f.hits.iter().any(|h| h.path == page.to_string())));
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
            let r = call::<Vec<Vec<[f64; 4]>>>("pdf_marks", args).await.unwrap_or_default();
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
            let Some(el) = stage.get_untracked().and_then(|s| s.query_selector(".pdf-mark.here").ok().flatten()) else { return };
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
    let open = {
        let path = path.clone();
        move |_| ctx.open_in_app_from(p, src, path.clone())
    };
    let reveal = move |_| {
        let path = path.clone();
        spawn_local(async move {
            if let Err(e) = call::<()>("reveal", json!({ "path": path })).await {
                p.error(e);
            }
        });
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
        let want = (cw * window().device_pixel_ratio().max(1.0) / PDF_RES_STEP).ceil() * PDF_RES_STEP;
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
        let Some(s) = stage.get_untracked() else { return change() };
        let (sh, ch) = (s.scroll_height() as f64, s.client_height() as f64);
        let mid = if sh > 0.0 { (s.scroll_top() as f64 + ch / 2.0) / sh } else { 0.0 };
        change();
        request_animation_frame(move || {
            if let Some(s) = stage.get_untracked() {
                s.set_scroll_top((mid * s.scroll_height() as f64 - s.client_height() as f64 / 2.0).max(0.0) as i32);
                s.set_scroll_left((s.scroll_width() - s.client_width()).max(0) / 2);
            }
        });
    };
    // Drawn pages keep their place through `anchor` instead.
    let set_zoom = move |z: f64| {
        let z = z.clamp(0.25, 8.0);
        if drawn() {
            zoom.set(z)
        } else {
            keep_place(&|| zoom.set(z))
        }
    };
    let turn = move |by: u8| {
        if drawn() {
            turns.update(|t| *t = (*t + by) % 4)
        } else {
            keep_place(&|| turns.update(|t| *t = (*t + by) % 4))
        }
    };
    // (The box it takes up, the viewer's own size and how it's moved and turned into that box.)
    let placed = move || {
        let Some((w, h)) = room.get() else { return (String::new(), "width: 100%; height: 100%".to_string()) };
        let z = zoom.get();
        let t = turns.get();
        // The viewer fills the stage (its sides swapped when turned on its side) at the width it
        // was first laid out at, scaled to fit, then zoomed: a real zoom, not a wider page column.
        let (own_w, own_h) = if t % 2 == 1 { (h, w) } else { (w, h) };
        let laid_w = base.get().unwrap_or(own_w);
        let fit = if laid_w > 0.0 && own_w > 0.0 { own_w / laid_w } else { 1.0 };
        let (big_w, big_h) = (own_w * z, own_h * z);
        let (box_w, box_h) = if t % 2 == 1 { (big_h, big_w) } else { (big_w, big_h) };
        let turn = match t {
            1 => format!("translateX({big_h}px) rotate(90deg) "),
            2 => format!("translate({big_w}px, {big_h}px) rotate(180deg) "),
            3 => format!("translateY({big_w}px) rotate(-90deg) "),
            _ => String::new(),
        };
        let (laid_h, s) = (own_h / fit, z * fit);
        (format!("width: {box_w}px; height: {box_h}px"), format!("width: {laid_w}px; height: {laid_h}px; transform: {turn}scale({s})"))
    };
    // Where the reader is, kept through zooming, turning and resizing, which move everything
    // else: the page at the middle of what's shown (from 0), how far down it that is (a fraction
    // of its height), and how far across the column (a fraction of its width).
    let anchor = StoredValue::new(None::<(usize, f64, f64)>);
    // While the place is put back: the scrolls that makes aren't the reader's.
    let restoring = StoredValue::new(false);
    let note_anchor = move |s: &web_sys::HtmlElement| {
        let (top, ch) = (s.scroll_top() as f64, s.client_height() as f64);
        let across = (s.scroll_left() as f64 + s.client_width() as f64 / 2.0) / (s.scroll_width() as f64).max(1.0);
        let at = layout.with_untracked(|(_, at, _)| {
            let mid = top + ch / 2.0;
            let i = at.partition_point(|&(y, h)| y + h < mid).min(at.len().checked_sub(1)?);
            let (y, h) = at[i];
            Some((i, ((mid - y) / h.max(1.0)).clamp(0.0, 1.0), across.clamp(0.0, 1.0)))
        });
        if at.is_some() {
            anchor.set_value(at);
        }
    };
    Effect::new(move |_| {
        layout.track();
        let Some((i, down, across)) = anchor.get_value() else { return };
        restoring.set_value(true);
        // After the pages are laid out again, so the stage is as tall as they are.
        request_animation_frame(move || {
            if let Some(s) = stage.get_untracked() {
                if let Some((y, h)) = layout.try_with_untracked(|(_, at, _)| at.get(i).copied()).flatten() {
                    let ch = s.client_height() as f64;
                    s.set_scroll_top((y + down * h - ch / 2.0).max(0.0).round() as i32);
                    let cw = s.client_width() as f64;
                    s.set_scroll_left((across * s.scroll_width() as f64 - cw / 2.0).max(0.0).round() as i32);
                    let _ = scrolled.try_set((s.scroll_top() as f64, ch));
                }
            }
            // The scroll events it makes come before the next frame.
            request_animation_frame(move || {
                let _ = restoring.try_set_value(false);
            });
        });
    });
    // Bumped once scrolling, zooming or resizing has stopped for a moment: the pages in view
    // are drawn sharp then, not on every step.
    let settled = RwSignal::new(0u32);
    let settle_timer = StoredValue::new_local(None::<TimeoutHandle>);
    Effect::new(move |_| {
        scrolled.track();
        layout.track();
        res.track();
        if let Some(h) = settle_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        let h = set_timeout_with_handle(
            move || {
                settled.try_update(|n| *n += 1);
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
            Some(i) => format!("p={}&s={size}&m={mtime}&e={}&z={}", uri_part(&i.zip_path), uri_part(&i.name), i.zip_size),
            None => format!("p={}&s={size}&m={mtime}", uri_part(&path)),
        };
        let tile = tile.map(|[x, y, w, h]| format!("&t={x},{y},{w},{h}")).unwrap_or_default();
        crate::os::served(&format!("pdfpage?src={}&{at}&n={n}&w={px}{tile}", src.key()))
    };
    // Page `i` (from 0): a box of its size, with the page drawn in it when it's near what's shown.
    let page_view = move |i: usize| {
        let shown = Memo::new(move |_| {
            let (a, b) = near.get();
            i >= a && i < b
        });
        let place = move || {
            layout.with(|(cw, at, _)| at.get(i).map(|&(y, h)| format!("top: {y}px; left: {PDF_PAD}px; width: {cw}px; height: {h}px")).unwrap_or_default())
        };
        let sheet = move || layout.with(|(cw, at, _)| at.get(i).map(|&(_, h)| pdf_sheet(turns.get(), *cw, h)).unwrap_or_default());
        // Device pixels across the unturned page: the step's, its sides swapped when it's turned.
        let page = i + 1;
        let aspect = move || {
            let [pw, ph] = sizes.with(|s| s.as_ref().and_then(|s| s.as_ref()).and_then(|s| s.get(i).copied()).unwrap_or([1.0, 1.0]));
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
                        let seen = view(0.0);
                        if seen[2] <= seen[0] || seen[3] <= seen[1] {
                            drawer.forget(page, true);
                            let _ = asked.try_update_value(|a| a.1 += 1);
                            if have.try_get_value().flatten().is_some() {
                                have.set_value(None);
                                super::pdf::clear(&c);
                            }
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
    let heading_list = move || match outline.get() {
        None => view! { <span class="hint">"Loading…"</span> }.into_any(),
        Some(Err(e)) => view! { <span class="hint">{e}</span> }.into_any(),
        Some(Ok(o)) if o.headings.is_empty() => view! { <span class="hint">"This PDF has no outline."</span> }.into_any(),
        Some(Ok(o)) => o
            .headings
            .into_iter()
            .map(|h| {
                let indent = format!("padding-left: {}px", 8 + 12 * h.level.saturating_sub(1).min(8));
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
    let stage_body = move || match sizes.get().map(|s| s.map(|s| s.len())) {
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
    };
    view! {
        <div class="viewer-bar pdf-bar pdf-reader">
            // Rendered only with PDFium: `hidden` loses to `.btn-group`'s display.
            {move || drawn().then(|| view! {
                <span class="btn-group" role="group">
                    <button class="btn" class:active=move || show.get() && !search_tab.get() title="Outline" aria-label="Outline"
                        on:click=move |_| show_outline()>{ico(Icon::Contents)}</button>
                    <button class="btn" class:active=move || show.get() && search_tab.get() title=crate::os::keys("Find in the PDF (⌘F)") aria-label="Find in the PDF"
                        aria-pressed=move || (show.get() && search_tab.get()).to_string()
                        on:click=move |_| if show.get_untracked() && search_tab.get_untracked() { show.set(false) } else { open_find() }>{ico(Icon::Search)}</button>
                </span>
            })}
            {move || unread.get().map(|e| view! { <span class="hint pdf-unread" title=e.clone()>{format!("No outline or search: {e}")}</span> })}
            <span class="gap"></span>
            <input class="pdf-page" node_ref=page_input type="text" inputmode="numeric" placeholder="Page" aria-label="Go to page"
                prop:value=move || typed.get() on:input=move |ev| typed.set(event_target_value(&ev)) on:keydown=on_key/>
            {move || pages().map(|n| view! { <span class="pdf-pages">{format!("of {n}")}</span> })}
            <span class="gap"></span>
            <span class="btn-group" role="group">
                <button class="btn" title="Zoom out" aria-label="Zoom out" on:click=move |_| set_zoom(zoom.get_untracked() / 1.25)>{ico(Icon::ZoomOut)}</button>
                <button class="btn zoom" title="Fit the width" on:click=move |_| set_zoom(1.0)>{move || format!("{:.0}%", zoom.get() * 100.0)}</button>
                <button class="btn" title="Zoom in" aria-label="Zoom in" on:click=move |_| set_zoom(zoom.get_untracked() * 1.25)>{ico(Icon::ZoomIn)}</button>
            </span>
            <span class="btn-group" role="group">
                <button class="btn" title="Turn left" aria-label="Turn left" on:click=move |_| turn(3)>{ico(Icon::TurnLeft)}</button>
                <button class="btn" title="Turn right" aria-label="Turn right" on:click=move |_| turn(1)>{ico(Icon::TurnRight)}</button>
            </span>
            <span class="pdf-fill"></span>
            {(!in_zip).then(|| view! { <button class="btn" title="Open in its default app" aria-label="Open in its default app" on:click=open>{ico(Icon::OpenOut)}</button> })}
            {(!in_zip && src == Src::Mac).then(|| {
                let tip = format!("Show in {}", crate::os::file_manager());
                view! { <button class="btn" title=tip.clone() aria-label=tip on:click=reveal>{ico(Icon::Folder)}</button> }
            })}
        </div>
        <div class="pdf-body">
            {move || (show.get() && drawn()).then(|| view! {
                <nav class="pdf-outline epub-outline" aria-label="Outline and search">
                    <div class="outline-tabs" role="tablist">
                        <button role="tab" class:on=move || !search_tab.get() aria-selected=move || (!search_tab.get()).to_string()
                            title="Outline" on:click=move |_| close_find()>{ico(Icon::Contents)}"Outline"</button>
                        <button role="tab" class:on=search_tab aria-selected=move || search_tab.get().to_string()
                            title=crate::os::keys("Search the PDF (⌘F)") on:click=move |_| open_find()>{ico(Icon::Search)}"Search"</button>
                    </div>
                    {move || if search_tab.get() {
                        super::reading::find_bar(super::reading::PDF_WORDS, fq, found, searching, find_focus, search, pick, current, close_find).into_any()
                    } else {
                        heading_list().into_any()
                    }}
                </nav>
            })}
            <div class="pdf-stage" class:drawn=drawn node_ref=stage on:scroll=on_scroll>{stage_body}</div>
        </div>
    }
}

pub(super) const EPUB_READ_KEY: &str = "ferry-files.epub-read";
/// The text size of a book read in the preview, in percent of 15 px.
const EPUB_ZOOM_KEY: &str = "ferry-files.epub-zoom";

/// An EPUB: its cover, or its chapters one at a time. Only the book's contents list and the
/// chapter shown are read (and its pictures, as they show), not the whole book. `line` is the
/// name and facts; the book's buttons go at the end of that line.
#[component]
fn EpubView(pane: Pane, src: Src, entry: Entry, line: AnyView) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let ep = ctx.ep;
    // Editing reads the whole book into the app; reading takes a chapter at a time.
    let editing = {
        let path = entry.path.clone();
        Signal::derive(move || ep.host.get() == Some(pane.id) && ep.holds(&src.key(), &path))
    };
    let opening = RwSignal::new(false);
    let edit_error = RwSignal::new(None::<String>);
    let book_path = StoredValue::new(entry.path.clone());
    let edit = move |_| {
        edit_error.set(None);
        if editing.get_untracked() {
            ep.close(false);
            return;
        }
        opening.set(true);
        let path = book_path.get_value();
        spawn_local(async move {
            let r = ep.open(&src.key(), ctx.place(src), path, pane.id).await;
            let _ = opening.try_set(false);
            if let Err(e) = r {
                let _ = edit_error.try_set(Some(e));
            }
        });
    };
    let reading = ctx.epub_read;
    let contents = RwSignal::new(None::<Result<EpubContents, String>>);
    let at = RwSignal::new(0usize);
    let page = RwSignal::new(None::<Result<String, String>>);
    let asked = StoredValue::new(0u32);
    let (path, size) = (StoredValue::new(entry.path.clone()), entry.size);
    Effect::new(move |_| {
        if !reading.get() || contents.with_untracked(Option::is_some) {
            return;
        }
        let args = json!({ "src": src.key(), "path": path.get_value(), "size": size });
        spawn_local(async move {
            let r = call::<EpubContents>("epub_contents", args).await;
            let _ = contents.try_set(Some(r));
        });
    });
    Effect::new(move |_| {
        if !reading.get() {
            return;
        }
        let Some(ch) = contents.with(|c| c.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| c.chapters.get(at.get()).cloned())) else { return };
        let n = asked.get_value() + 1;
        asked.set_value(n);
        page.set(None);
        let args = json!({ "src": src.key(), "path": path.get_value(), "size": size, "chapter": ch.path });
        spawn_local(async move {
            let r = call::<String>("epub_peek", args).await;
            if asked.try_get_value() == Some(n) {
                let _ = page.try_set(Some(r));
            }
        });
    });
    let count = move || contents.with(|c| c.as_ref().and_then(|c| c.as_ref().ok()).map_or(0, |c| c.chapters.len()));
    let go = move |d: i32| {
        let to = at.get_untracked() as i32 + d;
        if to >= 0 && (to as usize) < count() {
            at.set(to as usize);
        }
    };
    let toggle = move |_| {
        if editing.get_untracked() {
            ep.close(false);
            reading.set(true);
            store(EPUB_READ_KEY, "1");
            return;
        }
        let on = !reading.get_untracked();
        reading.set(on);
        store(EPUB_READ_KEY, if on { "1" } else { "0" });
    };
    let zoom = RwSignal::new(stored(EPUB_ZOOM_KEY).and_then(|z| z.parse::<u32>().ok()).unwrap_or(100).clamp(60, 250));
    let set_zoom = move |z: u32| {
        let z = z.clamp(60, 250);
        zoom.set(z);
        store(EPUB_ZOOM_KEY, &z.to_string());
    };
    let outline = ep.outline;
    let frame = NodeRef::<html::Iframe>::new();
    // The page in a quiet style, after the <base> the backend adds so the book's own styles load.
    // A new zoom builds the page again.
    let doc = move || {
        let html = page.get()?.ok()?;
        let style = super::reading::page_style(&ctx.theme.get(), 15, zoom.get(), "16px 20px 60px", "1.55");
        let next = contents.with(|c| c.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| c.chapters.get(at.get() + 1)).map(|ch| ch.label.clone()));
        Some(super::reading::page(&html, &style, next.as_deref().map(|l| (l, at.get() + 1))))
    };
    // Scrolling on past a chapter's end turns to the next; back past its start, to the previous
    // one's end.
    let land_end = StoredValue::new(false);
    // Find while reading: the book's chapters are searched (their text read for it), and the
    // matches marked in the chapter shown.
    // Its box and results are the Search tab of the pane beside the page (the other tab is the
    // chapters); finding is that tab showing.
    let search_tab = RwSignal::new(false);
    let finding = Memo::new(move |_| ep.outline.get() && search_tab.get());
    let fq = RwSignal::new(Query::default());
    let found = RwSignal::new(None::<Result<Found, String>>);
    let searching = RwSignal::new(false);
    let find_focus = RwSignal::new(0u32);
    let open_find = move || {
        search_tab.set(true);
        if !ep.outline.get_untracked() {
            ep.toggle_outline();
        }
        find_focus.update(|n| *n += 1);
    };
    // Esc in the Search tab: back to the chapters.
    let close_find = move || search_tab.set(false);
    let show_chapters = move || {
        if ep.outline.get_untracked() && !search_tab.get_untracked() {
            ep.toggle_outline();
        } else {
            search_tab.set(false);
            if !ep.outline.get_untracked() {
                ep.toggle_outline();
            }
        }
    };
    // The page's marks go when the Search tab does.
    Effect::new(move |was: Option<bool>| {
        let now = finding.get();
        if was == Some(true) && !now {
            if let Some(f) = frame.get_untracked() {
                super::reading::unmark(&f);
            }
        }
        now
    });
    // Searches now, or (`now` false, while typing) once typing has stopped for a moment. A
    // search still running when another starts is let go: only the last one's results show.
    let asked_search = StoredValue::new(0u32);
    let timer = StoredValue::new(None::<TimeoutHandle>);
    let run_search = move || {
        let query = fq.get_untracked();
        let n = asked_search.get_value() + 1;
        asked_search.set_value(n);
        if query.find.trim().is_empty() {
            searching.set(false);
            found.set(None);
            if let Some(f) = frame.get_untracked() {
                super::reading::unmark(&f);
            }
            return;
        }
        searching.set(true);
        let args = json!({ "src": src.key(), "path": path.get_value(), "size": size, "query": query });
        spawn_local(async move {
            let r = call::<Found>("epub_peek_find", args).await;
            if asked_search.try_get_value() != Some(n) {
                return;
            }
            let _ = searching.try_set(false);
            let _ = found.try_set(Some(r));
            if let Some(f) = frame.try_get_untracked().flatten() {
                super::reading::mark(&f, &query);
            }
        });
    };
    let search = move |now: bool| {
        if let Some(h) = timer.get_value() {
            h.clear();
        }
        if now {
            run_search();
        } else {
            timer.set_value(set_timeout_with_handle(run_search, std::time::Duration::from_millis(300)).ok());
        }
    };
    // A match picked in the results: its chapter, with that match the current one. In another
    // chapter, it's marked once that one has loaded.
    let current = RwSignal::new(None::<(String, usize)>);
    let pending_hit = StoredValue::new(None::<usize>);
    let chapter_at =
        move |p: &str| contents.with_untracked(|c| c.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| c.chapters.iter().position(|ch| ch.path == p)));
    let pick = move |p: String, hit: usize| {
        let Some(k) = chapter_at(&p) else { return };
        current.set(Some((p, hit)));
        if k == at.get_untracked() {
            if let Some(f) = frame.get_untracked() {
                super::reading::mark(&f, &fq.get_untracked());
                super::reading::focus_mark(&f, hit);
            }
        } else {
            pending_hit.set_value(Some(hit));
            land_end.set_value(false);
            at.set(k);
        }
    };
    // A link in the page: to another chapter (or a place in it), or a place in this one. Links
    // out of the book aren't followed.
    let anchor = StoredValue::new(None::<String>);
    let link = move |href: String| {
        let Ok(u) = web_sys::Url::new(&href) else { return };
        let decode = |s: String| js_sys::decode_uri_component(&s).ok().and_then(|v| v.as_string()).unwrap_or(s);
        let frag = Some(decode(u.hash().trim_start_matches('#').to_string())).filter(|f| !f.is_empty());
        let target = decode(u.pathname());
        let k = contents.with_untracked(|c| {
            c.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| c.chapters.iter().position(|ch| target.ends_with(&format!("/{}", ch.path))))
        });
        match k {
            Some(k) if k == at.get_untracked() => {
                if let (Some(f), Some(id)) = (frame.get_untracked(), frag) {
                    super::reading::scroll_to(&f, &id);
                }
            }
            Some(k) => {
                anchor.set_value(frag);
                land_end.set_value(false);
                at.set(k);
            }
            None => {}
        }
    };
    // ⌘F from the app's window, with the focus in this preview (app.rs).
    let asked_find = StoredValue::new(ctx.read_find.get_untracked().1);
    Effect::new(move |_| {
        let (id, n) = ctx.read_find.get();
        if n != asked_find.get_value() {
            asked_find.set_value(n);
            if id == pane.id && reading.get_untracked() && !editing.get_untracked() {
                open_find();
            }
        }
    });
    let turn = move |d: i32| {
        let to = at.get_untracked() as i32 + d;
        if to >= 0 && (to as usize) < count() {
            land_end.set_value(d < 0);
            at.set(to as usize);
        }
    };
    let loaded = move |e: web_sys::Event| {
        let end = land_end.get_value();
        land_end.set_value(false);
        let frame = event_target::<web_sys::HtmlIFrameElement>(&e);
        match super::reading::followed(&frame) {
            Some(k) if k < count() => at.set(k),
            Some(_) => {}
            None => {
                super::reading::follow(&frame, end, turn, open_find, link);
                if finding.get_untracked() {
                    super::reading::mark(&frame, &fq.get_untracked());
                }
                if let Some(hit) = pending_hit.try_update_value(Option::take).flatten() {
                    super::reading::focus_mark(&frame, hit);
                } else if let Some(id) = anchor.try_update_value(Option::take).flatten() {
                    super::reading::scroll_to(&frame, &id);
                }
            }
        }
    };
    let chapter_list = move || {
        contents.with(|c| match c {
            Some(Ok(c)) => {
                c.chapters.iter().enumerate().map(|(k, ch)| view! { <option value=k.to_string()>{ch.label.clone()}</option> }).collect_view().into_any()
            }
            _ => ().into_any(),
        })
    };
    let outline_list = move || {
        contents.with(|c| match c {
            Some(Ok(c)) if c.chapters.is_empty() => view! { <span class="hint">"This book lists no chapters."</span> }.into_any(),
            Some(Ok(c)) => c
                .chapters
                .iter()
                .enumerate()
                .map(|(k, ch)| {
                    let label = ch.label.clone();
                    view! {
                        <button class="pdf-head" class:here=move || at.get() == k title=label.clone() on:click=move |_| at.set(k)>
                            <span class="name">{label.clone()}</span>
                        </button>
                    }
                })
                .collect_view()
                .into_any(),
            Some(Err(e)) => view! { <span class="hint">{e.clone()}</span> }.into_any(),
            None => view! { <span class="hint">"Loading…"</span> }.into_any(),
        })
    };
    // Narrow, the bar drops the zoom percentage, then the chapter list, then the zoom.
    let bar = NodeRef::<html::Div>::new();
    fold_bar(bar, 3);
    let chapter = move || match (contents.get(), page.get()) {
        (Some(Err(e)), _) | (_, Some(Err(e))) => view! { <p class="hint">{e}</p> }.into_any(),
        (Some(Ok(c)), _) if c.chapters.is_empty() => view! { <p class="hint">"This book lists no chapters."</p> }.into_any(),
        (Some(Ok(_)), Some(Ok(_))) => view! {
            // No allow-scripts: a book's scripts must never run next to the app's commands.
            <iframe class="epub-page" sandbox="allow-same-origin" node_ref=frame srcdoc=doc title="Chapter" on:load=loaded></iframe>
        }
        .into_any(),
        _ => view! { <p class="hint">"Opening…"</p> }.into_any(),
    };
    view! {
        <div class="meta">
            <div class="info">{line}</div>
            <div class="meta-tools">
                {move || opening.get().then(|| view! { <span class="spin"></span> })}
                <span class="btn-group" role="group">
                    <button class="btn" class:active=move || reading.get() && !editing.get() title="Read the book here" aria-label="Read the book here" on:click=toggle>{ico(Icon::Book)}</button>
                    <button class="btn" class:active=editing title="Edit the book" aria-label="Edit the book" disabled=opening on:click=edit>{ico(Icon::Edit)}</button>
                </span>
            </div>
        </div>
        <div class="view" class:epub-view=move || editing.get() || reading.get()>
            {move || edit_error.get().map(|e| view! {
                <div class="notice error">
                    <span class="text">{e}</span>
                    <button class="btn" title="Dismiss" aria-label="Dismiss" on:click=move |_| edit_error.set(None)>{ico(Icon::Close)}</button>
                </div>
            })}
            {move || if editing.get() {
                view! {
                    <div class="viewer-bar epub-bar book-editor" node_ref=bar><crate::epub::EditBar/></div>
                    <crate::epub::EditBody/>
                }
                .into_any()
            } else if !reading.get() {
                view! { <BigCover src entry=entry.clone()/> }.into_any()
            } else {
                view! {
                    <div class="viewer-bar epub-bar book-reader" node_ref=bar>
                        // The pane beside the page: its Chapters tab, and its Search tab.
                        <span class="btn-group" role="group">
                            <button class="btn" class:active=move || outline.get() && !search_tab.get() title="Chapters" aria-label="Chapters"
                                on:click=move |_| show_chapters()>{ico(Icon::Contents)}</button>
                            <button class="btn" class:active=finding title=crate::os::keys("Find in the book (⌘F)") aria-label="Find in the book"
                                aria-pressed=move || finding.get().to_string()
                                on:click=move |_| if finding.get_untracked() { ep.toggle_outline() } else { open_find() }>{ico(Icon::Search)}</button>
                        </span>
                        <span class="btn-group" role="group">
                            <button class="btn" title="Smaller text" aria-label="Smaller text" on:click=move |_| set_zoom(zoom.get_untracked().saturating_sub(10))>{ico(Icon::ZoomOut)}</button>
                            <button class="btn zoom" title="Back to 100%" on:click=move |_| set_zoom(100)>{move || format!("{}%", zoom.get())}</button>
                            <button class="btn" title="Bigger text" aria-label="Bigger text" on:click=move |_| set_zoom(zoom.get_untracked() + 10)>{ico(Icon::ZoomIn)}</button>
                        </span>
                        <select class="epub-chapters grow" aria-label="Chapter" prop:value=move || at.get().to_string()
                            on:keydown=|ev| ev.stop_propagation()
                            on:change=move |ev| at.set(event_target_value(&ev).parse().unwrap_or(0))>{chapter_list}</select>
                        <span class="epub-of">{move || match count() { 0 => String::new(), n => format!("{} of {n}", at.get() + 1) }}</span>
                        <span class="btn-group" role="group">
                            <button class="btn" title="Previous chapter" aria-label="Previous chapter" disabled=move || {at.get() == 0} on:click=move |_| go(-1)>{ico(Icon::Prev)}</button>
                            <button class="btn" title="Next chapter" aria-label="Next chapter" disabled=move || {at.get() + 1 >= count()} on:click=move |_| go(1)>{ico(Icon::Next)}</button>
                        </span>
                    </div>
                    <div class="epub-body book-reader">
                        {move || outline.get().then(|| view! {
                            <nav class="pdf-outline epub-outline">
                                <div class="outline-tabs" role="tablist">
                                    <button role="tab" class:on=move || !search_tab.get() aria-selected=move || (!search_tab.get()).to_string()
                                        title="Chapters" on:click=move |_| search_tab.set(false)>{ico(Icon::Contents)}"Chapters"</button>
                                    <button role="tab" class:on=search_tab aria-selected=move || search_tab.get().to_string()
                                        title=crate::os::keys("Search the book (⌘F)") on:click=move |_| open_find()>{ico(Icon::Search)}"Search"</button>
                                </div>
                                {move || if search_tab.get() {
                                    super::reading::find_bar(super::reading::BOOK_WORDS, fq, found, searching, find_focus, search, pick, current, close_find).into_any()
                                } else {
                                    outline_list().into_any()
                                }}
                            </nav>
                        })}
                        {chapter}
                    </div>
                }
                .into_any()
            }}
        </div>
    }
}

/// A file's size, then its width × height and length once known, for beside its name.
fn measures(size: Option<String>, info: RwSignal<MediaInfo>) -> String {
    let mut v: Vec<String> = size.into_iter().collect();
    info.with(|i| {
        v.extend(i.width.zip(i.height).map(|(w, h)| format!("{w} × {h}")));
        v.extend(i.duration.map(length));
    });
    v.join(" · ")
}

/// The cover of an EPUB packed in a ZIP, from `url`; the book is unpacked for it.
#[component]
fn ZippedCover(url: String) -> impl IntoView {
    let state = RwSignal::new(None::<bool>);
    view! {
        <div class="pic" class:whole=move || state.get() == Some(true)>
            {move || match state.get() {
                None => view! { <span class="spin"></span> }.into_any(),
                Some(false) => view! { <span class="hint">"This book has no cover."</span> }.into_any(),
                Some(true) => ().into_any(),
            }}
            <img class="full" src=url alt="Book cover" draggable="false" class:pending=move || state.get() != Some(true)
                on:load=move |_| state.set(Some(true)) on:error=move |_| state.set(Some(false))/>
        </div>
    }
}

/// The backend serves a whole file (a PDF, a font) up to this size.
const WHOLE_MAX: u64 = 64 * 1024 * 1024;

pub(super) fn has_ext(e: &Entry, exts: &[&str]) -> bool {
    !e.dir && exts.contains(&ext(&e.name).as_str())
}

/// What's selected, to the right of the files: the picture, video, sound, text or contents, and
/// details. Shown in every view; the button at the end of the status bar hides it.
/// `entry`: this file instead of the pane's selected item (the Editor view's preview tab), with
/// no ‹ › (the files around it aren't the pane's); `src`, where it is, if not the pane's place.
#[component]
pub(super) fn Preview(pane: Pane, #[prop(optional)] entry: Option<Entry>, #[prop(optional)] src: Option<Src>) -> impl IntoView {
    let src_of = src;
    let p = pane;
    let alone = entry.is_some();
    // Kept as the preview moves from one picture to the next, and from one video to the next.
    let (pic_turn, video_turn) = (Turn::new(), Turn::new());
    // Only a different item (or a different "show as") rebuilds it, so a reload doesn't restart
    // a playing video.
    let wanted = Memo::new(move |_| {
        let picked = match &entry {
            Some(e) => Some(e.clone()),
            None => p.selected_entry(),
        };
        picked.map(|e| {
            let forced = p.peek_as.with(|f| f.as_ref().filter(|(path, _)| *path == e.path).map(|f| f.1));
            // A file picked in this ZIP's columns.
            let src = p.src.get();
            let inner = p.zip_pick.with(|z| z.as_ref().filter(|z| z.0 == src && z.1 == e.path).map(|z| (z.2.clone(), z.3, z.4)));
            // A file picked in the columns of something opened as a ZIP (an EPUB, say) is what
            // shows, not the ZIP again.
            let forced = forced.filter(|f| !(inner.is_some() && *f == PeekAs::Zip));
            (e, forced, inner)
        })
    });
    // What's shown: what's picked, except that a picture not yet read is read (and decoded)
    // first, while what was shown stays, instead of its thumbnail and then the picture. The
    // first thing shown, and anything else, shows at once.
    let shown = RwSignal::new(wanted.get_untracked());
    Effect::new(move |_| {
        let w = wanted.get();
        if shown.with_untracked(|s| *s == w) {
            return;
        }
        let read = (!alone && shown.with_untracked(Option::is_some)).then(|| picture_url(p, &w)).flatten();
        match read {
            Some((path, url)) => {
                let target = StoredValue::new(w);
                when_read(p, path, url, move || target.with_value(|t| wanted.with_untracked(|now| now == t)), move || shown.set(target.get_value()));
            }
            None => {
                p.coming.set(None);
                p.pending.set_value(None);
                shown.set(w);
            }
        }
    });
    view! {
        <div class="preview" style=move || match p.preview_w.get() {
            w if w > 0.0 && !alone => format!("flex-basis: {w}px"),
            _ => String::new(),
        }>
            {move || match shown.get() {
                None => view! { <span class="hint">{move || match p.marked.with(HashSet::len) {
                    0 | 1 => "Pick an item to see it here.".to_string(),
                    n => format!("{n} items picked."),
                }}</span> }.into_any(),
                Some((zip, None, Some((name, size, stored)))) => view! { <Zipped pane=p src=p.src.get_untracked() zip name size stored/> }.into_any(),
                Some((e, forced, _)) => {
                    let what = match kind(&e) {
                        Kind::Folder => "Folder", Kind::Epub => "EPUB book", Kind::Text => "Text file", Kind::Other => "File",
                    };
                    let src = src_of.unwrap_or_else(|| p.src.get_untracked());
                    // Width, height and length, from the file's header; the picture or player
                    // fills in what the header didn't give (an SVG's size, an MP3's length).
                    let info = RwSignal::new(MediaInfo::default());
                    if media(&e).is_some() && forced.is_none() {
                        let path = e.path.clone();
                        spawn_local(async move {
                            if let Ok(h) = call::<MediaInfo>("media_info", json!({ "src": src.key(), "path": path })).await {
                                let _ = info.try_update(|i| {
                                    i.width = h.width.or(i.width);
                                    i.height = h.height.or(i.height);
                                    i.duration = h.duration.or(i.duration);
                                });
                            }
                        });
                    }
                    // Name with its size, width × height and length beside it, then kind, date
                    // and folder, on one line above.
                    let size = (!e.dir).then(|| human_size(e.size));
                    let modified = (e.mtime > 0).then(|| when(e.mtime));
                    let folder = parent(&e.path);
                    let measures = Signal::derive(move || measures(size.clone(), info));
                    let facts = move || {
                        let mut v = vec![what.to_string()];
                        v.extend(modified.clone());
                        v.push(folder.clone());
                        v.join(" · ")
                    };
                    let facts = Signal::derive(facts);
                    // The name and facts as a view, for the line above what's shown. An EPUB puts
                    // its own buttons at the end of it, so it takes the line (None here after).
                    let path = e.path.clone();
                    let mut line = Some(view! {
                        <span class="head">
                            <b class="name" title=e.path.clone()>{e.name.clone()}</b>
                            <span class="measures">{move || measures.get()}</span>
                        </span>
                        <span class="sub" title=move || facts.get()>{move || facts.get()}</span>
                        {forced.map(|now| peek_as_buttons(now, true, move |how| p.peek_as.set(how.map(|h| (path.clone(), h)))))}
                    }.into_any());
                    let thumb = |e: &Entry| view! { <div class="pic"><Thumbnail src=src entry=e.clone()/></div> };
                    // Set when the body is text, which puts the wrap and line-number toggles in the line.
                    let mut text = false;
                    let body = match (forced, media(&e)) {
                        (Some(PeekAs::Text), _) => { text = true; view! { <TextPeek src entry=e.clone()/> }.into_any() }
                        (Some(PeekAs::Hex), _) => view! { <HexPeek src entry=e.clone()/> }.into_any(),
                        (Some(PeekAs::Image), _) => view! { <FullImage pane=p src entry=e.clone() info turn=pic_turn url=stream_url(src, &e)/> }.into_any(),
                        (Some(PeekAs::Video), _) => view! { <Player pane=p entry=e.clone() url=stream_url(src, &e) video=true info turn=video_turn alone/> }.into_any(),
                        (Some(PeekAs::Zip), _) => view! { <ZipList src entry=e.clone()/> }.into_any(),
                        (None, Some(Media::Image)) if alone => view! { <FullImage pane=p src entry=e.clone() info turn=pic_turn url=stream_url(src, &e)/> }.into_any(),
                        (None, Some(Media::Image)) => view! { <FullImage pane=p src entry=e.clone() info turn=pic_turn/> }.into_any(),
                        (None, Some(Media::Video)) => view! { <Player pane=p entry=e.clone() url=stream_url(src, &e) video=true info turn=video_turn alone/> }.into_any(),
                        (None, Some(Media::Audio)) => view! {
                            {thumb(&e)}
                            <Player pane=p entry=e.clone() url=stream_url(src, &e) video=false info turn=video_turn alone/>
                        }.into_any(),
                        _ if has_ext(&e, PDF_EXTS) && e.size <= WHOLE_MAX => view! { <PdfView pane=p src entry=e.clone() url=stream_url(src, &e)/> }.into_any(),
                        _ if has_ext(&e, FONT_EXTS) && e.size <= WHOLE_MAX => view! { <FontSample src entry=e.clone()/> }.into_any(),
                        _ if has_ext(&e, &["epub"]) => {
                            let line = line.take().unwrap_or_else(|| ().into_any());
                            view! { <EpubView pane=p src entry=e.clone() line/> }.into_any()
                        }
                        _ if has_ext(&e, ZIP_EXTS) => view! { <ZipList src entry=e.clone()/> }.into_any(),
                        _ if is_text(&e) => { text = true; view! { <TextPeek src entry=e.clone()/> }.into_any() }
                        _ if e.dir => thumb(&e).into_any(),
                        // A type the preview doesn't know: offer to look inside anyway.
                        _ => {
                            let path = e.path.clone();
                            view! {
                                {thumb(&e)}
                                <div class="viewer-bar">
                                    <OpenAs zip=true pick=move |h| p.peek_as.set(Some((path.clone(), h)))/>
                                </div>
                            }.into_any()
                        }
                    };
                    match line {
                        Some(line) => view! {
                            <div class="meta">
                                <div class="info">{line}</div>
                                {text.then(text_tools)}
                            </div>
                            <div class="view">{body}</div>
                        }.into_any(),
                        // The EPUB drew the line itself.
                        None => body,
                    }
                }
            }}
        </div>
    }
}

/// A length in seconds as "1:02:03" or "2:03".
fn length(secs: f64) -> String {
    let s = secs.round() as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Text files the preview shows the start of: any size, as only the start is read.
fn is_text(e: &Entry) -> bool {
    !e.dir && is_text_name(&e.name)
}

/// Selects the picture, video or sound before (`-1`) or after (`1`) `e` in the pane, skipping
/// other files. It goes from what's picked, which can be ahead of what the preview shows while
/// the next picture is read.
pub(super) fn step_media(p: Pane, e: &Entry, delta: isize) {
    let Some(m) = media(e) else { return };
    let picked = p.selected.get_untracked();
    let next = p.entries.with_untracked(|es| {
        let same: Vec<&Entry> = es.iter().filter(|x| media(x) == Some(m)).collect();
        let at = picked.as_ref().and_then(|s| same.iter().position(|x| x.path == *s)).or_else(|| same.iter().position(|x| x.path == e.path))?;
        let i = at as isize + delta;
        (i >= 0).then(|| same.get(i as usize).map(|x| x.path.clone())).flatten()
    });
    if let Some(path) = next {
        p.select_only(Some(path));
    }
}

/// The picture the preview would show for `w` in full, and the URL it reads it from (the photo
/// shrunk to the preview, or the file; in a ZIP, the file unpacked). None for anything else.
fn picture_url(p: Pane, w: &Option<(Entry, Option<PeekAs>, Option<(String, u64, bool)>)>) -> Option<(String, String)> {
    let (e, forced, inner) = w.as_ref()?;
    if forced.is_some() {
        return None;
    }
    let src = p.src.get_untracked();
    let is_picture =
        |name: &str| media(&Entry { name: base(name), path: name.to_string(), dir: false, size: 0, mtime: 0, hidden: false }) == Some(Media::Image);
    match inner {
        Some((name, _, _)) => is_picture(name).then(|| (name.clone(), super::views::zipped_url(src, e, name))),
        None => (!e.dir && media(e) == Some(Media::Image)).then(|| (e.path.clone(), fit_url(src, e).unwrap_or_else(|| stream_url(src, e)))),
    }
}

/// Shows a picture once it's been read into memory and decoded: `show` puts `path`'s picture
/// (read from `url`) in the preview, which then draws it at once from that copy. Until then the
/// picture shown stays; a picture read ahead is shown at once.
/// Only the last one asked for is shown, and only if `still()` (nothing else was picked
/// meanwhile). One that can't be read is shown anyway, to say why.
pub(super) fn when_read(p: Pane, path: String, url: String, still: impl Fn() -> bool + 'static, show: impl FnOnce() + 'static) {
    if read_copy(p, &url).is_some() {
        p.coming.set(None);
        p.pending.set_value(None);
        show();
        return;
    }
    p.coming.set(Some((path, url.clone())));
    p.pending.set_value(Some(Box::new(move || {
        if still() {
            show();
        }
    })));
    read_pic(p, url);
}

/// How many pictures are kept in memory for ‹ ›: the one before, the one shown, the next.
const PICS_KEPT: usize = 3;

/// Reads the picture at `url` into memory and decodes it, unless that's done or under way; then
/// shows it if it's what ‹ › is waiting for (`coming`).
fn read_pic(p: Pane, url: String) {
    if p.pics.with_value(|v| v.iter().any(|(u, _)| *u == url)) {
        return;
    }
    p.pics.update_value(|v| v.push((url.clone(), None)));
    spawn_local(async move {
        let copy = read_decoded(&url).await;
        let Some(()) = p.pics.try_update_value(|v| {
            match (v.iter().position(|(u, _)| *u == url), &copy) {
                (Some(i), Some(c)) => v[i].1 = Some(c.clone()),
                (Some(i), None) => {
                    v.remove(i);
                }
                _ => {}
            }
            // The oldest go, once read: what's drawn from them stays drawn.
            while v.len() > PICS_KEPT && v.first().is_some_and(|(_, c)| c.is_some()) {
                if let (_, Some(old)) = v.remove(0) {
                    let _ = web_sys::Url::revoke_object_url(&old);
                }
            }
        }) else {
            return;
        };
        if p.coming.with_untracked(|c| c.as_ref().is_some_and(|(_, u)| *u == url)) {
            p.coming.set(None);
            if let Some(show) = p.pending.try_update_value(Option::take).flatten() {
                show();
            }
        }
    });
}

/// Reads the next picture after `e` in the pane ahead of time, so › shows it at once.
fn read_ahead(p: Pane, e: &Entry) {
    let next = p.entries.with_untracked(|es| {
        let same: Vec<&Entry> = es.iter().filter(|x| media(x) == Some(Media::Image)).collect();
        let i = same.iter().position(|x| x.path == e.path)?;
        same.get(i + 1).map(|&x| x.clone())
    });
    if let Some(next) = next {
        let src = p.src.get_untracked();
        read_pic(p, fit_url(src, &next).unwrap_or_else(|| stream_url(src, &next)));
    }
}

/// The picture at `url` (one the backend serves) as a copy in memory (an object URL), decoded,
/// so showing it doesn't hold up the window; None when it can't be read.
async fn read_decoded(url: &str) -> Option<String> {
    use wasm_bindgen_futures::JsFuture;
    let resp: web_sys::Response = JsFuture::from(window().fetch_with_str(url)).await.ok()?.dyn_into().ok()?;
    if !resp.ok() {
        return None;
    }
    let blob: web_sys::Blob = JsFuture::from(resp.blob().ok()?).await.ok()?.dyn_into().ok()?;
    let copy = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
    // Decoded now, off the window's way; a picture the web view can't decode is shown anyway.
    if let Ok(img) = web_sys::HtmlImageElement::new() {
        img.set_src(&copy);
        let _ = JsFuture::from(img.decode()).await;
    }
    Some(copy)
}

/// The copy in memory of the picture at `url`, when it's been read.
fn read_copy(p: Pane, url: &str) -> Option<String> {
    p.pics.with_value(|v| v.iter().find(|(u, _)| u == url).and_then(|(_, c)| c.clone()))
}

/// ‹ and › for the previous and next file of the same kind: `go` steps by -1 or 1.
fn neighbours(go: Callback<isize>) -> impl IntoView {
    view! {
        <span class="btn-group" role="group">
            <button class="btn" title="Previous (←)" aria-label="Previous" on:click=move |_| go.run(-1)>{ico(Icon::Prev)}</button>
            <button class="btn" title="Next (→)" aria-label="Next" on:click=move |_| go.run(1)>{ico(Icon::Next)}</button>
        </span>
    }
}

/// Extra scale for a quarter turn, so a `w` × `h` picture turned on its side fits where it was.
fn turn_fit(turn: i32, w: u32, h: u32) -> f64 {
    if turn.rem_euclid(2) == 0 || w == 0 || h == 0 {
        1.0
    } else {
        w.min(h) as f64 / w.max(h) as f64
    }
}

/// How a picture is zoomed when it's opened, picked by right-clicking the zoom level: all of it
/// in view, as wide as the preview, or at its own size. Kept for the next pictures.
#[derive(Clone, Copy, PartialEq)]
enum PicFit {
    Whole,
    Width,
    Actual,
}

const PIC_FIT_KEY: &str = "ferry-files.picture-fit";

impl PicFit {
    const ALL: [PicFit; 3] = [PicFit::Whole, PicFit::Width, PicFit::Actual];

    fn stored() -> Self {
        match stored(PIC_FIT_KEY).as_deref() {
            Some("width") => PicFit::Width,
            Some("actual") => PicFit::Actual,
            _ => PicFit::Whole,
        }
    }

    fn key(self) -> &'static str {
        match self {
            PicFit::Whole => "whole",
            PicFit::Width => "width",
            PicFit::Actual => "actual",
        }
    }

    fn label(self) -> &'static str {
        match self {
            PicFit::Whole => "Fit whole picture",
            PicFit::Width => "Fit width",
            PicFit::Actual => "Actual size",
        }
    }
}

/// The thumbnail's size, in pixels (the backend makes them this big at most).
const THUMB_PX: f64 = 320.0;

/// An EPUB's cover: its thumbnail, then the cover itself when the preview has room for more
/// pixels than the thumbnail has.
#[component]
fn BigCover(src: Src, entry: Entry) -> impl IntoView {
    let want = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let pic = NodeRef::<html::Div>::new();
    // The space the cover could take is the preview's, above the details.
    request_animation_frame(move || {
        let Some(room) = pic.try_get_untracked().flatten().and_then(|d| d.parent_element()) else {
            return;
        };
        let dpr = window().device_pixel_ratio().max(1.0);
        if (room.client_width().max(room.client_height()) as f64) * dpr > THUMB_PX {
            want.set(true);
        }
    });
    let p = uri_part(&entry.path);
    let url = crate::os::served(&format!("cover?src={}&p={p}&s={}&m={}", src.key(), entry.size, entry.mtime));
    view! {
        <div class="pic" class:whole=loaded node_ref=pic>
            {let e = entry.clone(); move || (!loaded.get()).then(|| view! { <Thumbnail src entry=e.clone()/> })}
            {move || want.get().then(|| view! {
                <img class="full" src=url.clone() alt="" draggable="false" class:pending=move || !loaded.get()
                    on:load=move |_| loaded.set(true)/>
            })}
        </div>
    }
}

/// The image itself, as big as the preview allows (never bigger than it is). Its thumbnail shows
/// while the whole file is read, which can take a while from a phone. It can be zoomed (buttons,
/// pinch, double-click), dragged when zoomed, and turned.
///
/// With `url`, a picture from somewhere else than the file (one in a ZIP): no thumbnail while it
/// loads, and no ‹ ›.
///
/// A big photo comes shrunk to about what the preview shows (`fit_url`), and the file itself
/// replaces it when it's zoomed in. Sizes and zoom levels are always of the photo's own pixels.
#[component]
fn FullImage(
    pane: Pane,
    src: Src,
    entry: Entry,
    info: RwSignal<MediaInfo>,
    turn: Turn,
    #[prop(optional)] url: Option<String>,
    #[prop(optional, into)] step: Option<Callback<isize>>,
) -> impl IntoView {
    let alone = url.is_some();
    // To the previous or next picture: in the pane's folder, or as `step` says (in a ZIP).
    let step = step.or_else(|| {
        let e = StoredValue::new(entry.clone());
        (!alone).then(|| Callback::new(move |d| step_media(pane, &e.get_value(), d)))
    });
    let loaded = RwSignal::new(false);
    // Until the picture first shows, it's zoomed and moved at once (not eased in from the middle).
    let settled = RwSignal::new(false);
    let natural = RwSignal::new((0u32, 0u32));
    let zoom = RwSignal::new(1.0f64);
    let pan = RwSignal::new((0.0f64, 0.0f64));
    // How the picture is zoomed until it's zoomed by hand (`manual`), and the list to change it
    // (where it is; None while closed).
    let fit_mode = RwSignal::new(PicFit::stored());
    let manual = StoredValue::new(false);
    let fit_menu = RwSignal::new(None::<(f64, f64)>);
    let stage = NodeRef::<html::Div>::new();
    let viewer = NodeRef::<html::Div>::new();
    let full_screen = RwSignal::new(false);
    let shrunk_url = if alone { None } else { fit_url(src, &entry) };
    let url = url.unwrap_or_else(|| stream_url(src, &entry));
    // Showing the shrunk photo; and how many of the photo's pixels each of its pixels stands for.
    let shrunk = RwSignal::new(shrunk_url.is_some());
    let first = shrunk_url.unwrap_or_else(|| url.clone());
    // Already read by ‹ › (`when_read`): shown from memory, with no thumbnail first.
    let copy = read_copy(pane, &first);
    let from_memory = copy.is_some();
    let ahead_of = StoredValue::new(entry.clone());
    let shown = RwSignal::new(copy.unwrap_or(first));
    let per_px = RwSignal::new(1.0f64);
    let full = StoredValue::new(url.clone());
    // The shown image's own pixels.
    let raw = RwSignal::new((0u32, 0u32));
    // The photo's size, from the shrunk one's and the size in the details (which may come later).
    Effect::new(move || {
        let (w, h) = raw.get();
        if w == 0 || !shrunk.get() {
            return;
        }
        let own = info.with(|i| i.width.zip(i.height).map(|(a, b)| a.max(b)));
        let k = own.map_or(1.0, |own| (own as f64 / w.max(h) as f64).max(1.0));
        per_px.set(k);
        natural.set(((w as f64 * k).round() as u32, (h as f64 * k).round() as u32));
    });
    // How big the picture shows when it fits, against its own size (at most 1): `zoom` is on top
    // of that, so the picture is at zoom × fit (× the turn's) of its pixels. Measured again when
    // the preview changes size.
    let fit = RwSignal::new(1.0f64);
    let img_ref = NodeRef::<html::Img>::new();
    let measure = move || {
        if let Some(img) = img_ref.try_get_untracked().flatten() {
            if img.natural_width() > 0 && img.offset_width() > 0 {
                fit.set(img.offset_width() as f64 / (img.natural_width() as f64 * per_px.get_untracked()));
            }
        }
    };
    // Again when the photo's size is known.
    Effect::new(move || {
        per_px.track();
        measure();
    });
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(img) = img_ref.get() else { return };
        let cb = Closure::<dyn Fn()>::new(measure);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&img);
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
    let load = move |ev: web_sys::Event| {
        if let Some(img) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlImageElement>().ok()) {
            let (w, h) = (img.natural_width(), img.natural_height());
            if shrunk.get_untracked() {
                raw.set((w, h)); // The effect above works out the photo's size.
            } else {
                per_px.set(1.0);
                natural.set((w, h));
                let _ = info.try_update(|i| {
                    if i.width.is_none() {
                        (i.width, i.height) = (Some(w), Some(h));
                    }
                });
            }
        }
        loaded.set(true);
        // The next picture in the pane is read meanwhile, for ›.
        if !alone {
            read_ahead(pane, &ahead_of.get_value());
        }
        // Measured, zoomed as `fit_mode` says and at its start, then zooms and turns are eased.
        request_animation_frame(move || {
            measure();
            request_animation_frame(move || {
                let _ = settled.try_set(true);
            });
        });
    };
    // How much of its own size the picture shows at `zoom` 1 (fitted), turn included.
    let base = move || {
        let (w, h) = natural.get();
        fit.get() * turn_fit(turn.get(), w, h)
    };
    // The picture's size on screen at zoom `z`, turn included, and the stage's.
    let sizes = move |z: f64| {
        let (Some(img), Some(st)) = (img_ref.try_get_untracked().flatten(), stage.try_get_untracked().flatten()) else { return None };
        let (w, h) = natural.get_untracked();
        let t = turn.get_untracked();
        let s = z * turn_fit(t, w, h);
        let (iw, ih) = (img.offset_width() as f64 * s, img.offset_height() as f64 * s);
        let shown = if t.rem_euclid(2) == 0 { (iw, ih) } else { (ih, iw) };
        Some((shown, (st.client_width() as f64, st.client_height() as f64)))
    };
    // How far the picture can be moved: until its edge reaches the stage's.
    let reach = move || sizes(zoom.get_untracked()).map_or((0.0, 0.0), |((w, h), (sw, sh))| (((w - sw) / 2.0).max(0.0), ((h - sh) / 2.0).max(0.0)));
    let move_to = move |(x, y): (f64, f64)| {
        let (rx, ry) = reach();
        pan.set((x.clamp(-rx, rx), y.clamp(-ry, ry)));
    };
    // Up to 800% of its pixels, and at least fitted.
    let set_zoom = move |z: f64| {
        let z = z.clamp(1.0, (8.0 / base()).max(1.0));
        zoom.set(z);
        // Zoomed in: the shrunk photo would blur, so the file itself takes its place (which
        // keeps showing until the file has loaded).
        if z > 1.0 && shrunk.get_untracked() {
            shrunk.set(false);
            shown.set(full.get_value());
        }
        move_to(pan.get_untracked());
    };
    let zoom_by_hand = move |z: f64| {
        manual.set_value(true);
        set_zoom(z);
    };
    // The zoom for `fit_mode`, applied again when the picture or the preview's size changes,
    // until the picture is zoomed by hand. A picture bigger than the preview starts at its top
    // left.
    let apply_fit = move |m: PicFit| {
        let z = match m {
            PicFit::Whole => 1.0,
            PicFit::Actual => 1.0 / base(),
            PicFit::Width => sizes(1.0).map_or(1.0, |((w, _), (sw, _))| if w > 0.0 { sw / w } else { 1.0 }),
        };
        set_zoom(z);
        move_to((f64::INFINITY, f64::INFINITY));
    };
    Effect::new(move || {
        let m = fit_mode.get();
        fit.track();
        natural.track();
        turn.get();
        if !manual.get_value() {
            apply_fit(m);
        }
    });
    let pick_fit = move |m: PicFit| {
        fit_menu.set(None);
        store(PIC_FIT_KEY, m.key());
        manual.set_value(false);
        fit_mode.set(m);
        apply_fit(m);
    };
    // Full screen: the picture and its buttons, on black.
    viewer.on_load(move |v| {
        let cb = Closure::<dyn Fn()>::new(move || {
            let on = super::video::full_screen_element()
                .is_some_and(|el| viewer.try_get_untracked().flatten().is_some_and(|v| wasm_bindgen::JsValue::from(v) == el));
            let _ = full_screen.try_set(on);
        });
        for name in ["fullscreenchange", "webkitfullscreenchange"] {
            let _ = v.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref());
        }
        cb.forget();
    });
    let toggle_full = move || {
        if full_screen.get_untracked() {
            super::video::exit_full_screen();
        } else if let Some(v) = viewer.get_untracked() {
            let _ = v.focus();
            super::video::enter_full_screen(&v.into());
        }
    };
    // Keys while the picture has the focus (it's been clicked, or it's full screen): ← → for the
    // previous and next, F for full screen.
    let keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.meta_key() || ev.ctrl_key() || ev.alt_key() || ev.repeat() && ev.code() == "KeyF" {
            return;
        }
        let done = match (ev.code().as_str(), step) {
            ("ArrowLeft", Some(go)) => {
                go.run(-1);
                true
            }
            ("ArrowRight", Some(go)) => {
                go.run(1);
                true
            }
            ("KeyF", _) => {
                toggle_full();
                true
            }
            _ => false,
        };
        if done {
            ev.prevent_default();
            ev.stop_propagation();
        }
    };
    let style = move || {
        let (x, y) = pan.get();
        let (w, h) = natural.get();
        let s = zoom.get() * turn_fit(turn.get(), w, h);
        let instant = if settled.get() { turn.css() } else { "; transition: none" };
        format!("transform: translate({x}px, {y}px) scale({s}) rotate({}deg){instant}", turn.get() * 90)
    };
    // Dragging a zoomed picture around.
    let listeners = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let stop = move || {
        if let Some((a, b)) = listeners.try_update_value(Option::take).flatten() {
            a.remove();
            b.remove();
        }
    };
    on_cleanup(stop);
    // Whether the last press moved the picture, so letting go on a side doesn't change pictures.
    let dragged = StoredValue::new(false);
    let down = move |ev: web_sys::MouseEvent| {
        dragged.set_value(false);
        if ev.button() != 0 || zoom.get_untracked() <= 1.0 {
            return;
        }
        ev.prevent_default();
        let (x0, y0) = (ev.client_x() as f64, ev.client_y() as f64);
        let (px, py) = pan.get_untracked();
        let moved = window_event_listener(ev::mousemove, move |e| {
            let (dx, dy) = (e.client_x() as f64 - x0, e.client_y() as f64 - y0);
            if dx.abs() + dy.abs() > 3.0 {
                dragged.set_value(true);
            }
            move_to((px + dx, py + dy));
        });
        let up = window_event_listener(ev::mouseup, move |_| stop());
        stop();
        listeners.set_value(Some((moved, up)));
    };
    // A pinch on the trackpad comes as a wheel event with Ctrl down. Otherwise scrolling moves a
    // zoomed picture (sideways with Shift, for a mouse wheel).
    let wheel = move |ev: web_sys::WheelEvent| {
        if ev.ctrl_key() || ev.meta_key() {
            ev.prevent_default();
            zoom_by_hand(zoom.get_untracked() * (-ev.delta_y() * 0.01).exp());
        } else if zoom.get_untracked() > 1.0 {
            ev.prevent_default();
            // In lines or pages rather than pixels on some mice.
            let k = match ev.delta_mode() {
                1 => 40.0,
                2 => 400.0,
                _ => 1.0,
            };
            let (mut dx, mut dy) = (ev.delta_x() * k, ev.delta_y() * k);
            if ev.shift_key() && dx == 0.0 {
                (dx, dy) = (dy, 0.0);
            }
            let (x, y) = pan.get_untracked();
            move_to((x - dx, y - dy));
        }
    };
    // Fitted, or its actual size (100%): whichever it isn't. A picture that fits at its actual
    // size goes to 200%.
    let fit_or_actual = move || {
        let actual = 1.0 / base();
        zoom_by_hand(if zoom.get_untracked() > 1.0 {
            1.0
        } else if actual > 1.001 {
            actual
        } else {
            2.0
        });
    };
    view! {
        <div class="picview" class:full-screen=full_screen node_ref=viewer tabindex="-1" on:keydown=keydown>
        <div class="pic stage" class:whole=loaded class:zoomed=move || { zoom.get() > 1.0 } node_ref=stage
            on:mousedown=down on:wheel=wheel
            on:dblclick=move |ev| { ev.stop_propagation(); fit_or_actual(); }>
            {let e = entry.clone(); move || (!loaded.get() && !alone && !from_memory).then(|| view! { <Thumbnail src entry=e.clone()/> })}
            <img class="full" node_ref=img_ref src=move || shown.get() alt="" draggable="false" class:pending=move || !loaded.get()
                style=style on:load=load on:transitionend=move |_| turn.settle()/>
            // Clicking the picture's left or right side goes to the previous or next one. Zoomed
            // in, a press there can also drag the picture, and then doesn't change pictures.
            {step.map(|go| {
                let edge = move |d: isize| {
                    let what = if d < 0 { "Previous (←)" } else { "Next (→)" };
                    view! {
                        <button class="pic-edge" class:next={d > 0} title=what aria-label=what
                            on:dblclick=|e: web_sys::MouseEvent| e.stop_propagation()
                            on:click=move |e: web_sys::MouseEvent| {
                                e.stop_propagation();
                                if !dragged.get_value() {
                                    go.run(d)
                                }
                            }>{ico(if d < 0 { Icon::Prev } else { Icon::Next })}</button>
                    }
                };
                view! { {edge(-1)} {edge(1)} }
            })}
        </div>
        <div class="viewer-bar">
            {step.map(|go| view! { {neighbours(go)} <span class="gap"></span> })}
            <span class="btn-group" role="group">
                <button class="btn" title="Zoom out" aria-label="Zoom out" on:click=move |_| zoom_by_hand(zoom.get_untracked() / 1.25)>{ico(Icon::ZoomOut)}</button>
                <button class="btn zoom" aria-haspopup="menu"
                    title="Of the picture's own size. Click (or double-click the picture) for 100% or to fit. Right-click to fit the whole picture, its width, or show its actual size."
                    on:click=move |_| fit_or_actual()
                    on:contextmenu=move |ev: web_sys::MouseEvent| {
                        ev.prevent_default();
                        ev.stop_propagation();
                        fit_menu.set(Some(super::menu::menu_at(&ev, 170.0, PicFit::ALL.len())));
                    }>
                    {move || format!("{:.0}%", zoom.get() * base() * 100.0)}
                </button>
                <button class="btn" title="Zoom in (or pinch)" aria-label="Zoom in" on:click=move |_| zoom_by_hand(zoom.get_untracked() * 1.25)>{ico(Icon::ZoomIn)}</button>
            </span>
            <span class="gap"></span>
            <span class="btn-group" role="group">
                <button class="btn" title="Turn left" aria-label="Turn left" on:click=move |_| turn.by(-1)>{ico(Icon::TurnLeft)}</button>
                <button class="btn" title="Turn right" aria-label="Turn right" on:click=move |_| turn.by(1)>{ico(Icon::TurnRight)}</button>
            </span>
            <span class="gap"></span>
            <button class="btn" title=move || if full_screen.get() { "Leave full screen (F or Esc)" } else { "Full screen (F)" }
                aria-label=move || if full_screen.get() { "Leave full screen" } else { "Full screen" }
                on:click=move |_| toggle_full()>{ico(Icon::FullScreen)}</button>
            {move || fit_menu.get().map(|(x, y)| {
                let now = fit_mode.get();
                view! {
                    <div class="menu-backdrop" on:click=move |_| fit_menu.set(None)
                        on:contextmenu=move |ev: web_sys::MouseEvent| { ev.prevent_default(); fit_menu.set(None) }></div>
                    <div class="menu open-as-list" role="menu" style=format!("left: {x}px; top: {y}px")
                        on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Escape" { ev.stop_propagation(); fit_menu.set(None) }>
                        {PicFit::ALL.into_iter().map(|m| view! {
                            <button role="menuitemradio" aria-checked=(now == m).to_string() class:on=now == m
                                on:click=move |_| pick_fit(m)>{m.label()}</button>
                        }).collect_view()}
                    </div>
                }
            })}
        </div>
        </div>
    }
}

/// A video or audio player streaming from the device (video.rs), with ‹ › under it. It takes
/// the pane's media keys, and goes on from where the file was stopped last time.
/// `alone`: no ‹ › (a file in a ZIP).
#[component]
fn Player(
    pane: Pane,
    entry: Entry,
    url: String,
    video: bool,
    info: RwSignal<MediaInfo>,
    turn: Turn,
    #[prop(optional)] alone: bool,
    #[prop(optional, into)] step: Option<Callback<isize>>,
) -> impl IntoView {
    let key = resume_key(&entry);
    // To the previous or next video or sound: in the pane's folder, or as `step` says (in a ZIP).
    let step = step.or_else(|| {
        let e = StoredValue::new(entry.clone());
        (!alone).then(|| Callback::new(move |d| step_media(pane, &e.get_value(), d)))
    });
    let player = if video {
        let pop = Callback::new(|video: Popped| {
            spawn_local(async move {
                if let Err(e) = call::<()>("pop_out", json!({ "video": video })).await {
                    web_sys::console::error_1(&e.into());
                }
            });
        });
        let near = (!alone).then(|| (pane.src.get_untracked(), entry.path.clone()));
        view! { <VideoPlayer url key title=entry.name.clone() info turn pane=pane.id pop near/> }.into_any()
    } else {
        view! { <SoundPlayer url key info pane=pane.id/> }.into_any()
    };
    view! {
        {player}
        {step.map(|go| view! { <div class="viewer-bar">{neighbours(go)}</div> })}
    }
}

/// A font's letters at a few sizes, loaded from the file itself.
#[component]
fn FontSample(src: Src, entry: Entry) -> impl IntoView {
    // A name of its own for each preview, so a font shown earlier isn't reused.
    let family = format!("ferry-peek-{}", js_sys::Date::now() as u64);
    let css = format!("@font-face {{ font-family: \"{family}\"; src: url(\"{}\"); }}", stream_url(src, &entry));
    let face = format!("font-family: \"{family}\", system-ui");
    view! {
        <style>{css}</style>
        <div class="font-sample" style=face>
            <div class="abc">"ABCDEFGHIJKLMNOPQRSTUVWXYZ"<br/>"abcdefghijklmnopqrstuvwxyz"<br/>"0123456789 !?&@#%(){}"</div>
            {[12, 16, 24, 36, 56].map(|px| view! {
                <div class="line" style=format!("font-size: {px}px")>
                    <span class="px">{format!("{px}")}</span>
                    "The quick brown fox jumps over the lazy dog"
                </div>
            })}
        </div>
    }
}

/// A row of a ZIP's tree: its full name in the ZIP (folders end in /), its name, how deep it is,
/// and its unpacked size (a folder's: all that's in it).
#[derive(Clone, Debug, PartialEq)]
struct ZipRow {
    path: String,
    name: String,
    depth: usize,
    dir: bool,
    size: u64,
}

/// A ZIP's items as a tree, in the order shown: each folder before what's in it, folders first,
/// then by name. Folders are also made up from the files' names, since a ZIP needn't list them.
fn zip_tree(items: &[Packed]) -> Vec<ZipRow> {
    #[derive(Default)]
    struct Node {
        dir: bool,
        size: u64,
        kids: std::collections::BTreeMap<String, Node>,
    }
    let mut top = Node { dir: true, ..Default::default() };
    for i in items {
        let parts: Vec<&str> = i.name.split('/').filter(|s| !s.is_empty()).collect();
        let Some((last, dirs)) = parts.split_last() else {
            continue;
        };
        let mut at = &mut top;
        for d in dirs {
            at = at.kids.entry((*d).to_string()).or_default();
            at.dir = true;
        }
        let node = at.kids.entry((*last).to_string()).or_default();
        node.dir |= i.dir || i.name.ends_with('/');
        if !node.dir {
            node.size = i.size;
        }
    }
    fn total(n: &mut Node) -> u64 {
        if n.dir {
            n.size = n.kids.values_mut().map(total).sum();
        }
        n.size
    }
    total(&mut top);
    fn walk(n: &Node, prefix: &str, depth: usize, out: &mut Vec<ZipRow>) {
        let mut kids: Vec<(&String, &Node)> = n.kids.iter().collect();
        kids.sort_by_key(|(name, k)| (!k.dir, name.to_lowercase()));
        for (name, k) in kids {
            let path = format!("{prefix}{name}{}", if k.dir { "/" } else { "" });
            out.push(ZipRow { path: path.clone(), name: name.clone(), depth, dir: k.dir, size: k.size });
            if k.dir {
                walk(k, &path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&top, "", 0, &mut out);
    out
}

/// The folders `path` is in, in a ZIP: "a/b/c" is in "a/" and "a/b/".
fn zip_parents(path: &str) -> impl Iterator<Item = &str> {
    let path = path.strip_suffix('/').unwrap_or(path);
    path.match_indices('/').map(move |(at, _)| &path[..=at])
}

/// What's in a ZIP (or APK, JAR, CBZ), as a tree whose folders open and close, read from the end
/// of the file.
#[component]
fn ZipList(src: Src, entry: Entry) -> impl IntoView {
    let got = RwSignal::new(None::<Result<Listing, String>>);
    let packed = entry.size;
    let args = json!({ "src": src.key(), "path": entry.path, "size": entry.size });
    spawn_local(async move {
        let r = call::<Listing>("zip_list", args).await;
        let _ = got.try_set(Some(r));
    });
    // The folders open. A ZIP whose all is in one folder opens with it open.
    let open = RwSignal::new(HashSet::<String>::new());
    view! {
        <div class="zip">
            {move || match got.get() {
                None => view! { <span class="spin"></span> }.into_any(),
                Some(Err(e)) => view! { <span class="hint">{e}</span> }.into_any(),
                Some(Ok(l)) => {
                    let unpacked: u64 = l.items.iter().map(|i| i.size).sum();
                    let shown = l.items.len() as u64;
                    let rows = zip_tree(&l.items);
                    let tops: Vec<&ZipRow> = rows.iter().filter(|r| r.depth == 0).collect();
                    if let [one] = tops.as_slice() {
                        if one.dir {
                            let x = one.path.clone();
                            open.update_untracked(|o| { o.insert(x); });
                        }
                    }
                    view! {
                        <span class="sub">{format!("{} {} · {} packed, {} unpacked", l.total, if l.total == 1 { "item" } else { "items" }, human_size(packed), human_size(unpacked))}</span>
                        <div class="zip-list">
                            <table>
                                <tbody>
                                    {rows.into_iter().map(|r| {
                                        let path = StoredValue::new(r.path.clone());
                                        let hidden = move || path.with_value(|x| open.with(|o| !zip_parents(x).all(|d| o.contains(d))));
                                        let is_open = Signal::derive(move || path.with_value(|x| open.with(|o| o.contains(x))));
                                        let toggle = move |_| {
                                            let x = path.get_value();
                                            open.update(|o| if !o.remove(&x) { o.insert(x); });
                                        };
                                        view! {
                                            <tr class:dir=r.dir hidden=hidden on:click=move |ev| if r.dir { toggle(ev) }
                                                aria-expanded=r.dir.then(|| move || is_open.get().to_string())>
                                                <td class="n" style=format!("--depth: {}", r.depth) title=r.path.clone()>
                                                    <span class="twisty">{r.dir.then(|| chev(is_open))}</span>
                                                    {r.name.clone()}
                                                </td>
                                                <td class="s">{human_size(r.size)}</td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
                        </div>
                        {(shown < l.total).then(|| view! { <span class="more">{format!("Only the first {shown} are listed.")}</span> })}
                        {(l.total == 0).then(|| view! { <span class="hint">"Empty"</span> })}
                    }.into_any()
                }
            }}
        </div>
    }
}

/// How much of a file the text view loads, at most, as it's scrolled.
const TEXT_CAP: u64 = 8 * 1024 * 1024;
/// Code is coloured this far into a file, at most (as the backend's `highlight::COLOUR_MAX`).
const COLOUR_MAX: u64 = 2 * 1024 * 1024;

/// A piece of a file as shown: its text, and colours for code as (kind, bytes) runs over it.
#[derive(Clone, Default)]
struct Piece {
    text: String,
    runs: Option<Vec<(u8, u32)>>,
    /// Colours stopped in this piece: the rest is plain.
    stopped: bool,
    /// The number of the piece's first line (from 1).
    line: usize,
    /// The first line goes on from the last piece (a line longer than a piece): it has no number.
    cont: bool,
}

impl Piece {
    /// Where the piece after this one starts counting lines.
    fn next_line(&self) -> (usize, bool) {
        (self.line + self.text.matches('\n').count(), !self.text.is_empty() && !self.text.ends_with('\n'))
    }
}

/// The class of each kind of run (`highlight::Kind` in the backend); plain text has none.
const TOKEN_CLASSES: [&str; 17] = [
    "",
    "tok-comment",
    "tok-string",
    "tok-number",
    "tok-keyword",
    "tok-type",
    "tok-function",
    "tok-tag",
    "tok-attr",
    "tok-heading",
    "tok-link",
    "tok-url",
    "tok-bold",
    "tok-italic",
    "tok-quote",
    "tok-list",
    "tok-code",
];

/// The piece as (class, text) runs; plain when the runs don't fit the text.
fn segments(p: &Piece) -> Vec<(&'static str, &str)> {
    let plain = || vec![("", p.text.as_str())];
    let Some(runs) = &p.runs else { return plain() };
    let mut at = 0;
    let mut out = Vec::with_capacity(runs.len() + 1);
    for &(kind, len) in runs {
        let end = (at + len as usize).min(p.text.len());
        let Some(s) = p.text.get(at..end) else {
            return plain();
        };
        out.push((TOKEN_CLASSES.get(kind as usize).copied().unwrap_or(""), s));
        at = end;
    }
    if let Some(rest) = p.text.get(at..).filter(|r| !r.is_empty()) {
        out.push(("", rest));
    }
    out
}

fn segment_view(class: &'static str, s: &str) -> AnyView {
    if class.is_empty() {
        s.to_string().into_any()
    } else {
        view! { <span class=class>{s.to_string()}</span> }.into_any()
    }
}

/// A piece's text, with a `<span>` per coloured run. The text is only ever text, never HTML.
/// `lines`: each line in a `<span class="ln">` whose number CSS shows (and copying leaves out).
fn piece_text(p: &Piece, lines: bool) -> AnyView {
    let segs = segments(p);
    if !lines {
        return segs.into_iter().map(|(c, s)| segment_view(c, s)).collect_view().into_any();
    }
    let mut out = vec![];
    let mut line = vec![];
    for (class, s) in segs {
        let mut rest = s;
        while let Some(i) = rest.find('\n') {
            line.push(segment_view(class, &rest[..=i]));
            out.push(std::mem::take(&mut line));
            rest = &rest[i + 1..];
        }
        if !rest.is_empty() {
            line.push(segment_view(class, rest));
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out.into_iter()
        .enumerate()
        .map(|(k, segs)| {
            let n = if k == 0 && p.cont { String::new() } else { (p.line + k).to_string() };
            view! { <span class="ln" data-n=n>{segs.collect_view()}</span> }
        })
        .collect_view()
        .into_any()
}

/// Pieces of a file loaded one after another as the end is scrolled near, up to `cap` bytes, and
/// shown (`piece_view`). `get(start)` fetches the piece at `start` and says where the next one
/// starts.
fn pieces<F, Fut>(get: F, cap: u64) -> impl IntoView
where
    F: Fn(u64) -> Fut + Copy + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(Piece, Option<u64>), String>> + 'static,
{
    let pieces = RwSignal::new(Vec::<Piece>::new());
    // Where the next piece starts; None once the whole file is in.
    let next = RwSignal::new(None::<u64>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let fetch = move |start: u64| {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        spawn_local(async move {
            // The preview may show something else by now: then these are gone and nothing is set.
            match get(start).await {
                Ok((mut text, n)) => {
                    let numbered = pieces.try_update(|v| {
                        (text.line, text.cont) = v.last().map_or((1, false), Piece::next_line);
                        v.push(text)
                    });
                    if numbered.is_some() {
                        next.set(n);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            let _ = loading.try_set(false);
        });
    };
    fetch(0);
    let scrolled = move |ev: web_sys::Event| {
        let Some(el) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else {
            return;
        };
        let left = el.scroll_height() - el.scroll_top() - el.client_height();
        match next.get_untracked() {
            Some(n) if n < cap && left < 600 && error.get_untracked().is_none() => fetch(n),
            _ => {}
        }
    };
    // After a failed read, a button reads it again: the start, or the piece after the last.
    let retry = move || {
        error.set(None);
        fetch(if pieces.with_untracked(Vec::is_empty) { 0 } else { next.get_untracked().unwrap_or(0) });
    };
    piece_view(pieces, next, error, loading, scrolled, retry, cap)
}

/// What failed to load, and a button to load it again: "Try again" when nothing is shown yet,
/// else "Load more".
fn retry_line(e: String, nothing_yet: bool, again: impl Fn() + 'static) -> impl IntoView {
    view! {
        <div class="peek-retry">
            <span class="more">{e}</span>
            <button class="btn small" on:click=move |_| again()>{if nothing_yet { "Try again" } else { "Load more" }}</button>
        </div>
    }
}

/// The wrap and line-number toggles for text, at the end of the line above the preview.
fn text_tools() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let (wrap, numbers) = (ctx.wrap_text, ctx.line_numbers);
    view! {
        <div class="meta-tools">
            <button class="filter-opt" class:on=move || wrap.get() aria-pressed=move || wrap.get().to_string()
                aria-label="Wrap long lines" title=move || if wrap.get() { "Wrap long lines: on" } else { "Wrap long lines: off" }
                on:click=move |_| ctx.set_wrap_text(!wrap.get_untracked())>{ico(Icon::Wrap)}</button>
            <button class="filter-opt" class:on=move || numbers.get() aria-pressed=move || numbers.get().to_string()
                aria-label="Line numbers" title=move || if numbers.get() { "Line numbers: on" } else { "Line numbers: off" }
                on:click=move |_| ctx.set_line_numbers(!numbers.get_untracked())>{ico(Icon::LineNumbers)}</button>
        </div>
    }
}

/// The pieces in a `<pre>`, with a line under it saying what's going on.
fn piece_view(
    pieces: RwSignal<Vec<Piece>>,
    next: RwSignal<Option<u64>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    scrolled: impl Fn(web_sys::Event) + Copy + Send + Sync + 'static,
    retry: impl Fn() + Copy + Send + Sync + 'static,
    cap: u64,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let empty = move || pieces.with(|v| v.len() == 1 && v[0].text.trim().is_empty()) && next.get().is_none();
    let stopped = move || pieces.with(|v| v.iter().any(|p| p.stopped));
    // Room for the longest line number so far.
    let digits = move || pieces.with(|v| v.last().map_or(1, |p| p.next_line().0.max(1).to_string().len()));
    let (wrap, numbers) = (ctx.wrap_text, ctx.line_numbers);
    view! {
        <div class="peek">
            {move || if pieces.with(Vec::is_empty) {
                match error.get() {
                    Some(e) => retry_line(e, true, retry).into_any(),
                    None => view! { <span class="spin"></span> }.into_any(),
                }
            } else if empty() {
                view! { <span class="hint">"Empty"</span> }.into_any()
            } else {
                view! {
                    <pre on:scroll=scrolled class:nowrap=move || !wrap.get() class:numbered=move || numbers.get()
                        style:--ln-w=move || format!("{}ch", digits())>
                        <span class="lines">
                            <For each=move || 0..pieces.with(Vec::len) key=|i| *i
                                children=move |i| pieces.with_untracked(|v| piece_text(&v[i], true))/>
                        </span>
                    </pre>
                    {move || stopped().then(|| view! { <span class="more">{format!("Colours stop partway (after {}, or where colouring got slow); the rest is plain.", human_size(COLOUR_MAX))}</span> })}
                    {move || if let Some(e) = error.get() {
                        retry_line(e, false, retry).into_any()
                    } else if next.get().is_some_and(|n| n >= cap) {
                        view! { <span class="more">{format!("Only the first {} is shown.", human_size(cap))}</span> }.into_any()
                    } else if loading.get() {
                        view! { <span class="more">"Loading more…"</span> }.into_any()
                    } else {
                        ().into_any()
                    }}
                }.into_any()
            }}
        </div>
    }
}

/// A file packed in a ZIP: its name in it, and the ZIP's path and size.
#[derive(Clone)]
struct Inner {
    name: String,
    zip_path: String,
    zip_size: u64,
}

/// Files in a ZIP are unpacked whole for the preview, up to this size (as the backend's).
const UNPACK_MAX: u64 = 64 * 1024 * 1024;

/// A file picked in a ZIP's columns, `size` bytes unpacked: shown like any other file, from the
/// ZIP unpacked, or read in place when it's `stored` without compression.
#[component]
fn Zipped(pane: Pane, src: Src, zip: Entry, name: String, size: u64, stored: bool) -> impl IntoView {
    let p = pane;
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    let e = Entry { name: short.clone(), path: name.clone(), dir: false, size, mtime: 0, hidden: false };
    let url = super::views::zipped_url(src, &zip, &name);
    // An EPUB in the ZIP shows its cover.
    let cover = crate::os::served(&format!("cover?src={}&p={}&s={}&m={}&e={}", src.key(), uri_part(&zip.path), zip.size, zip.mtime, uri_part(&name),));
    let inner = Inner { name: name.clone(), zip_path: zip.path.clone(), zip_size: zip.size };
    let (pic_turn, video_turn) = (Turn::new(), Turn::new());
    let info = RwSignal::new(MediaInfo::default());
    let forced = RwSignal::new(None::<PeekAs>);
    // ‹ › and the picture's sides go through the pictures (or videos, or sounds) in its folder.
    let step = Callback::new(move |d| super::views::step_zipped(p, d));
    // Whether the body is text (as the match below picks it), for the wrap and line-number toggles.
    let text_e = e.clone();
    let body = move || {
        let (e, url, inner, cover) = (e.clone(), url.clone(), inner.clone(), cover.clone());
        if size > UNPACK_MAX && !stored {
            return view! { <span class="hint">"Too big to show from inside the ZIP."</span> }.into_any();
        }
        match (forced.get(), media(&e)) {
            (Some(PeekAs::Text), _) => view! { <TextPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Hex), _) => view! { <HexPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Image), _) => view! { <FullImage pane=p src entry=e info turn=pic_turn url step/> }.into_any(),
            (Some(PeekAs::Video), _) => view! { <Player pane=p entry=e url video=true info turn=video_turn alone=true step/> }.into_any(),
            // A ZIP in a ZIP isn't listed.
            (Some(PeekAs::Zip), _) => view! { <span class="hint">"Not shown from inside a ZIP."</span> }.into_any(),
            (None, Some(Media::Image)) => view! { <FullImage pane=p src entry=e info turn=pic_turn url step/> }.into_any(),
            (None, Some(Media::Video)) => view! { <Player pane=p entry=e url video=true info turn=video_turn alone=true step/> }.into_any(),
            (None, Some(Media::Audio)) => view! { <Player pane=p entry=e url video=false info turn=video_turn alone=true step/> }.into_any(),
            _ if has_ext(&e, PDF_EXTS) => view! { <PdfView pane=p src entry=e url inner/> }.into_any(),
            _ if has_ext(&e, &["epub"]) => view! { <ZippedCover url=cover/> }.into_any(),
            _ if is_text(&e) => view! { <TextPeek src entry=e inner/> }.into_any(),
            _ => view! {
                <div class="viewer-bar">
                    <OpenAs pick=move |h| forced.set(Some(h))/>
                </div>
            }
            .into_any(),
        }
    };
    let text = Memo::new(move |_| {
        if size > UNPACK_MAX && !stored {
            return false;
        }
        match forced.get() {
            Some(how) => how == PeekAs::Text,
            None => media(&text_e).is_none() && !has_ext(&text_e, PDF_EXTS) && !has_ext(&text_e, &["epub"]) && is_text(&text_e),
        }
    });
    let folder = match name.rfind('/') {
        Some(at) => format!("{}/{}", zip.name, &name[..at]),
        None => zip.name.clone(),
    };
    let measures = Signal::derive(move || measures(Some(human_size(size)), info));
    let facts = Signal::derive(move || format!("In a ZIP · {folder}"));
    view! {
        <div class="meta">
            <div class="info">
                <span class="head">
                    <b class="name" title=format!("{}/{name}", zip.path)>{short.clone()}</b>
                    <span class="measures">{move || measures.get()}</span>
                </span>
                <span class="sub" title=move || facts.get()>{move || facts.get()}</span>
                {move || forced.get().map(|now| peek_as_buttons(now, false, move |how| forced.set(how)))}
            </div>
            {move || text.get().then(text_tools)}
        </div>
        <div class="view">{body}</div>
    }
}

/// A text file, a piece at a time: the next piece loads when the end is scrolled near. Code is
/// coloured unless that's turned off in Settings.
#[component]
fn TextPeek(src: Src, entry: Entry, #[prop(optional)] inner: Option<Inner>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // A file packed in a ZIP is read from the ZIP.
    let path = StoredValue::new(inner.as_ref().map_or(entry.path, |i| i.zip_path.clone()));
    let inner = StoredValue::new(inner);
    move || {
        let colour = ctx.colour_code.get();
        let get = move |start: u64| async move {
            let path = path.try_get_value().unwrap_or_default();
            let p = call::<Peek>("peek_text", json!({ "src": src.key(), "path": path, "start": start, "inner": inner.try_get_value().flatten().map(|i| i.name), "zip_size": inner.try_get_value().flatten().map(|i| i.zip_size), "colour": colour })).await?;
            Ok((Piece { text: p.text, runs: p.runs, stopped: p.colours_stopped, ..Piece::default() }, p.next))
        };
        pieces(get, TEXT_CAP)
    }
}

/// A row of the hex view, in px (as `.peek.hex pre` in views.css), and the room above the rows.
const ROW_H: f64 = 16.0;
const ROW_PAD: f64 = 4.0;
/// The tallest the scrolled space gets: past it, the scroll bar stands for the file in proportion.
const SPACE_MAX: f64 = 4_000_000.0;
/// The piece of a file read at a time (as the backend's `PEEK_MAX`), and how many are kept.
const CHUNK: u64 = 64 * 1024;
const CHUNKS_KEPT: usize = 16;

/// Where the hex view's rows are: how many the file has and fit, and the scrolled space.
#[derive(Clone, Copy, Debug, PartialEq)]
struct HexGeom {
    rows: u64,
    fit: u64,
    /// The last row that can be at the top.
    last: u64,
    space: f64,
    /// The furthest it scrolls, in px.
    max_px: f64,
}

impl HexGeom {
    fn new(size: u64, view_h: f64) -> Self {
        let rows = size.div_ceil(16);
        let fit = (((view_h - ROW_PAD) / ROW_H).floor() as u64).max(1);
        let space = (rows as f64 * ROW_H + ROW_PAD).min(SPACE_MAX).max(view_h);
        HexGeom { rows, fit, last: rows.saturating_sub(fit), space, max_px: (space - view_h).max(0.0) }
    }
    /// The row at the top when scrolled `px`.
    fn row_at(&self, px: f64) -> u64 {
        if self.max_px <= 0.0 || self.last == 0 {
            return 0;
        }
        ((px / self.max_px).clamp(0.0, 1.0) * self.last as f64).round() as u64
    }
    /// How far it's scrolled with `row` at the top.
    fn px_of(&self, row: u64) -> f64 {
        if self.last == 0 {
            return 0.0;
        }
        row.min(self.last) as f64 / self.last as f64 * self.max_px
    }
}

/// An offset typed to go to: hex (with 0x, or with a letter in it) or a number.
fn parse_offset(s: &str) -> Option<u64> {
    let s = s.trim().replace(['_', ' '], "");
    let lower = s.to_lowercase();
    if let Some(h) = lower.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()
    } else if lower.chars().all(|c| c.is_ascii_digit()) {
        lower.parse().ok()
    } else {
        u64::from_str_radix(&lower, 16).ok()
    }
}

/// Any file's bytes, 16 to a row: where, in hex, and as text. Only the rows in sight are drawn,
/// and only the pieces of the file under them read, wherever it's scrolled to.
#[component]
fn HexPeek(src: Src, entry: Entry, #[prop(optional)] inner: Option<Inner>) -> impl IntoView {
    // A file packed in a ZIP is read from the ZIP.
    let path = StoredValue::new(inner.as_ref().map_or(entry.path, |i| i.zip_path.clone()));
    let inner = StoredValue::new(inner);
    // The file's size; made smaller when it ends sooner.
    let size = RwSignal::new(entry.size);
    // The row at the top, and the height of the rows' box.
    let top = RwSignal::new(0u64);
    let view_h = RwSignal::new(0.0f64);
    // The pieces read, by number, and those being read.
    let chunks = RwSignal::new(HashMap::<u64, Vec<u8>>::new());
    let asked = RwSignal::new(HashSet::<u64>::new());
    let error = RwSignal::new(None::<String>);
    let geom = Memo::new(move |_| HexGeom::new(size.get(), view_h.get()));
    let fetch = move |k: u64| {
        asked.update(|a| {
            a.insert(k);
        });
        spawn_local(async move {
            let start = k * CHUNK;
            let path = path.try_get_value().unwrap_or_default();
            let i = inner.try_get_value().flatten();
            let args =
                json!({ "src": src.key(), "path": path, "start": start, "inner": i.as_ref().map(|i| i.name.clone()), "zip_size": i.map(|i| i.zip_size) });
            // The preview may show something else by now: then these are gone and nothing is set.
            match call::<Bytes>("peek_bytes", args).await {
                Ok(b) => {
                    let end = start + b.bytes.len() as u64;
                    if b.next.is_none() && size.try_get_untracked().is_some_and(|s| s > end) {
                        let _ = size.try_set(end);
                    }
                    let _ = chunks.try_update(|c| {
                        c.insert(k, b.bytes);
                        // The furthest from what's shown goes.
                        if c.len() > CHUNKS_KEPT {
                            let at = top.get_untracked() * 16 / CHUNK;
                            if let Some(far) = c.keys().copied().max_by_key(|x| x.abs_diff(at)) {
                                c.remove(&far);
                            }
                        }
                    });
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            let _ = asked.try_update(|a| a.remove(&k));
        });
    };
    // The pieces under the rows in sight are read, two at a time: scrolled fast, the ones passed
    // aren't.
    Effect::new(move |_| {
        if error.with(Option::is_some) {
            return;
        }
        let (t, g, s) = (top.get(), geom.get(), size.get());
        let first = t * 16 / CHUNK;
        let last = (((t + g.fit + 1) * 16).min(s.max(1)) - 1) / CHUNK;
        for k in first..=last.max(first) {
            if asked.with(HashSet::len) >= 2 {
                break;
            }
            if !chunks.with(|c| c.contains_key(&k)) && !asked.with(|a| a.contains(&k)) {
                fetch(k);
            }
        }
    });
    let scroller = NodeRef::<html::Div>::new();
    // How far it was scrolled to show `top`: a scroll there isn't the user's.
    let synced = StoredValue::new(0.0f64);
    let go = move |row: u64| {
        let g = geom.get_untracked();
        let row = row.min(g.last);
        top.set(row);
        if let Some(el) = scroller.get_untracked() {
            let px = g.px_of(row).round();
            synced.set_value(px);
            el.set_scroll_top(px as i32);
        }
    };
    let scrolled = move |_| {
        let Some(el) = scroller.get_untracked() else { return };
        let px = el.scroll_top() as f64;
        if (px - synced.get_value()).abs() >= 1.5 {
            synced.set_value(px);
            top.set(geom.get_untracked().row_at(px));
        }
    };
    // The wheel moves by rows (a part of one kept for the next turn).
    let spare = StoredValue::new(0.0f64);
    let wheel = move |ev: web_sys::WheelEvent| {
        let (dy, dx) = (ev.delta_y(), ev.delta_x());
        if dy == 0.0 || dx.abs() > dy.abs() {
            return;
        }
        ev.prevent_default();
        let rows = match ev.delta_mode() {
            1 => dy,
            2 => dy * geom.get_untracked().fit as f64,
            _ => dy / ROW_H,
        } + spare.get_value();
        spare.set_value(rows.fract());
        let whole = rows.trunc() as i64;
        if whole != 0 {
            go(top.get_untracked().saturating_add_signed(whole));
        }
    };
    let keys = move |ev: web_sys::KeyboardEvent| {
        let (t, fit, last) = (top.get_untracked(), geom.get_untracked().fit, geom.get_untracked().last);
        let to = match ev.key().as_str() {
            "ArrowDown" => t + 1,
            "ArrowUp" => t.saturating_sub(1),
            "PageDown" | " " => t + fit,
            "PageUp" => t.saturating_sub(fit),
            "Home" => 0,
            "End" => last,
            _ => return,
        };
        // Not the pane's: arrows there move the selection.
        ev.prevent_default();
        ev.stop_propagation();
        go(to);
    };
    let goto = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" {
            if let Some(n) = parse_offset(&event_target_value(&ev)) {
                go(n / 16);
            }
        }
    };
    // The box's height, as it's resized.
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(el) = scroller.get() else { return };
        let measure = {
            let el = el.clone();
            move || {
                let _ = view_h.try_set(el.client_height() as f64);
            }
        };
        measure();
        let cb = Closure::<dyn Fn()>::new(measure);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&el);
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
    // Kept in place when the box is resized or the file found shorter.
    Effect::new(move |_| {
        let g = geom.get();
        if top.get_untracked() > g.last {
            go(g.last);
        }
    });
    let text = move || {
        let (t, g, s) = (top.get(), geom.get(), size.get());
        let width = hex_width(s);
        chunks.with(|c| {
            let mut out = String::new();
            for row in t..(t + g.fit + 1).min(g.rows) {
                let at = row * 16;
                let (k, off) = (at / CHUNK, (at % CHUNK) as usize);
                match c.get(&k).filter(|b| off < b.len()) {
                    Some(b) => out.push_str(&hex_lines(at, &b[off..(off + 16).min(b.len())], width)),
                    None => {
                        out.push_str(&format!("{at:0width$x}\n"));
                    }
                }
            }
            out
        })
    };
    let at = move || {
        let (t, s) = (top.get(), size.get());
        format!("{:x} of {}", t * 16, human_size(s))
    };
    let empty = move || size.get() == 0 && chunks.with(|c| c.contains_key(&0));
    view! {
        <div class="peek hex">
            <div class="peek-tools">
                <span class="hex-at" title="The offset of the top row, in hex">{at}</span>
                <input class="hex-go" type="text" spellcheck="false" placeholder="Go to offset" aria-label="Go to offset"
                    title="An offset in hex (0x1f0) or a number; Enter goes there" on:keydown=goto/>
            </div>
            <div class="hex-scroll" node_ref=scroller tabindex="0" hidden=empty
                on:scroll=scrolled on:wheel=wheel on:keydown=keys>
                <div class="hex-space" style:height=move || format!("{}px", geom.get().space)>
                    <pre style:height=move || format!("{}px", view_h.get())>{text}</pre>
                </div>
            </div>
            {move || empty().then(|| view! { <span class="hint">"Empty"</span> })}
            {move || error.get().map(|e| retry_line(e, chunks.with(HashMap::is_empty), move || error.set(None)))}
        </div>
    }
}

/// Hex digits for any offset in a file of `size` bytes: 8, or more for one past 4 GB.
fn hex_width(size: u64) -> usize {
    let top = size.saturating_sub(1);
    (16 - top.leading_zeros() as usize / 4).max(8)
}

/// "00000010  48 65 6c 6c 6f 20 77 6f  72 6c 64 0a 00 00 00 00  Hello world....." per 16 bytes,
/// the offset `width` digits.
fn hex_lines(start: u64, bytes: &[u8], width: usize) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 5);
    for (n, row) in bytes.chunks(16).enumerate() {
        let _ = write!(out, "{:0width$x}  ", start + n as u64 * 16);
        for i in 0..16 {
            match row.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        out.extend(row.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_rows_map_to_the_scroll_bar() {
        // Small: one px a px, the last rows at the bottom.
        let g = HexGeom::new(16 * 1000, 404.0);
        assert_eq!((g.rows, g.fit, g.last), (1000, 25, 975));
        assert_eq!(g.row_at(0.0), 0);
        assert_eq!(g.row_at(g.max_px), 975);
        assert_eq!(g.row_at(g.px_of(500)), 500);
        // Huge: the space stops at SPACE_MAX, and the bar goes in proportion.
        let g = HexGeom::new(10 << 30, 404.0);
        assert_eq!(g.space, SPACE_MAX);
        assert_eq!(g.row_at(g.max_px), g.last);
        assert_eq!(g.row_at(g.max_px / 2.0), g.last / 2 + g.last % 2);
        // Smaller than the box: nothing to scroll.
        let g = HexGeom::new(20, 404.0);
        assert_eq!((g.rows, g.last, g.max_px), (2, 0, 0.0));
        assert_eq!(g.row_at(100.0), 0);
    }

    #[test]
    fn offsets_and_their_width() {
        assert_eq!(parse_offset("0x1F0"), Some(0x1f0));
        assert_eq!(parse_offset(" 496 "), Some(496));
        assert_eq!(parse_offset("ff"), Some(255));
        assert_eq!(parse_offset("1_000"), Some(1000));
        assert_eq!(parse_offset("zz"), None);
        assert_eq!(hex_width(0), 8);
        assert_eq!(hex_width(1 << 32), 8);
        assert_eq!(hex_width((1 << 32) + 1), 9);
        assert_eq!(&hex_lines(0x10, b"Hi", 8)[..12], "00000010  48");
    }

    fn packed(name: &str, size: u64) -> Packed {
        Packed { name: name.into(), size, dir: name.ends_with('/'), stored: false }
    }

    #[test]
    fn zip_tree_nests_and_sorts() {
        let items = [packed("b.txt", 1), packed("a/z.txt", 2), packed("a/c/d.txt", 3), packed("A2/", 0)];
        let rows: Vec<(String, usize, bool, u64)> = zip_tree(&items).into_iter().map(|r| (r.path, r.depth, r.dir, r.size)).collect();
        assert_eq!(
            rows,
            [
                ("a/".into(), 0, true, 5),
                ("a/c/".into(), 1, true, 3),
                ("a/c/d.txt".into(), 2, false, 3),
                ("a/z.txt".into(), 1, false, 2),
                ("A2/".into(), 0, true, 0),
                ("b.txt".into(), 0, false, 1),
            ]
        );
    }

    #[test]
    fn zip_parents_lists_folders() {
        assert_eq!(zip_parents("a/b/c.txt").collect::<Vec<_>>(), ["a/", "a/b/"]);
        assert_eq!(zip_parents("a/b/").collect::<Vec<_>>(), ["a/"]);
        assert_eq!(zip_parents("x").count(), 0);
    }
}
