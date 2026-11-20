//! Playing videos and sounds. A video gets controls of its own under the picture, so they stay
//! level and the same size when the picture is turned (the web view's own controls are part of
//! the video, and would turn and scale with it). On the picture: click to play or pause, hold for
//! 2×, double-click the left side to go back 5 s and the right side to go forward 10 s. Hovering
//! the seek bar shows that moment. Used in the preview and in a popped-out window.
//!
//! Also: the keys for the preview's video or sound (keymap.rs), and where each was stopped, so
//! it goes on from there next time.

use super::*;
use crate::api::MediaInfo;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsValue;

/// The video settings (Settings > Video), kept in the web view's storage, so a popped-out
/// window has them too.
#[derive(Clone, Copy)]
pub(super) struct VideoPrefs {
    /// The speed while the video (or the forward key) is held.
    pub(super) hold_speed: RwSignal<f64>,
    /// Whether the controls turn with the video, staying along its bottom edge.
    pub(super) turn_controls: RwSignal<bool>,
}

const HOLD_SPEED_KEY: &str = "ferry-files.hold-speed";
const TURN_CONTROLS_KEY: &str = "ferry-files.turn-controls";
/// The speeds offered for holding.
pub(super) const HOLD_SPEEDS: &[f64] = &[1.5, 2.0, 3.0, 4.0];

impl VideoPrefs {
    pub(super) fn load() -> Self {
        let speed = stored(HOLD_SPEED_KEY).and_then(|s| s.parse().ok()).filter(|s| HOLD_SPEEDS.contains(s)).unwrap_or(2.0);
        VideoPrefs { hold_speed: RwSignal::new(speed), turn_controls: RwSignal::new(stored(TURN_CONTROLS_KEY).as_deref() == Some("1")) }
    }

    pub(super) fn set_hold_speed(self, s: f64) {
        store(HOLD_SPEED_KEY, &s.to_string());
        self.hold_speed.set(s);
    }

    pub(super) fn set_turn_controls(self, on: bool) {
        store(TURN_CONTROLS_KEY, if on { "1" } else { "0" });
        self.turn_controls.set(on);
    }
}

/// A speed as "1.5×".
pub(super) fn speed_label(s: f64) -> String {
    format!("{s}×")
}

/// How long the video is held down (or → / the forward key) before it plays faster.
const HOLD_MS: u64 = 300;
/// Skips: back, and forward.
const BACK_S: f64 = 5.0;
const FORWARD_S: f64 = 10.0;

/// Quarter turns, kept from one picture to the next (and, separately, one video to the next).
/// Counted on past a whole turn so each click turns the short way. Once a turn's animation is
/// over, the count goes back to 0–3 with the animation off for a moment (the same look).
#[derive(Clone, Copy)]
pub(super) struct Turn {
    count: RwSignal<i32>,
    instant: RwSignal<bool>,
}

impl Turn {
    pub(super) fn new() -> Self {
        Self::at(0)
    }

    pub(super) fn at(t: i32) -> Self {
        Turn { count: RwSignal::new(t.rem_euclid(4)), instant: RwSignal::new(false) }
    }

    pub(super) fn get(self) -> i32 {
        self.count.get()
    }

    fn get_untracked(self) -> i32 {
        self.count.get_untracked()
    }

    pub(super) fn by(self, d: i32) {
        self.count.update(|t| *t += d);
    }

    /// For `transitionend`.
    pub(super) fn settle(self) {
        let Some(t) = self.count.try_get_untracked() else { return };
        if (0..4).contains(&t) {
            return;
        }
        self.instant.set(true);
        self.count.set(t.rem_euclid(4));
        set_timeout(move || { let _ = self.instant.try_set(false); }, Duration::from_millis(50));
    }

    /// Added to the style: no animation while settling.
    pub(super) fn css(self) -> &'static str {
        if self.instant.get() { "; transition: none" } else { "" }
    }
}

// ---- Where each video or sound was stopped ----

const RESUME_KEY: &str = "ferry-files.resume";
/// How many are remembered; the ones played longest ago are forgotten first.
const RESUME_KEEP: usize = 300;
/// Stopped before this, or this close to the end: it starts from the beginning next time.
const RESUME_EDGE: f64 = 5.0;

