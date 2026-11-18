//! The file browser UI: connect screen, one or two panes of folders (each on the phone or the Mac,
//! in a list, thumbnail or column view, with a context menu), and a text editor.

use crate::api::{call, Backup, BookInfo, CompareReport, Issue, Devices, Space, Entry, FileDrop, Info, Progress, TextFile, TransferReport, UsbDevice, WriteResult};
use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::collections::{HashMap, HashSet};
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

/// Where a pane's files are: on the Mac, or on connected device n (the backend's "dn").
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Src {
    Mac,
    Dev(u32),
}

impl Src {
    /// The name the backend's commands use.
    fn key(self) -> String {
        match self {
            Src::Mac => "mac".into(),
            Src::Dev(n) => format!("d{n}"),
        }
    }

    fn of(id: &str) -> Src {
        id.strip_prefix('d').and_then(|n| n.parse().ok()).map_or(Src::Mac, Src::Dev)
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

/// Whether `path` is `dir` or inside it.
fn within(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    path == dir || dir.is_empty() || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// Every folder from `root` down to `path`, e.g. ("/a", "/a/b/c") gives ["/a", "/a/b", "/a/b/c"].
/// Just `path` when it isn't inside `root`.
fn chain_from(root: &str, path: &str) -> Vec<String> {
    if root.is_empty() || !within(path, root) {
        return vec![path.to_string()];
    }
    let mut out = vec![root.to_string()];
    let mut acc = root.trim_end_matches('/').to_string();
    for part in path[acc.len()..].split('/').filter(|p| !p.is_empty()) {
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
const HIDDEN_KEY: &str = "yomi-files.hidden";
const PHONE_THUMBS_KEY: &str = "yomi-files.phone-thumbs";
const AUTO_CONNECT_KEY: &str = "yomi-files.auto-connect";

/// A setting kept in localStorage as "1" or "0".
fn stored_flag(key: &str, default: bool) -> bool {
    stored(key).map_or(default, |v| v == "1")
}

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

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Kind,
    Date,
    Size,
}

impl SortKey {
    const ALL: [SortKey; 4] = [SortKey::Name, SortKey::Kind, SortKey::Date, SortKey::Size];

    fn key(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Kind => "kind",
            SortKey::Date => "date",
            SortKey::Size => "size",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Kind => "Kind",
            SortKey::Date => "Modified",
            SortKey::Size => "Size",
        }
    }
}

/// How a pane sorts: by what, and whether ascending. Stored as e.g. "date:0".
#[derive(Clone, Copy, PartialEq)]
struct Sort(SortKey, bool);

impl Sort {
    fn storage_key(pane: usize) -> String {
        format!("yomi-files.sort.{pane}")
    }

    fn stored(pane: usize) -> Self {
        let saved = stored(&Self::storage_key(pane)).unwrap_or_default();
        let (k, asc) = saved.split_once(':').unwrap_or(("name", "1"));
        Sort(SortKey::ALL.into_iter().find(|s| s.key() == k).unwrap_or(SortKey::Name), asc != "0")
    }

    /// Folders first, then by the key (ties by name).
    fn apply(self, es: &mut [Entry]) {
        let Sort(key, asc) = self;
        es.sort_by(|a, b| {
            let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
            let o = match key {
                SortKey::Name => by_name(),
                SortKey::Kind => kind_label(a).cmp(&kind_label(b)).then_with(by_name),
                SortKey::Date => a.mtime.cmp(&b.mtime).then_with(by_name),
                SortKey::Size => a.size.cmp(&b.size).then_with(by_name),
            };
            b.dir.cmp(&a.dir).then(if asc { o } else { o.reverse() })
        });
    }
}

/// "Folder", "EPUB book", "Text", or the file's extension ("PDF").
fn kind_label(e: &Entry) -> String {
    match kind(e) {
        Kind::Folder => "Folder".into(),
        Kind::Epub => "EPUB book".into(),
        Kind::Text => "Text".into(),
        Kind::Other => match ext(&e.name) {
            x if x.is_empty() => "File".into(),
            x => x.to_uppercase(),
        },
    }
}

/// A folder in the sidebar's Favourites. `place` is "mac" or the device's transport and name
/// ("adb:Pixel 7"), so it's found again when the device is connected another time.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Fav {
    place: String,
    label: String,
    path: String,
}

const FAVS_KEY: &str = "yomi-files.favs";

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
    OpenInApp,
    Download,
    Copy,
    Move,
    Rename,
    QuickLook,
    Favourite,
    Delete,
    Sync,
    Read,
    Check,
    Library,
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
    ConfirmDelete(Vec<Entry>),
    Rename(Entry),
    /// The result of comparing this pane's folder with the other pane's, before syncing.
    Sync { report: CompareReport, from: (Src, String), to: (Src, String) },
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
    /// The item the keyboard is on.
    selected: RwSignal<Option<String>>,
    /// Items picked with ⌘-click, Shift-click or ⌘A. When any are, they're what actions act on;
    /// otherwise the selected item is.
    marked: RwSignal<Vec<String>>,
    /// The folder's listing (or search results), before the filter.
    all: RwSignal<Vec<Entry>>,
    /// Shows only names containing this.
    filter: RwSignal<String>,
    sort: RwSignal<Sort>,
    /// Set while the pane shows search results instead of the folder: the search and whether it
    /// found everything.
    found: RwSignal<Option<(String, bool)>>,
    notice: RwSignal<Notice>,
    view: RwSignal<ViewMode>,
    /// Listings of other folders, for the column view's columns to the left of the current one.
    cols: RwSignal<HashMap<String, Vec<Entry>>>,
    /// The folder last shown on each side, so switching between phone and Mac goes back there.
    last: StoredValue<HashMap<Src, String>>,
    /// The column view's first column. Reset to the current folder when the view switches to
    /// columns, when a place in the sidebar is opened, and when the pane leaves it.
    root: RwSignal<String>,
    /// Set before opening a sidebar place: that folder becomes the column view's first column.
    reroot: StoredValue<bool>,
}

impl Pane {
    fn new(id: usize) -> Self {
        Pane {
            id,
            src: RwSignal::new(Src::Mac),
            cwd: RwSignal::new(String::new()),
            entries: RwSignal::new(vec![]),
            loading: RwSignal::new(false),
            selected: RwSignal::new(None),
            marked: RwSignal::new(vec![]),
            all: RwSignal::new(vec![]),
            filter: RwSignal::new(String::new()),
            sort: RwSignal::new(Sort::stored(id)),
            found: RwSignal::new(None),
            notice: RwSignal::new(Notice::None),
            view: RwSignal::new(ViewMode::stored(id)),
            cols: RwSignal::new(HashMap::new()),
            last: StoredValue::new(HashMap::new()),
            root: RwSignal::new(String::new()),
            reroot: StoredValue::new(false),
        }
    }

    fn error(self, msg: impl Into<String>) {
        self.notice.set(Notice::Error(msg.into()));
    }

    fn selected_entry(self) -> Option<Entry> {
        let sel = self.selected.get()?;
        self.entries.with(|es| es.iter().find(|e| e.path == sel).cloned())
    }

    /// Sorts and filters `all` into `entries`, dropping marks on items no longer shown.
    fn arrange(self) {
        let mut es = self.all.get_untracked();
        self.sort.get_untracked().apply(&mut es);
        let f = self.filter.get_untracked().to_lowercase();
        if !f.is_empty() {
            es.retain(|e| e.name.to_lowercase().contains(&f));
        }
        self.marked.update(|m| m.retain(|x| es.iter().any(|e| &e.path == x)));
        self.entries.set(es);
    }

    fn set_sort(self, key: SortKey) {
        let Sort(now, asc) = self.sort.get_untracked();
        // Clicking the same column again reverses it; dates and sizes start biggest first.
        let s = if now == key { Sort(key, !asc) } else { Sort(key, matches!(key, SortKey::Name | SortKey::Kind)) };
        self.sort.set(s);
        store(&Sort::storage_key(self.id), &format!("{}:{}", s.0.key(), if s.1 { 1 } else { 0 }));
        self.cols.update(|c| c.values_mut().for_each(|es| s.apply(es)));
        self.arrange();
    }

    /// Whether `path` is one of the items actions act on (tracked, for highlighting).
    fn is_picked(self, path: &str) -> bool {
        let marked = self.marked.with(|m| (!m.is_empty()).then(|| m.iter().any(|x| x == path)));
        marked.unwrap_or_else(|| self.selected.get().as_deref() == Some(path))
    }

    /// The items actions act on: the marked ones, or else the selected one.
    fn picked(self) -> Vec<Entry> {
        let marked = self.marked.get_untracked();
        let sel = self.selected.get_untracked();
        self.entries.with_untracked(|es| {
            es.iter()
                .filter(|e| if marked.is_empty() { sel.as_deref() == Some(e.path.as_str()) } else { marked.contains(&e.path) })
                .cloned()
                .collect()
        })
    }

    /// Selects one item, unmarking the rest.
    fn select_only(self, path: Option<String>) {
        self.marked.set(vec![]);
        self.selected.set(path);
    }

    /// A click on an item: ⌘ adds or removes it, Shift picks everything from the selected item to it.
    fn click(self, ev: &web_sys::MouseEvent, path: &str) {
        if ev.meta_key() {
            let sel = self.selected.get_untracked();
            self.marked.update(|m| {
                if m.is_empty() {
                    m.extend(sel);
                }
                match m.iter().position(|x| x == path) {
                    Some(i) => {
                        m.remove(i);
                    }
                    None => m.push(path.to_string()),
                }
            });
            self.selected.set(Some(path.to_string()));
        } else if ev.shift_key() && self.selected.get_untracked().is_some() {
            let anchor = self.selected.get_untracked().unwrap_or_default();
            let range = self.entries.with_untracked(|es| {
                let at = |p: &str| es.iter().position(|e| e.path == p);
                match (at(&anchor), at(path)) {
                    (Some(a), Some(b)) => es[a.min(b)..=a.max(b)].iter().map(|e| e.path.clone()).collect(),
                    _ => vec![path.to_string()],
                }
            });
            self.marked.set(range);
        } else {
            self.select_only(Some(path.to_string()));
        }
    }
}

#[derive(Clone, PartialEq)]
enum Panel {
    Backups,
    History,
    /// The books under a folder.
    Library(Src, String),
    /// Reading a book; the folder is the library to go back to.
    Reader(Src, String, Option<String>),
}

/// The copy or move running now (one at a time).
#[derive(Clone, PartialEq)]
struct Job {
    id: u32,
    moving: bool,
    progress: Option<Progress>,
}

#[derive(Clone, Copy)]
struct Ctx {
    /// The connected devices, oldest first.
    phones: RwSignal<Vec<Info>>,
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
    settings: RwSignal<bool>,
    /// Devices on USB that aren't connected, and the Mac's disks (checked every few seconds).
    devices: RwSignal<Option<Devices>>,
    scanning: StoredValue<bool>,
    /// The device being connected (its `usb`).
    connecting: RwSignal<Option<String>>,
    conn_error: RwSignal<Option<String>>,
    /// Settings: connect phones as soon as they're found, with adb when it can, else MTP.
    auto_connect: RwSignal<bool>,
    /// Phones (`UsbDevice::key`) connected automatically once already: not again, even after a
    /// failure or a disconnect, until they're unplugged.
    tried: StoredValue<HashSet<String>>,
    /// Phones found by the last look, and how many looks there have been.
    seen: StoredValue<(HashSet<String>, u32)>,
    /// Settings: list names starting with a dot.
    show_hidden: RwSignal<bool>,
    /// Settings: make thumbnails of phone files (each one reads the whole file over USB).
    phone_thumbs: RwSignal<bool>,
    job: RwSignal<Option<Job>>,
    next_job: StoredValue<u32>,
    favs: RwSignal<Vec<Fav>>,
    /// Free space per storage, by side ("mac", "d1", ...).
    space: RwSignal<HashMap<String, Vec<Space>>>,
    /// What was done this session, newest last: (time, what).
    history: RwSignal<Vec<(String, String)>>,
    /// The Backups or History window, when open.
    panel: RwSignal<Option<Panel>>,
    /// What's being dragged from a pane: its side and paths.
    drag: StoredValue<Option<(Src, Vec<String>)>>,
    /// Where a drag would drop now ("pane:folder"), to highlight it.
    drop_at: RwSignal<Option<String>>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            phones: RwSignal::new(vec![]),
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
            settings: RwSignal::new(false),
            devices: RwSignal::new(None),
            scanning: StoredValue::new(false),
            connecting: RwSignal::new(None),
            conn_error: RwSignal::new(None),
            show_hidden: RwSignal::new(stored_flag(HIDDEN_KEY, false)),
            phone_thumbs: RwSignal::new(stored_flag(PHONE_THUMBS_KEY, true)),
            auto_connect: RwSignal::new(stored_flag(AUTO_CONNECT_KEY, true)),
            tried: StoredValue::new(HashSet::new()),
            seen: StoredValue::new((HashSet::new(), 0)),
            job: RwSignal::new(None),
            next_job: StoredValue::new(1),
            space: RwSignal::new(HashMap::new()),
            history: RwSignal::new(vec![]),
            panel: RwSignal::new(None),
            favs: RwSignal::new(stored(FAVS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()),
            drag: StoredValue::new(None),
            drop_at: RwSignal::new(None),
        }
    }

