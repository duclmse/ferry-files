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
        self.menu.set(Some(Menu { x, y, pane: p.id, entry, columns: false, folders: vec![], place: None }));
    }

    /// The list's columns to show or hide, from a right-click on its header.
    pub(super) fn show_columns_menu(self, p: Pane, ev: &web_sys::MouseEvent) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        let (x, y) = menu_at(ev, 220.0, self.column_items(p).len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: true, folders: vec![], place: None }));
    }

    /// A folder in the sidebar: open it here or in a new tab, or (un)favourite it.
    pub(super) fn show_place_menu(self, ev: &web_sys::MouseEvent, src: Src, path: String) {
        ev.prevent_default();
        ev.stop_propagation();
        let p = self.act();
        let (x, y) = menu_at(ev, 220.0, self.place_items(src, &path).len());
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: false, folders: vec![], place: Some((src, path)) }));
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

    /// The path's folders folded into "…", under it.
    pub(super) fn show_folders_menu(self, p: Pane, ev: &web_sys::MouseEvent, folders: Vec<String>) {
        ev.stop_propagation();
        self.active.set(p.id);
        let (mut x, mut y) = (ev.client_x() as f64, ev.client_y() as f64);
        if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
            let r = el.get_bounding_client_rect();
            (x, y) = (r.left(), r.bottom() + 2.0);
        }
        self.menu.set(Some(Menu { x, y, pane: p.id, entry: None, columns: false, folders, place: None }));
    }

    /// ✓ beside each of the list's columns that's shown; Name is always shown.
    pub(super) fn column_items(self, p: Pane) -> Vec<MenuItem> {
        let shown = p.list_cols.get_untracked();
        let mut v = vec![MenuItem::Do { label: "✓ Name".into(), key: "", act: Act::Column(usize::MAX), danger: false, enabled: false }];
        v.extend(LIST_COLS.iter().enumerate().map(|(c, k)| {
            item(format!("{} {}", if shown.contains(&c) { "✓" } else { "\u{2003}" }, k.label()), "", Act::Column(c))
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

    pub(super) fn menu_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
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
            v.push(item("Books on all devices…", "", Act::AllBooks));
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
            let mut v = vec![];
            if split {
                let enabled = self.in_storage(self.other(p));
                v.push(MenuItem::Do { label: self.to_label(p, "Copy", picked), key: "copy", act: Act::Copy, danger: false, enabled });
                v.push(MenuItem::Do { label: self.to_label(p, "Move", picked), key: "move", act: Act::Move, danger: false, enabled });
                v.push(MenuItem::Sep);
            }
            let renamable = self.renamable(p);
            v.push(MenuItem::Do { label: format!("Rename {picked} items…"), key: "rename", act: Act::Rename, danger: false, enabled: renamable });
            v.push(MenuItem::Sep);
            v.push(MenuItem::Do { label: format!("Delete {picked} items…"), key: "delete", act: Act::Delete, danger: true, enabled: true });
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
        if device && !e.dir {
            v.push(item(&format!("Download to {}…", crate::os::computer()), "", Act::Download));
        }
        if split {
            let enabled = self.in_storage(self.other(p));
            v.push(MenuItem::Do { label: self.to_label(p, "Copy", 1), key: "copy", act: Act::Copy, danger: false, enabled });
            v.push(MenuItem::Do { label: self.to_label(p, "Move", 1), key: "move", act: Act::Move, danger: false, enabled });
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
        v.push(MenuItem::Do { label: label.into(), key: "delete", act: Act::Delete, danger: true, enabled: true });
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
            (Act::AllBooks, _) => self.panel.set(Some(Panel::AllBooks)),
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
                        let at = v.iter().position(|&x| x > c).unwrap_or(v.len());
                        v.insert(at, c);
                    }
                }
                p.set_list_cols(v);
            }
            (Act::ResetColumns, _) => p.set_list_cols((0..LIST_COLS.len()).collect()),
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
        let items = if let Some((src, path)) = &m.place {
            ctx.place_items(*src, path)
        } else if !m.folders.is_empty() {
            m.folders.iter().enumerate().map(|(i, f)| item(base(f), "", Act::Go(i))).collect()
        } else if m.columns {
            ctx.column_items(p)
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

/// Puts `text` on the clipboard.
fn copy_text(p: Pane, text: String) {
    spawn_local(async move {
        if let Err(e) = call::<()>("copy_text", json!({ "text": text })).await {
            p.error(e);
        }
    });
}
