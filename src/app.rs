//! The file browser UI: connect screen, one or two panes of folders (each on the phone or the Mac,
//! in a list, thumbnail or column view, with a context menu), and a text editor.

use crate::api::{call, Backup, BookInfo, CompareReport, DupeProgress, Dupes, Marks, Issue, Devices, Space, Entry, FileDrop, Info, Progress, SmbMount, TextFile, TransferReport, UsbDevice, WriteResult};
use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use wasm_bindgen::JsCast;

mod batch;
mod books;
mod compare;
mod devices;
mod everything;
mod servers;
mod dupes;
mod editor;
mod code;
mod files;
mod filetype;
mod find;
mod import;
mod icons;
mod keymap;
mod layout;
mod listtree;
mod menu;
mod nav;
mod near;
mod palette;
mod pane;
mod preview;
mod rows;
mod safety;
mod search;
mod connections;
mod terminal;
mod sidebar;
mod transfer;
mod undo;
mod util;
mod video;
mod views;
mod wifi;
mod workspace;

use batch::*;
use books::*;
use compare::*;
use dupes::*;
use import::*;
use devices::*;
use editor::*;
use code::*;
use filetype::*;
pub(crate) use icons::{ico, Icon};
/// For the EPUB editor's Source view.
pub(crate) use editor::{code_html, shift_runs, LAYER_MAX};
pub(crate) use util::{store, stored_flag};
use keymap::*;
use layout::*;
use menu::*;
use palette::*;
use pane::*;
use preview::*;
use safety::*;
use search::*;
use connections::*;
use terminal::*;
use sidebar::*;
use transfer::*;
use video::*;
pub use video::{popped_out, PopOut};
use undo::*;
use util::*;
use views::*;
use workspace::*;
use wifi::*;

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
    /// A folder tree beside a text editor with tabs, as in a code editor.
    Code,
}

impl ViewMode {
    const ALL: [ViewMode; 4] = [ViewMode::List, ViewMode::Thumbs, ViewMode::Columns, ViewMode::Code];

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
        format!("ferry-files.sort.{pane}")
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
const SYNC_MODES: [(&str, &str); 3] = [("copy", "Copy missing"), ("update", "Update"), ("mirror", "Mirror")];
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
    /// Shows the terminal, in this folder.
    Terminal,
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
    /// `mode` is the one picked at first: "copy", "update" or "mirror".
    Sync { report: CompareReport, from: (Src, String), to: (Src, String), mode: String },
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
        self.editor.with(|e| e.as_ref().is_some_and(|e| self.text.with(|t| *e.saved_text != **t)))
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
    /// Shows only names containing this (or matching it, with `regex`).
    filter: RwSignal<String>,
    /// The filter and search take a regular expression.
    regex: RwSignal<bool>,
    /// Why the filter isn't a working regular expression.
    bad_filter: RwSignal<Option<String>>,
    sort: RwSignal<Sort>,
    /// Set while the pane shows search results instead of the folder: the search and whether it
    /// found everything.
    found: RwSignal<Option<(String, bool)>>,
    /// The search's filters (`SEARCH_KINDS`, `SEARCH_SIZES`, `SEARCH_TIMES` indexes) and
    /// whether their row is shown.
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

impl Pane {
    /// Whether any search filter is set.
    fn filtering(self) -> bool {
        self.search_kind.get() + self.search_size.get() + self.search_time.get() > 0
    }

