//! The context menu: what it offers for a file, a folder or the background, and doing it.

use super::*;

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
        v.push(MenuItem::Do { label: "Rename".into(), key: "F2", act: Act::Rename, danger: false, enabled: renamable });
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
        v.push(MenuItem::Do { label: label.into(), key: "⌘⌫", act: Act::Delete, danger: true, enabled: true });
        v
    }

    pub(super) fn run(self, p: Pane, act: Act, entry: Option<Entry>) {
        match (act, entry) {
            (Act::Open, Some(e)) => self.open(p, e),
            (Act::OpenInApp, Some(e)) => self.open_in_app(p, e.path),
            (Act::Download, Some(e)) => self.download(p, e.path),
            (Act::Copy, Some(_)) => self.send_to_other(p, false),
            (Act::Move, Some(_)) => self.send_to_other(p, true),
            (Act::Rename, Some(e)) => p.renaming.set(Some(e.path)),
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
            (Act::CopyName, Some(e)) => copy_text(p, e.name),
            (Act::CopyPath, Some(e)) => copy_text(p, e.path),
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

/// Puts `text` on the clipboard.
fn copy_text(p: Pane, text: String) {
    spawn_local(async move {
        if let Err(e) = call::<()>("copy_text", json!({ "text": text })).await {
            p.error(e);
        }
    });
}
