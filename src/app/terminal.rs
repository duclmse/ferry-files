//! The terminal under a pane: a shell in a pseudo-terminal on the pane's device (this computer, a
//! phone over adb, an SFTP server over ssh), so programs can ask for input and draw full-screen.
//!
//! The backend sends what the shell prints as "term" events; `vt100` keeps the screen (and 5,000
//! lines of scrollback), drawn as HTML. Hiding the terminal keeps the shell running. What it
//! printed is also kept (the last `KEEP` bytes) per pane in the app's storage, so after a restart
//! the terminal shows it again above a new shell.

use super::*;
use base64::Engine;
use std::cell::RefCell;
use wasm_bindgen::closure::Closure;

/// How much output each pane's terminal keeps for after a restart.
const KEEP: usize = 256 * 1024;
const SCROLLBACK: usize = 5000;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn keep_key(pane: usize) -> String {
    format!("ferry-files.terminal.{pane}")
}

/// A pane's terminal: the screen, the output kept, and the shell running in it.
struct Screen {
    parser: vt100::Parser,
    log: Vec<u8>,
    /// The shell's id in the backend, while it runs.
    session: Option<u32>,
    /// Where it runs, as shown in the terminal's title.
    place: String,
    /// The output kept hasn't been stored yet.
    unsaved: bool,
    /// Keys typed while the shell was starting, sent once it runs.
    typed: Vec<u8>,
}

impl Screen {
    fn new(rows: u16, cols: u16) -> Screen {
        Screen {
            parser: vt100::Parser::new(nz(rows), nz(cols), SCROLLBACK),
            log: vec![],
            session: None,
            place: String::new(),
            unsaved: false,
            typed: vec![],
        }
    }

