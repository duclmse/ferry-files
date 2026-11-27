//! The file browser UI: connect screen, one or two panes of folders (each on the phone or the Mac,
//! in a list, thumbnail or column view, with a context menu), and a text editor.

use crate::api::{
    call, Backup, BookInfo, CompareReport, Devices, DupeProgress, Dupes, Entry, FileDrop, Info,
    Issue, Marks, Packed, Progress, SmbMount, Space, TextFile, TransferReport, UsbDevice,
    WriteResult,
};
use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsCast;

mod batch;
mod books;
mod code;
mod compare;
mod connections;
mod context;
mod devices;
mod dupes;
mod editor;
mod everything;
mod files;
mod filetype;
mod find;
mod fsd;
mod icons;
mod import;
mod keymap;
mod layout;
mod listtree;
mod menu;
mod nav;
mod near;
mod palette;
mod pane;
mod pane_state;
mod pdf;
mod preview;
mod reading;
mod rows;
mod safety;
mod scripts;
mod search;
mod servers;
mod shell;
mod sidebar;
mod terminal;
mod transfer;
mod undo;
mod util;
mod video;
mod views;
mod wifi;
mod workspace;

use batch::*;
use books::*;
use code::*;
use compare::*;
use connections::*;
use devices::*;
use dupes::*;
use editor::*;
/// For the EPUB editor's Source view.
pub(crate) use editor::{code_html, shift_runs, LAYER_MAX};
use filetype::*;
pub(crate) use icons::{ico, Icon};
use import::*;
use keymap::*;
/// For the EPUB editor's side pane.
pub(crate) use layout::{stored_px, tab_pick, Grip, Target};
use menu::*;
use palette::*;
use pane::*;
use preview::*;
use safety::*;
use search::*;
pub use shell::App;
use sidebar::*;
use terminal::*;
use transfer::*;
use undo::*;
use util::*;
pub(crate) use util::{store, stored, stored_flag};
use video::*;
pub use video::{popped_out, PopOut};
use views::*;
use wifi::*;
use workspace::*;

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
        id.strip_prefix('d')
            .and_then(|n| n.parse().ok())
            .map_or(Src::Mac, Src::Dev)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    List,
    Thumbs,
    Columns,
    /// A folder tree beside a text editor with tabs, as in a code editor.
    Code,
}

impl ViewMode {
    const ALL: [ViewMode; 4] = [
        ViewMode::List,
        ViewMode::Thumbs,
        ViewMode::Columns,
        ViewMode::Code,
    ];

    fn key(self) -> &'static str {
        match self {
            ViewMode::List => "list",
            ViewMode::Thumbs => "thumbs",
            ViewMode::Columns => "columns",
            ViewMode::Code => "code",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ViewMode::List => "List",
            ViewMode::Thumbs => "Thumbnails",
            ViewMode::Columns => "Columns",
            ViewMode::Code => "Editor",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            ViewMode::List => "☰",
            ViewMode::Thumbs => "▦",
            ViewMode::Columns => "▥",
            ViewMode::Code => "✎",
        }
    }

    fn storage_key(pane: usize) -> String {
        format!("ferry-files.view.{pane}")
    }

    /// The view the pane had last time. Storage can be missing or blocked; then it's the list.
    fn stored(pane: usize) -> Self {
        let saved = stored(&Self::storage_key(pane));
        Self::ALL
            .into_iter()
            .find(|v| saved.as_deref() == Some(v.key()))
            .unwrap_or(ViewMode::List)
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

/// What a pane's list is grouped by, under a heading for each group. Stored per pane.
#[derive(Clone, Copy, PartialEq)]
enum Group {
    None,
    Name,
    Kind,
    Date,
    Size,
}

impl Group {
    const ALL: [Group; 5] = [
        Group::None,
        Group::Name,
        Group::Kind,
        Group::Date,
        Group::Size,
    ];

    fn storage_key(pane: usize) -> String {
        format!("ferry-files.group.{pane}")
    }

    fn key(self) -> &'static str {
        match self {
            Group::None => "none",
            Group::Name => "name",
            Group::Kind => "kind",
            Group::Date => "date",
            Group::Size => "size",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Group::None => "None",
            Group::Name => "Name",
            Group::Kind => "Kind",
            Group::Date => "Modified",
            Group::Size => "Size",
        }
    }

    fn stored(pane: usize) -> Self {
        let k = stored(&Self::storage_key(pane)).unwrap_or_default();
        Group::ALL
            .into_iter()
            .find(|g| g.key() == k)
            .unwrap_or(Group::None)
    }

    /// The group `e` is in, `now` being the time in seconds: its place among the groups, and its
    /// heading.
    fn of(self, e: &Entry, now: i64) -> (u32, String) {
        match self {
            Group::None => (0, String::new()),
            Group::Name => match e
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().next().unwrap_or(c))
            {
                Some(c) if c.is_alphabetic() => (c as u32 + 1, c.to_string()),
                _ => (0, "0–9 and others".into()),
            },
            Group::Kind if e.dir => (0, "Folders".into()),
            Group::Kind => (1, kind_label(e)),
            Group::Date => date_group(e.mtime, now),
            Group::Size if e.dir => (0, "Folders".into()),
            Group::Size => {
                const KB: u64 = 1024;
                const BOUNDS: [(u64, &str); 6] = [
                    (1, "Empty"),
                    (16 * KB, "Tiny (under 16 KB)"),
                    (KB * KB, "Small (under 1 MB)"),
                    (128 * KB * KB, "Medium (under 128 MB)"),
                    (KB * KB * KB, "Large (under 1 GB)"),
                    (u64::MAX, "Huge (1 GB and over)"),
                ];
                let i = BOUNDS
                    .iter()
                    .position(|(b, _)| e.size < *b)
                    .unwrap_or(BOUNDS.len() - 1);
                (i as u32 + 1, BOUNDS[i].1.into())
            }
        }
    }
}