    fn new(id: usize) -> Self {
        Pane {
            id,
            src: RwSignal::new(Src::Mac),
            cwd: RwSignal::new(String::new()),
            entries: RwSignal::new(vec![]),
            loading: RwSignal::new(false),
            selected: RwSignal::new(None),
            marked: RwSignal::new(HashSet::new()),
            all: RwSignal::new(vec![]),
            filter: RwSignal::new(String::new()),
            regex: RwSignal::new(stored_flag(&format!("{REGEX_KEY}.{id}"), false)),
            bad_filter: RwSignal::new(None),
            sort: RwSignal::new(Sort::stored(id)),
            found: RwSignal::new(None),
            search_kind: RwSignal::new(0),
            search_size: RwSignal::new(0),
            search_time: RwSignal::new(0),
            filters_open: RwSignal::new(false),
            search_options: RwSignal::new(stored_flag(&format!("{OPTIONS_KEY}.{id}"), false)),
            replacing: RwSignal::new(false),
            replace: RwSignal::new(String::new()),
            replace_rows: RwSignal::new(Ok(HashMap::new())),
            cmp: RwSignal::new(None),
            diff_only: RwSignal::new(false),
            notice: RwSignal::new(Notice::None),
            view: RwSignal::new(ViewMode::stored(id)),
            cols: RwSignal::new(HashMap::new()),
            last: StoredValue::new(HashMap::new()),
            root: RwSignal::new(String::new()),
            reroot: StoredValue::new(None),
            root_src: StoredValue::new(None),
            renaming: RwSignal::new(None),
            preview: RwSignal::new(stored_flag(&format!("ferry-files.preview.{id}"), true)),
            preview_w: stored_px(&format!("ferry-files.preview-w.{id}")),
            col_ws: StoredValue::new(HashMap::new()),
            list_w: [0, 1, 2].map(|n| stored_px(&format!("ferry-files.list-w.{id}.{n}"))),
            list_cols: RwSignal::new(stored_cols(id)),
            list_tree: RwSignal::new(stored_flag(&format!("ferry-files.list-tree.{id}"), false)),
            kids: RwSignal::new(HashMap::new()),
            peek_as: RwSignal::new(None),
            zip_pick: RwSignal::new(None),
            editing_path: RwSignal::new(false),
            load_seq: StoredValue::new(0),
            going_to: StoredValue::new(None),
            more: RwSignal::new(false),
            loading_more: RwSignal::new(false),
            got: StoredValue::new(0),
            total: RwSignal::new(0),
            term: RwSignal::new(false),
            term_id: RwSignal::new(None),
            term_tick: RwSignal::new(0),
            term_starting: StoredValue::new(false),
            tabs: RwSignal::new(vec![Tab { src: Src::Mac, cwd: String::new(), selected: None, view: ViewMode::stored(id), root: String::new() }]),
            tab: RwSignal::new(0),
            doc: RwSignal::new(None),
            doc_text: RwSignal::new(String::new()),
            doc_ask: RwSignal::new(false),
            doc_status: RwSignal::new((String::new(), "")),
            doc_find: RwSignal::new(false),
            docs: RwSignal::new(vec![]),
            doc_at: RwSignal::new(0),
            tree_open: RwSignal::new(HashSet::new()),
            tree_w: stored_px(&format!("ferry-files.tree-w.{id}")),
            tree_gen: RwSignal::new(0),
            doc_peek: RwSignal::new(None),
            peek_on: RwSignal::new(false),
        }
    }

    /// Where the Editor view keeps its file.
    fn slot(self) -> Slot {
        Slot { editor: self.doc, text: self.doc_text, ask: self.doc_ask, status: self.doc_status, find: self.doc_find, pane: Some(self) }
    }

    fn error(self, msg: impl Into<String>) {
        self.notice.set(Notice::Error(msg.into()));
    }

    fn selected_entry(self) -> Option<Entry> {
        let sel = self.selected.get()?;
        self.entries
            .with(|es| es.iter().find(|e| e.path == sel).cloned())
            .or_else(|| self.kids.with(|k| k.values().flatten().flat_map(|v| v.iter()).find(|e| e.path == sel).cloned()))
    }

    /// Sorts and filters `all` into `entries`, dropping marks on items no longer shown.
    fn arrange(self) {
        let f = self.filter.get_untracked();
        let mut bad = None;
        // What the box asks for: a pattern, or else text the name contains in any case.
        let mut re = None;
        let mut text = None;
        if !f.is_empty() {
            if self.regex.get_untracked() {
                // A pattern that doesn't work filters nothing out, and says why.
                match regex(&f, true) {
                    Ok(r) => re = Some(r),
                    Err(e) => bad = Some(e),
                }
            } else if f.contains('*') || f.contains('?') {
                // A pattern, as the search takes it: * any run of characters, ? one.
                re = regex(&glob_source(&f), true).ok();
            } else {
                text = Some(f.to_lowercase());
            }
        }
        if self.bad_filter.get_untracked() != bad {
            self.bad_filter.set(bad);
        }
        let diff_only = self.diff_only.get_untracked();
        // Filtered before sorting, and only what's kept is copied.
        let mut es: Vec<Entry> = self.cmp.with_untracked(|cmp| {
            let cmp = cmp.as_ref().filter(|_| diff_only);
            self.all.with_untracked(|all| {
                all.iter()
                    .filter(|e| re.as_ref().is_none_or(|re| re.is_match(&e.name)))
                    .filter(|e| text.as_deref().is_none_or(|t| contains_any_case(&e.name, t)))
                    .filter(|e| cmp.is_none_or(|m| m.get(&e.name).is_some_and(|x| x != "same")))
                    .cloned()
                    .collect()
            })
        });
        self.sort.get_untracked().apply(&mut es);
        if !self.marked.with_untracked(HashSet::is_empty) {
            // Items in folders open in the tree stay marked too.
            let kids = self.kids.get_untracked();
            let shown: HashSet<&str> = es.iter().chain(kids.values().flatten().flatten()).map(|e| e.path.as_str()).collect();
            self.marked.update(|m| m.retain(|x| shown.contains(x.as_str())));
        }
        self.entries.set(es);
    }

