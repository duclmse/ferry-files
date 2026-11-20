//! The EPUB editor, shown in a pane's preview: book info and cover, find & replace, and each
//! chapter in a visual or source view. The book itself lives in the backend (src-tauri/src/epub.rs)
//! until it's saved; this module only holds what's on screen. Chapter edits are sent to the backend before any other action, so the
//! backend copy is always complete when the book is saved.

use crate::api::*;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::future::Future;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

#[derive(Clone, PartialEq)]
pub enum View {
    Info,
    Find,
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
    /// Source shown in colour.
    colours: RwSignal<bool>,
    /// The pane whose preview shows the editor, so only one place draws it.
    pub host: RwSignal<Option<usize>>,
    /// The visual view's edits, taken as its frame went away (the preview moved to another file).
    kept: StoredValue<Option<String>>,
}

const COLOURS_KEY: &str = "ferry-files.epub-colours";

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn human_size(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if unit == "B" { format!("{v:.0} B") } else { format!("{v:.1} {unit}") };
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

/// Whether the open book is on the Mac.
fn b_mac(ep: Ep) -> bool {
    ep.book.with_untracked(|b| b.as_ref().is_some_and(|b| b.src == "mac"))
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
            status: RwSignal::new((String::new(), "")),
            busy: RwSignal::new(false),
            confirm_close: RwSignal::new(false),
            query: RwSignal::new(Query::default()),
            found: RwSignal::new(None),
            frame: NodeRef::new(),
            host: RwSignal::new(None),
            kept: StoredValue::new(None),
        }
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
        self.book.with(|b| b.as_ref().is_some_and(|b| b.src == src && b.path == path))
    }

    /// Opens the book at `path` on `src` ("mac" or a device id) in pane `host`'s preview; `place`
    /// names it for sentences. Editing reads the whole book. A book already open with unsaved
    /// changes isn't replaced.
    pub async fn open(self, src: &str, place: String, path: String, host: usize) -> Result<(), String> {
        if self.book.with_untracked(|b| b.as_ref().is_some_and(|b| b.src == src && b.path == path)) {
            self.host.set(Some(host));
            return Ok(());
        }
        if self.dirty() {
            let name = self.book.with_untracked(|b| b.as_ref().map(|b| b.name.clone()).unwrap_or_default());
            return Err(format!("{name} has changes that aren't saved. Save or discard them first."));
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
        self.view.set(View::Info);
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
        let View::Chapter(path) = self.view.get_untracked() else { return Ok(()) };
        let id = self.id();
        let book = match self.mode.get_untracked() {
            Mode::Source => {
                let source = self.source.get_untracked();
                call::<Book>("epub_set_source", json!({ "id": id, "path": path, "source": source })).await
            }
            Mode::Visual => {
                let body = match self.kept.try_update_value(Option::take).flatten() {
                    Some(b) => b,
                    None => self.visual_body().inspect_err(|_| self.chapter_dirty.set(false))
                        .map_err(|e| format!("The last edits to this chapter were lost. {e}"))?,
                };
                call::<Book>("epub_set_body", json!({ "id": id, "path": path, "body": body })).await
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
        let doc = call::<ChapterDoc>("epub_chapter", json!({ "id": self.id(), "path": path })).await?;
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
        let View::Chapter(p) = self.view.get_untracked() else { return };
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
            let note = if s.backup { format!(" The old version is backed up on {}.", crate::os::the_computer()) } else { String::new() };
            self.say(format!("Saved to {}.{note}", self.place()), "ok");
            Ok(())
        });
    }

    fn export(self) {
        self.act(move || async move {
            if let Some(dest) = call::<Option<String>>("epub_export", json!({ "id": self.id() })).await? {
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
            self.say(format!("Book info updated. Save to write it to {}.", self.place()), "ok");
            Ok(())
        });
    }

    fn cover(self) {
        self.act(move || async move {
            if let Some(b) = call::<Option<Book>>("epub_cover", json!({ "id": self.id() })).await? {
                self.book.set(Some(b));
                self.say(format!("Cover changed. Save to write it to {}.", self.place()), "ok");
            }
            Ok(())
        });
    }

    async fn search(self) -> Result<(), String> {
        let q = self.query.get_untracked();
        let f = call::<Found>("epub_find", json!({ "id": self.id(), "query": q })).await?;
        self.found.set(Some(f));
        Ok(())
    }

    fn find(self) {
        self.act(move || self.search());
    }

    fn replace(self) {
        self.act(move || async move {
            let q = self.query.get_untracked();
            let r = call::<Replaced>("epub_replace", json!({ "id": self.id(), "query": q })).await?;
            self.book.set(Some(r.book));
            self.search().await?;
            self.say(
                format!("Replaced {}. Save to write the book to {}.", plural(r.count, "match", "matches"), self.place()),
                "ok",
            );
            Ok(())
        });
    }

    // ----- The visual chapter view: an editable iframe holding the chapter as HTML -----

    fn frame_doc(self) -> Option<web_sys::HtmlDocument> {
        self.frame.get_untracked()?.content_document()?.dyn_into().ok()
    }

    /// The edited <body> content as XHTML.
    fn visual_body(self) -> Result<String, String> {
        let doc = self.frame_doc().ok_or("The chapter view isn't ready yet.")?;
        let body = doc.body().ok_or("The chapter has no body.")?;
        let ser = web_sys::XmlSerializer::new().map_err(|e| format!("{e:?}"))?;
        let kids = body.child_nodes();
        let mut out = String::new();
        for i in 0..kids.length() {
            let Some(n) = kids.item(i) else { continue };
            let s = ser.serialize_to_string(&n).map_err(|e| format!("{e:?}"))?;
            if n.node_type() == web_sys::Node::ELEMENT_NODE {
                // The serializer marks each top-level element with the XHTML namespace; <html> already declares it.
                out.push_str(&s.replacen(r#" xmlns="http://www.w3.org/1999/xhtml""#, "", 1));
            } else {
                out.push_str(&s);
            }
        }
        Ok(out)
    }

    fn frame_loaded(self) {
        let Some(doc) = self.frame_doc() else { return };
        doc.set_design_mode("on");
        let _ = doc.exec_command_with_show_ui_and_value("defaultParagraphSeparator", false, "p");
        let input = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| self.chapter_dirty.set(true));
        let _ = doc.add_event_listener_with_callback("input", input.as_ref().unchecked_ref());
        input.forget();
        // Key presses inside the iframe don't reach the app window, so ⌘S is caught here too.
        let keys = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if (e.meta_key() || e.ctrl_key()) && e.key() == "s" {
                e.prevent_default();
                self.save();
            }
        });
        let _ = doc.add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref());
        keys.forget();
    }

    fn exec(self, cmd: &str, arg: &str) {
        if let Some(doc) = self.frame_doc() {
            let _ = doc.exec_command_with_show_ui_and_value(cmd, false, arg);
            self.chapter_dirty.set(true);
        }
    }
}