    /// Drops hidden files from a listing unless the settings say to show them.
    fn visible(self, entries: Vec<Entry>) -> Vec<Entry> {
        if self.show_hidden.get_untracked() {
            entries
        } else {
            entries.into_iter().filter(|e| !e.name.starts_with('.')).collect()
        }
    }

    fn set_show_hidden(self, on: bool) {
        self.show_hidden.set(on);
        store(HIDDEN_KEY, if on { "1" } else { "0" });
        self.reload_all();
    }

    fn set_phone_thumbs(self, on: bool) {
        self.phone_thumbs.set(on);
        store(PHONE_THUMBS_KEY, if on { "1" } else { "0" });
    }

    fn set_auto_connect(self, on: bool) {
        self.auto_connect.set(on);
        store(AUTO_CONNECT_KEY, if on { "1" } else { "0" });
        if on {
            self.scan();
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
            Src::Mac => self.mac.get(),
            Src::Dev(_) => {
                let id = src.key();
                self.phones.with(|ps| ps.iter().find(|i| i.id == id).cloned())
            }
        }
    }

    /// For sentences: "Saved to the Mac." or "Saved to Pixel 7."
    fn place(self, src: Src) -> String {
        match src {
            Src::Mac => "the Mac".into(),
            Src::Dev(_) => self.src_info(src).map_or_else(|| "the device".into(), |i| i.label),
        }
    }

    fn dirty(&self) -> bool {
        self.editor.with(|e| e.as_ref().is_some_and(|e| e.saved_text != self.text.get()))
    }

    /// Lists `path` on `src` and makes it pane `p`'s folder, with `select` selected.
    /// Keeps the old listing (and side) if it fails.
    fn load_select(self, p: Pane, src: Src, path: String, select: Option<String>) {
        self.load_or(p, src, path, select, None);
    }

    /// Like `load_select`, but if `path` can't be listed, lists `fallback` instead.
    fn load_or(self, p: Pane, src: Src, path: String, select: Option<String>, fallback: Option<String>) {
        p.loading.set(true);
        spawn_local(async move {
            match call::<Vec<Entry>>("list", json!({ "src": src.key(), "path": path, "pane": p.id })).await {
                Ok(entries) => {
                    let mut entries = self.visible(entries);
                    p.sort.get_untracked().apply(&mut entries);
                    let new_src = p.src.get_untracked() != src;
                    if new_src {
                        p.cols.set(HashMap::new());
                        p.src.set(src);
                    }
                    if new_src || !self.space.with_untracked(|m| m.contains_key(&src.key())) {
                        self.load_space(src);
                    }
                    let reroot = std::mem::take(&mut *p.reroot.write_value());
                    if reroot || new_src || !within(&path, &p.root.get_untracked()) {
                        p.root.set(path.clone());
                    }
                    p.cols.update(|c| {
                        c.insert(path.clone(), entries.clone());
                    });
                    p.last.update_value(|l| {
                        l.insert(src, path.clone());
                    });
                    store(&self.last_key(p, src), &path);
                    if p.cwd.get_untracked() != path || new_src {
                        p.marked.set(vec![]);
                        p.filter.set(String::new());
                    }
                    p.found.set(None);
                    p.cwd.set(path);
                    p.all.set(entries);
                    p.arrange();
                    p.selected.set(select);
                    if matches!(p.notice.get_untracked(), Notice::Error(_)) {
                        p.notice.set(Notice::None);
                    }
                }
                Err(_) if fallback.as_ref().is_some_and(|f| *f != path) => {
                    self.load_or(p, src, fallback.unwrap_or_default(), None, None);
                }
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    /// Where the pane's last folder on `src` is kept across launches.
    fn last_key(self, p: Pane, src: Src) -> String {
        format!("yomi-files.last.{}.{}", p.id, self.place_key(src))
    }

    /// "mac", or a device's transport and name ("adb:Pixel 7"), which stay the same across connections.
    fn place_key(self, src: Src) -> String {
        match src {
            Src::Mac => "mac".into(),
            _ => self.src_info(src).map_or_else(String::new, |i| format!("{}:{}", i.transport, i.label)),
        }
    }

    /// Shows `src` in pane `p`, in the folder the pane last showed there (this session or before),
    /// or its default folder.
    fn open_remembered(self, p: Pane, src: Src) {
        let Some(info) = self.src_info(src) else { return };
        let path = p.last.with_value(|l| l.get(&src).cloned()).or_else(|| stored(&self.last_key(p, src)));
        match path {
            Some(path) => self.load_or(p, src, path, None, Some(info.default_dir)),
            None => self.load_select(p, src, info.default_dir, None),
        }
    }

    /// Searches the pane's folder and its subfolders for names containing the filter's text.
    fn find(self, p: Pane) {
        let query = p.filter.get_untracked().trim().to_string();
        if query.is_empty() {
            return;
        }
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        p.loading.set(true);
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Matches {
                entries: Vec<Entry>,
                complete: bool,
            }
            let args = json!({ "src": src.key(), "dir": dir, "query": query, "hidden": self.show_hidden.get_untracked() });
            match call::<Matches>("find", args).await {
                Ok(m) if p.cwd.get_untracked() == dir && p.src.get_untracked() == src => {
                    p.all.set(m.entries);
                    p.filter.set(String::new());
                    p.found.set(Some((query, m.complete)));
                    p.select_only(None);
                    p.arrange();
                }
                Ok(_) => {}
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    fn quick_look(self, p: Pane) {
        let picked = p.picked();
        let [e] = picked.as_slice() else { return };
        if e.dir {
            return;
        }
        let e = e.clone();
        let src = p.src.get_untracked();
        spawn_local(async move {
            if let Err(err) = call::<()>("quick_look", json!({ "src": src.key(), "path": e.path })).await {
                p.error(err);
            }
        });
    }

    /// Closes the open window; the reader goes back to the library it came from.
    fn close_panel(self) {
        match self.panel.get_untracked() {
            Some(Panel::Reader(src, _, Some(dir))) => self.panel.set(Some(Panel::Library(src, dir))),
            _ => self.panel.set(None),
        }
    }

    /// Checks one book and says what's wrong with it.
    fn check_book(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        p.notice.set(Notice::Ok(format!("Checking {}…", e.name)));
        spawn_local(async move {
            match call::<Vec<Issue>>("book_check", json!({ "src": src.key(), "path": e.path })).await {
                Ok(is) if is.is_empty() => p.notice.set(Notice::Ok(format!("{}: no problems found.", e.name))),
                Ok(is) => {
                    let serious = is.iter().any(|i| i.serious);
                    let mut lines = vec![format!("{}: {}", e.name, if serious { "readers may refuse or misshow this book." } else { "only minor issues." })];
                    lines.extend(is.into_iter().map(|i| format!("{} {}", if i.serious { "•" } else { "◦" }, i.text)));
                    let msg = lines.join("\n");
                    p.notice.set(if serious { Notice::Error(msg) } else { Notice::Ok(msg) });
                }
                Err(err) => p.error(err),
            }
        });
    }

    /// Notes an action in the session's history.
    fn log(self, what: impl Into<String>) {
        let d = js_sys::Date::new_0();
        let at = format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds());
        self.history.update(|h| h.push((at, what.into())));
    }

    /// Asks how much space is free on `src`'s storages.
    fn load_space(self, src: Src) {
        spawn_local(async move {
            if let Ok(s) = call::<Vec<Space>>("space", json!({ "src": src.key() })).await {
                self.space.update(|m| {
                    m.insert(src.key(), s);
                });
            }
        });
    }

    /// Free space on the storage holding the pane's folder, e.g. "12.3 GB free of 64 GB".
    fn free_text(self, p: Pane) -> Option<String> {
        let (src, cwd) = (p.src.get(), p.cwd.get());
        let transport = self.src_info(src).map(|i| i.transport).unwrap_or_default();
        self.space.with(|m| {
            let list = m.get(&src.key())?;
            let s = match (src, transport.as_str()) {
                (Src::Mac, _) => {
                    let disk = cwd.strip_prefix("/Volumes/").map(|r| r.split('/').next().unwrap_or_default().to_string());
                    list.iter().find(|s| Some(&s.name) == disk.as_ref()).or_else(|| list.iter().find(|s| s.name == "Mac"))
                }
                (_, "mtp") => {
                    let first = cwd.trim_start_matches('/').split('/').next().unwrap_or_default();
                    list.iter().find(|s| s.name == first)
                }
                _ => list.first(),
            }?;
            Some(format!("{} free of {}", human_size(s.free), human_size(s.total)))
        })
    }

    /// Ejects a disk. Panes showing it go back to the home folder first.
    fn eject(self, path: String) {
        if self.job.with_untracked(|j| j.is_some()) {
            self.act().error("Wait for the copy to finish before ejecting.");
            return;
        }
        for p in self.panes {
            if p.src.get_untracked() == Src::Mac && within(&p.cwd.get_untracked(), &path) {
                if let Some(home) = self.mac.get_untracked().map(|i| i.default_dir) {
                    self.load_select(p, Src::Mac, home, None);
                }
            }
        }
        spawn_local(async move {
            match call::<()>("eject", json!({ "path": path })).await {
                Ok(()) => self.log(format!("Ejected {}", base(&path))),
                Err(e) => self.act().error(e),
            }
            self.scan();
        });
    }

    fn set_favs(self, favs: Vec<Fav>) {
        store(FAVS_KEY, &serde_json::to_string(&favs).unwrap_or_default());
        self.favs.set(favs);
    }

    /// Adds a folder to Favourites (or removes it, if it's there).
    fn toggle_fav(self, src: Src, path: String) {
        let place = self.place_key(src);
        let mut favs = self.favs.get_untracked();
        if let Some(i) = favs.iter().position(|f| f.place == place && f.path == path) {
            favs.remove(i);
        } else {
            let label = self.src_info(src).map_or_else(String::new, |i| i.label);
            favs.push(Fav { place, label, path });
        }
        self.set_favs(favs);
    }

    fn is_fav(self, src: Src, path: &str) -> bool {
        let place = self.place_key(src);
        self.favs.with_untracked(|f| f.iter().any(|f| f.place == place && f.path == path))
    }

    /// The connected device (or the Mac) a favourite is on.
    fn fav_src(self, f: &Fav) -> Option<Src> {
        if f.place == "mac" {
            return Some(Src::Mac);
        }
        self.phones.with(|ps| ps.iter().find(|i| format!("{}:{}", i.transport, i.label) == f.place).map(|i| Src::of(&i.id)))
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
        self.load_space(src);
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
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return;
            }
            self.editor.set(None);
        }
        self.open_remembered(p, src);
    }

    /// Opens a sidebar place in the active pane; the column view starts from it.
    fn open_place(self, src: Src, path: String) {
        let p = self.act();
        p.reroot.set_value(true);
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
        if v == ViewMode::Columns && p.view.get_untracked() != v {
            p.root.set(p.cwd.get_untracked());
        }
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

    /// Shows the pane's starting folder on its side (the Mac when there's no phone).
    fn start(self, p: Pane) {
        let mut src = p.src.get_untracked();
        if self.src_info(src).is_none() {
            src = Src::Mac;
            p.src.set(src);
        }
        self.open_remembered(p, src);
    }

    /// New files and folders can't go in "/" over MTP: it lists the device's storages.
    fn in_storage(self, p: Pane) -> bool {
        p.cwd.get() != "/" || self.src_info(p.src.get()).is_some_and(|i| i.transport != "mtp")
    }

    fn show_menu(self, p: Pane, ev: &web_sys::MouseEvent, entry: Option<Entry>) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        if let Some(e) = &entry {
            if !p.picked().iter().any(|x| x.path == e.path) {
                p.select_only(Some(e.path.clone()));
            }
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

    /// "Copy to the Mac", "Move 3 items to <folder>" (when both panes are on the same side).
    fn to_label(self, p: Pane, verb: &str, n: usize) -> String {
        let o = self.other(p);
        let (from, to) = (p.src.get_untracked(), o.src.get_untracked());
        let what = if n > 1 { format!(" {n} items") } else { String::new() };
        if from != to {
            format!("{verb}{what} to {}", self.place(to).trim_start_matches("the "))
        } else {
            format!("{verb}{what} to “{}”", base(&o.cwd.get_untracked()))
        }
    }

    fn menu_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
        let device = p.src.get_untracked() != Src::Mac;
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
            let fav = if self.is_fav(p.src.get_untracked(), &p.cwd.get_untracked()) { "Remove this folder from Favourites" } else { "Add this folder to Favourites" };
            v.push(MenuItem::Do {
                label: "Books in this folder…".into(),
                key: "",
                act: Act::Library,
                danger: false,
                enabled: !p.cwd.get_untracked().is_empty(),
            });
            v.extend([MenuItem::Sep, item(fav, "", Act::Favourite), item("Refresh", "", Act::Refresh)]);
            v.push(MenuItem::Do {
                label: "Sync to the other pane…".into(),
                key: "",
                act: Act::Sync,
                danger: false,
                enabled: split && !p.cwd.get_untracked().is_empty(),
            });
            v.push(MenuItem::Sep);
            let now = p.view.get_untracked();
            v.extend(ViewMode::ALL.map(|m| {
                item(format!("{} as {}", if m == now { "✓" } else { "\u{2003}" }, m.label()), "", Act::View(m))
            }));
            v.push(MenuItem::Sep);
            v.push(item(if split { "Hide second pane" } else { "Show second pane" }, "⌘\\", Act::Split));
            return v;
        };
        let picked = p.picked().len();
        if picked > 1 {
            let mut v = vec![];
            if split {
                let enabled = self.in_storage(self.other(p));
                v.push(MenuItem::Do { label: self.to_label(p, "Copy", picked), key: "F5", act: Act::Copy, danger: false, enabled });
                v.push(MenuItem::Do { label: self.to_label(p, "Move", picked), key: "F6", act: Act::Move, danger: false, enabled });
                v.push(MenuItem::Sep);
            }
            v.push(MenuItem::Do { label: format!("Delete {picked} items…"), key: "⌘⌫", act: Act::Delete, danger: true, enabled: true });
            return v;
        }
        let mut v = vec![];
        let open = match kind(e) {
            Kind::Folder => "Open",
            Kind::Epub => "Open in EPUB editor",
            Kind::Text => "Edit",
            Kind::Other => "Open",
        };
        if kind(e) == Kind::Epub {
            v.push(item("Read", "", Act::Read));
        }
        v.push(item(open, "↵", Act::Open));
        if kind(e) == Kind::Epub {
            v.push(item("Check book", "", Act::Check));
        }
        if !e.dir {
            v.push(item("Quick Look", "Space", Act::QuickLook));
        }
        if !e.dir && kind(e) != Kind::Other {
            v.push(item("Open in default app", "", Act::OpenInApp));
        }
        if device && !e.dir {
            v.push(item("Download to Mac…", "", Act::Download));
        }
        if split {
            let enabled = self.in_storage(self.other(p));
            v.push(MenuItem::Do { label: self.to_label(p, "Copy", 1), key: "F5", act: Act::Copy, danger: false, enabled });
            v.push(MenuItem::Do { label: self.to_label(p, "Move", 1), key: "F6", act: Act::Move, danger: false, enabled });
        }
        let renamable = !(p.cwd.get_untracked() == "/" && self.src_info(p.src.get_untracked()).is_some_and(|i| i.transport == "mtp"));
        v.push(MenuItem::Do { label: "Rename…".into(), key: "F2", act: Act::Rename, danger: false, enabled: renamable });
        if e.dir {
            let fav = if self.is_fav(p.src.get_untracked(), &e.path) { "Remove from Favourites" } else { "Add to Favourites" };
            v.push(item(fav, "", Act::Favourite));
        }
        v.extend([MenuItem::Sep, item("Refresh", "", Act::Refresh), MenuItem::Sep]);
        let label = if e.dir { "Delete folder if empty…" } else { "Delete…" };
        v.push(MenuItem::Do { label: label.into(), key: "⌘⌫", act: Act::Delete, danger: true, enabled: true });
        v
    }

    fn run(self, p: Pane, act: Act, entry: Option<Entry>) {
        match (act, entry) {
            (Act::Open, Some(e)) => self.open(p, e),
            (Act::OpenInApp, Some(e)) => self.open_in_app(p, e.path),
            (Act::Download, Some(e)) => self.download(p, e.path),
            (Act::Copy, Some(_)) => self.send_to_other(p, false),
            (Act::Move, Some(_)) => self.send_to_other(p, true),
            (Act::Rename, Some(e)) => p.notice.set(Notice::Rename(e)),
            (Act::QuickLook, Some(_)) => self.quick_look(p),
            (Act::Favourite, Some(e)) => self.toggle_fav(p.src.get_untracked(), e.path),
            (Act::Favourite, None) => self.toggle_fav(p.src.get_untracked(), p.cwd.get_untracked()),
            (Act::Delete, Some(_)) => self.ask_delete(p),
            (Act::NewFolder, _) => p.notice.set(Notice::NewItem { folder: true }),
            (Act::NewFile, _) => p.notice.set(Notice::NewItem { folder: false }),
            (Act::Upload, _) => self.upload(p),
            (Act::Sync, _) => self.compare(p),
            (Act::Read, Some(e)) => self.panel.set(Some(Panel::Reader(p.src.get_untracked(), e.path, None))),
            (Act::Check, Some(e)) => self.check_book(p, e),
            (Act::Library, _) => self.panel.set(Some(Panel::Library(p.src.get_untracked(), p.cwd.get_untracked()))),
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
                    if let Err(err) = ep.open(&src.key(), self.place(src), e.path).await {
                        p.error(err);
                    }
                    p.loading.set(false);
                });
            }
            Kind::Text => self.open_text(p, e.path),
            Kind::Other => self.open_in_app(p, e.path),
        }
    }