    fn print(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.log.extend_from_slice(bytes);
        if self.log.len() > KEEP {
            // From the start of a line, so it's replayed from a clean place.
            let cut = self.log.len() - KEEP;
            let at = self.log[cut..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(cut, |i| cut + i + 1);
            self.log.drain(..at);
        }
        self.unsaved = true;
    }
}

fn nz(n: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(n.max(1)).unwrap_or(std::num::NonZeroU16::MIN)
}

thread_local! {
    static SCREENS: RefCell<HashMap<usize, Screen>> = RefCell::new(HashMap::new());
}

/// Does `f` with pane `pane`'s terminal, made (with what was kept from before) when it has none.
fn with_screen<R>(pane: usize, f: impl FnOnce(&mut Screen) -> R) -> R {
    SCREENS.with(|s| {
        let mut s = s.borrow_mut();
        let screen = s.entry(pane).or_insert_with(|| {
            let mut sc = Screen::new(24, 80);
            if let Some(kept) = stored(&keep_key(pane))
                .and_then(|t| B64.decode(t).ok())
                .filter(|k| !k.is_empty())
            {
                sc.print(&kept);
                sc.print(
                    b"\x1b[0m\r\n\x1b[2m[Above: output from before the app was closed]\x1b[0m\r\n",
                );
                sc.unsaved = false;
            }
            sc
        });
        f(screen)
    })
}

fn pane_of(session: u32) -> Option<usize> {
    SCREENS.with(|s| {
        s.borrow()
            .iter()
            .find(|(_, sc)| sc.session == Some(session))
            .map(|(p, _)| *p)
    })
}

#[derive(serde::Deserialize)]
struct TermOut {
    id: u32,
    data: String,
}

#[derive(serde::Deserialize)]
struct TermExit {
    id: u32,
    code: Option<u32>,
}

/// The bytes a key sends to the shell, or None for keys left to the app (⌘ shortcuts).
fn key_bytes(e: &web_sys::KeyboardEvent, app_cursor: bool) -> Option<Vec<u8>> {
    if e.meta_key() {
        return None;
    }
    let key = e.key();
    let csi = |s: &str| format!("\x1b[{s}").into_bytes();
    let arrow = |c: char| {
        if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let bytes = match key.as_str() {
        "Enter" => b"\r".to_vec(),
        "Backspace" => {
            if e.alt_key() {
                b"\x1b\x7f".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        "Tab" if e.shift_key() => csi("Z"),
        "Tab" => b"\t".to_vec(),
        "Escape" => b"\x1b".to_vec(),
        "ArrowUp" => arrow('A'),
        "ArrowDown" => arrow('B'),
        "ArrowRight" => arrow('C'),
        "ArrowLeft" => arrow('D'),
        "Home" => arrow('H'),
        "End" => arrow('F'),
        "PageUp" => csi("5~"),
        "PageDown" => csi("6~"),
        "Delete" => csi("3~"),
        "Insert" => csi("2~"),
        "F1" => b"\x1bOP".to_vec(),
        "F2" => b"\x1bOQ".to_vec(),
        "F3" => b"\x1bOR".to_vec(),
        "F4" => b"\x1bOS".to_vec(),
        f if f.len() > 1 && f.starts_with('F') => {
            let n = match f[1..].parse::<u8>().ok()? {
                5 => "15",
                6 => "17",
                7 => "18",
                8 => "19",
                9 => "20",
                10 => "21",
                11 => "23",
                12 => "24",
                _ => return None,
            };
            csi(&format!("{n}~"))
        }
        k if k.chars().count() == 1 => {
            let c = k.chars().next()?;
            let mut b = vec![];
            if e.alt_key() {
                b.push(0x1b);
            }
            if e.ctrl_key() {
                // Ctrl+letter and the few others that have a control code.
                match c.to_ascii_lowercase() {
                    l @ 'a'..='z' => b.push(l as u8 - b'a' + 1),
                    '@' | ' ' | '2' => b.push(0),
                    '[' | '3' => b.push(0x1b),
                    '\\' | '4' => b.push(0x1c),
                    ']' | '5' => b.push(0x1d),
                    '^' | '6' => b.push(0x1e),
                    '_' | '-' | '7' => b.push(0x1f),
                    '?' | '8' => b.push(0x7f),
                    _ => return None,
                }
            } else {
                b.extend_from_slice(c.to_string().as_bytes());
            }
            b
        }
        _ => return None,
    };
    Some(bytes)
}

/// The CSS colour of terminal colour `c`; None for the default.
fn colour(c: vt100::Color) -> Option<String> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i @ 0..=15) => Some(format!("var(--ansi-{i})")),
        vt100::Color::Idx(i @ 16..=231) => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Some(format!(
                "rgb({},{},{})",
                level(i / 36),
                level(i / 6 % 6),
                level(i % 6)
            ))
        }
        vt100::Color::Idx(i) => {
            let g = 8 + (i - 232) * 10;
            Some(format!("rgb({g},{g},{g})"))
        }
        vt100::Color::Rgb(r, g, b) => Some(format!("rgb({r},{g},{b})")),
    }
}

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// The screen as HTML: a line per row, runs of the same look in spans. Text is escaped, and
/// styles are only colours made here.
fn screen_html(screen: &vt100::Screen, focused: bool) -> String {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let (cr, cc) = screen.cursor_position();
    let show_cursor = !screen.hide_cursor() && screen.scrollback() == 0;
    let mut out = String::with_capacity(rows as usize * (cols as usize + 16));
    for r in 0..rows {
        out.push_str("<div>");
        let mut open: Option<String> = None;
        let mut run = String::new();
        let flush = |out: &mut String, open: &Option<String>, run: &mut String| {
            if run.is_empty() {
                return;
            }
            match open {
                Some(style) if !style.is_empty() => {
                    out.push_str("<span ");
                    out.push_str(style);
                    out.push('>');
                    out.push_str(run);
                    out.push_str("</span>");
                }
                _ => out.push_str(run),
            }
            run.clear();
        };
        // The last cell with something in it, so rows don't end in spaces to copy.
        let last = (0..cols).rev().find(|&c| {
            screen.cell(r, c).is_some_and(|x| {
                x.has_contents() || x.bgcolor() != vt100::Color::Default || x.inverse()
            })
        });
        let end = match (last, show_cursor && cr == r) {
            (Some(l), true) => l.max(cc),
            (Some(l), false) => l,
            (None, true) => cc,
            (None, false) => {
                out.push_str("</div>");
                continue;
            }
        };
        for c in 0..=end.min(cols - 1) {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let (mut fg, mut bg) = (colour(cell.fgcolor()), colour(cell.bgcolor()));
            if cell.inverse() {
                (fg, bg) = (
                    Some(bg.unwrap_or_else(|| "var(--panel)".into())),
                    Some(fg.unwrap_or_else(|| "var(--ink)".into())),
                );
            }
            let mut class = String::new();
            for (on, name) in [
                (cell.bold(), "b"),
                (cell.dim(), "d"),
                (cell.italic(), "i"),
                (cell.underline(), "u"),
            ] {
                if on {
                    class.push_str(name);
                    class.push(' ');
                }
            }
            if show_cursor && r == cr && c == cc {
                class.push_str(if focused { "cur" } else { "cur off" });
            }
            let mut style = String::new();
            if !class.is_empty() {
                style.push_str(&format!("class=\"{}\" ", class.trim_end()));
            }
            if fg.is_some() || bg.is_some() {
                style.push_str("style=\"");
                if let Some(f) = fg {
                    style.push_str(&format!("color:{f};"));
                }
                if let Some(b) = bg {
                    style.push_str(&format!("background:{b};"));
                }
                style.push('"');
            }
            let style = style.trim_end().to_string();
            if open.as_deref() != Some(style.as_str()) {
                flush(&mut out, &open, &mut run);
                open = Some(style);
            }
            let text = cell.contents();
            escape_into(&mut run, if text.is_empty() { " " } else { text });
        }
        flush(&mut out, &open, &mut run);
        out.push_str("</div>");
    }
    out
}