/// The editor's controls, in the preview's bar: which part of the book is shown, Save copy and Save.
#[component]
pub fn EditBar() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let dirty = Memo::new(move |_| ep.dirty());
    let chapters = move || ep.book.with(|b| b.as_ref().map(|b| b.chapters.clone()).unwrap_or_default());
    let shown = move || match ep.view.get() {
        View::Info => "info".to_string(),
        View::Find => "find".to_string(),
        View::Chapter(p) => format!("ch:{p}"),
    };
    let pick = move |ev| {
        let v = event_target_value(&ev);
        ep.go(match v.as_str() {
            "info" => View::Info,
            "find" => View::Find,
            _ => View::Chapter(v.strip_prefix("ch:").unwrap_or(&v).to_string()),
        });
    };
    let save_title = move || {
        ep.book.track();
        let to = if b_mac(ep) { String::new() } else { format!(" to {}", ep.place()) };
        let state = if dirty.get() { "" } else { " (nothing new)" };
        format!("Save{to}{state} ({})", crate::os::keys("⌘S"))
    };
    let copy_title = format!("Save a copy on {}…", crate::os::the_computer());
    view! {
        <select class="epub-chapters" aria-label="Part of the book" prop:value=shown
            on:keydown=|ev| ev.stop_propagation() on:change=pick>
            <option value="info">"Book info"</option>
            <option value="find">"Find & replace"</option>
            <optgroup label="Chapters">
                {move || chapters().into_iter().map(|c| view! {
                    <option value=format!("ch:{}", c.path) title=c.path.clone()>{c.label}</option>
                }).collect_view()}
            </optgroup>
        </select>
        {move || ep.busy.get().then(|| view! { <span class="spin"></span> })}
        <span class="spacer"></span>
        <button class="btn" title=copy_title.clone() aria-label=copy_title disabled=move || ep.busy.get()
            on:click=move |_| ep.export()>{crate::app::ico(crate::app::Icon::Copy)}</button>
        <button class="btn primary" class:unsaved=move || dirty.get() title=save_title aria-label=save_title
            disabled=move || !dirty.get() || ep.busy.get() on:click=move |_| ep.save()>{crate::app::ico(crate::app::Icon::Save)}</button>
    }
}

