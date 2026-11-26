//! The preview to the right of the files: pictures (zoom, turn, pan), video and audio
//! (video.rs), text and hex (loaded as they're scrolled), PDFs, fonts,
//! what's in a ZIP, and details.

use super::sidebar::chev;
use super::*;
use crate::api::{
    Bytes, EpubContents, Listing, MediaInfo, Packed, PdfInfo, PdfLayer, PdfLink, PdfOutline,
    PdfRun, Peek,
};
use crate::api::{Found, Query};
use wasm_bindgen::closure::Closure;

mod epub;
mod hex;
mod pdf_view;
mod picture;
mod text;
mod zip;

pub(super) use {epub::*, hex::*, pdf_view::*, picture::*, text::*, zip::*};

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

/// A file's size, then its width × height and length once known, for beside its name.
fn measures(size: Option<String>, info: RwSignal<MediaInfo>) -> String {
    let mut v: Vec<String> = size.into_iter().collect();
    info.with(|i| {
        v.extend(i.width.zip(i.height).map(|(w, h)| format!("{w} × {h}")));
        v.extend(i.duration.map(length));
    });
    v.join(" · ")
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
    let wanted = Memo::new(move |_| {
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
                    .map(|z| (z.2.clone(), z.3, z.4))
            });
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
        let read = (!alone && shown.with_untracked(Option::is_some))
            .then(|| picture_url(p, &w))
            .flatten();
        match read {
            Some((path, url)) => {
                let target = StoredValue::new(w);
                when_read(
                    p,
                    path,
                    url,
                    move || target.with_value(|t| wanted.with_untracked(|now| now == t)),
                    move || shown.set(target.get_value()),
                );
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
        view! { <VideoPlayer url key title=entry.name.clone() info turn pane=pane.id pop near/> }
            .into_any()
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
