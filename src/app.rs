//! The file browser UI: connect screen, folder listing (list, thumbnail and column views, with a
//! context menu), and a text editor.

use crate::api::{call, Devices, Entry, Info, TextFile, WriteResult};
use leptos::ev;
use leptos::html;
use std::collections::HashMap;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use wasm_bindgen::JsCast;

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "json", "xml", "html", "htm", "xhtml", "css", "js", "csv", "tsv", "log", "ini",
    "conf", "cfg", "yaml", "yml", "toml", "properties", "opf", "ncx", "svg", "srt", "vtt", "sh", "py", "kt",
    "java", "gradle", "env",
];
const TEXT_MAX: u64 = 5 * 1024 * 1024;

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Folder,
    Epub,
    Text,
    Other,
}

fn kind(e: &Entry) -> Kind {
    let x = ext(&e.name);
    if e.dir {
        Kind::Folder
    } else if x == "epub" {
        Kind::Epub
    } else if TEXT_EXTS.contains(&x.as_str()) && e.size <= TEXT_MAX {
        Kind::Text
    } else {
        Kind::Other
    }
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

fn when(mtime: i64) -> String {
    if mtime <= 0 {
        return String::new();
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(mtime as f64 * 1000.0));
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes()
    )
}

fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

fn icon(k: Kind) -> &'static str {
    match k {
        Kind::Folder => "📁",
        Kind::Epub => "📘",
        Kind::Text => "📝",
        Kind::Other => "📄",
    }
}

const THUMB_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "epub"];
/// Same limit as the backend (src-tauri/src/thumbs.rs): bigger files aren't read for a thumbnail.
const THUMB_MAX: u64 = 60 * 1024 * 1024;

/// Thumbnail URL for images and EPUBs (their cover), served by the backend's `yomi://` scheme.
fn thumb_url(e: &Entry) -> Option<String> {
    (!e.dir && e.size <= THUMB_MAX && THUMB_EXTS.contains(&ext(&e.name).as_str())).then(|| {
        let p = String::from(js_sys::encode_uri_component(&e.path));
        format!("yomi://localhost/thumb?p={p}&s={}&m={}", e.size, e.mtime)
    })
}

/// Every folder from "/" down to `path`, e.g. "/a/b" gives ["/", "/a", "/a/b"].
fn ancestors(path: &str) -> Vec<String> {
    let mut out = vec!["/".to_string()];
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        acc = format!("{acc}/{part}");
        out.push(acc.clone());
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    List,
    Thumbs,
    Columns,
}

const VIEW_KEY: &str = "yomi-files.view";

impl ViewMode {
    const ALL: [ViewMode; 3] = [ViewMode::List, ViewMode::Thumbs, ViewMode::Columns];

    fn key(self) -> &'static str {
        match self {
            ViewMode::List => "list",
            ViewMode::Thumbs => "thumbs",
            ViewMode::Columns => "columns",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ViewMode::List => "List",
            ViewMode::Thumbs => "Thumbnails",
            ViewMode::Columns => "Columns",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            ViewMode::List => "☰",
            ViewMode::Thumbs => "▦",
            ViewMode::Columns => "▥",
        }
    }

    /// The view chosen last time. Storage can be missing or blocked; then it's the list.
    fn stored() -> Self {
        let saved = window().local_storage().ok().flatten().and_then(|s| s.get_item(VIEW_KEY).ok().flatten());
        Self::ALL.into_iter().find(|v| saved.as_deref() == Some(v.key())).unwrap_or(ViewMode::List)
    }
}

/// An open context menu: where it is, and the file or folder it's for (None: the folder's background).
#[derive(Clone, PartialEq)]
struct Menu {
    x: f64,
    y: f64,
    entry: Option<Entry>,
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Open,
    Download,
    Delete,
    NewFolder,
    NewFile,
    Upload,
    Refresh,
    View(ViewMode),
}

enum MenuItem {
    Do { label: String, key: &'static str, act: Act, danger: bool, enabled: bool },
    Sep,
}

fn item(label: impl Into<String>, key: &'static str, act: Act) -> MenuItem {
    MenuItem::Do { label: label.into(), key, act, danger: false, enabled: true }
}

#[derive(Clone, PartialEq)]
enum Notice {
    None,
    Error(String),
    Ok(String),
    ConfirmDelete(Entry),
    /// Asking for a name; `folder` says whether it's a new folder or a new text file.
    NewItem { folder: bool },
}

#[derive(Clone, PartialEq)]
struct Editor {
    path: String,
    /// Version of the device copy we started from; saving refuses if the device copy differs.
    version: String,
    saved_text: String,
}

#[derive(Clone, Copy)]
struct Ctx {
    info: RwSignal<Option<Info>>,
    cwd: RwSignal<String>,
    entries: RwSignal<Vec<Entry>>,
    loading: RwSignal<bool>,
    selected: RwSignal<Option<String>>,
    notice: RwSignal<Notice>,
    editor: RwSignal<Option<Editor>>,
    text: RwSignal<String>,
    status: RwSignal<(String, &'static str)>,
    confirm_discard: RwSignal<bool>,
    busy: RwSignal<bool>,
    ep: crate::epub::Ep,
    view: RwSignal<ViewMode>,
    menu: RwSignal<Option<Menu>>,
    /// Listings of other folders, for the column view's columns to the left of the current one.
    cols: RwSignal<HashMap<String, Vec<Entry>>>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            info: RwSignal::new(None),
            cwd: RwSignal::new(String::new()),
            entries: RwSignal::new(vec![]),
            loading: RwSignal::new(false),
            selected: RwSignal::new(None),
            notice: RwSignal::new(Notice::None),
            editor: RwSignal::new(None),
            text: RwSignal::new(String::new()),
            status: RwSignal::new((String::new(), "")),
            confirm_discard: RwSignal::new(false),
            busy: RwSignal::new(false),
            ep: crate::epub::Ep::new(),
            view: RwSignal::new(ViewMode::stored()),
            menu: RwSignal::new(None),
            cols: RwSignal::new(HashMap::new()),
        }
    }

