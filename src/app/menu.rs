//! The context menu: what it offers for a file, a folder or the background, and doing it.

use super::*;

/// Where a menu of `rows` rows opens for a click at `ev`, kept whole on screen.
pub(super) fn menu_at(ev: &web_sys::MouseEvent, w: f64, rows: usize) -> (f64, f64) {
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

    /// A sidebar folder's menu: ways to open it, what can go in it, where else to see it, its
    /// path, Favourites, and Eject for a disk that can be taken out.
    pub(super) fn place_items(self, src: Src, path: &str) -> Vec<MenuItem> {
        let fav = if self.is_fav(src, path) { "Remove from Favourites" } else { "Add to Favourites" };
        // Nothing goes on an Everything server, nor at the top of an MTP device (its storages).
        let writable = !self.read_only(src) && (path != "/" || self.src_info(src).is_some_and(|i| i.transport != "mtp"));
        let mut v = vec![item("Open", "", Act::Open), item("Open in new tab", "", Act::NewTab)];
        if self.split.get_untracked() {
            v.push(item("Open in the other pane", "", Act::OtherPane));
        }
        v.extend([
            MenuItem::Sep,
            MenuItem::Do { label: self.paste_label(), key: "", act: Act::Paste, danger: false, enabled: writable && self.can_paste() },
            MenuItem::Do { label: "New folder…".into(), key: "", act: Act::NewFolder, danger: false, enabled: writable },
            MenuItem::Do { label: "New text file…".into(), key: "", act: Act::NewFile, danger: false, enabled: writable },
            MenuItem::Sep,
        ]);
        if src == Src::Mac {
            v.push(item(format!("Show in {}", crate::os::file_manager()), "", Act::Reveal));
        }
        if self.has_shell(src) {
            v.push(item("Open in terminal", "", Act::Terminal));
        }
        v.extend([item("Copy path", "", Act::CopyPath), MenuItem::Sep, item(fav, "", Act::Favourite)]);
        // The folder itself: not a device's top, a disk, or a folder the device names (Home, Storage).
        if !self.fixed_place(src, path) {
            let writable = !self.read_only(src);
            v.push(MenuItem::Sep);
            v.push(MenuItem::Do { label: "Rename".into(), key: "", act: Act::Rename, danger: false, enabled: writable });
            if src == Src::Mac {
                let trash = crate::os::trash();
                v.push(MenuItem::Do { label: format!("Move to {trash}…"), key: "", act: Act::DeletePlace(false), danger: true, enabled: writable });
                v.push(MenuItem::Do { label: "Delete permanently…".into(), key: "", act: Act::DeletePlace(true), danger: true, enabled: writable });
            } else {
                v.push(MenuItem::Do { label: "Delete folder if empty…".into(), key: "", act: Act::DeletePlace(false), danger: true, enabled: writable });
            }
        }
        if src == Src::Mac && self.devices.with_untracked(|d| d.as_ref().is_some_and(|d| d.ejectable.iter().any(|e| e == path))) {
            v.extend([MenuItem::Sep, item(format!("Eject {}", base(path)), "", Act::Eject)]);
        }
        v
    }

    /// Whether a sidebar folder is one that stays put: a device's top, a disk, or one of the
    /// folders a device names (Home, Desktop, Storage): these aren't renamed, deleted or dragged.
    pub(super) fn fixed_place(self, src: Src, path: &str) -> bool {
        let top = path == "/" || parent(path) == "/";
        let named = self.src_info(src).is_some_and(|i| i.default_dir == path || i.quick_dirs.iter().any(|(_, d)| d == path));
        let drive = self.devices.with_untracked(|d| d.as_ref().is_some_and(|d| d.drives.iter().any(|(_, p)| p == path)));
        top || named || drive
    }

    /// What a sidebar menu's item does with its folder. New files and the terminal go through the
    /// active pane, which opens the folder first.
    fn run_place(self, act: Act, src: Src, path: String) {
        let p = self.act();
        match act {
            Act::Open => self.open_place(src, path),
            Act::NewTab => self.new_tab(p, src, path),
            Act::OtherPane => {
                let o = self.other(p);
                self.active.set(o.id);
                self.open_place(src, path);
            }
            Act::Paste => self.paste_to(p, src, path),
            Act::NewFolder | Act::NewFile => {
                if p.src.get_untracked() != src || p.cwd.get_untracked() != path {
                    self.open_place(src, path);
                }
                p.notice.set(Notice::NewItem { folder: act == Act::NewFolder });
            }
            Act::Reveal => spawn_local(async move {
                if let Err(e) = call::<()>("reveal", json!({ "path": path })).await {
                    p.error(e);
                }
            }),
            Act::Terminal => {
                if p.src.get_untracked() != src {
                    self.open_place(src, path.clone());
                }
                self.cd_in_terminal(p, path);
            }
            Act::CopyPath => copy_text(p, if src == Src::Mac { crate::os::native_path(&path) } else { path }),
            Act::Favourite => self.toggle_fav(src, path),
            Act::Eject => self.eject(path),
            Act::Rename => self.side_renaming.set(Some((src, path))),
            Act::DeletePlace(forever) => {
                let e = Entry { name: base(&path), path, dir: true, size: 0, mtime: 0, hidden: false };
                p.notice.set(Notice::ConfirmDelete(src, vec![e], forever));
            }
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
        let mut v: Vec<MenuItem> = SECTIONS
            .iter()
            .enumerate()
            .map(|(i, &(key, title))| {
                let title = if key == "mac" { crate::os::computer() } else { title };
                item(format!("{} {title}", if hidden.iter().any(|h| h == key) { "\u{2003}" } else { "✓" }), "", Act::Section(i))
            })
            .collect();
        v.extend([
            MenuItem::Sep,
            MenuItem::Do { label: "Show all sections".into(), key: "", act: Act::AllSections, danger: false, enabled: !hidden.is_empty() },
            MenuItem::Sep,
            item(format!("{} Show the current folder", if self.follow_folder.get_untracked() { "✓" } else { "\u{2003}" }), "", Act::FollowFolder),
        ]);
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
        v.extend(
            LIST_ORDER.iter().map(|&c| item(format!("{} {}", if shown.contains(&c) { "✓" } else { "\u{2003}" }, LIST_COLS[c].label()), "", Act::Column(c))),
        );
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
        let dir = dir.unwrap_or_else(|| p.cwd.get_untracked());
        self.paste_to(p, p.src.get_untracked(), dir);
    }

    /// Paste into folder `to_dir` on `to_src` (a sidebar folder), with the work shown in pane `p`.
    pub(super) fn paste_to(self, p: Pane, to_src: Src, to_dir: String) {
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
                MenuItem::Do {
                    label: "Collapse all folders".into(),
                    key: "",
                    act: Act::CollapseAll,
                    danger: false,
                    enabled: !p.tree_open.with_untracked(HashSet::is_empty),
                },
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
            let fav =
                if self.is_fav(p.src.get_untracked(), &p.cwd.get_untracked()) { "Remove this folder from Favourites" } else { "Add this folder to Favourites" };
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
            v.push(self.script_menu());
            v.extend([MenuItem::Sep, self.sort_menu(p), self.group_menu(p)]);
            v.extend([MenuItem::Sep, item("New tab", "new_tab", Act::NewTab), item(fav, "", Act::Favourite), item("Refresh", "", Act::Refresh)]);
            v.push(MenuItem::Do { label: "Import photos…".into(), key: "", act: Act::Import, danger: false, enabled: !p.cwd.get_untracked().is_empty() });
            v.push(MenuItem::Do { label: "Find duplicates…".into(), key: "", act: Act::Dupes, danger: false, enabled: !p.cwd.get_untracked().is_empty() });
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
            let now = p.view.get_untracked();
            if now == ViewMode::List {
                v.push(MenuItem::Sep);
                v.extend(self.column_items(p));
            }
            v.push(MenuItem::Sep);
            v.push(item(if split { "Hide second pane" } else { "Show second pane" }, "split", Act::Split));
            let side = if self.side.get_untracked() { "Hide" } else { "Show" };
            v.push(item(format!("{side} {}", crate::os::sidebar()), "sidebar", Act::Sidebar));
            v.push(item(if self.show_hidden.get_untracked() { "Hide hidden files" } else { "Show hidden files" }, "hidden", Act::Hidden));
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
                v.push(MenuItem::Do {
                    label: self.to_label(p, "Move", picked),
                    key: "move",
                    act: Act::Move,
                    danger: false,
                    enabled: enabled && !self.read_only(p.src.get_untracked()),
                });
                v.push(MenuItem::Sep);
            }
            let renamable = self.renamable(p);
            v.push(MenuItem::Do { label: format!("Rename {picked} items…"), key: "rename", act: Act::Rename, danger: false, enabled: renamable });
            v.push(self.script_menu());
            v.push(MenuItem::Sep);
            v.extend(self.delete_items(p, &format!("{picked} items"), false));
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
            v.push(MenuItem::Do {
                label: self.to_label(p, "Move", 1),
                key: "move",
                act: Act::Move,
                danger: false,
                enabled: enabled && !self.read_only(p.src.get_untracked()),
            });
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
        v.extend([MenuItem::Sep, self.script_menu(), item("Refresh", "", Act::Refresh), MenuItem::Sep]);
        v.extend(self.delete_items(p, "", e.dir));
        v
    }

    /// Sort by ▸: what by (✓ the one now), then which way.
    fn sort_menu(self, p: Pane) -> MenuItem {
        let Sort(now, asc) = p.sort.get_untracked();
        let tick = |on: bool| if on { "✓" } else { "\u{2003}" };
        let mut items: Vec<MenuItem> = SortKey::ALL
            .into_iter()
            .map(|k| {
                item(
                    format!("{} {}", tick(k == now), k.label()),
                    "",
                    Act::SortBy(Sort(k, if k == now { asc } else { matches!(k, SortKey::Name | SortKey::Kind) })),
                )
            })
            .collect();
        items.extend([
            MenuItem::Sep,
            item(format!("{} Ascending", tick(asc)), "", Act::SortBy(Sort(now, true))),
            item(format!("{} Descending", tick(!asc)), "", Act::SortBy(Sort(now, false))),
        ]);
        MenuItem::Sub { label: "Sort by".into(), items }
    }

    /// Group by ▸: nothing, or what the list's headings go by (✓ the one now). Only the list
    /// shows the headings; the other views keep the groups together.
    fn group_menu(self, p: Pane) -> MenuItem {
        let now = p.group.get_untracked();
        let items = Group::ALL.into_iter().map(|g| item(format!("{} {}", if g == now { "✓" } else { "\u{2003}" }, g.label()), "", Act::GroupBy(g))).collect();
        MenuItem::Sub { label: "Group by".into(), items }
    }

    /// Run script ▸: the scripts (Settings > Scripts), then a way to add some.
    fn script_menu(self) -> MenuItem {
        // A tab before its shortcut puts the shortcut where the menu shows them.
        let mut items: Vec<MenuItem> = self.scripts.with_untracked(|v| {
            v.iter()
                .enumerate()
                .map(|(i, s)| {
                    let key = untrack(|| self.script_key_label(&s.path));
                    item(if key.is_empty() { s.name.clone() } else { format!("{}\t{key}", s.name) }, "", Act::Script(i))
                })
                .collect()
        });
        if items.is_empty() {
            items.push(MenuItem::Do { label: "No scripts yet".into(), key: "", act: Act::ScriptSettings, danger: false, enabled: false });
        }
        items.extend([MenuItem::Sep, item("Scripts…", "", Act::ScriptSettings)]);
        // Found again for the next time, in case the scripts folder changed.
        self.load_scripts();
        MenuItem::Sub { label: "Run script".into(), items }
    }

    /// Delete, and on the computer Delete permanently. `what` is "3 items", or "" for one item;
    /// `dir`: that one item is a folder.
    fn delete_items(self, p: Pane, what: &str, dir: bool) -> Vec<MenuItem> {
        let enabled = !self.read_only(p.src.get_untracked());
        let mut v = vec![];
        if p.src.get_untracked() == Src::Mac {
            let trash = crate::os::trash();
            let label = if what.is_empty() { format!("Move to {trash}") } else { format!("Move {what} to {trash}") };
            v.push(MenuItem::Do { label, key: "delete", act: Act::Delete, danger: true, enabled });
            let label = if what.is_empty() { "Delete permanently…".to_string() } else { format!("Delete {what} permanently…") };
            v.push(MenuItem::Do { label, key: "delete_forever", act: Act::DeleteForever, danger: true, enabled });
        } else {
            let label = match (what.is_empty(), dir) {
                (true, true) => "Delete folder if empty…".to_string(),
                (true, false) => "Delete…".to_string(),
                _ => format!("Delete {what}…"),
            };
            v.push(MenuItem::Do { label, key: "delete", act: Act::Delete, danger: true, enabled });
        }
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
            (Act::Delete, Some(_)) => self.ask_delete(p, false),
            (Act::DeleteForever, Some(_)) => self.ask_delete(p, true),
            (Act::NewFolder, _) => p.notice.set(Notice::NewItem { folder: true }),
            (Act::NewFile, _) => p.notice.set(Notice::NewItem { folder: false }),
            (Act::Upload, _) => self.upload(p),
            (Act::Sync, _) => self.compare(p),
            (Act::Compare, _) => self.compare_marks(p),
            (Act::Import, _) => self.ask_import(p),
            (Act::ReplaceNames, _) => self.replace_names(p),
            (Act::SortBy(s), _) => {
                p.sort_to(s);
                // Only part of the folder is here: the server sorts it all and sends the first page again.
                if p.more.get_untracked() {
                    self.reload(p);
                }
            }
            (Act::GroupBy(g), _) => p.set_group(g),
            (Act::Script(i), e) => {
                // The items picked (the one right-clicked among them), or the folder.
                let items: Vec<String> = match e {
                    Some(_) => p.picked().into_iter().map(|x| x.path).collect(),
                    None => vec![],
                };
                if let Some(s) = self.scripts.with_untracked(|v| v.get(i).cloned()) {
                    self.run_script(p, s, items);
                }
            }
            (Act::ScriptSettings, _) => {
                self.settings_tab.set("scripts");
                self.settings.set(true);
            }
            (Act::Dupes, _) => {
                let mut folders: Vec<(Src, String)> = vec![(p.src.get_untracked(), p.cwd.get_untracked())];
                let o = self.other(p);
                let theirs = (o.src.get_untracked(), o.cwd.get_untracked());
                if self.split.get_untracked() && !theirs.1.is_empty() && !folders.contains(&theirs) {
                    folders.push(theirs);
                }
                self.panel.set(Some(Panel::Dupes(folders)));
            }
            (Act::Read, Some(e)) => self.read_book(p.src.get_untracked(), e.path),
            (Act::Check, Some(e)) => self.check_book(p, e),
            (Act::Library, _) => self.panel.set(Some(Panel::Library(p.src.get_untracked(), p.cwd.get_untracked()))),
            (Act::NewTab, Some(e)) if e.dir => self.new_tab(p, p.src.get_untracked(), e.path),
            (Act::NewTab, None) => self.new_tab(p, p.src.get_untracked(), p.cwd.get_untracked()),
            (Act::Refresh, _) => self.refresh(p),
            (Act::View(v), _) => self.set_view(p, v),
            (Act::Split, _) => self.set_split(!self.split.get_untracked()),
            (Act::Sidebar, _) => self.set_side(!self.side.get_untracked()),
            (Act::Hidden, _) => self.set_show_hidden(!self.show_hidden.get_untracked()),
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
            (Act::FollowFolder, _) => self.set_follow_folder(!self.follow_folder.get_untracked()),
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
    move || {
        ctx.menu.get().map(|m| {
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
            let entry = StoredValue::new(m.entry.clone());
            let tree = m.tree;
            let run = Callback::new(move |act: Act| {
                ctx.menu.set(None);
                if let Some((src, path)) = place.get_value() {
                    ctx.run_place(act, src, path);
                    return;
                }
                if tree {
                    ctx.run_tree(p, act, entry.get_value());
                    return;
                }
                match act {
                    Act::Go(i) => {
                        if let Some(f) = folders.with_value(|f| f.get(i).cloned()) {
                            ctx.navigate(p, f)
                        }
                    }
                    _ => ctx.run(p, act, entry.get_value()),
                }
            });
            view! {
                <div class="menu" role="menu" style=format!("left:{}px;top:{}px", m.x, m.y)
                    on:contextmenu=|e| { e.prevent_default(); e.stop_propagation(); }>
                    {rows(ctx, items, run)}
                </div>
            }
        })
    }
}

/// A menu's rows. A submenu opens beside its row while the pointer (or the focus) is on it, on
/// the left when there's no room on the right.
fn rows(ctx: Ctx, items: Vec<MenuItem>, run: Callback<Act>) -> AnyView {
    items
        .into_iter()
        .map(|it| match it {
            MenuItem::Sep => view! { <div class="sep"></div> }.into_any(),
            MenuItem::Do { label, key, act, danger, enabled } => {
                // A shortcut written after a tab in the label (a script's) shows like a command's.
                let (label, own) = match label.split_once('\t') {
                    Some((l, k)) => (l.to_string(), Some(k.to_string())),
                    None => (label, None),
                };
                view! {
                    <button role="menuitem" class:danger=danger disabled=!enabled on:click=move |_| run.run(act)>
                        <span>{label}</span><span class="key">{move || own.clone().unwrap_or_else(|| ctx.key_label(key))}</span>
                    </button>
                }
                .into_any()
            }
            MenuItem::Sub { label, items } => {
                let left = RwSignal::new(false);
                let enter = move |ev: web_sys::MouseEvent| {
                    let Some(row) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
                    let r = row.get_bounding_client_rect();
                    let vw = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
                    left.set(r.right() + 220.0 > vw);
                };
                view! {
                    <div class="sub" class:left=left on:mouseenter=enter>
                        <button role="menuitem" aria-haspopup="menu"><span>{label}</span><span class="key">"›"</span></button>
                        <div class="menu submenu" role="menu">{rows(ctx, items, run)}</div>
                    </div>
                }
                .into_any()
            }
        })
        .collect_view()
        .into_any()
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