/// The group of a file changed at `mtime` (seconds), `now` being the time: newest first.
fn date_group(mtime: i64, now: i64) -> (u32, String) {
    const DAY: i64 = 86_400;
    if mtime <= 0 {
        return (9, "No date".into());
    }
    // Days back from the start of today, in local time.
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(now as f64 * 1000.0));
    let midnight =
        now - (d.get_hours() as i64 * 3600 + d.get_minutes() as i64 * 60 + d.get_seconds() as i64);
    let year_start =
        js_sys::Date::new_with_year_month_day(d.get_full_year(), 0, 1).get_time() as i64 / 1000;
    match mtime {
        t if t > now => (0, "Later than now".into()),
        t if t >= midnight => (1, "Today".into()),
        t if t >= midnight - DAY => (2, "Yesterday".into()),
        t if t >= midnight - 6 * DAY => (3, "Last 7 days".into()),
        t if t >= midnight - 29 * DAY => (4, "Last 30 days".into()),
        t if t >= year_start => (5, "Earlier this year".into()),
        t => {
            let y = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(t as f64 * 1000.0))
                .get_full_year();
            // Years newest first.
            (6 + (3000 - y.min(3000)), y.to_string())
        }
    }
}

/// How a pane sorts: by what, and whether ascending. Stored as e.g. "date:0".
#[derive(Clone, Copy, PartialEq)]
struct Sort(SortKey, bool);

impl Sort {
    fn storage_key(pane: usize) -> String {
        format!("ferry-files.sort.{pane}")
    }