impl Ctx {
    /// Whether `src` has a command line: this computer, a phone over adb, an SFTP server.
    pub(super) fn has_shell(self, src: Src) -> bool {
        src == Src::Mac
            || self
                .src_info(src)
                .is_some_and(|i| matches!(i.transport.as_str(), "adb" | "sftp"))
    }

    /// Shows or hides the terminal under the pane. Hidden, its shell keeps running.
    pub(super) fn toggle_terminal(self, p: Pane) {
        let running = p.term_id.get_untracked().is_some();
        if !running && !p.term.get_untracked() && !self.has_shell(p.src.get_untracked()) {
            p.error("No terminal here. It works on this computer, adb phones and SFTP servers.");
            return;
        }
        p.term.update(|t| *t = !*t);
    }

    /// Shows the terminal and goes to `dir` in its shell. A shell that's starting (or starts when
    /// the terminal is shown) gets the cd once it runs.
    pub(super) fn cd_in_terminal(self, p: Pane, dir: String) {
        let cd = cd_line(p.src.get_untracked(), &dir).into_bytes();
        if p.term_id.get_untracked().is_some() || p.term_starting.get_value() {
            self.type_in(p, cd);
        } else if dir != p.cwd.get_untracked() {
            with_screen(p.id, |s| s.typed.extend_from_slice(&cd));
        }
        p.term.set(true);
    }