/// What a file is remembered by: its path, size and date (not the device's number, which
/// changes from one connection to the next).
pub(super) fn resume_key(e: &Entry) -> String {
    format!("{}|{}|{}", e.path, e.size, e.mtime)
}

fn resumes() -> serde_json::Map<String, serde_json::Value> {
    stored(RESUME_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn resume_at(key: &str) -> Option<f64> {
    resumes().get(key)?.get(0)?.as_f64()
}

/// Remembers where `m` is, or forgets it when it's near the start or the end.
fn remember(key: &str, m: &web_sys::HtmlMediaElement) {
    let (t, d) = (m.current_time(), m.duration());
    if !d.is_finite() || d <= 0.0 {
        return;
    }
    let mut all = resumes();
    if t >= RESUME_EDGE && t < d - RESUME_EDGE {
        all.insert(key.to_string(), json!([t, js_sys::Date::now()]));
        if all.len() > RESUME_KEEP {
            let mut by_age: Vec<(String, f64)> =
                all.iter().map(|(k, v)| (k.clone(), v.get(1).and_then(|s| s.as_f64()).unwrap_or(0.0))).collect();
            by_age.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (k, _) in by_age.into_iter().take(all.len() - RESUME_KEEP) {
                all.remove(&k);
            }
        }
    } else if all.remove(key).is_none() {
        return;
    }
    store(RESUME_KEY, &serde_json::Value::Object(all).to_string());
}

/// Remembering where a video or sound is as it plays: now and then, when it's paused, and when
/// it's closed. Returns the handlers for `loadedmetadata` (going on from where it was stopped,
/// or from `start`), `timeupdate`, `pause` and `ended`.
#[derive(Clone, Copy)]
struct Resume {
    key: StoredValue<String>,
    last: StoredValue<f64>,
}

impl Resume {
    fn new(key: String, el: StoredValue<Option<web_sys::HtmlMediaElement>, LocalStorage>) -> Self {
        let r = Resume { key: StoredValue::new(key), last: StoredValue::new(0.0) };
        let key = r.key;
        on_cleanup(move || {
            if let (Some(Some(m)), Some(k)) = (el.try_get_value(), key.try_get_value()) {
                remember(&k, &m);
            }
        });
        r
    }

    /// Once the length is known: on from `start`, or from where it was stopped. Says where.
    fn restore(&self, m: &web_sys::HtmlMediaElement, start: Option<f64>) -> Option<f64> {
        let at = start.or_else(|| resume_at(&self.key.get_value()))?;
        let d = m.duration();
        let at = if d.is_finite() { at.min(d - 1.0) } else { at };
        (at > 0.0).then(|| {
            m.set_current_time(at);
            self.last.set_value(at);
            at
        })
    }

    fn tick(&self, m: &web_sys::HtmlMediaElement) {
        if (m.current_time() - self.last.get_value()).abs() >= 5.0 {
            self.save(m);
        }
    }

    fn save(&self, m: &web_sys::HtmlMediaElement) {
        self.last.set_value(m.current_time());
        remember(&self.key.get_value(), m);
    }
}

// ---- The keys (keymap.rs) for the preview's video or sound ----

#[derive(Clone, Copy, PartialEq)]
pub(super) enum MediaKey {
    Play,
    Back,
    /// Pressed: held, it plays at 2× until let go; let go before that, it skips forward.
    ForwardDown,
    ForwardUp,
    Mute,
}

type KeyHandler = Rc<dyn Fn(MediaKey)>;

thread_local! {
    /// The player in each pane's preview: (pane, player, handler).
    static KEYS: RefCell<Vec<(usize, u64, KeyHandler)>> = const { RefCell::new(Vec::new()) };
    static NEXT_PLAYER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Makes `f` take the media keys while a pane shows this player.
fn take_keys(pane: usize, f: impl Fn(MediaKey) + 'static) {
    let id = NEXT_PLAYER.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    KEYS.with(|k| k.borrow_mut().push((pane, id, Rc::new(f))));
    on_cleanup(move || KEYS.with(|k| k.borrow_mut().retain(|(_, i, _)| *i != id)));
}

/// A media key for the player in `pane`'s preview; false when there's none.
pub(super) fn media_key(pane: usize, key: MediaKey) -> bool {
    let f = KEYS.with(|k| k.borrow().iter().rev().find(|(p, _, _)| *p == pane).map(|(_, _, f)| f.clone()));
    match f {
        Some(f) => {
            f(key);
            true
        }
        None => false,
    }
}

fn toggle(m: &web_sys::HtmlMediaElement) {
    if m.paused() {
        let _ = m.play();
    } else {
        let _ = m.pause();
    }
}

fn skip(m: &web_sys::HtmlMediaElement, by: f64) {
    let to = (m.current_time() + by).max(0.0);
    let d = m.duration();
    m.set_current_time(if d.is_finite() { to.min(d) } else { to });
}

/// Fills in the details the header didn't give, from the player.
fn fill_info(info: RwSignal<MediaInfo>, m: &web_sys::HtmlMediaElement) {
    let d = m.duration();
    let size = m.dyn_ref::<web_sys::HtmlVideoElement>().map(|v| (v.video_width(), v.video_height()));
    let _ = info.try_update(|i| {
        if i.duration.is_none() && d.is_finite() && d > 0.0 {
            i.duration = Some(d);
        }
        // 0 × 0 for a file with sound only.
        if let Some((w, h)) = size.filter(|s| s.0 > 0) {
            if i.width.is_none() {
                (i.width, i.height) = (Some(w), Some(h));
            }
        }
    });
}

/// Stops it and lets go of the file, so the web view drops what it buffered.
fn let_go(m: &web_sys::HtmlMediaElement) {
    let _ = m.pause();
    let _ = m.remove_attribute("src");
    m.load();
}

fn target_media(ev: &web_sys::Event) -> Option<web_sys::HtmlMediaElement> {
    ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlMediaElement>().ok())
}

/// A sound, with the web view's own controls. It takes the media keys in `pane` and goes on from
/// where it was stopped.
#[component]
pub(super) fn SoundPlayer(url: String, key: String, info: RwSignal<MediaInfo>, pane: usize) -> impl IntoView {
    let el = StoredValue::new_local(None::<web_sys::HtmlMediaElement>);
    let resume = Resume::new(key, el);
    on_cleanup(move || {
        if let Some(Some(m)) = el.try_get_value() {
            let_go(&m);
        }
    });
    let audio = NodeRef::<html::Audio>::new();
    audio.on_load(move |a| el.set_value(Some(a.into())));
    take_keys(pane, move |k| {
        let Some(m) = el.get_value() else { return };
        match k {
            MediaKey::Play => toggle(&m),
            MediaKey::Back => skip(&m, -BACK_S),
            MediaKey::ForwardDown => skip(&m, FORWARD_S),
            MediaKey::ForwardUp => {}
            MediaKey::Mute => m.set_muted(!m.muted()),
        }
    });
    view! {
        <audio class="play" controls preload="metadata" src=url node_ref=audio
            on:loadedmetadata=move |ev| if let Some(m) = target_media(&ev) {
                fill_info(info, &m);
                resume.restore(&m, None);
            }
            on:timeupdate=move |ev| if let Some(m) = target_media(&ev) { resume.tick(&m) }
            on:pause=move |ev| if let Some(m) = target_media(&ev) { resume.save(&m) }
            on:ended=move |ev| if let Some(m) = target_media(&ev) { resume.save(&m) }></audio>
    }
}

/// What a popped-out player is given: the backend opens a window with it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Popped {
    url: String,
    key: String,
    title: String,
    time: f64,
    turn: i32,
    playing: bool,
    width: f64,
    height: f64,
}

/// The video, turned and scaled to fill its room, with its controls under it.
///
/// `pane`: takes that pane's media keys. `pop`: shows the pop-out button. `start` and `autoplay`:
/// where it starts, and playing (a popped-out one).
#[component]
pub(super) fn VideoPlayer(
    url: String,
    key: String,
    title: String,
    info: RwSignal<MediaInfo>,
    turn: Turn,
    #[prop(optional)] pane: Option<usize>,
    #[prop(optional)] pop: Option<Callback<Popped>>,
    #[prop(optional)] start: Option<f64>,
    #[prop(optional)] autoplay: bool,
) -> impl IntoView {
    let prefs = use_context::<VideoPrefs>().unwrap_or_else(VideoPrefs::load);
    let el = StoredValue::new_local(None::<web_sys::HtmlMediaElement>);
    let resume = Resume::new(key.clone(), el);
    on_cleanup(move || {
        if let Some(Some(m)) = el.try_get_value() {
            let_go(&m);
        }
    });
    let video_ref = NodeRef::<html::Video>::new();
    video_ref.on_load(move |v| el.set_value(Some(v.into())));
    let media = move || el.try_get_value().flatten();

    let playing = RwSignal::new(false);
    let time = RwSignal::new(0.0f64);
    let length = RwSignal::new(0.0f64);
    let muted = RwSignal::new(false);
    let natural = RwSignal::new((0u32, 0u32));

    // A word over the video for a moment ("2×" while held, "+10s" on a skip).
    let badge = RwSignal::new(None::<String>);
    let badge_timer = StoredValue::new_local(None::<TimeoutHandle>);
    let flash = move |what: String| {
        badge.set(Some(what));
        if let Some(t) = badge_timer.try_update_value(Option::take).flatten() {
            t.clear();
        }
        let t = set_timeout_with_handle(move || { let _ = badge.try_set(None); }, Duration::from_millis(900)).ok();
        badge_timer.set_value(t);
    };
    let back = move || {
        if let Some(m) = media() {
            skip(&m, -BACK_S);
            flash(format!("−{BACK_S}s"));
        }
    };
    let forward = move || {
        if let Some(m) = media() {
            skip(&m, FORWARD_S);
            flash(format!("+{FORWARD_S}s"));
        }
    };

    // 2× while the video (or a forward key) is held: Some(whether it was paused) while it is.
    let fast = StoredValue::new_local(None::<bool>);
    let start_fast = move || {
        let Some(m) = media() else { return };
        if fast.get_value().is_some() {
            return;
        }
        fast.set_value(Some(m.paused()));
        let speed = prefs.hold_speed.get_untracked();
        m.set_playback_rate(speed);
        let _ = m.play();
        badge.set(Some(speed_label(speed)));
    };
    let stop_fast = move || {
        let Some(was_paused) = fast.try_update_value(Option::take).flatten() else { return };
        badge.set(None);
        if let Some(m) = media() {
            m.set_playback_rate(1.0);
            if was_paused {
                let _ = m.pause();
            }
        }
    };
    // Waiting to see whether a press is held: the mouse's, and a forward key's.
    let mouse_wait = StoredValue::new_local(None::<TimeoutHandle>);
    let key_wait = StoredValue::new_local(None::<TimeoutHandle>);
    let wait = move |slot: StoredValue<Option<TimeoutHandle>, LocalStorage>| {
        let t = set_timeout_with_handle(
            move || {
                slot.set_value(None);
                start_fast();
            },
            Duration::from_millis(HOLD_MS),
        )
        .ok();
        slot.set_value(t);
    };
    // Whether the wait was still on: a tap rather than a hold.
    let end_wait = move |slot: StoredValue<Option<TimeoutHandle>, LocalStorage>| match slot.try_update_value(Option::take).flatten() {
        Some(t) => {
            t.clear();
            true
        }
        None => false,
    };
    let forward_down = move || {
        if key_wait.get_value().is_none() && fast.get_value().is_none() {
            wait(key_wait);
        }
    };
    let forward_up = move || {
        if end_wait(key_wait) {
            forward();
        } else {
            stop_fast();
        }
    };
    let mute = move || {
        if let Some(m) = media() {
            m.set_muted(!m.muted());
        }
    };
    let play = move || {
        if let Some(m) = media() {
            toggle(&m);
        }
    };
    if let Some(pane) = pane {
        take_keys(pane, move |k| match k {
            MediaKey::Play => play(),
            MediaKey::Back => back(),
            MediaKey::ForwardDown => forward_down(),
            MediaKey::ForwardUp => forward_up(),
            MediaKey::Mute => mute(),
        });
    }

    // The mouse on the picture: a click plays or pauses, but not the one that ends a hold.
    let swallow = StoredValue::new(false);
    let press = move |ev: web_sys::MouseEvent| {
        if ev.button() == 0 {
            wait(mouse_wait);
        }
    };
    let release = move |up: bool| {
        if !end_wait(mouse_wait) && fast.get_value().is_some() {
            stop_fast();
            swallow.set_value(up);
        }
    };
    let click = move |_| {
        if !swallow.try_update_value(std::mem::take).unwrap_or(false) {
            play();
        }
    };
    let double = move |ev: web_sys::MouseEvent| {
        ev.prevent_default();
        let Some(m) = media() else { return };
        let r = m.get_bounding_client_rect();
        if (ev.client_x() as f64) < r.left() + r.width() / 2.0 {
            back();
        } else {
            forward();
        }
    };

    // Full screen: the player with its controls, or else the web view's own for the video.
    let player_ref = NodeRef::<html::Div>::new();
    let full_screen = move || {
        let doc = document();
        let get = |o: &JsValue, k: &str| js_sys::Reflect::get(o, &k.into()).ok().filter(|v| !v.is_undefined() && !v.is_null());
        let call0 = |o: &JsValue, k: &str| {
            get(o, k).and_then(|f| f.dyn_into::<js_sys::Function>().ok()).is_some_and(|f| f.call0(o).is_ok())
        };
        let doc: JsValue = doc.into();
        if get(&doc, "fullscreenElement").or_else(|| get(&doc, "webkitFullscreenElement")).is_some() {
            let _ = call0(&doc, "exitFullscreen") || call0(&doc, "webkitExitFullscreen");
            return;
        }
        let enabled = get(&doc, "fullscreenEnabled").or_else(|| get(&doc, "webkitFullscreenEnabled")).and_then(|v| v.as_bool()).unwrap_or(false);
        if let Some(p) = player_ref.get_untracked() {
            let p: JsValue = p.into();
            if enabled && (call0(&p, "requestFullscreen") || call0(&p, "webkitRequestFullscreen")) {
                return;
            }
        }
        if let Some(m) = media() {
            call0(&m.into(), "webkitEnterFullscreen");
        }
    };

    // The keys while the player has the focus (it's been clicked; a popped-out one always).
    let keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.meta_key() || ev.ctrl_key() || ev.alt_key() {
            return;
        }
        let repeat = ev.repeat();
        let done = match ev.code().as_str() {
            "Space" | "KeyK" if !repeat => {
                play();
                true
            }
            "ArrowLeft" | "KeyJ" if !repeat => {
                back();
                true
            }
            "ArrowRight" | "KeyL" => {
                if !repeat {
                    forward_down();
                }
                true
            }
            "KeyM" if !repeat => {
                mute();
                true
            }
            "KeyF" if !repeat => {
                full_screen();
                true
            }
            "Space" | "KeyK" | "ArrowLeft" | "KeyJ" | "KeyM" | "KeyF" => true,
            _ => false,
        };
        if done {
            ev.prevent_default();
            ev.stop_propagation();
        }
    };
    let keyup = move |ev: web_sys::KeyboardEvent| {
        if matches!(ev.code().as_str(), "ArrowRight" | "KeyL") {
            ev.prevent_default();
            ev.stop_propagation();
            forward_up();
        }
    };

    // The room the video has and the size of its box, measured again when either changes, so a
    // video on its side is scaled to fill the room.
    let stage = NodeRef::<html::Div>::new();
    let sizes = RwSignal::new(((0.0f64, 0.0f64), (0.0f64, 0.0f64)));
    // The whole player's room, for turning the controls with the video.
    let room = RwSignal::new((0.0f64, 0.0f64));
    let measure = move || {
        if let Some(p) = player_ref.try_get_untracked().flatten() {
            let _ = room.try_set((p.client_width() as f64, p.client_height() as f64));
        }
        let (Some(s), Some(v)) = (stage.try_get_untracked().flatten(), video_ref.try_get_untracked().flatten()) else { return };
        let _ = sizes.try_set(((s.client_width() as f64, s.client_height() as f64), (v.offset_width() as f64, v.offset_height() as f64)));
    };
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let (Some(s), Some(v), Some(p)) = (stage.get(), video_ref.get(), player_ref.get()) else { return };
        let cb = Closure::<dyn Fn()>::new(measure);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&s);
            ro.observe(&v);
            ro.observe(&p);
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
    // The video's turn, when the controls stay level; with them turned too, the box holding both
    // turns instead, swapping its width and height on its side, so the video fits as it is.
    let video_turn = move || if prefs.turn_controls.get() { 0 } else { turn.get() };
    let box_style = move || {
        let t = turn.get();
        if !prefs.turn_controls.get() || t == 0 {
            return String::new();
        }
        let (w, h) = room.get();
        let size = if t.rem_euclid(2) == 1 { format!("width: {h}px; height: {w}px; ") } else { String::new() };
        format!("{size}transform: translate(-50%, -50%) rotate({}deg){}", t * 90, turn.css())
    };
    let style = move || {
        if prefs.turn_controls.get() {
            return "transform: none".to_string();
        }
        let ((cw, ch), (ew, eh)) = sizes.get();
        let (vw, vh) = natural.get();
        let t = turn.get();
        let s = if t.rem_euclid(2) == 1 && vw > 0 && vh > 0 && ew > 0.0 && eh > 0.0 {
            let (vw, vh) = (vw as f64, vh as f64);
            // The picture inside the box now, and as big as it can be on its side in the room.
            let now = (ew / vw).min(eh / vh);
            let turned = (cw / vh).min(ch / vw);
            turned / now
        } else {
            1.0
        };
        format!("transform: rotate({}deg) scale({s}){}", t * 90, turn.css())
    };

    let loaded = move |ev: web_sys::Event| {
        let Some(m) = target_media(&ev) else { return };
        if let Ok(v) = m.clone().dyn_into::<web_sys::HtmlVideoElement>() {
            natural.set((v.video_width(), v.video_height()));
        }
        fill_info(info, &m);
        let d = m.duration();
        length.set(if d.is_finite() { d } else { 0.0 });
        if let Some(at) = resume.restore(&m, start) {
            if start.is_none() {
                flash(format!("Going on from {}", clock(at)));
            }
        }
        if autoplay {
            let _ = m.play();
        }
        request_animation_frame(measure);
    };

    // Hovering the seek bar: where (px from its left) and when. The moment is shown by a second,
    // silent copy of the video, loaded the first time the bar is hovered.
    let hover = RwSignal::new(None::<(f64, f64, f64)>);
    let peek_on = RwSignal::new(false);
    let peek_ref = NodeRef::<html::Video>::new();
    let peek_next = StoredValue::new(None::<f64>);
    let peek_to = move |t: f64| {
        let Some(p) = peek_ref.get_untracked() else {
            peek_next.set_value(Some(t));
            return;
        };
        if p.seeking() || p.ready_state() == 0 {
            peek_next.set_value(Some(t));
        } else {
            p.set_current_time(t);
        }
    };
    let peek_ready = move |_| {
        if let (Some(t), Some(p)) = (peek_next.try_update_value(Option::take).flatten(), peek_ref.get_untracked()) {
            p.set_current_time(t);
        }
    };
    let over_bar = move |ev: web_sys::MouseEvent| {
        // Along the bar, in its own terms (offsetX), so this works when it's turned with the video.
        let Some(bar) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok()) else { return };
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let (d, w) = (length.get_untracked(), bar.offset_width() as f64);
        if d <= 0.0 || w <= 0.0 {
            return;
        }
        let x = (ev.offset_x() as f64 + input.offset_left() as f64 - bar.offset_left() as f64).clamp(0.0, w);
        let t = x / w * d;
        peek_on.set(true);
        hover.set(Some((x, w, t)));
        peek_to(t);
    };
    // The frame's box, as the video is turned: at most 160 × 120.
    let tip_box = move || {
        let (vw, vh) = natural.get();
        let a = if vw > 0 && vh > 0 { vw as f64 / vh as f64 } else { 16.0 / 9.0 };
        let a = if video_turn().rem_euclid(2) == 1 { 1.0 / a } else { a };
        if a >= 160.0 / 120.0 { (160.0, 160.0 / a) } else { (120.0 * a, 120.0) }
    };
    let tip = move || {
        let (x, w, t) = hover.get()?;
        let (bw, bh) = tip_box();
        let left = x.clamp(bw / 2.0, (w - bw / 2.0).max(bw / 2.0));
        let odd = video_turn().rem_euclid(2) == 1;
        let (iw, ih) = if odd { (bh, bw) } else { (bw, bh) };
        Some((left, bw, bh, iw, ih, t))
    };
    let peek_url = url.clone();
    let video_url = url.clone();

    let pop_out = move || {
        let (Some(pop), Some(m)) = (pop, media()) else { return };
        let (vw, vh) = natural.get_untracked();
        let (vw, vh) = if vw > 0 && vh > 0 { (vw as f64, vh as f64) } else { (16.0, 9.0) };
        let (vw, vh) = if turn.get_untracked().rem_euclid(2) == 1 { (vh, vw) } else { (vw, vh) };
        // About 480 px across (less for a tall one), plus the controls: at the side of a video on
        // its side when they turn with it.
        let scale = (480.0 / vw).min(480.0 / vh);
        let side = prefs.turn_controls.get_untracked() && turn.get_untracked().rem_euclid(2) == 1;
        let (cw, ch) = if side { (CONTROLS_H, 0.0) } else { (0.0, CONTROLS_H) };
        let playing = !m.paused();
        let _ = m.pause();
        resume.save(&m);
        pop.run(Popped {
            url: url.clone(),
            key: key.clone(),
            title: title.clone(),
            time: m.current_time(),
            turn: turn.get_untracked().rem_euclid(4),
            playing,
            width: (vw * scale).round() + cw,
            height: (vh * scale).round() + ch,
        });
    };
    let has_pop = pop.is_some();
    let quiet = |ev: web_sys::MouseEvent| ev.prevent_default();

    view! {
        <div class="vplayer" tabindex="0" node_ref=player_ref on:keydown=keydown on:keyup=keyup>
            <div class="vbox" class:turned=move || prefs.turn_controls.get() && turn.get() != 0 style=box_style
                on:transitionend=move |ev| if ev.target() == ev.current_target() { turn.settle() }>
            <div class="vstage" node_ref=stage>
                <video class="play" preload="metadata" src=video_url node_ref=video_ref style=style
                    on:loadedmetadata=loaded
                    on:durationchange=move |ev| if let Some(m) = target_media(&ev) {
                        let d = m.duration();
                        length.set(if d.is_finite() { d } else { 0.0 });
                    }
                    on:timeupdate=move |ev| if let Some(m) = target_media(&ev) {
                        time.set(m.current_time());
                        resume.tick(&m);
                    }
                    on:play=move |_| playing.set(true)
                    on:pause=move |ev| {
                        playing.set(false);
                        if let Some(m) = target_media(&ev) { resume.save(&m) }
                    }
                    on:ended=move |ev| if let Some(m) = target_media(&ev) { resume.save(&m) }
                    on:volumechange=move |ev| if let Some(m) = target_media(&ev) { muted.set(m.muted()) }
                    on:mousedown=press on:mouseup=move |_| release(true) on:mouseleave=move |_| release(false)
                    on:click=click on:dblclick=double on:transitionend=move |_| turn.settle()></video>
                {move || badge.get().map(|b| view! { <span class="badge">{b}</span> })}
            </div>
            <div class="vcontrols">
                <div class="seek" on:mousemove=over_bar on:mouseleave=move |_| hover.set(None)>
                    <input type="range" min="0" step="any" aria-label="Where in the video"
                        prop:max=move || length.get().max(0.001)
                        prop:value=move || time.get()
                        on:input=move |ev| if let Some(m) = media() {
                            let v = event_target_value(&ev).parse::<f64>().unwrap_or(0.0);
                            m.set_current_time(v);
                            time.set(v);
                        }/>
                    {move || peek_on.get().then(|| {
                        let u = peek_url.clone();
                        view! {
                            <div class="peek-tip" style:display=move || if tip().is_some() { "block" } else { "none" }
                                style:left=move || tip().map(|t| format!("{}px", t.0)).unwrap_or_default()>
                                <div class="peek-frame" style:width=move || format!("{}px", tip_box().0) style:height=move || format!("{}px", tip_box().1)>
                                    <video muted preload="auto" src=u node_ref=peek_ref on:seeked=peek_ready on:loadeddata=peek_ready
                                        style:width=move || tip().map(|t| format!("{}px", t.3)).unwrap_or_default()
                                        style:height=move || tip().map(|t| format!("{}px", t.4)).unwrap_or_default()
                                        style:transform=move || format!("translate(-50%, -50%) rotate({}deg)", video_turn() * 90)></video>
                                </div>
                                <span class="peek-time">{move || tip().map(|t| clock(t.5))}</span>
                            </div>
                        }
                    })}
                </div>
                <div class="vbuttons">
                    <button class="btn icon-only" on:mousedown=quiet on:click=move |_| play()
                        title=move || if playing.get() { "Pause (Space, or K)" } else { "Play (Space, or K)" }
                        aria-label=move || if playing.get() { "Pause" } else { "Play" }>
                        {move || if playing.get() { ico(Icon::Pause).into_any() } else { ico(Icon::Play).into_any() }}
                    </button>
                    <span class="vtime">{move || format!("{} / {}", clock(time.get()), clock(length.get()))}</span>
                    <span class="grow"></span>
                    <button class="btn icon-only" on:mousedown=quiet on:click=move |_| mute()
                        title=move || if muted.get() { "Sound on (M)" } else { "Sound off (M)" }
                        aria-label=move || if muted.get() { "Sound on" } else { "Sound off" }>
                        {move || if muted.get() { ico(Icon::SoundOff).into_any() } else { ico(Icon::SoundOn).into_any() }}
                    </button>
                    <span class="btn-group" role="group">
                        <button class="btn icon-only" title="Turn left" aria-label="Turn left" on:mousedown=quiet on:click=move |_| turn.by(-1)>{ico(Icon::TurnLeft)}</button>
                        <button class="btn icon-only" title="Turn right" aria-label="Turn right" on:mousedown=quiet on:click=move |_| turn.by(1)>{ico(Icon::TurnRight)}</button>
                    </span>
                    {has_pop.then(|| view! {
                        <button class="btn icon-only" title="Pop out: play it in a small window that stays on top" aria-label="Pop out"
                            on:mousedown=quiet on:click=move |_| pop_out()>{ico(Icon::PopOut)}</button>
                    })}
                    <button class="btn icon-only" title="Full screen (F)" aria-label="Full screen" on:mousedown=quiet on:click=move |_| full_screen()>{ico(Icon::FullScreen)}</button>
                </div>
            </div>
            </div>
        </div>
    }
}