    /// Opens a file in its default Mac app. A phone file is copied to the Mac first; that copy is
    /// what the app sees, so changes to it don't go back to the phone.
    fn open_in_app(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        p.loading.set(true);
        spawn_local(async move {
            match call::<String>("open_file", json!({ "src": src.key(), "path": path })).await {
                Ok(_) if src != Src::Mac => {
                    p.notice.set(Notice::Ok(format!("Opened a copy of {}. Changes to it stay on the Mac.", base(&path))))
                }
                Ok(_) => {}
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
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
                    self.status.set((format!("Saved to {}.{note}", self.place(ed.src)), "ok"));
                    self.log(format!("Saved {} on {}", base(&ed.path), self.place(ed.src)));
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
        let src = p.src.get_untracked();
        spawn_local(async move {
            match call::<Option<String>>("download", json!({ "src": src.key(), "path": path })).await {
                Ok(Some(dest)) => p.notice.set(Notice::Ok(format!("Saved to {dest}"))),
                Ok(None) => {}
                Err(e) => p.error(e),
            }
        });
    }

    fn upload(self, p: Pane) {
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        self.busy.set(true);
        spawn_local(async move {
            match call::<Vec<String>>("upload", json!({ "src": src.key(), "dir": dir })).await {
                Ok(lines) if lines.is_empty() => {}
                Ok(lines) => {
                    p.notice.set(Notice::Ok(lines.join("\n")));
                    self.changed(src, &dir);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    /// Copies or moves the picked items into the other pane's folder.
    fn send_to_other(self, p: Pane, moving: bool) {
        let items: Vec<String> = p.picked().into_iter().map(|e| e.path).collect();
        if !self.split.get_untracked() || items.is_empty() {
            return;
        }
        let o = self.other(p);
        let (from_src, to_src, to_dir) = (p.src.get_untracked(), o.src.get_untracked(), o.cwd.get_untracked());
        if to_src == from_src && to_dir == p.cwd.get_untracked() {
            p.error("Both panes show this folder. Open another folder in the other pane first.");
            return;
        }
        self.transfer(p, from_src, items, to_src, to_dir, moving);
    }

    /// Copies (or moves) files and folders into `to_dir`, with progress below the panes.
    /// Files already there are skipped, never replaced. The report shows in pane `p`.
    fn transfer(self, p: Pane, from_src: Src, items: Vec<String>, to_src: Src, to_dir: String, moving: bool) {
        if self.job.with_untracked(|j| j.is_some()) {
            p.error("Another copy is running. Wait for it to finish, or cancel it.");
            return;
        }
        if to_dir == "/" && self.src_info(to_src).is_some_and(|i| i.transport == "mtp") {
            p.error("Open a storage first: files can't go at the top level over MTP.");
            return;
        }
        let id = self.next_job.get_value();
        self.next_job.set_value(id + 1);
        self.job.set(Some(Job { id, moving, progress: None }));
        p.notice.set(Notice::None);
        let froms: Vec<String> = items.iter().map(|x| parent(x)).collect();
        spawn_local(async move {
            let args = json!({
                "job": id, "from_src": from_src.key(), "paths": items, "to_src": to_src.key(), "to_dir": to_dir,
                "mode": if moving { "move" } else { "copy" },
            });
            match call::<TransferReport>("transfer", args).await {
                Ok(r) => {
                    let n = |c: u32| format!("{c} {}", if c == 1 { "file" } else { "files" });
                    let verb = if moving { "Moved" } else { "Copied" };
                    let mut lines = vec![format!("{verb} {} to {}.", n(r.copied), to_dir)];
                    if r.cancelled {
                        lines.push("Cancelled: the rest wasn't copied.".into());
                    }
                    if moving && !r.backups.is_empty() {
                        lines.push("The originals are backed up on the Mac.".into());
                    }
                    let shown = 5;
                    for (name, why) in r.skipped.iter().take(shown) {
                        lines.push(format!("Skipped {name}: {why}"));
                    }
                    if r.skipped.len() > shown {
                        lines.push(format!("…and {} more skipped.", r.skipped.len() - shown));
                    }
                    self.log(lines[0].clone());
                    let msg = lines.join("\n");
                    p.notice.set(if r.skipped.is_empty() { Notice::Ok(msg) } else { Notice::Error(msg) });
                }
                Err(e) => p.error(e),
            }
            self.job.set(None);
            self.changed(to_src, &to_dir);
            if moving {
                let mut seen = vec![];
                for d in froms {
                    if !seen.contains(&d) {
                        self.changed(from_src, &d);
                        seen.push(d);
                    }
                }
            }
        });
    }

    /// Compares this pane's folder with the other pane's, and asks before copying what's missing.
    fn compare(self, p: Pane) {
        let o = self.other(p);
        let from = (p.src.get_untracked(), p.cwd.get_untracked());
        let to = (o.src.get_untracked(), o.cwd.get_untracked());
        if to.1 == "/" && self.src_info(to.0).is_some_and(|i| i.transport == "mtp") {
            p.error("Open a storage in the other pane first: files can't go at the top level over MTP.");
            return;
        }
        p.notice.set(Notice::Ok("Comparing the two folders…".into()));
        let hidden = self.show_hidden.get_untracked();
        spawn_local(async move {
            let args = json!({ "from_src": from.0.key(), "from_dir": from.1, "to_src": to.0.key(), "to_dir": to.1, "hidden": hidden });
            match call::<CompareReport>("compare", args).await {
                Ok(report) => p.notice.set(Notice::Sync { report, from, to }),
                Err(e) => p.error(e),
            }
        });
    }

    /// Copies what `compare` found missing. Runs as a transfer, with progress and Cancel.
    fn sync(self, p: Pane, from: (Src, String), to: (Src, String)) {
        if self.job.with_untracked(|j| j.is_some()) {
            p.error("Another copy is running. Wait for it to finish, or cancel it.");
            return;
        }
        let id = self.next_job.get_value();
        self.next_job.set_value(id + 1);
        self.job.set(Some(Job { id, moving: false, progress: None }));
        p.notice.set(Notice::None);
        let hidden = self.show_hidden.get_untracked();
        spawn_local(async move {
            let args = json!({
                "job": id, "from_src": from.0.key(), "from_dir": from.1, "to_src": to.0.key(), "to_dir": to.1, "hidden": hidden,
            });
            match call::<TransferReport>("sync", args).await {
                Ok(r) => {
                    let mut lines = vec![format!("Synced {} into {}: copied {} {}.", base(&from.1), to.1, r.copied, if r.copied == 1 { "file" } else { "files" })];
                    if r.cancelled {
                        lines.push("Cancelled: the rest wasn't copied.".into());
                    }
                    for (name, why) in r.skipped.iter().take(5) {
                        lines.push(format!("Skipped {name}: {why}"));
                    }
                    if r.skipped.len() > 5 {
                        lines.push(format!("…and {} more skipped.", r.skipped.len() - 5));
                    }
                    self.log(lines[0].clone());
                    let msg = lines.join("\n");
                    p.notice.set(if r.skipped.is_empty() { Notice::Ok(msg) } else { Notice::Error(msg) });
                }
                Err(e) => p.error(e),
            }
            self.job.set(None);
            self.changed(to.0, &to.1);
        });
    }

    fn cancel_transfer(self) {
        if let Some(j) = self.job.get_untracked() {
            spawn_local(async move {
                let _ = call::<()>("cancel_transfer", json!({ "job": j.id })).await;
            });
        }
    }

    fn ask_delete(self, p: Pane) {
        let items = p.picked();
        if !items.is_empty() {
            p.notice.set(Notice::ConfirmDelete(items));
        }
    }

    /// Deletes each item in turn (folders only when empty), stopping at the first error.
    fn delete(self, p: Pane, items: Vec<Entry>) {
        let src = p.src.get_untracked();
        p.notice.set(Notice::None);
        spawn_local(async move {
            let mut done = 0;
            let mut backups = 0;
            let mut error = None;
            for e in &items {
                match call::<Option<String>>("remove", json!({ "src": src.key(), "path": e.path })).await {
                    Ok(b) => {
                        done += 1;
                        backups += b.is_some() as usize;
                    }
                    Err(err) => {
                        error = Some(err);
                        break;
                    }
                }
            }
            let what = if items.len() == 1 { items[0].name.clone() } else { format!("{done} items") };
            let note = if backups > 0 { " Backups are kept on the Mac." } else { "" };
            if done > 0 {
                self.log(format!("Deleted {what} from {}", self.place(src)));
            }
            match error {
                None => p.notice.set(Notice::Ok(format!("Deleted {what}.{note}"))),
                Some(err) if done == 0 => p.error(err),
                Some(err) => p.error(format!("Deleted {done} of {}, then: {err}", items.len())),
            }
            if let Some(e) = items.first() {
                self.changed(src, &parent(&e.path));
            }
        });
    }

    fn rename(self, p: Pane, e: Entry, name: String) {
        let name = name.trim().to_string();
        if name == e.name {
            p.notice.set(Notice::None);
            return;
        }
        let src = p.src.get_untracked();
        spawn_local(async move {
            match call::<String>("rename", json!({ "src": src.key(), "path": e.path, "name": name })).await {
                Ok(new) => {
                    p.notice.set(Notice::None);
                    self.log(format!("Renamed {} to {}", e.name, base(&new)));
                    p.select_only(Some(new));
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
                    self.log(format!("Created {}", path));
                    if folder { self.changed(src, &dir) } else { self.open_text(p, path) }
                }
                Err(e) => p.error(e),
            }
        });
    }

    /// A device is connected. With `show`, the active pane goes to it.
    fn connected(self, info: Info, show: bool) {
        let src = Src::of(&info.id);
        self.phones.update(|ps| {
            if !ps.iter().any(|i| i.id == info.id) {
                ps.push(info);
            }
        });
        self.conn_error.set(None);
        if show {
            self.open_remembered(self.act(), src);
        }
    }

    /// Looks for devices on USB and disks on the Mac, unless the app is busy or a look is under way.
    fn scan(self) {
        if self.busy.get_untracked() || self.scanning.get_value() || self.job.with_untracked(|j| j.is_some()) {
            return;
        }
        self.scanning.set_value(true);
        spawn_local(async move {
            if let Ok(d) = call::<Devices>("devices", json!({})).await {
                let pick = self.auto_pick(&d);
                if self.devices.get_untracked().as_ref() != Some(&d) {
                    self.devices.set(Some(d));
                }
                if let Some((f, show)) = pick {
                    self.connect(f, show);
                }
            }
            self.scanning.set_value(false);
        });
    }

    /// Keys of the connected phones: serial and USB place.
    fn connected_keys(self) -> HashSet<String> {
        self.phones.with_untracked(|ps| ps.iter().flat_map(|i| [i.serial.clone(), i.usb.clone()]).flatten().collect())
    }

    /// The phone to connect automatically, if any, and whether to show it. Phones there when the
    /// app starts connect at once and don't take over the window; one plugged in later waits for a
    /// second look, so adb has time to show up next to MTP, and is shown.
    fn auto_pick(self, d: &Devices) -> Option<(UsbDevice, bool)> {
        let connected = self.connected_keys();
        let now: HashSet<String> = d.found.iter().map(UsbDevice::key).collect();
        let (before, looks) = self.seen.get_value();
        self.seen.set_value((now.clone(), looks + 1));
        // Gone for two looks in a row: unplugged, so plugging it in again connects it again.
        self.tried.update_value(|t| t.retain(|k| now.contains(k) || before.contains(k) || connected.contains(k)));
        if !self.auto_connect.get_untracked() || self.busy.get_untracked() {
            return None;
        }
        let first = looks == 0;
        let f = d
            .found
            .iter()
            .filter(|f| {
                let k = f.key();
                !connected.contains(&k) && !self.tried.with_value(|t| t.contains(&k)) && (first || before.contains(&k))
            })
            // adb first for each phone.
            .min_by_key(|f| (f.key(), f.transport != "adb"))?
            .clone();
        self.tried.update_value(|t| {
            t.insert(f.key());
        });
        Some((f, !first))
    }

    /// Connects a device found on USB; `show` opens it in the active pane.
    fn connect(self, f: UsbDevice, show: bool) {
        self.busy.set(true);
        self.connecting.set(Some(f.usb.clone()));
        self.conn_error.set(None);
        spawn_local(async move {
            match call::<Info>("connect", json!({ "transport": f.transport, "which": f.which, "serial": f.serial })).await {
                Ok(info) => self.connected(info, show),
                Err(e) => self.conn_error.set(Some(e)),
            }
            self.busy.set(false);
            self.connecting.set(None);
            self.scan();
        });
    }

    /// Lets go of a device. Panes showing it go back to the Mac.
    fn disconnect(self, src: Src) {
        self.disconnect_then(src, None);
    }

    /// Connects a connected phone the other way (adb or MTP).
    fn switch(self, src: Src, to: UsbDevice) {
        self.disconnect_then(src, Some(to));
    }

    fn disconnect_then(self, src: Src, then: Option<UsbDevice>) {
        if self.editor.with_untracked(|e| e.as_ref().is_some_and(|e| e.src == src)) && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        spawn_local(async move {
            let _ = call::<()>("disconnect", json!({ "src": src.key() })).await;
            let id = src.key();
            self.phones.update(|ps| ps.retain(|i| i.id != id));
            if self.editor.with_untracked(|e| e.as_ref().is_some_and(|e| e.src == src)) {
                self.editor.set(None);
            }
            for p in self.panes {
                p.last.update_value(|l| {
                    l.remove(&src);
                });
                if p.src.get_untracked() == src {
                    p.entries.set(vec![]);
                    p.cols.set(HashMap::new());
                    p.cwd.set(String::new());
                    p.selected.set(None);
                    p.notice.set(Notice::None);
                    p.src.set(Src::Mac);
                    if self.split.get_untracked() || p.id == 0 {
                        self.start(p);
                    }
                }
            }
            match then {
                Some(f) => self.connect(f, true),
                None => self.scan(),
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

    // Start on the Mac's files. Pick up existing device connections after a webview reload.
    spawn_local(async move {
        if let Ok(Some(mac)) = call::<Option<Info>>("info", json!({ "src": "mac" })).await {
            ctx.mac.set(Some(mac));
        }
        for info in call::<Vec<Info>>("connections", json!({})).await.unwrap_or_default() {
            ctx.connected(info, false);
        }
        ctx.start(ctx.panes[0]);
        if ctx.split.get_untracked() {
            ctx.start(ctx.panes[1]);
        }
    });
    // Look for devices and disks now and every few seconds.
    ctx.scan();
    let _ = set_interval_with_handle(move || ctx.scan(), std::time::Duration::from_secs(3));

    // Keyboard. ⌘S saves. In the panes: Tab switches pane, ⌘\ shows or hides the second one, arrows
    // move (←/→ also go up/in in the column view and move across in the thumbnail view), Enter or
    // ⌘↓ opens, ⌘↑ or Backspace goes up, ⌘⌫ deletes, F2 renames, F5 copies and F6 moves to the
    // other pane, ⌘A picks everything, Escape unpicks.
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
        if ctx.panel.get_untracked().is_some() {
            if e.key() == "Escape" {
                e.prevent_default();
                ctx.close_panel();
            }
            return;
        }
        if ctx.settings.get_untracked() {
            if e.key() == "Escape" {
                e.prevent_default();
                ctx.settings.set(false);
            }
            return;
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
        if !matches!(p.notice.get_untracked(), Notice::None | Notice::Ok(_) | Notice::Error(_)) {
            return;
        }
        // Typing in a field (the filter) is the field's.
        let typing = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest("input, textarea").ok().flatten().is_some());
        if typing {
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
                p.select_only(Some(x.path.clone()));
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
            ("F5", _) | ("F6", _) => {
                e.prevent_default();
                ctx.send_to_other(p, e.key() == "F6");
            }
            ("F2", _) => {
                if let [x] = p.picked().as_slice() {
                    e.prevent_default();
                    p.notice.set(Notice::Rename(x.clone()));
                }
            }
            ("a", true) => {
                e.prevent_default();
                p.marked.set(entries.iter().map(|x| x.path.clone()).collect());
            }
            ("Escape", false) => p.marked.set(vec![]),
            (" ", false) => {
                e.prevent_default();
                ctx.quick_look(p);
            }
            ("f", true) => {
                e.prevent_default();
                if let Some(el) = document().query_selector(".pane.active .filter").ok().flatten() {
                    let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
                }
            }
            ("Backspace", true) => {
                e.prevent_default();
                ctx.ask_delete(p);
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

    crate::api::listen("transfer", move |pr: Progress| {
        ctx.job.update(|j| {
            if let Some(j) = j.as_mut().filter(|j| j.id == pr.job) {
                j.progress = Some(pr);
            }
        });
    });
    // Files dragged in from Finder are copied to the folder (or pane) they're dropped on.
    crate::api::listen("tauri://drag-drop", move |d: FileDrop| {
        let dpr = window().device_pixel_ratio().max(1.0);
        let Some(el) = document().element_from_point((d.position.x / dpr) as f32, (d.position.y / dpr) as f32) else { return };
        let Some(pane_el) = el.closest("[data-pane]").ok().flatten() else { return };
        let Some(p) = pane_el.get_attribute("data-pane").and_then(|i| i.parse::<usize>().ok()).and_then(|i| ctx.panes.get(i).copied()) else { return };
        let dir = el.closest("[data-dir]").ok().flatten().and_then(|f| f.get_attribute("data-dir")).unwrap_or_else(|| p.cwd.get_untracked());
        ctx.active.set(p.id);
        ctx.transfer(p, Src::Mac, d.paths, p.src.get_untracked(), dir, false);
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
        <Browser/>
        <ContextMenu/>
        {move || ctx.settings.get().then(|| view! { <Settings/> })}
        {move || ctx.panel.get().map(|w| match w {
            Panel::Backups => view! { <Backups/> }.into_any(),
            Panel::History => view! { <History/> }.into_any(),
            Panel::Library(src, dir) => view! { <Library src=src dir=dir/> }.into_any(),
            Panel::Reader(src, path, back) => view! { <Reader src=src path=path back=back/> }.into_any(),
        })}
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
                <TransferBar/>
            </div>
        </div>
    }.into_any() }
}

/// Places on the Mac (and its disks) and on the connected devices. Clicking one opens it in the
/// active pane.
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
                on:click=move |_| ctx.open_place(src, path.clone())>{name}</button>
        }
    };
    // A section heading that folds its section away; remembered across launches.
    let section = move |key: &'static str, title: &'static str| {
        let storage_key = format!("yomi-files.fold.{key}");
        let open = RwSignal::new(!stored_flag(&storage_key, false));
        let heading = view! {
            <h2><button class="fold" aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    let now = !open.get_untracked();
                    open.set(now);
                    store(&storage_key, if now { "0" } else { "1" });
                }>
                <span class="chev" class:open=move || open.get()>"▸"</span>{title}
            </button></h2>
        };
        (heading, open)
    };
    let (fav_heading, fav_open) = section("favs", "Favourites");
    let (mac_heading, mac_open) = section("mac", "Mac");
    let (dev_heading, dev_open) = section("devices", "Devices");
    // Disks mounted on the Mac: USB drives, SD cards, disk images.
    let drives = Memo::new(move |_| ctx.devices.with(|d| d.as_ref().map(|d| d.drives.clone()).unwrap_or_default()));
    let device = move |i: Info| {
        let src = Src::of(&i.id);
        view! {
            <div class="device">
                // The device's name shows all its storage ("/").
                <button class="name" title=format!("All storage on {}", i.label)
                    class:active=move || {
                        let pane = ctx.panes[ctx.active.get()];
                        pane.src.get() == src && pane.cwd.get() == "/"
                    }
                    on:click=move |_| ctx.open_place(src, "/".into())>
                    <b>{i.label.clone()}</b>
                    <span class="kind">{i.transport.clone()}</span>
                </button>
                {move || {
                    // The same phone, found the other way: offer to switch.
                    let serial = i.serial.clone()?;
                    let other = ctx.devices.with(|d| d.as_ref()?.found.iter().find(|f| f.serial.as_deref() == Some(serial.as_str())).cloned())?;
                    let to = if other.transport == "adb" { "adb" } else { "MTP" };
                    let title = format!("Connect with {to} instead");
                    Some(view! {
                        <button class="switch" title=title disabled=move || ctx.busy.get()
                            on:click=move |_| ctx.switch(src, other.clone())>{to}</button>
                    })
                }}
                <button class="eject" title=format!("Disconnect {}", i.label) aria-label="Disconnect"
                    on:click=move |_| ctx.disconnect(src)>"⏏"</button>
            </div>
            {i.quick_dirs.into_iter().map(|(name, path)| place(src, name, path)).collect_view()}
        }
    };
    view! {
        <nav class="side">
            {move || (!ctx.favs.with(|f| f.is_empty())).then(|| view! {
                {fav_heading.clone()}
                {move || fav_open.get().then(|| ctx.favs.get().into_iter().map(|f| {
                    let src = ctx.fav_src(&f);
                    let name = base(&f.path);
                    let on = if f.place == "mac" { "the Mac".to_string() } else { f.label.clone() };
                    let tip = match src {
                        Some(_) => format!("{} on {on}", f.path),
                        None => format!("{} on {on}. Connect {on} to open it.", f.path),
                    };
                    let (f1, f2) = (f.clone(), f.clone());
                    let active = move || src.is_some_and(|s| {
                        let pane = ctx.panes[ctx.active.get()];
                        pane.src.get() == s && pane.cwd.get() == f1.path
                    });
                    view! {
                        <div class="fav">
                            <button class="item" class:active=active class:off=src.is_none() title=tip
                                on:click=move |_| if let Some(s) = src { ctx.open_place(s, f2.path.clone()) }>{name}</button>
                            <button class="unfav" title="Remove from Favourites" aria-label="Remove from Favourites"
                                on:click=move |_| ctx.set_favs(ctx.favs.get_untracked().into_iter().filter(|x| *x != f).collect())>"✕"</button>
                        </div>
                    }
                }).collect_view())}
            })}
            {mac_heading}
            {move || mac_open.get().then(|| view! {
                {ctx.mac.get().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                    .map(|(name, path)| place(Src::Mac, name, path)).collect_view()}
                {drives.get().into_iter().map(|(name, path)| {
                    let (tip, at) = (format!("Eject {name}"), path.clone());
                    view! {
                        <div class="line drive">
                            {place(Src::Mac, name, path)}
                            <button class="act" title=tip aria-label="Eject" on:click=move |_| ctx.eject(at.clone())>"⏏"</button>
                        </div>
                    }
                }).collect_view()}
            })}
            {dev_heading}
            {move || dev_open.get().then(|| view! {
                {ctx.phones.get().into_iter().map(device).collect_view()}
                <DeviceFinder/>
            })}
            <span class="grow"></span>
            <div class="side-buttons">
                <button class="btn" title="Files replaced or deleted by the app, which can be put back"
                    on:click=move |_| ctx.panel.set(Some(Panel::Backups))>"Backups"</button>
                <button class="btn" title="What was done since the app started"
                    on:click=move |_| ctx.panel.set(Some(Panel::History))>"History"</button>
            </div>
            <button class="btn" on:click=move |_| ctx.settings.set(true)>"Settings"</button>
        </nav>
    }
}

/// Devices on USB that aren't connected yet, each with a button per way to connect. A phone with
/// both USB debugging and File transfer on shows up once for adb and once for MTP; when exactly one
/// of each is found they're taken to be the same phone and shown as one row.
#[component]
fn DeviceFinder() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || {
        let error = move || ctx.conn_error.get().map(|e| view! { <p class="error">{e}</p> });
        let Some(d) = ctx.devices.get() else {
            return view! { <p class="hint">"Looking for devices…"</p> }.into_any();
        };
        // A connected phone found the other way is offered as a switch on its own row instead.
        let connected = ctx.phones.with(|ps| ps.iter().flat_map(|i| [i.serial.clone(), i.usb.clone()]).flatten().collect::<HashSet<_>>());
        let found: Vec<UsbDevice> = d.found.iter().filter(|f| !connected.contains(&f.key())).cloned().collect();
        let waiting = d.adb_waiting.then(|| view! { <p class="hint">"A phone is waiting: allow USB debugging on it to use adb."</p> });
        if found.is_empty() {
            if !ctx.phones.with(|p| p.is_empty()) {
                return view! { {waiting} {error} }.into_any();
            }
            let mut notes = vec!["No phone found. Plug one in by USB, then allow USB debugging (for adb) or set the USB mode to File transfer (for MTP).".to_string()];
            if !d.adb_found {
                notes.push("adb isn't installed: brew install --cask android-platform-tools".into());
            }
            notes.extend(d.mtp_error.clone());
            return view! {
                {notes.into_iter().map(|n| view! { <p class="hint">{n}</p> }).collect_view()}
                {waiting}
                {error}
            }.into_any();
        }
        // (name, adb, mtp) per row: one row per phone, by serial number.
        let mut rows: Vec<(String, Option<UsbDevice>, Option<UsbDevice>)> = vec![];
        for f in found {
            let row = rows.iter_mut().find(|(_, a, m)| a.iter().chain(m.iter()).any(|g| g.serial.is_some() && g.serial == f.serial));
            let row = match row {
                Some(r) => r,
                None => {
                    rows.push((f.name.clone(), None, None));
                    rows.last_mut().expect("just pushed")
                }
            };
            if f.transport == "adb" {
                row.1 = Some(f);
            } else {
                row.0 = f.name.clone();
                row.2 = Some(f);
            }
        }
        // Without serials, one adb phone and one MTP phone are most likely the same.
        if let [(_, Some(_), None), (_, None, Some(m))] | [(_, None, Some(m)), (_, Some(_), None)] = rows.as_slice() {
            if m.serial.is_none() || rows.iter().any(|r| r.1.as_ref().is_some_and(|a| a.serial.is_none())) {
                let m = m.clone();
                let a = rows.iter().find_map(|r| r.1.clone());
                rows = vec![(m.name.clone(), a, Some(m))];
            }
        }
        let adb_off = if d.adb_found { "Not available: allow USB debugging on the phone" } else { "Not available: adb isn't installed" };
        view! {
            {rows.into_iter().map(|(name, a, m)| {
                let ids: Vec<String> = a.iter().chain(m.iter()).map(|f| f.usb.clone()).collect();
                let connecting = move || ctx.connecting.get().is_some_and(|u| ids.contains(&u));
                let adb_title = if a.is_some() { "Connect with adb (USB debugging): faster, and can create files at the top level" } else { adb_off };
                let mtp_title = if m.is_some() { "Connect with MTP (File transfer). Close Android File Transfer first: only one app can use a device's MTP at a time." } else { "Not available: set the phone's USB mode to File transfer" };
                let (has_a, has_m) = (a.is_some(), m.is_some());
                let tip = name.clone();
                view! {
                    <div class="found">
                        <b title=tip>{name}</b>
                        {move || if connecting() {
                            view! { <span class="hint"><span class="spin"></span>" Connecting…"</span> }.into_any()
                        } else {
                            let (a, m) = (a.clone(), m.clone());
                            view! {
                                <div class="seg connect-as" role="group" aria-label="Connect with">
                                    <button title=adb_title disabled=move || !has_a || ctx.busy.get() class:pref=has_a
                                        on:click=move |_| if let Some(f) = a.clone() { ctx.connect(f, true) }>"adb"</button>
                                    <button title=mtp_title disabled=move || !has_m || ctx.busy.get() class:pref=has_m && !has_a
                                        on:click=move |_| if let Some(f) = m.clone() { ctx.connect(f, true) }>"MTP"</button>
                                </div>
                            }.into_any()
                        }}
                    </div>
                }
            }).collect_view()}
            {waiting}
            {error}
        }.into_any()
    }
}

#[derive(Clone, serde::Deserialize)]
struct Folder {
    path: String,
    files: u64,
    bytes: u64,
}

/// Settings, and the folders the app keeps on the Mac.
#[component]
fn Settings() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let folders = RwSignal::new(None::<Result<HashMap<String, Folder>, String>>);
    let load = move || spawn_local(async move { folders.set(Some(call("app_folders", json!({})).await)) });
    load();
    let error = RwSignal::new(None::<String>);
    let open = move |which: &'static str| {
        spawn_local(async move {
            if let Err(e) = call::<()>("open_folder", json!({ "which": which })).await {
                error.set(Some(e));
            }
        })
    };
    let clear = move |which: &'static str| {
        spawn_local(async move {
            match call::<()>("clear_cache", json!({ "which": which })).await {
                Ok(()) => load(),
                Err(e) => error.set(Some(e)),
            }
        })
    };
    let folder = move |which: &'static str, title: &'static str, about: &'static str| {
        view! {
            <div class="folder">
                <div class="what">
                    <b>{title}</b>
                    <span class="about">{about}</span>
                    <span class="path">{move || match folders.get() {
                        None => "…".to_string(),
                        Some(Err(e)) => e,
                        Some(Ok(f)) => f.get(which).map(|f| format!("{} · {} files, {}", f.path, f.files, human_size(f.bytes))).unwrap_or_default(),
                    }}</span>
                </div>
                <button class="btn" on:click=move |_| open(which)>"Open"</button>
                {(which != "backups").then(|| view! { <button class="btn" on:click=move |_| clear(which)>"Clear"</button> })}
            </div>
        }
    };
    view! {
        <div class="overlay" on:click=move |_| ctx.settings.set(false)>
            <div class="dialog" on:click=|e| e.stop_propagation()>
                <h2>"Settings"</h2>
                <label class="check">
                    <input type="checkbox" prop:checked=move || ctx.show_hidden.get()
                        on:change=move |e| ctx.set_show_hidden(event_target_checked(&e))/>
                    "Show hidden files (names starting with a dot)"
                </label>
                <label class="check">
                    <input type="checkbox" prop:checked=move || ctx.phone_thumbs.get()
                        on:change=move |e| ctx.set_phone_thumbs(event_target_checked(&e))/>
                    "Thumbnails of phone files"
                </label>
                <p class="hint">"Each phone thumbnail reads the whole image or EPUB over USB, once; then it's cached on the Mac. Mac files always get thumbnails."</p>
                <label class="check">
                    <input type="checkbox" prop:checked=move || ctx.auto_connect.get()
                        on:change=move |e| ctx.set_auto_connect(event_target_checked(&e))/>
                    "Connect phones when they're plugged in (adb if allowed, else MTP)"
                </label>
                <h3>"On this Mac"</h3>
                {folder("backups", "Backups", "Copies of files from before they were replaced or deleted. Never removed by the app.")}
                {folder("thumbs", "Thumbnail cache", "Made again when needed, so it's safe to clear.")}
                {folder("opened", "Opened phone files", "Copies made to open phone files in Mac apps. Changes to them don't go back to the phone.")}
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="actions">
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| ctx.settings.set(false)>"Done"</button>
                </div>
            </div>
        </div>
    }
}

/// The backups the app has made, newest first, each with Restore (to where it came from, when that
/// device is connected) and Restore here (into the active pane's folder).
#[component]
fn Backups() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let list = RwSignal::new(None::<Result<Vec<Backup>, String>>);
    let load = move || spawn_local(async move { list.set(Some(call("backups", json!({})).await)) });
    load();
    let message = RwSignal::new(None::<(String, bool)>);
    let filter = RwSignal::new(String::new());
    let restore = move |b: Backup, src: Src, path: String| {
        spawn_local(async move {
            match call::<WriteResult>("restore", json!({ "file": b.file, "src": src.key(), "path": path })).await {
                Ok(r) => {
                    let note = if r.backup.is_some() { " The file that was there is backed up too." } else { "" };
                    message.set(Some((format!("Restored {} to {path} on {}.{note}", b.name, ctx.place(src)), true)));
                    ctx.log(format!("Restored {} to {path}", b.name));
                    ctx.changed(src, &parent(&path));
                    load();
                }
                Err(e) => message.set(Some((e, false))),
            }
        });
    };
    let close = move || ctx.panel.set(None);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog wide" on:click=|e| e.stop_propagation()>
                <h2>"Backups"</h2>
                <p class="hint">"Copies of files from before the app replaced or deleted them. Restoring puts a copy back; a file already there is backed up first."</p>
                <input class="filter-wide" type="search" placeholder="Filter by name" prop:value=move || filter.get()
                    on:input=move |ev| filter.set(event_target_value(&ev))/>
                {move || message.get().map(|(m, ok)| view! { <p class:ok=ok class:error=!ok>{m}</p> })}
                <div class="backups">
                    {move || match list.get() {
                        None => view! { <p class="hint">"Loading…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok(bs)) if bs.is_empty() => view! { <p class="hint">"No backups yet."</p> }.into_any(),
                        Some(Ok(bs)) => {
                            let f = filter.get().to_lowercase();
                            bs.into_iter().filter(|b| b.name.to_lowercase().contains(&f)).map(|b| {
                                let origin = b.note.as_ref().map(|n| {
                                    let on = if n.place == "mac" { "the Mac".to_string() } else { n.place.split_once(':').map_or(n.place.clone(), |(_, l)| l.to_string()) };
                                    format!("{} {} on {on}", if n.reason == "deleted" { "Deleted from" } else { "Replaced at" }, n.path)
                                }).unwrap_or_else(|| "Where it came from wasn't noted (an older backup).".into());
                                let back = b.note.as_ref().and_then(|n| ctx.fav_src(&Fav { place: n.place.clone(), label: String::new(), path: n.path.clone() }).map(|s| (s, n.path.clone())));
                                let back_tip = if back.is_some() { "Put it back where it was".to_string() } else if b.note.is_some() { "Connect that device to put it back where it was".into() } else { "Where it came from isn't known".into() };
                                let (b1, b2) = (b.clone(), b.clone());
                                let here_tip = "Restore into the folder open in the active pane";
                                view! {
                                    <div class="backup">
                                        <div class="what">
                                            <b>{b.name.clone()}</b>
                                            <span class="about">{format!("{} · {}", when(b.mtime), human_size(b.size))}</span>
                                            <span class="path">{origin}</span>
                                        </div>
                                        <button class="btn" title=back_tip disabled=back.is_none()
                                            on:click=move |_| if let Some((s, path)) = back.clone() { restore(b1.clone(), s, path) }>"Restore"</button>
                                        <button class="btn" title=here_tip on:click=move |_| {
                                            let p = ctx.act();
                                            let dir = p.cwd.get_untracked();
                                            if dir.is_empty() { return }
                                            restore(b2.clone(), p.src.get_untracked(), join(&dir, &b2.name));
                                        }>"Restore here"</button>
                                    </div>
                                }
                            }).collect_view().into_any()
                        }
                    }}
                </div>
                <div class="actions">
                    <button class="btn" on:click=move |_| { let _ = spawn_local(async move { let _ = call::<()>("open_folder", json!({ "which": "backups" })).await; }); }>"Open in Finder"</button>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}

/// What was done since the app started.
#[component]
fn History() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let close = move || ctx.panel.set(None);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog wide" on:click=|e| e.stop_propagation()>
                <h2>"History"</h2>
                <p class="hint">"What was done since the app started. Replaced and deleted files can be put back from Backups."</p>
                <div class="history">
                    {move || {
                        let h = ctx.history.get();
                        if h.is_empty() {
                            return view! { <p class="hint">"Nothing yet."</p> }.into_any();
                        }
                        h.into_iter().rev().map(|(at, what)| view! {
                            <div class="event"><span class="at">{at}</span><span>{what}</span></div>
                        }).collect_view().into_any()
                    }}
                </div>
                <div class="actions">
                    <button class="btn" on:click=move |_| ctx.panel.set(Some(Panel::Backups))>"Backups…"</button>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}

/// Runs `f` on each book in turn, showing "Checking 3 of 10: name" in `busy`, until done or stopped.
async fn for_each_book<F, Fut>(busy: RwSignal<Option<String>>, stop: StoredValue<bool>, verb: &str, books: Vec<Entry>, f: F) -> usize
where
    F: Fn(Entry) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    stop.set_value(false);
    let n = books.len();
    let mut done = 0;
    for (k, e) in books.into_iter().enumerate() {
        if stop.try_get_value().unwrap_or(true) {
            break;
        }
        busy.try_set(Some(format!("{verb} {} of {n}: {}", k + 1, e.name)));
        f(e).await;
        done += 1;
    }
    busy.try_set(None);
    done
}

/// Splits "Ann Smith; Bob Jones" into names.
fn names(s: &str) -> Vec<String> {
    s.split(';').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum BookForm {
    Info,
    Shrink,
}

/// Every EPUB under a folder, with covers and info, and work on many at once: check, change info,
/// make smaller copies.
#[component]
fn Library(src: Src, dir: String) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let dir = StoredValue::new(dir);
    let list = RwSignal::new(None::<Result<(Vec<Entry>, bool), String>>);
    let infos = RwSignal::new(HashMap::<String, BookInfo>::new());
    let notes = RwSignal::new(HashMap::<String, (String, bool)>::new());
    let issues = RwSignal::new(HashMap::<String, Vec<Issue>>::new());
    let shown_issues = RwSignal::new(None::<String>);
    let picked = RwSignal::new(std::collections::HashSet::<String>::new());
    let filter = RwSignal::new(String::new());
    let busy = RwSignal::new(None::<String>);
    let stop = StoredValue::new(false);
    let form = RwSignal::new(None::<BookForm>);
    let message = RwSignal::new(None::<(String, bool)>);
    let (authors, language, publisher, subjects) = (RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()), RwSignal::new(String::new()));
    let (side, quality) = (RwSignal::new("1600".to_string()), RwSignal::new("80".to_string()));

    let load = move || {
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Matches {
                entries: Vec<Entry>,
                complete: bool,
            }
            let args = json!({ "src": src.key(), "dir": dir.get_value(), "hidden": ctx.show_hidden.get_untracked() });
            match call::<Matches>("library", args).await {
                Ok(m) => {
                    list.try_set(Some(Ok((m.entries.clone(), m.complete))));
                    for e in m.entries {
                        if stop.try_get_value().is_none() {
                            return;
                        }
                        if infos.try_with_untracked(|i| i.contains_key(&e.path)).unwrap_or(true) {
                            continue;
                        }
                        let args = json!({ "src": src.key(), "path": e.path, "size": e.size, "mtime": e.mtime });
                        let info = call::<BookInfo>("book_info", args).await.unwrap_or_else(|err| BookInfo { problem: Some(err), ..Default::default() });
                        infos.try_update(|i| {
                            i.insert(e.path.clone(), info);
                        });
                    }
                }
                Err(e) => {
                    list.try_set(Some(Err(e)));
                }
            }
        })
    };
    load();

