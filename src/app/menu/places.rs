//! The sidebar's menus: a place's, and the sections'.

use super::*;

impl Ctx {
    /// A folder in the sidebar: open it here or in a new tab, or (un)favourite it.
    pub(in crate::app) fn show_place_menu(self, ev: &web_sys::MouseEvent, src: Src, path: String) {
        ev.prevent_default();
        ev.stop_propagation();
        let p = self.act();
        let (x, y) = menu_at(ev, 220.0, self.place_items(src, &path).len());
        self.menu.set(Some(Menu {
            x,
            y,
            pane: p.id,
            entry: None,
            columns: false,
            folders: vec![],
            place: Some((src, path)),
            sections: false,
            tree: false,
        }));
    }

    /// A sidebar folder's menu: ways to open it, what can go in it, where else to see it, its
    /// path, Favourites, and Eject for a disk that can be taken out.
    pub(in crate::app) fn place_items(self, src: Src, path: &str) -> Vec<MenuItem> {
        let fav = if self.is_fav(src, path) {
            "Remove from Favourites"
        } else {
            "Add to Favourites"
        };
        // Nothing goes on an Everything server, nor at the top of an MTP device (its storages).
        let writable = !self.read_only(src)
            && (path != "/" || self.src_info(src).is_some_and(|i| i.transport != "mtp"));
        let mut v = vec![
            item("Open", "", Act::Open),
            item("Open in new tab", "", Act::NewTab),
        ];
        if self.split.get_untracked() {
            v.push(item("Open in the other pane", "", Act::OtherPane));
        }
        v.push(MenuItem::Sep);
        if writable && self.can_paste() {
            v.push(MenuItem::Do {
                label: self.paste_label(),
                key: "",
                act: Act::Paste,
                danger: false,
                enabled: true,
            });
        }
        v.extend([
            MenuItem::Do {
                label: "New folder…".into(),
                key: "",
                act: Act::NewFolder,
                danger: false,
                enabled: writable,
            },
            MenuItem::Do {
                label: "New text file…".into(),
                key: "",
                act: Act::NewFile,
                danger: false,
                enabled: writable,
            },
            MenuItem::Sep,
        ]);
        if src == Src::Mac {
            v.push(item(
                format!("Show in {}", crate::os::file_manager()),
                "",
                Act::Reveal,
            ));
        }
        if self.has_shell(src) {
            v.push(item("Open in terminal", "", Act::Terminal));
        }
        v.extend([
            item("Copy path", "", Act::CopyPath),
            MenuItem::Sep,
            item(fav, "", Act::Favourite),
        ]);
        // The folder itself: not a device's top, a disk, or a folder the device names (Home, Storage).
        if !self.fixed_place(src, path) {
            let writable = !self.read_only(src);
            v.push(MenuItem::Sep);
            v.push(MenuItem::Do {
                label: "Rename".into(),
                key: "",
                act: Act::Rename,
                danger: false,
                enabled: writable,
            });
            if src == Src::Mac {
                let trash = crate::os::trash();
                v.push(MenuItem::Do {
                    label: format!("Move to {trash}…"),
                    key: "",
                    act: Act::DeletePlace(false),
                    danger: true,
                    enabled: writable,
                });
                v.push(MenuItem::Do {
                    label: "Delete permanently…".into(),
                    key: "",
                    act: Act::DeletePlace(true),
                    danger: true,
                    enabled: writable,
                });
            } else {
                v.push(MenuItem::Do {
                    label: "Delete folder if empty…".into(),
                    key: "",
                    act: Act::DeletePlace(false),
                    danger: true,
                    enabled: writable,
                });
            }
        }
        if src == Src::Mac
            && self.devices.with_untracked(|d| {
                d.as_ref()
                    .is_some_and(|d| d.ejectable.iter().any(|e| e == path))
            })
        {
            v.extend([
                MenuItem::Sep,
                item(format!("Eject {}", base(path)), "", Act::Eject),
            ]);
        }
        v
    }

