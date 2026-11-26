//! The quiet page a book is read (and edited) in, in the preview: colours that keep the text
//! readable whatever the book's own styles say, and scrolling on past the end of a chapter (or
//! back before its start) into the next (or previous) one.
//!
//! The page is an `<iframe sandbox="allow-same-origin">` with no scripts, so what it does on
//! scrolling is listened to from here, through its document.

use super::*;
use crate::api::{Found, Query};
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;

/// Background, text, links and quiet text: each pair is over 6:1.
const LIGHT: [&str; 4] = ["#fbfaf7", "#1d1d1f", "#0b57d0", "#5f5f63"];
const DARK: [&str; 4] = ["#1c1c1e", "#e6e6e6", "#8ab4ff", "#a1a1a6"];

fn palette([bg, fg, link, quiet]: [&str; 4]) -> String {
    format!("--ferry-bg:{bg};--ferry-fg:{fg};--ferry-link:{link};--ferry-quiet:{quiet}")
}

/// The page's style. `theme` is the app's ("light", "dark", or "auto" to go with the computer);
/// `size` the text size in px, `zoom` the whole page's in percent (text, pictures and margins:
/// books mostly set their own text sizes in px or pt, which `size` doesn't reach) and `pad` the
/// space around it. The text is as wide as the page, so a wider preview has wider lines. The
/// book's own colours give way
/// to the page's, so dark text never lands on a dark page (or light on light). The style comes
/// before the book's, so its rules carry an id (`:not(#ferry-none)`) to outrank even the
/// book's `!important` ones.
pub(super) fn page_style(theme: &str, size: u32, zoom: u32, pad: &str, line: &str) -> String {
    let colours = match theme {
        "dark" => format!(":root{{{};color-scheme:dark}}", palette(DARK)),
        "light" => format!(":root{{{};color-scheme:light}}", palette(LIGHT)),
        _ => format!(":root{{{};color-scheme:light}}@media (prefers-color-scheme: dark){{:root{{{};color-scheme:dark}}}}", palette(LIGHT), palette(DARK)),
    };
    format!(
        "<style>{colours}\
         html:not(#ferry-none){{font-size:{size}px !important;background:var(--ferry-bg) !important;color:var(--ferry-fg) !important}}\
         html:not(#ferry-none) body{{zoom:{z} !important;max-width:none !important;margin:0 !important;padding:{pad} !important;line-height:{line};background:transparent !important;color:var(--ferry-fg) !important}}\
         html:not(#ferry-none) body *{{color:inherit !important;background-color:transparent !important}}\
         html:not(#ferry-none) body a,html:not(#ferry-none) body a *{{color:var(--ferry-link) !important}}\
         img,svg{{max-width:100%;height:auto}}\
         html:not(#ferry-none) body :is(p,div):has(> :is(img,svg):only-child){{text-indent:0 !important}}\
         html:not(#ferry-none) body mark.ferry-hit{{background:#ffd54a !important;color:#1d1d1f !important;border-radius:2px}}\
         html:not(#ferry-none) body mark.ferry-hit.on{{background:#ff9632 !important;box-shadow:0 0 0 2px #ff9632}}\
         html:not(#ferry-none) .ferry-next{{margin:4em 0 1em;padding-top:1em;border-top:1px solid var(--ferry-quiet);text-align:center;font-size:.85rem;color:var(--ferry-quiet) !important}}\
         </style>",
        z = zoom as f64 / 100.0
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The chapter's HTML with `style` after the `<base>` the backend adds (so the book's own styles
/// still load, and give way to it), and a line at the end with a link to the next chapter: its
/// label, and its place in the reading order.
pub(super) fn page(html: &str, style: &str, next: Option<(&str, usize)>) -> String {
    let mut out = match html
        .find("<base ")
        .and_then(|k| html[k..].find('>').map(|j| k + j + 1))
    {
        Some(k) => format!("{}{style}{}", &html[..k], &html[k..]),
        None => format!("{style}{html}"),
    };
    if let Some((label, k)) = next {
        let note = format!(
            r#"<p class="ferry-next">Next chapter: <a href="{}">{}</a></p>"#,
            escape(&chapter_link(k)),
            escape(label)
        );
        match out.to_ascii_lowercase().rfind("</body") {
            Some(k) => out.insert_str(k, &note),
            None => out.push_str(&note),
        }
    }
    out
}

const CHAPTER_PARAM: &str = "ferry-chapter=";

/// Where the page's link to chapter `k` goes: the app's own address, marked. The page runs no
/// scripts and the app's listeners don't reach into it, so the link is followed for real and the
/// frame's load says where it went (`followed`). The app's address keeps the frame readable;
/// the book's (its `<base>`) would not be.
fn chapter_link(k: usize) -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default();
    format!("{origin}/?{CHAPTER_PARAM}{k}")
}

/// The chapter a link in the page in `frame` was followed to, once the frame has loaded it.
/// None for the page itself.
pub(super) fn followed(frame: &web_sys::HtmlIFrameElement) -> Option<usize> {
    let href = frame.content_window()?.location().href().ok()?;
    let (_, rest) = href.split_once(CHAPTER_PARAM)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// How far to scroll on, past the end, to turn to the next chapter.
const TURN: f64 = 240.0;
/// A pause this long (ms) between scrolls starts a new one: scrolling that only just reached the
/// end (a flick still gliding) doesn't turn the chapter.
const PAUSE: f64 = 250.0;

/// Listens to the page in `frame` once it has loaded: scrolling on past its end, or Page Down
/// or Space there, calls `turn(1)`; past its start, `turn(-1)`. ← and → call it too. With `end`,
/// the page starts at its end (as when coming back from the next chapter).
/// A link clicked in the page isn't followed by the frame (which would show the chapter bare,
/// without the page's style): `link` gets where it goes, as a whole address.
pub(super) fn follow(
    frame: &web_sys::HtmlIFrameElement,
    end: bool,
    turn: impl Fn(i32) + Copy + 'static,
    find: impl Fn() + Copy + 'static,
    link: impl Fn(String) + Copy + 'static,
) {
    let Some(doc) = frame.content_document() else {
        return;
    };
    let Some(el) = doc.scrolling_element() else {
        return;
    };
    if end {
        el.set_scroll_top(el.scroll_height());
    }
    let edge = move |dir: i32| {
        if dir > 0 {
            el.scroll_top() + el.client_height() >= el.scroll_height() - 2
        } else {
            el.scroll_top() <= 0
        }
    };
    let (last, armed, sum) = (
        Rc::new(Cell::new(0.0)),
        Rc::new(Cell::new(0i32)),
        Rc::new(Cell::new(0.0)),
    );
    let edge2 = edge.clone();
    let wheel = Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |e: web_sys::WheelEvent| {
        let now = e.time_stamp();
        let gap = now - last.replace(now);
        let dy = e.delta_y() * if e.delta_mode() == 0 { 1.0 } else { 16.0 };
        if dy == 0.0 {
            return;
        }
        let dir = if dy > 0.0 { 1 } else { -1 };
        if !edge(dir) {
            armed.set(0);
            return;
        }
        if armed.get() != dir {
            if gap < PAUSE {
                return;
            }
            armed.set(dir);
            sum.set(0.0);
        }
        sum.set(sum.get() + dy.abs());
        if sum.get() >= TURN {
            armed.set(0);
            turn(dir);
        }
    });
    let _ = doc.add_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref());
    wheel.forget();
    let keys =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            // ⌘F: find in the book (key presses in the page don't reach the app's window).
            if (e.meta_key() || e.ctrl_key()) && matches!(e.key().as_str(), "f" | "ƒ") {
                e.prevent_default();
                find();
                return;
            }
            if e.meta_key() || e.ctrl_key() || e.alt_key() {
                return;
            }
            let dir: i32 = match e.key().as_str() {
                "ArrowRight" => 2,
                "ArrowLeft" => -2,
                "PageDown" | "ArrowDown" => 1,
                " " if !e.shift_key() => 1,
                "PageUp" | "ArrowUp" => -1,
                " " => -1,
                _ => return,
            };
            // Page keys turn only at the end (or start); before that they scroll as usual.
            if dir.abs() == 2 || edge2(dir) {
                e.prevent_default();
                turn(dir.signum());
            }
        });
    let _ = doc.add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref());
    keys.forget();
    let page = doc.clone();
    let click = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e: web_sys::MouseEvent| {
        let Some(t) = e.target() else { return };
        let el = t.dyn_ref::<web_sys::Element>().cloned().or_else(|| {
            t.dyn_ref::<web_sys::Node>()
                .and_then(|n| n.parent_element())
        });
        let Some(a) = el.and_then(|el| el.closest("a[href]").ok().flatten()) else {
            return;
        };
        // The note at the end, to the next chapter, is followed for real (see `followed`).
        if a.closest(".ferry-next").ok().flatten().is_some() {
            return;
        }
        e.prevent_default();
        let href = a.get_attribute("href").unwrap_or_default();
        let base = page.base_uri().ok().flatten().unwrap_or_default();
        if let Ok(u) = web_sys::Url::new_with_base(&href, &base) {
            link(u.href());
        }
    });
    let _ = doc.add_event_listener_with_callback("click", click.as_ref().unchecked_ref());
    click.forget();
}