    /// Listens for the terminals' output, and stores what they printed now and then. Once, at launch.
    pub(super) fn watch_terminals(self) {
        crate::api::listen("term", move |o: TermOut| {
            let Ok(bytes) = B64.decode(o.data) else {
                return;
            };
            if let Some(pane) = pane_of(o.id) {
                with_screen(pane, |s| s.print(&bytes));
                self.panes[pane].term_tick.update(|t| *t += 1);
            }
        });
        crate::api::listen("term_exit", move |o: TermExit| {
            if let Some(pane) = pane_of(o.id) {
                let note = match o.code {
                    Some(0) | None => "\r\n\x1b[0;2m[The shell ended. Press a key for a new one.]\x1b[0m\r\n".to_string(),
                    Some(c) => format!("\r\n\x1b[0;2m[The shell ended with code {c}. Press a key for a new one.]\x1b[0m\r\n"),
                };
                with_screen(pane, |s| {
                    s.session = None;
                    s.print(note.as_bytes());
                });
                let p = self.panes[pane];
                p.term_id.set(None);
                p.term_tick.update(|t| *t += 1);
            }
        });
        let _ = set_interval_with_handle(
            move || {
                for p in self.panes {
                    let log = with_screen(p.id, |s| {
                        std::mem::take(&mut s.unsaved).then(|| B64.encode(&s.log))
                    });
                    if let Some(log) = log {
                        store(&keep_key(p.id), &log);
                    }
                }
            },
            std::time::Duration::from_secs(2),
        );
    }

    /// Starts a shell in the pane's folder, `rows` by `cols`.
    fn start_shell(self, p: Pane, rows: u16, cols: u16) {
        if p.term_id.get_untracked().is_some() || p.term_starting.get_value() {
            return;
        }
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        if !self.has_shell(src) {
            with_screen(p.id, |s| {
                s.print(b"\x1b[0;2m[This device has no command line.]\x1b[0m\r\n")
            });
            p.term_tick.update(|t| *t += 1);
            return;
        }
        let place = match src {
            Src::Mac => self.mac.get_untracked().map(|i| i.label),
            src => self.src_info(src).map(|i| i.label),
        }
        .unwrap_or_default();
        p.term_starting.set_value(true);
        spawn_local(async move {
            let r = call::<u32>(
                "term_open",
                json!({ "src": src.key(), "dir": dir, "rows": rows, "cols": cols }),
            )
            .await;
            p.term_starting.set_value(false);
            match r {
                Ok(id) => {
                    let typed = with_screen(p.id, |s| {
                        s.session = Some(id);
                        s.place = place;
                        std::mem::take(&mut s.typed)
                    });
                    p.term_id.set(Some(id));
                    if !typed.is_empty() {
                        self.type_in(p, typed);
                    }
                }
                Err(e) => {
                    with_screen(p.id, |s| {
                        s.print(format!("\x1b[0;31m{e}\x1b[0m\r\n").as_bytes())
                    });
                    p.term_tick.update(|t| *t += 1);
                }
            }
        });
    }

    /// Types `bytes` into the pane's shell. While the shell is starting, they wait for it.
    fn type_in(self, p: Pane, bytes: Vec<u8>) {
        let Some(id) = p.term_id.get_untracked() else {
            if p.term_starting.get_value() {
                with_screen(p.id, |s| s.typed.extend_from_slice(&bytes));
            }
            return;
        };
        // Keys are text or escape sequences, all UTF-8.
        let data = String::from_utf8_lossy(&bytes).into_owned();
        with_screen(p.id, |s| s.parser.screen_mut().set_scrollback(0));
        spawn_local(async move {
            let _ = call::<()>("term_write", json!({ "id": id, "data": data })).await;
        });
    }

    /// Ends the pane's shell.
    fn end_shell(self, p: Pane) {
        if let Some(id) = p.term_id.get_untracked() {
            spawn_local(async move {
                let _ = call::<()>("term_close", json!({ "id": id })).await;
            });
        }
    }
}

