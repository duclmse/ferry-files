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
            outline: RwSignal::new(crate::app::stored_flag(OUTLINE_KEY, false)),
            renaming: RwSignal::new(None),
            replacing: RwSignal::new(false),
            find_focus: RwSignal::new((0, false)),
            finding: RwSignal::new(false),
            contents: RwSignal::new(false),
            anchor: StoredValue::new(None),
            find_timer: StoredValue::new(None),
            replace_from: StoredValue::new(None),
            zoom: RwSignal::new(crate::app::stored(ZOOM_KEY).and_then(|z| z.parse().ok()).unwrap_or(100).clamp(ZOOM_MIN, ZOOM_MAX)),
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
        self.view.set(book.chapters.first().map_or(View::Info, |c| View::Chapter(c.path.clone())));
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
                    None => self
                        .visual_body()
                        .inspect_err(|_| self.chapter_dirty.set(false))
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

    /// Find & replace (⌘F), with Replace with (⌘R): shown, with the cursor in its box.
    pub fn open_find(self, replace: bool) {
        if replace {
            self.replacing.set(true);
        }
        self.finding.set(true);
        if !self.outline.get_untracked() {
            self.toggle_outline();
        }
        self.find_focus.update(|(n, r)| (*n, *r) = (*n + 1, replace));
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
        let same = self.book.with_untracked(|b| b.as_ref().and_then(|b| b.chapters.iter().find(|c| c.path == path)).is_some_and(|c| c.label == label.trim()));
        if same {
            return;
        }
        self.act(move || async move {
            let b = call::<Book>("epub_rename_chapter", json!({ "id": self.id(), "path": path, "label": label })).await?;
            self.book.set(Some(b));
            self.say(format!("Chapter renamed in the contents. Save to write the book to {}.", self.place()), "ok");
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
        if self.query.with_untracked(|q| q.find.trim().is_empty()) {
            self.found.set(None);
            return;
        }
        self.act(move || self.search());
    }

    /// Finds once typing has stopped for a moment (or, while something else runs, after it).
    fn find_soon(self) {
        if let Some(h) = self.find_timer.get_value() {
            h.clear();
        }
        let h = set_timeout_with_handle(
            move || {
                self.find_timer.set_value(None);
                if self.busy.get_untracked() {
                    self.find_soon();
                } else {
                    self.find();
                }
            },
            FIND_DELAY,
        );
        self.find_timer.set_value(h.ok());
    }

    /// Whether the Search tab shows (the pane beside the page is open on it).
    fn searching(self) -> bool {
        self.outline.get() && self.finding.get()
    }

    /// The Chapters (or Contents) tab: opens the pane on it, or closes the pane when it's showing.
    fn show_chapters(self) {
        if self.outline.get_untracked() && !self.finding.get_untracked() {
            self.toggle_outline();
        } else {
            self.finding.set(false);
            if !self.outline.get_untracked() {
                self.toggle_outline();
            }
        }
    }

    /// Opens an entry of the contents: its chapter, at its place.
    fn open_entry(self, path: String, fragment: Option<String>) {
        let here = self.view.with_untracked(|v| matches!(v, View::Chapter(p) if *p == path));
        if here {
            if let Some(f) = fragment {
                self.scroll_to(&f);
            }
        } else {
            self.anchor.set_value(fragment);
            self.go(View::Chapter(path));
        }
    }

    fn scroll_to(self, id: &str) {
        if let Some(el) = self.frame_doc().and_then(|d| d.get_element_by_id(id)) {
            el.scroll_into_view_with_bool(true);
        }
    }

    /// Adds the cursor's place in the open chapter to the contents (see `epub_toc_add`).
    fn toc_add(self, label: String, level: &'static str) {
        let View::Chapter(path) = self.view.get_untracked() else {
            return self.say("Open a chapter and click where the entry should go first.", "error");
        };
        let cut = match self.cut(true) {
            Ok(c) => c,
            Err(e) => return self.say(e, "error"),
        };
        self.act(move || async move {
            let mut args = cut;
            args["id"] = json!(self.id());
            args["path"] = json!(path);
            args["label"] = json!(label);
            args["level"] = json!(level);
            let b = call::<Book>("epub_toc_add", args).await?;
            let added = b.toc.len() > self.book.with_untracked(|o| o.as_ref().map_or(0, |o| o.toc.len()));
            self.book.set(Some(b));
            // The place may have been given an id.
            self.load_chapter(&path).await?;
            if added {
                self.say(format!("Added to the contents. Save to write the book to {}.", self.place()), "ok");
            }
            Ok(())
        });
    }

    fn toc_remove(self, index: usize) {
        self.act(move || async move {
            let label = self.book.with_untracked(|b| b.as_ref().and_then(|b| b.toc.get(index)).map(|e| e.label.clone()).unwrap_or_default());
            let b = call::<Book>("epub_toc_remove", json!({ "id": self.id(), "index": index })).await?;
            self.book.set(Some(b));
            self.say(format!("Took “{label}” out of the contents. Save to write the book to {}.", self.place()), "ok");
            Ok(())
        });
    }

    /// Replaces the next match: in the open chapter after the last one replaced there, else in
    /// the next chapter with matches (which is opened). The matches are found again after it.
    fn replace_next(self) {
        self.act(move || async move {
            if self.found.with_untracked(Option::is_none) {
                self.search().await?;
            }
            let q = self.query.get_untracked();
            let open = match self.view.get_untracked() {
                View::Chapter(p) => Some(p),
                View::Info => None,
            };
            let hits: Vec<(String, String)> =
                self.found.with_untracked(|f| f.iter().flat_map(|f| f.hits.iter().map(|h| (h.path.clone(), h.label.clone()))).collect());
            // The open chapter first (on from the last replacement), then those after it, round.
            let k = open.as_ref().and_then(|o| hits.iter().position(|h| h.0 == *o)).map_or(0, |k| k + 1);
            let mut order: Vec<(String, usize)> = vec![];
            if let Some(o) = &open {
                let from = self.replace_from.with_value(|r| r.as_ref().filter(|(rq, rp, _)| *rq == q && rp == o).map_or(0, |r| r.2));
                order.push((o.clone(), from));
            }
            order.extend(hits[k.min(hits.len())..].iter().chain(&hits[..k.min(hits.len())]).filter(|h| Some(&h.0) != open.as_ref()).map(|h| (h.0.clone(), 0)));
            for (path, from) in order {
                let r = call::<ReplacedOne>("epub_replace_one", json!({ "id": self.id(), "query": q, "path": path, "from": from })).await?;
                let Some(at) = r.at else { continue };
                let label = r.book.chapters.iter().find(|c| c.path == path).map(|c| c.label.clone()).unwrap_or_default();
                self.book.set(Some(r.book));
                self.replace_from.set_value(Some((q.clone(), path.clone(), at)));
                self.load_chapter(&path).await?;
                self.view.set(View::Chapter(path));
                self.search().await?;
                let left = self.found.with_untracked(|f| f.as_ref().map_or(0, |f| f.total));
                let left = if left == 0 { "No more matches.".to_string() } else { format!("{} left.", plural(left, "match", "matches")) };
                self.say(format!("Replaced one in “{label}”. {left} Save to write the book to {}.", self.place()), "ok");
                return Ok(());
            }
            self.replace_from.set_value(None);
            self.search().await?;
            self.say("No more matches to replace.", "");
            Ok(())
        });
    }

    fn replace(self) {
        self.act(move || async move {
            let q = self.query.get_untracked();
            let r = call::<Replaced>("epub_replace", json!({ "id": self.id(), "query": q })).await?;
            self.book.set(Some(r.book));
            self.search().await?;
            self.say(format!("Replaced {}. Save to write the book to {}.", plural(r.count, "match", "matches"), self.place()), "ok");
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

    /// Zooms the chapter. In Visual the page is built again: its edits are sent first, and it's
    /// read back with them.
    fn set_zoom(self, z: u32) {
        let z = z.clamp(ZOOM_MIN, ZOOM_MAX);
        let keep = move || {
            self.zoom.set(z);
            crate::app::store(ZOOM_KEY, &z.to_string());
        };
        if self.mode.get_untracked() != Mode::Visual {
            keep();
            return;
        }
        self.act(move || async move {
            keep();
            if let View::Chapter(p) = self.view.get_untracked() {
                self.load_chapter(&p).await?;
            }
            Ok(())
        });
    }

    /// Gives the visual page the focus back, with the cursor or selection it had.
    fn refocus(self) {
        let Some(frame) = self.frame.get_untracked() else { return };
        let Some(doc) = self.frame_doc() else { return };
        if let Some(w) = frame.content_window() {
            let _ = w.focus();
        }
        if let (Some(r), Ok(Some(sel))) = (self.range.get_value(), doc.get_selection()) {
            let _ = sel.remove_all_ranges();
            let _ = sel.add_range(&r);
        }
    }

    fn frame_loaded(self) {
        self.range.set_value(None);
        if let Some(id) = self.anchor.try_update_value(Option::take).flatten() {
            self.scroll_to(&id);
        }
        let Some(doc) = self.frame_doc() else { return };
        let kept = doc.clone();
        let moved = Closure::<dyn FnMut()>::new(move || {
            let r = kept.get_selection().ok().flatten().filter(|s| s.range_count() > 0).and_then(|s| s.get_range_at(0).ok());
            if r.is_some() {
                self.range.set_value(r);
            }
        });
        let _ = doc.add_event_listener_with_callback("selectionchange", moved.as_ref().unchecked_ref());
        moved.forget();
        doc.set_design_mode("on");
        let _ = doc.exec_command_with_show_ui_and_value("defaultParagraphSeparator", false, "p");
        let input = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| self.chapter_dirty.set(true));
        let _ = doc.add_event_listener_with_callback("input", input.as_ref().unchecked_ref());
        input.forget();
        // Key presses inside the iframe don't reach the app window, so ⌘S is caught here too.
        let keys = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            if !(e.meta_key() || e.ctrl_key()) {
                return;
            }
            match e.key().as_str() {
                "s" => self.save(),
                "f" | "ƒ" => self.open_find(e.alt_key()),
                "r" => self.open_find(true),
                _ => return,
            }
            e.prevent_default();
        });
        let _ = doc.add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref());
        keys.forget();
    }

    /// Where to cut the open chapter to move part of it, from the cursor or selection, as the
    /// arguments of `epub_move`. To the previous chapter goes what's before the cursor's block (or
    /// up to the end of the selection's last); to the next, the cursor's block (or the
    /// selection's first) and what follows.
    fn cut(self, next: bool) -> Result<serde_json::Value, String> {
        const NO_CURSOR: &str = "Click in the chapter where it should be cut first.";
        match self.mode.get_untracked() {
            Mode::Source => {
                let ta = self.source_box.get_untracked().ok_or(NO_CURSOR)?;
                let text = ta.value();
                let (start, end) = (ta.selection_start().ok().flatten().unwrap_or(0), ta.selection_end().ok().flatten().unwrap_or(0));
                // The box counts UTF-16 units; the backend counts bytes, with a line break as one.
                let bytes = |units: u32| String::from_utf16_lossy(&text.encode_utf16().take(units as usize).collect::<Vec<_>>()).len();
                let (offset, through) = if !next && start != end { (bytes(end).saturating_sub(1), true) } else { (bytes(start), false) };
                Ok(json!({ "offset": offset, "through": through }))
            }
            Mode::Visual => {
                self.refocus();
                let doc = self.frame_doc().ok_or(NO_CURSOR)?;
                let body = doc.body().ok_or("The chapter has no body.")?;
                let body_node: &web_sys::Node = body.as_ref();
                let sel = doc.get_selection().ok().flatten().filter(|s| s.range_count() > 0).ok_or(NO_CURSOR)?;
                let range = sel.get_range_at(0).map_err(|_| NO_CURSOR)?;
                let kids: Vec<web_sys::Node> = {
                    let list = body.child_nodes();
                    (0..list.length()).filter_map(|i| list.item(i)).collect()
                };
                let is_element = |n: &web_sys::Node| n.node_type() == web_sys::Node::ELEMENT_NODE;
                let elements = kids.iter().filter(|n| is_element(n)).count();
                // The top-level element a point is in, as an index among them; a point between
                // them counts as in the next one.
                let block_of = |node: web_sys::Node, offset: u32| -> usize {
                    let top = if node.is_same_node(Some(body_node)) {
                        kids.get(offset as usize).cloned()
                    } else {
                        let mut n = node;
                        loop {
                            match n.parent_node() {
                                Some(p) if p.is_same_node(Some(body_node)) => break Some(n),
                                Some(p) => n = p,
                                None => break None,
                            }
                        }
                    };
                    let Some(top) = top else { return elements };
                    let at = kids.iter().position(|k| k.is_same_node(Some(&top))).unwrap_or(kids.len());
                    kids[..at].iter().filter(|n| is_element(n)).count()
                };
                let block = if !next && !range.collapsed() {
                    let k = block_of(range.end_container().map_err(|_| NO_CURSOR)?, range.end_offset().unwrap_or(0));
                    (k + 1).min(elements)
                } else {
                    block_of(range.start_container().map_err(|_| NO_CURSOR)?, range.start_offset().unwrap_or(0))
                };
                let names: Vec<String> = kids.iter().filter_map(|n| n.dyn_ref::<web_sys::Element>().map(|e| e.local_name())).collect();
                Ok(json!({ "block": block, "names": names }))
            }
        }
    }

    /// Moves part of the open chapter into the chapter before it or after it (see `cut`).
    fn move_part(self, next: bool) {
        let View::Chapter(path) = self.view.get_untracked() else { return };
        let cut = match self.cut(next) {
            Ok(c) => c,
            Err(e) => return self.say(e, "error"),
        };
        self.act(move || async move {
            let mut args = cut;
            args["id"] = json!(self.id());
            args["path"] = json!(path);
            args["next"] = json!(next);
            let m = call::<Moved>("epub_move", args).await?;
            self.book.set(Some(m.book));
            self.load_chapter(&path).await?;
            let links = match m.links {
                0 => String::new(),
                n => format!(" {} changed to match.", plural(n, "link", "links")),
            };
            let place = if next { "start" } else { "end" };
            self.say(
                format!("Moved {} to the {place} of “{}”.{links} Save to write the book to {}.", plural(m.blocks, "block", "blocks"), m.to, self.place()),
                "ok",
            );
            Ok(())
        });
    }

    fn exec(self, cmd: &str, arg: &str) {
        self.refocus();
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
        let to = if b_mac(ep) { String::new() } else { format!(" to {}", ep.place()) };
        let state = if dirty.get() { "" } else { " (nothing new)" };
        format!("Save{to}{state} ({})", crate::os::keys("⌘S"))
    };
    let copy_title = format!("Save a copy on {}…", crate::os::the_computer());
    // Previous and next go through the chapters; from Book info or Find, next is the first chapter.
    let at = move |v: &View, b: &Option<Book>| match v {
        View::Chapter(p) => b.as_ref().and_then(|b| b.chapters.iter().position(|c| c.path == *p)),
        _ => None,
    };
    let count = move || ep.book.with(|b| b.as_ref().map_or(0, |b| b.chapters.len()));
    let here = move || ep.view.with(|v| ep.book.with(|b| at(v, b)));
    let step = move |d: isize| {
        let to = match ep.view.with_untracked(|v| ep.book.with_untracked(|b| at(v, b))) {
            Some(k) => k as isize + d,
            None if d > 0 => 0,
            None => return,
        };
        let path = ep.book.with_untracked(|b| b.as_ref().and_then(|b| usize::try_from(to).ok().and_then(|k| b.chapters.get(k))).map(|c| c.path.clone()));
        if let Some(p) = path {
            ep.go(View::Chapter(p));
        }
    };
    let no_page = move || !matches!(ep.view.get(), View::Chapter(_));
    // Save's menu, closed by a click anywhere else.
    let save_open = RwSignal::new(false);
    let save_menu = NodeRef::<html::Span>::new();
    let away = window_event_listener(leptos::ev::mousedown, move |e| {
        let inside = save_menu.get_untracked().zip(e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok())).is_some_and(|(m, t)| m.contains(Some(&t)));
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
        let chapters = ep.book.with(|b| b.as_ref().map(|b| b.chapters.clone()).unwrap_or_default());
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
    if s.is_empty() {
        vec![]
    } else {
        s.split('\n').map(String::from).collect()
    }
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
                    // In pairs, a line each: identifier and language, publisher and date.
                    {field(form, "Identifier * (ISBN, UUID…)", false, 1, |m| m.identifier.clone(), |m, v| m.identifier = v)}
                    {field(form, "Language * (en, vi, ja…)", false, 1, |m| m.language.clone(), |m, v| m.language = v)}
                    {field(form, "Publisher", false, 1, |m| m.publisher.clone(), |m, v| m.publisher = v)}
                    {field(form, "Publication date (YYYY-MM-DD)", false, 1, |m| m.date.clone(), |m, v| m.date = v)}
                    {field(form, "Subjects (one per line)", true, 2, |m| lines(&m.subjects), |m, v| m.subjects = split_lines(v))}
                    {field(form, "Description", true, 4, |m| m.description.clone(), |m, v| m.description = v)}
                </div>
                <div class="actions">
                    <span class="spacer"></span>
                    <button type="button" class="btn" title="Discard changes" aria-label="Discard changes" disabled=move || !changed()
                        on:click=move |_| form.set(saved())>{ico(Icon::Restore)}</button>
                    <button type="submit" class="btn primary" title="Apply" aria-label="Apply" disabled=move || !changed() || ep.busy.get()>{ico(Icon::Check)}</button>
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

/// The table of contents, in its tab beside the editor: its entries as a tree (a click opens
/// one; ✕ takes it out), and a form that adds the cursor's place as a new entry.
#[component]
fn ContentsView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let label = RwSignal::new(String::new());
    // As the EPUB nests its contents: beside the entry before it, a level up, or under it.
    let level = RwSignal::new("chapter");
    let in_chapter = move || ep.view.with(|v| matches!(v, View::Chapter(_)));
    let levels: [(&'static str, &'static str, &'static str); 3] = [
        ("section", "Section", "A level up from the entry before it (a part, say, holding what follows)"),
        ("chapter", "Chapter", "At the level of the entry before it"),
        ("subsection", "Subsection", "A level in, under the entry before it"),
    ];
    let add = move || {
        ep.toc_add(label.get_untracked(), level.get_untracked());
        label.set(String::new());
    };
    let entries = move || ep.book.with(|b| b.as_ref().map(|b| b.toc.clone()).unwrap_or_default());
    let here = move |path: &str, frag: &Option<String>| ep.view.with(|v| matches!(v, View::Chapter(p) if p == path)) && frag.is_none();
    view! {
        <div class="toc-view">
            <form class="toc-add" on:submit=move |e| { e.prevent_default(); add(); }>
                <input type="text" class="toc-label" aria-label="Name of the new entry" spellcheck="false"
                    placeholder="New entry: the paragraph's text, or a name"
                    prop:value=move || label.get() on:input=move |e| label.set(event_target_value(&e))
                    on:keydown=|e| e.stop_propagation()/>
                <div class="toc-add-row">
                    <div class="seg toc-levels" role="radiogroup" aria-label="Level">
                        {levels.into_iter().map(|(v, name, tip)| view! {
                            <button type="button" role="radio" class:active=move || level.get() == v aria-checked=move || (level.get() == v).to_string()
                                title=tip on:click=move |_| level.set(v)>{name}</button>
                        }).collect_view()}
                    </div>
                    <button type="submit" class="btn icon-only" disabled=move || !in_chapter()
                        title=move || if in_chapter() { "Add the cursor's place to the contents" } else { "Open a chapter and click where the entry should go" }
                        aria-label="Add to the contents">{ico(Icon::Plus)}</button>
                </div>
            </form>
            {move || {
                let list = entries();
                if list.is_empty() {
                    return view! { <p class="hint">"The contents are empty. Click in a chapter where an entry should go, then Add."</p> }.into_any();
                }
                view! {
                    <ul class="toc-tree" role="tree">
                        {list.into_iter().enumerate().map(|(k, e)| {
                            let (path, frag) = (e.path.clone(), e.fragment.clone());
                            let (p2, f2) = (e.path.clone(), e.fragment.clone());
                            view! {
                                <li role="treeitem" class="toc-entry" style=format!("--depth: {}", e.depth - 1)>
                                    <button class="pdf-head" class:here=move || here(&p2, &f2) title=e.label.clone()
                                        on:click=move |_| ep.open_entry(path.clone(), frag.clone())>
                                        <span class="name">{e.label.clone()}</span>
                                    </button>
                                    <button class="ch-rename" title="Take this entry out of the contents" aria-label="Take out of the contents"
                                        on:click=move |_| ep.toc_remove(k)>{ico(Icon::Close)}</button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </div>
    }
}

/// A chapter in the outline beside the editor: opens it, and its name can be changed there
/// (double-click, or the pencil).
#[component]
fn OutlineChapter(path: String, label: String) -> impl IntoView {
    let ep = expect_context::<Ep>();
    let path = StoredValue::new(path);
    let label = StoredValue::new(label);
    let editing = move || ep.renaming.with(|r| path.with_value(|p| r.as_ref() == Some(p)));
    let here = move || ep.view.with(|v| matches!(v, View::Chapter(p) if path.with_value(|q| q == p)));
    move || {
        if editing() {
            let input = NodeRef::<html::Input>::new();
            input.on_load(move |el| {
                request_animation_frame(move || {
                    let _ = el.focus();
                    el.select();
                });
            });
            let done = move |keep: bool| {
                if !editing() {
                    return;
                }
                match input.get_untracked().filter(|_| keep) {
                    Some(el) => ep.rename_chapter(path.get_value(), el.value()),
                    None => ep.renaming.set(None),
                }
            };
            view! {
                <input class="ch-name" node_ref=input type="text" aria-label="Chapter name" prop:value=label.get_value()
                    on:keydown=move |e| {
                        e.stop_propagation();
                        match e.key().as_str() {
                            "Enter" => done(true),
                            "Escape" => done(false),
                            _ => {}
                        }
                    }
                    on:blur=move |_| done(true)/>
            }
            .into_any()
        } else {
            let name = label.get_value();
            view! {
                <div class="epub-ch" class:here=here>
                    <button class="pdf-head" class:here=here title=name.clone() on:click=move |_| ep.go(View::Chapter(path.get_value()))
                        on:dblclick=move |_| ep.start_rename(path.get_value())>
                        <span class="name">{name.clone()}</span>
                    </button>
                    <button class="ch-rename" title="Rename this chapter" aria-label="Rename this chapter" disabled=move || ep.busy.get()
                        on:click=move |_| ep.start_rename(path.get_value())>{ico(Icon::Rename)}</button>
                </div>
            }
            .into_any()
        }
    }
}

/// Find and replace across the book, in the Search tab beside the page: the find box with its
/// options, Replace with under it (the chevron), and the chapters it's found in below. It
/// searches as you type, once typing stops. Esc goes back to the chapters.
#[component]
fn FindView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let confirm = RwSignal::new(false);
    let replacing = ep.replacing;
    let q = ep.query;
    let find_box = NodeRef::<html::Input>::new();
    let replace_box = NodeRef::<html::Input>::new();
    // On opening, and on ⌘F / ⌘R while open: the cursor in the find (or Replace with) box.
    Effect::new(move |_| {
        let (_, replace) = ep.find_focus.get();
        request_animation_frame(move || {
            let el = if replace { replace_box.get_untracked() } else { None }.or_else(|| find_box.get_untracked());
            if let Some(el) = el {
                let _ = el.focus();
                el.select();
            }
        });
    });
    let empty = move || q.with(|x| x.find.trim().is_empty());
    // An option turned on or off: found again at once.
    let opt = move |label: &'static str, on: fn(&Query) -> bool, flip: fn(&mut Query), glyph: &'static str, class: &'static str| {
        view! {
            <button class="filter-opt" class:on=move || q.with(on) aria-pressed=move || q.with(on).to_string() title=label aria-label=label
                on:mousedown=|e: web_sys::MouseEvent| e.prevent_default()
                on:click=move |_| {
                    q.update(flip);
                    confirm.set(false);
                    ep.find_soon();
                }>
                <span class=class>{glyph}</span>
            </button>
        }
    };
    let count = move || {
        ep.found.with(|f| match f {
            None => String::new(),
            Some(f) if f.total == 0 => "No matches".into(),
            Some(f) => format!("{} in {}", plural(f.total, "match", "matches"), plural(f.hits.len(), "chapter", "chapters")),
        })
    };
    view! {
        <div class="read-find epub-find" on:keydown=move |e| if e.key() == "Escape" { e.prevent_default(); e.stop_propagation(); ep.finding.set(false); }>
            <div class="read-find-box">
                <div class="filter-box find" class:bad=move || ep.found.with(|f| f.as_ref().is_some_and(|f| f.total == 0))>
                    {ico(Icon::Search)}
                    <input class="filter" node_ref=find_box type="text" placeholder=move || if q.with(|x| x.regex) { "Find (regex)" } else { "Find in every chapter" } aria-label="Find" spellcheck="false"
                        prop:value=move || q.with(|x| x.find.clone())
                        on:input=move |e| { q.update(|x| x.find = event_target_value(&e)); confirm.set(false); ep.find_soon(); }
                        on:keydown=move |e| if e.key() == "Enter" { e.prevent_default(); confirm.set(false); ep.find(); }/>
                    {opt("Match case", |x| x.match_case, |x| x.match_case = !x.match_case, "Aa", "")}
                    {opt("Match whole word", |x| x.whole_word, |x| x.whole_word = !x.whole_word, "ab", "word")}
                    {opt("Use regular expression", |x| x.regex, |x| x.regex = !x.regex, ".*", "")}
                </div>
                {move || replacing.get().then(|| view! {
                    <div class="filter-box with">
                        {ico(Icon::Edit)}
                        <input class="filter" node_ref=replace_box type="text" placeholder=move || if q.with(|x| x.regex) { "Replace with ($1 for groups)" } else { "Replace with" } aria-label="Replace with" spellcheck="false"
                            prop:value=move || q.with(|x| x.replace.clone())
                            on:input=move |e| { q.update(|x| x.replace = event_target_value(&e)); confirm.set(false); }
                            on:keydown=move |e| if e.key() == "Enter" && !empty() {
                                e.prevent_default();
                                if e.meta_key() || e.ctrl_key() { confirm.set(true) } else { confirm.set(false); ep.replace_next() }
                            }/>
                        <button class="filter-opt" class:on=move || q.with(|x| x.preserve_case) aria-pressed=move || q.with(|x| x.preserve_case).to_string()
                            title="Preserve case: Foo becomes Bar, FOO becomes BAR" aria-label="Preserve case"
                            on:mousedown=|e: web_sys::MouseEvent| e.prevent_default() on:click=move |_| q.update(|x| x.preserve_case = !x.preserve_case)><span>"AB"</span></button>
                    </div>
                })}
                <div class="find-tools">
                    <button class="btn icon-only chevron" class:open=replacing aria-expanded=move || replacing.get().to_string()
                        title=move || if replacing.get() { "Hide Replace with" } else { "Replace" } aria-label="Replace with"
                        on:mousedown=|e: web_sys::MouseEvent| e.prevent_default() on:click=move |_| replacing.update(|r| *r = !*r)>{ico(Icon::Chevron)}</button>
                    <span class="count">{count}</span>
                    <span class="spacer"></span>
                    {move || replacing.get().then(|| view! {
                        <button class="btn icon-only" title="Replace (Enter): the next match in this chapter, then in the chapters after it" aria-label="Replace"
                            disabled=empty on:click=move |_| { confirm.set(false); ep.replace_next(); }>{ico(Icon::Replace)}</button>
                        <button class="btn icon-only" title=crate::os::keys("Replace all… (⌘Enter)") aria-label="Replace all" disabled=empty
                            on:click=move |_| confirm.set(true)>{ico(Icon::ReplaceAll)}</button>
                    })}
                </div>
            </div>
            {move || confirm.get().then(|| {
                let x = q.get();
                view! {
                    <div class="confirm">
                        <span class="text">{format!("Replace every “{}” with “{}” in all chapters?", x.find, x.replace)}</span>
                        <button class="btn danger solid" on:click=move |_| { confirm.set(false); ep.replace(); }>"Replace all"</button>
                        <button class="btn" on:click=move |_| confirm.set(false)>"Cancel"</button>
                    </div>
                }
            })}
            {move || ep.found.get().map(|f| {
                let skipped = (!f.skipped.is_empty()).then(|| format!("Not searched, because their XHTML is broken: {}", f.skipped.join(", ")));
                view! {
                    {skipped.map(|s| view! { <p class="hint error">{s}</p> })}
                    <ul class="read-results">
                        {f.hits.into_iter().map(|h| {
                            let path = h.path.clone();
                            view! {
                                <li>
                                    <button type="button" on:click=move |_| ep.go(View::Chapter(path.clone()))>
                                        <b>{h.label}</b>" "<span class="kind">{h.count}</span>
                                        {h.snippets.into_iter().take(3).map(|s| view! {
                                            <span class="snip">"…"{s.before}<mark>{s.text}</mark>{s.after}"…"</span>
                                        }).collect_view()}
                                    </button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }
            })}
        </div>
    }
}

type Format = (&'static str, &'static str, &'static str, &'static str);

const INLINE: &[Format] = &[("bold", "", "B", "Bold"), ("italic", "", "I", "Italic")];

/// Headings, paragraph and quote: buttons, or one menu when the bar is narrow.
const BLOCKS: &[Format] = &[
    ("formatBlock", "h1", "H1", "Heading 1"),
    ("formatBlock", "h2", "H2", "Heading 2"),
    ("formatBlock", "h3", "H3", "Heading 3"),
    ("formatBlock", "p", "¶", "Paragraph"),
    ("formatBlock", "blockquote", "“ ”", "Quote"),
];

const MORE: &[Format] = &[
    ("insertUnorderedList", "", "• List", "Bulleted list"),
    ("removeFormat", "", "Clear", "Clear formatting"),
    ("undo", "", "↶", "Undo"),
    ("redo", "", "↷", "Redo"),
];

fn fmt_buttons(ep: Ep, which: &'static [Format]) -> impl IntoView {
    which
        .iter()
        .map(|&(cmd, arg, text, title)| view! { <button title=title aria-label=title on:click=move |_| ep.exec(cmd, arg)>{text}</button> })
        .collect_view()
}

/// The block styles as one button and its menu, shown instead of their buttons when the bar is
/// narrow (styles/epub-editor.css).
#[component]
fn BlockMenu() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let open = RwSignal::new(false);
    let menu = NodeRef::<html::Span>::new();
    let away = window_event_listener(leptos::ev::mousedown, move |e| {
        let inside = menu.get_untracked().zip(e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok())).is_some_and(|(m, t)| m.contains(Some(&t)));
        if !inside {
            open.set(false);
        }
    });
    on_cleanup(move || away.remove());
    view! {
        <span class="fmt-block-menu" node_ref=menu>
            <button class:active=open title="Heading, paragraph or quote" aria-label="Heading, paragraph or quote" aria-haspopup="menu"
                aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)>"H ▾"</button>
            {move || open.get().then(|| view! {
                <span class="fmt-popup" role="menu">
                    {BLOCKS.iter().map(|&(cmd, arg, text, title)| view! {
                        <button role="menuitem" title=title on:click=move |_| {
                            open.set(false);
                            ep.exec(cmd, arg);
                        }>
                            <span class="mark">{text}</span>
                            {title}
                        </button>
                    }).collect_view()}
                </span>
            })}
        </span>
    }
}

#[component]
fn ChapterView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let visual_ok = move || ep.doc.with(|d| d.as_ref().is_some_and(|d| d.html.is_some()));
    // Where this chapter is in reading order, for moving part of it to the one before or after.
    let path = Memo::new(move |_| match ep.view.get() {
        View::Chapter(p) => p,
        View::Info => String::new(),
    });
    let here = Memo::new(move |_| path.with(|path| ep.book.with(|b| b.as_ref().and_then(|b| b.chapters.iter().position(|c| c.path == *path)))));
    // What the page is: none yet, the visual page, a chapter that can't show visually, or its
    // XHTML (drawn anew for each chapter). The visual page stays from chapter to chapter: only
    // what it shows changes.
    let kind = Memo::new(move |_| {
        let mode = ep.mode.get();
        let shown = ep.doc.with(|d| match d {
            None => 0u8,
            Some(d) if d.html.is_some() => 1,
            Some(_) => 2,
        });
        let source = if mode == Mode::Source && shown > 0 { ep.doc.with(|_| path.get()) } else { String::new() };
        (mode, shown, source)
    });
    let count = move || ep.book.with(|b| b.as_ref().map_or(0, |b| b.chapters.len()));
    // The tools after Visual and Source fold into a menu button only when the bar can't hold
    // them: `fit` lays them out in full (`measuring`) to see whether they fit, before the paint.
    let more = RwSignal::new(false);
    let compact = RwSignal::new(false);
    let bar = NodeRef::<html::Div>::new();
    let fit = move || {
        let Some(el) = bar.try_get_untracked().flatten() else { return };
        let classes = el.class_list();
        let _ = classes.add_1("measuring");
        let fits = el.scroll_width() <= el.client_width() + 1;
        let _ = classes.remove_1("measuring");
        if compact.get_untracked() == fits {
            compact.set(!fits);
        }
        if fits && more.get_untracked() {
            more.set(false);
        }
    };
    // When what the bar holds changes (Visual or Source, a chapter that can't show visually).
    Effect::new(move |_| {
        ep.mode.track();
        ep.doc.track();
        request_animation_frame(fit);
    });
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(node) = bar.get() else { return };
        let cb = Closure::<dyn Fn()>::new(fit);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&node);
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
    // When the preview moves to another file, the frame goes with it: its edits are kept to send later.
    on_cleanup(move || {
        if ep.mode.get_untracked() == Mode::Visual && ep.chapter_dirty.get_untracked() {
            if let Ok(b) = ep.visual_body() {
                ep.kept.set_value(Some(b));
            }
        }
    });
    view! {
        // mousedown is kept from the bar, so clicking it leaves the focus and the cursor in the page.
        <div class="viewer-bar tools" class:compact=compact node_ref=bar on:mousedown=|e| e.prevent_default()>
            <div class="seg">
                <button class:active=move || ep.mode.get() == Mode::Visual title="Visual" aria-label="Visual"
                    on:click=move |_| ep.set_mode(Mode::Visual)>{ico(Icon::Eye)}</button>
                <button class:active=move || ep.mode.get() == Mode::Source title="Source" aria-label="Source"
                    on:click=move |_| ep.set_mode(Mode::Source)>{ico(Icon::Code)}</button>
            </div>
            <button class="btn tools-more" class:active=more title="Editing tools" aria-label="Editing tools"
                aria-expanded=move || more.get().to_string() on:click=move |_| more.update(|m| *m = !*m)>{ico(Icon::Menu)}</button>
            <div class="tools-rest" class:open=more>
            <span class="btn-group" role="group">
                <button class="btn" title="Move to the end of the previous chapter: what's before the cursor's paragraph, or up to the end of the selection"
                    aria-label="Move to the previous chapter" disabled=move || here.get().is_none_or(|k| k == 0)
                    on:mousedown=|e| e.prevent_default() on:click=move |_| ep.move_part(false)>{ico(Icon::ToPrevPart)}</button>
                <button class="btn" title="Move to the start of the next chapter: the cursor's paragraph (or the selection's first) and what follows"
                    aria-label="Move to the next chapter" disabled=move || here.get().is_none_or(|k| k + 1 >= count())
                    on:mousedown=|e| e.prevent_default() on:click=move |_| ep.move_part(true)>{ico(Icon::ToNextPart)}</button>
            </span>
            {move || (ep.mode.get() == Mode::Source).then(|| view! {
                <button class="btn" class:active=move || ep.colours.get() title="Colours" aria-label="Colours"
                    on:click=move |_| {
                        let on = !ep.colours.get_untracked();
                        ep.colours.set(on);
                        crate::app::store(COLOURS_KEY, if on { "1" } else { "0" });
                    }>{ico(Icon::Highlight)}</button>
            })}
            {move || (ep.mode.get() == Mode::Visual && visual_ok()).then(|| view! {
                <div class="fmt">
                    {fmt_buttons(ep, INLINE)}
                    <span class="fmt-blocks">{fmt_buttons(ep, BLOCKS)}</span>
                    <BlockMenu/>
                    {fmt_buttons(ep, MORE)}
                </div>
            })}
            </div>
        </div>
        {move || match kind.get() {
            (_, 0, _) => view! { <div class="empty"><span class="spin"></span></div> }.into_any(),
            (Mode::Visual, 1, _) => {
                let page = move || ep.doc.with(|d| d.as_ref().and_then(|d| d.html.as_deref()).map(|h| crate::app::editor_page(h, ep.zoom.get_untracked())).unwrap_or_default());
                view! {
                    // No allow-scripts: a book's scripts must never run next to the app's commands.
                    <iframe class="page" sandbox="allow-same-origin" node_ref=ep.frame srcdoc=page
                        on:load=move |_| ep.frame_loaded()></iframe>
                }.into_any()
            }
            (Mode::Visual, _, _) => view! {
                <div class="notice error">
                    <span class="text">"This chapter isn't well-formed XHTML, so it can't be shown visually. "
                        {move || ep.doc.with(|d| d.as_ref().and_then(|d| d.problem.clone()).unwrap_or_default())}</span>
                    <button class="btn" on:click=move |_| ep.set_mode(Mode::Source)>"Fix it in Source"</button>
                </div>
            }.into_any(),
            (Mode::Source, _, path) => view! { <SourceEdit path/> }.into_any(),
        }}
    }
}

/// The chapter's XHTML: a clear-lettered box over a layer that draws it in colour (as the text
/// editor does), or the plain box when colours are off or the chapter is big.
#[component]
fn SourceEdit(path: String) -> impl IntoView {
    let ep = expect_context::<Ep>();
    let layer = NodeRef::<html::Pre>::new();
    let ta = ep.source_box;
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
        <div class="code-edit source" class:layered=layered style=move || format!("--zoom: {}", ep.zoom.get() as f64 / 100.0)>
            <pre class="code-layer" node_ref=layer aria-hidden="true"></pre>
            <textarea class="code-input" node_ref=ta spellcheck="false" autocomplete="off" autocapitalize="off"
                aria-label="Chapter source" prop:value=move || ep.source.get()
                on:input=move |ev| { ep.source.set(event_target_value(&ev)); ep.chapter_dirty.set(true); }
                on:scroll=scrolled></textarea>
        </div>
    }
}