/// The editor's notices and the part of the book shown, under the preview's bar.
#[component]
pub fn EditBody() -> impl IntoView {
    let ep = expect_context::<Ep>();
    ep.reshow();
    view! {
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
                        <button class="btn" title="Dismiss" aria-label="Dismiss" on:click=move |_| ep.say("", "")>{crate::app::ico(crate::app::Icon::Close)}</button>
                    </div>
                })
            }}
            {move || match ep.view.get() {
                View::Info => view! { <InfoView/> }.into_any(),
                View::Find => view! { <FindView/> }.into_any(),
                View::Chapter(path) => view! { <ChapterView path/> }.into_any(),
            }}
        </div>
    }
}

fn field(form: RwSignal<Meta>, label: &'static str, wide: bool, rows: u32, get: fn(&Meta) -> String, set: fn(&mut Meta, String)) -> impl IntoView {
    let value = move || form.with(get);
    let input = move |ev| form.update(|m| set(m, event_target_value(&ev)));
    view! {
        <label class="field" class:wide=wide>
            <span>{label}</span>
            {if rows > 1 {
                view! { <textarea rows=rows prop:value=value on:input=input></textarea> }.into_any()
            } else {
                view! { <input prop:value=value on:input=input/> }.into_any()
            }}
        </label>
    }
}

fn lines(v: &[String]) -> String {
    v.join("\n")
}

/// Keeps every line, even empty ones, so typing Enter in the box isn't undone. The backend drops blanks.
fn split_lines(s: String) -> Vec<String> {
    if s.is_empty() { vec![] } else { s.split('\n').map(String::from).collect() }
}