    fn dirty(&self) -> bool {
        self.editor.with(|e| e.as_ref().is_some_and(|e| e.saved_text != self.text.get()))
    }

    fn error(&self, msg: impl Into<String>) {
        self.notice.set(Notice::Error(msg.into()));
    }

    /// Lists `path` and makes it the current folder, with `select` selected. Keeps the old listing if it fails.
    fn load_select(self, path: String, select: Option<String>) {
        self.loading.set(true);
        spawn_local(async move {
            match call::<Vec<Entry>>("list", json!({ "path": path })).await {
                Ok(entries) => {
                    self.cols.update(|c| {
                        c.insert(path.clone(), entries.clone());
                    });
                    self.cwd.set(path);
                    self.entries.set(entries);
                    self.selected.set(select);
                    if matches!(self.notice.get_untracked(), Notice::Error(_)) {
                        self.notice.set(Notice::None);
                    }
                }
                Err(e) => self.error(e),
            }
            self.loading.set(false);
        });
    }

    fn load(self, path: String) {
        self.load_select(path, None);
    }

    /// Lists the current folder again, keeping the selection. Other folders' listings are dropped too.
    fn reload(self) {
        self.cols.set(HashMap::new());
        self.load_select(self.cwd.get_untracked(), self.selected.get_untracked());
    }

    /// Asks the device to re-read its folders (MTP caches them), then lists the folder again.
    fn refresh(self) {
        spawn_local(async move {
            let _ = call::<()>("refresh", json!({})).await;
            self.reload();
        });
    }

    fn navigate(self, path: String) {
        self.navigate_select(path, None);
    }

    /// Up one folder, with the folder we were in selected.
    fn go_up(self) {
        let cwd = self.cwd.get_untracked();
        if cwd != "/" {
            self.navigate_select(parent(&cwd), Some(cwd));
        }
    }

    fn set_view(self, v: ViewMode) {
        self.view.set(v);
        if let Ok(Some(s)) = window().local_storage() {
            let _ = s.set_item(VIEW_KEY, v.key());
        }
    }

    /// New files and folders can't go in "/" over MTP: it lists the phone's storages.
    fn in_storage(self) -> bool {
        self.cwd.get() != "/" || self.info.get().is_some_and(|i| i.transport == "adb")
    }

    fn show_menu(self, ev: &web_sys::MouseEvent, entry: Option<Entry>) {
        ev.prevent_default();
        ev.stop_propagation();
        if let Some(e) = &entry {
            self.selected.set(Some(e.path.clone()));
        }
        // Keep the whole menu on screen: estimate its size from the number of rows.
        let rows = self.menu_items(entry.as_ref()).len() as f64;
        let (w, h) = (230.0, rows * 28.0 + 12.0);
        let size = |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
        let (vw, vh) = (size(window().inner_width()), size(window().inner_height()));
        let x = (ev.client_x() as f64).min(vw - w - 4.0).max(4.0);
        let y = (ev.client_y() as f64).min(vh - h - 4.0).max(4.0);
        self.menu.set(Some(Menu { x, y, entry }));
    }

    fn menu_items(self, entry: Option<&Entry>) -> Vec<MenuItem> {
        let Some(e) = entry else {
            let here = self.in_storage();
            let mut v = vec![
                MenuItem::Do { label: "New folder…".into(), key: "", act: Act::NewFolder, danger: false, enabled: here },
                MenuItem::Do { label: "New text file…".into(), key: "", act: Act::NewFile, danger: false, enabled: here },
                MenuItem::Do { label: "Upload files…".into(), key: "", act: Act::Upload, danger: false, enabled: here },
                MenuItem::Sep,
                item("Refresh", "", Act::Refresh),
                MenuItem::Sep,
            ];
            let now = self.view.get_untracked();
            v.extend(ViewMode::ALL.map(|m| {
                item(format!("{} as {}", if m == now { "✓" } else { "\u{2003}" }, m.label()), "", Act::View(m))
            }));
            return v;
        };
        let mut v = vec![];
        let open = match kind(e) {
            Kind::Folder => Some("Open"),
            Kind::Epub => Some("Open in EPUB editor"),
            Kind::Text => Some("Edit"),
            Kind::Other => None,
        };
        if let Some(label) = open {
            v.push(item(label, "↵", Act::Open));
        }
        if !e.dir {
            v.push(item("Download to Mac…", if open.is_none() { "↵" } else { "" }, Act::Download));
        }
        v.push(MenuItem::Sep);
        let label = if e.dir { "Delete empty folder…" } else { "Delete…" };
        v.push(MenuItem::Do { label: label.into(), key: "⌘⌫", act: Act::Delete, danger: true, enabled: true });
        v
    }

