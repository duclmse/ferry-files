//! Pictures: read ahead for ‹ ›, decoded off the page, zoomed, turned and panned.

use super::*;

/// Selects the picture, video or sound before (`-1`) or after (`1`) `e` in the pane, skipping
/// other files. It goes from what's picked, which can be ahead of what the preview shows while
/// the next picture is read.
pub(in crate::app) fn step_media(p: Pane, e: &Entry, delta: isize) {
    let Some(m) = media(e) else { return };
    let picked = p.selected.get_untracked();
    let next = p.entries.with_untracked(|es| {
        let same: Vec<&Entry> = es.iter().filter(|x| media(x) == Some(m)).collect();
        let at = picked
            .as_ref()
            .and_then(|s| same.iter().position(|x| x.path == *s))
            .or_else(|| same.iter().position(|x| x.path == e.path))?;
        let i = at as isize + delta;
        (i >= 0)
            .then(|| same.get(i as usize).map(|x| x.path.clone()))
            .flatten()
    });
    if let Some(path) = next {
        p.select_only(Some(path));
    }
}

/// The picture the preview would show for `w` in full, and the URL it reads it from (the photo
/// shrunk to the preview, or the file; in a ZIP, the file unpacked). None for anything else.
pub(super) fn picture_url(
    p: Pane,
    w: &Option<(Entry, Option<PeekAs>, Option<(String, u64, bool)>)>,
) -> Option<(String, String)> {
    let (e, forced, inner) = w.as_ref()?;
    if forced.is_some() {
        return None;
    }
    let src = p.src.get_untracked();
    let is_picture = |name: &str| {
        media(&Entry {
            name: base(name),
            path: name.to_string(),
            dir: false,
            size: 0,
            mtime: 0,
            hidden: false,
        }) == Some(Media::Image)
    };
    match inner {
        Some((name, _, _)) => {
            is_picture(name).then(|| (name.clone(), super::views::zipped_url(src, e, name)))
        }
        None => (!e.dir && media(e) == Some(Media::Image)).then(|| {
            (
                e.path.clone(),
                fit_url(src, e).unwrap_or_else(|| stream_url(src, e)),
            )
        }),
    }
}

/// Shows a picture once it's been read into memory and decoded: `show` puts `path`'s picture
/// (read from `url`) in the preview, which then draws it at once from that copy. Until then the
/// picture shown stays; a picture read ahead is shown at once.
/// Only the last one asked for is shown, and only if `still()` (nothing else was picked
/// meanwhile). One that can't be read is shown anyway, to say why.
pub(in crate::app) fn when_read(
    p: Pane,
    path: String,
    url: String,
    still: impl Fn() -> bool + 'static,
    show: impl FnOnce() + 'static,
) {
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
        if p.coming
            .with_untracked(|c| c.as_ref().is_some_and(|(_, u)| *u == url))
        {
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
        let same: Vec<&Entry> = es
            .iter()
            .filter(|x| media(x) == Some(Media::Image))
            .collect();
        let i = same.iter().position(|x| x.path == e.path)?;
        same.get(i + 1).map(|&x| x.clone())
    });
    if let Some(next) = next {
        let src = p.src.get_untracked();
        read_pic(
            p,
            fit_url(src, &next).unwrap_or_else(|| stream_url(src, &next)),
        );
    }
}

/// The picture at `url` (one the backend serves) as a copy in memory (an object URL), decoded,
/// so showing it doesn't hold up the window; None when it can't be read.
async fn read_decoded(url: &str) -> Option<String> {
    use wasm_bindgen_futures::JsFuture;
    let resp: web_sys::Response = JsFuture::from(window().fetch_with_str(url))
        .await
        .ok()?
        .dyn_into()
        .ok()?;
    if !resp.ok() {
        return None;
    }
    let blob: web_sys::Blob = JsFuture::from(resp.blob().ok()?)
        .await
        .ok()?
        .dyn_into()
        .ok()?;
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
    p.pics.with_value(|v| {
        v.iter()
            .find(|(u, _)| u == url)
            .and_then(|(_, c)| c.clone())
    })
}

/// ‹ and › for the previous and next file of the same kind: `go` steps by -1 or 1.
pub(super) fn neighbours(go: Callback<isize>) -> impl IntoView {
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
pub(super) fn BigCover(src: Src, entry: Entry) -> impl IntoView {
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
    let p = uri_part(&entry.path);
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
pub(super) fn FullImage(
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
        let (Some(img), Some(st)) = (
            img_ref.try_get_untracked().flatten(),
            stage.try_get_untracked().flatten(),
        ) else {
            return None;
        };
        let (w, h) = natural.get_untracked();
        let t = turn.get_untracked();
        let s = z * turn_fit(t, w, h);
        let (iw, ih) = (
            img.offset_width() as f64 * s,
            img.offset_height() as f64 * s,
        );
        let shown = if t.rem_euclid(2) == 0 {
            (iw, ih)
        } else {
            (ih, iw)
        };
        Some((shown, (st.client_width() as f64, st.client_height() as f64)))
    };
    // How far the picture can be moved: until its edge reaches the stage's.
    let reach = move || {
        sizes(zoom.get_untracked()).map_or((0.0, 0.0), |((w, h), (sw, sh))| {
            (((w - sw) / 2.0).max(0.0), ((h - sh) / 2.0).max(0.0))
        })
    };
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
            PicFit::Width => {
                sizes(1.0).map_or(1.0, |((w, _), (sw, _))| if w > 0.0 { sw / w } else { 1.0 })
            }
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
            let on = super::video::full_screen_element().is_some_and(|el| {
                viewer
                    .try_get_untracked()
                    .flatten()
                    .is_some_and(|v| wasm_bindgen::JsValue::from(v) == el)
            });
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
        let instant = if settled.get() {
            turn.css()
        } else {
            "; transition: none"
        };
        format!(
            "transform: translate({x}px, {y}px) scale({s}) rotate({}deg){instant}",
            turn.get() * 90
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