    fn set_sort(self, key: SortKey) {
        let Sort(now, asc) = self.sort.get_untracked();
        // Clicking the same column again reverses it; dates and sizes start biggest first.
        let s = if now == key { Sort(key, !asc) } else { Sort(key, matches!(key, SortKey::Name | SortKey::Kind)) };
        self.sort.set(s);
        store(&Sort::storage_key(self.id), &format!("{}:{}", s.0.key(), if s.1 { 1 } else { 0 }));
        self.cols.update(|c| c.values_mut().for_each(|es| s.apply(es)));
        self.kids.update(|k| k.values_mut().flatten().for_each(|es| s.apply(es)));
        self.arrange();
    }

    /// Whether `path` is one of the items actions act on (tracked, for highlighting).
    fn is_picked(self, path: &str) -> bool {
        let marked = self.marked.with(|m| (!m.is_empty()).then(|| m.contains(path)));
        marked.unwrap_or_else(|| self.selected.get().as_deref() == Some(path))
    }

    /// The items actions act on: the marked ones, or else the selected one.
    fn picked(self) -> Vec<Entry> {
        let marked = self.marked.get_untracked();
        let sel = self.selected.get_untracked();
        self.shown_rows()
            .into_iter()
            .filter(|e| if marked.is_empty() { sel.as_deref() == Some(e.path.as_str()) } else { marked.contains(&e.path) })
            .collect()
    }

    /// Selects one item, unmarking the rest.
    fn select_only(self, path: Option<String>) {
        self.marked.set(HashSet::new());
        self.selected.set(path);
    }