#[component]
fn InfoView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let saved = move || ep.book.with_untracked(|b| b.as_ref().map(|b| b.meta.clone()).unwrap_or_default());
    let form = RwSignal::new(saved());
    // When the book's info changes (after Apply), show the cleaned-up values.
    Effect::new(move |prev: Option<Meta>| {
        let m = ep.book.with(|b| b.as_ref().map(|b| b.meta.clone()).unwrap_or_default());
        if prev.as_ref().is_some_and(|p| *p != m) {
            form.set(m.clone());
        }
        m
    });
    let changed = move || ep.book.with(|b| b.as_ref().is_some_and(|b| b.meta != form.get()));
    let book = move || ep.book.get();
    view! {
        <div class="scroll">
            <form class="card" on:submit=move |e| { e.prevent_default(); ep.set_meta(form.get_untracked()); }>
                <h2>"Book info"</h2>
                <p class="sub">{move || { ep.book.track(); format!("Changes stay in the app until you save the book to {}.", ep.place()) }}</p>
                <div class="grid">
                    {field(form, "Title *", true, 1, |m| m.title.clone(), |m, v| m.title = v)}
                    {field(form, "Authors (one per line)", true, 2, |m| lines(&m.creators), |m, v| m.creators = split_lines(v))}
                    {field(form, "Language * (en, vi, ja…)", false, 1, |m| m.language.clone(), |m, v| m.language = v)}
                    {field(form, "Publisher", false, 1, |m| m.publisher.clone(), |m, v| m.publisher = v)}
                    {field(form, "Publication date (YYYY-MM-DD)", false, 1, |m| m.date.clone(), |m, v| m.date = v)}
                    {field(form, "Identifier * (ISBN, UUID…)", false, 1, |m| m.identifier.clone(), |m, v| m.identifier = v)}
                    {field(form, "Subjects (one per line)", true, 2, |m| lines(&m.subjects), |m, v| m.subjects = split_lines(v))}
                    {field(form, "Description", true, 4, |m| m.description.clone(), |m, v| m.description = v)}
                </div>
                <div class="actions">
                    <span class="spacer"></span>
                    <button type="button" class="btn" disabled=move || !changed() on:click=move |_| form.set(saved())>"Discard changes"</button>
                    <button type="submit" class="btn primary" disabled=move || !changed() || ep.busy.get()>"Apply"</button>
                </div>
            </form>
            <div class="card">
                <h2>"Cover"</h2>
                <div class="cover-row">
                    {move || match book().and_then(|b| b.cover) {
                        Some(url) => view! { <img class="cover" src=url alt="Book cover"/> }.into_any(),
                        None => view! { <div class="cover none">"No cover image"</div> }.into_any(),
                    }}
                    <div>
                        <p class="sub">{format!("Pick an image on {} (JPEG, PNG, GIF, WebP or SVG).", crate::os::the_computer())}</p>
                        <button class="btn" disabled=move || ep.busy.get() on:click=move |_| ep.cover()>
                            {move || if book().is_some_and(|b| b.cover.is_some()) { "Replace cover…" } else { "Add a cover…" }}
                        </button>
                    </div>
                </div>
            </div>
            <div class="card">
                <h2>"Package"</h2>
                {move || book().map(|b| {
                    let s = b.stats;
                    view! {
                        <dl class="stats">
                            <dt>"EPUB version"</dt><dd>{if s.version.is_empty() { "unknown".to_string() } else { s.version }}</dd>
                            <dt>"Chapters"</dt><dd>{s.chapters}</dd>
                            <dt>"Files"</dt><dd>{format!("{} ({})", s.files, human_size(s.bytes))}</dd>
                            <dt>"Images"</dt><dd>{s.images}</dd>
                            <dt>"Stylesheets"</dt><dd>{s.styles}</dd>
                            <dt>"Fonts"</dt><dd>{s.fonts}</dd>
                            <dt>{format!("On {}", ep.place())}</dt><dd class="path">{b.path}</dd>
                        </dl>
                    }
                })}
            </div>
        </div>
    }
}

