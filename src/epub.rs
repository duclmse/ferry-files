//! The EPUB editor, shown in a pane's preview: book info and cover, find & replace, and each
//! chapter in a visual or source view. The book itself lives in the backend (src-tauri/src/epub.rs)
//! until it's saved; this module only holds what's on screen. Chapter edits are sent to the backend before any other action, so the
//! backend copy is always complete when the book is saved.

use crate::api::*;
use crate::app::{ico, Icon};
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::future::Future;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

mod chapter;
mod contents;
mod find;
mod info;

use chapter::*;
use contents::*;
use find::*;
use info::*;

#[derive(Clone, PartialEq)]
pub enum View {
    Info,
    Chapter(String),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Visual,
    Source,
}

#[derive(Clone, Copy)]
pub struct Ep {
    pub book: RwSignal<Option<Book>>,
    /// Where the open book is saved back to, for sentences: "the Mac" or the device's name.
    place: RwSignal<String>,
    view: RwSignal<View>,
    mode: RwSignal<Mode>,
    doc: RwSignal<Option<ChapterDoc>>,
    source: RwSignal<String>,
    /// The open chapter has edits not yet sent to the backend.
    chapter_dirty: RwSignal<bool>,
    status: RwSignal<(String, &'static str)>,
    busy: RwSignal<bool>,
    confirm_close: RwSignal<bool>,
    query: RwSignal<Query>,
    found: RwSignal<Option<Found>>,
    frame: NodeRef<html::Iframe>,
    /// The Source view's text box.
    source_box: NodeRef<html::Textarea>,
    /// Source shown in colour.
    colours: RwSignal<bool>,
    /// The chapter's zoom, in percent: the whole page in Visual, the code's 13 px in Source.
    zoom: RwSignal<u32>,
    /// The chapters beside the page, when reading or editing; one setting for both.
    pub outline: RwSignal<bool>,
    /// The chapter whose name is being typed in the outline.
    renaming: RwSignal<Option<String>>,
    /// Find & replace: Replace with shown, and asks to put the cursor in a box (bumped; with
    /// `true`, the Replace with box).
    replacing: RwSignal<bool>,
    find_focus: RwSignal<(u32, bool)>,
    /// The pane beside the page shows its Search tab (find & replace), not the chapters.
    finding: RwSignal<bool>,
    /// …or its Contents tab (the table of contents), when not Search.
    contents: RwSignal<bool>,
    /// A place (an id) to scroll the page to once its chapter has loaded.
    anchor: StoredValue<Option<String>>,
    /// The search waiting for typing to stop.
    find_timer: StoredValue<Option<TimeoutHandle>>,
    /// Where Replace goes on from: the search, the chapter and the place in its XHTML after the
    /// last match it replaced.
    replace_from: StoredValue<Option<(Query, String, usize)>>,
    /// The pane whose preview shows the editor, so only one place draws it.
    pub host: RwSignal<Option<usize>>,
    /// The visual view's edits, taken as its frame went away (the preview moved to another file).
    kept: StoredValue<Option<String>>,
    /// The visual page's last cursor or selection, put back when the page gets the focus again
    /// (clicking anywhere outside it takes the focus and, in a frame, the selection with it).
    range: StoredValue<Option<web_sys::Range>, LocalStorage>,
}

const COLOURS_KEY: &str = "ferry-files.epub-colours";
const ZOOM_KEY: &str = "ferry-files.epub-edit-zoom";
const OUTLINE_KEY: &str = "ferry-files.epub-outline";
/// How long typing in a find box waits before searching.
const FIND_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
const ZOOM_MIN: u32 = 60;
const ZOOM_MAX: u32 = 250;

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn human_size(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if unit == "B" {
                format!("{v:.0} B")
            } else {
                format!("{v:.1} {unit}")
            };
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

/// Whether the open book is on the Mac.
fn b_mac(ep: Ep) -> bool {
    ep.book
        .with_untracked(|b| b.as_ref().is_some_and(|b| b.src == "mac"))
}

impl Ep {
    pub fn new() -> Self {
        Ep {
            book: RwSignal::new(None),
            place: RwSignal::new(String::new()),
            view: RwSignal::new(View::Info),
            mode: RwSignal::new(Mode::Visual),
            doc: RwSignal::new(None),
            source: RwSignal::new(String::new()),
            chapter_dirty: RwSignal::new(false),
            colours: RwSignal::new(crate::app::stored_flag(COLOURS_KEY, true)),
            outline: RwSignal::new(crate::app::stored_flag(OUTLINE_KEY, false)),
            renaming: RwSignal::new(None),
            replacing: RwSignal::new(false),
            find_focus: RwSignal::new((0, false)),
            finding: RwSignal::new(false),
            contents: RwSignal::new(false),
            anchor: StoredValue::new(None),
            find_timer: StoredValue::new(None),
            replace_from: StoredValue::new(None),
            zoom: RwSignal::new(
                crate::app::stored(ZOOM_KEY)
                    .and_then(|z| z.parse().ok())
                    .unwrap_or(100)
                    .clamp(ZOOM_MIN, ZOOM_MAX),
            ),
            status: RwSignal::new((String::new(), "")),
            busy: RwSignal::new(false),
            confirm_close: RwSignal::new(false),
            query: RwSignal::new(Query::default()),
            found: RwSignal::new(None),
            frame: NodeRef::new(),
            source_box: NodeRef::new(),
            host: RwSignal::new(None),
            kept: StoredValue::new(None),
            range: StoredValue::new_local(None),
        }
    }

    /// Shows or hides the chapters beside the page.
    pub fn toggle_outline(self) {
        let on = !self.outline.get_untracked();
        self.outline.set(on);
        crate::app::store(OUTLINE_KEY, if on { "1" } else { "0" });
    }

    fn id(self) -> u32 {
        self.book.with_untracked(|b| b.as_ref().map_or(0, |b| b.id))
    }

    /// Where the book is saved back to, for sentences: "the Mac" or the device's name.
    fn place(self) -> String {
        self.place.get_untracked()
    }

    pub fn dirty(self) -> bool {
        self.chapter_dirty.get() || self.book.with(|b| b.as_ref().is_some_and(|b| b.dirty))
    }

    fn say(self, msg: impl Into<String>, kind: &'static str) {
        self.status.set((msg.into(), kind));
    }

    /// Whether the open book is the one at `path` on `src`.
    pub fn holds(self, src: &str, path: &str) -> bool {
        self.book
            .with(|b| b.as_ref().is_some_and(|b| b.src == src && b.path == path))
    }

    /// Opens the book at `path` on `src` ("mac" or a device id) in pane `host`'s preview; `place`
    /// names it for sentences. Editing reads the whole book. A book already open with unsaved
    /// changes isn't replaced.
    pub async fn open(
        self,
        src: &str,
        place: String,
        path: String,
        host: usize,
    ) -> Result<(), String> {
        if self
            .book
            .with_untracked(|b| b.as_ref().is_some_and(|b| b.src == src && b.path == path))
        {
            self.host.set(Some(host));
            return Ok(());
        }
        if self.dirty() {
            let name = self
                .book
                .with_untracked(|b| b.as_ref().map(|b| b.name.clone()).unwrap_or_default());
            return Err(format!(
                "{name} has changes that aren't saved. Save or discard them first."
            ));
        }
        let book = call::<Book>("epub_open", json!({ "src": src, "path": path })).await?;
        // A book still open here is replaced: the backend lets go of it.
        let old = self.id();
        if old != 0 && old != book.id {
            spawn_local(async move {
                let _ = call::<()>("epub_close", json!({ "id": old })).await;
            });
        }
        self.place.set(place);
        self.view.set(
            book.chapters
                .first()
                .map_or(View::Info, |c| View::Chapter(c.path.clone())),
        );
        self.mode.set(Mode::Visual);
        self.doc.set(None);
        self.chapter_dirty.set(false);
        self.found.set(None);
        self.confirm_close.set(false);
        self.say("", "");
        self.kept.set_value(None);
        self.book.set(Some(book));
        self.host.set(Some(host));
        Ok(())
    }

    /// Sends the open chapter's edits to the backend.
    async fn commit(self) -> Result<(), String> {
        if !self.chapter_dirty.get_untracked() {
            return Ok(());
        }
        let View::Chapter(path) = self.view.get_untracked() else {
            return Ok(());
        };
        let id = self.id();
        let book = match self.mode.get_untracked() {
            Mode::Source => {
                let source = self.source.get_untracked();
                call::<Book>(
                    "epub_set_source",
                    json!({ "id": id, "path": path, "source": source }),
                )
                .await
            }
            Mode::Visual => {
                let body = match self.kept.try_update_value(Option::take).flatten() {
                    Some(b) => b,
                    None => self
                        .visual_body()
                        .inspect_err(|_| self.chapter_dirty.set(false))
                        .map_err(|e| format!("The last edits to this chapter were lost. {e}"))?,
                };
                call::<Book>(
                    "epub_set_body",
                    json!({ "id": id, "path": path, "body": body }),
                )
                .await
            }
        }
        .map_err(|e| format!("This chapter can't be kept as it is. {e}"))?;
        self.book.set(Some(book));
        self.chapter_dirty.set(false);
        Ok(())
    }

    /// Runs one action at a time: first the chapter's edits are sent, then `f`. Errors go to the status line.
    fn act<F, Fut>(self, f: F)
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output = Result<(), String>> + 'static,
    {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        spawn_local(async move {
            let r = match self.commit().await {
                Ok(()) => f().await,
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                self.say(e, "error");
            }
            self.busy.set(false);
        });
    }

    async fn load_chapter(self, path: &str) -> Result<(), String> {
        let doc =
            call::<ChapterDoc>("epub_chapter", json!({ "id": self.id(), "path": path })).await?;
        self.kept.set_value(None);
        self.source.set(doc.source.clone());
        self.doc.set(Some(doc));
        self.chapter_dirty.set(false);
        Ok(())
    }

    fn go(self, v: View) {
        self.act(move || async move {
            if let View::Chapter(p) = &v {
                self.load_chapter(p).await?;
            }
            if matches!(self.status.get_untracked().1, "error") {
                self.say("", "");
            }
            self.view.set(v);
            Ok(())
        });
    }

    /// Shows the open chapter again when the editor is drawn anew, as it is when the preview comes
    /// back to the book: its edits are sent first, then it's read back with them.
    fn reshow(self) {
        let View::Chapter(p) = self.view.get_untracked() else {
            return;
        };
        self.doc.set(None);
        spawn_local(async move {
            let r = match self.commit().await {
                Ok(()) => self.load_chapter(&p).await,
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                self.say(e, "error");
            }
        });
    }

    fn set_mode(self, m: Mode) {
        if self.mode.get_untracked() == m {
            return;
        }
        self.act(move || async move {
            if let View::Chapter(p) = self.view.get_untracked() {
                self.load_chapter(&p).await?;
            }
            self.mode.set(m);
            Ok(())
        });
    }

    pub fn save(self) {
        self.act(move || async move {
            self.say(format!("Saving to {}…", self.place()), "");
            let s = call::<Saved>("epub_save", json!({ "id": self.id() })).await?;
            self.book.set(Some(s.book));
            self.confirm_close.set(false);
            let note = if s.backup {
                format!(
                    " The old version is backed up on {}.",
                    crate::os::the_computer()
                )
            } else {
                String::new()
            };
            self.say(format!("Saved to {}.{note}", self.place()), "ok");
            Ok(())
        });
    }

    fn export(self) {
        self.act(move || async move {
            if let Some(dest) =
                call::<Option<String>>("epub_export", json!({ "id": self.id() })).await?
            {
                self.say(format!("Copy saved to {dest}"), "ok");
            }
            Ok(())
        });
    }

    pub fn close(self, force: bool) {
        if !force && self.dirty() {
            self.confirm_close.set(true);
            return;
        }
        let id = self.id();
        spawn_local(async move {
            let _ = call::<()>("epub_close", json!({ "id": id })).await;
        });
        self.chapter_dirty.set(false);
        self.confirm_close.set(false);
        self.kept.set_value(None);
        self.doc.set(None);
        self.host.set(None);
        self.book.set(None);
    }

    fn set_meta(self, meta: Meta) {
        self.act(move || async move {
            let b = call::<Book>("epub_set_meta", json!({ "id": self.id(), "meta": meta })).await?;
            self.book.set(Some(b));
            self.say(
                format!("Book info updated. Save to write it to {}.", self.place()),
                "ok",
            );
            Ok(())
        });
    }

    /// Find & replace (⌘F), with Replace with (⌘R): shown, with the cursor in its box.
    pub fn open_find(self, replace: bool) {
        if replace {
            self.replacing.set(true);
        }
        self.finding.set(true);
        if !self.outline.get_untracked() {
            self.toggle_outline();
        }
        self.find_focus
            .update(|(n, r)| (*n, *r) = (*n + 1, replace));
    }

    /// Starts typing a new name for the chapter at `path`, in the outline (shown for it).
    fn start_rename(self, path: String) {
        if !self.outline.get_untracked() {
            self.toggle_outline();
        }
        self.renaming.set(Some(path));
    }

    fn rename_chapter(self, path: String, label: String) {
        self.renaming.set(None);
        let same = self.book.with_untracked(|b| {
            b.as_ref()
                .and_then(|b| b.chapters.iter().find(|c| c.path == path))
                .is_some_and(|c| c.label == label.trim())
        });
        if same {
            return;
        }
        self.act(move || async move {
            let b = call::<Book>(
                "epub_rename_chapter",
                json!({ "id": self.id(), "path": path, "label": label }),
            )
            .await?;
            self.book.set(Some(b));
            self.say(
                format!(
                    "Chapter renamed in the contents. Save to write the book to {}.",
                    self.place()
                ),
                "ok",
            );
            Ok(())
        });
    }

    fn cover(self) {
        self.act(move || async move {
            if let Some(b) = call::<Option<Book>>("epub_cover", json!({ "id": self.id() })).await? {
                self.book.set(Some(b));
                self.say(
                    format!("Cover changed. Save to write it to {}.", self.place()),
                    "ok",
                );
            }
            Ok(())
        });
    }
}

/// The editor's controls, in the preview's bar: which part of the book is shown, Save copy and Save.
#[component]
pub fn EditBar() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let dirty = Memo::new(move |_| ep.dirty());
    let chapters = move || {
        ep.book
            .with(|b| b.as_ref().map(|b| b.chapters.clone()).unwrap_or_default())
    };
    let shown = move || match ep.view.get() {
        View::Info => "info".to_string(),
        View::Chapter(p) => format!("ch:{p}"),
    };
    let pick = move |ev| {
        let v = event_target_value(&ev);
        ep.go(match v.as_str() {
            "info" => View::Info,
            _ => View::Chapter(v.strip_prefix("ch:").unwrap_or(&v).to_string()),
        });
    };
    let save_title = move || {
        ep.book.track();
        let to = if b_mac(ep) {
            String::new()
        } else {
            format!(" to {}", ep.place())
        };
        let state = if dirty.get() { "" } else { " (nothing new)" };
        format!("Save{to}{state} ({})", crate::os::keys("⌘S"))
    };
    let copy_title = format!("Save a copy on {}…", crate::os::the_computer());
    // Previous and next go through the chapters; from Book info or Find, next is the first chapter.
    let at = move |v: &View, b: &Option<Book>| match v {
        View::Chapter(p) => b
            .as_ref()
            .and_then(|b| b.chapters.iter().position(|c| c.path == *p)),
        _ => None,
    };
    let count = move || ep.book.with(|b| b.as_ref().map_or(0, |b| b.chapters.len()));
    let here = move || ep.view.with(|v| ep.book.with(|b| at(v, b)));
    let step = move |d: isize| {
        let to = match ep
            .view
            .with_untracked(|v| ep.book.with_untracked(|b| at(v, b)))
        {
            Some(k) => k as isize + d,
            None if d > 0 => 0,
            None => return,
        };
        let path = ep.book.with_untracked(|b| {
            b.as_ref()
                .and_then(|b| usize::try_from(to).ok().and_then(|k| b.chapters.get(k)))
                .map(|c| c.path.clone())
        });
        if let Some(p) = path {
            ep.go(View::Chapter(p));
        }
    };
    let no_page = move || !matches!(ep.view.get(), View::Chapter(_));
    // Save's menu, closed by a click anywhere else.
    let save_open = RwSignal::new(false);
    let save_menu = NodeRef::<html::Span>::new();
    let away = window_event_listener(leptos::ev::mousedown, move |e| {
        let inside = save_menu
            .get_untracked()
            .zip(e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()))
            .is_some_and(|(m, t)| m.contains(Some(&t)));
        if !inside {
            save_open.set(false);
        }
    });
    on_cleanup(move || away.remove());
    // In the order of the bar when reading: contents, zoom, the part shown, previous and next.
    view! {
        // The pane beside the page: its Chapters tab, and its Search tab (find & replace).
        <span class="btn-group" role="group">
            <button class="btn" class:active=move || ep.outline.get() && !ep.finding.get() title="Chapters" aria-label="Chapters"
                on:click=move |_| ep.show_chapters()>{ico(Icon::Contents)}</button>
            <button class="btn" class:active=move || ep.searching() title=crate::os::keys("Find & replace (⌘F)") aria-label="Find & replace"
                aria-pressed=move || ep.searching().to_string()
                on:click=move |_| if ep.searching() { ep.toggle_outline() } else { ep.open_find(false) }>{ico(Icon::Search)}</button>
        </span>
        <span class="btn-group" role="group">
            <button class="btn" title="Smaller text" aria-label="Smaller text" disabled=no_page
                on:click=move |_| ep.set_zoom(ep.zoom.get_untracked().saturating_sub(10))>{ico(Icon::ZoomOut)}</button>
            <button class="btn zoom" title="Back to 100%" disabled=no_page on:click=move |_| ep.set_zoom(100)>{move || format!("{}%", ep.zoom.get())}</button>
            <button class="btn" title="Bigger text" aria-label="Bigger text" disabled=no_page
                on:click=move |_| ep.set_zoom(ep.zoom.get_untracked() + 10)>{ico(Icon::ZoomIn)}</button>
        </span>
        <select class="epub-chapters grow" aria-label="Part of the book" prop:value=shown
            on:keydown=|ev| ev.stop_propagation() on:change=pick>
            <option value="info">"Book info"</option>
            <optgroup label="Chapters">
                {move || chapters().into_iter().map(|c| view! {
                    <option value=format!("ch:{}", c.path) title=c.path.clone()>{c.label}</option>
                }).collect_view()}
            </optgroup>
        </select>
        <span class="epub-of">{move || match (here(), count()) { (Some(k), n) => format!("{} of {n}", k + 1), _ => String::new() }}</span>
        <span class="btn-group" role="group">
            <button class="btn" title="Previous chapter" aria-label="Previous chapter"
                disabled=move || here().is_none_or(|k| k == 0) on:click=move |_| step(-1)>{ico(Icon::Prev)}</button>
            <button class="btn" title="Next chapter" aria-label="Next chapter"
                disabled=move || here().map_or(count() == 0, |k| k + 1 >= count()) on:click=move |_| step(1)>{ico(Icon::Next)}</button>
        </span>
        // Shown only when something takes a while, so a quick chapter change doesn't flash it.
        <span class="spin bar-spin" class:on=move || ep.busy.get()></span>
        <span class="btn-group epub-save" role="group" node_ref=save_menu>
            <button class="btn primary" title=save_title aria-label=save_title
                disabled=move || !dirty.get() on:click=move |_| ep.save()>{ico(Icon::Save)}</button>
            <button class="btn more" class:active=save_open title="More ways to save" aria-label="More ways to save" aria-haspopup="menu"
                aria-expanded=move || save_open.get().to_string()
                on:click=move |_| save_open.update(|o| *o = !*o)>{ico(Icon::Chevron)}</button>
            {move || save_open.get().then(|| view! {
                <span class="save-menu" role="menu">
                    <button role="menuitem" on:click=move |_| { save_open.set(false); ep.export(); }>{ico(Icon::Copy)}{copy_title.clone()}</button>
                </span>
            })}
        </span>
    }
}

