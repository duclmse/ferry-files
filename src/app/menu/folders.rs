//! Menus for folders outside the list: the path's folded folders, the column view's, and the
//! Editor view's tree.

use super::*;

impl Ctx {
    /// The path's folders folded into "…", under it.
    pub(in crate::app) fn show_folders_menu(
        self,
        p: Pane,
        ev: &web_sys::MouseEvent,
        folders: Vec<String>,
    ) {
        ev.stop_propagation();
        self.active.set(p.id);
        let (mut x, mut y) = (ev.client_x() as f64, ev.client_y() as f64);
        if let Some(el) = ev
            .current_target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        {
            let r = el.get_bounding_client_rect();
            (x, y) = (r.left(), r.bottom() + 2.0);
        }
        self.menu.set(Some(Menu {
            x,
            y,
            pane: p.id,
            entry: None,
            columns: false,
            folders,
            place: None,
            sections: false,
            tree: false,
        }));
    }

    /// ✓ beside each of the list's columns that's shown; Name is always shown.
    pub(in crate::app) fn column_items(self, p: Pane) -> Vec<MenuItem> {
        let shown = p.list_cols.get_untracked();
        let mut v = vec![
            item(
                format!(
                    "{} Show as a tree",
                    if p.list_tree.get_untracked() {
                        "✓"
                    } else {
                        "\u{2003}"
                    }
                ),
                "",
                Act::ListTree,
            ),
            MenuItem::Sep,
            MenuItem::Do {
                label: "✓ Name".into(),
                key: "",
                act: Act::Column(usize::MAX),
                danger: false,
                enabled: false,
            },
        ];
        v.extend(LIST_ORDER.iter().map(|&c| {
            item(
                format!(
                    "{} {}",
                    if shown.contains(&c) {
                        "✓"
                    } else {
                        "\u{2003}"
                    },
                    LIST_COLS[c].label()
                ),
                "",
                Act::Column(c),
            )
        }));
        v.extend([
            MenuItem::Sep,
            item("Default columns", "", Act::ResetColumns),
        ]);
        v
    }

    /// A row of the Editor view's tree (or, with None, its folder at the top).
    pub(in crate::app) fn show_tree_menu(
        self,
        p: Pane,
        ev: &web_sys::MouseEvent,
        entry: Option<Entry>,
    ) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        let (x, y) = menu_at(ev, 240.0, self.tree_items(p, entry.as_ref()).len());
        self.menu.set(Some(Menu {
            x,
            y,
            pane: p.id,
            entry,
            columns: false,
            folders: vec![],
            place: None,
            sections: false,
            tree: true,
        }));
    }

    pub(in crate::app) fn tree_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
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
                MenuItem::Do {
                    label: "Set root to the folder above".into(),
                    key: "",
                    act: Act::Up,
                    danger: false,
                    enabled: cwd != "/" && !cwd.is_empty(),
                },
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
            if mac && crate::os::has_properties(&cwd) {
                v.push(item(crate::os::properties(), "", Act::Properties));
            }
            return v;
        };
        let mut v = vec![];
        if e.dir {
            let open = p.tree_open.with_untracked(|o| o.contains(&e.path));
            v.push(item("Set root here", "", Act::SetRoot));
            v.push(item(
                if open { "Collapse" } else { "Expand" },
                "",
                Act::Expand,
            ));
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
        let renamable = !(parent(&e.path) == "/"
            && self.src_info(src).is_some_and(|i| i.transport == "mtp"))
            && !self.read_only(src);
        v.extend([
            MenuItem::Sep,
            MenuItem::Do {
                label: "Rename".into(),
                key: "",
                act: Act::Rename,
                danger: false,
                enabled: renamable,
            },
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
        if mac {
            v.push(item(crate::os::properties(), "", Act::Properties));
        }
        if e.dir {
            let fav = if self.is_fav(src, &e.path) {
                "Remove from Favourites"
            } else {
                "Add to Favourites"
            };
            v.extend([MenuItem::Sep, item(fav, "", Act::Favourite)]);
        }
        v
    }

    /// What a tree menu's item does with its row (None: the tree's folder).
    pub(super) fn run_tree(self, p: Pane, act: Act, entry: Option<Entry>) {
        let src = p.src.get_untracked();
        let cwd = p.cwd.get_untracked();
        let path = entry
            .as_ref()
            .map_or_else(|| cwd.clone(), |e| e.path.clone());
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
            Act::Properties => self.show_properties(p, path),
            Act::Terminal => self.cd_in_terminal(p, path),
            Act::CopyName => copy_text(p, base(&path)),
            Act::CopyPath => copy_text(
                p,
                if src == Src::Mac {
                    crate::os::native_path(&path)
                } else {
                    path
                },
            ),
            Act::CopyRelPath => copy_text(p, relative(&cwd, &path)),
            Act::Favourite => self.toggle_fav(src, path),
            Act::Rename => p.renaming.set(entry.map(|e| e.path)),
            _ => {}
        }
    }
}