#[component]
pub(super) fn Terminal(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let screen_el = NodeRef::<html::Div>::new();
    let measure_el = NodeRef::<html::Span>::new();
    let focused = RwSignal::new(false);
    // The terminal's size in characters, from its size on screen.
    let size = RwSignal::new((24u16, 80u16));
    let fit = move || {
        let (Some(el), Some(m)) = (screen_el.get_untracked(), measure_el.get_untracked()) else {
            return;
        };
        let r = m.get_bounding_client_rect();
        let (cw, ch) = (r.width() / 10.0, r.height());
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let cols = ((el.client_width() as f64 - 16.0) / cw)
            .floor()
            .clamp(10.0, 1000.0) as u16;
        let rows = ((el.client_height() as f64 - 8.0) / ch)
            .floor()
            .clamp(2.0, 500.0) as u16;
        if size.get_untracked() != (rows, cols) {
            size.set((rows, cols));
        }
    };
    // Sizes the screen (and the shell) to fit.
    Effect::new(move |_| {
        let (rows, cols) = size.get();
        with_screen(p.id, |s| s.parser.screen_mut().set_size(nz(rows), nz(cols)));
        p.term_tick.update(|t| *t += 1);
        if let Some(id) = p.term_id.get_untracked() {
            spawn_local(async move {
                let _ = call::<()>(
                    "term_resize",
                    json!({ "id": id, "rows": rows, "cols": cols }),
                )
                .await;
            });
        }
    });
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(el) = screen_el.get() else { return };
        let cb = Closure::<dyn Fn()>::new(fit);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&el);
            if let Some((old, _)) = watcher.try_update_value(|w| w.replace((ro, cb))).flatten() {
                old.disconnect();
            }
        }
        fit();
        // A shell starts when the terminal is shown without one (and, after one ended, at a key).
        if p.term_id.get_untracked().is_none() {
            let (rows, cols) = size.get_untracked();
            ctx.start_shell(p, rows, cols);
        }
        request_animation_frame(move || {
            let _ = el.focus();
        });
    });
    on_cleanup(move || {
        if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
            ro.disconnect();
        }
    });
    // Drawn again at each output, key, scroll and resize.
    Effect::new(move |_| {
        p.term_tick.track();
        let f = focused.get();
        let Some(el) = screen_el.get() else { return };
        let html = with_screen(p.id, |s| screen_html(s.parser.screen(), f));
        el.set_inner_html(&html);
    });
    let keydown = move |e: web_sys::KeyboardEvent| {
        if e.is_composing() {
            return;
        }
        let app_cursor = with_screen(p.id, |s| s.parser.screen().application_cursor());
        let Some(bytes) = key_bytes(&e, app_cursor) else {
            return;
        };
        e.prevent_default();
        e.stop_propagation();
        // With no shell, a key starts one; keys typed while it starts are sent once it runs.
        if p.term_id.get_untracked().is_none() && !p.term_starting.get_value() {
            let (rows, cols) = size.get_untracked();
            ctx.start_shell(p, rows, cols);
            return;
        }
        ctx.type_in(p, bytes);
    };
    let paste = move |e: web_sys::ClipboardEvent| {
        let Some(text) = e
            .clipboard_data()
            .and_then(|d| d.get_data("text/plain").ok())
        else {
            return;
        };
        e.prevent_default();
        let bracketed = with_screen(p.id, |s| s.parser.screen().bracketed_paste());
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let data = if bracketed {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        ctx.type_in(p, data.into_bytes());
    };
    let wheel = move |e: web_sys::WheelEvent| {
        let alt = with_screen(p.id, |s| s.parser.screen().alternate_screen());
        if alt {
            return;
        }
        e.prevent_default();
        let lines = (e.delta_y() / 16.0).round() as isize;
        with_screen(p.id, |s| {
            let screen = s.parser.screen_mut();
            let at = screen.scrollback() as isize - lines;
            screen.set_scrollback(at.max(0) as usize);
        });
        p.term_tick.update(|t| *t += 1);
    };
    let clear = move |_| {
        with_screen(p.id, |s| {
            s.parser.process(b"\x1b[H\x1b[2J");
            s.parser.screen_mut().clear_scrollback();
            s.log.clear();
            s.unsaved = true;
        });
        // The prompt again, on the cleared screen.
        if p.term_id.get_untracked().is_some() {
            ctx.type_in(p, b"\x0c".to_vec());
        }
        p.term_tick.update(|t| *t += 1);
    };
    let cd_here = move |_| {
        let dir = p.cwd.get_untracked();
        ctx.type_in(p, cd_line(p.src.get_untracked(), &dir).into_bytes());
        if let Some(el) = screen_el.get_untracked() {
            let _ = el.focus();
        }
    };
    let place = move || {
        p.term_tick.track();
        p.term_id.track();
        with_screen(p.id, |s| s.place.clone())
    };
    let scrolled = move || {
        p.term_tick.track();
        with_screen(p.id, |s| s.parser.screen().scrollback())
    };
    view! {
        <div class="term" on:mousedown=move |e| e.stop_propagation() on:click=move |_| ctx.active.set(p.id)>
            <div class="term-head">
                <span class="where">{move || match (p.term_id.get(), place()) {
                    (Some(_), place) if !place.is_empty() => place,
                    (Some(_), _) => "Terminal".to_string(),
                    (None, _) => "Not running".to_string(),
                }}</span>
                {move || (scrolled() > 0).then(|| view! {
                    <button class="btn small" on:click=move |_| { with_screen(p.id, |s| s.parser.screen_mut().set_scrollback(0)); p.term_tick.update(|t| *t += 1) }>"Back to the bottom"</button>
                })}
                <button class="btn icon-only small" title="cd here: type cd and the pane's folder into the shell" aria-label="cd here"
                    disabled=move || p.term_id.get().is_none() on:click=cd_here>{ico(Icon::Folder)}</button>
                <button class="btn icon-only small" title="Clear" aria-label="Clear the terminal" on:click=clear>{ico(Icon::Trash)}</button>
                {move || match p.term_id.get() {
                    Some(_) => view! { <button class="btn icon-only small" title="End the shell" aria-label="End the shell" on:click=move |_| ctx.end_shell(p)>{ico(Icon::Stop)}</button> }.into_any(),
                    None => view! { <button class="btn small" title="Start a new shell in the pane's folder"
                        on:click=move |_| { let (r, c) = size.get_untracked(); ctx.start_shell(p, r, c) }>"New shell"</button> }.into_any(),
                }}
                <button class="btn icon-only small" title="Hide the terminal (the shell keeps running)" aria-label="Hide the terminal"
                    on:click=move |_| p.term.set(false)>{ico(Icon::Close)}</button>
            </div>
            <div class="term-screen" node_ref=screen_el tabindex="0" role="textbox" aria-label="Terminal" aria-multiline="true"
                on:keydown=keydown on:paste=paste on:wheel=wheel
                on:focus=move |_| focused.set(true) on:blur=move |_| focused.set(false)></div>
            <span class="term-measure" node_ref=measure_el aria-hidden="true">"MMMMMMMMMM"</span>
        </div>
    }
}