    fn run(self, act: Act, entry: Option<Entry>) {
        match (act, entry) {
            (Act::Open, Some(e)) => self.open(e),
            (Act::Download, Some(e)) => self.download(e.path),
            (Act::Delete, Some(e)) => self.notice.set(Notice::ConfirmDelete(e)),
            (Act::NewFolder, _) => self.notice.set(Notice::NewItem { folder: true }),
            (Act::NewFile, _) => self.notice.set(Notice::NewItem { folder: false }),
            (Act::Upload, _) => self.upload(),
            (Act::Refresh, _) => self.refresh(),
            (Act::View(v), _) => self.set_view(v),
            _ => {}
        }
    }

    /// Navigation that would leave the editor asks first when there are unsaved changes.
    fn navigate_select(self, path: String, select: Option<String>) {
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return;
            }
            self.editor.set(None);
        }
        self.load_select(path, select);
    }

    fn open(self, e: Entry) {
        match kind(&e) {
            Kind::Folder => self.navigate(e.path),
            Kind::Epub => {
                self.loading.set(true);
                let ep = self.ep;
                spawn_local(async move {
                    if let Err(err) = ep.open(e.path).await {
                        self.error(err);
                    }
                    self.loading.set(false);
                });
            }
            Kind::Text => self.open_text(e.path),
            Kind::Other => self.download(e.path),
        }
    }

    fn open_text(self, path: String) {
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "path": path })).await {
                Ok(f) => {
                    self.text.set(f.text.clone());
                    self.editor.set(Some(Editor { path, version: f.version, saved_text: f.text }));
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                }
                Err(e) => self.error(e),
            }
            self.busy.set(false);
        });
    }

    fn save(self) {
        let Some(ed) = self.editor.get_untracked() else { return };
        if self.busy.get_untracked() {
            return;
        }
        let text = self.text.get_untracked();
        self.busy.set(true);
        self.status.set(("Saving…".into(), ""));
        spawn_local(async move {
            let args = json!({ "path": ed.path, "text": text, "base_version": ed.version, "create": false });
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    self.editor.set(Some(Editor { version: r.version, saved_text: text, ..ed }));
                    self.confirm_discard.set(false);
                    let note = if r.backup.is_some() { " The old version is backed up on the Mac." } else { "" };
                    self.status.set((format!("Saved to the device.{note}"), "ok"));
                }
                Err(e) => self.status.set((e, "error")),
            }
            self.busy.set(false);
        });
    }

    fn close_editor(self, force: bool) {
        if !force && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        self.editor.set(None);
        self.confirm_discard.set(false);
        self.reload();
    }

    fn download(self, path: String) {
        spawn_local(async move {
            match call::<Option<String>>("download", json!({ "path": path })).await {
                Ok(Some(dest)) => self.notice.set(Notice::Ok(format!("Saved to {dest}"))),
                Ok(None) => {}
                Err(e) => self.error(e),
            }
        });
    }

    fn upload(self) {
        let dir = self.cwd.get_untracked();
        self.busy.set(true);
        spawn_local(async move {
            match call::<Vec<String>>("upload", json!({ "dir": dir })).await {
                Ok(lines) if lines.is_empty() => {}
                Ok(lines) => {
                    self.notice.set(Notice::Ok(lines.join("\n")));
                    self.reload();
                }
                Err(e) => self.error(e),
            }
            self.busy.set(false);
        });
    }

    fn delete(self, e: Entry) {
        spawn_local(async move {
            match call::<Option<String>>("remove", json!({ "path": e.path })).await {
                Ok(backup) => {
                    let note = backup.map(|b| format!(" Backup: {b}")).unwrap_or_default();
                    self.notice.set(Notice::Ok(format!("Deleted {}.{note}", e.name)));
                    self.reload();
                }
                Err(err) => self.error(err),
            }
        });
    }

    fn create(self, name: String, folder: bool) {
        let name = name.trim().to_string();
        if name.is_empty() || name.contains('/') {
            self.error("Enter a name without /.");
            return;
        }
        let path = join(&self.cwd.get_untracked(), &name);
        spawn_local(async move {
            let result = if folder {
                call::<()>("mkdir", json!({ "path": path })).await
            } else {
                call::<WriteResult>("write_text", json!({ "path": path, "text": "", "base_version": null, "create": true }))
                    .await
                    .map(|_| ())
            };
            match result {
                Ok(()) => {
                    self.notice.set(Notice::None);
                    if folder { self.reload() } else { self.open_text(path) }
                }
                Err(e) => self.error(e),
            }
        });
    }

    fn connect(self, transport: &'static str, conn_error: RwSignal<Option<String>>) {
        self.busy.set(true);
        conn_error.set(None);
        spawn_local(async move {
            match call::<Info>("connect", json!({ "transport": transport, "serial": null })).await {
                Ok(info) => {
                    let start = info.default_dir.clone();
                    self.info.set(Some(info));
                    self.load(start);
                }
                Err(e) => conn_error.set(Some(e)),
            }
            self.busy.set(false);
        });
    }

    fn disconnect(self) {
        if self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        spawn_local(async move {
            let _ = call::<()>("disconnect", json!({})).await;
            self.info.set(None);
            self.editor.set(None);
            self.entries.set(vec![]);
            self.notice.set(Notice::None);
        });
    }
}