    let shown = move || -> Vec<Entry> {
        let f = filter.get().to_lowercase();
        let Some(Ok((all, _))) = list.get() else { return vec![] };
        infos.with(|i| {
            all.into_iter()
                .filter(|e| {
                    f.is_empty()
                        || e.name.to_lowercase().contains(&f)
                        || i.get(&e.path).is_some_and(|b| b.title.to_lowercase().contains(&f) || b.creators.iter().any(|c| c.to_lowercase().contains(&f)))
                })
                .collect()
        })
    };
    let targets = move || -> Vec<Entry> {
        let p = picked.get_untracked();
        list.get_untracked().and_then(|l| l.ok()).map(|(all, _)| all.into_iter().filter(|e| p.contains(&e.path)).collect()).unwrap_or_default()
    };
    let n_picked = move || picked.with(|p| p.len());
    let note = move |path: &str, text: String, ok: bool| {
        notes.try_update(|n| {
            n.insert(path.to_string(), (text, ok));
        });
    };

    let check = move || {
        let books = targets();
        message.set(None);
        spawn_local(async move {
            let n = for_each_book(busy, stop, "Checking", books, |e| async move {
                match call::<Vec<Issue>>("book_check", json!({ "src": src.key(), "path": e.path })).await {
                    Ok(is) => {
                        let serious = is.iter().filter(|i| i.serious).count();
                        let text = match (serious, is.len()) {
                            (_, 0) => "No problems found.".to_string(),
                            (0, n) => format!("{n} minor {}.", if n == 1 { "issue" } else { "issues" }),
                            (s, n) => format!("{s} {} (and {} minor).", if s == 1 { "problem" } else { "problems" }, n - s),
                        };
                        note(&e.path, text, serious == 0);
                        issues.try_update(|m| {
                            m.insert(e.path.clone(), is);
                        });
                    }
                    Err(err) => note(&e.path, err, false),
                }
            })
            .await;
            message.try_set(Some((format!("Checked {n} {}.", if n == 1 { "book" } else { "books" }), true)));
        });
    };

