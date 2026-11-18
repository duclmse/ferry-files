//! The preview to the right of the files: pictures (zoom, turn, pan), video and audio (skip,
//! speed), text and hex (loaded as they're scrolled), PDFs, fonts, what's in a ZIP, and details.

use super::*;
use crate::api::{Bytes, Listing, MediaInfo, Peek};
use wasm_bindgen::closure::Closure;

/// A file shown another way than its type says, from the context menu.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum PeekAs {
    Text,
    Hex,
}

const PDF_EXTS: &[&str] = &["pdf"];
const FONT_EXTS: &[&str] = &["ttf", "otf", "woff", "woff2"];
pub(super) const ZIP_EXTS: &[&str] = &["zip", "apk", "jar", "cbz", "xapk"];
/// The backend serves a whole file (a PDF, a font) up to this size.
const WHOLE_MAX: u64 = 64 * 1024 * 1024;

pub(super) fn has_ext(e: &Entry, exts: &[&str]) -> bool {
    !e.dir && exts.contains(&ext(&e.name).as_str())
}

/// What's selected, to the right of the files: the picture, video, sound, text or contents, and
/// details. Shown in every view; the button at the end of the status bar hides it.
#[component]
pub(super) fn Preview(pane: Pane) -> impl IntoView {
    let p = pane;
    // Only a different item (or a different "show as") rebuilds it, so a reload doesn't restart
    // a playing video.
    let shown = Memo::new(move |_| {
        p.selected_entry().map(|e| {
            let forced = p.peek_as.with(|f| f.as_ref().filter(|(path, _)| *path == e.path).map(|f| f.1));
            // A file picked in this ZIP's columns.
            let src = p.src.get();
            let inner = p.zip_pick.with(|z| z.as_ref().filter(|z| z.0 == src && z.1 == e.path).map(|z| (z.2.clone(), z.3)));
            (e, forced, inner)
        })
    });
    view! {
        <div class="preview">
            {move || match shown.get() {
                None => view! { <span class="hint">{move || match p.marked.with(Vec::len) {
                    0 | 1 => "Pick an item to see it here.".to_string(),
                    n => format!("{n} items picked."),
                }}</span> }.into_any(),
                Some((zip, None, Some((name, size)))) => view! { <Zipped pane=p src=p.src.get_untracked() zip name size/> }.into_any(),
                Some((e, forced, _)) => {
                    let what = match kind(&e) {
                        Kind::Folder => "Folder", Kind::Epub => "EPUB book", Kind::Text => "Text file", Kind::Other => "File",
                    };
                    let src = p.src.get_untracked();
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
                        (None, Some(Media::Image)) => view! { <FullImage pane=p src entry=e.clone() info/> }.into_any(),
                        (None, Some(Media::Video)) => view! { <Player pane=p entry=e.clone() url=stream_url(src, &e) video=true info/> }.into_any(),
                        (None, Some(Media::Audio)) => view! {
                            {thumb(&e)}
                            <Player pane=p entry=e.clone() url=stream_url(src, &e) video=false info/>
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
                            let (a, b) = (e.path.clone(), e.path.clone());
                            view! {
                                {thumb(&e)}
                                <div class="viewer-bar">
                                    <button class="btn" on:click=move |_| p.peek_as.set(Some((a.clone(), PeekAs::Text)))>"Open as text"</button>
                                    <button class="btn" on:click=move |_| p.peek_as.set(Some((b.clone(), PeekAs::Hex)))>"Open as binary"</button>
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
                            <b class="name">{e.name.clone()}</b>
                            <span class="sub">{facts}</span>
                            {(e.mtime > 0).then(|| view! { <span class="sub">{format!("Modified {}", when(e.mtime))}</span> })}
                            <span class="sub path" title=e.path.clone()>{parent(&e.path)}</span>
                            {forced.map(|_| view! {
                                <button class="btn small" on:click=move |_| p.peek_as.set(None)>"Show normally"</button>
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
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Text files the preview shows the start of: any size, as only the start is read.
fn is_text(e: &Entry) -> bool {
    !e.dir && TEXT_EXTS.contains(&ext(&e.name).as_str())
}

/// Selects the picture, video or sound before (`-1`) or after (`1`) `e` in the pane, skipping
/// other files.
pub(super) fn step_media(p: Pane, e: &Entry, delta: isize) {
    let Some(m) = media(e) else { return };
    let next = p.entries.with_untracked(|es| {
        let same: Vec<&Entry> = es.iter().filter(|x| media(x) == Some(m)).collect();
        let i = same.iter().position(|x| x.path == e.path)? as isize + delta;
        (i >= 0).then(|| same.get(i as usize).map(|x| x.path.clone())).flatten()
    });
    if let Some(path) = next {
        p.select_only(Some(path));
    }
}

/// ‹ and › for the previous and next file of the same kind.
fn neighbours(p: Pane, e: &Entry) -> impl IntoView {
    let a = StoredValue::new(e.clone());
    view! {
        <button class="btn" title="Previous (←)" on:click=move |_| step_media(p, &a.get_value(), -1)>"‹"</button>
        <button class="btn" title="Next (→)" on:click=move |_| step_media(p, &a.get_value(), 1)>"›"</button>
    }
}

/// Extra scale for a quarter turn, so a `w` × `h` picture turned on its side fits where it was.
fn turn_fit(turn: i32, w: u32, h: u32) -> f64 {
    if turn % 2 == 0 || w == 0 || h == 0 {
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
        let Some(room) = pic.get_untracked().and_then(|d| d.parent_element()) else { return };
        let dpr = window().device_pixel_ratio().max(1.0);
        if (room.client_width().max(room.client_height()) as f64) * dpr > THUMB_PX {
            want.set(true);
        }
    });
    let p = String::from(js_sys::encode_uri_component(&entry.path));
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
#[component]
/// With `url`, a picture from somewhere else than the file (one in a ZIP): no thumbnail while it
/// loads, and no ‹ ›.
fn FullImage(pane: Pane, src: Src, entry: Entry, info: RwSignal<MediaInfo>, #[prop(optional)] url: Option<String>) -> impl IntoView {
    let alone = url.is_some();
    let loaded = RwSignal::new(false);
    let natural = RwSignal::new((0u32, 0u32));
    let zoom = RwSignal::new(1.0f64);
    let turn = RwSignal::new(0i32);
    let pan = RwSignal::new((0.0f64, 0.0f64));
    let url = url.unwrap_or_else(|| stream_url(src, &entry));
    // How big the picture shows when it fits, against its own size (at most 1): `zoom` is on top
    // of that, so the picture is at zoom × fit (× the turn's) of its pixels. Measured again when
    // the preview changes size.
    let fit = RwSignal::new(1.0f64);
    let img_ref = NodeRef::<html::Img>::new();
    let measure = move || {
        if let Some(img) = img_ref.get_untracked() {
            if img.natural_width() > 0 && img.offset_width() > 0 {
                fit.set(img.offset_width() as f64 / img.natural_width() as f64);
            }
        }
    };
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(img) = img_ref.get() else { return };
        let cb = Closure::<dyn Fn()>::new(measure);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&img);
            watcher.set_value(Some((ro, cb)));
        }
    });
    on_cleanup(move || {
        if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
            ro.disconnect();
        }
    });
    let load = move |ev: web_sys::Event| {
        if let Some(img) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlImageElement>().ok()) {
            natural.set((img.natural_width(), img.natural_height()));
            let _ = info.try_update(|i| {
                if i.width.is_none() {
                    (i.width, i.height) = (Some(img.natural_width()), Some(img.natural_height()));
                }
            });
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
        if z == 1.0 {
            pan.set((0.0, 0.0));
        }
    };
    let style = move || {
        let (x, y) = pan.get();
        let (w, h) = natural.get();
        let s = zoom.get() * turn_fit(turn.get(), w, h);
        format!("transform: translate({x}px, {y}px) scale({s}) rotate({}deg)", turn.get() * 90)
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
        set_zoom(if zoom.get_untracked() > 1.0 { 1.0 } else if actual > 1.001 { actual } else { 2.0 });
    };
    view! {
        <div class="pic stage" class:whole=loaded class:zoomed=move || { zoom.get() > 1.0 }
            on:mousedown=down on:wheel=wheel
            on:dblclick=move |ev| { ev.stop_propagation(); fit_or_actual(); }>
            {let e = entry.clone(); move || (!loaded.get() && !alone).then(|| view! { <Thumbnail src entry=e.clone()/> })}
            <img class="full" node_ref=img_ref src=url alt="" draggable="false" class:pending=move || !loaded.get()
                style=style on:load=load/>
        </div>
        <div class="viewer-bar">
            {(!alone).then(|| view! { {neighbours(pane, &entry)} <span class="gap"></span> })}
            <button class="btn" title="Zoom out" on:click=move |_| set_zoom(zoom.get_untracked() / 1.25)>"−"</button>
            <button class="btn zoom" title="Of the picture's own size. Click (or double-click the picture) for 100% or to fit."
                on:click=move |_| fit_or_actual()>
                {move || format!("{:.0}%", zoom.get() * base() * 100.0)}
            </button>
            <button class="btn" title="Zoom in (or pinch)" on:click=move |_| set_zoom(zoom.get_untracked() * 1.25)>"+"</button>
            <span class="gap"></span>
            <button class="btn" title="Turn left" on:click=move |_| turn.update(|t| *t = (*t + 3) % 4)>"⟲"</button>
            <button class="btn" title="Turn right" on:click=move |_| turn.update(|t| *t = (*t + 1) % 4)>"⟳"</button>
        </div>
    }
}

/// A video or audio player streaming from the device, with skips, speeds and (video) turning.
/// When the preview moves on, it stops and lets go of the file, so the web view drops what it
/// buffered.
#[component]
/// `alone`: no ‹ › (a file in a ZIP).
fn Player(pane: Pane, entry: Entry, url: String, video: bool, info: RwSignal<MediaInfo>, #[prop(optional)] alone: bool) -> impl IntoView {
    let el = StoredValue::new_local(None::<web_sys::HtmlMediaElement>);
    let keep = move |e: &web_sys::HtmlMediaElement| el.set_value(Some(e.clone()));
    on_cleanup(move || {
        if let Some(m) = el.try_update_value(Option::take).flatten() {
            let _ = m.pause();
            let _ = m.remove_attribute("src");
            m.load();
        }
    });
    let turn = RwSignal::new(0i32);
    let natural = RwSignal::new((0u32, 0u32));
    // What the header didn't give, from the player once it has read the start of the file.
    let meta = move |ev: web_sys::Event| {
        let Some(m) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlMediaElement>().ok()) else { return };
        let d = m.duration();
        let v = m.dyn_into::<web_sys::HtmlVideoElement>().ok();
        if let Some(v) = &v {
            natural.set((v.video_width(), v.video_height()));
        }
        let _ = info.try_update(|i| {
            if i.duration.is_none() && d.is_finite() && d > 0.0 {
                i.duration = Some(d);
            }
            // 0 × 0 for a file with sound only.
            if let Some(v) = v.filter(|v| v.video_width() > 0) {
                if i.width.is_none() {
                    (i.width, i.height) = (Some(v.video_width()), Some(v.video_height()));
                }
            }
        });
    };
    let skip = move |by: f64| {
        el.with_value(|m| {
            if let Some(m) = m {
                let to = (m.current_time() + by).max(0.0);
                let d = m.duration();
                m.set_current_time(if d.is_finite() { to.min(d) } else { to });
            }
        })
    };
    let speed = RwSignal::new(1.0f64);
    let set_speed = move |s: f64| {
        speed.set(s);
        el.with_value(|m| {
            if let Some(m) = m {
                m.set_playback_rate(s);
            }
        });
    };
    let video_ref = NodeRef::<html::Video>::new();
    let audio_ref = NodeRef::<html::Audio>::new();
    video_ref.on_load(move |v| keep(&v));
    audio_ref.on_load(move |a| keep(&a));
    let style = move || {
        let (w, h) = natural.get();
        format!("transform: rotate({}deg) scale({})", turn.get() * 90, turn_fit(turn.get(), w, h))
    };
    let player = if video {
        view! {
            <video class="play" controls preload="metadata" src=url node_ref=video_ref style=style on:loadedmetadata=meta></video>
        }
        .into_any()
    } else {
        view! { <audio class="play" controls preload="metadata" src=url node_ref=audio_ref on:loadedmetadata=meta></audio> }.into_any()
    };
    view! {
        {player}
        <div class="viewer-bar">
            {(!alone).then(|| view! { {neighbours(pane, &entry)} <span class="gap"></span> })}
            <button class="btn" title="Back 10 seconds" on:click=move |_| skip(-10.0)>"−10s"</button>
            <button class="btn" title="Forward 10 seconds" on:click=move |_| skip(10.0)>"+10s"</button>
            <button class="btn" title="Forward 30 seconds" on:click=move |_| skip(30.0)>"+30s"</button>
            <span class="gap"></span>
            <span class="seg" title="Speed">
                {[1.0, 1.25, 1.5, 2.0].map(|s| view! {
                    <button class:active=move || speed.get() == s on:click=move |_| set_speed(s)>{format!("{s}×")}</button>
                })}
            </span>
            {video.then(|| view! {
                <span class="gap"></span>
                <button class="btn" title="Turn" on:click=move |_| turn.update(|t| *t = (*t + 1) % 4)>"⟳"</button>
            })}
        </div>
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

/// What's in a ZIP (or APK, JAR, CBZ): read from the end of the file, not the whole of it.
#[component]
fn ZipList(src: Src, entry: Entry) -> impl IntoView {
    let got = RwSignal::new(None::<Result<Listing, String>>);
    let args = json!({ "src": src.key(), "path": entry.path, "size": entry.size });
    spawn_local(async move {
        let r = call::<Listing>("zip_list", args).await;
        let _ = got.try_set(Some(r));
    });
    view! {
        <div class="zip">
            {move || match got.get() {
                None => view! { <span class="spin"></span> }.into_any(),
                Some(Err(e)) => view! { <span class="hint">{e}</span> }.into_any(),
                Some(Ok(l)) => {
                    let files = l.items.iter().filter(|i| !i.dir).count();
                    let unpacked: u64 = l.items.iter().map(|i| i.size).sum();
                    let shown = l.items.len() as u64;
                    view! {
                        <span class="sub">{format!("{} items · {} unpacked", l.total, human_size(unpacked))}</span>
                        <div class="zip-list">
                            <table>
                                <tbody>
                                    {l.items.into_iter().map(|i| view! {
                                        <tr class:dir=i.dir>
                                            <td class="n" title=i.name.clone()>{i.name.clone()}</td>
                                            <td class="s">{(!i.dir).then(|| human_size(i.size))}</td>
                                        </tr>
                                    }).collect_view()}
                                </tbody>
                            </table>
                        </div>
                        {(shown < l.total).then(|| view! { <span class="more">{format!("Only the first {shown} are listed.")}</span> })}
                        {(files == 0 && l.total == 0).then(|| view! { <span class="hint">"Empty"</span> })}
                    }.into_any()
                }
            }}
        </div>
    }
}

/// How much of a file the text or hex view loads, at most, as it's scrolled.
const TEXT_CAP: u64 = 8 * 1024 * 1024;
const HEX_CAP: u64 = 4 * 1024 * 1024;

/// Pieces of a file loaded one after another as the end is scrolled near, up to `cap` bytes.
/// `get(start)` fetches the piece at `start`: its text and where the next one starts.
fn pieces<F, Fut>(get: F, cap: u64) -> (RwSignal<Vec<String>>, RwSignal<Option<u64>>, RwSignal<Option<String>>, RwSignal<bool>, impl Fn(web_sys::Event) + Copy + Send + Sync)
where
    F: Fn(u64) -> Fut + Copy + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(String, Option<u64>), String>> + 'static,
{
    let pieces = RwSignal::new(Vec::<String>::new());
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
                Ok((text, n)) => {
                    if pieces.try_update(|v| v.push(text)).is_some() {
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
        let Some(el) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let left = el.scroll_height() - el.scroll_top() - el.client_height();
        match next.get_untracked() {
            Some(n) if n < cap && left < 600 && error.get_untracked().is_none() => fetch(n),
            _ => {}
        }
    };
    (pieces, next, error, loading, scrolled)
}

/// The pieces in a `<pre>`, with a line under it saying what's going on.
fn piece_view(
    pieces: RwSignal<Vec<String>>,
    next: RwSignal<Option<u64>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    scrolled: impl Fn(web_sys::Event) + Copy + Send + Sync + 'static,
    cap: u64,
    hex: bool,
) -> impl IntoView {
    let empty = move || pieces.with(|v| v.len() == 1 && v[0].trim().is_empty()) && next.get().is_none();
    view! {
        <div class="peek" class:hex=hex>
            {move || if pieces.with(Vec::is_empty) {
                match error.get() {
                    Some(e) => view! { <span class="hint">{e}</span> }.into_any(),
                    None => view! { <span class="spin"></span> }.into_any(),
                }
            } else if empty() {
                view! { <span class="hint">"Empty"</span> }.into_any()
            } else {
                view! {
                    <pre on:scroll=scrolled>
                        <For each=move || 0..pieces.with(Vec::len) key=|i| *i
                            children=move |i| pieces.with_untracked(|v| v[i].clone())/>
                    </pre>
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

/// A file packed in a ZIP: its name in it, and the ZIP's size.
#[derive(Clone)]
struct Inner {
    name: String,
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
    let e = Entry { name: short.clone(), path: name.clone(), dir: false, size, mtime: 0 };
    let url = crate::os::served(&format!(
        "zipped?src={}&p={}&s={}&m={}&e={}",
        src.key(),
        String::from(js_sys::encode_uri_component(&zip.path)),
        zip.size,
        zip.mtime,
        String::from(js_sys::encode_uri_component(&name)),
    ));
    let inner = Inner { name: name.clone(), zip_size: zip.size };
    let info = RwSignal::new(MediaInfo::default());
    let forced = RwSignal::new(None::<PeekAs>);
    let body = move || {
        let (e, url, inner) = (e.clone(), url.clone(), inner.clone());
        if size > UNPACK_MAX {
            return view! { <span class="hint">"Too big to show from inside the ZIP."</span> }.into_any();
        }
        match (forced.get(), media(&e)) {
            (Some(PeekAs::Text), _) => view! { <TextPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Hex), _) => view! { <HexPeek src entry=e inner/> }.into_any(),
            (None, Some(Media::Image)) => view! { <FullImage pane=p src entry=e info url/> }.into_any(),
            (None, Some(Media::Video)) => view! { <Player pane=p entry=e url video=true info alone=true/> }.into_any(),
            (None, Some(Media::Audio)) => view! { <Player pane=p entry=e url video=false info alone=true/> }.into_any(),
            _ if has_ext(&e, PDF_EXTS) => view! { <iframe class="pdf" src=url title="PDF"></iframe> }.into_any(),
            _ if is_text(&e) => view! { <TextPeek src entry=e inner/> }.into_any(),
            _ => view! {
                <div class="viewer-bar">
                    <button class="btn" on:click=move |_| forced.set(Some(PeekAs::Text))>"Open as text"</button>
                    <button class="btn" on:click=move |_| forced.set(Some(PeekAs::Hex))>"Open as binary"</button>
                </div>
            }.into_any(),
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
            <b class="name">{short}</b>
            <span class="sub">{facts}</span>
            <span class="sub path" title=format!("{}/{name}", zip.path)>{match name.rfind('/') { Some(at) => format!("{}/{}", zip.name, &name[..at]), None => zip.name.clone() }}</span>
            {move || forced.get().map(|_| view! {
                <button class="btn small" on:click=move |_| forced.set(None)>"Show normally"</button>
            })}
        </div>
    }
}

/// A text file, a piece at a time: the next piece loads when the end is scrolled near.
#[component]
fn TextPeek(src: Src, entry: Entry, #[prop(optional)] inner: Option<Inner>) -> impl IntoView {
    let path = StoredValue::new(entry.path);
    let inner = StoredValue::new(inner);
    let get = move |start: u64| async move {
        let path = path.try_get_value().unwrap_or_default();
        let p = call::<Peek>("peek_text", json!({ "src": src.key(), "path": path, "start": start, "inner": inner.try_get_value().flatten().map(|i| i.name), "zipSize": inner.try_get_value().flatten().map(|i| i.zip_size) })).await?;
        Ok((p.text, p.next))
    };
    let (pieces, next, error, loading, scrolled) = pieces(get, TEXT_CAP);
    piece_view(pieces, next, error, loading, scrolled, TEXT_CAP, false)
}

/// Any file's bytes, 16 to a line: where, in hex, and as text.
#[component]
fn HexPeek(src: Src, entry: Entry, #[prop(optional)] inner: Option<Inner>) -> impl IntoView {
    let path = StoredValue::new(entry.path);
    let inner = StoredValue::new(inner);
    let get = move |start: u64| async move {
        let path = path.try_get_value().unwrap_or_default();
        let b = call::<Bytes>("peek_bytes", json!({ "src": src.key(), "path": path, "start": start, "inner": inner.try_get_value().flatten().map(|i| i.name), "zipSize": inner.try_get_value().flatten().map(|i| i.zip_size) })).await?;
        Ok((hex_lines(start, &b.bytes), b.next))
    };
    let (pieces, next, error, loading, scrolled) = pieces(get, HEX_CAP);
    piece_view(pieces, next, error, loading, scrolled, HEX_CAP, true)
}

/// "00000010  48 65 6c 6c 6f 20 77 6f  72 6c 64 0a 00 00 00 00  Hello world....." per 16 bytes.
fn hex_lines(start: u64, bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 5);
    for (n, row) in bytes.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", start + n as u64 * 16);
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