    fn stored(pane: usize) -> Self {
        let saved = stored(&Self::storage_key(pane)).unwrap_or_default();
        let (k, asc) = saved.split_once(':').unwrap_or(("name", "1"));
        Sort(
            SortKey::ALL
                .into_iter()
                .find(|s| s.key() == k)
                .unwrap_or(SortKey::Name),
            asc != "0",
        )
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

const FAVS_KEY: &str = "ferry-files.favs";

/// A saved sync: two folders, and how the second is kept like the first ("copy", "update" or
/// "mirror"). Listed in the sidebar to preview and run again.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SyncPair {
    from: Side,
    from_dir: String,
    to: Side,
    to_dir: String,
    mode: String,
}

const SYNC_PAIRS_KEY: &str = "ferry-files.sync-pairs";

/// How a sync can go: value and label.
const SYNC_MODES: [(&str, &str); 3] = [
    ("copy", "Copy missing"),
    ("update", "Update"),
    ("mirror", "Mirror"),
];
/// A mirror removing more than this many items needs "remove" typed to run.
const MIRROR_TYPE_OVER: usize = 10;

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
    /// Folders to go to: the path's folders folded into "…".
    folders: Vec<String>,
    /// A folder in the sidebar, from a right-click on it.
    place: Option<(Src, String)>,
    /// The sidebar's sections to show or hide, from a right-click on the sidebar.
    sections: bool,
    /// From the Editor view's tree: `entry` is a row of it, or None for its folder.
    tree: bool,
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
    /// To the Trash on the computer; elsewhere deleted with a backup.
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
    /// Keeps the picked items to paste somewhere else.
    CopyItems,
    /// Keeps the picked items to move somewhere else with Paste.
    CutItems,
    /// Copies the items kept by `CopyItems` into the folder (or the pane's folder).
    Paste,
    ShowAs(PeekAs),
    /// Shows or hides one of the list's columns (an index into `LIST_COLS`).
    Column(usize),
    ResetColumns,
    /// Shows the list as a tree, or flat.
    ListTree,
    /// Goes to one of the menu's `folders`.
    Go(usize),
    /// Opens the folder (or the pane's folder) in a new tab.
    NewTab,
    /// Shows or hides the sidebar.
    Sidebar,
    /// Shows or hides hidden files.
    Hidden,
    /// Undoes the newest action that can be.
    Undo,
    /// Marks how each item differs from the other pane's.
    Compare,
    /// Finds duplicate files in the panes' folders.
    Dupes,
    /// Imports the photos in this folder into dated folders.
    Import,
    /// Find and replace in the names of what's picked, or of everything shown.
    ReplaceNames,
    /// Shows or hides one of the sidebar's sections (an index into `SECTIONS`).
    Section(usize),
    /// Shows all the sidebar's sections.
    AllSections,
    /// Sorts the list by this, ascending or not.
    SortBy(Sort),
    /// Groups the list by this.
    GroupBy(Group),
    /// Turns on or off the sidebar showing the current folder.
    FollowFolder,
    /// The Editor view's tree: shows this folder at its top.
    SetRoot,
    /// The Editor view's tree: shows the folder above at its top.
    Up,
    /// The Editor view's tree: shows or hides what's in the folder.
    Expand,
    /// The Editor view's tree: hides what's in every folder.
    CollapseAll,
    CopyRelPath,
    /// Shows it in Finder (File Explorer, Files).
    Reveal,
    /// Opens the system's Get Info (Properties) window for it: on the computer only.
    Properties,
    /// Shows the terminal, in this folder.
    Terminal,
    /// A sidebar folder: opens it in the other pane.
    OtherPane,
    /// A sidebar disk: ejects it.
    Eject,
    /// A sidebar folder: deletes it (asked first).
    DeletePlace,
    /// Runs a Lua script (`Ctx::scripts`' index) on what's picked (or the folder).
    Script(usize),
    /// Runs a Lua script the same way without changing anything: it says what it would change.
    TryScript(usize),
    /// Opens Settings > Scripts.
    ScriptSettings,
}

enum MenuItem {
    Do {
        label: String,
        key: &'static str,
        act: Act,
        danger: bool,
        enabled: bool,
    },
    /// A menu of its own, shown beside the item when the pointer is on it.
    Sub {
        label: String,
        items: Vec<MenuItem>,
    },
    /// A row that does `act`, with a button at its end that does `alt` instead (a script's
    /// Try). `alt_label` says what the button does ("Try script"), for its title and the palette.
    Pair {
        label: String,
        act: Act,
        alt: Act,
        alt_icon: Icon,
        alt_label: &'static str,
    },
    Sep,
}

fn item(label: impl Into<String>, key: &'static str, act: Act) -> MenuItem {
    MenuItem::Do {
        label: label.into(),
        key,
        act,
        danger: false,
        enabled: true,
    }
}