/// Scrolls the page in `frame` to the element with id `id` (a link's #fragment).
pub(super) fn scroll_to(frame: &web_sys::HtmlIFrameElement, id: &str) {
    if let Some(el) = frame.content_document().and_then(|d| {
        d.get_element_by_id(id).or_else(|| {
            d.query_selector(&format!("a[name=\"{}\"]", id.replace('"', "")))
                .ok()
                .flatten()
        })
    }) {
        el.scroll_into_view_with_bool(true);
    }
}

/// Makes the `k`th match in the page in `frame` the current one (all its marks, when it crosses
/// formatting), and scrolls it to the middle.
pub(super) fn focus_mark(frame: &web_sys::HtmlIFrameElement, k: usize) {
    let Some(doc) = frame.content_document() else {
        return;
    };
    let Ok(marks) = doc.query_selector_all("mark.ferry-hit") else {
        return;
    };
    let mut first = true;
    for i in 0..marks.length() {
        if let Some(m) = marks
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        {
            let on = m
                .get_attribute("data-hit")
                .and_then(|h| h.parse::<usize>().ok())
                == Some(k);
            let _ = m.class_list().toggle_with_force("on", on);
            if on && first {
                first = false;
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Center);
                m.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        }
    }
}

/// Elements inside a run of text, which a match may cross: the same as the backend's
/// (books/epub/search.rs), so the page's matches are the search's, one for one.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "big", "cite", "code", "data", "del", "dfn", "em", "font", "i",
    "ins", "kbd", "mark", "q", "s", "samp", "small", "span", "strike", "strong", "sub", "sup",
    "time", "tt", "u", "var",
];