/// The line that takes `src`'s shell to `dir`: the computer's own shell as it writes folders
/// (PowerShell on Windows); phones over adb and SFTP servers have a POSIX shell.
fn cd_line(src: Src, dir: &str) -> String {
    if src == Src::Mac {
        crate::os::cd_line(dir)
    } else {
        crate::os::posix_cd(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_the_screen() {
        let mut s = Screen::new(3, 20);
        s.print(b"hi \x1b[1;31mred\x1b[0m <b>\r\nnext");
        let html = screen_html(s.parser.screen(), true);
        assert!(html.starts_with("<div>hi <span class=\"b\" style=\"color:var(--ansi-1);\">red</span> &lt;b&gt;</div>"), "{html}");
        assert!(html.contains("next<span class=\"cur\"> </span>"), "{html}");
        assert_eq!(colour(vt100::Color::Idx(196)).unwrap(), "rgb(255,0,0)");
        assert_eq!(colour(vt100::Color::Idx(232)).unwrap(), "rgb(8,8,8)");
    }

    #[test]
    fn keeps_the_last_output() {
        let mut s = Screen::new(3, 20);
        let line = [b'x'; 1023]
            .iter()
            .copied()
            .chain([b'\n'])
            .collect::<Vec<_>>();
        for _ in 0..300 {
            s.print(&line);
        }
        assert!(s.log.len() <= KEEP);
        assert_eq!(s.log.len() % 1024, 0);
    }
}