#[derive(Clone, PartialEq)]
enum Notice {
    None,
    Error(String),
    Ok(String),
    /// Asks before deleting these, `forever` (no backup) or with a backup.
    ConfirmDelete(Src, Vec<Entry>, bool),
    /// The result of comparing this pane's folder with the other pane's, before syncing.
    /// `mode` is the one picked at first: "copy", "update" or "mirror".
    Sync {
        report: CompareReport,
        from: (Src, String),
        to: (Src, String),
        mode: String,
    },
    /// Asking for a name; `folder` says whether it's a new folder or a new text file.
    NewItem {
        folder: bool,
    },
    /// A script running (`trial`: changing nothing).
    ScriptRunning {
        name: String,
        trial: bool,
    },
    /// Work on several items under way, `done` of `total` (see `Ctx::working`). Its Stop
    /// button sets `stopping`, and the work stops after the item it's on.
    Working {
        id: u32,
        text: String,
        done: usize,
        total: usize,
        stopping: bool,
    },
    /// What a script did (or, in a trial, would do), which can be run again on `items`.
    ScriptDone {
        script: crate::api::ScriptInfo,
        items: Vec<String>,
        report: crate::api::ScriptReport,
        trial: bool,
    },
}

#[derive(Clone, PartialEq)]
struct Editor {
    src: Src,
    /// The pane it was opened from; it lists the folder again when the editor closes.
    pane: usize,
    path: String,
    /// Version of the file we started from; saving refuses if the file differs now.
    version: String,
    /// Shared by the copies of it (the tab's entry, the shown file).
    saved_text: std::sync::Arc<str>,
    /// The file's lines end in CRLF (most of them, when it has both). The text is edited with LF
    /// (as a `<textarea>` keeps it) and saved with CRLF again.
    crlf: bool,
    /// The file had both LF and CRLF; it's saved with one.
    mixed: bool,
}

/// Where a text editor keeps its file: the full-window editor's (`Ctx::full_slot`) or a pane's
/// Editor view's (`Pane::slot`).
/// The editors' find bar: what's looked for, how, and what Replace puts in.
#[derive(Clone, Copy)]
struct FindState {
    query: RwSignal<String>,
    with: RwSignal<String>,
    opts: RwSignal<find::FindOpts>,
    /// Replace keeps each match's case.
    keep: RwSignal<bool>,
    /// The Replace with box is shown.
    replacing: RwSignal<bool>,
}

