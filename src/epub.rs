//! The EPUB editor: book info and cover, find & replace, and each chapter in a visual or source view.
//! The book itself lives in the backend (src-tauri/src/epub.rs) until it's saved; this module only
//! holds what's on screen. Chapter edits are sent to the backend before any other action, so the
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
}

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

impl Ep {
    pub fn new() -> Self {
        Ep {
            book: RwSignal::new(None),
            view: RwSignal::new(View::Info),
            mode: RwSignal::new(Mode::Visual),
            doc: RwSignal::new(None),
            source: RwSignal::new(String::new()),
            chapter_dirty: RwSignal::new(false),
            status: RwSignal::new((String::new(), "")),
            busy: RwSignal::new(false),
            confirm_close: RwSignal::new(false),
            query: RwSignal::new(Query::default()),
            found: RwSignal::new(None),
            frame: NodeRef::new(),
        }
    }

    fn id(self) -> u32 {
        self.book.with_untracked(|b| b.as_ref().map_or(0, |b| b.id))
    }

    fn dirty(self) -> bool {
        self.chapter_dirty.get() || self.book.with(|b| b.as_ref().is_some_and(|b| b.dirty))
    }

    fn say(self, msg: impl Into<String>, kind: &'static str) {
        self.status.set((msg.into(), kind));
    }