#[component]
pub fn App() -> impl IntoView {
    let ctx = Ctx::new();
    provide_context(ctx);
    let ep = ctx.ep;
    provide_context(ep);
    // Closing a book goes back to the listing, which may have changed (a save writes the file and its backup).
    Effect::new(move |was_open: Option<bool>| {
        let open = ep.book.with(|b| b.is_some());
        if was_open == Some(true) && !open {
            ctx.reload();
        }
        open
    });

    // Pick up an existing connection after a webview reload.
    spawn_local(async move {
        if let Ok(Some(info)) = call::<Option<Info>>("info", json!({})).await {
            let start = info.default_dir.clone();
            ctx.info.set(Some(info));
            ctx.load(start);
        }
    });

    // Keyboard. ⌘S saves. In the listing: arrows move (←/→ also go up/in in the column view and
    // move across in the thumbnail view), Enter or ⌘↓ opens, ⌘↑ or Backspace goes up, ⌘⌫ deletes.
    let _keys = window_event_listener(ev::keydown, move |e| {
        let meta = e.meta_key() || e.ctrl_key();
        if ctx.menu.get_untracked().is_some() {
            if e.key() == "Escape" {
                e.prevent_default();
            }
            ctx.menu.set(None);
            if e.key() == "Escape" {
                return;
            }
        }
        if ep.book.with_untracked(|b| b.is_some()) {
            if meta && e.key() == "s" {
                e.prevent_default();
                ep.save();
            }
            return;
        }
        if ctx.editor.get_untracked().is_some() {
            if meta && e.key() == "s" {
                e.prevent_default();
                ctx.save();
            }
            return;
        }
        if ctx.info.get_untracked().is_none() || !matches!(ctx.notice.get_untracked(), Notice::None | Notice::Ok(_) | Notice::Error(_)) {
            return;
        }
        let entries = ctx.entries.get_untracked();
        let idx = ctx.selected.get_untracked().and_then(|p| entries.iter().position(|x| x.path == p));
        let current = idx.and_then(|i| entries.get(i)).cloned();
        let view = ctx.view.get_untracked();
        let step = |delta: isize| {
            let last = entries.len() as isize - 1;
            let next = match idx {
                None => 0,
                Some(i) => (i as isize + delta).clamp(0, last.max(0)),
            };
            if let Some(x) = entries.get(next as usize) {
                ctx.selected.set(Some(x.path.clone()));
            }
        };
        // Tiles per row in the thumbnail view, from the grid's width (tiles are 128px plus an 8px gap).
        let per_row = || {
            document()
                .query_selector(".thumbs")
                .ok()
                .flatten()
                .map_or(1, |g| ((g.client_width() - 24 + 8) / 136).max(1) as isize)
        };
        match (e.key().as_str(), meta) {
            ("Backspace", true) => {
                if let Some(x) = current {
                    e.prevent_default();
                    ctx.notice.set(Notice::ConfirmDelete(x));
                }
            }
            ("ArrowDown", false) => {
                e.prevent_default();
                step(if view == ViewMode::Thumbs && idx.is_some() { per_row() } else { 1 });
            }
            ("ArrowUp", false) => {
                e.prevent_default();
                step(if view == ViewMode::Thumbs { -per_row() } else { -1 });
            }
            ("ArrowRight", false) if view == ViewMode::Thumbs => {
                e.prevent_default();
                step(1);
            }
            ("ArrowLeft", false) if view == ViewMode::Thumbs => {
                e.prevent_default();
                step(-1);
            }
            ("ArrowRight", false) if view == ViewMode::Columns => {
                if let Some(x) = current.filter(|x| x.dir) {
                    e.prevent_default();
                    ctx.navigate(x.path);
                }
            }
            ("ArrowLeft", false) if view == ViewMode::Columns => {
                e.prevent_default();
                ctx.go_up();
            }
            ("Enter", _) | ("ArrowDown", true) => {
                if let Some(x) = current {
                    e.prevent_default();
                    ctx.open(x);
                }
            }
            ("Backspace", false) | ("ArrowUp", true) => {
                e.prevent_default();
                ctx.go_up();
            }
            _ => {}
        }
    });

    // The context menu closes on any click outside it, and when the window loses focus.
    let _press = window_event_listener(ev::mousedown, move |e| {
        let inside = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest(".menu").ok().flatten().is_some());
        if !inside {
            ctx.menu.set(None);
        }
    });
    let _blur = window_event_listener(ev::blur, move |_| ctx.menu.set(None));
    let _resize = window_event_listener(ev::resize, move |_| ctx.menu.set(None));
    // No browser menu (Reload, Inspect…) outside text fields; the listing has its own.
    let _ctx_menu = window_event_listener(ev::contextmenu, move |e| {
        let editable = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest("input, textarea").ok().flatten().is_some());
        if !editable {
            e.prevent_default();
        }
    });

    // Keep the selected item visible when the keyboard moves the selection.
    Effect::new(move |_| {
        ctx.selected.track();
        ctx.view.track();
        request_animation_frame(|| {
            if let Ok(Some(el)) = document().query_selector(".selected") {
                let opts = web_sys::ScrollIntoViewOptions::new();
                opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                opts.set_inline(web_sys::ScrollLogicalPosition::Nearest);
                el.scroll_into_view_with_scroll_into_view_options(&opts);
            }
        });
    });

    view! {
        {move || if ctx.info.get().is_some() { view! { <Browser/> }.into_any() } else { view! { <Connect/> }.into_any() }}
        <ContextMenu/>
    }
}