#[derive(Clone, Copy)]
struct Slot {
    editor: RwSignal<Option<Editor>>,
    text: RwSignal<String>,
    /// Asking whether to save or discard unsaved changes.
    ask: RwSignal<bool>,
    status: RwSignal<(String, &'static str)>,
    /// The find bar is shown.
    find: RwSignal<bool>,
    /// The pane, for the Editor view.
    pane: Option<Pane>,
}

impl Slot {
    fn dirty(self) -> bool {
        self.editor.with(|e| {
            e.as_ref()
                .is_some_and(|e| self.text.with(|t| *e.saved_text != **t))
        })
    }
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
    marked: RwSignal<HashSet<String>>,
    /// The folder's listing (or search results), before the filter.
    all: RwSignal<Vec<Entry>>,
    /// Shows only names containing this (or matching it, as `opts` say).
    filter: RwSignal<String>,
    /// How the filter matches names: case, whole words, a regular expression.
    opts: RwSignal<find::FindOpts>,
    /// Replace with puts the new text in the case of what it replaces.
    keep_case: RwSignal<bool>,
    /// Why the filter isn't a working regular expression.
    bad_filter: RwSignal<Option<String>>,
    sort: RwSignal<Sort>,
    /// What the list is grouped by.
    group: RwSignal<Group>,
    /// Set while the pane shows what an Everything server's index found under the folder instead
    /// of the folder: the search and whether it found everything.
    found: RwSignal<Option<(String, bool)>>,
    /// The filter's kind, size and modified time (`SEARCH_KINDS`, `SEARCH_SIZES`,
    /// `SEARCH_TIMES` indexes), and whether the filter pane shows.
    search_kind: RwSignal<usize>,
    search_size: RwSignal<usize>,
    search_time: RwSignal<usize>,
    filters_open: RwSignal<bool>,
    /// Whether the filter pane shows the kind, size and time choices.
    search_options: RwSignal<bool>,
    /// The Replace with box is showing under the search box.
    replacing: RwSignal<bool>,
    /// What Replace with puts in place of what the search box matches.
    replace: RwSignal<String>,
    /// While replacing: the shown items whose names change, as path → (new name, what's wrong with
    /// it), or why the names can't be worked out.
    replace_rows: RwSignal<Result<ReplaceRows, String>>,
    /// While comparing with the other pane: each item's mark by name (see `Marks`), and whether
    /// only the differences are shown.
    cmp: RwSignal<Option<HashMap<String, String>>>,
    diff_only: RwSignal<bool>,
    notice: RwSignal<Notice>,
    view: RwSignal<ViewMode>,
    /// Listings of other folders, for the column view's columns to the left of the current one.
    cols: RwSignal<HashMap<String, Vec<Entry>>>,
    /// The folder last shown on each side, so switching between phone and Mac goes back there.
    last: StoredValue<HashMap<Src, String>>,
    /// The column view's first column. Reset to the current folder when the view switches to
    /// columns, when a place in the sidebar is opened, and when the pane leaves it.
    root: RwSignal<String>,
    /// Set before opening a sidebar place: when that folder loads, it becomes the column view's
    /// first column.
    reroot: StoredValue<Option<String>>,
    /// Set when a tab is shown: the side its `root` is on, so landing there from another side
    /// keeps that root.
    root_src: StoredValue<Option<Src>>,
    /// The item whose name is being edited in place.
    renaming: RwSignal<Option<String>>,
    /// Whether the preview is shown to the right of the files.
    preview: RwSignal<bool>,
    /// Widths the user dragged (0: the default): the files beside the preview, and the list's
    /// kind, date and size columns.
    /// The preview's width; 0 is its default (360px, `.preview` in views.css).
    preview_w: RwSignal<f64>,
    /// Widths of the column view's columns, by folder, for this session.
    col_ws: StoredValue<HashMap<String, f64>>,
    list_w: [RwSignal<f64>; 3],
    /// The list's columns after the name, in order (indexes into `LIST_COLS`); missing ones are
    /// hidden.
    list_cols: RwSignal<Vec<usize>>,
    /// Whether the list shows as a tree (listtree.rs), and its open folders' items by path: None
    /// while they're listed.
    list_tree: RwSignal<bool>,
    kids: RwSignal<HashMap<String, Option<Vec<Entry>>>>,
    /// A file the preview shows as text or bytes, whatever its type, from the context menu.
    peek_as: RwSignal<Option<(String, PeekAs)>>,
    /// A file picked in a ZIP's columns, which the preview shows instead of the ZIP: the side
    /// and the ZIP's path, and the file's name in the ZIP and unpacked size.
    zip_pick: RwSignal<Option<(Src, String, String, u64, bool)>>,
    /// What's in the ZIP whose columns are shown (its side and path), for ‹ › in the preview.
    zip_list: StoredValue<Option<((Src, String), Vec<Packed>)>>,
    /// The picture ‹ › went to, being read before it's shown (its path, and the URL read), so the
    /// one shown stays until then.
    coming: RwSignal<Option<(String, String)>>,
    /// Pictures read into memory for ‹ › (the last few, and the next one ahead), by the URL read:
    /// the decoded copy (an object URL) the preview shows them from, None while being read.
    pics: StoredValue<Vec<(String, Option<String>)>>,
    /// What to do once `coming` has been read: show it.
    pending: StoredValue<Option<Box<dyn FnOnce()>>, LocalStorage>,
    /// Whether the path above the files is being typed in.
    editing_path: RwSignal<bool>,
    /// Counts folder listings asked for; a listing that comes back after a newer one was asked
    /// for is dropped, so a slow device can't overwrite where the pane (or tab) went since.
    load_seq: StoredValue<u64>,
    /// The folder the latest listing asked for will show, until it comes back.
    going_to: StoredValue<Option<(Src, String)>>,
    /// The folder was listed a page (`PAGE` items) at a time and has more to load; while the
    /// next page is being loaded; and how many items the pages so far had.
    more: RwSignal<bool>,
    loading_more: RwSignal<bool>,
    got: StoredValue<usize>,
    /// How many items the folder has in all (its listing's length when it was listed at once).
    total: RwSignal<u64>,
    /// The terminal under the pane: whether it's shown, its shell's id while one runs (the
    /// screen itself is in terminal.rs), a count bumped to draw it again, and whether a shell
    /// is being started.
    term: RwSignal<bool>,
    term_id: RwSignal<Option<u32>>,
    term_tick: RwSignal<u64>,
    term_starting: StoredValue<bool>,
    /// The pane's tabs. The shown one's entry is only brought up to date when another is shown;
    /// until then the pane's own signals are where it is.
    tabs: RwSignal<Vec<Tab>>,
    /// Which of `tabs` is shown.
    tab: RwSignal<usize>,
    /// The Editor view's file: the shown tab's file, its text, and whether closing it asks about
    /// unsaved changes.
    doc: RwSignal<Option<Editor>>,
    doc_text: RwSignal<String>,
    doc_ask: RwSignal<bool>,
    doc_status: RwSignal<(String, &'static str)>,
    doc_find: RwSignal<bool>,
    /// The Editor view's tabs. The shown one's entry is only brought up to date when another is
    /// shown; until then `doc` and `doc_text` are where it is.
    docs: RwSignal<Vec<Doc>>,
    /// Which of `docs` is shown.
    doc_at: RwSignal<usize>,
    /// The Editor view's tree: the folders open in it, and its width (0: the default).
    tree_open: RwSignal<HashSet<String>>,
    tree_w: RwSignal<f64>,
    /// Bumped when files on the pane's side change, so the tree's open folders are listed again.
    tree_gen: RwSignal<u32>,
    /// The Editor view's preview tab: a file that isn't text, shown as the preview shows it, and
    /// whether it's the tab shown (over the text file's).
    doc_peek: RwSignal<Option<Entry>>,
    peek_on: RwSignal<bool>,
}

/// A file open in a tab of the Editor view.
#[derive(Clone)]
enum Doc {
    Text {
        ed: Editor,
        /// Its text when it differs from the saved text; None while it's the shown tab (whose
        /// text is in the editor) or unchanged, so an unchanged file in another tab is held once.
        text: Option<String>,
    },
    /// A file that isn't text, kept open from the preview tab: shown as the preview shows it.
    File { src: Src, entry: Entry },
}

impl Doc {
    fn src(&self) -> Src {
        match self {
            Doc::Text { ed, .. } => ed.src,
            Doc::File { src, .. } => *src,
        }
    }

