//! The preview to the right of the files: pictures (zoom, turn, pan), video and audio
//! (video.rs), text and hex (loaded as they're scrolled), PDFs, fonts,
//! what's in a ZIP, and details.

use super::sidebar::chev;
use super::*;
use crate::api::{Bytes, Listing, MediaInfo, Packed, Peek};
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
fn peek_as_buttons(
    now: PeekAs,
    zip: bool,
    set: impl Fn(Option<PeekAs>) + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let set2 = set.clone();
    view! {
        <div class="peek-as">
            <button class="btn small" on:click=move |_| set(None)>"Show normally"</button>
            <OpenAs now=now zip small=true pick=move |h| set2(Some(h))/>
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
    let ways: Vec<PeekAs> = [PeekAs::Text, PeekAs::Hex, PeekAs::Image, PeekAs::Video]
        .into_iter()
        .chain(zip.then_some(PeekAs::Zip))
        .collect();
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
        let size =
            |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
        let (vw, vh) = (size(window().inner_width()), size(window().inner_height()));
        let (w, h) = (140.0_f64.max(r.width()), rows as f64 * 26.0 + 12.0);
        let x = r.left().min(vw - w - 4.0).max(4.0);
        let y = if r.bottom() + 4.0 + h <= vh - 4.0 {
            r.bottom() + 4.0
        } else {
            (r.top() - 4.0 - h).max(4.0)
        };
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
pub(super) const ZIP_EXTS: &[&str] = &[
    "zip", "apk", "jar", "cbz", "xapk", "ipa", "aar", "war", "whl", "vsix", "nupkg",
];

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
pub(super) fn Preview(
    pane: Pane,
    #[prop(optional)] entry: Option<Entry>,
    #[prop(optional)] src: Option<Src>,
) -> impl IntoView {
    let src_of = src;
    let p = pane;
    let alone = entry.is_some();
    // Kept as the preview moves from one picture to the next, and from one video to the next.
    let (pic_turn, video_turn) = (Turn::new(), Turn::new());
    // Only a different item (or a different "show as") rebuilds it, so a reload doesn't restart
    // a playing video.
    let shown = Memo::new(move |_| {
        let picked = match &entry {
            Some(e) => Some(e.clone()),
            None => p.selected_entry(),
        };
        picked.map(|e| {
            let forced = p
                .peek_as
                .with(|f| f.as_ref().filter(|(path, _)| *path == e.path).map(|f| f.1));
            // A file picked in this ZIP's columns.
            let src = p.src.get();
            let inner = p.zip_pick.with(|z| {
                z.as_ref()
                    .filter(|z| z.0 == src && z.1 == e.path)
                    .map(|z| (z.2.clone(), z.3))
            });
            (e, forced, inner)
        })
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
                Some((zip, None, Some((name, size)))) => view! { <Zipped pane=p src=p.src.get_untracked() zip name size/> }.into_any(),
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
                    let thumb = |e: &Entry| view! { <div class="pic"><Thumbnail src=src entry=e.clone()/></div> };
                    let body = match (forced, media(&e)) {
                        (Some(PeekAs::Text), _) => view! { <TextPeek src entry=e.clone()/> }.into_any(),
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
                        _ if has_ext(&e, PDF_EXTS) && e.size <= WHOLE_MAX => view! {
                            <iframe class="pdf" src=stream_url(src, &e) title="PDF"></iframe>
                        }.into_any(),
                        _ if has_ext(&e, FONT_EXTS) && e.size <= WHOLE_MAX => view! { <FontSample src entry=e.clone()/> }.into_any(),
                        _ if has_ext(&e, &["epub"]) => view! { <BigCover src entry=e.clone()/> }.into_any(),
                        _ if has_ext(&e, ZIP_EXTS) => view! { <ZipList src entry=e.clone()/> }.into_any(),
                        _ if is_text(&e) => view! { <TextPeek src entry=e.clone()/> }.into_any(),
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
                    // Kind, size, and the width × height and length once known, on one line.
                    let size = (!e.dir).then(|| human_size(e.size));
                    let facts = move || {
                        let mut v = vec![what.to_string()];
                        v.extend(size.clone());
                        info.with(|i| {
                            v.extend(i.width.zip(i.height).map(|(w, h)| format!("{w} × {h}")));
                            v.extend(i.duration.map(length));
                        });
                        v.join(" · ")
                    };
                    view! {
                        <div class="view">{body}</div>
                        <div class="meta">
                            <b class="name" title=e.name.clone()>{e.name.clone()}</b>
                            <span class="sub">{facts}</span>
                            {(e.mtime > 0).then(|| view! { <span class="sub">{format!("Modified {}", when(e.mtime))}</span> })}
                            <span class="sub path" title=e.path.clone()>{parent(&e.path)}</span>
                            {forced.map(|now| {
                                let path = e.path.clone();
                                peek_as_buttons(now, true, move |how| p.peek_as.set(how.map(|h| (path.clone(), h))))
                            })}
                        </div>
                    }.into_any()
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
/// other files.
pub(super) fn step_media(p: Pane, e: &Entry, delta: isize) {
    let Some(m) = media(e) else { return };
    let next = p.entries.with_untracked(|es| {
        let same: Vec<&Entry> = es.iter().filter(|x| media(x) == Some(m)).collect();
        let i = same.iter().position(|x| x.path == e.path)? as isize + delta;
        (i >= 0)
            .then(|| same.get(i as usize).map(|x| x.path.clone()))
            .flatten()
    });
    if let Some(path) = next {
        p.select_only(Some(path));
    }
}

/// ‹ and › for the previous and next file of the same kind.
fn neighbours(p: Pane, e: &Entry) -> impl IntoView {
    let a = StoredValue::new(e.clone());
    view! {
        <button class="btn" title="Previous (←)" aria-label="Previous" on:click=move |_| step_media(p, &a.get_value(), -1)>{ico(Icon::Prev)}</button>
        <button class="btn" title="Next (→)" aria-label="Next" on:click=move |_| step_media(p, &a.get_value(), 1)>{ico(Icon::Next)}</button>
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
        let Some(room) = pic
            .try_get_untracked()
            .flatten()
            .and_then(|d| d.parent_element())
        else {
            return;
        };
        let dpr = window().device_pixel_ratio().max(1.0);
        if (room.client_width().max(room.client_height()) as f64) * dpr > THUMB_PX {
            want.set(true);
        }
    });
    let p = String::from(js_sys::encode_uri_component(&entry.path));
    let url = crate::os::served(&format!(
        "cover?src={}&p={p}&s={}&m={}",
        src.key(),
        entry.size,
        entry.mtime
    ));
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
) -> impl IntoView {
    let alone = url.is_some();
    let loaded = RwSignal::new(false);
    let natural = RwSignal::new((0u32, 0u32));
    let zoom = RwSignal::new(1.0f64);
    let pan = RwSignal::new((0.0f64, 0.0f64));
    let shrunk_url = if alone { None } else { fit_url(src, &entry) };
    let url = url.unwrap_or_else(|| stream_url(src, &entry));
    // Showing the shrunk photo; and how many of the photo's pixels each of its pixels stands for.
    let shrunk = RwSignal::new(shrunk_url.is_some());
    let shown = RwSignal::new(shrunk_url.unwrap_or_else(|| url.clone()));
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
                fit.set(
                    img.offset_width() as f64
                        / (img.natural_width() as f64 * per_px.get_untracked()),
                );
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
        if let Some(img) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::HtmlImageElement>().ok())
        {
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
        request_animation_frame(measure);
    };
    // How much of its own size the picture shows at `zoom` 1 (fitted), turn included.
    let base = move || {
        let (w, h) = natural.get();
        fit.get() * turn_fit(turn.get(), w, h)
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
        if z == 1.0 {
            pan.set((0.0, 0.0));
        }
    };
    let style = move || {
        let (x, y) = pan.get();
        let (w, h) = natural.get();
        let s = zoom.get() * turn_fit(turn.get(), w, h);
        format!(
            "transform: translate({x}px, {y}px) scale({s}) rotate({}deg){}",
            turn.get() * 90,
            turn.css()
        )
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
    let down = move |ev: web_sys::MouseEvent| {
        if ev.button() != 0 || zoom.get_untracked() <= 1.0 {
            return;
        }
        ev.prevent_default();
        let (x0, y0) = (ev.client_x() as f64, ev.client_y() as f64);
        let (px, py) = pan.get_untracked();
        let moved = window_event_listener(ev::mousemove, move |e| {
            pan.set((px + e.client_x() as f64 - x0, py + e.client_y() as f64 - y0));
        });
        let up = window_event_listener(ev::mouseup, move |_| stop());
        stop();
        listeners.set_value(Some((moved, up)));
    };
    // A pinch on the trackpad comes as a wheel event with Ctrl down.
    let wheel = move |ev: web_sys::WheelEvent| {
        if ev.ctrl_key() || ev.meta_key() {
            ev.prevent_default();
            set_zoom(zoom.get_untracked() * (-ev.delta_y() * 0.01).exp());
        }
    };
    // Fitted, or its actual size (100%): whichever it isn't. A picture that fits at its actual
    // size goes to 200%.
    let fit_or_actual = move || {
        let actual = 1.0 / base();
        set_zoom(if zoom.get_untracked() > 1.0 {
            1.0
        } else if actual > 1.001 {
            actual
        } else {
            2.0
        });
    };
    view! {
        <div class="pic stage" class:whole=loaded class:zoomed=move || { zoom.get() > 1.0 }
            on:mousedown=down on:wheel=wheel
            on:dblclick=move |ev| { ev.stop_propagation(); fit_or_actual(); }>
            {let e = entry.clone(); move || (!loaded.get() && !alone).then(|| view! { <Thumbnail src entry=e.clone()/> })}
            <img class="full" node_ref=img_ref src=move || shown.get() alt="" draggable="false" class:pending=move || !loaded.get()
                style=style on:load=load on:transitionend=move |_| turn.settle()/>
        </div>
        <div class="viewer-bar">
            {(!alone).then(|| view! { {neighbours(pane, &entry)} <span class="gap"></span> })}
            <button class="btn" title="Zoom out" aria-label="Zoom out" on:click=move |_| set_zoom(zoom.get_untracked() / 1.25)>{ico(Icon::ZoomOut)}</button>
            <button class="btn zoom" title="Of the picture's own size. Click (or double-click the picture) for 100% or to fit."
                on:click=move |_| fit_or_actual()>
                {move || format!("{:.0}%", zoom.get() * base() * 100.0)}
            </button>
            <button class="btn" title="Zoom in (or pinch)" aria-label="Zoom in" on:click=move |_| set_zoom(zoom.get_untracked() * 1.25)>{ico(Icon::ZoomIn)}</button>
            <span class="gap"></span>
            <button class="btn" title="Turn left" aria-label="Turn left" on:click=move |_| turn.by(-1)>{ico(Icon::TurnLeft)}</button>
            <button class="btn" title="Turn right" aria-label="Turn right" on:click=move |_| turn.by(1)>{ico(Icon::TurnRight)}</button>
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
) -> impl IntoView {
    let key = resume_key(&entry);
    let player = if video {
        let pop = Callback::new(|video: Popped| {
            spawn_local(async move {
                if let Err(e) = call::<()>("pop_out", json!({ "video": video })).await {
                    web_sys::console::error_1(&e.into());
                }
            });
        });
        view! { <VideoPlayer url key title=entry.name.clone() info turn pane=pane.id pop/> }
            .into_any()
    } else {
        view! { <SoundPlayer url key info pane=pane.id/> }.into_any()
    };
    view! {
        {player}
        {(!alone).then(|| view! { <div class="viewer-bar">{neighbours(pane, &entry)}</div> })}
    }
}

/// A font's letters at a few sizes, loaded from the file itself.
#[component]
fn FontSample(src: Src, entry: Entry) -> impl IntoView {
    // A name of its own for each preview, so a font shown earlier isn't reused.
    let family = format!("ferry-peek-{}", js_sys::Date::now() as u64);
    let css = format!(
        "@font-face {{ font-family: \"{family}\"; src: url(\"{}\"); }}",
        stream_url(src, &entry)
    );
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
    let mut top = Node {
        dir: true,
        ..Default::default()
    };
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
            out.push(ZipRow {
                path: path.clone(),
                name: name.clone(),
                depth,
                dir: k.dir,
                size: k.size,
            });
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
        (
            self.line + self.text.matches('\n').count(),
            !self.text.is_empty() && !self.text.ends_with('\n'),
        )
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
        return segs
            .into_iter()
            .map(|(c, s)| segment_view(c, s))
            .collect_view()
            .into_any();
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
            let n = if k == 0 && p.cont {
                String::new()
            } else {
                (p.line + k).to_string()
            };
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
        let Some(el) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let left = el.scroll_height() - el.scroll_top() - el.client_height();
        match next.get_untracked() {
            Some(n) if n < cap && left < 600 && error.get_untracked().is_none() => fetch(n),
            _ => {}
        }
    };
    piece_view(pieces, next, error, loading, scrolled, cap)
}

/// The pieces in a `<pre>`, with a line under it saying what's going on.
fn piece_view(
    pieces: RwSignal<Vec<Piece>>,
    next: RwSignal<Option<u64>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    scrolled: impl Fn(web_sys::Event) + Copy + Send + Sync + 'static,
    cap: u64,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let empty = move || {
        pieces.with(|v| v.len() == 1 && v[0].text.trim().is_empty()) && next.get().is_none()
    };
    let stopped = move || pieces.with(|v| v.iter().any(|p| p.stopped));
    // Room for the longest line number so far.
    let digits = move || {
        pieces.with(|v| {
            v.last()
                .map_or(1, |p| p.next_line().0.max(1).to_string().len())
        })
    };
    let (wrap, numbers) = (ctx.wrap_text, ctx.line_numbers);
    let tools = view! {
        <div class="peek-tools">
            <button class="filter-opt" class:on=move || wrap.get() aria-pressed=move || wrap.get().to_string()
                aria-label="Wrap long lines" title=move || if wrap.get() { "Wrap long lines: on" } else { "Wrap long lines: off" }
                on:click=move |_| ctx.set_wrap_text(!wrap.get_untracked())>{ico(Icon::Wrap)}</button>
            <button class="filter-opt" class:on=move || numbers.get() aria-pressed=move || numbers.get().to_string()
                aria-label="Line numbers" title=move || if numbers.get() { "Line numbers: on" } else { "Line numbers: off" }
                on:click=move |_| ctx.set_line_numbers(!numbers.get_untracked())>{ico(Icon::LineNumbers)}</button>
        </div>
    };
    view! {
        <div class="peek">
            {tools}
            {move || if pieces.with(Vec::is_empty) {
                match error.get() {
                    Some(e) => view! { <span class="hint">{e}</span> }.into_any(),
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
                        view! { <span class="more">{e}</span> }.into_any()
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
/// ZIP unpacked.
#[component]
fn Zipped(pane: Pane, src: Src, zip: Entry, name: String, size: u64) -> impl IntoView {
    let p = pane;
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    let e = Entry {
        name: short.clone(),
        path: name.clone(),
        dir: false,
        size,
        mtime: 0,
    };
    let url = crate::os::served(&format!(
        "zipped?src={}&p={}&s={}&m={}&e={}",
        src.key(),
        String::from(js_sys::encode_uri_component(&zip.path)),
        zip.size,
        zip.mtime,
        String::from(js_sys::encode_uri_component(&name)),
    ));
    let inner = Inner {
        name: name.clone(),
        zip_path: zip.path.clone(),
        zip_size: zip.size,
    };
    let (pic_turn, video_turn) = (Turn::new(), Turn::new());
    let info = RwSignal::new(MediaInfo::default());
    let forced = RwSignal::new(None::<PeekAs>);
    let body = move || {
        let (e, url, inner) = (e.clone(), url.clone(), inner.clone());
        if size > UNPACK_MAX {
            return view! { <span class="hint">"Too big to show from inside the ZIP."</span> }
                .into_any();
        }
        match (forced.get(), media(&e)) {
            (Some(PeekAs::Text), _) => view! { <TextPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Hex), _) => view! { <HexPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Image), _) => {
                view! { <FullImage pane=p src entry=e info turn=pic_turn url/> }.into_any()
            }
            (Some(PeekAs::Video), _) => {
                view! { <Player pane=p entry=e url video=true info turn=video_turn alone=true/> }
                    .into_any()
            }
            // A ZIP in a ZIP isn't listed.
            (Some(PeekAs::Zip), _) => {
                view! { <span class="hint">"Not shown from inside a ZIP."</span> }.into_any()
            }
            (None, Some(Media::Image)) => {
                view! { <FullImage pane=p src entry=e info turn=pic_turn url/> }.into_any()
            }
            (None, Some(Media::Video)) => {
                view! { <Player pane=p entry=e url video=true info turn=video_turn alone=true/> }
                    .into_any()
            }
            (None, Some(Media::Audio)) => {
                view! { <Player pane=p entry=e url video=false info turn=video_turn alone=true/> }
                    .into_any()
            }
            _ if has_ext(&e, PDF_EXTS) => {
                view! { <iframe class="pdf" src=url title="PDF"></iframe> }.into_any()
            }
            _ if is_text(&e) => view! { <TextPeek src entry=e inner/> }.into_any(),
            _ => view! {
                <div class="viewer-bar">
                    <OpenAs pick=move |h| forced.set(Some(h))/>
                </div>
            }
            .into_any(),
        }
    };
    let facts = move || {
        let mut v = vec!["In a ZIP".to_string(), human_size(size)];
        info.with(|i| {
            v.extend(i.width.zip(i.height).map(|(w, h)| format!("{w} × {h}")));
            v.extend(i.duration.map(length));
        });
        v.join(" · ")
    };
    view! {
        <div class="view">{body}</div>
        <div class="meta">
            <b class="name" title=short.clone()>{short.clone()}</b>
            <span class="sub">{facts}</span>
            <span class="sub path" title=format!("{}/{name}", zip.path)>{match name.rfind('/') { Some(at) => format!("{}/{}", zip.name, &name[..at]), None => zip.name.clone() }}</span>
            {move || forced.get().map(|now| peek_as_buttons(now, false, move |how| forced.set(how)))}
        </div>
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
            Ok((
                Piece {
                    text: p.text,
                    runs: p.runs,
                    stopped: p.colours_stopped,
                    ..Piece::default()
                },
                p.next,
            ))
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
            let args = json!({ "src": src.key(), "path": path, "start": start, "inner": i.as_ref().map(|i| i.name.clone()), "zip_size": i.map(|i| i.zip_size) });
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
            {move || error.get().map(|e| view! { <span class="more">{e}</span> })}
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
        out.extend(row.iter().map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        }));
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
        Packed {
            name: name.into(),
            size,
            dir: name.ends_with('/'),
        }
    }

    #[test]
    fn zip_tree_nests_and_sorts() {
        let items = [
            packed("b.txt", 1),
            packed("a/z.txt", 2),
            packed("a/c/d.txt", 3),
            packed("A2/", 0),
        ];
        let rows: Vec<(String, usize, bool, u64)> = zip_tree(&items)
            .into_iter()
            .map(|r| (r.path, r.depth, r.dir, r.size))
            .collect();
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