#[component]
fn Connect() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let devices = RwSignal::new(None::<Devices>);
    let conn_error = RwSignal::new(None::<String>);
    let scan = move || {
        devices.set(None);
        spawn_local(async move {
            match call::<Devices>("devices", json!({})).await {
                Ok(d) => devices.set(Some(d)),
                Err(e) => conn_error.set(Some(e)),
            }
        });
    };
    scan();

    view! {
        <div class="connect">
            <h1>"yomi-files"</h1>
            <p>"Browse, read and edit files on an Android phone connected by USB."</p>
            <dl>
                <dt>"adb"</dt>
                <dd>{move || match devices.get() {
                    None => "Looking…".to_string(),
                    Some(d) if !d.adb_found => "adb isn't installed (brew install --cask android-platform-tools).".into(),
                    Some(d) if d.adb.is_empty() => "No device. Needs USB debugging, authorised on the phone.".into(),
                    Some(d) => d.adb.join(", "),
                }}</dd>
                <dt>"MTP"</dt>
                <dd>{move || match devices.get() {
                    None => "Looking…".to_string(),
                    Some(Devices { mtp_error: Some(e), .. }) => e,
                    Some(d) if d.mtp.is_empty() => "No device. Set the phone's USB mode to \"File transfer\".".into(),
                    Some(d) => d.mtp.join(", "),
                }}</dd>
            </dl>
            <div class="row">
                <button class="btn primary" disabled=move || ctx.busy.get() on:click=move |_| ctx.connect("auto", conn_error)>
                    {move || if ctx.busy.get() { view! { <span class="spin"></span>" Connecting…" }.into_any() } else { "Connect".into_any() }}
                </button>
                <button class="btn" disabled=move || ctx.busy.get() on:click=move |_| ctx.connect("adb", conn_error)>"Use adb"</button>
                <button class="btn" disabled=move || ctx.busy.get() on:click=move |_| ctx.connect("mtp", conn_error)>"Use MTP"</button>
                <span class="spacer"></span>
                <button class="btn" on:click=move |_| scan()>"Scan again"</button>
            </div>
            {move || conn_error.get().map(|e| view! { <p class="error">{e}</p> })}
            <p>"Connect uses adb when a device is ready, otherwise MTP. Over MTP, close Android File Transfer and yomi-files.py first: only one app can use the phone at a time."</p>
        </div>
    }
}

#[component]
fn Browser() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let ep = ctx.ep;
    move || if ep.book.with(|b| b.is_some()) { view! { <crate::epub::EpubEditor/> }.into_any() } else { view! {
        <div class="app">
            <Sidebar/>
            <div class="main">
                {move || if ctx.editor.get().is_some() { view! { <TextEditor/> }.into_any() } else { view! { <Listing/> }.into_any() }}
            </div>
        </div>
    }.into_any() }
}

#[component]
fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let info = move || ctx.info.get().unwrap_or_else(|| Info {
        label: String::new(), transport: String::new(), default_dir: "/".into(), quick_dirs: vec![], backup_dir: String::new(),
    });
    view! {
        <nav class="side">
            <div class="device">
                <b>{move || info().label}</b>
                <span class="kind">{move || info().transport}</span>
            </div>
            <h2>"Places"</h2>
            {move || info().quick_dirs.into_iter().map(|(name, path)| {
                let p = path.clone();
                let active = move || ctx.cwd.get() == p;
                view! {
                    <button class="item" class:active=active on:click=move |_| ctx.navigate(path.clone())>{name}</button>
                }
            }).collect_view()}
            <button class="item" class:active=move || ctx.cwd.get() == "/" on:click=move |_| ctx.navigate("/".into())>"/ (all storage)"</button>
            <span class="grow"></span>
            <button class="btn" on:click=move |_| ctx.disconnect()>"Disconnect"</button>
            <div class="foot">"Backups of replaced and deleted files: " {move || info().backup_dir}</div>
        </nav>
    }
}

#[component]
fn Crumbs() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="crumbs">
            {move || {
                let cwd = ctx.cwd.get();
                let mut acc = String::new();
                let mut out = vec![view! { <button on:click=move |_| ctx.navigate("/".into())>"/"</button> }.into_any()];
                for part in cwd.split('/').filter(|p| !p.is_empty()) {
                    acc = format!("{acc}/{part}");
                    let target = acc.clone();
                    out.push(view! {
                        <button on:click=move |_| ctx.navigate(target.clone())>{part.to_string()}</button>
                    }.into_any());
                    out.push(view! { <span class="sep">"/"</span> }.into_any());
                }
                out.pop_if(|_| cwd != "/");
                out
            }}
        </div>
    }
}