    let change = move || {
        let mut c = serde_json::Map::new();
        let a = names(&authors.get_untracked());
        if !a.is_empty() {
            c.insert("creators".into(), json!(a));
        }
        for (k, v) in [("language", language), ("publisher", publisher)] {
            let v = v.get_untracked().trim().to_string();
            if !v.is_empty() {
                c.insert(k.into(), json!(v));
            }
        }
        let s = names(&subjects.get_untracked().replace(',', ";"));
        if !s.is_empty() {
            c.insert("add_subjects".into(), json!(s));
        }
        if c.is_empty() {
            message.set(Some(("Fill in at least one field. Empty fields are left as they are.".into(), false)));
            return;
        }
        let books = targets();
        form.set(None);
        message.set(None);
        let c = serde_json::Value::Object(c);
        spawn_local(async move {
            let changed = std::rc::Rc::new(std::cell::Cell::new(0));
            let c2 = changed.clone();
            let n = for_each_book(busy, stop, "Changing", books, move |e| {
                let (c, changed) = (c.clone(), c2.clone());
                async move {
                    match call::<serde_json::Value>("book_change", json!({ "src": src.key(), "path": e.path, "change": c })).await {
                        Ok(r) if r["changed"] == json!(true) => {
                            changed.set(changed.get() + 1);
                            note(&e.path, "Info changed. The old file is backed up.".into(), true);
                        }
                        Ok(_) => note(&e.path, "Already had this info.".into(), true),
                        Err(err) => note(&e.path, err, false),
                    }
                }
            })
            .await;
            let k = changed.get();
            if k > 0 {
                ctx.log(format!("Changed the info of {k} {} in {}", if k == 1 { "book" } else { "books" }, dir.try_get_value().unwrap_or_default()));
            }
            message.try_set(Some((format!("Changed {k} of {n} {}.", if n == 1 { "book" } else { "books" }), true)));
            if stop.try_get_value().is_some() {
                infos.update(|i| i.clear());
                load();
            }
        });
    };

