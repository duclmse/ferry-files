//! Text and code, a piece at a time as it's scrolled, coloured as the backend says.

use super::*;

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
    // After a failed read, a button reads it again: the start, or the piece after the last.
    let retry = move || {
        error.set(None);
        fetch(if pieces.with_untracked(Vec::is_empty) {
            0
        } else {
            next.get_untracked().unwrap_or(0)
        });
    };
    piece_view(pieces, next, error, loading, scrolled, retry, cap)
}

/// What failed to load, and a button to load it again: "Try again" when nothing is shown yet,
/// else "Load more".
pub(super) fn retry_line(
    e: String,
    nothing_yet: bool,
    again: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <div class="peek-retry">
            <span class="more">{e}</span>
            <button class="btn small" on:click=move |_| again()>{if nothing_yet { "Try again" } else { "Load more" }}</button>
        </div>
    }
}

/// The wrap and line-number toggles for text, at the end of the line above the preview.
pub(super) fn text_tools() -> impl IntoView {
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

#[component]
pub(super) fn TextPeek(
    src: Src,
    entry: Entry,
    #[prop(optional)] inner: Option<Inner>,
) -> impl IntoView {
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