#[component]
fn Notices() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let name = RwSignal::new(String::new());
    view! {
        {move || match ctx.notice.get() {
            Notice::None => ().into_any(),
            Notice::Error(m) => view! {
                <div class="notice error"><span class="text">{m}</span>
                    <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Dismiss"</button></div>
            }.into_any(),
            Notice::Ok(m) => view! {
                <div class="notice ok"><span class="text">{m}</span>
                    <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Dismiss"</button></div>
            }.into_any(),
            Notice::ConfirmDelete(e) => {
                let what = if e.dir { "the empty folder" } else { "" };
                let label = format!("Delete {what} {} from the device?{}", e.name,
                    if e.dir { "" } else { " A backup is kept on the Mac." });
                view! {
                    <div class="notice"><span class="text">{label}</span>
                        <button class="btn danger solid" on:click=move |_| ctx.delete(e.clone())>"Delete"</button>
                        <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Cancel"</button></div>
                }.into_any()
            }
            Notice::NewItem { folder } => {
                name.set(String::new());
                let submit = move || ctx.create(name.get_untracked(), folder);
                view! {
                    <div class="notice">
                        <span>{if folder { "New folder:" } else { "New text file:" }}</span>
                        <input autofocus placeholder=if folder { "Folder name" } else { "notes.txt" }
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => submit(),
                                "Escape" => ctx.notice.set(Notice::None),
                                _ => {}
                            }/>
                        <button class="btn primary" on:click=move |_| submit()>"Create"</button>
                        <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Cancel"</button>
                    </div>
                }.into_any()
            }
        }}
    }
}

#[component]
fn Listing() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let here = move || ctx.in_storage();
    view! {
        <div class="bar">
            <button class="btn" title="Up (⌘↑)" disabled=move || ctx.cwd.get() == "/" on:click=move |_| ctx.go_up()>"↑"</button>
            <Crumbs/>
            {move || ctx.loading.get().then(|| view! { <span class="spin"></span> })}
            <div class="seg" role="group" aria-label="View">
                {ViewMode::ALL.map(|m| view! {
                    <button title=format!("View as {}", m.label()) aria-label=m.label()
                        class:active=move || ctx.view.get() == m on:click=move |_| ctx.set_view(m)>{m.glyph()}</button>
                })}
            </div>
            <button class="btn" title="Re-read the folder from the device" on:click=move |_| ctx.refresh()>"Refresh"</button>
            <button class="btn" disabled=move || !here() on:click=move |_| ctx.notice.set(Notice::NewItem { folder: true })>"New folder"</button>
            <button class="btn" disabled=move || !here() on:click=move |_| ctx.notice.set(Notice::NewItem { folder: false })>"New file"</button>
            <button class="btn" disabled=move || !here() || ctx.busy.get() on:click=move |_| ctx.upload()>"Upload…"</button>
        </div>
        <Notices/>
        {move || match ctx.view.get() {
            ViewMode::List => view! { <ListView/> }.into_any(),
            ViewMode::Thumbs => view! { <ThumbView/> }.into_any(),
            ViewMode::Columns => view! { <ColumnView/> }.into_any(),
        }}
        <ListStatus/>
    }
}

/// Selects on click, opens on double-click, and shows the context menu on right-click.
fn row_events(ctx: Ctx, e: &Entry) -> (impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static) {
    let (p, e1, e2) = (e.path.clone(), e.clone(), e.clone());
    (
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            ctx.selected.set(Some(p.clone()));
        },
        move |_| ctx.open(e1.clone()),
        move |ev: web_sys::MouseEvent| ctx.show_menu(&ev, Some(e2.clone())),
    )
}

fn empty_note(ctx: Ctx) -> impl IntoView {
    move || (ctx.entries.with(|e| e.is_empty()) && !ctx.loading.get()).then(|| view! {
        <div class="empty">"This folder is empty."</div>
    })
}

#[component]
fn ListView() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="list" on:click=move |_| ctx.selected.set(None) on:contextmenu=move |ev| ctx.show_menu(&ev, None)>
            <table>
                <thead><tr><th>"Name"</th><th class="num">"Modified"</th><th class="num">"Size"</th></tr></thead>
                <tbody>
                    <For each=move || ctx.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                        {
                            let k = kind(&e);
                            let p = e.path.clone();
                            let (click, dbl, menu) = row_events(ctx, &e);
                            let tag = match k { Kind::Epub => "EPUB", Kind::Text => "text", _ => "" };
                            view! {
                                <tr class:selected=move || ctx.selected.get().as_deref() == Some(p.as_str())
                                    on:click=click on:dblclick=dbl on:contextmenu=menu>
                                    <td class="name"><span class="icon">{icon(k)}</span>{e.name.clone()}
                                        {(!tag.is_empty()).then(|| view! { <span class="kind">{tag}</span> })}</td>
                                    <td class="num">{when(e.mtime)}</td>
                                    <td class="num">{if e.dir { String::new() } else { human_size(e.size) }}</td>
                                </tr>
                            }
                        }
                    </For>
                </tbody>
            </table>
            {empty_note(ctx)}
        </div>
    }
}

