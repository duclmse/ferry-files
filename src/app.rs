//! The file browser UI: connect screen, one or two panes of folders (each on the phone or the Mac,
//! in a list, thumbnail or column view, with a context menu), and a text editor.

use crate::api::{call, Devices, Entry, Info, TextFile, WriteResult};
use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::collections::HashMap;
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

fn base(path: &str) -> String {
    match path.trim_end_matches('/').rsplit('/').next() {
        Some("") | None => "/".into(),
        Some(b) => b.into(),
    }
}

fn icon(k: Kind) -> &'static str {
    match k {
        Kind::Folder => "📁",
        Kind::Epub => "📘",
        Kind::Text => "📝",
        Kind::Other => "📄",
    }
}

/// Where a pane's files are: on the phone or on the Mac.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Src {
    Device,
    Mac,
}

impl Src {
    /// The name the backend's commands use.
    fn key(self) -> &'static str {
        match self {
            Src::Device => "device",
            Src::Mac => "mac",
        }
    }

    /// For sentences: "Saved to the phone."
    fn place(self) -> &'static str {
        match self {
            Src::Device => "the phone",
            Src::Mac => "the Mac",
        }
    }
}

const THUMB_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "epub"];
/// Same limit as the backend (src-tauri/src/thumbs.rs): bigger files aren't read for a thumbnail.
const THUMB_MAX: u64 = 60 * 1024 * 1024;

/// Thumbnail URL for images and EPUBs (their cover), served by the backend's `yomi://` scheme.
fn thumb_url(src: Src, e: &Entry) -> Option<String> {
    (!e.dir && e.size <= THUMB_MAX && THUMB_EXTS.contains(&ext(&e.name).as_str())).then(|| {
        let p = String::from(js_sys::encode_uri_component(&e.path));
        format!("yomi://localhost/thumb?src={}&p={p}&s={}&m={}", src.key(), e.size, e.mtime)
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

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

fn stored(key: &str) -> Option<String> {
    storage().and_then(|s| s.get_item(key).ok().flatten())
}

fn store(key: &str, value: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, value);
    }
}

const SPLIT_KEY: &str = "yomi-files.split";

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    List,
    Thumbs,
    Columns,
}

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

    fn storage_key(pane: usize) -> String {
        format!("yomi-files.view.{pane}")
    }

    /// The view the pane had last time. Storage can be missing or blocked; then it's the list.
    fn stored(pane: usize) -> Self {
        let saved = stored(&Self::storage_key(pane));
        Self::ALL.into_iter().find(|v| saved.as_deref() == Some(v.key())).unwrap_or(ViewMode::List)
    }
}

/// An open context menu: where it is, which pane it's for, and the file or folder it's for
/// (None: the folder's background).
#[derive(Clone, PartialEq)]
struct Menu {
    x: f64,
    y: f64,
    pane: usize,
    entry: Option<Entry>,
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Open,
    Download,
    Copy,
    Delete,
    NewFolder,
    NewFile,
    Upload,
    Refresh,
    View(ViewMode),
    Split,
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
    src: Src,
    /// The pane it was opened from; it lists the folder again when the editor closes.
    pane: usize,
    path: String,
    /// Version of the file we started from; saving refuses if the file differs now.
    version: String,
    saved_text: String,
}

/// One pane of files. Each pane has its own place (phone or Mac, and folder), selection and view.
#[derive(Clone, Copy)]
struct Pane {
    id: usize,
    src: RwSignal<Src>,
    cwd: RwSignal<String>,
    entries: RwSignal<Vec<Entry>>,
    loading: RwSignal<bool>,
    selected: RwSignal<Option<String>>,
    notice: RwSignal<Notice>,
    view: RwSignal<ViewMode>,
    /// Listings of other folders, for the column view's columns to the left of the current one.
    cols: RwSignal<HashMap<String, Vec<Entry>>>,
    /// The folder last shown on each side, so switching between phone and Mac goes back there.
    last: StoredValue<HashMap<Src, String>>,
}

impl Pane {
    fn new(id: usize) -> Self {
        Pane {
            id,
            src: RwSignal::new(if id == 0 { Src::Device } else { Src::Mac }),
            cwd: RwSignal::new(String::new()),
            entries: RwSignal::new(vec![]),
            loading: RwSignal::new(false),
            selected: RwSignal::new(None),
            notice: RwSignal::new(Notice::None),
            view: RwSignal::new(ViewMode::stored(id)),
            cols: RwSignal::new(HashMap::new()),
            last: StoredValue::new(HashMap::new()),
        }
    }

    fn error(self, msg: impl Into<String>) {
        self.notice.set(Notice::Error(msg.into()));
    }

    fn selected_entry(self) -> Option<Entry> {
        let sel = self.selected.get()?;
        self.entries.with(|es| es.iter().find(|e| e.path == sel).cloned())
    }
}

