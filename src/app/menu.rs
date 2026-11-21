//! The context menu: what it offers for a file, a folder or the background, and doing it.

use super::*;

/// Where a menu of `rows` rows opens for a click at `ev`, kept whole on screen.
fn menu_at(ev: &web_sys::MouseEvent, w: f64, rows: usize) -> (f64, f64) {
    let h = rows as f64 * 28.0 + 12.0;
    let size = |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
    let (vw, vh) = (size(window().inner_width()), size(window().inner_height()));
    let x = (ev.client_x() as f64).min(vw - w - 4.0).max(4.0);
    let y = (ev.client_y() as f64).min(vh - h - 4.0).max(4.0);
    (x, y)
}

impl Ctx {
    pub(super) fn show_menu(self, p: Pane, ev: &web_sys::MouseEvent, entry: Option<Entry>) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        if let Some(e) = &entry {
            if !p.picked().iter().any(|x| x.path == e.path) {
                p.select_only(Some(e.path.clone()));
            }
        }
        let (x, y) = menu_at(ev, 240.0, self.menu_items(p, entry.as_ref()).len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry, columns: false, folders: vec![], place: None, sections: false, tree: false }));
    }

    /// The list's columns to show or hide, from a right-click on its header.
    pub(super) fn show_columns_menu(self, p: Pane, ev: &web_sys::MouseEvent) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        let (x, y) = menu_at(ev, 220.0, self.column_items(p).len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: true, folders: vec![], place: None, sections: false, tree: false }));
    }

    /// A folder in the sidebar: open it here or in a new tab, or (un)favourite it.
    pub(super) fn show_place_menu(self, ev: &web_sys::MouseEvent, src: Src, path: String) {
        ev.prevent_default();
        ev.stop_propagation();
        let p = self.act();
        let (x, y) = menu_at(ev, 220.0, self.place_items(src, &path).len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: false, folders: vec![], place: Some((src, path)), sections: false, tree: false }));
    }

    pub(super) fn place_items(self, src: Src, path: &str) -> Vec<MenuItem> {
        let fav = if self.is_fav(src, path) { "Remove from Favourites" } else { "Add to Favourites" };
        vec![item("Open", "", Act::Open), item("Open in new tab", "", Act::NewTab), MenuItem::Sep, item(fav, "", Act::Favourite)]
    }

    /// What a sidebar menu's item does with its folder.
    fn run_place(self, act: Act, src: Src, path: String) {
        match act {
            Act::Open => self.open_place(src, path),
            Act::NewTab => self.new_tab(self.act(), src, path),
            Act::Favourite => self.toggle_fav(src, path),
            _ => {}
        }
    }

    /// The sidebar's sections to show or hide, from a right-click on the sidebar.
    pub(super) fn show_sections_menu(self, ev: &web_sys::MouseEvent) {
        ev.prevent_default();
        ev.stop_propagation();
        let p = self.act();
        let (x, y) = menu_at(ev, 220.0, self.section_items().len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: false, folders: vec![], place: None, sections: true, tree: false }));
    }

    /// ✓ beside each of the sidebar's sections that's shown.
    pub(super) fn section_items(self) -> Vec<MenuItem> {
        let hidden = self.hidden_sections.get_untracked();
        let mut v: Vec<MenuItem> = SECTIONS.iter().enumerate().map(|(i, &(key, title))| {
            let title = if key == "mac" { crate::os::computer() } else { title };
            item(format!("{} {title}", if hidden.iter().any(|h| h == key) { "\u{2003}" } else { "✓" }), "", Act::Section(i))
        }).collect();
        v.extend([MenuItem::Sep, MenuItem::Do { label: "Show all sections".into(), key: "", act: Act::AllSections, danger: false, enabled: !hidden.is_empty() }]);
        v
    }

    /// Shows the section if it's hidden, else hides it.
    fn toggle_section(self, i: usize) {
        let Some(&(key, _)) = SECTIONS.get(i) else { return };
        let mut hidden = self.hidden_sections.get_untracked();
        match hidden.iter().position(|h| h == key) {
            Some(at) => {
                hidden.remove(at);
            }
            None => hidden.push(key.to_string()),
        }
        self.set_hidden_sections(hidden);
    }

    /// The path's folders folded into "…", under it.
    pub(super) fn show_folders_menu(self, p: Pane, ev: &web_sys::MouseEvent, folders: Vec<String>) {
        ev.stop_propagation();
        self.active.set(p.id);
        let (mut x, mut y) = (ev.client_x() as f64, ev.client_y() as f64);
        if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
            let r = el.get_bounding_client_rect();
            (x, y) = (r.left(), r.bottom() + 2.0);
        }
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: false, folders, place: None, sections: false, tree: false }));
    }

    /// ✓ beside each of the list's columns that's shown; Name is always shown.
    pub(super) fn column_items(self, p: Pane) -> Vec<MenuItem> {
        let shown = p.list_cols.get_untracked();
        let mut v = vec![
            item(format!("{} Show as a tree", if p.list_tree.get_untracked() { "✓" } else { "\u{2003}" }), "", Act::ListTree),
            MenuItem::Sep,
            MenuItem::Do { label: "✓ Name".into(), key: "", act: Act::Column(usize::MAX), danger: false, enabled: false },
        ];
        v.extend(LIST_ORDER.iter().map(|&c| {
            item(format!("{} {}", if shown.contains(&c) { "✓" } else { "\u{2003}" }, LIST_COLS[c].label()), "", Act::Column(c))
        }));
        v.extend([MenuItem::Sep, item("Default columns", "", Act::ResetColumns)]);
        v
    }

    /// "Copy to the Mac", "Move 3 items to <folder>" (when both panes are on the same side).
    pub(super) fn to_label(self, p: Pane, verb: &str, n: usize) -> String {
        let o = self.other(p);
        let (from, to) = (p.src.get_untracked(), o.src.get_untracked());
        let what = if n > 1 { format!(" {n} items") } else { String::new() };
        if from != to {
            format!("{verb}{what} to {}", self.place(to).trim_start_matches("the "))
        } else {
            format!("{verb}{what} to “{}”", base(&o.cwd.get_untracked()))
        }
    }

    /// Keeps the picked items for Paste, which copies them (moves them, when `moving`). Items on
    /// the computer go on the system clipboard too, for Finder.
    pub(super) fn copy_items(self, p: Pane, moving: bool) {
        let items: Vec<String> = p.picked().into_iter().map(|e| e.path).collect();
        if items.is_empty() {
            return;
        }
        if moving && !self.renamable(p) {
            return p.error("These can't be moved from here.");
        }
        let what = match items.as_slice() {
            [one] => format!("“{}”", base(one)),
            _ => format!("{} items", items.len()),
        };
        let src = p.src.get_untracked();
        // Finder gets the files; for a phone's, the clipboard gets their names instead, so what
        // Finder copied before isn't pasted in their place.
        let mac: Vec<String> = if src == Src::Mac { items.clone() } else { vec![] };
        let names = items.iter().map(|x| base(x)).collect::<Vec<_>>().join("\n");
        self.wrote.set_value(mac.clone());
        self.finder.set(vec![]);
        spawn_local(async move {
            let _ = call::<()>("clip_set", json!({ "paths": mac, "text": names })).await;
        });
        self.clip.set(Some(Clip { side: self.side(src), items, moving }));
        let key = match self.key_label("paste") {
            k if k.is_empty() => String::new(),
            k => format!(" ({k})"),
        };
        let verb = if moving { "Cut" } else { "Copied" };
        let how = if moving { "move it" } else { "paste it" };
        p.notice.set(Notice::Ok(format!("{verb} {what}: paste to {how} into another folder{key}.")));
    }

    /// Looks at the system clipboard for files copied in Finder since the app's last Copy, which
    /// Paste then pastes instead.
    pub(super) fn check_clipboard(self) {
        spawn_local(async move {
            let files = call::<Vec<String>>("clip_files", json!({})).await.unwrap_or_default();
            let theirs = if files == self.wrote.get_value() { vec![] } else { files };
            if self.finder.get_untracked() != theirs {
                self.finder.set(theirs);
            }
        });
    }

    /// Whether Paste has something to paste.
    pub(super) fn can_paste(self) -> bool {
        !self.finder.with_untracked(Vec::is_empty) || self.clip.with_untracked(Option::is_some)
    }

    /// Paste, with what it pastes: "Paste “name”", "Paste 3 items", or "Move …" after Cut.
    pub(super) fn paste_label(self) -> String {
        let finder = self.finder.get_untracked();
        let (items, moving) = match self.clip.get_untracked() {
            _ if !finder.is_empty() => (finder, false),
            Some(c) => (c.items, c.moving),
            None => (vec![], false),
        };
        let verb = if moving { "Move" } else { "Paste" };
        match items.as_slice() {
            [one] => format!("{verb} “{}”", base(one)),
            [] => verb.into(),
            _ => format!("{verb} {} items", items.len()),
        }
    }

    /// Copies (or, after Cut, moves) what was copied into `dir` (None: the pane's folder): files
    /// copied in Finder since the app's last Copy, else what Copy kept.
    pub(super) fn paste(self, p: Pane, dir: Option<String>) {
        spawn_local(async move {
            let files = call::<Vec<String>>("clip_files", json!({})).await.unwrap_or_default();
            let finder = if files == self.wrote.get_value() { vec![] } else { files };
            self.finder.set(finder.clone());
            let clip = match self.clip.get_untracked() {
                _ if !finder.is_empty() => Clip { side: Side::Mac, items: finder, moving: false },
                Some(c) => c,
                None => return p.error("Nothing to paste: pick files or folders and Copy them first."),
            };
            let from = match self.src_of(&clip.side) {
                Ok(s) => s,
                Err(e) => return p.error(e),
            };
            let (to_src, to_dir) = (p.src.get_untracked(), dir.unwrap_or_else(|| p.cwd.get_untracked()));
            let items = clip.items;
            if from == to_src {
                if items.iter().any(|x| *x == to_dir || to_dir.starts_with(&format!("{}/", x.trim_end_matches('/')))) {
                    return p.error("A folder can't be pasted into itself.");
                }
                if items.iter().all(|x| parent(x) == to_dir) {
                    return p.error("That's already in this folder. Open another folder to paste it into.");
                }
            }
            // What was cut is moved once; after that it isn't where it was.
            if clip.moving {
                self.clip.set(None);
            }
            self.transfer(p, from, items, to_src, to_dir, clip.moving);
        });
    }

    /// A row of the Editor view's tree (or, with None, its folder at the top).
    pub(super) fn show_tree_menu(self, p: Pane, ev: &web_sys::MouseEvent, entry: Option<Entry>) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        let (x, y) = menu_at(ev, 240.0, self.tree_items(p, entry.as_ref()).len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry, columns: false, folders: vec![], place: None, sections: false, tree: true }));
    }

    pub(super) fn tree_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
        let src = p.src.get_untracked();
        let mac = src == Src::Mac;
        let shell = self.has_shell(src);
        let reveal = format!("Show in {}", crate::os::file_manager());
        let copies = |v: &mut Vec<MenuItem>, top: bool| {
            v.push(MenuItem::Sep);
            if !top {
                v.push(item("Copy name", "", Act::CopyName));
            }
            v.push(item("Copy path", "", Act::CopyPath));
            if !top {
                v.push(item("Copy relative path", "", Act::CopyRelPath));
            }
        };
        let Some(e) = entry else {
            let cwd = p.cwd.get_untracked();
            let mut v = vec![
                MenuItem::Do { label: "Set root to the folder above".into(), key: "", act: Act::Up, danger: false, enabled: cwd != "/" && !cwd.is_empty() },
                MenuItem::Do { label: "Collapse all folders".into(), key: "", act: Act::CollapseAll, danger: false, enabled: !p.tree_open.with_untracked(HashSet::is_empty) },
                item("Refresh", "", Act::Refresh),
                MenuItem::Sep,
                item("New text file…", "", Act::NewFile),
                item("New folder…", "", Act::NewFolder),
            ];
            if mac || shell {
                v.push(MenuItem::Sep);
            }
            if mac {
                v.push(item(reveal, "", Act::Reveal));
            }
            if shell {
                v.push(item("Open in terminal", "", Act::Terminal));
            }
            copies(&mut v, true);
            return v;
        };
        let mut v = vec![];
        if e.dir {
            let open = p.tree_open.with_untracked(|o| o.contains(&e.path));
            v.push(item("Set root here", "", Act::SetRoot));
            v.push(item(if open { "Collapse" } else { "Expand" }, "", Act::Expand));
            v.push(item("Open in new tab", "", Act::NewTab));
        } else if kind(e) == Kind::Text {
            v.push(item("Open", "", Act::Open));
            if mac {
                v.push(item("Open in default app", "", Act::OpenInApp));
            }
        } else {
            v.push(item("Open in default app", "", Act::OpenInApp));
        }
        // A device's storages, at the top of MTP, can't be renamed.
        let renamable = !(parent(&e.path) == "/" && self.src_info(src).is_some_and(|i| i.transport == "mtp")) && !self.read_only(src);
        v.extend([
            MenuItem::Sep,
            MenuItem::Do { label: "Rename".into(), key: "", act: Act::Rename, danger: false, enabled: renamable },
            item("Refresh", "", Act::Refresh),
        ]);
        if mac || (e.dir && shell) {
            v.push(MenuItem::Sep);
        }
        if mac {
            v.push(item(reveal, "", Act::Reveal));
        }
        if e.dir && shell {
            v.push(item("Open in terminal", "", Act::Terminal));
        }
        copies(&mut v, false);
        if e.dir {
            let fav = if self.is_fav(src, &e.path) { "Remove from Favourites" } else { "Add to Favourites" };
            v.extend([MenuItem::Sep, item(fav, "", Act::Favourite)]);
        }
        v
    }

    /// What a tree menu's item does with its row (None: the tree's folder).
    fn run_tree(self, p: Pane, act: Act, entry: Option<Entry>) {
        let src = p.src.get_untracked();
        let cwd = p.cwd.get_untracked();
        let path = entry.as_ref().map_or_else(|| cwd.clone(), |e| e.path.clone());
        match act {
            Act::SetRoot => self.navigate(p, path),
            Act::Up => self.navigate(p, parent(&cwd)),
            Act::Expand => p.tree_open.update(|o| {
                if !o.remove(&path) {
                    o.insert(path);
                }
            }),
            Act::CollapseAll => p.tree_open.update(HashSet::clear),
            Act::Refresh => {
                // Open folders are listed again too.
                let open = p.tree_open.get_untracked();
                p.tree_open.update(HashSet::clear);
                spawn_local(async move {
                    let _ = call::<()>("refresh", json!({ "src": src.key() })).await;
                    self.reload(p);
                    p.tree_open.set(open);
                });
            }
            Act::Open => match entry {
                Some(e) if kind(&e) == Kind::Text => self.open_doc(p, e.path),
                _ => {}
            },
            Act::OpenInApp => self.open_in_app(p, path),
            Act::NewTab => self.new_tab(p, src, path),
            Act::NewFile => p.notice.set(Notice::NewItem { folder: false }),
            Act::NewFolder => p.notice.set(Notice::NewItem { folder: true }),
            Act::Reveal => spawn_local(async move {
                if let Err(e) = call::<()>("reveal", json!({ "path": path })).await {
                    p.error(e);
                }
            }),
            Act::Terminal => self.cd_in_terminal(p, path),
            Act::CopyName => copy_text(p, base(&path)),
            Act::CopyPath => copy_text(p, if src == Src::Mac { crate::os::native_path(&path) } else { path }),
            Act::CopyRelPath => copy_text(p, relative(&cwd, &path)),
            Act::Favourite => self.toggle_fav(src, path),
            Act::Rename => p.renaming.set(entry.map(|e| e.path)),
            _ => {}
        }
    }

    pub(super) fn menu_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
        let device = p.src.get_untracked() != Src::Mac;
        let split = self.split.get_untracked();
        let Some(e) = entry else {
            let here = self.in_storage(p);
            let mut v = vec![
                MenuItem::Do { label: self.paste_label(), key: "paste", act: Act::Paste, danger: false, enabled: here && self.can_paste() },
                MenuItem::Sep,
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
            v.push(MenuItem::Do {
                label: "Find and replace in names…".into(),
                key: "replace_names",
                act: Act::ReplaceNames,
                danger: false,
                enabled: !p.entries.with_untracked(Vec::is_empty) && self.renamable(p),
            });
            v.extend([MenuItem::Sep, item("New tab", "new_tab", Act::NewTab), item(fav, "", Act::Favourite), item("Refresh", "", Act::Refresh)]);
            v.push(MenuItem::Do {
                label: "Import photos…".into(),
                key: "",
                act: Act::Import,
                danger: false,
                enabled: !p.cwd.get_untracked().is_empty(),
            });
            v.push(MenuItem::Do {
                label: "Find duplicates…".into(),
                key: "",
                act: Act::Dupes,
                danger: false,
                enabled: !p.cwd.get_untracked().is_empty(),
            });
            v.push(MenuItem::Do {
                label: "Compare with the other pane".into(),
                key: "",
                act: Act::Compare,
                danger: false,
                enabled: split && !p.cwd.get_untracked().is_empty(),
            });
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
            if now == ViewMode::List {
                v.push(MenuItem::Sep);
                v.extend(self.column_items(p));
            }
            v.push(MenuItem::Sep);
            v.push(item(if split { "Hide second pane" } else { "Show second pane" }, "split", Act::Split));
            let side = if self.side.get_untracked() { "Hide" } else { "Show" };
            v.push(item(format!("{side} {}", crate::os::sidebar()), "sidebar", Act::Sidebar));
            if let Some((_, text)) = self.last_undo() {
                let short: String = if text.chars().count() > 40 { format!("{}…", text.chars().take(39).collect::<String>()) } else { text };
                v.splice(0..0, [item(format!("Undo: {short}"), "undo", Act::Undo), MenuItem::Sep]);
            }
            return v;
        };
        let picked = p.picked().len();
        if picked > 1 {
            let mut v = vec![
                MenuItem::Do { label: format!("Cut {picked} items"), key: "cut_items", act: Act::CutItems, danger: false, enabled: self.renamable(p) },
                item(format!("Copy {picked} items"), "copy_items", Act::CopyItems),
                MenuItem::Sep,
            ];
            if split {
                let enabled = self.in_storage(self.other(p));
                v.push(MenuItem::Do { label: self.to_label(p, "Copy", picked), key: "copy", act: Act::Copy, danger: false, enabled });
                v.push(MenuItem::Do { label: self.to_label(p, "Move", picked), key: "move", act: Act::Move, danger: false, enabled: enabled && !self.read_only(p.src.get_untracked()) });
                v.push(MenuItem::Sep);
            }
            let renamable = self.renamable(p);
            v.push(MenuItem::Do { label: format!("Rename {picked} items…"), key: "rename", act: Act::Rename, danger: false, enabled: renamable });
            v.push(MenuItem::Sep);
            v.push(MenuItem::Do { label: format!("Delete {picked} items…"), key: "delete", act: Act::Delete, danger: true, enabled: !self.read_only(p.src.get_untracked()) });
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
        v.push(item(open, "open", Act::Open));
        if e.dir {
            v.push(item("Open in new tab", "", Act::NewTab));
        }
        if kind(e) == Kind::Epub {
            v.push(item("Check book", "", Act::Check));
        }
        if !e.dir {
            v.push(item(crate::os::quick_look(), "quick_look", Act::QuickLook));
        }
        // Elsewhere, Space already opens in the default app.
        if !e.dir && kind(e) != Kind::Other && crate::os::is_mac() {
            v.push(item("Open in default app", "", Act::OpenInApp));
        }
        // The column view opens a ZIP's folders as more columns; any file can be tried as one
        // (an EPUB, an Office file).
        if !e.dir && p.view.get_untracked() == ViewMode::Columns && !super::preview::has_ext(e, super::preview::ZIP_EXTS) {
            v.push(item("Open as ZIP in columns", "", Act::ShowAs(PeekAs::Zip)));
        }
        if device && !e.dir {
            v.push(item(format!("Download to {}…", crate::os::computer()), "", Act::Download));
        }
        v.push(MenuItem::Do { label: "Cut".into(), key: "cut_items", act: Act::CutItems, danger: false, enabled: self.renamable(p) });
        v.push(item("Copy", "copy_items", Act::CopyItems));
        if e.dir && self.can_paste() {
            v.push(item(format!("{} into “{}”", self.paste_label(), e.name), "", Act::Paste));
        }
        if split {
            let enabled = self.in_storage(self.other(p));
            v.push(MenuItem::Do { label: self.to_label(p, "Copy", 1), key: "copy", act: Act::Copy, danger: false, enabled });
            v.push(MenuItem::Do { label: self.to_label(p, "Move", 1), key: "move", act: Act::Move, danger: false, enabled: enabled && !self.read_only(p.src.get_untracked()) });
        }
        let renamable = self.renamable(p);
        v.push(MenuItem::Do { label: "Rename".into(), key: "rename", act: Act::Rename, danger: false, enabled: renamable });
        if e.dir {
            let fav = if self.is_fav(p.src.get_untracked(), &e.path) { "Remove from Favourites" } else { "Add to Favourites" };
            v.push(item(fav, "", Act::Favourite));
        }
        v.push(MenuItem::Sep);
        if !e.dir {
            v.push(item("Show as text", "", Act::ShowAs(PeekAs::Text)));
            v.push(item("Show as hex", "", Act::ShowAs(PeekAs::Hex)));
        }
        v.extend([item("Copy name", "", Act::CopyName), item("Copy path", "", Act::CopyPath)]);
        v.extend([MenuItem::Sep, item("Refresh", "", Act::Refresh), MenuItem::Sep]);
        let label = if e.dir { "Delete folder if empty…" } else { "Delete…" };
        v.push(MenuItem::Do { label: label.into(), key: "delete", act: Act::Delete, danger: true, enabled: !self.read_only(p.src.get_untracked()) });
        v
    }

    pub(super) fn run(self, p: Pane, act: Act, entry: Option<Entry>) {
        match (act, entry) {
            (Act::Open, Some(e)) => self.open(p, e),
            (Act::OpenInApp, Some(e)) => self.open_in_app(p, e.path),
            (Act::Download, Some(e)) => self.download(p, e.path),
            (Act::Copy, Some(_)) => self.send_to_other(p, false),
            (Act::Move, Some(_)) => self.send_to_other(p, true),
            (Act::Rename, Some(_)) => self.ask_rename(p),
            (Act::QuickLook, Some(_)) => self.quick_look(p),
            (Act::Favourite, Some(e)) => self.toggle_fav(p.src.get_untracked(), e.path),
            (Act::Favourite, None) => self.toggle_fav(p.src.get_untracked(), p.cwd.get_untracked()),
            (Act::Delete, Some(_)) => self.ask_delete(p),
            (Act::NewFolder, _) => p.notice.set(Notice::NewItem { folder: true }),
            (Act::NewFile, _) => p.notice.set(Notice::NewItem { folder: false }),
            (Act::Upload, _) => self.upload(p),
            (Act::Sync, _) => self.compare(p),
            (Act::Compare, _) => self.compare_marks(p),
            (Act::Import, _) => self.ask_import(p),
            (Act::ReplaceNames, _) => self.replace_names(p),
            (Act::Dupes, _) => {
                let mut folders: Vec<(Src, String)> = vec![(p.src.get_untracked(), p.cwd.get_untracked())];
                let o = self.other(p);
                let theirs = (o.src.get_untracked(), o.cwd.get_untracked());
                if self.split.get_untracked() && !theirs.1.is_empty() && !folders.contains(&theirs) {
                    folders.push(theirs);
                }
                self.panel.set(Some(Panel::Dupes(folders)));
            }
            (Act::Read, Some(e)) => self.panel.set(Some(Panel::Reader(p.src.get_untracked(), e.path, None))),
            (Act::Check, Some(e)) => self.check_book(p, e),
            (Act::Library, _) => self.panel.set(Some(Panel::Library(p.src.get_untracked(), p.cwd.get_untracked()))),
            (Act::NewTab, Some(e)) if e.dir => self.new_tab(p, p.src.get_untracked(), e.path),
            (Act::NewTab, None) => self.new_tab(p, p.src.get_untracked(), p.cwd.get_untracked()),
            (Act::Refresh, _) => self.refresh(p),
            (Act::View(v), _) => self.set_view(p, v),
            (Act::Split, _) => self.set_split(!self.split.get_untracked()),
            (Act::Sidebar, _) => self.set_side(!self.side.get_untracked()),
            (Act::Undo, _) => self.undo_last(),
            (Act::CopyItems, Some(_)) => self.copy_items(p, false),
            (Act::CutItems, Some(_)) => self.copy_items(p, true),
            (Act::Paste, Some(e)) if e.dir => self.paste(p, Some(e.path)),
            (Act::Paste, None) => self.paste(p, None),
            (Act::CopyName, Some(e)) => copy_text(p, e.name),
            (Act::CopyPath, Some(e)) => copy_text(p, if p.src.get_untracked() == Src::Mac { crate::os::native_path(&e.path) } else { e.path }),
            (Act::Column(c), _) if c < LIST_COLS.len() => {
                let mut v = p.list_cols.get_untracked();
                match v.iter().position(|&x| x == c) {
                    Some(i) => {
                        v.remove(i);
                    }
                    // Back where it is by default, among those shown.
                    None => {
                        let rank = |c: usize| LIST_ORDER.iter().position(|&x| x == c);
                        let at = v.iter().position(|&x| rank(x) > rank(c)).unwrap_or(v.len());
                        v.insert(at, c);
                    }
                }
                p.set_list_cols(v);
            }
            (Act::ResetColumns, _) => p.set_list_cols(LIST_ORDER.to_vec()),
            (Act::ListTree, _) => p.set_list_tree(!p.list_tree.get_untracked()),
            (Act::Section(i), _) => self.toggle_section(i),
            (Act::AllSections, _) => self.set_hidden_sections(vec![]),
            (Act::ShowAs(how), Some(e)) => {
                p.peek_as.set(Some((e.path, how)));
                p.preview.set(true);
            }
            _ => {}
        }
    }
}