    let shrink = move || {
        let max_side: u32 = side.get_untracked().trim().parse().unwrap_or(1600);
        let quality: u8 = quality.get_untracked().trim().parse().unwrap_or(80);
        let books = targets();
        form.set(None);
        message.set(None);
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Shrunk {
                before: u64,
                after: u64,
                images: u32,
                path: Option<String>,
            }
            let made = std::rc::Rc::new(std::cell::Cell::new(0));
            let m2 = made.clone();
            let n = for_each_book(busy, stop, "Shrinking", books, move |e| {
                let made = m2.clone();
                async move {
                    let args = json!({ "src": src.key(), "path": e.path, "max_side": max_side, "quality": quality });
                    match call::<Shrunk>("book_shrink", args).await {
                        Ok(Shrunk { before, after, images, path: Some(to) }) => {
                            made.set(made.get() + 1);
                            note(&e.path, format!("{} → {} ({images} {} made smaller): saved as {}.", human_size(before), human_size(after), if images == 1 { "image" } else { "images" }, base(&to)), true);
                            ctx.changed(src, &parent(&to));
                        }
                        Ok(_) => note(&e.path, "Already as small as it gets: no copy made.".into(), true),
                        Err(err) => note(&e.path, err, false),
                    }
                }
            })
            .await;
            let k = made.get();
            if k > 0 {
                ctx.log(format!("Made smaller copies of {k} {}", if k == 1 { "book" } else { "books" }));
            }
            message.try_set(Some((format!("Made {k} smaller {} from {n}. The books themselves are unchanged.", if k == 1 { "copy" } else { "copies" }), true)));
            if stop.try_get_value().is_some() {
                load();
            }
        });
    };

    let close = move || ctx.close_panel();
    let field = |label: &'static str, hint: &'static str, sig: RwSignal<String>| view! {
        <label class="field"><span>{label}</span>
            <input placeholder=hint prop:value=move || sig.get() on:input=move |ev| sig.set(event_target_value(&ev))/></label>
    };
    let place = ctx.place(src);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog library" on:click=|e| e.stop_propagation()>
                <h2>"Books in " {base(&dir.get_value())} <span class="about">{format!(" on {place}, with its subfolders")}</span></h2>
                <div class="lib-bar">
                    <input class="filter-wide" type="search" placeholder="Filter by title, author or file name"
                        prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev))/>
                    <button class="btn" on:click=move |_| {
                        let all: std::collections::HashSet<String> = shown().into_iter().map(|e| e.path).collect();
                        picked.update(|p| if all.iter().all(|x| p.contains(x)) { p.clear() } else { p.extend(all) });
                    }>{move || if n_picked() > 0 { "Pick none" } else { "Pick all" }}</button>
                    <span class="grow"></span>
                    <span class="about">{move || match n_picked() { 0 => "Pick books to work on".to_string(), n => format!("{n} picked") }}</span>
                    <button class="btn" disabled=move || n_picked() == 0 || busy.get().is_some() on:click=move |_| check()>"Check"</button>
                    <button class="btn" disabled=move || n_picked() == 0 || busy.get().is_some()
                        on:click=move |_| form.set(if form.get_untracked() == Some(BookForm::Info) { None } else { Some(BookForm::Info) })>"Change info…"</button>
                    <button class="btn" disabled=move || n_picked() == 0 || busy.get().is_some()
                        on:click=move |_| form.set(if form.get_untracked() == Some(BookForm::Shrink) { None } else { Some(BookForm::Shrink) })>"Make smaller…"</button>
                </div>
                {move || form.get().map(|f| match f {
                    BookForm::Info => view! {
                        <div class="lib-form">
                            <p class="hint">{format!("Set these on the {} picked books. Empty fields are left as they are. Each book is backed up before it's changed.", n_picked())}</p>
                            {field("Authors", "Ann Smith; Bob Jones (replaces the authors)", authors)}
                            {field("Language", "en", language)}
                            {field("Publisher", "", publisher)}
                            {field("Add subjects", "Fiction; Poetry", subjects)}
                            <div class="actions"><span class="grow"></span>
                                <button class="btn" on:click=move |_| form.set(None)>"Cancel"</button>
                                <button class="btn primary" on:click=move |_| change()>"Change"</button></div>
                        </div>
                    }.into_any(),
                    BookForm::Shrink => view! {
                        <div class="lib-form">
                            <p class="hint">"Makes a smaller copy next to each book, named “… (smaller).epub”: big pictures are scaled down and photos saved again at the quality below. The books themselves aren't changed."</p>
                            {field("Longest side of pictures (pixels)", "1600", side)}
                            {field("Photo quality (30–95)", "80", quality)}
                            <div class="actions"><span class="grow"></span>
                                <button class="btn" on:click=move |_| form.set(None)>"Cancel"</button>
                                <button class="btn primary" on:click=move |_| shrink()>"Make copies"</button></div>
                        </div>
                    }.into_any(),
                })}
                {move || busy.get().map(|b| view! {
                    <div class="lib-busy"><span class="text">{b}</span>
                        <button class="btn small" on:click=move |_| stop.set_value(true)>"Stop"</button></div>
                })}
                {move || message.get().map(|(m, ok)| view! { <p class:ok=ok class:error=!ok>{m}</p> })}
                <div class="books">
                    {move || match list.get() {
                        None => view! { <p class="hint">"Looking for books…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok((all, _))) if all.is_empty() => view! { <p class="hint">"No EPUB books in this folder or its subfolders."</p> }.into_any(),
                        Some(Ok((_, complete))) => view! {
                            {(!complete).then(|| view! { <p class="hint">"Stopped looking early: there are too many files. Open a smaller folder to see all its books."</p> })}
                            <For each=shown key=|e| (e.path.clone(), e.size, e.mtime) let:e>
                                {
                                    let e = StoredValue::new(e);
                                    let path = e.get_value().path;
                                    let (p1, p2, p3, p4, p5) = (path.clone(), path.clone(), path.clone(), path.clone(), path.clone());
                                    let rel = path.strip_prefix(&dir.get_value()).unwrap_or(&path).trim_start_matches('/').to_string();
                                    let cover = thumb_url(src, &e.get_value());
                                    view! {
                                        <div class="book" class:picked=move || picked.with(|p| p.contains(&p1))>
                                            <input type="checkbox" prop:checked=move || picked.with(|p| p.contains(&p2))
                                                on:change=move |_| picked.update(|p| if !p.remove(&p3) { p.insert(p3.clone()); })/>
                                            <div class="cover">{cover.map(|u| view! { <img src=u loading="lazy" alt=""/> })}</div>
                                            <div class="what">
                                                {move || {
                                                    let i = infos.with(|m| m.get(&p4).cloned());
                                                    let name = e.get_value().name;
                                                    match i {
                                                        None => view! { <b>{name}</b><span class="about">"Reading…"</span> }.into_any(),
                                                        Some(BookInfo { problem: Some(p), .. }) => view! { <b>{name}</b><span class="about error">{p}</span> }.into_any(),
                                                        Some(i) => {
                                                            let title = if i.title.is_empty() { name } else { i.title };
                                                            let about = [i.creators.join(", "), i.language, i.publisher].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
                                                            view! { <b>{title}</b><span class="about">{about}</span> }.into_any()
                                                        }
                                                    }
                                                }}
                                                <span class="path">{format!("{rel} · {}", human_size(e.get_value().size))}</span>
                                                {move || notes.with(|n| n.get(&p5).cloned()).map(|(t, ok)| {
                                                    let path = e.get_value().path;
                                                    let has = issues.with(|m| m.get(&path).is_some_and(|v| !v.is_empty()));
                                                    view! {
                                                        <span class="note" class:ok=ok class:error=!ok>{t}
                                                            {has.then(|| view! {
                                                                <button class="link" on:click=move |_| {
                                                                    let path = e.get_value().path;
                                                                    shown_issues.update(|s| *s = if s.as_deref() == Some(path.as_str()) { None } else { Some(path) });
                                                                }>" Details"</button>
                                                            })}
                                                        </span>
                                                    }
                                                })}
                                                {move || {
                                                    let path = e.get_value().path;
                                                    (shown_issues.get().as_deref() == Some(path.as_str())).then(|| view! {
                                                        <ul class="issues">
                                                            {issues.with(|m| m.get(&path).cloned().unwrap_or_default()).into_iter().map(|i| view! {
                                                                <li class:serious=i.serious>{i.text}</li>
                                                            }).collect_view()}
                                                        </ul>
                                                    })
                                                }}
                                            </div>
                                            <button class="btn" title="Read the book" on:click=move |_| {
                                                ctx.panel.set(Some(Panel::Reader(src, e.get_value().path, Some(dir.get_value()))));
                                            }>"Read"</button>
                                            <button class="btn" title="Open in the EPUB editor" on:click=move |_| {
                                                let path = e.get_value().path;
                                                ctx.panel.set(None);
                                                spawn_local(async move {
                                                    if let Err(err) = ctx.ep.open(&src.key(), ctx.place(src), path).await {
                                                        ctx.act().error(err);
                                                    }
                                                });
                                            }>"Edit"</button>
                                        </div>
                                    }
                                }
                            </For>
                        }.into_any(),
                    }}
                </div>
                <div class="actions">
                    <span class="about">{move || match list.get() {
                        Some(Ok((all, _))) => {
                            let read = infos.with(|i| i.len()).min(all.len());
                            if read < all.len() { format!("{} books · reading info {read} of {}…", all.len(), all.len()) } else { format!("{} books", all.len()) }
                        }
                        _ => String::new(),
                    }}</span>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}

