//! What the context menu offers for a file, a folder or the background.

use super::*;

impl Ctx {
    pub(in crate::app) fn menu_items(self, p: Pane, entry: Option<&Entry>) -> Vec<MenuItem> {
        let device = p.src.get_untracked() != Src::Mac;
        let split = self.split.get_untracked();
        let Some(e) = entry else {
            let here = self.in_storage(p);
            let mut v = vec![];
            // Paste only when there's something to paste and somewhere to put it.
            if here && self.can_paste() {
                v.push(MenuItem::Do {
                    label: self.paste_label(),
                    key: "paste",
                    act: Act::Paste,
                    danger: false,
                    enabled: true,
                });
                v.push(MenuItem::Sep);
            }
            v.extend([
                MenuItem::Do {
                    label: "New folder…".into(),
                    key: "",
                    act: Act::NewFolder,
                    danger: false,
                    enabled: here,
                },
                MenuItem::Do {
                    label: "New text file…".into(),
                    key: "",
                    act: Act::NewFile,
                    danger: false,
                    enabled: here,
                },
            ]);
            if device {
                v.push(MenuItem::Do {
                    label: "Upload files…".into(),
                    key: "",
                    act: Act::Upload,
                    danger: false,
                    enabled: here,
                });
            }
            let fav = if self.is_fav(p.src.get_untracked(), &p.cwd.get_untracked()) {
                "Remove this folder from Favourites"
            } else {
                "Add this folder to Favourites"
            };
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
            v.extend(self.script_menus());
            v.extend([MenuItem::Sep, self.sort_menu(p), self.group_menu(p)]);
            v.extend([
                MenuItem::Sep,
                item("New tab", "new_tab", Act::NewTab),
                item(fav, "", Act::Favourite),
                item("Refresh", "", Act::Refresh),
            ]);
            let cwd = p.cwd.get_untracked();
            if !device && !cwd.is_empty() && crate::os::has_properties(&cwd) {
                v.push(item(crate::os::properties(), "", Act::Properties));
            }
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
            let now = p.view.get_untracked();
            if now == ViewMode::List {
                v.push(MenuItem::Sep);
                v.extend(self.column_items(p));
            }
            v.push(MenuItem::Sep);
            v.push(item(
                if split {
                    "Hide second pane"
                } else {
                    "Show second pane"
                },
                "split",
                Act::Split,
            ));
            let side = if self.side.get_untracked() {
                "Hide"
            } else {
                "Show"
            };
            v.push(item(
                format!("{side} {}", crate::os::sidebar()),
                "sidebar",
                Act::Sidebar,
            ));
            v.push(item(
                if self.show_hidden.get_untracked() {
                    "Hide hidden files"
                } else {
                    "Show hidden files"
                },
                "hidden",
                Act::Hidden,
            ));
            if let Some((_, text)) = self.last_undo() {
                let short: String = if text.chars().count() > 40 {
                    format!("{}…", text.chars().take(39).collect::<String>())
                } else {
                    text
                };
                v.splice(
                    0..0,
                    [
                        item(format!("Undo: {short}"), "undo", Act::Undo),
                        MenuItem::Sep,
                    ],
                );
            }
            return v;
        };
        let picked = p.picked().len();
        if picked > 1 {
            let mut v = vec![
                MenuItem::Do {
                    label: format!("Cut {picked} items"),
                    key: "cut_items",
                    act: Act::CutItems,
                    danger: false,
                    enabled: self.renamable(p),
                },
                item(format!("Copy {picked} items"), "copy_items", Act::CopyItems),
                MenuItem::Sep,
            ];
            if split {
                let enabled = self.in_storage(self.other(p));
                v.push(MenuItem::Do {
                    label: self.to_label(p, "Copy", picked),
                    key: "copy",
                    act: Act::Copy,
                    danger: false,
                    enabled,
                });
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
            v.push(MenuItem::Do {
                label: format!("Rename {picked} items…"),
                key: "rename",
                act: Act::Rename,
                danger: false,
                enabled: renamable,
            });
            v.extend(self.script_menus());
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
        if !e.dir
            && p.view.get_untracked() == ViewMode::Columns
            && !super::preview::has_ext(e, super::preview::ZIP_EXTS)
        {
            v.push(item("Open as ZIP in columns", "", Act::ShowAs(PeekAs::Zip)));
        }
        if device && !e.dir {
            v.push(item(
                format!("Download to {}…", crate::os::computer()),
                "",
                Act::Download,
            ));
        }
        v.push(MenuItem::Do {
            label: "Cut".into(),
            key: "cut_items",
            act: Act::CutItems,
            danger: false,
            enabled: self.renamable(p),
        });
        v.push(item("Copy", "copy_items", Act::CopyItems));
        if e.dir && self.can_paste() {
            v.push(item(
                format!("{} into “{}”", self.paste_label(), e.name),
                "",
                Act::Paste,
            ));
        }
        if split {
            let enabled = self.in_storage(self.other(p));
            v.push(MenuItem::Do {
                label: self.to_label(p, "Copy", 1),
                key: "copy",
                act: Act::Copy,
                danger: false,
                enabled,
            });
            v.push(MenuItem::Do {
                label: self.to_label(p, "Move", 1),
                key: "move",
                act: Act::Move,
                danger: false,
                enabled: enabled && !self.read_only(p.src.get_untracked()),
            });
        }
        let renamable = self.renamable(p);
        v.push(MenuItem::Do {
            label: "Rename".into(),
            key: "rename",
            act: Act::Rename,
            danger: false,
            enabled: renamable,
        });
        if e.dir {
            let fav = if self.is_fav(p.src.get_untracked(), &e.path) {
                "Remove from Favourites"
            } else {
                "Add to Favourites"
            };
            v.push(item(fav, "", Act::Favourite));
        }
        v.push(MenuItem::Sep);
        if !e.dir {
            v.push(item("Show as text", "", Act::ShowAs(PeekAs::Text)));
            v.push(item("Show as hex", "", Act::ShowAs(PeekAs::Hex)));
        }
        v.extend([
            item("Copy name", "", Act::CopyName),
            item("Copy path", "", Act::CopyPath),
        ]);
        if !device {
            v.push(item(crate::os::properties(), "", Act::Properties));
        }
        v.push(MenuItem::Sep);
        v.extend(self.script_menus());
        v.extend([item("Refresh", "", Act::Refresh), MenuItem::Sep]);
        v.extend(self.delete_items(p, "", e.dir));
        v
    }

    /// Sort by ▸: what by (✓ the one now), then which way.
    pub(super) fn sort_menu(self, p: Pane) -> MenuItem {
        let Sort(now, asc) = p.sort.get_untracked();
        let tick = |on: bool| if on { "✓" } else { "\u{2003}" };
        let mut items: Vec<MenuItem> = SortKey::ALL
            .into_iter()
            .map(|k| {
                item(
                    format!("{} {}", tick(k == now), k.label()),
                    "",
                    Act::SortBy(Sort(
                        k,
                        if k == now {
                            asc
                        } else {
                            matches!(k, SortKey::Name | SortKey::Kind)
                        },
                    )),
                )
            })
            .collect();
        items.extend([
            MenuItem::Sep,
            item(
                format!("{} Ascending", tick(asc)),
                "",
                Act::SortBy(Sort(now, true)),
            ),
            item(
                format!("{} Descending", tick(!asc)),
                "",
                Act::SortBy(Sort(now, false)),
            ),
        ]);
        MenuItem::Sub {
            label: "Sort by".into(),
            items,
        }
    }

    /// Group by ▸: nothing, or what the list's headings go by (✓ the one now). Only the list
    /// shows the headings; the other views keep the groups together.
    pub(super) fn group_menu(self, p: Pane) -> MenuItem {
        let now = p.group.get_untracked();
        let items = Group::ALL
            .into_iter()
            .map(|g| {
                item(
                    format!("{} {}", if g == now { "✓" } else { "\u{2003}" }, g.label()),
                    "",
                    Act::GroupBy(g),
                )
            })
            .collect();
        MenuItem::Sub {
            label: "Group by".into(),
            items,
        }
    }

    /// Run script ▸: the scripts (Settings > Scripts), then a way to add some; and Try script ▸,
    /// the same scripts run without changing anything.
    pub(super) fn script_menus(self) -> Vec<MenuItem> {
        // A tab before its shortcut puts the shortcut where the menu shows them.
        let list = |act: fn(usize) -> Act, keys: bool| -> Vec<MenuItem> {
            self.scripts.with_untracked(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let key = if keys {
                            untrack(|| self.script_key_label(&s.path))
                        } else {
                            String::new()
                        };
                        item(
                            if key.is_empty() {
                                s.name.clone()
                            } else {
                                format!("{}\t{key}", s.name)
                            },
                            "",
                            act(i),
                        )
                    })
                    .collect()
            })
        };
        let mut run = list(Act::Script, true);
        let tries = list(Act::TryScript, false);
        if run.is_empty() {
            run.push(MenuItem::Do {
                label: "No scripts yet".into(),
                key: "",
                act: Act::ScriptSettings,
                danger: false,
                enabled: false,
            });
        }
        run.extend([MenuItem::Sep, item("Scripts…", "", Act::ScriptSettings)]);
        // Found again for the next time, in case the scripts folder changed.
        self.load_scripts();
        let mut v = vec![MenuItem::Sub {
            label: "Run script".into(),
            items: run,
        }];
        if !tries.is_empty() {
            v.push(MenuItem::Sub {
                label: "Try script".into(),
                items: tries,
            });
        }
        v
    }

    /// Delete: to the Trash on the computer (Delete permanently is only a shortcut, ⌥⌘⌫).
    /// `what` is "3 items", or "" for one item; `dir`: that one item is a folder.
    pub(super) fn delete_items(self, p: Pane, what: &str, dir: bool) -> Vec<MenuItem> {
        let enabled = !self.read_only(p.src.get_untracked());
        let mut v = vec![];
        if p.src.get_untracked() == Src::Mac {
            let trash = crate::os::trash();
            let label = if what.is_empty() {
                format!("Move to {trash}")
            } else {
                format!("Move {what} to {trash}")
            };
            v.push(MenuItem::Do {
                label,
                key: "delete",
                act: Act::Delete,
                danger: true,
                enabled,
            });
        } else {
            let label = match (what.is_empty(), dir) {
                (true, true) => "Delete folder if empty…".to_string(),
                (true, false) => "Delete…".to_string(),
                _ => format!("Delete {what}…"),
            };
            v.push(MenuItem::Do {
                label,
                key: "delete",
                act: Act::Delete,
                danger: true,
                enabled,
            });
        }
        v
    }
}