/// The file's icon, replaced by its thumbnail (an image, or an EPUB's cover) once that has loaded.
/// Files with no thumbnail, or whose thumbnail fails, keep the icon.
#[component]
fn Thumbnail(entry: Entry) -> impl IntoView {
    let state = RwSignal::new(0u8); // 0 loading, 1 loaded, 2 failed
    let glyph = icon(kind(&entry));
    let url = thumb_url(&entry);
    view! {
        {move || (state.get() != 1).then(|| view! { <span class="glyph">{glyph}</span> })}
        {url.map(|url| view! {
            // Kept in the layout (just invisible) while loading, so lazy loading still starts it.
            <img src=url loading="lazy" decoding="async" draggable="false" alt=""
                class:pending=move || state.get() != 1
                on:load=move |_| state.set(1) on:error=move |_| state.set(2)/>
        })}
    }
}

#[component]
fn ThumbView() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="list" on:click=move |_| ctx.selected.set(None) on:contextmenu=move |ev| ctx.show_menu(&ev, None)>
            <div class="thumbs">
                <For each=move || ctx.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                    {
                        let p = e.path.clone();
                        let (click, dbl, menu) = row_events(ctx, &e);
                        let sub = if e.dir { "Folder".to_string() } else { human_size(e.size) };
                        view! {
                            <div class="tile" title=e.name.clone()
                                class:selected=move || ctx.selected.get().as_deref() == Some(p.as_str())
                                on:click=click on:dblclick=dbl on:contextmenu=menu>
                                <div class="pic"><Thumbnail entry=e.clone()/></div>
                                <div class="label">{e.name.clone()}</div>
                                <div class="sub">{sub}</div>
                            </div>
                        }
                    }
                </For>
            </div>
            {empty_note(ctx)}
        </div>
    }
}

/// Finder-style columns: one per folder from "/" down to the current one, then a preview of the
/// selected item. Clicking a folder opens it; clicking a file in an earlier column goes back there.
#[component]
fn ColumnView() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let chain = Memo::new(move |_| ancestors(&ctx.cwd.get()));
    // Fetch the folders to the left that aren't known yet (e.g. when starting deep in this view).
    Effect::new(move |_| {
        let cwd = ctx.cwd.get();
        for dir in chain.get() {
            if dir == cwd || ctx.cols.with_untracked(|c| c.contains_key(&dir)) {
                continue;
            }
            spawn_local(async move {
                if let Ok(es) = call::<Vec<Entry>>("list", json!({ "path": dir, "current": false })).await {
                    ctx.cols.update(|c| {
                        c.insert(dir, es);
                    });
                }
            });
        }
    });
    let strip = NodeRef::<html::Div>::new();
    // Show the newest column: scroll to the right end when the folder changes.
    Effect::new(move |_| {
        chain.track();
        request_animation_frame(move || {
            if let Some(el) = strip.get_untracked() {
                el.set_scroll_left(el.scroll_width());
            }
        });
    });
    let preview = move || {
        let sel = ctx.selected.get()?;
        ctx.entries.with(|es| es.iter().find(|e| e.path == sel).cloned())
    };
    view! {
        <div class="columns" node_ref=strip>
            <For each=move || chain.get() key=|d| d.clone() let(dir)>
                <Column dir chain/>
            </For>
            {move || preview().map(|e| {
                let k = kind(&e);
                let what = match k {
                    Kind::Folder => "Folder", Kind::Epub => "EPUB book", Kind::Text => "Text file", Kind::Other => "File",
                };
                view! {
                    <div class="preview">
                        <div class="pic"><Thumbnail entry=e.clone()/></div>
                        <b class="name">{e.name.clone()}</b>
                        <span class="sub">{what}{(!e.dir).then(|| format!(" · {}", human_size(e.size)))}</span>
                        <dl>
                            <dt>"Modified"</dt><dd>{when(e.mtime)}</dd>
                            <dt>"Where"</dt><dd class="path">{parent(&e.path)}</dd>
                        </dl>
                        <span class="hint">{if e.dir { "Double-click or → to open" } else { "Double-click to open · right-click for more" }}</span>
                    </div>
                }
            })}
        </div>
    }
}

#[component]
fn Column(dir: String, chain: Memo<Vec<String>>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let d = dir.clone();
    let current = Memo::new(move |_| ctx.cwd.get() == d);
    let d = dir.clone();
    // The folder open to the right of this column, if any.
    let open_child = Memo::new(move |_| chain.with(|c| c.iter().position(|x| *x == d).and_then(|i| c.get(i + 1).cloned())));
    let d = dir.clone();
    let entries = move || if current.get() { Some(ctx.entries.get()) } else { ctx.cols.with(|c| c.get(&d).cloned()) };
    let d = dir.clone();
    let bg_menu = move |ev: web_sys::MouseEvent| {
        if !current.get_untracked() {
            ctx.navigate(d.clone());
        }
        ctx.show_menu(&ev, None);
    };
    view! {
        <div class="col" class:current=current on:contextmenu=bg_menu>
            {move || match entries() {
                None => view! { <div class="empty"><span class="spin"></span></div> }.into_any(),
                Some(es) if es.is_empty() => view! { <div class="empty">"Empty"</div> }.into_any(),
                Some(es) => es.into_iter().map(|e| {
                    let (p1, p2) = (e.path.clone(), e.path.clone());
                    let (e1, e2) = (e.clone(), e.clone());
                    let dir = dir.clone();
                    let click = move |_| {
                        if e1.dir {
                            ctx.navigate(e1.path.clone());
                        } else if current.get_untracked() {
                            ctx.selected.set(Some(e1.path.clone()));
                        } else {
                            ctx.navigate_select(dir.clone(), Some(e1.path.clone()));
                        }
                    };
                    let k = kind(&e);
                    view! {
                        <div class="row"
                            class:open=move || open_child.get().as_deref() == Some(p1.as_str())
                            class:selected=move || current.get() && ctx.selected.get().as_deref() == Some(p2.as_str())
                            on:click=click
                            on:dblclick=move |_| if !e2.dir { ctx.open(e2.clone()) }
                            on:contextmenu=move |ev| ctx.show_menu(&ev, Some(e.clone()))>
                            <span class="icon">{icon(k)}</span>
                            <span class="name">{e.name.clone()}</span>
                            {e.dir.then(|| view! { <span class="chev">"›"</span> })}
                        </div>
                    }
                }).collect_view().into_any(),
            }}
        </div>
    }
}