/// Reading a book, a chapter at a time, in a quiet page.
#[component]
fn Reader(src: Src, path: String, back: Option<String>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let book = RwSignal::new(None::<Result<crate::api::Book, String>>);
    let at = RwSignal::new(0usize);
    let page = RwSignal::new(None::<Result<String, String>>);
    let size = RwSignal::new(19u32);
    // The backend keeps the book open until this closes.
    let id = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let (id, alive) = (id.clone(), alive.clone());
        let path = path.clone();
        spawn_local(async move {
            let r = call::<crate::api::Book>("epub_open", json!({ "src": src.key(), "path": path })).await;
            if let Ok(b) = &r {
                if !alive.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = call::<()>("epub_close", json!({ "id": b.id })).await;
                    return;
                }
                id.store(b.id, std::sync::atomic::Ordering::Relaxed);
            }
            book.try_set(Some(r));
        });
    }
    {
        let (id, alive) = (id.clone(), alive.clone());
        on_cleanup(move || {
            alive.store(false, std::sync::atomic::Ordering::Relaxed);
            let id = id.load(std::sync::atomic::Ordering::Relaxed);
            if id != u32::MAX {
                spawn_local(async move {
                    let _ = call::<()>("epub_close", json!({ "id": id })).await;
                });
            }
        });
    }
    Effect::new(move |_| {
        let Some(Ok(b)) = book.get() else { return };
        let Some(ch) = b.chapters.get(at.get()).cloned() else { return };
        page.set(None);
        spawn_local(async move {
            let r = call::<crate::api::ChapterDoc>("epub_chapter", json!({ "id": b.id, "path": ch.path })).await;
            let r = r.and_then(|d| d.html.ok_or_else(|| d.problem.unwrap_or_else(|| "This chapter can't be shown.".into())));
            page.try_set(Some(r));
        });
    });
    let count = move || book.with(|b| b.as_ref().and_then(|b| b.as_ref().ok()).map_or(0, |b| b.chapters.len()));
    let go = move |d: i32| {
        let n = count();
        let to = at.get_untracked() as i32 + d;
        if to >= 0 && (to as usize) < n {
            at.set(to as usize);
        }
    };
    let keys = window_event_listener(ev::keydown, move |e| match e.key().as_str() {
        "ArrowRight" | "PageDown" => go(1),
        "ArrowLeft" | "PageUp" => go(-1),
        _ => {}
    });
    on_cleanup(move || keys.remove());
    let doc = move || {
        let html = page.get()?.ok()?;
        let style = format!(
            "<style>html{{font-size:{}px !important;background:#fbfaf7;color:#1d1d1f}}body{{max-width:36em;margin:0 auto !important;padding:28px 28px 80px !important;line-height:1.6}}img,svg{{max-width:100%;height:auto}}@media (prefers-color-scheme: dark){{html{{background:#1c1c1e;color:#e6e6e6}}a{{color:#8ab4ff}}}}</style>",
            size.get()
        );
        // After the <base> the backend adds, so the book's own styles still load.
        Some(match html.find("<base ").and_then(|k| html[k..].find('>').map(|j| k + j + 1)) {
            Some(k) => format!("{}{style}{}", &html[..k], &html[k..]),
            None => format!("{style}{html}"),
        })
    };
    let back = StoredValue::new(back);
    let path = StoredValue::new(path);
    let close = move || match back.get_value() {
        Some(dir) => ctx.panel.set(Some(Panel::Library(src, dir))),
        None => ctx.panel.set(None),
    };
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog reader" on:click=|e| e.stop_propagation()>
                <div class="reader-bar">
                    <b class="title">{move || book.with(|b| match b {
                        Some(Ok(b)) => if b.meta.title.is_empty() { b.name.clone() } else { b.meta.title.clone() },
                        _ => base(&path.get_value()),
                    })}</b>
                    <button class="btn" title="Previous chapter (←)" disabled=move || at.get() == 0 on:click=move |_| go(-1)>"‹"</button>
                    <select prop:value=move || at.get().to_string() on:change=move |ev| at.set(event_target_value(&ev).parse().unwrap_or(0))>
                        {move || book.with(|b| match b {
                            Some(Ok(b)) => b.chapters.iter().enumerate().map(|(k, c)| view! { <option value=k.to_string()>{c.label.clone()}</option> }).collect_view().into_any(),
                            _ => ().into_any(),
                        })}
                    </select>
                    <button class="btn" title="Next chapter (→)" disabled=move || at.get() + 1 >= count() on:click=move |_| go(1)>"›"</button>
                    <button class="btn" title="Smaller text" on:click=move |_| size.update(|s| *s = (*s).saturating_sub(1).max(12))>"A−"</button>
                    <button class="btn" title="Bigger text" on:click=move |_| size.update(|s| *s = (*s + 1).min(32))>"A+"</button>
                    <span class="grow"></span>
                    <span class="about">{move || match count() { 0 => String::new(), n => format!("{} of {n}", at.get() + 1) }}</span>
                    <button class="btn" title="Open in the EPUB editor" on:click=move |_| {
                        ctx.panel.set(None);
                        spawn_local(async move {
                            if let Err(err) = ctx.ep.open(&src.key(), ctx.place(src), path.get_value()).await {
                                ctx.act().error(err);
                            }
                        });
                    }>"Edit"</button>
                    <button class="btn primary" on:click=move |_| close()>{if back.get_value().is_some() { "Back" } else { "Close" }}</button>
                </div>
                {move || match (book.get(), page.get()) {
                    (Some(Err(e)), _) | (_, Some(Err(e))) => view! { <p class="error">{e}</p> }.into_any(),
                    (Some(Ok(_)), Some(Ok(_))) => view! { <iframe class="read" sandbox="allow-same-origin" srcdoc=doc></iframe> }.into_any(),
                    _ => view! { <p class="hint">"Opening…"</p> }.into_any(),
                }}
            </div>
        </div>
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
            Notice::ConfirmDelete(items) => {
                let place = ctx.place(p.src.get_untracked());
                let label = match items.as_slice() {
                    [e] if e.dir => format!("Delete the folder {} from {place}? Only an empty folder can be deleted.", e.name),
                    [e] => format!("Delete {} from {place}? A backup is kept on the Mac.", e.name),
                    _ => format!("Delete {} items from {place}? Files are backed up on the Mac; folders are deleted only when empty.", items.len()),
                };
                view! {
                    <div class="notice"><span class="text">{label}</span>
                        <button class="btn danger solid" on:click=move |_| ctx.delete(p, items.clone())>"Delete"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button></div>
                }.into_any()
            }
            Notice::Sync { report: r, from, to } => {
                let list = |what: &str, v: &[String]| -> String {
                    let mut s = v.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                    if v.len() > 4 {
                        s.push_str(&format!(", and {} more", v.len() - 4));
                    }
                    format!("{} {what}: {s}.", v.len())
                };
                let mut lines = vec![if r.files + r.folders == 0 {
                    format!("Nothing to copy: everything in {} is already in {}.", base(&from.1), to.1)
                } else {
                    format!("Copy {} {} ({}){} from {} into {} on {}?",
                        r.files, if r.files == 1 { "file" } else { "files" }, human_size(r.bytes),
                        if r.folders > 0 { format!(" and {} {}", r.folders, if r.folders == 1 { "folder" } else { "folders" }) } else { String::new() },
                        base(&from.1), to.1, ctx.place(to.0))
                }];
                if r.same > 0 {
                    lines.push(format!("{} already there.", r.same));
                }
                if !r.differ.is_empty() {
                    lines.push(format!("{} Left as they are.", list("differ in size", &r.differ)));
                }
                if !r.extra.is_empty() {
                    lines.push(format!("{} Left as they are.", list("only in the other pane", &r.extra)));
                }
                let go = r.files + r.folders > 0;
                let (f, t) = (StoredValue::new(from), StoredValue::new(to));
                view! {
                    <div class="notice"><span class="text">{lines.join("\n")}</span>
                        {go.then(|| view! { <button class="btn primary" on:click=move |_| ctx.sync(p, f.get_value(), t.get_value())>"Copy"</button> })}
                        <button class="btn" on:click=dismiss>{if go { "Cancel" } else { "Dismiss" }}</button></div>
                }.into_any()
            }
            Notice::Rename(e) => {
                name.set(e.name.clone());
                let e = StoredValue::new(e);
                let submit = move || ctx.rename(p, e.get_value(), name.get_untracked());
                view! {
                    <div class="notice">
                        <span>"Rename to:"</span>
                        <input autofocus prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => submit(),
                                "Escape" => p.notice.set(Notice::None),
                                _ => {}
                            }/>
                        <button class="btn primary" on:click=move |_| submit()>"Rename"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button>
                    </div>
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
        <section class="pane" class:active=active data-pane=p.id.to_string() on:mousedown=move |_| ctx.active.set(p.id)>
            <div class="bar">
                <button class="btn" title="Up (⌘↑)" disabled=move || p.cwd.get() == "/" on:click=move |_| ctx.go_up(p)>"↑"</button>
                <div class="seg" role="group" aria-label="Show files on">
                    <button title="Files on the Mac" aria-label="Mac" class:active=move || p.src.get() == Src::Mac
                        on:click=move |_| ctx.switch_src(p, Src::Mac)>"💻"</button>
                    // One button per connected device.
                    {move || {
                        let phones = ctx.phones.get();
                        if phones.is_empty() {
                            return view! { <button title="No device connected" aria-label="Device" disabled=true>"📱"</button> }.into_any();
                        }
                        phones.into_iter().map(|i| {
                            let src = Src::of(&i.id);
                            view! {
                                <button title=format!("Files on {} ({})", i.label, i.transport) aria-label=i.label.clone()
                                    class:active=move || p.src.get() == src on:click=move |_| ctx.switch_src(p, src)>"📱"</button>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
                <Crumbs pane=p/>
                {move || p.loading.get().then(|| view! { <span class="spin"></span> })}
                <input class="filter" type="search" placeholder="Filter" title="Show names containing this (⌘F). Enter searches subfolders too."
                    prop:value=move || p.filter.get()
                    on:input=move |ev| { p.filter.set(event_target_value(&ev)); p.arrange(); }
                    on:keydown=move |ev| match ev.key().as_str() {
                        "Enter" => ctx.find(p),
                        "Escape" => {
                            p.filter.set(String::new());
                            p.arrange();
                            if let Some(t) = ev.target() { let _ = t.unchecked_into::<web_sys::HtmlElement>().blur(); }
                        }
                        _ => {}
                    }/>
                {(p.id == 0).then(|| view! {
                    <button class="btn" class:active=move || ctx.split.get()
                        title=move || if ctx.split.get() { "Hide the second pane (⌘\\)" } else { "Show a second pane (⌘\\)" }
                        aria-label="Second pane" on:click=move |_| ctx.set_split(!ctx.split.get_untracked())>"◫"</button>
                })}
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

/// Selects on click (⌘ and Shift pick several), opens on double-click, and shows the context menu
/// on right-click.
fn row_events(
    ctx: Ctx,
    p: Pane,
    e: &Entry,
) -> (impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static) {
    let (path, e1, e2) = (e.path.clone(), e.clone(), e.clone());
    (
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            p.click(&ev, &path);
        },
        move |_| ctx.open(p, e1.clone()),
        move |ev: web_sys::MouseEvent| ctx.show_menu(p, &ev, Some(e2.clone())),
    )
}

/// Starts dragging an item: the picked items if it's one of them, otherwise just it.
fn drag_start(ctx: Ctx, p: Pane, path: String) -> impl Fn(web_sys::DragEvent) + 'static {
    move |ev: web_sys::DragEvent| {
        if !p.picked().iter().any(|e| e.path == path) {
            p.select_only(Some(path.clone()));
        }
        let paths: Vec<String> = p.picked().into_iter().map(|e| e.path).collect();
        if let Some(dt) = ev.data_transfer() {
            let _ = dt.set_data("text/plain", &paths.join("\n"));
            dt.set_effect_allowed("copyMove");
        }
        ctx.drag.set_value(Some((p.src.get_untracked(), paths)));
    }
}

fn drag_end(ctx: Ctx) -> impl Fn(web_sys::DragEvent) + 'static {
    move |_| {
        ctx.drag.set_value(None);
        ctx.drop_at.set(None);
    }
}

/// Lets items dragged from a pane drop into folder `dir` (None: the pane's folder). Dropping moves
/// within the same device and copies between devices, like Finder; ⌥ always copies, ⌘ always moves.
fn drop_events(
    ctx: Ctx,
    p: Pane,
    dir: Option<String>,
) -> (impl Fn(web_sys::DragEvent) + 'static, impl Fn(web_sys::DragEvent) + 'static) {
    let key = format!("{}:{}", p.id, dir.clone().unwrap_or_default());
    let d = dir.clone();
    let over = move |ev: web_sys::DragEvent| {
        let Some((src, paths)) = ctx.drag.get_value() else { return };
        let target = dir_or_cwd(p, &d);
        // Not into one of the dragged folders, nor into the folder they're already in.
        if src == p.src.get_untracked() && paths.iter().any(|x| *x == target || parent(x) == target) {
            if ctx.drop_at.get_untracked().is_some() {
                ctx.drop_at.set(None);
            }
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        if let Some(dt) = ev.data_transfer() {
            dt.set_drop_effect(if moves(&ev, src, p.src.get_untracked()) { "move" } else { "copy" });
        }
        if ctx.drop_at.get_untracked().as_deref() != Some(key.as_str()) {
            ctx.drop_at.set(Some(key.clone()));
        }
    };
    let drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        ctx.drop_at.set(None);
        let Some((src, paths)) = ctx.drag.write_value().take() else { return };
        let to_src = p.src.get_untracked();
        ctx.active.set(p.id);
        ctx.transfer(p, src, paths, to_src, dir_or_cwd(p, &dir), moves(&ev, src, to_src));
    };
    (over, drop)
}

fn dir_or_cwd(p: Pane, dir: &Option<String>) -> String {
    dir.clone().unwrap_or_else(|| p.cwd.get_untracked())
}

fn moves(ev: &web_sys::DragEvent, from: Src, to: Src) -> bool {
    if ev.alt_key() {
        false
    } else if ev.meta_key() {
        true
    } else {
        from == to
    }
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
    let (over, drop) = drop_events(ctx, p, None);
    let here = format!("{}:", p.id);
    view! {
        <div class="list" class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:dragover=over on:drop=drop>
            <table>
                <thead><tr>
                    {SortKey::ALL.map(|k| {
                        let num = matches!(k, SortKey::Date | SortKey::Size);
                        view! {
                            <th class:num=num><button class="sort" title=format!("Sort by {}", k.label().to_lowercase())
                                on:click=move |_| p.set_sort(k)>
                                {k.label()}
                                <span class="arrow">{move || match p.sort.get() {
                                    Sort(s, asc) if s == k => if asc { "▲" } else { "▼" },
                                    _ => "",
                                }}</span>
                            </button></th>
                        }
                    })}
                </tr></thead>
                <tbody>
                    <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                        {
                            let k = kind(&e);
                            let path = e.path.clone();
                            let (click, dbl, menu) = row_events(ctx, p, &e);
                            let where_ = p.found.get_untracked().is_some().then(|| parent(&e.path));
                            let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                            let (over, drop) = drop_events(ctx, p, dir.clone());
                            view! {
                                <tr class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                    draggable="true" data-dir=dir
                                    on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                    on:dragover=move |ev| if e.dir { over(ev) } on:drop=move |ev| if e.dir { drop(ev) }
                                    on:click=click on:dblclick=dbl on:contextmenu=menu>
                                    <td class="name" title=e.path.clone()><span class="icon">{icon(k)}</span>{e.name.clone()}
                                        {where_.map(|w| view! { <span class="where">{w}</span> })}</td>
                                    <td class="kindcol">{kind_label(&e)}</td>
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
    let ctx = expect_context::<Ctx>();
    let state = RwSignal::new(0u8); // 0 loading, 1 loaded, 2 failed
    let glyph = icon(kind(&entry));
    let url = thumb_url(src, &entry);
    // Phone thumbnails can be turned off in the settings.
    let allowed = move || src == Src::Mac || ctx.phone_thumbs.get();
    view! {
        {move || (state.get() != 1 || !allowed()).then(|| view! { <span class="glyph">{glyph}</span> })}
        {move || url.clone().filter(|_| allowed()).map(|url| view! {
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
    let (over, drop) = drop_events(ctx, p, None);
    let here = format!("{}:", p.id);
    view! {
        <div class="list" class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:dragover=over on:drop=drop>
            <div class="thumbs">
                <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                    {
                        let path = e.path.clone();
                        let (click, dbl, menu) = row_events(ctx, p, &e);
                        let sub = if e.dir { "Folder".to_string() } else { human_size(e.size) };
                        let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                        let (over, drop) = drop_events(ctx, p, dir.clone());
                        let is_dir = e.dir;
                        view! {
                            <div class="tile" title=e.name.clone()
                                class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                draggable="true" data-dir=dir
                                on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
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
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let chain = Memo::new(move |_| chain_from(&p.root.get(), &p.cwd.get()));
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
                        let mut es = ctx.visible(es);
                        p.sort.get_untracked().apply(&mut es);
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
    let (over, drop) = drop_events(ctx, p, Some(dir.clone()));
    let key = format!("{}:{}", p.id, dir);
    let data_dir = dir.clone();
    view! {
        <div class="col" class:current=current on:contextmenu=bg_menu data-dir=data_dir
            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
            on:dragover=over on:drop=drop>
            {move || match entries() {
                None => view! { <div class="empty"><span class="spin"></span></div> }.into_any(),
                Some(es) if es.is_empty() => view! { <div class="empty">"Empty"</div> }.into_any(),
                Some(es) => es.into_iter().map(|e| {
                    let (p1, p2) = (e.path.clone(), e.path.clone());
                    let (e1, e2) = (e.clone(), e.clone());
                    let dir = dir.clone();
                    let click = move |ev: web_sys::MouseEvent| {
                        if current.get_untracked() && (ev.meta_key() || ev.shift_key()) {
                            p.click(&ev, &e1.path);
                        } else if e1.dir {
                            ctx.navigate(p, e1.path.clone());
                        } else if current.get_untracked() {
                            p.select_only(Some(e1.path.clone()));
                        } else {
                            ctx.navigate_select(p, p.src.get_untracked(), dir.clone(), Some(e1.path.clone()));
                        }
                    };
                    let k = kind(&e);
                    let (fdir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                    let (over, drop) = drop_events(ctx, p, fdir.clone());
                    let is_dir = e.dir;
                    view! {
                        <div class="row"
                            class:open=move || open_child.get().as_deref() == Some(p1.as_str())
                            class:selected=move || current.get() && p.is_picked(&p2)
                            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                            draggable="true" data-dir=fdir
                            on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                            on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
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
        let n = p.marked.with(|m| m.len());
        if n > 1 {
            let bytes: u64 = p.marked.with(|m| p.entries.with(|es| es.iter().filter(|e| m.contains(&e.path)).map(|e| e.size).sum()));
            return Some(format!("{n} picked · {}", human_size(bytes)));
        }
        p.selected_entry().map(|e| {
            let size = if e.dir { String::new() } else { format!(" · {}", human_size(e.size)) };
            let modified = if e.mtime > 0 { format!(" · modified {}", when(e.mtime)) } else { String::new() };
            format!("{}{size}{modified}", e.name)
        })
    };
    let place = move || match p.src.get() {
        Src::Mac => ctx.mac.get().map(|i| i.label).unwrap_or_default(),
        src => ctx.src_info(src).map(|i| format!("{} · {}", i.label, i.transport)).unwrap_or_default(),
    };
    let search = move || p.found.get().map(|(q, complete)| {
        let n = p.all.with(|a| a.len());
        let more = if complete { "" } else { " (stopped early: open a smaller folder to see all)" };
        view! {
            <span class="search">{format!("{n} found for “{q}”{more}")}</span>
            <button class="btn small" on:click=move |_| ctx.reload(p)>"Back to folder"</button>
        }
    });
    view! {
        <div class="statusbar">
            {search}
            <span>{summary}</span>
            <span class="sel">{selection}</span>
            <span class="spacer"></span>
            <span class="free">{move || ctx.free_text(p)}</span>
            <span>{place}</span>
            <div class="seg" role="group" aria-label="View">
                {ViewMode::ALL.map(|m| view! {
                    <button title=format!("View as {}", m.label()) aria-label=m.label()
                        class:active=move || p.view.get() == m on:click=move |_| ctx.set_view(p, m)>{m.glyph()}</button>
                })}
            </div>
        </div>
    }
}

/// The copy or move under way: what it's on, how far it is, and Cancel.
#[component]
fn TransferBar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || ctx.job.get().map(|j| {
        let verb = if j.moving { "Moving" } else { "Copying" };
        let (text, pct) = match &j.progress {
            None => (format!("{verb}: looking at what to copy…"), 0.0),
            Some(pr) => {
                let pct = if pr.bytes_total > 0 { pr.bytes_done as f64 * 100.0 / pr.bytes_total as f64 } else { 0.0 };
                let now = if pr.current.is_empty() { String::new() } else { format!(" {}", pr.current) };
                (format!("{verb}{now} · {} of {} files · {} of {}", pr.files_done, pr.files_total,
                    human_size(pr.bytes_done), human_size(pr.bytes_total)), pct)
            }
        };
        view! {
            <div class="transfer">
                <span class="spin"></span>
                <span class="text">{text}</span>
                <div class="meter"><div style=format!("width:{pct:.1}%")></div></div>
                <button class="btn" title="Stops after the file being copied now"
                    on:click=move |_| ctx.cancel_transfer()>"Cancel"</button>
            </div>
        }
    })
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
            <span class="kind">{move || ctx.editor.get().map(|e| match e.src {
                Src::Mac => "Mac".to_string(),
                src => ctx.src_info(src).map_or_else(|| "Device".into(), |i| i.label),
            })}</span>
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
