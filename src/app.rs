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

mod books;
mod devices;
mod editor;
mod files;
mod layout;
mod menu;
mod nav;
mod pane;
mod preview;
mod safety;
mod sidebar;
mod transfer;
mod util;
mod views;

use books::*;
use devices::*;
use editor::*;
use layout::*;
use menu::*;
use pane::*;
use preview::*;
use safety::*;
use sidebar::*;
use transfer::*;
use util::*;
use views::*;

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
    /// The list's columns to show or hide, from a right-click on its header.
    columns: bool,
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
    CopyName,
    CopyPath,
    ShowAs(PeekAs),
    /// Shows or hides one of the list's columns (an index into `LIST_COLS`).
    Column(usize),
    ResetColumns,
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
    /// The item whose name is being edited in place.
    renaming: RwSignal<Option<String>>,
    /// Whether the preview is shown to the right of the files.
    preview: RwSignal<bool>,
    /// Widths the user dragged (0: the default): the files beside the preview, and the list's
    /// kind, date and size columns.
    view_w: RwSignal<f64>,
    /// Widths of the column view's columns, by folder, for this session.
    col_ws: StoredValue<HashMap<String, f64>>,
    list_w: [RwSignal<f64>; 3],
    /// The list's columns after the name, in order (indexes into `LIST_COLS`); missing ones are
    /// hidden.
    list_cols: RwSignal<Vec<usize>>,
    /// A file the preview shows as text or bytes, whatever its type, from the context menu.
    peek_as: RwSignal<Option<(String, PeekAs)>>,
    /// Whether the path above the files is being typed in.
    editing_path: RwSignal<bool>,
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
            renaming: RwSignal::new(None),
            preview: RwSignal::new(stored_flag(&format!("yomi-files.preview.{id}"), true)),
            view_w: stored_px(&format!("yomi-files.view-w.{id}")),
            col_ws: StoredValue::new(HashMap::new()),
            list_w: [0, 1, 2].map(|n| stored_px(&format!("yomi-files.list-w.{id}.{n}"))),
            list_cols: RwSignal::new(stored_cols(id)),
            peek_as: RwSignal::new(None),
            editing_path: RwSignal::new(false),
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
    /// Widths the user dragged (0: the default): the sidebar, and the left pane when there are two.
    side_w: RwSignal<f64>,
    pane_w: RwSignal<f64>,
    /// The last searches (Enter in the filter), newest first, offered as the filter is typed in.
    searches: RwSignal<Vec<String>>,
    /// Settings: "auto", "light" or "dark".
    theme: RwSignal<String>,
    /// Whether the list of keyboard shortcuts is shown.
    help: RwSignal<bool>,
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
            side_w: stored_px(SIDE_W_KEY),
            pane_w: stored_px(PANE_W_KEY),
            searches: RwSignal::new(stored(SEARCHES_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()),
            theme: RwSignal::new(stored(THEME_KEY).unwrap_or_else(|| "auto".into())),
            help: RwSignal::new(false),
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
}

#[component]
pub fn App() -> impl IntoView {
    let ctx = Ctx::new();
    apply_theme(&ctx.theme.get_untracked());
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
        if ctx.help.get_untracked() {
            if matches!(e.key().as_str(), "Escape" | "?") {
                e.prevent_default();
                ctx.help.set(false);
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
                    p.renaming.set(Some(x.path.clone()));
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
            ("f", true) | ("k", true) | ("/", false) => {
                e.prevent_default();
                if let Some(el) = document().query_selector(".pane.active .filter").ok().flatten() {
                    let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
                }
            }
            ("Backspace", true) | ("Delete", _) => {
                e.prevent_default();
                ctx.ask_delete(p);
            }
            ("?", _) => {
                e.prevent_default();
                ctx.help.set(true);
            }
            ("l", true) => {
                e.prevent_default();
                p.editing_path.set(true);
            }
            // In the list, ← and → go to the previous and next picture, video or sound.
            ("ArrowRight", false) | ("ArrowLeft", false) if view == ViewMode::List => {
                if let Some(x) = current.filter(|x| media(x).is_some()) {
                    e.prevent_default();
                    step_media(p, &x, if e.key() == "ArrowRight" { 1 } else { -1 });
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
        {move || ctx.help.get().then(|| view! { <Shortcuts/> })}
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
        <div class="app browse" style=move || match ctx.side_w.get() {
            w if w > 0.0 => format!("grid-template-columns: {w}px auto 1fr"),
            _ => String::new(),
        }>
            <Sidebar/>
            <Grip size=ctx.side_w key=SIDE_W_KEY.into() min=160.0 max=480.0 room=480.0 class="grip side-grip"/>
            <div class="main">
                {move || if ctx.editor.get().is_some() { view! { <TextEditor/> }.into_any() } else { view! {
                    <div class="panes">
                        <PaneView pane=ctx.panes[0]/>
                        {move || ctx.split.get().then(|| view! {
                            <Grip size=ctx.pane_w key=PANE_W_KEY.into() min=240.0 room=240.0 class="grip pane-grip"/>
                            <PaneView pane=ctx.panes[1]/>
                        })}
                    </div>
                }.into_any() }}
                <TransferBar/>
            </div>
        </div>
    }.into_any() }
}