#[derive(Clone, Copy)]
struct Ctx {
    /// The connected phone.
    info: RwSignal<Option<Info>>,
    /// The Mac's own files: label and places.
    mac: RwSignal<Option<Info>>,
    panes: [Pane; 2],
    /// The pane the keyboard and the sidebar act on.
    active: RwSignal<usize>,
    /// Whether the second pane is shown.
    split: RwSignal<bool>,
    editor: RwSignal<Option<Editor>>,
    text: RwSignal<String>,
    status: RwSignal<(String, &'static str)>,
    confirm_discard: RwSignal<bool>,
    busy: RwSignal<bool>,
    ep: crate::epub::Ep,
    menu: RwSignal<Option<Menu>>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            info: RwSignal::new(None),
            mac: RwSignal::new(None),
            panes: [Pane::new(0), Pane::new(1)],
            active: RwSignal::new(0),
            split: RwSignal::new(stored(SPLIT_KEY).as_deref() == Some("1")),
            editor: RwSignal::new(None),
            text: RwSignal::new(String::new()),
            status: RwSignal::new((String::new(), "")),
            confirm_discard: RwSignal::new(false),
            busy: RwSignal::new(false),
            ep: crate::epub::Ep::new(),
            menu: RwSignal::new(None),
        }
    }

    fn act(self) -> Pane {
        self.panes[self.active.get_untracked()]
    }

    fn other(self, p: Pane) -> Pane {
        self.panes[1 - p.id]
    }

    /// The panes on screen.
    fn shown(self) -> Vec<Pane> {
        if self.split.get_untracked() { self.panes.to_vec() } else { vec![self.panes[0]] }
    }

    fn src_info(self, src: Src) -> Option<Info> {
        match src {
            Src::Device => self.info.get(),
            Src::Mac => self.mac.get(),
        }
    }

    fn dirty(&self) -> bool {
        self.editor.with(|e| e.as_ref().is_some_and(|e| e.saved_text != self.text.get()))
    }

    /// Lists `path` on `src` and makes it pane `p`'s folder, with `select` selected.
    /// Keeps the old listing (and side) if it fails.
    fn load_select(self, p: Pane, src: Src, path: String, select: Option<String>) {
        p.loading.set(true);
        spawn_local(async move {
            match call::<Vec<Entry>>("list", json!({ "src": src.key(), "path": path, "pane": p.id })).await {
                Ok(entries) => {
                    if p.src.get_untracked() != src {
                        p.cols.set(HashMap::new());
                        p.src.set(src);
                    }
                    p.cols.update(|c| {
                        c.insert(path.clone(), entries.clone());
                    });
                    p.last.update_value(|l| {
                        l.insert(src, path.clone());
                    });
                    p.cwd.set(path);
                    p.entries.set(entries);
                    p.selected.set(select);
                    if matches!(p.notice.get_untracked(), Notice::Error(_)) {
                        p.notice.set(Notice::None);
                    }
                }
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    /// Lists the pane's folder again, keeping the selection. Other folders' listings are dropped too.
    fn reload(self, p: Pane) {
        let cwd = p.cwd.get_untracked();
        if cwd.is_empty() {
            return;
        }
        p.cols.set(HashMap::new());
        self.load_select(p, p.src.get_untracked(), cwd, p.selected.get_untracked());
    }

    /// Lists again every shown pane that's in folder `dir` on `src`, after something changed there.
    fn changed(self, src: Src, dir: &str) {
        for p in self.shown() {
            if p.src.get_untracked() == src && p.cwd.get_untracked() == dir {
                self.reload(p);
            }
        }
    }

    fn reload_all(self) {
        for p in self.shown() {
            self.reload(p);
        }
    }

    /// Asks the phone to re-read its folders (MTP caches them), then lists the folder again.
    fn refresh(self, p: Pane) {
        let src = p.src.get_untracked();
        spawn_local(async move {
            let _ = call::<()>("refresh", json!({ "src": src.key() })).await;
            self.reload(p);
        });
    }

    fn navigate(self, p: Pane, path: String) {
        self.navigate_select(p, p.src.get_untracked(), path, None);
    }

    /// Navigation that would leave the editor asks first when there are unsaved changes.
    fn navigate_select(self, p: Pane, src: Src, path: String, select: Option<String>) {
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return;
            }
            self.editor.set(None);
        }
        self.load_select(p, src, path, select);
    }

    /// Shows the phone or the Mac in pane `p`, in the folder it last showed there.
    fn switch_src(self, p: Pane, src: Src) {
        if p.src.get_untracked() == src {
            return;
        }
        let Some(info) = self.src_info(src) else { return };
        let path = p.last.with_value(|l| l.get(&src).cloned()).unwrap_or(info.default_dir);
        self.navigate_select(p, src, path, None);
    }

    /// Up one folder, with the folder we were in selected.
    fn go_up(self, p: Pane) {
        let cwd = p.cwd.get_untracked();
        if cwd != "/" {
            self.navigate_select(p, p.src.get_untracked(), parent(&cwd), Some(cwd));
        }
    }

    fn set_view(self, p: Pane, v: ViewMode) {
        p.view.set(v);
        store(&ViewMode::storage_key(p.id), v.key());
    }

    fn set_split(self, on: bool) {
        self.split.set(on);
        store(SPLIT_KEY, if on { "1" } else { "0" });
        if on {
            let p = self.panes[1];
            if p.cwd.get_untracked().is_empty() {
                self.start(p);
            } else {
                self.reload(p);
            }
        } else {
            self.active.set(0);
        }
    }

    /// Shows the pane's starting folder: the phone's for the first pane, the Mac's home for the second.
    fn start(self, p: Pane) {
        let src = p.src.get_untracked();
        if let Some(info) = self.src_info(src) {
            self.load_select(p, src, info.default_dir, None);
        }
    }

    /// New files and folders can't go in "/" over MTP: it lists the phone's storages.
    fn in_storage(self, p: Pane) -> bool {
        p.src.get() == Src::Mac || p.cwd.get() != "/" || self.info.get().is_some_and(|i| i.transport == "adb")
    }

    fn show_menu(self, p: Pane, ev: &web_sys::MouseEvent, entry: Option<Entry>) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        if let Some(e) = &entry {
            p.selected.set(Some(e.path.clone()));
        }
        // Keep the whole menu on screen: estimate its size from the number of rows.
        let rows = self.menu_items(p, entry.as_ref()).len() as f64;
        let (w, h) = (240.0, rows * 28.0 + 12.0);
        let size = |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
        let (vw, vh) = (size(window().inner_width()), size(window().inner_height()));
        let x = (ev.client_x() as f64).min(vw - w - 4.0).max(4.0);
        let y = (ev.client_y() as f64).min(vh - h - 4.0).max(4.0);
        self.menu.set(Some(Menu { x, y, pane: p.id, entry }));
    }

    /// "Copy to the Mac", or "Copy to <folder>" when both panes are on the same side.
    fn copy_label(self, p: Pane) -> String {
        let o = self.other(p);
        let (from, to) = (p.src.get_untracked(), o.src.get_untracked());
        if from != to {
            format!("Copy to {}", to.place().trim_start_matches("the "))
        } else {
            format!("Copy to “{}”", base(&o.cwd.get_untracked()))
        }
    }

    fn menu_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
        let device = p.src.get_untracked() == Src::Device;
        let split = self.split.get_untracked();
        let Some(e) = entry else {
            let here = self.in_storage(p);
            let mut v = vec![
                MenuItem::Do { label: "New folder…".into(), key: "", act: Act::NewFolder, danger: false, enabled: here },
                MenuItem::Do { label: "New text file…".into(), key: "", act: Act::NewFile, danger: false, enabled: here },
            ];
            if device {
                v.push(MenuItem::Do { label: "Upload files…".into(), key: "", act: Act::Upload, danger: false, enabled: here });
            }
            v.extend([MenuItem::Sep, item("Refresh", "", Act::Refresh), MenuItem::Sep]);
            let now = p.view.get_untracked();
            v.extend(ViewMode::ALL.map(|m| {
                item(format!("{} as {}", if m == now { "✓" } else { "\u{2003}" }, m.label()), "", Act::View(m))
            }));
            v.push(MenuItem::Sep);
            v.push(item(if split { "Hide second pane" } else { "Show second pane" }, "⌘\\", Act::Split));
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
        if device && !e.dir {
            v.push(item("Download to Mac…", if open.is_none() { "↵" } else { "" }, Act::Download));
        }
        if split {
            let enabled = !e.dir && self.in_storage(self.other(p));
            v.push(MenuItem::Do { label: self.copy_label(p), key: "F5", act: Act::Copy, danger: false, enabled });
        }
        if !v.is_empty() {
            v.push(MenuItem::Sep);
        }
        let label = if e.dir { "Delete empty folder…" } else { "Delete…" };
        v.push(MenuItem::Do { label: label.into(), key: "⌘⌫", act: Act::Delete, danger: true, enabled: true });
        v
    }

    fn run(self, p: Pane, act: Act, entry: Option<Entry>) {
        match (act, entry) {
            (Act::Open, Some(e)) => self.open(p, e),
            (Act::Download, Some(e)) => self.download(p, e.path),
            (Act::Copy, Some(e)) => self.copy(p, e),
            (Act::Delete, Some(e)) => p.notice.set(Notice::ConfirmDelete(e)),
            (Act::NewFolder, _) => p.notice.set(Notice::NewItem { folder: true }),
            (Act::NewFile, _) => p.notice.set(Notice::NewItem { folder: false }),
            (Act::Upload, _) => self.upload(p),
            (Act::Refresh, _) => self.refresh(p),
            (Act::View(v), _) => self.set_view(p, v),
            (Act::Split, _) => self.set_split(!self.split.get_untracked()),
            _ => {}
        }
    }

    fn open(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        match kind(&e) {
            Kind::Folder => self.navigate(p, e.path),
            Kind::Epub => {
                p.loading.set(true);
                let ep = self.ep;
                spawn_local(async move {
                    if let Err(err) = ep.open(src.key(), e.path).await {
                        p.error(err);
                    }
                    p.loading.set(false);
                });
            }
            Kind::Text => self.open_text(p, e.path),
            Kind::Other if src == Src::Device => self.download(p, e.path),
            Kind::Other => {}
        }
    }

    fn open_text(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    self.text.set(f.text.clone());
                    self.editor.set(Some(Editor { src, pane: p.id, path, version: f.version, saved_text: f.text }));
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                }
                Err(e) => p.error(e),
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
            let args = json!({ "src": ed.src.key(), "path": ed.path, "text": text, "base_version": ed.version, "create": false });
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    let note = if r.backup.is_some() { " The old version is backed up on the Mac." } else { "" };
                    self.status.set((format!("Saved to {}.{note}", ed.src.place()), "ok"));
                    self.editor.set(Some(Editor { version: r.version, saved_text: text, ..ed }));
                    self.confirm_discard.set(false);
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
        self.reload_all();
    }

    fn download(self, p: Pane, path: String) {
        spawn_local(async move {
            match call::<Option<String>>("download", json!({ "path": path })).await {
                Ok(Some(dest)) => p.notice.set(Notice::Ok(format!("Saved to {dest}"))),
                Ok(None) => {}
                Err(e) => p.error(e),
            }
        });
    }

    fn upload(self, p: Pane) {
        let dir = p.cwd.get_untracked();
        self.busy.set(true);
        spawn_local(async move {
            match call::<Vec<String>>("upload", json!({ "dir": dir })).await {
                Ok(lines) if lines.is_empty() => {}
                Ok(lines) => {
                    p.notice.set(Notice::Ok(lines.join("\n")));
                    self.changed(Src::Device, &dir);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    /// Copies a file into the other pane's folder.
    fn copy(self, p: Pane, e: Entry) {
        if !self.split.get_untracked() || e.dir || self.busy.get_untracked() {
            return;
        }
        let o = self.other(p);
        let (from_src, to_src, to_dir) = (p.src.get_untracked(), o.src.get_untracked(), o.cwd.get_untracked());
        if to_src == from_src && to_dir == p.cwd.get_untracked() {
            p.error("Both panes show this folder. Open another folder in the other pane first.");
            return;
        }
        self.busy.set(true);
        p.notice.set(Notice::Ok(format!("Copying {}…", e.name)));
        spawn_local(async move {
            let args = json!({ "from_src": from_src.key(), "from": e.path, "to_src": to_src.key(), "to_dir": to_dir });
            match call::<String>("copy", args).await {
                Ok(target) => {
                    p.notice.set(Notice::Ok(format!("Copied {} to {}.", e.name, parent(&target))));
                    self.changed(to_src, &to_dir);
                    o.selected.set(Some(target));
                }
                Err(err) => p.error(err),
            }
            self.busy.set(false);
        });
    }

    fn delete(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        spawn_local(async move {
            match call::<Option<String>>("remove", json!({ "src": src.key(), "path": e.path })).await {
                Ok(backup) => {
                    let note = backup.map(|b| format!(" Backup: {b}")).unwrap_or_default();
                    p.notice.set(Notice::Ok(format!("Deleted {}.{note}", e.name)));
                    self.changed(src, &parent(&e.path));
                }
                Err(err) => p.error(err),
            }
        });
    }

    fn create(self, p: Pane, name: String, folder: bool) {
        let name = name.trim().to_string();
        if name.is_empty() || name.contains('/') {
            p.error("Enter a name without /.");
            return;
        }
        let src = p.src.get_untracked();
        let dir = p.cwd.get_untracked();
        let path = join(&dir, &name);
        spawn_local(async move {
            let result = if folder {
                call::<()>("mkdir", json!({ "src": src.key(), "path": path })).await
            } else {
                let args = json!({ "src": src.key(), "path": path, "text": "", "base_version": null, "create": true });
                call::<WriteResult>("write_text", args).await.map(|_| ())
            };
            match result {
                Ok(()) => {
                    p.notice.set(Notice::None);
                    if folder { self.changed(src, &dir) } else { self.open_text(p, path) }
                }
                Err(e) => p.error(e),
            }
        });
    }

    /// After connecting: the phone in the first pane, the Mac in the second.
    fn connected(self, info: Info) {
        self.info.set(Some(info));
        for p in self.panes {
            p.src.set(if p.id == 0 { Src::Device } else { Src::Mac });
            p.last.set_value(HashMap::new());
        }
        spawn_local(async move {
            if let Ok(Some(mac)) = call::<Option<Info>>("info", json!({ "src": "mac" })).await {
                self.mac.set(Some(mac));
            }
            self.start(self.panes[0]);
            if self.split.get_untracked() {
                self.start(self.panes[1]);
            }
        });
    }

    fn connect(self, transport: &'static str, conn_error: RwSignal<Option<String>>) {
        self.busy.set(true);
        conn_error.set(None);
        spawn_local(async move {
            match call::<Info>("connect", json!({ "transport": transport, "serial": null })).await {
                Ok(info) => self.connected(info),
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
            for p in self.panes {
                p.entries.set(vec![]);
                p.cwd.set(String::new());
                p.cols.set(HashMap::new());
                p.notice.set(Notice::None);
            }
        });
    }
}

#[component]
pub fn App() -> impl IntoView {
    let ctx = Ctx::new();
    provide_context(ctx);
    let ep = ctx.ep;
    provide_context(ep);
    // Closing a book goes back to the panes, which may have changed (a save writes the file and its backup).
    Effect::new(move |was_open: Option<bool>| {
        let open = ep.book.with(|b| b.is_some());
        if was_open == Some(true) && !open {
            ctx.reload_all();
        }
        open
    });

    // Pick up an existing connection after a webview reload.
    spawn_local(async move {
        if let Ok(Some(info)) = call::<Option<Info>>("info", json!({})).await {
            ctx.connected(info);
        }
    });

    // Keyboard. ⌘S saves. In the panes: Tab switches pane, ⌘\ shows or hides the second one, arrows
    // move (←/→ also go up/in in the column view and move across in the thumbnail view), Enter or
    // ⌘↓ opens, ⌘↑ or Backspace goes up, ⌘⌫ deletes, F5 copies to the other pane.
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
        let p = ctx.act();
        if ctx.info.get_untracked().is_none() || !matches!(p.notice.get_untracked(), Notice::None | Notice::Ok(_) | Notice::Error(_)) {
            return;
        }
        let entries = p.entries.get_untracked();
        let idx = p.selected.get_untracked().and_then(|s| entries.iter().position(|x| x.path == s));
        let current = idx.and_then(|i| entries.get(i)).cloned();
        let view = p.view.get_untracked();
        let step = |delta: isize| {
            let last = entries.len() as isize - 1;
            let next = match idx {
                None => 0,
                Some(i) => (i as isize + delta).clamp(0, last.max(0)),
            };
            if let Some(x) = entries.get(next as usize) {
                p.selected.set(Some(x.path.clone()));
            }
        };
        // Tiles per row in the thumbnail view, from the grid's width (tiles are 128px plus an 8px gap).
        let per_row = || {
            document()
                .query_selector(".pane.active .thumbs")
                .ok()
                .flatten()
                .map_or(1, |g| ((g.client_width() - 24 + 8) / 136).max(1) as isize)
        };
        match (e.key().as_str(), meta) {
            ("Tab", false) if ctx.split.get_untracked() => {
                e.prevent_default();
                ctx.active.set(1 - p.id);
            }
            ("\\", true) => {
                e.prevent_default();
                ctx.set_split(!ctx.split.get_untracked());
            }
            ("F5", _) => {
                if let Some(x) = current {
                    e.prevent_default();
                    ctx.copy(p, x);
                }
            }
            ("Backspace", true) => {
                if let Some(x) = current {
                    e.prevent_default();
                    p.notice.set(Notice::ConfirmDelete(x));
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
                    ctx.navigate(p, x.path);
                }
            }
            ("ArrowLeft", false) if view == ViewMode::Columns => {
                e.prevent_default();
                ctx.go_up(p);
            }
            ("Enter", _) | ("ArrowDown", true) => {
                if let Some(x) = current {
                    e.prevent_default();
                    ctx.open(p, x);
                }
            }
            ("Backspace", false) | ("ArrowUp", true) => {
                e.prevent_default();
                ctx.go_up(p);
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
    // No browser menu (Reload, Inspect…) outside text fields; the panes have their own.
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
        let p = ctx.panes[ctx.active.get()];
        p.selected.track();
        p.view.track();
        request_animation_frame(|| {
            if let Ok(Some(el)) = document().query_selector(".pane.active .selected") {
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
                {move || if ctx.editor.get().is_some() { view! { <TextEditor/> }.into_any() } else { view! {
                    <div class="panes">
                        <PaneView pane=ctx.panes[0]/>
                        {move || ctx.split.get().then(|| view! { <PaneView pane=ctx.panes[1]/> })}
                    </div>
                }.into_any() }}
            </div>
        </div>
    }.into_any() }
}

/// Places on the phone and on the Mac. Clicking one opens it in the active pane.
#[component]
fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let place = move |src: Src, name: String, path: String| {
        let p = path.clone();
        let active = move || {
            let pane = ctx.panes[ctx.active.get()];
            pane.src.get() == src && pane.cwd.get() == p
        };
        view! {
            <button class="item" class:active=active title=path.clone()
                on:click=move |_| ctx.navigate_select(ctx.act(), src, path.clone(), None)>{name}</button>
        }
    };
    let phone = move || ctx.info.get();
    view! {
        <nav class="side">
            <div class="device">
                <b>{move || phone().map(|i| i.label).unwrap_or_default()}</b>
                <span class="kind">{move || phone().map(|i| i.transport).unwrap_or_default()}</span>
            </div>
            <h2>"Phone"</h2>
            {move || phone().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                .map(|(name, path)| place(Src::Device, name, path)).collect_view()}
            {move || place(Src::Device, "/ (all storage)".into(), "/".into())}
            <h2>"Mac"</h2>
            {move || ctx.mac.get().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                .map(|(name, path)| place(Src::Mac, name, path)).collect_view()}
            <span class="grow"></span>
            <button class="btn" class:active=move || ctx.split.get() title="⌘\\"
                on:click=move |_| ctx.set_split(!ctx.split.get_untracked())>
                {move || if ctx.split.get() { "Hide second pane" } else { "Show second pane" }}
            </button>
            <button class="btn" on:click=move |_| ctx.disconnect()>"Disconnect"</button>
            <div class="foot">"Backups of replaced and deleted files: " {move || phone().map(|i| i.backup_dir).unwrap_or_default()}</div>
        </nav>
    }
}

#[component]
fn Crumbs(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    view! {
        <div class="crumbs">
            {move || {
                let cwd = p.cwd.get();
                let mut acc = String::new();
                let mut out = vec![view! { <button on:click=move |_| ctx.navigate(p, "/".into())>"/"</button> }.into_any()];
                for part in cwd.split('/').filter(|s| !s.is_empty()) {
                    acc = format!("{acc}/{part}");
                    let target = acc.clone();
                    out.push(view! {
                        <button on:click=move |_| ctx.navigate(p, target.clone())>{part.to_string()}</button>
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
fn Notices(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let name = RwSignal::new(String::new());
    let dismiss = move |_| p.notice.set(Notice::None);
    view! {
        {move || match p.notice.get() {
            Notice::None => ().into_any(),
            Notice::Error(m) => view! {
                <div class="notice error"><span class="text">{m}</span>
                    <button class="btn" on:click=dismiss>"Dismiss"</button></div>
            }.into_any(),
            Notice::Ok(m) => view! {
                <div class="notice ok"><span class="text">{m}</span>
                    <button class="btn" on:click=dismiss>"Dismiss"</button></div>
            }.into_any(),
            Notice::ConfirmDelete(e) => {
                let what = if e.dir { "the empty folder " } else { "" };
                let label = format!("Delete {what}{} from {}?{}", e.name, p.src.get_untracked().place(),
                    if e.dir { "" } else { " A backup is kept on the Mac." });
                view! {
                    <div class="notice"><span class="text">{label}</span>
                        <button class="btn danger solid" on:click=move |_| ctx.delete(p, e.clone())>"Delete"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button></div>
                }.into_any()
            }
            Notice::NewItem { folder } => {
                name.set(String::new());
                let submit = move || ctx.create(p, name.get_untracked(), folder);
                view! {
                    <div class="notice">
                        <span>{if folder { "New folder:" } else { "New text file:" }}</span>
                        <input autofocus placeholder=if folder { "Folder name" } else { "notes.txt" }
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => submit(),
                                "Escape" => p.notice.set(Notice::None),
                                _ => {}
                            }/>
                        <button class="btn primary" on:click=move |_| submit()>"Create"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button>
                    </div>
                }.into_any()
            }
        }}
    }
}

/// One pane: its toolbar (phone or Mac, the folder path, the view), its files, and its status bar.
/// New folder, new file, upload and copy are in the context menu.
#[component]
fn PaneView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let active = move || ctx.active.get() == p.id;
    view! {
        <section class="pane" class:active=active on:mousedown=move |_| ctx.active.set(p.id)>
            <div class="bar">
                <button class="btn" title="Up (⌘↑)" disabled=move || p.cwd.get() == "/" on:click=move |_| ctx.go_up(p)>"↑"</button>
                <div class="seg" role="group" aria-label="Show files on">
                    {[(Src::Device, "📱", "Phone"), (Src::Mac, "💻", "Mac")].map(|(src, glyph, label)| view! {
                        <button title=format!("Files on the {label}") aria-label=label
                            class:active=move || p.src.get() == src on:click=move |_| ctx.switch_src(p, src)>{glyph}</button>
                    })}
                </div>
                <Crumbs pane=p/>
                {move || p.loading.get().then(|| view! { <span class="spin"></span> })}
                <div class="seg" role="group" aria-label="View">
                    {ViewMode::ALL.map(|m| view! {
                        <button title=format!("View as {}", m.label()) aria-label=m.label()
                            class:active=move || p.view.get() == m on:click=move |_| ctx.set_view(p, m)>{m.glyph()}</button>
                    })}
                </div>
                <button class="btn" title="Refresh" aria-label="Refresh" on:click=move |_| ctx.refresh(p)>"⟳"</button>
                {(p.id == 1).then(|| view! {
                    <button class="btn" title="Hide this pane (⌘\\)" aria-label="Hide this pane"
                        on:click=move |_| ctx.set_split(false)>"✕"</button>
                })}
            </div>
            <Notices pane=p/>
            {move || match p.view.get() {
                ViewMode::List => view! { <ListView pane=p/> }.into_any(),
                ViewMode::Thumbs => view! { <ThumbView pane=p/> }.into_any(),
                ViewMode::Columns => view! { <ColumnView pane=p/> }.into_any(),
            }}
            <ListStatus pane=p/>
        </section>
    }
}

/// Selects on click, opens on double-click, and shows the context menu on right-click.
fn row_events(
    ctx: Ctx,
    p: Pane,
    e: &Entry,
) -> (impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static) {
    let (path, e1, e2) = (e.path.clone(), e.clone(), e.clone());
    (
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            p.selected.set(Some(path.clone()));
        },
        move |_| ctx.open(p, e1.clone()),
        move |ev: web_sys::MouseEvent| ctx.show_menu(p, &ev, Some(e2.clone())),
    )
}

fn empty_note(p: Pane) -> impl IntoView {
    move || (p.entries.with(|e| e.is_empty()) && !p.loading.get()).then(|| view! {
        <div class="empty">"This folder is empty."</div>
    })
}

#[component]
fn ListView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    view! {
        <div class="list" on:click=move |_| p.selected.set(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)>
            <table>
                <thead><tr><th>"Name"</th><th class="num">"Modified"</th><th class="num">"Size"</th></tr></thead>
                <tbody>
                    <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                        {
                            let k = kind(&e);
                            let path = e.path.clone();
                            let (click, dbl, menu) = row_events(ctx, p, &e);
                            let tag = match k { Kind::Epub => "EPUB", Kind::Text => "text", _ => "" };
                            view! {
                                <tr class:selected=move || p.selected.get().as_deref() == Some(path.as_str())
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
            {empty_note(p)}
        </div>
    }
}

/// The file's icon, replaced by its thumbnail (an image, or an EPUB's cover) once that has loaded.
/// Files with no thumbnail, or whose thumbnail fails, keep the icon.
#[component]
fn Thumbnail(src: Src, entry: Entry) -> impl IntoView {
    let state = RwSignal::new(0u8); // 0 loading, 1 loaded, 2 failed
    let glyph = icon(kind(&entry));
    let url = thumb_url(src, &entry);
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
fn ThumbView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    view! {
        <div class="list" on:click=move |_| p.selected.set(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)>
            <div class="thumbs">
                <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                    {
                        let path = e.path.clone();
                        let (click, dbl, menu) = row_events(ctx, p, &e);
                        let sub = if e.dir { "Folder".to_string() } else { human_size(e.size) };
                        view! {
                            <div class="tile" title=e.name.clone()
                                class:selected=move || p.selected.get().as_deref() == Some(path.as_str())
                                on:click=click on:dblclick=dbl on:contextmenu=menu>
                                <div class="pic"><Thumbnail src=p.src.get_untracked() entry=e.clone()/></div>
                                <div class="label">{e.name.clone()}</div>
                                <div class="sub">{sub}</div>
                            </div>
                        }
                    }
                </For>
            </div>
            {empty_note(p)}
        </div>
    }
}

/// Finder-style columns: one per folder from "/" down to the current one, then a preview of the
/// selected item. Clicking a folder opens it; clicking a file in an earlier column goes back there.
#[component]
fn ColumnView(pane: Pane) -> impl IntoView {
    let p = pane;
    let chain = Memo::new(move |_| ancestors(&p.cwd.get()));
    // Fetch the folders to the left that aren't known yet (e.g. when starting deep in this view).
    Effect::new(move |_| {
        let cwd = p.cwd.get();
        let src = p.src.get_untracked();
        for dir in chain.get() {
            if dir == cwd || p.cols.with_untracked(|c| c.contains_key(&dir)) {
                continue;
            }
            spawn_local(async move {
                let args = json!({ "src": src.key(), "path": dir, "current": false });
                if let Ok(es) = call::<Vec<Entry>>("list", args).await {
                    if p.src.get_untracked() == src {
                        p.cols.update(|c| {
                            c.insert(dir, es);
                        });
                    }
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
    view! {
        <div class="columns" node_ref=strip>
            // Keyed by side too, so switching between phone and Mac rebuilds the columns.
            <For each=move || { let s = p.src.get(); chain.get().into_iter().map(move |d| (s, d)).collect::<Vec<_>>() }
                key=|(s, d)| (s.key(), d.clone()) let((_s, dir))>
                <Column pane=p dir chain/>
            </For>
            {move || p.selected_entry().map(|e| {
                let k = kind(&e);
                let what = match k {
                    Kind::Folder => "Folder", Kind::Epub => "EPUB book", Kind::Text => "Text file", Kind::Other => "File",
                };
                view! {
                    <div class="preview">
                        <div class="pic"><Thumbnail src=p.src.get_untracked() entry=e.clone()/></div>
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
fn Column(pane: Pane, dir: String, chain: Memo<Vec<String>>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let d = dir.clone();
    let current = Memo::new(move |_| p.cwd.get() == d);
    let d = dir.clone();
    // The folder open to the right of this column, if any.
    let open_child = Memo::new(move |_| chain.with(|c| c.iter().position(|x| *x == d).and_then(|i| c.get(i + 1).cloned())));
    let d = dir.clone();
    let entries = move || if current.get() { Some(p.entries.get()) } else { p.cols.with(|c| c.get(&d).cloned()) };
    let d = dir.clone();
    let bg_menu = move |ev: web_sys::MouseEvent| {
        if !current.get_untracked() {
            ctx.navigate(p, d.clone());
        }
        ctx.show_menu(p, &ev, None);
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
                            ctx.navigate(p, e1.path.clone());
                        } else if current.get_untracked() {
                            p.selected.set(Some(e1.path.clone()));
                        } else {
                            ctx.navigate_select(p, p.src.get_untracked(), dir.clone(), Some(e1.path.clone()));
                        }
                    };
                    let k = kind(&e);
                    view! {
                        <div class="row"
                            class:open=move || open_child.get().as_deref() == Some(p1.as_str())
                            class:selected=move || current.get() && p.selected.get().as_deref() == Some(p2.as_str())
                            on:click=click
                            on:dblclick=move |_| if !e2.dir { ctx.open(p, e2.clone()) }
                            on:contextmenu=move |ev| ctx.show_menu(p, &ev, Some(e.clone()))>
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
        let p = ctx.panes[m.pane];
        let items = ctx.menu_items(p, m.entry.as_ref());
        view! {
            <div class="menu" role="menu" style=format!("left:{}px;top:{}px", m.x, m.y)
                on:contextmenu=|e| { e.prevent_default(); e.stop_propagation(); }>
                {items.into_iter().map(|it| match it {
                    MenuItem::Sep => view! { <div class="sep"></div> }.into_any(),
                    MenuItem::Do { label, key, act, danger, enabled } => {
                        let entry = m.entry.clone();
                        view! {
                            <button role="menuitem" class:danger=danger disabled=!enabled
                                on:click=move |_| { ctx.menu.set(None); ctx.run(p, act, entry.clone()); }>
                                <span>{label}</span><span class="key">{key}</span>
                            </button>
                        }.into_any()
                    }
                }).collect_view()}
            </div>
        }
    })
}

/// A pane's status bar: what's in the folder, what's selected, and where the files are.
#[component]
fn ListStatus(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let summary = move || {
        if p.loading.get() {
            return "Loading…".to_string();
        }
        p.entries.with(|es| {
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
        p.selected_entry().map(|e| {
            let size = if e.dir { String::new() } else { format!(" · {}", human_size(e.size)) };
            let modified = if e.mtime > 0 { format!(" · modified {}", when(e.mtime)) } else { String::new() };
            format!("{}{size}{modified}", e.name)
        })
    };
    let place = move || match p.src.get() {
        Src::Device => ctx.info.get().map(|i| format!("{} · {}", i.label, i.transport)).unwrap_or_default(),
        Src::Mac => ctx.mac.get().map(|i| i.label).unwrap_or_default(),
    };
    view! {
        <div class="statusbar">
            <span>{summary}</span>
            <span class="sel">{selection}</span>
            <span class="spacer"></span>
            <span>{place}</span>
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
            <span class="kind">{move || ctx.editor.get().map(|e| if e.src == Src::Mac { "Mac" } else { "Phone" })}</span>
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