    pub async fn open(self, path: String) -> Result<(), String> {
        let book = call::<Book>("epub_open", json!({ "path": path })).await?;
        self.view.set(View::Info);
        self.mode.set(Mode::Visual);
        self.doc.set(None);
        self.chapter_dirty.set(false);
        self.found.set(None);
        self.confirm_close.set(false);
        self.say("", "");
        self.book.set(Some(book));
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
                let body = self.visual_body()?;
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
            self.say("Saving to the phone…", "");
            let s = call::<Saved>("epub_save", json!({ "id": self.id() })).await?;
            self.book.set(Some(s.book));
            self.confirm_close.set(false);
            let note = if s.backup { " The old version is backed up on the Mac." } else { "" };
            self.say(format!("Saved to the phone.{note}"), "ok");
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

    fn close(self, force: bool) {
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
        self.doc.set(None);
        self.book.set(None);
    }

    fn set_meta(self, meta: Meta) {
        self.act(move || async move {
            let b = call::<Book>("epub_set_meta", json!({ "id": self.id(), "meta": meta })).await?;
            self.book.set(Some(b));
            self.say("Book info updated. Save to write it to the phone.", "ok");
            Ok(())
        });
    }

    fn cover(self) {
        self.act(move || async move {
            if let Some(b) = call::<Option<Book>>("epub_cover", json!({ "id": self.id() })).await? {
                self.book.set(Some(b));
                self.say("Cover changed. Save to write it to the phone.", "ok");
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
                format!("Replaced {}. Save to write the book to the phone.", plural(r.count, "match", "matches")),
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

#[component]
pub fn EpubEditor() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let name = move || ep.book.with(|b| b.as_ref().map(|b| b.name.clone()).unwrap_or_default());
    let dirty = Memo::new(move |_| ep.dirty());
    let chapters = move || ep.book.with(|b| b.as_ref().map(|b| b.chapters.clone()).unwrap_or_default());
    let on = move |v: View| move || ep.view.with(|x| *x == v);
    view! {
        <div class="app">
            <nav class="side">
                <div class="device">
                    <b title=move || ep.book.with(|b| b.as_ref().map(|b| b.path.clone()).unwrap_or_default())>{name}</b>
                    <span class="kind">"EPUB"</span>
                </div>
                <button class="item" class:active=on(View::Info) on:click=move |_| ep.go(View::Info)>"Book info"</button>
                <button class="item" class:active=on(View::Find) on:click=move |_| ep.go(View::Find)>"Find & replace"</button>
                <h2>"Chapters"</h2>
                <For each=chapters key=|c| c.path.clone() let(c)>
                    {
                        let (p1, p2) = (c.path.clone(), c.path.clone());
                        view! {
                            <button class="item" title=c.path.clone()
                                class:active=move || ep.view.with(|v| matches!(v, View::Chapter(x) if *x == p1))
                                on:click=move |_| ep.go(View::Chapter(p2.clone()))>{c.label.clone()}</button>
                        }
                    }
                </For>
            </nav>
            <div class="main">
                <div class="bar">
                    <button class="btn" on:click=move |_| ep.close(false)>"← Files"</button>
                    <span class="title">{name}{move || dirty.get().then_some(" •")}</span>
                    <span class="spacer"></span>
                    {move || ep.busy.get().then(|| view! { <span class="spin"></span> })}
                    <button class="btn" disabled=move || ep.busy.get() on:click=move |_| ep.export()
                        title="Save the book, with your edits, to a file on the Mac">"Save copy on Mac…"</button>
                    <button class="btn primary" title="⌘S" disabled=move || !dirty.get() || ep.busy.get()
                        on:click=move |_| ep.save()>"Save to phone"</button>
                </div>
                {move || ep.confirm_close.get().then(|| view! {
                    <div class="notice">
                        <span class="text">"This book has changes that aren't saved to the phone."</span>
                        <button class="btn primary" on:click=move |_| ep.save()>"Save"</button>
                        <button class="btn danger" on:click=move |_| ep.close(true)>"Discard changes"</button>
                        <button class="btn" on:click=move |_| ep.confirm_close.set(false)>"Keep editing"</button>
                    </div>
                })}
                {move || {
                    let (msg, kind) = ep.status.get();
                    (!msg.is_empty()).then(|| view! {
                        <div class=format!("notice {kind}")>
                            <span class="text">{msg}</span>
                            <button class="btn" on:click=move |_| ep.say("", "")>"Dismiss"</button>
                        </div>
                    })
                }}
                {move || match ep.view.get() {
                    View::Info => view! { <InfoView/> }.into_any(),
                    View::Find => view! { <FindView/> }.into_any(),
                    View::Chapter(path) => view! { <ChapterView path/> }.into_any(),
                }}
            </div>
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
                <p class="sub">"Changes stay in the app until you save the book to the phone."</p>
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
                        <p class="sub">"Pick an image on the Mac (JPEG, PNG, GIF, WebP or SVG)."</p>
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
                            <dt>"On the phone"</dt><dd class="path">{b.path}</dd>
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
    let label = ep.book.with_untracked(|b| {
        b.as_ref().and_then(|b| b.chapters.iter().find(|c| c.path == path).map(|c| c.label.clone())).unwrap_or_default()
    });
    let visual_ok = move || ep.doc.with(|d| d.as_ref().is_some_and(|d| d.html.is_some()));
    view! {
        <div class="bar tools">
            <span class="title" title=path.clone()>{label}</span>
            <div class="seg">
                <button class:active=move || ep.mode.get() == Mode::Visual on:click=move |_| ep.set_mode(Mode::Visual)>"Visual"</button>
                <button class:active=move || ep.mode.get() == Mode::Source on:click=move |_| ep.set_mode(Mode::Source)>"Source"</button>
            </div>
            <span class="spacer"></span>
            {move || (ep.mode.get() == Mode::Visual && visual_ok()).then(|| view! {
                <div class="fmt">
                    {FORMAT.iter().map(|&(cmd, arg, text, title)| view! {
                        <button title=title on:mousedown=|e| e.prevent_default() on:click=move |_| ep.exec(cmd, arg)>{text}</button>
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
            (Mode::Source, Some(_)) => view! {
                <div class="editor">
                    <textarea spellcheck="false" prop:value=move || ep.source.get()
                        on:input=move |ev| { ep.source.set(event_target_value(&ev)); ep.chapter_dirty.set(true); }></textarea>
                </div>
            }.into_any(),
        }}
    }
}