    /// Whether a sidebar folder is one that stays put: a device's top, a disk, or one of the
    /// folders a device names (Home, Desktop, Storage): these aren't renamed, deleted or dragged.
    pub(in crate::app) fn fixed_place(self, src: Src, path: &str) -> bool {
        let top = path == "/" || parent(path) == "/";
        let named = self
            .src_info(src)
            .is_some_and(|i| i.default_dir == path || i.quick_dirs.iter().any(|(_, d)| d == path));
        let drive = self.devices.with_untracked(|d| {
            d.as_ref()
                .is_some_and(|d| d.drives.iter().any(|(_, p)| p == path))
        });
        top || named || drive
    }

    /// What a sidebar menu's item does with its folder. New files and the terminal go through the
    /// active pane, which opens the folder first.
    pub(super) fn run_place(self, act: Act, src: Src, path: String) {
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
                p.notice.set(Notice::NewItem {
                    folder: act == Act::NewFolder,
                });
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
            Act::CopyPath => copy_text(
                p,
                if src == Src::Mac {
                    crate::os::native_path(&path)
                } else {
                    path
                },
            ),
            Act::Favourite => self.toggle_fav(src, path),
            Act::Eject => self.eject(path),
            Act::Rename => self.side_renaming.set(Some((src, path))),
            Act::DeletePlace(forever) => {
                let e = Entry {
                    name: base(&path),
                    path,
                    dir: true,
                    size: 0,
                    mtime: 0,
                    hidden: false,
                };
                p.notice.set(Notice::ConfirmDelete(src, vec![e], forever));
            }
            _ => {}
        }
    }

    /// The sidebar's sections to show or hide, from a right-click on the sidebar.
    pub(in crate::app) fn show_sections_menu(self, ev: &web_sys::MouseEvent) {
        ev.prevent_default();
        ev.stop_propagation();
        let p = self.act();
        let (x, y) = menu_at(ev, 220.0, self.section_items().len());
        self.menu.set(Some(Menu {
            x,
            y,
            pane: p.id,
            entry: None,
            columns: false,
            folders: vec![],
            place: None,
            sections: true,
            tree: false,
        }));
    }

    /// ✓ beside each of the sidebar's sections that's shown.
    pub(in crate::app) fn section_items(self) -> Vec<MenuItem> {
        let hidden = self.hidden_sections.get_untracked();
        let mut v: Vec<MenuItem> = SECTIONS
            .iter()
            .enumerate()
            .map(|(i, &(key, title))| {
                let title = if key == "mac" {
                    crate::os::computer()
                } else {
                    title
                };
                item(
                    format!(
                        "{} {title}",
                        if hidden.iter().any(|h| h == key) {
                            "\u{2003}"
                        } else {
                            "✓"
                        }
                    ),
                    "",
                    Act::Section(i),
                )
            })
            .collect();
        v.extend([
            MenuItem::Sep,
            MenuItem::Do {
                label: "Show all sections".into(),
                key: "",
                act: Act::AllSections,
                danger: false,
                enabled: !hidden.is_empty(),
            },
            MenuItem::Sep,
            item(
                format!(
                    "{} Show the current folder",
                    if self.follow_folder.get_untracked() {
                        "✓"
                    } else {
                        "\u{2003}"
                    }
                ),
                "",
                Act::FollowFolder,
            ),
        ]);
        v
    }

    /// Shows the section if it's hidden, else hides it.
    pub(super) fn toggle_section(self, i: usize) {
        let Some(&(key, _)) = SECTIONS.get(i) else {
            return;
        };
        let mut hidden = self.hidden_sections.get_untracked();
        match hidden.iter().position(|h| h == key) {
            Some(at) => {
                hidden.remove(at);
            }
            None => hidden.push(key.to_string()),
        }
        self.set_hidden_sections(hidden);
    }
}