/// The height of a video's controls, added to a popped-out window's.
const CONTROLS_H: f64 = 64.0;

/// A time in seconds as "1:02:03" or "2:03".
fn clock(secs: f64) -> String {
    let s = if secs.is_finite() { secs.max(0.0).floor() as u64 } else { 0 };
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// The video this window was opened for, when it's a popped-out player (the backend sets it).
pub fn popped_out() -> Option<Popped> {
    let v = js_sys::Reflect::get(&window(), &"__FERRY_PLAYER".into()).ok()?.as_string()?;
    serde_json::from_str(&v).ok()
}

/// A popped-out player: the video alone, filling its window, with the focus so its keys work.
#[component]
pub fn PopOut(video: Popped) -> impl IntoView {
    apply_theme(&stored(THEME_KEY).unwrap_or_default());
    provide_context(VideoPrefs::load());
    document().set_title(&video.title);
    let info = RwSignal::new(MediaInfo::default());
    let turn = Turn::at(video.turn);
    let start = Some(video.time).filter(|t| *t > 0.0);
    request_animation_frame(|| {
        if let Some(p) = document().query_selector(".vplayer").ok().flatten().and_then(|p| p.dyn_into::<web_sys::HtmlElement>().ok()) {
            let _ = p.focus();
        }
    });
    view! {
        <div class="popout">
            {match start {
                Some(start) => view! { <VideoPlayer url=video.url key=video.key title=video.title info turn start autoplay=video.playing/> }.into_any(),
                None => view! { <VideoPlayer url=video.url key=video.key title=video.title info turn autoplay=video.playing/> }.into_any(),
            }}
        </div>
    }
}