/// The editor's notices and the part of the book shown, under the preview's bar.
#[component]
pub fn EditBody() -> impl IntoView {
    let ep = expect_context::<Ep>();
    ep.reshow();
    let shown = move |v: &View| ep.view.with(|w| w == v);
    let in_chapter = Memo::new(move |_| ep.view.with(|v| matches!(v, View::Chapter(_))));
    let outline = move || {
        let chapters = ep
            .book
            .with(|b| b.as_ref().map(|b| b.chapters.clone()).unwrap_or_default());
        let item = move |v: View, label: String| {
            let here = v.clone();
            view! {
                <button class="pdf-head" class:here=move || shown(&here) title=label.clone() on:click=move |_| ep.go(v.clone())>
                    <span class="name">{label.clone()}</span>
                </button>
            }
        };
        view! {
            {item(View::Info, "Book info".into())}
            <div class="epub-outline-rule"></div>
            {chapters.into_iter().map(|c| view! { <OutlineChapter path=c.path label=c.label/> }).collect_view()}
        }
    };
    view! {
        <div class="epub-body book-editor">
        {move || ep.outline.get().then(|| view! {
            <nav class="pdf-outline epub-outline" aria-label="Chapters and search">
                <div class="outline-tabs" role="tablist">
                    <button role="tab" class:on=move || !ep.finding.get() && !ep.contents.get() aria-selected=move || (!ep.finding.get() && !ep.contents.get()).to_string()
                        title="The chapters, in reading order" on:click=move |_| { ep.finding.set(false); ep.contents.set(false); }>{ico(Icon::File)}"Chapters"</button>
                    <button role="tab" class:on=move || !ep.finding.get() && ep.contents.get() aria-selected=move || (!ep.finding.get() && ep.contents.get()).to_string()
                        title="The table of contents" on:click=move |_| { ep.finding.set(false); ep.contents.set(true); }>{ico(Icon::Contents)}"Contents"</button>
                    <button role="tab" class:on=move || ep.finding.get() aria-selected=move || ep.finding.get().to_string()
                        title=crate::os::keys("Find & replace (⌘F)") on:click=move |_| ep.open_find(false)>{ico(Icon::Search)}"Search"</button>
                </div>
                {move || if ep.finding.get() {
                    view! { <FindView/> }.into_any()
                } else if ep.contents.get() {
                    view! { <ContentsView/> }.into_any()
                } else {
                    outline().into_any()
                }}
            </nav>
        })}
        <div class="epub-edit">
            {move || ep.confirm_close.get().then(|| view! {
                <div class="notice">
                    <span class="text">{format!("This book has changes that aren't saved to {}.", ep.place())}</span>
                    <button class="btn primary" on:click=move |_| ep.save()>"Save"</button>
                    <button class="btn danger" on:click=move |_| ep.close(true)>"Discard"</button>
                    <button class="btn" on:click=move |_| ep.confirm_close.set(false)>"Keep editing"</button>
                </div>
            })}
            {move || {
                let (msg, kind) = ep.status.get();
                (!msg.is_empty()).then(|| view! {
                    <div class=format!("notice {kind}")>
                        <span class="text">{msg}</span>
                        <button class="btn" title="Dismiss" aria-label="Dismiss" on:click=move |_| ep.say("", "")>{ico(Icon::Close)}</button>
                    </div>
                })
            }}
            // Drawn anew only between Book info and the chapters: going from one chapter to the
            // next keeps the chapter's bar and page, so they don't flicker.
            {move || if in_chapter.get() { view! { <ChapterView/> }.into_any() } else { view! { <InfoView/> }.into_any() }}
        </div>
        </div>
    }
}