    /// A click on an item: ⌘ adds or removes it, Shift picks everything from the selected item to it.
    fn click(self, ev: &web_sys::MouseEvent, path: &str) {
        if crate::os::cmd(ev.meta_key(), ev.ctrl_key()) {
            let sel = self.selected.get_untracked();
            self.marked.update(|m| {
                if m.is_empty() {
                    m.extend(sel);
                }
                if !m.remove(path) {
                    m.insert(path.to_string());
                }
            });
            self.selected.set(Some(path.to_string()));
        } else if ev.shift_key() && self.selected.get_untracked().is_some() {
            let anchor = self.selected.get_untracked().unwrap_or_default();
            let es = self.shown_rows();
            let at = |p: &str| es.iter().position(|e| e.path == p);
            let range = match (at(&anchor), at(path)) {
                (Some(a), Some(b)) => es[a.min(b)..=a.max(b)].iter().map(|e| e.path.clone()).collect(),
                _ => HashSet::from([path.to_string()]),
            };
            self.marked.set(range);
        } else {
            self.select_only(Some(path.to_string()));
        }
    }
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
    /// The command palette.
    Palette,
    /// Reading a book; the folder is the library to go back to.
    Reader(Src, String, Option<String>),
}

/// A side of a job: the computer, or a phone found again by its serial (by its id when it has none,
/// which only holds while it stays connected).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum Side {
    Mac,
    Phone { serial: Option<String>, id: String, label: String },
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
    Import { from: Side, from_dir: String, to: Side, to_dir: String, pattern: String, moving: bool },
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
    /// The last searches (Enter in the filter), newest first, offered as the filter is typed in.
    searches: RwSignal<Vec<String>>,
    /// Settings: "auto", "light" or "dark".
    theme: RwSignal<String>,
    /// The user's keyboard shortcuts, by command, where they differ from the defaults (keymap.rs).
    keymap: RwSignal<HashMap<String, Vec<String>>>,
    /// The command waiting for a key press in the Keyboard shortcuts dialog.
    capturing: RwSignal<Option<&'static str>>,
    /// The section shown in Settings.
    settings_tab: RwSignal<&'static str>,
    /// Whether the Connect over Wi-Fi dialog is shown.
    wifi: RwSignal<bool>,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            phones: RwSignal::new(vec![]),
            mac: RwSignal::new(None),
            panes: [Pane::new(0), Pane::new(1)],
            active: RwSignal::new(0),
            split: RwSignal::new(stored(SPLIT_KEY).as_deref() == Some("1")),
            side: RwSignal::new(stored_flag(SIDE_KEY, true)),
            editor: RwSignal::new(None),
            text: RwSignal::new(String::new()),
            status: RwSignal::new((String::new(), "")),
            confirm_discard: RwSignal::new(false),
            finding: RwSignal::new(false),
            find_state: FindState {
                query: RwSignal::new(String::new()),
                with: RwSignal::new(String::new()),
                opts: RwSignal::new(find::FindOpts::default()),
                keep: RwSignal::new(false),
                replacing: RwSignal::new(false),
            },
            busy: RwSignal::new(false),
            ep: crate::epub::Ep::new(),
            menu: RwSignal::new(None),
            settings: RwSignal::new(false),
            devices: RwSignal::new(None),
            scanning: StoredValue::new(false),
            scan_again: StoredValue::new(false),
            looking: RwSignal::new(false),
            failed: StoredValue::new(None),
            connecting: RwSignal::new(None),
            awaiting: RwSignal::new(None),
            access_timer: StoredValue::new_local(None),
            conn_error: RwSignal::new(None),
            show_hidden: RwSignal::new(stored_flag(HIDDEN_KEY, false)),
            colour_code: RwSignal::new(stored_flag(COLOUR_KEY, true)),
            wrap_text: RwSignal::new(stored_flag(WRAP_KEY, true)),
            line_numbers: RwSignal::new(stored_flag(LINE_NUMBERS_KEY, true)),
            phone_thumbs: RwSignal::new(stored_flag(PHONE_THUMBS_KEY, true)),
            section_order: RwSignal::new(sidebar::section_order(
                &stored(SECTION_ORDER_KEY).and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default(),
            )),
            // Only sections there still are (Everything is now part of Servers).
            hidden_sections: RwSignal::new(
                stored(HIDDEN_SECTIONS_KEY)
                    .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|k| sidebar::SECTIONS.iter().any(|s| s.0 == k))
                    .collect(),
            ),
            conns: RwSignal::new(servers::stored_conns()),
            conn_errors: RwSignal::new(HashMap::new()),
            smb_mounts: RwSignal::new(vec![]),
            auto_connect: RwSignal::new(stored_flag(AUTO_CONNECT_KEY, true)),
            verify: RwSignal::new(stored_flag(VERIFY_KEY, false)),
            comparing: RwSignal::new(false),
            cmp_content: RwSignal::new(false),
            dupe_progress: RwSignal::new(None),
            tried: StoredValue::new(HashSet::new()),
            seen: StoredValue::new((HashSet::new(), 0)),
            jobs: RwSignal::new(vec![]),
            next_job: StoredValue::new(1),
            space: RwSignal::new(HashMap::new()),
            history: RwSignal::new(vec![]),
            panel: RwSignal::new(None),
            favs: RwSignal::new(stored(FAVS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()),
            sync_pairs: RwSignal::new(stored(SYNC_PAIRS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()),
            workspaces: RwSignal::new(saved_workspaces()),
            drag: StoredValue::new(None),
            clip: RwSignal::new(None),
            wrote: StoredValue::new(vec![]),
            finder: RwSignal::new(vec![]),
            drop_at: RwSignal::new(None),
            side_w: stored_px(SIDE_W_KEY),
            pane_w: stored_px(PANE_W_KEY),
            searches: RwSignal::new(stored(SEARCHES_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()),
            theme: RwSignal::new(stored(THEME_KEY).unwrap_or_else(|| "auto".into())),
            keymap: RwSignal::new(saved_keys()),
            capturing: RwSignal::new(None),
            settings_tab: RwSignal::new("general"),
            wifi: RwSignal::new(false),
        }
    }

    /// Settings, at the keyboard shortcuts.
    fn show_shortcuts(self) {
        self.settings_tab.set("keys");
        self.settings.set(true);
    }

    /// Drops hidden files from a listing unless the settings say to show them.
    fn visible(self, entries: Vec<Entry>) -> Vec<Entry> {
        if self.show_hidden.get_untracked() {
            entries
        } else {
            entries.into_iter().filter(|e| !e.is_hidden()).collect()
        }
    }

    fn set_show_hidden(self, on: bool) {
        self.show_hidden.set(on);
        store(HIDDEN_KEY, if on { "1" } else { "0" });
        self.reload_all();
    }

    fn set_colour_code(self, on: bool) {
        self.colour_code.set(on);
        store(COLOUR_KEY, if on { "1" } else { "0" });
    }

    fn set_wrap_text(self, on: bool) {
        self.wrap_text.set(on);
        store(WRAP_KEY, if on { "1" } else { "0" });
    }

    fn set_line_numbers(self, on: bool) {
        self.line_numbers.set(on);
        store(LINE_NUMBERS_KEY, if on { "1" } else { "0" });
    }

    fn set_hidden_sections(self, keys: Vec<String>) {
        store(HIDDEN_SECTIONS_KEY, &serde_json::to_string(&keys).unwrap_or_default());
        self.hidden_sections.set(keys);
    }

    fn set_section_order(self, order: Vec<&'static str>) {
        store(SECTION_ORDER_KEY, &serde_json::to_string(&order).unwrap_or_default());
        self.section_order.set(order);
    }

    fn set_phone_thumbs(self, on: bool) {
        self.phone_thumbs.set(on);
        store(PHONE_THUMBS_KEY, if on { "1" } else { "0" });
    }

    fn set_verify(self, on: bool) {
        self.verify.set(on);
        store(VERIFY_KEY, if on { "1" } else { "0" });
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
            Src::Mac => crate::os::the_computer().into(),
            Src::Dev(_) => self.src_info(src).map_or_else(|| "the device".into(), |i| i.label),
        }
    }

    fn dirty(&self) -> bool {
        self.full_slot().dirty()
    }

    /// Where the full-window editor keeps its file.
    fn full_slot(self) -> Slot {
        Slot { editor: self.editor, text: self.text, ask: self.confirm_discard, status: self.status, find: self.finding, pane: None }
    }
}

