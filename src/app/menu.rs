//! The context menu: what it offers for a file, a folder or the background, and doing it.

use super::*;

mod clipboard;
mod folders;
mod items;
mod places;

/// Where a menu of `rows` rows opens for a click at `ev`, kept whole on screen.
pub(super) fn menu_at(ev: &web_sys::MouseEvent, w: f64, rows: usize) -> (f64, f64) {
    let h = rows as f64 * 28.0 + 12.0;
    let size =
        |v: Result<wasm_bindgen::JsValue, _>| v.ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
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
        self.menu.set(Some(Menu {
            x,
            y,
            pane: p.id,
            entry,
            columns: false,
            folders: vec![],
            place: None,
            sections: false,
            tree: false,
        }));
    }

    /// The list's columns to show or hide, from a right-click on its header.
    pub(super) fn show_columns_menu(self, p: Pane, ev: &web_sys::MouseEvent) {
        ev.prevent_default();
        ev.stop_propagation();
        self.active.set(p.id);
        let (x, y) = menu_at(ev, 220.0, self.column_items(p).len());
        self.menu.set(Some(Menu {
            x,
            y,
            pane: p.id,
            entry: None,
            columns: true,
            folders: vec![],
            place: None,
            sections: false,
            tree: false,
        }));
    }

    /// The system's Get Info (Properties) window for `path` on the computer; a problem shows in `p`.
    pub(super) fn show_properties(self, p: Pane, path: String) {
        spawn_local(async move {
            if let Err(e) = call::<()>("show_properties", json!({ "path": path })).await {
                p.error(e);
            }
        });
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
            (act @ (Act::Script(i) | Act::TryScript(i)), e) => {
                // The items picked (the one right-clicked among them), or the folder.
                let items: Vec<String> = match e {
                    Some(_) => p.picked().into_iter().map(|x| x.path).collect(),
                    None => vec![],
                };
                if let Some(s) = self.scripts.with_untracked(|v| v.get(i).cloned()) {
                    self.run_script(p, s, items, matches!(act, Act::TryScript(_)));
                }
            }
            (Act::ScriptSettings, _) => {
                self.settings_tab.set("scripts");
                self.settings.set(true);
            }
            (Act::Dupes, _) => {
                let mut folders: Vec<(Src, String)> =
                    vec![(p.src.get_untracked(), p.cwd.get_untracked())];
                let o = self.other(p);
                let theirs = (o.src.get_untracked(), o.cwd.get_untracked());
                if self.split.get_untracked() && !theirs.1.is_empty() && !folders.contains(&theirs)
                {
                    folders.push(theirs);
                }
                self.panel.set(Some(Panel::Dupes(folders)));
            }
            (Act::Read, Some(e)) => self.read_book(p.src.get_untracked(), e.path),
            (Act::Check, Some(e)) => self.check_book(p, e),
            (Act::Library, _) => self.panel.set(Some(Panel::Library(
                p.src.get_untracked(),
                p.cwd.get_untracked(),
            ))),
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
            (Act::CopyPath, Some(e)) => copy_text(
                p,
                if p.src.get_untracked() == Src::Mac {
                    crate::os::native_path(&e.path)
                } else {
                    e.path
                },
            ),
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
            (Act::Properties, e) => {
                self.show_properties(p, e.map_or_else(|| p.cwd.get_untracked(), |e| e.path))
            }
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
                m.folders
                    .iter()
                    .enumerate()
                    .map(|(i, f)| item(base(f), "", Act::Go(i)))
                    .collect()
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
