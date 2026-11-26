//! The hex view: rows read in chunks as they're scrolled to, over a space as tall as the file.

use super::*;

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
        HexGeom {
            rows,
            fit,
            last: rows.saturating_sub(fit),
            space,
            max_px: (space - view_h).max(0.0),
        }
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
pub(super) fn HexPeek(
    src: Src,
    entry: Entry,
    #[prop(optional)] inner: Option<Inner>,
) -> impl IntoView {
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
        let Some(el) = scroller.get_untracked() else {
            return;
        };
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
        let (t, fit, last) = (
            top.get_untracked(),
            geom.get_untracked().fit,
            geom.get_untracked().last,
        );
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
                    Some(b) => {
                        out.push_str(&hex_lines(at, &b[off..(off + 16).min(b.len())], width))
                    }
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
}