#[component]
fn ContextMenu() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || ctx.menu.get().map(|m| {
        let items = ctx.menu_items(m.entry.as_ref());
        view! {
            <div class="menu" role="menu" style=format!("left:{}px;top:{}px", m.x, m.y)
                on:contextmenu=|e| { e.prevent_default(); e.stop_propagation(); }>
                {items.into_iter().map(|it| match it {
                    MenuItem::Sep => view! { <div class="sep"></div> }.into_any(),
                    MenuItem::Do { label, key, act, danger, enabled } => {
                        let entry = m.entry.clone();
                        view! {
                            <button role="menuitem" class:danger=danger disabled=!enabled
                                on:click=move |_| { ctx.menu.set(None); ctx.run(act, entry.clone()); }>
                                <span>{label}</span><span class="key">{key}</span>
                            </button>
                        }.into_any()
                    }
                }).collect_view()}
            </div>
        }
    })
}

/// The listing's status bar: what's in the folder, and what's selected.
#[component]
fn ListStatus() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let summary = move || {
        if ctx.loading.get() {
            return "Loading…".to_string();
        }
        ctx.entries.with(|es| {
            let folders = es.iter().filter(|e| e.dir).count();
            let files = es.len() - folders;
            let bytes: u64 = es.iter().filter(|e| !e.dir).map(|e| e.size).sum();
            let n = |c: usize, one: &str, many: &str| format!("{c} {}", if c == 1 { one } else { many });
            match (folders, files) {
                (0, 0) => "Empty folder".to_string(),
                (_, 0) => n(folders, "folder", "folders"),
                (0, _) => format!("{} · {}", n(files, "file", "files"), human_size(bytes)),
                _ => format!("{}, {} · {}", n(folders, "folder", "folders"), n(files, "file", "files"), human_size(bytes)),
            }
        })
    };
    let selection = move || {
        let sel = ctx.selected.get()?;
        ctx.entries.with(|es| es.iter().find(|e| e.path == sel).map(|e| {
            let size = if e.dir { String::new() } else { format!(" · {}", human_size(e.size)) };
            let modified = if e.mtime > 0 { format!(" · modified {}", when(e.mtime)) } else { String::new() };
            format!("{}{size}{modified}", e.name)
        }))
    };
    let device = move || ctx.info.get().map(|i| format!("{} · {}", i.label, i.transport)).unwrap_or_default();
    view! {
        <div class="statusbar">
            <span>{summary}</span>
            <span class="sel">{selection}</span>
            <span class="spacer"></span>
            <span>{device}</span>
        </div>
    }
}

#[component]
fn TextEditor() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let path = move || ctx.editor.get().map(|e| e.path).unwrap_or_default();
    let dirty = Memo::new(move |_| ctx.dirty());
    view! {
        <div class="bar">
            <button class="btn" on:click=move |_| ctx.close_editor(false)>"← Files"</button>
            <span class="title">{path}{move || dirty.get().then_some(" •")}</span>
            <span class="spacer"></span>
            <button class="btn primary" title="⌘S" disabled=move || !dirty.get() || ctx.busy.get() on:click=move |_| ctx.save()>"Save"</button>
        </div>
        {move || ctx.confirm_discard.get().then(|| view! {
            <div class="notice">
                <span class="text">"You have unsaved changes."</span>
                <button class="btn primary" on:click=move |_| ctx.save()>"Save"</button>
                <button class="btn danger" on:click=move |_| ctx.close_editor(true)>"Discard changes"</button>
                <button class="btn" on:click=move |_| ctx.confirm_discard.set(false)>"Keep editing"</button>
            </div>
        })}
        <Notices/>
        <div class="editor">
            <textarea spellcheck="false" prop:value=move || ctx.text.get()
                on:input=move |ev| ctx.text.set(event_target_value(&ev))></textarea>
        </div>
        <div class="statusbar">
            <span class=move || format!("status {}", ctx.status.get().1)>{move || {
                let (msg, _) = ctx.status.get();
                if !msg.is_empty() { msg } else if dirty.get() { "Unsaved changes".into() } else { "Saved".into() }
            }}</span>
            <span class="spacer"></span>
            <span>{move || ctx.text.with(|t| {
                let lines = t.lines().count().max(1);
                let chars = t.chars().count();
                format!("{lines} {} · {chars} {}", if lines == 1 { "line" } else { "lines" }, if chars == 1 { "character" } else { "characters" })
            })}</span>
        </div>
    }
}