    fn path(&self) -> &str {
        match self {
            Doc::Text { ed, .. } => &ed.path,
            Doc::File { entry, .. } => &entry.path,
        }
    }

    /// For a tab not shown: whether it has unsaved changes.
    fn dirty(&self) -> bool {
        matches!(self, Doc::Text { ed, text: Some(t) } if *t != *ed.saved_text)
    }
}

/// Where a tab was when another was shown.
#[derive(Clone)]
struct Tab {
    src: Src,
    cwd: String,
    selected: Option<String>,
    view: ViewMode,
    root: String,
}

#[derive(Clone, PartialEq)]
enum Panel {
    Backups,
    /// Renaming several items of a pane (by its id).
    Rename(usize, Vec<Entry>),
    /// Finding duplicate files, starting with these folders.
    Dupes(Vec<(Src, String)>),
    /// Importing photos for a pane (by its id): from a folder, into one of the targets.
    Import(usize, (Src, String), Vec<(Src, String)>),
    History,
    /// The books under a folder.
    Library(Src, String),
    /// The command palette, with the text it starts with (":" finds files with fsd).
    Palette(&'static str),
}

/// A side of a job: the computer, or a phone found again by its serial (by its id when it has none,
/// which only holds while it stays connected).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum Side {
    Mac,
    Phone {
        serial: Option<String>,
        id: String,
        label: String,
    },
}

/// What Copy or Cut kept: where it is, its paths, and whether Paste moves them.
#[derive(Clone)]
struct Clip {
    side: Side,
    items: Vec<String>,
    moving: bool,
}

/// What a job does.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum Work {
    /// Copies (or moves) `items` into `to_dir`.
    /// With `replace`, files already there are written over (backed up first).
    Copy {
        from: Side,
        items: Vec<String>,
        to: Side,
        to_dir: String,
        moving: bool,
        #[serde(default)]
        replace: bool,
    },
    /// Copies what's missing from `to_dir` out of `from_dir`. With `mode` "update", also replaces
    /// files that differ; with "mirror", also removes the `remove` items (relative to `to_dir`).
    Sync {
        from: Side,
        from_dir: String,
        to: Side,
        to_dir: String,
        hidden: bool,
        #[serde(default)]
        mode: String,
        #[serde(default)]
        remove: Vec<String>,
    },
    /// Copies (or moves) the photos and videos under `from_dir` into folders by date under
    /// `to_dir`, named by `pattern`.
    Import {
        from: Side,
        from_dir: String,
        to: Side,
        to_dir: String,
        pattern: String,
        moving: bool,
    },
}

#[derive(Clone, PartialEq)]
enum JobState {
    Queued,
    Running,
    /// Not finished when the app last quit: resumed only when asked.
    Paused,
    /// Finished with something left to do (cancelled, or files skipped): what happened.
    Stopped(String),
    Failed(String),
}

/// A copy, move or sync in the queue. One runs at a time, in order.
#[derive(Clone, PartialEq)]
struct Job {
    id: u32,
    work: Work,
    /// The pane its report goes to.
    pane: usize,
    state: JobState,
    progress: Option<Progress>,
    /// Running it again: files already there with the same size count as done.
    resume: bool,
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
    /// Whether the sidebar is shown.
    side: RwSignal<bool>,
    editor: RwSignal<Option<Editor>>,
    text: RwSignal<String>,
    status: RwSignal<(String, &'static str)>,
    confirm_discard: RwSignal<bool>,
    /// The full-window editor's find bar is shown.
    finding: RwSignal<bool>,
    /// What's looked for and how, the same in every editor.
    find_state: FindState,
    busy: RwSignal<bool>,
    ep: crate::epub::Ep,
    menu: RwSignal<Option<Menu>>,
    settings: RwSignal<bool>,
    /// Devices on USB that aren't connected, and the Mac's disks (looked for when something is
    /// plugged in or taken out).
    devices: RwSignal<Option<Devices>>,
    scanning: StoredValue<bool>,
    /// A look was asked for while one couldn't start: done at the next chance.
    scan_again: StoredValue<bool>,
    /// A look asked for with the refresh button is under way.
    looking: RwSignal<bool>,
    /// The device that last failed to connect, to try again.
    failed: StoredValue<Option<UsbDevice>>,
    /// The device being connected (its `usb`).
    connecting: RwSignal<Option<String>>,
    /// A phone whose storage couldn't be read (locked, or access to its files not allowed yet):
    /// it's connected again every `ACCESS_RETRY` until that works, it's unplugged or Stop is
    /// clicked. The timer for the next try.
    awaiting: RwSignal<Option<UsbDevice>>,
    access_timer: StoredValue<Option<TimeoutHandle>, LocalStorage>,
    conn_error: RwSignal<Option<String>>,
    /// Settings: connect phones as soon as they're found, with adb when it can, else MTP.
    auto_connect: RwSignal<bool>,
    /// Settings: read each copy back and compare it with the original.
    verify: RwSignal<bool>,
    /// Comparing the two panes now, and whether by content (else by size).
    comparing: RwSignal<bool>,
    cmp_content: RwSignal<bool>,
    /// The duplicate search's latest progress: (job, text).
    dupe_progress: RwSignal<Option<(u32, String)>>,
    /// Phones (`UsbDevice::key`) connected automatically once already: not again, even after a
    /// failure or a disconnect, until they're unplugged.
    tried: StoredValue<HashSet<String>>,
    /// Phones found by the last look, and how many looks there have been.
    seen: StoredValue<(HashSet<String>, u32)>,
    /// Settings: list names starting with a dot.
    show_hidden: RwSignal<bool>,
    /// Colour code in the text preview.
    colour_code: RwSignal<bool>,
    /// The text preview wraps long lines (else they scroll sideways).
    wrap_text: RwSignal<bool>,
    /// The text preview numbers its lines.
    line_numbers: RwSignal<bool>,
    /// Settings: make thumbnails of phone files (each one reads the whole file over USB).
    phone_thumbs: RwSignal<bool>,
    /// The sidebar's sections hidden with a right-click on it, by key.
    hidden_sections: RwSignal<Vec<String>>,
    /// The Lua scripts for Run script ▸ (scripts.rs), found in the scripts folder and `script_places`.
    scripts: RwSignal<Vec<crate::api::ScriptInfo>>,
    /// Script files and folders added in Settings > Scripts (app paths).
    script_places: RwSignal<Vec<String>>,
    /// The scripts folder picked in Settings > Scripts (an app path); empty: the app's own.
    script_folder: RwSignal<String>,
    /// Why the scripts couldn't be found (the folder picked is gone).
    script_error: RwSignal<Option<String>>,
    /// Scripts' shortcuts: a chord (as keymap.rs writes them) for a script's path.
    script_keys: RwSignal<HashMap<String, String>>,
    /// The script whose shortcut is being typed, in Settings > Scripts; and what the last try said.
    script_capturing: RwSignal<Option<String>>,
    script_key_note: RwSignal<Option<String>>,
    /// What the script running has printed so far (the last lines kept), and how many items
    /// it has changed.
    script_live: RwSignal<(Vec<String>, u32)>,
    /// The sidebar's folders open down to the one the active pane shows, which is scrolled to.
    follow_folder: RwSignal<bool>,
    /// The sidebar's sections, top to bottom (keys of `SECTIONS`).
    section_order: RwSignal<Vec<&'static str>>,
    /// The connections added in Settings > Connections: Everything, SFTP and SMB servers.
    conns: RwSignal<Vec<servers::Conn>>,
    /// Why each connection that isn't connected couldn't be, by id.
    conn_errors: RwSignal<HashMap<String, String>>,
    /// The SMB shares mounted now (by the app or not), and where.
    smb_mounts: RwSignal<Vec<SmbMount>>,
    /// The transfer queue: running, waiting, and stopped ones to retry.
    jobs: RwSignal<Vec<Job>>,
    next_job: StoredValue<u32>,
    favs: RwSignal<Vec<Fav>>,
    /// The sidebar folder being renamed in place.
    side_renaming: RwSignal<Option<(Src, String)>>,
    /// The last folder whose contents changed (and a count, so the same one again is a change):
    /// the sidebar lists it again where it's open.
    side_changed: RwSignal<(u32, Src, String)>,
    sync_pairs: RwSignal<Vec<SyncPair>>,
    /// Named workspaces, in the sidebar.
    workspaces: RwSignal<Vec<Workspace>>,
    /// Free space per storage, by side ("mac", "d1", ...).
    space: RwSignal<HashMap<String, Vec<Space>>>,
    /// What was done this session, newest last, with how to undo it.
    history: RwSignal<Vec<Op>>,
    /// The Backups or History window, when open.
    panel: RwSignal<Option<Panel>>,
    /// What's being dragged from a pane: its side and paths.
    drag: StoredValue<Option<(Src, Vec<String>)>>,
    /// A tab being dragged: its pane and place.
    tab_drag: StoredValue<Option<(usize, usize)>>,
    /// Where a dragged tab would go: the pane, and the place it would take there.
    tab_drop: RwSignal<Option<(usize, usize)>>,
    /// What Copy or Cut kept, for Paste.
    clip: RwSignal<Option<Clip>>,
    /// The computer's files the last Copy put on the system clipboard.
    wrote: StoredValue<Vec<String>>,
    /// Files copied in Finder since then (on the system clipboard), which Paste pastes instead.
    finder: RwSignal<Vec<String>>,
    /// Where a drag would drop now ("pane:folder"), to highlight it.
    drop_at: RwSignal<Option<String>>,
    /// Widths the user dragged (0: the default): the sidebar, and the left pane when there are two.
    side_w: RwSignal<f64>,
    pane_w: RwSignal<f64>,
    /// Settings: "auto", "light" or "dark".
    theme: RwSignal<String>,
    /// An EPUB in the preview shows its chapters, to read, rather than its cover.
    epub_read: RwSignal<bool>,
    /// ⌘F while reading a book: the pane whose preview it's in, and a count that goes up each time.
    read_find: RwSignal<(usize, u32)>,
    /// The user's keyboard shortcuts, by command, where they differ from the defaults (keymap.rs).
    keymap: RwSignal<HashMap<String, Vec<String>>>,
    /// The command waiting for a key press in the Keyboard shortcuts dialog.
    capturing: RwSignal<Option<&'static str>>,
    /// The section shown in Settings.
    settings_tab: RwSignal<&'static str>,
    /// Whether the Connect over Wi-Fi dialog is shown.
    wifi: RwSignal<bool>,
}

/// A chapter's HTML for the EPUB editor's visual view at `zoom` percent, in the colours of the
/// app's theme as when it's read. The theme is read once: following it later would reload the
/// page and lose what was typed.
pub(crate) fn editor_page(html: &str, zoom: u32) -> String {
    let theme = expect_context::<Ctx>().theme.get_untracked();
    reading::page(
        html,
        &reading::page_style(&theme, 15, zoom, "16px 20px 60px", "1.55"),
        None,
    )
}