/// Non-overlapping matches of `needle` in `hay`, as byte ranges: the backend's `find_all`
/// (books/epub.rs), so plain searches match the same text here. A space matches any run of
/// white space.
fn find_all(hay: &str, needle: &str, match_case: bool, whole_word: bool) -> Vec<(usize, usize)> {
    let h: Vec<(usize, char)> = hay.char_indices().collect();
    let mut n: Vec<char> = vec![];
    for c in needle.trim().chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if !(c == ' ' && n.last() == Some(&' ')) {
            n.push(c);
        }
    }
    let mut out = vec![];
    if n.is_empty() {
        return out;
    }
    let eq = |a: char, b: char| a == b || (!match_case && a.to_lowercase().eq(b.to_lowercase()));
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let at = |i: usize| -> Option<usize> {
        let mut j = i;
        for &c in &n {
            if c == ' ' {
                let k = j;
                while j < h.len() && h[j].1.is_whitespace() {
                    j += 1;
                }
                if j == k {
                    return None;
                }
            } else if j < h.len() && eq(h[j].1, c) {
                j += 1;
            } else {
                return None;
            }
        }
        Some(j)
    };
    let mut i = 0;
    while i < h.len() {
        if let Some(j) = at(i) {
            let bounded = (i == 0 || !word(h[i - 1].1)) && (j == h.len() || !word(h[j].1));
            if !whole_word || bounded {
                out.push((h[i].0, h.get(j).map_or(hay.len(), |x| x.0)));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Matches of a regular expression in `hay`, as byte ranges. JavaScript's regular expressions,
/// which read almost every pattern as the backend's do.
fn find_regex(hay: &str, q: &Query) -> Vec<(usize, usize)> {
    let pattern = if q.whole_word {
        format!(r"\b(?:{})\b", q.find)
    } else {
        q.find.clone()
    };
    let flags = if q.match_case { "gu" } else { "giu" };
    let make = js_sys::RegExp::new("", "").constructor();
    let args = js_sys::Array::of2(&pattern.into(), &flags.into());
    let Some(re) = js_sys::Reflect::construct(&make, &args)
        .ok()
        .map(|r| r.unchecked_into::<js_sys::RegExp>())
    else {
        return vec![];
    };
    // JavaScript counts UTF-16 units; this string, bytes.
    let units: Vec<usize> = hay
        .char_indices()
        .flat_map(|(b, c)| std::iter::repeat_n(b, c.len_utf16()))
        .chain([hay.len()])
        .collect();
    let byte = |u: u32| units.get(u as usize).copied().unwrap_or(hay.len());
    let mut out = vec![];
    while let Some(m) = re.exec(hay) {
        let at = js_sys::Reflect::get(&m, &"index".into())
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as u32;
        let len = m
            .get(0)
            .as_string()
            .map_or(0, |s| s.encode_utf16().count() as u32);
        if len == 0 {
            re.set_last_index(re.last_index() + 1);
            continue;
        }
        out.push((byte(at), byte(at + len)));
    }
    out
}

/// The runs of text in `node`: the texts of a paragraph (or heading, list item…) across the
/// formatting inside it, as the backend reads them. Any other element ends a run.
fn runs(node: &web_sys::Node, out: &mut Vec<Vec<web_sys::Node>>) {
    let kids = node.child_nodes();
    for i in 0..kids.length() {
        let Some(k) = kids.item(i) else { continue };
        match k.node_type() {
            web_sys::Node::TEXT_NODE => out.last_mut().expect("a run is always open").push(k),
            web_sys::Node::ELEMENT_NODE => {
                let name = k.node_name().to_ascii_lowercase();
                let skip = matches!(name.as_str(), "script" | "style")
                    || k.dyn_ref::<web_sys::Element>()
                        .is_some_and(|e| e.class_list().contains("ferry-next"));
                if INLINE.contains(&name.as_str()) && !skip {
                    runs(&k, out);
                } else {
                    out.push(vec![]);
                    if !skip {
                        runs(&k, out);
                    }
                    out.push(vec![]);
                }
            }
            _ => {}
        }
    }
}

/// Marks what `q` finds in the page in `frame` (the marks from before go first), matching as
/// the search does, so the page's matches are the search's in the same order: a match that
/// crosses formatting is marked in each of its parts. Each mark says which match it's part of
/// (`data-hit`). How many matches it marked.
pub(super) fn mark(frame: &web_sys::HtmlIFrameElement, q: &Query) -> usize {
    unmark(frame);
    let Some(doc) = frame.content_document() else {
        return 0;
    };
    let Some(body) = doc.body() else { return 0 };
    if q.find.trim().is_empty() {
        return 0;
    }
    let mut all = vec![vec![]];
    runs(body.as_ref(), &mut all);
    let mut hit = 0;
    // (text node, from, to in UTF-16 units, match)
    let mut parts: Vec<(web_sys::Node, u32, u32, usize)> = vec![];
    for run in all {
        let texts: Vec<String> = run
            .iter()
            .map(|n| n.node_value().unwrap_or_default())
            .collect();
        if texts.iter().all(|t| t.trim().is_empty()) {
            continue;
        }
        let plain = texts.concat();
        let found = if q.regex {
            find_regex(&plain, q)
        } else {
            find_all(&plain, &q.find, q.match_case, q.whole_word)
        };
        for (a, b) in found {
            let mut from = 0;
            for (node, text) in run.iter().zip(&texts) {
                let to = from + text.len();
                let (lo, hi) = (a.max(from), b.min(to));
                if lo < hi {
                    let units = |k: usize| text[..k - from].encode_utf16().count() as u32;
                    parts.push((node.clone(), units(lo), units(hi), hit));
                }
                from = to;
            }
            hit += 1;
        }
    }
    // Within a text, from its end, so the places of the parts before stay as they are.
    let mut k = 0;
    while k < parts.len() {
        let mut j = k;
        while j < parts.len() && parts[j].0.is_same_node(Some(&parts[k].0)) {
            j += 1;
        }
        for (node, s, e, h) in parts[k..j].iter().rev() {
            let (Ok(range), Ok(mark)) = (doc.create_range(), doc.create_element("mark")) else {
                continue;
            };
            mark.set_class_name("ferry-hit");
            let _ = mark.set_attribute("data-hit", &h.to_string());
            if range.set_start(node, *s).is_ok() && range.set_end(node, *e).is_ok() {
                let _ = range.surround_contents(&mark);
            }
        }
        k = j;
    }
    hit
}

/// Takes the marks out of the page in `frame`, leaving its text as it was.
pub(super) fn unmark(frame: &web_sys::HtmlIFrameElement) {
    let Some(doc) = frame.content_document() else {
        return;
    };
    let Ok(marks) = doc.query_selector_all("mark.ferry-hit") else {
        return;
    };
    for i in 0..marks.length() {
        let Some(m) = marks.item(i) else { continue };
        if let (Some(parent), Some(text)) = (m.parent_node(), m.text_content()) {
            let t = doc.create_text_node(&text);
            let _ = parent.replace_child(&t, &m);
            parent.normalize();
        }
    }
}

/// What the Search tab calls what it searches: the find box's label, and the parts the matches
/// are grouped by.
#[derive(Clone, Copy)]
pub(super) struct FindWords {
    pub find: &'static str,
    pub part: &'static str,
    pub parts: &'static str,
}

pub(super) const BOOK_WORDS: FindWords = FindWords {
    find: "Find in the book",
    part: "chapter",
    parts: "chapters",
};
pub(super) const PDF_WORDS: FindWords = FindWords {
    find: "Find in the PDF",
    part: "page",
    parts: "pages",
};

/// Find, while reading, in the Search tab beside the page: the find box with its options, how
/// many matches, and under it a tree: the chapters, each with its matches. A click on a match
/// opens its chapter there, that match marked as the current one (`pick`, with its place among
/// the chapter's matches; `current` is the one picked).
/// It searches as you type (`search` waits for typing to stop); Enter and Shift+Enter (or the
/// arrows) go to the next and previous match, over all the parts, and the count says which one
/// is current. Esc calls `close`. The first `FIND_PAGE` matches are listed, and Show more lists
/// the next ones (going to a match lists as far as it).
/// `words` names what's searched and its parts: a book's chapters, a PDF's pages.
/// How many matches the Search tab lists at first, and how many more Show more adds.
const FIND_PAGE: usize = 100;

#[allow(clippy::too_many_arguments)]
pub(super) fn find_bar(
    words: FindWords,
    q: RwSignal<Query>,
    found: RwSignal<Option<Result<Found, String>>>,
    busy: RwSignal<bool>,
    focus: RwSignal<u32>,
    search: impl Fn(bool) + Copy + Send + Sync + 'static,
    pick: impl Fn(String, usize) + Copy + Send + Sync + 'static,
    current: RwSignal<Option<(String, usize)>>,
    close: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let find_box = NodeRef::<html::Input>::new();
    Effect::new(move |_| {
        focus.track();
        request_animation_frame(move || {
            if let Some(el) = find_box.get_untracked() {
                let _ = el.focus();
                el.select();
            }
        });
    });
    // Typed since the last search: Enter searches now instead of moving on.
    let dirty = StoredValue::new(false);
    // How many matches are listed; new results start again from the first page.
    let listed = RwSignal::new(FIND_PAGE);
    Effect::new(move |_| {
        found.track();
        listed.set(FIND_PAGE);
    });
    let opt = move |label: &'static str,
                    on: fn(&Query) -> bool,
                    flip: fn(&mut Query),
                    glyph: &'static str,
                    class: &'static str| {
        view! {
            <button class="filter-opt" class:on=move || q.with(on) aria-pressed=move || q.with(on).to_string() title=label aria-label=label
                on:mousedown=|e: web_sys::MouseEvent| e.prevent_default()
                on:click=move |_| {
                    q.update(flip);
                    dirty.set_value(false);
                    search(true);
                }>
                <span class=class>{glyph}</span>
            </button>
        }
    };
    // Every match in order, as (part, its place in the part): the one `by` from the current,
    // round from the end to the start; the first (or last) when none is current.
    let step = move |by: isize| {
        let to = found.with_untracked(|f| {
            let f = f.as_ref()?.as_ref().ok()?;
            let order = || {
                f.hits
                    .iter()
                    .flat_map(|h| (0..h.count).map(move |k| (&h.path, k)))
            };
            let n = f.total.max(1) as isize;
            let at = current.with_untracked(|c| {
                c.as_ref()
                    .and_then(|(p, k)| order().position(|(q, j)| q == p && j == *k))
            });
            let i = match at {
                Some(i) => (i as isize + by).rem_euclid(n),
                None if by < 0 => n - 1,
                None => 0,
            };
            order().nth(i as usize).map(|(p, k)| (p.clone(), k))
        });
        if let Some((p, k)) = to {
            // List as far as the match, when it's one with its text listed.
            let at = found.with_untracked(|f| {
                let f = f.as_ref()?.as_ref().ok()?;
                let mut before = 0;
                for h in &f.hits {
                    if h.path == p {
                        return (k < h.snippets.len()).then_some(before + k);
                    }
                    before += h.snippets.len();
                }
                None
            });
            if let Some(at) = at.filter(|&at| at >= listed.get_untracked()) {
                listed.set((at / FIND_PAGE + 1) * FIND_PAGE);
            }
            pick(p, k);
        }
    };
    let enter = move |back: bool| {
        if dirty.get_value() || found.with_untracked(Option::is_none) {
            dirty.set_value(false);
            search(true);
        } else {
            step(if back { -1 } else { 1 });
        }
    };
    let which = move || {
        let (p, k) = current.get()?;
        found.with(|f| {
            let f = f.as_ref()?.as_ref().ok()?;
            let mut before = 0;
            for h in &f.hits {
                if h.path == p {
                    return Some(format!("{} / {}", before + k + 1, f.total));
                }
                before += h.count;
            }
            None
        })
    };
    let has_hits = move || found.with(|f| matches!(f, Some(Ok(f)) if f.total > 0));
    let count = move || {
        if let Some(w) = which() {
            return w;
        }
        found.with(|f| match f {
            None if busy.get() => "Finding…".to_string(),
            None => String::new(),
            Some(Err(_)) => String::new(),
            Some(Ok(f)) if f.total == 0 => "No matches".into(),
            Some(Ok(f)) => format!(
                "{} {} in {} {}",
                f.total,
                if f.total == 1 { "match" } else { "matches" },
                f.hits.len(),
                if f.hits.len() == 1 {
                    words.part
                } else {
                    words.parts
                }
            ),
        })
    };
    view! {
        <div class="read-find" on:keydown=move |e| if e.key() == "Escape" { e.prevent_default(); e.stop_propagation(); close(); }>
            <div class="read-find-box">
                <div class="filter-box find" class:bad=move || found.with(|f| matches!(f, Some(Err(_))) || matches!(f, Some(Ok(f)) if f.total == 0))
                    title=move || found.with(|f| match f { Some(Err(e)) => e.clone(), _ => String::new() })>
                    {ico(Icon::Search)}
                    <input class="filter" node_ref=find_box type="text" spellcheck="false" aria-label=words.find
                        placeholder=move || if q.with(|x| x.regex) { format!("{} (regex)", words.find) } else { words.find.to_string() }
                        prop:value=move || q.with(|x| x.find.clone())
                        on:input=move |e| { q.update(|x| x.find = event_target_value(&e)); dirty.set_value(true); search(false); }
                        on:keydown=move |e| if e.key() == "Enter" { e.prevent_default(); enter(e.shift_key()); }/>
                    {opt("Match case", |x| x.match_case, |x| x.match_case = !x.match_case, "Aa", "")}
                    {opt("Match whole word", |x| x.whole_word, |x| x.whole_word = !x.whole_word, "ab", "word")}
                    {opt("Use regular expression", |x| x.regex, |x| x.regex = !x.regex, ".*", "")}
                    // One above the other, in the room of one button (views.css).
                    <span class="find-steps">
                        <button class="filter-opt" title="Previous match (⇧↵)" aria-label="Previous match" disabled=move || !has_hits()
                            on:mousedown=|e: web_sys::MouseEvent| e.prevent_default() on:click=move |_| step(-1)>{ico(Icon::PrevMatch)}</button>
                        <button class="filter-opt" title="Next match (↵)" aria-label="Next match" disabled=move || !has_hits()
                            on:mousedown=|e: web_sys::MouseEvent| e.prevent_default() on:click=move |_| step(1)>{ico(Icon::NextMatch)}</button>
                    </span>
                </div>
                <div class="find-tools">
                    <span class="count">{count}</span>
                </div>
            </div>
            {move || found.get().map(|f| match f {
                Err(e) => view! { <p class="hint">{e}</p> }.into_any(),
                Ok(f) => {
                    let limit = listed.get();
                    let all: usize = f.hits.iter().map(|h| h.snippets.len()).sum();
                    let left = all.saturating_sub(limit);
                    let mut room = limit;
                    // The parts with a match still listed, each with as many as there's room for.
                    let hits: Vec<_> = f.hits.into_iter().map_while(|mut h| {
                        if room == 0 {
                            return None;
                        }
                        let whole = h.snippets.len() <= room;
                        h.snippets.truncate(room);
                        room -= h.snippets.len();
                        Some((h, whole))
                    }).collect();
                    view! {
                    <ul class="hit-tree" role="tree">
                        {hits.into_iter().map(|(h, whole)| {
                            let open = RwSignal::new(true);
                            let path = StoredValue::new(h.path.clone());
                            // Past the text listed for a part, matches are only counted.
                            let more = if whole { h.count.saturating_sub(h.snippets.len()) } else { 0 };
                            view! {
                                <li role="treeitem" aria-expanded=move || open.get().to_string()>
                                    <div class="hit-head">
                                        <button class="twisty" title=move || if open.get() { "Hide the matches" } else { "Show the matches" }
                                            aria-label="Show or hide the matches" on:click=move |_| open.update(|o| *o = !*o)>{super::sidebar::chev(open.into())}</button>
                                        <button class="name" title=h.label.clone() on:click=move |_| pick(path.get_value(), 0)>
                                            <span class="label">{h.label.clone()}</span>
                                            <span class="kind">{h.count}</span>
                                        </button>
                                    </div>
                                    {move || open.get().then(|| view! {
                                        <ul role="group">
                                            {h.snippets.clone().into_iter().enumerate().map(|(k, s)| {
                                                let here = move || current.with(|c| c.as_ref().is_some_and(|(p, i)| *i == k && path.with_value(|q| q == p)));
                                                view! {
                                                    <li role="treeitem">
                                                        <button class="hit" class:here=here on:click=move |_| pick(path.get_value(), k)>
                                                            {s.before}<mark>{s.text}</mark>{s.after}
                                                        </button>
                                                    </li>
                                                }
                                            }).collect_view()}
                                            {(more > 0).then(|| view! { <li class="hint">{format!("{more} more, not listed")}</li> })}
                                        </ul>
                                    })}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                    {(left > 0).then(|| view! {
                        <button class="btn small find-more" title="List more of the matches" aria-label="List more of the matches"
                            on:click=move |_| listed.update(|n| *n += FIND_PAGE)>
                            {format!("Show {} more", left.min(FIND_PAGE))}
                        </button>
                    })}
                    }.into_any()
                }
            })}
        </div>
    }
}