#[component]
fn FindView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let confirm = RwSignal::new(false);
    let q = ep.query;
    let text = move |get: fn(&Query) -> String, set: fn(&mut Query, String)| {
        view! { <input prop:value=move || q.with(get) on:input=move |ev| q.update(|x| set(x, event_target_value(&ev)))/> }
    };
    let check = move |label: &'static str, get: fn(&Query) -> bool, set: fn(&mut Query, bool)| {
        view! {
            <label class="check">
                <input type="checkbox" prop:checked=move || q.with(get) on:change=move |ev| q.update(|x| set(x, event_target_checked(&ev)))/>
                {label}
            </label>
        }
    };
    view! {
        <div class="scroll">
            <form class="card" on:submit=move |e| { e.prevent_default(); confirm.set(false); ep.find(); }>
                <h2>"Find & replace"</h2>
                <p class="sub">"Searches the text of every chapter; the markup is never changed. A phrase split by formatting (half of it bold, say) won't match."</p>
                <div class="grid">
                    <label class="field"><span>"Find"</span>{text(|x| x.find.clone(), |x, v| x.find = v)}</label>
                    <label class="field"><span>"Replace with"</span>{text(|x| x.replace.clone(), |x, v| x.replace = v)}</label>
                </div>
                <div class="actions">
                    {check("Match case", |x| x.match_case, |x, v| x.match_case = v)}
                    {check("Whole word", |x| x.whole_word, |x, v| x.whole_word = v)}
                    <span class="spacer"></span>
                    <button type="submit" class="btn" disabled=move || q.with(|x| x.find.is_empty()) || ep.busy.get()>"Find"</button>
                    <button type="button" class="btn primary" disabled=move || q.with(|x| x.find.is_empty()) || ep.busy.get()
                        on:click=move |_| confirm.set(true)>"Replace all…"</button>
                </div>
                {move || confirm.get().then(|| {
                    let x = q.get();
                    view! {
                        <div class="confirm">
                            <span class="text">{format!("Replace every “{}” with “{}” in all chapters?", x.find, x.replace)}</span>
                            <button type="button" class="btn danger solid" on:click=move |_| { confirm.set(false); ep.replace(); }>"Replace all"</button>
                            <button type="button" class="btn" on:click=move |_| confirm.set(false)>"Cancel"</button>
                        </div>
                    }
                })}
                {move || ep.found.get().map(|f| {
                    let summary = if f.total == 0 {
                        "No matches.".to_string()
                    } else {
                        format!("{} in {}.", plural(f.total, "match", "matches"), plural(f.hits.len(), "chapter", "chapters"))
                    };
                    let skipped = (!f.skipped.is_empty()).then(|| format!(
                        "Not searched, because their XHTML is broken: {}", f.skipped.join(", ")));
                    view! {
                        <p class="sub found">{summary}</p>
                        {skipped.map(|s| view! { <p class="sub error">{s}</p> })}
                        <ul class="results">
                            {f.hits.into_iter().map(|h| {
                                let path = h.path.clone();
                                view! {
                                    <li>
                                        <button type="button" on:click=move |_| ep.go(View::Chapter(path.clone()))>
                                            <b>{h.label}</b>" "<span class="kind">{h.count}</span>
                                            {h.snippets.into_iter().map(|s| view! {
                                                <span class="snip">"…"{s.before}<mark>{s.text}</mark>{s.after}"…"</span>
                                            }).collect_view()}
                                        </button>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    }
                })}
            </form>
        </div>
    }
}

const FORMAT: &[(&str, &str, &str, &str)] = &[
    ("bold", "", "B", "Bold"),
    ("italic", "", "I", "Italic"),
    ("formatBlock", "h1", "H1", "Heading 1"),
    ("formatBlock", "h2", "H2", "Heading 2"),
    ("formatBlock", "h3", "H3", "Heading 3"),
    ("formatBlock", "p", "¶", "Paragraph"),
    ("formatBlock", "blockquote", "“ ”", "Quote"),
    ("insertUnorderedList", "", "• List", "Bulleted list"),
    ("removeFormat", "", "Clear", "Clear formatting"),
    ("undo", "", "↶", "Undo"),
    ("redo", "", "↷", "Redo"),
];

#[component]
fn ChapterView(path: String) -> impl IntoView {
    let ep = expect_context::<Ep>();
    let visual_ok = move || ep.doc.with(|d| d.as_ref().is_some_and(|d| d.html.is_some()));
    // When the preview moves to another file, the frame goes with it: its edits are kept to send later.
    on_cleanup(move || {
        if ep.mode.get_untracked() == Mode::Visual && ep.chapter_dirty.get_untracked() {
            if let Ok(b) = ep.visual_body() {
                ep.kept.set_value(Some(b));
            }
        }
    });
    view! {
        <div class="viewer-bar tools">
            <div class="seg">
                <button class:active=move || ep.mode.get() == Mode::Visual on:click=move |_| ep.set_mode(Mode::Visual)>"Visual"</button>
                <button class:active=move || ep.mode.get() == Mode::Source on:click=move |_| ep.set_mode(Mode::Source)>"Source"</button>
            </div>
            {move || (ep.mode.get() == Mode::Source).then(|| view! {
                <button class="btn" class:active=move || ep.colours.get() title="Colours" aria-label="Colours"
                    on:click=move |_| {
                        let on = !ep.colours.get_untracked();
                        ep.colours.set(on);
                        crate::app::store(COLOURS_KEY, if on { "1" } else { "0" });
                    }>{crate::app::ico(crate::app::Icon::Highlight)}</button>
            })}
            {move || (ep.mode.get() == Mode::Visual && visual_ok()).then(|| view! {
                <div class="fmt">
                    {FORMAT.iter().map(|&(cmd, arg, text, title)| view! {
                        <button title=title aria-label=title on:mousedown=|e| e.prevent_default() on:click=move |_| ep.exec(cmd, arg)>{text}</button>
                    }).collect_view()}
                </div>
            })}
        </div>
        {move || match (ep.mode.get(), ep.doc.get()) {
            (_, None) => view! { <div class="empty"><span class="spin"></span></div> }.into_any(),
            (Mode::Visual, Some(ChapterDoc { html: Some(html), .. })) => view! {
                // No allow-scripts: a book's scripts must never run next to the app's commands.
                <iframe class="page" sandbox="allow-same-origin" node_ref=ep.frame srcdoc=html
                    on:load=move |_| ep.frame_loaded()></iframe>
            }.into_any(),
            (Mode::Visual, Some(d)) => view! {
                <div class="notice error">
                    <span class="text">"This chapter isn't well-formed XHTML, so it can't be shown visually. "
                        {d.problem.unwrap_or_default()}</span>
                    <button class="btn" on:click=move |_| ep.set_mode(Mode::Source)>"Fix it in Source"</button>
                </div>
            }.into_any(),
            (Mode::Source, Some(_)) => view! { <SourceEdit path=path.clone()/> }.into_any(),
        }}
    }
}

/// The chapter's XHTML: a clear-lettered box over a layer that draws it in colour (as the text
/// editor does), or the plain box when colours are off or the chapter is big.
#[component]
fn SourceEdit(path: String) -> impl IntoView {
    let ep = expect_context::<Ep>();
    let layer = NodeRef::<html::Pre>::new();
    let ta = NodeRef::<html::Textarea>::new();
    // The runs and the text they were for, and which colouring is the latest.
    let runs = StoredValue::new(None::<Vec<(u8, u32)>>);
    let ran_on = StoredValue::new(String::new());
    let asked = StoredValue::new(0u32);
    let timer = StoredValue::new(None::<leptos::leptos_dom::helpers::TimeoutHandle>);
    // The text as the box holds it, with line breaks as LF.
    let text = move || ep.source.with_untracked(|s| s.replace("\r\n", "\n"));
    let layered = move || ep.colours.get() && ep.source.with(|s| s.len() <= crate::app::LAYER_MAX);
    let draw = move || {
        let Some(l) = layer.get_untracked() else { return };
        let t = text();
        let r = ran_on.with_value(|old| runs.with_value(|r| r.as_ref().map(|r| if *old == t { r.clone() } else { crate::app::shift_runs(r, old, &t) })));
        l.set_inner_html(&crate::app::code_html(&t, r.as_deref(), &[], None, 1..usize::MAX));
        if let Some(b) = ta.get_untracked() {
            l.set_scroll_top(b.scroll_top());
            l.set_scroll_left(b.scroll_left());
        }
    };
    let colour = move || {
        let t = text();
        let n = asked.get_value() + 1;
        asked.set_value(n);
        let name = path.clone();
        spawn_local(async move {
            let Ok(c) = call::<TextColours>("colour_text", json!({ "name": name, "text": t })).await else { return };
            if asked.try_get_value() != Some(n) {
                return;
            }
            runs.set_value(c.runs);
            ran_on.set_value(t);
            draw();
        });
    };
    Effect::new(move |_| {
        ep.source.track();
        if !layered() {
            return;
        }
        draw();
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        let colour = colour.clone();
        timer.set_value(set_timeout_with_handle(move || colour(), std::time::Duration::from_millis(250)).ok());
    });
    on_cleanup(move || {
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    let scrolled = move |_| {
        if let (Some(b), Some(l)) = (ta.get_untracked(), layer.get_untracked()) {
            l.set_scroll_top(b.scroll_top());
            l.set_scroll_left(b.scroll_left());
        }
    };
    view! {
        <div class="code-edit source" class:layered=layered>
            <pre class="code-layer" node_ref=layer aria-hidden="true"></pre>
            <textarea class="code-input" node_ref=ta spellcheck="false" autocomplete="off" autocapitalize="off"
                aria-label="Chapter source" prop:value=move || ep.source.get()
                on:input=move |ev| { ep.source.set(event_target_value(&ev)); ep.chapter_dirty.set(true); }
                on:scroll=scrolled></textarea>
        </div>
    }
}