#[component]
pub(super) fn ContextMenu() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || ctx.menu.get().map(|m| {
        let p = ctx.panes[m.pane];
        let items = if m.tree {
            ctx.tree_items(p, m.entry.as_ref())
        } else if let Some((src, path)) = &m.place {
            ctx.place_items(*src, path)
        } else if !m.folders.is_empty() {
            m.folders.iter().enumerate().map(|(i, f)| item(base(f), "", Act::Go(i))).collect()
        } else if m.columns {
            ctx.column_items(p)
        } else if m.sections {
            ctx.section_items()
        } else {
            ctx.menu_items(p, m.entry.as_ref())
        };
        let folders = StoredValue::new(m.folders.clone());
        let place = StoredValue::new(m.place.clone());
        view! {
            <div class="menu" role="menu" style=format!("left:{}px;top:{}px", m.x, m.y)
                on:contextmenu=|e| { e.prevent_default(); e.stop_propagation(); }>
                {items.into_iter().map(|it| match it {
                    MenuItem::Sep => view! { <div class="sep"></div> }.into_any(),
                    MenuItem::Do { label, key, act, danger, enabled } => {
                        let entry = m.entry.clone();
                        view! {
                            <button role="menuitem" class:danger=danger disabled=!enabled
                                on:click=move |_| {
                                    ctx.menu.set(None);
                                    if let Some((src, path)) = place.get_value() {
                                        ctx.run_place(act, src, path);
                                        return;
                                    }
                                    if m.tree {
                                        ctx.run_tree(p, act, entry.clone());
                                        return;
                                    }
                                    match act {
                                        Act::Go(i) => if let Some(f) = folders.with_value(|f| f.get(i).cloned()) { ctx.navigate(p, f) },
                                        _ => ctx.run(p, act, entry.clone()),
                                    }
                                }>
                                <span>{label}</span><span class="key">{move || ctx.key_label(key)}</span>
                            </button>
                        }.into_any()
                    }
                }).collect_view()}
            </div>
        }
    })
}

/// `path` from the folder `root`: "src/main.rs" under "/a/b" for "/a/b/src/main.rs".
fn relative(root: &str, path: &str) -> String {
    let root = root.trim_end_matches('/');
    match path.strip_prefix(root).and_then(|r| r.strip_prefix('/')) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => base(path),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn relative_paths() {
        assert_eq!(super::relative("/a/b", "/a/b/src/x.rs"), "src/x.rs");
        assert_eq!(super::relative("/", "/etc/hosts"), "etc/hosts");
        // Not under the root: its name.
        assert_eq!(super::relative("/a/b", "/a/bc/x"), "x");
    }
}

/// Puts `text` on the clipboard.
fn copy_text(p: Pane, text: String) {
    spawn_local(async move {
        if let Err(e) = call::<()>("copy_text", json!({ "text": text })).await {
            p.error(e);
        }
    });
}