#[component]
pub fn App() -> impl IntoView {
    rename_stored_keys();
    let ctx = Ctx::new();
    apply_theme(&ctx.theme.get_untracked());
    provide_context(ctx);
    provide_context(VideoPrefs::load());
    let ep = ctx.ep;
    provide_context(ep);
    // Saving a book writes the file and its backup, so the panes are listed again.
    Effect::new(move |was: Option<Option<(u32, bool)>>| {
        let now = ep.book.with(|b| b.as_ref().map(|b| (b.id, b.dirty)));
        if let (Some(Some((id, true))), Some((id2, false))) = (was, now) {
            if id == id2 {
                ctx.reload_all();
            }
        }
        now
    });

    // Pick up existing device connections after a webview reload, then bring back the last
    // session's tabs (or start on the remembered folders).
    spawn_local(async move {
        if let Ok(Some(mac)) = call::<Option<Info>>("info", json!({ "src": "mac" })).await {
            ctx.mac.set(Some(mac));
        }
        for info in call::<Vec<Info>>("connections", json!({})).await.unwrap_or_default() {
            ctx.connected(info, false);
        }
        ctx.restore_session();
        ctx.check_conns();
    });
    ctx.keep_session();
    ctx.watch_terminals();
    // Look for devices and disks now, then when the backend says something was plugged in or taken
    // out. Every few seconds as well while a phone waits for USB debugging to be allowed (that's
    // not a USB change), after a look that couldn't start, and, when USB can't be watched, while
    // no phone is connected.
    ctx.scan();
    ctx.reconnect_wifi();
    ctx.load_jobs();
    let watching = StoredValue::new(false);
    spawn_local(async move {
        if let Ok(on) = call::<bool>("watch_devices", json!({})).await {
            watching.set_value(on);
        }
    });
    crate::api::listen("devices-changed", move |_: ()| ctx.scan());
    let _ = set_interval_with_handle(
        move || {
            let waiting = ctx.devices.with_untracked(|d| d.as_ref().is_some_and(|d| d.adb_waiting));
            let unwatched = !watching.get_value() && !ctx.has_phone();
            if waiting || unwatched || ctx.scan_again.get_value() {
                ctx.scan();
            }
        },
        std::time::Duration::from_secs(3),
    );
    let _ = set_interval_with_handle(
        move || ctx.check_conns(),
        everything::EVERYTHING_EVERY,
    );

    // Keyboard. ⌘S saves in the editors. Elsewhere the shortcuts are the user's (keymap.rs), but
    // for arrows, which move (←/→ also go up/in in the column view and move across in the
    // thumbnail view), and Escape, which unpicks.
    // Letting go of the forward key: a skip, or the end of 2×.
    let _keys_up = window_event_listener(ev::keyup, move |e| {
        let bound = chord_of(&e).and_then(|c| ctx.command_for(&c));
        if bound.is_some_and(|b| b.id == "media_forward") && media_key(ctx.act().id, MediaKey::ForwardUp) {
            e.prevent_default();
        }
    });
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
        let chord = chord_of(&e);
        let bound = chord.as_deref().and_then(|c| ctx.command_for(c));
        if ctx.settings.get_untracked() {
            // Changing a shortcut: the next key press is the new one; Esc stops waiting.
            if let Some(id) = ctx.capturing.get_untracked() {
                if let Some(c) = chord {
                    e.prevent_default();
                    if c != "Escape" {
                        ctx.bind(id, c);
                    }
                    ctx.capturing.set(None);
                }
                return;
            }
            if e.key() == "Escape" {
                e.prevent_default();
                ctx.settings.set(false);
            }
            return;
        }
        // ⌘S in the preview's book editor saves the book.
        if meta && e.key() == "s" && ep.book.with_untracked(|b| b.is_some()) {
            let in_book = document()
                .active_element()
                .is_some_and(|el| el.closest(".epub-edit, .epub-bar").ok().flatten().is_some());
            if in_book {
                e.prevent_default();
                ep.save();
                return;
            }
        }
        if ctx.editor.get_untracked().is_some() {
            if meta && e.key() == "s" {
                e.prevent_default();
                ctx.save();
            }
            return;
        }
        let p = ctx.act();
        // The Editor view: ⌘S saves, ⌘W closes the file's tab, and only shortcuts that work
        // anywhere do the rest; the others act on files, which aren't shown.
        if p.view.get_untracked() == ViewMode::Code && p.notice.with_untracked(|n| matches!(n, Notice::None | Notice::Ok(_) | Notice::Error(_))) {
            if e.default_prevented() {
                return;
            }
            let id = bound.map(|b| b.id);
            if meta && e.key() == "s" {
                e.prevent_default();
                ctx.save_in(p.slot());
            } else if meta && matches!(e.key().as_str(), "f" | "ƒ") && p.doc.with_untracked(Option::is_some) {
                // Find in the shown file, from anywhere in the pane (the editor's own ⌘F takes the selection).
                e.prevent_default();
                if e.alt_key() {
                    ctx.find_state.replacing.set(true);
                }
                p.doc_find.set(true);
            } else if id == Some("rename") {
                // The tree's row with the focus, or else the shown file. Not from a field: the
                // rename box is one.
                let focused = document().active_element();
                let in_field = focused.as_ref().is_some_and(|el| el.tag_name() == "INPUT");
                let row = focused
                    .and_then(|el| el.closest(".code-tree .tree-row").ok().flatten())
                    .and_then(|row| row.get_attribute("data-path"));
                let shown = || {
                    if p.peek_on.get_untracked() {
                        p.doc_peek.with_untracked(|e| e.as_ref().map(|e| e.path.clone()))
                    } else {
                        p.docs.with_untracked(|ds| ds.get(p.doc_at.get_untracked()).map(|d| d.path().to_string()))
                    }
                };
                if !in_field {
                    if let Some(path) = row.or_else(shown) {
                        e.prevent_default();
                        p.renaming.set(Some(path));
                    }
                }
                return;
            } else if id == Some("close_tab") && p.peek_on.get_untracked() {
                e.prevent_default();
                ctx.close_peek(p);
            } else if id == Some("close_tab") && p.docs.with_untracked(|ds| !ds.is_empty()) {
                e.prevent_default();
                ctx.close_doc(p, p.doc_at.get_untracked(), false);
            } else if bound.is_some_and(|b| b.anywhere && b.id != "replace") && chord.as_deref().is_some_and(held) {
                // Falls through to the shortcut below.
            } else {
                return;
            }
        }
        if !matches!(p.notice.get_untracked(), Notice::None | Notice::Ok(_) | Notice::Error(_)) {
            return;
        }
        // Typing in a field (the filter) is the field's, but for shortcuts with ⌘ or ⌃ that work
        // anywhere: tabs, the palette and the sidebar.
        let typing = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest("input, textarea").ok().flatten().is_some());
        if typing && !bound.is_some_and(|b| b.anywhere && chord.as_deref().is_some_and(held)) {
            return;
        }
        let entries = p.shown_rows();
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
        // Tiles per row in the thumbnail view: the grid's columns.
        let per_row = || {
            document()
                .query_selector(".pane.active .thumbs")
                .ok()
                .flatten()
                .and_then(|g| window().get_computed_style(&g).ok().flatten())
                .and_then(|s| s.get_property_value("grid-template-columns").ok())
                .map_or(1, |v| v.split_whitespace().count().max(1) as isize)
        };
        if let Some(b) = bound {
            let done = match b.id {
                "palette" => {
                    ctx.panel.set(Some(Panel::Palette));
                    true
                }
                "new_tab" => {
                    ctx.new_tab(p, p.src.get_untracked(), p.cwd.get_untracked());
                    true
                }
                // The last tab is left to the window (the menu's Close Window).
                "close_tab" if p.tabs.with_untracked(Vec::len) > 1 => {
                    ctx.close_tab(p, p.tab.get_untracked());
                    true
                }
                "next_tab" | "prev_tab" => {
                    ctx.step_tab(p, if b.id == "next_tab" { 1 } else { -1 });
                    true
                }
                "sidebar" => {
                    ctx.set_side(!ctx.side.get_untracked());
                    true
                }
                "other_pane" if ctx.split.get_untracked() => {
                    ctx.active.set(1 - p.id);
                    true
                }
                "split" => {
                    ctx.set_split(!ctx.split.get_untracked());
                    true
                }
                "hidden" => {
                    ctx.set_show_hidden(!ctx.show_hidden.get_untracked());
                    true
                }
                "terminal" => {
                    ctx.toggle_terminal(p);
                    true
                }
                "copy" | "move" => {
                    ctx.send_to_other(p, b.id == "move");
                    true
                }
                // Not while text is selected (in a preview): ⌘C copies that.
                "copy_items" | "cut_items" if !text_selected() => {
                    ctx.copy_items(p, b.id == "cut_items");
                    true
                }
                "paste" => {
                    ctx.paste(p, None);
                    true
                }
                "rename" => {
                    ctx.ask_rename(p);
                    true
                }
                "replace_names" => {
                    ctx.replace_names(p);
                    true
                }
                "undo" => {
                    ctx.undo_last();
                    true
                }
                "pick_all" if select_all_peek() => true,
                "pick_all" => {
                    p.marked.set(entries.iter().map(|x| x.path.clone()).collect());
                    true
                }
                "quick_look" => {
                    ctx.quick_look(p);
                    true
                }
                "media_play" => media_key(p.id, MediaKey::Play),
                "media_back" => media_key(p.id, MediaKey::Back),
                // Held: once, then repeats, which the player ignores until it's let go.
                "media_forward" => e.repeat() || media_key(p.id, MediaKey::ForwardDown),
                "media_mute" => media_key(p.id, MediaKey::Mute),
                "filter" => {
                    ctx.open_search(p, false);
                    true
                }
                "replace" => {
                    ctx.toggle_replace(p);
                    true
                }
                "delete" => {
                    ctx.ask_delete(p);
                    true
                }
                "help" => {
                    ctx.show_shortcuts();
                    true
                }
                "type_path" => {
                    p.editing_path.set(true);
                    true
                }
                "open" => match current.clone() {
                    Some(x) => {
                        ctx.open(p, x);
                        true
                    }
                    None => false,
                },
                "up" => {
                    ctx.go_up(p);
                    true
                }
                _ => false,
            };
            if done {
                e.prevent_default();
                return;
            }
        }
        // Moving around isn't changeable.
        match (e.key().as_str(), meta) {
            ("Escape", false) => p.marked.set(HashSet::new()),
            // In the list, ← and → go to the previous and next picture, video or sound.
            ("ArrowRight", false) | ("ArrowLeft", false) if view == ViewMode::List => {
                if let Some(x) = current.clone().filter(|x| media(x).is_some()) {
                    e.prevent_default();
                    step_media(p, &x, if e.key() == "ArrowRight" { 1 } else { -1 });
                } else if let Some(x) = current.clone().filter(|_| untrack(|| p.tree_on())) {
                    e.prevent_default();
                    ctx.tree_key(p, &x, e.key() == "ArrowRight", &entries);
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
    // Back from Finder, maybe with files copied there to paste.
    let _focus = window_event_listener(ev::focus, move |_| ctx.check_clipboard());
    ctx.check_clipboard();
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

    crate::api::listen("dupes", move |d: DupeProgress| ctx.dupe_progress.set(Some((d.job, d.text))));
    crate::api::listen("transfer", move |pr: Progress| {
        ctx.jobs.update(|js| {
            if let Some(j) = js.iter_mut().find(|j| j.id == pr.job) {
                j.progress = Some(pr);
            }
        });
    });
    // Files dragged in from the file manager are copied to the folder (or pane) they're dropped on,
    // except subtitles dropped on a video.
    crate::api::listen("tauri://drag-drop", move |d: FileDrop| {
        let dpr = window().device_pixel_ratio().max(1.0);
        let Some(el) = document().element_from_point((d.position.x / dpr) as f32, (d.position.y / dpr) as f32) else { return };
        // Subtitle files dropped on a video are shown with it.
        if video::drop_on_player(&el, Src::Mac, d.paths.iter().map(|f| crate::os::app_path(f)).collect()) {
            return;
        }
        let Some(pane_el) = el.closest("[data-pane]").ok().flatten() else { return };
        let Some(p) = pane_el.get_attribute("data-pane").and_then(|i| i.parse::<usize>().ok()).and_then(|i| ctx.panes.get(i).copied()) else { return };
        let dir = el.closest("[data-dir]").ok().flatten().and_then(|f| f.get_attribute("data-dir")).unwrap_or_else(|| p.cwd.get_untracked());
        ctx.active.set(p.id);
        let paths = d.paths.iter().map(|f| crate::os::app_path(f)).collect();
        ctx.transfer(p, Src::Mac, paths, p.src.get_untracked(), dir, false);
    });

    // Keep the item last picked visible when the keyboard moves the selection. That item, not
    // the first one picked: with several picked, ⌘-clicking one mustn't scroll to another. Only a
    // pane whose selection (or view) changed scrolls: clicking into a pane makes it active first,
    // and scrolling then would move the list to the old selection under the pointer.
    Effect::new(move |prev: Option<[(Option<String>, ViewMode); 2]>| {
        let now = ctx.panes.map(|p| (p.selected.get(), p.view.get()));
        for (i, (path, _)) in now.iter().enumerate() {
            let Some(path) = path.clone() else { continue };
            if prev.as_ref().is_some_and(|prev| prev[i] == now[i]) {
                continue;
            }
            request_animation_frame(move || {
                let Ok(found) = document().query_selector_all(&format!(".pane[data-pane=\"{i}\"] [data-path]")) else { return };
                let el = (0..found.length())
                    .filter_map(|n| found.item(n).and_then(|e| e.dyn_into::<web_sys::Element>().ok()))
                    .find(|e| e.get_attribute("data-path").as_deref() == Some(path.as_str()));
                if let Some(el) = el {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                    opts.set_inline(web_sys::ScrollLogicalPosition::Nearest);
                    el.scroll_into_view_with_scroll_into_view_options(&opts);
                } else if let Some(sc) = document().query_selector(&format!(".pane[data-pane=\"{i}\"] [data-rows]")).ok().flatten() {
                    // Not rendered, being out of view in a long list: scroll to where it would be.
                    if let Some(idx) = ctx.panes[i].shown_rows().iter().position(|e| e.path == path) {
                        rows::reveal(i, &sc, idx);
                    }
                }
            });
        }
        now
    });

    view! {
        <Browser/>
        <ContextMenu/>
        {move || ctx.settings.get().then(|| view! { <Settings/> })}
        {move || ctx.wifi.get().then(|| view! { <WifiDialog/> })}
        {move || ctx.panel.get().map(|w| match w {
            Panel::Backups => view! { <Backups/> }.into_any(),
            Panel::History => view! { <History/> }.into_any(),
            Panel::Rename(pane, items) => view! { <BatchRename pane=pane items=items/> }.into_any(),
            Panel::Dupes(folders) => view! { <Duplicates folders=folders/> }.into_any(),
            Panel::Import(pane, from, targets) => view! { <ImportPhotos pane from targets/> }.into_any(),
            Panel::Library(src, dir) => view! { <Library src=src dir=dir/> }.into_any(),
            Panel::Palette => view! { <Palette/> }.into_any(),
            Panel::Reader(src, path, back) => view! { <Reader src=src path=path back=back/> }.into_any(),
        })}
    }
}

#[component]
fn Browser() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // Only opening and closing the editor rebuilds the main area, not saving in it.
    let editing = Memo::new(move |_| ctx.editor.with(Option::is_some));
    move || view! {
        <div class="app browse" class:no-side=move || !ctx.side.get() style=move || match ctx.side_w.get() {
            w if w > 0.0 && ctx.side.get() => format!("grid-template-columns: auto {}px auto 1fr", w.max(SIDE_MIN)),
            _ => String::new(),
        }>
            <ActivityBar/>
            {move || ctx.side.get().then(|| view! {
                <Sidebar/>
                <Grip size=ctx.side_w key=SIDE_W_KEY.into() min=SIDE_MIN max=480.0 room=480.0 class="grip side-grip"/>
            })}
            <div class="main">
                {move || if editing.get() { view! { <TextEditor file=ctx.full_slot()/> }.into_any() } else { view! {
                    <div class="panes">
                        <PaneView pane=ctx.panes[0]/>
                        {move || ctx.split.get().then(|| view! {
                            <Grip size=ctx.pane_w key=PANE_W_KEY.into() min=240.0 room=RIGHT_PANE_MIN class="grip pane-grip"/>
                            <PaneView pane=ctx.panes[1]/>
                        })}
                    </div>
                }.into_any() }}
                <TransferBar/>
            </div>
        </div>
    }
}
