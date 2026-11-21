//! The list view shown as a tree: folders open in place, their items under them.

use super::*;

impl Pane {
    /// Whether the list shows as a tree now (tracked): in the list view, and not for search results.
    pub(super) fn tree_on(self) -> bool {
        self.list_tree.get() && self.view.get() == ViewMode::List && self.found.with(Option::is_none)
    }

    pub(super) fn set_list_tree(self, on: bool) {
        store(&format!("ferry-files.list-tree.{}", self.id), if on { "1" } else { "0" });
        if !on {
            self.kids.set(HashMap::new());
        }
        self.list_tree.set(on);
    }

    /// The list's rows (tracked) with their depth: each item, and an open folder's items under it.
    pub(super) fn tree_rows(self) -> Vec<(Entry, usize)> {
        fn add(out: &mut Vec<(Entry, usize)>, es: &[Entry], kids: &HashMap<String, Option<Vec<Entry>>>, depth: usize) {
            for e in es {
                out.push((e.clone(), depth));
                if let Some(Some(k)) = e.dir.then(|| kids.get(&e.path)).flatten() {
                    add(out, k, kids, depth + 1);
                }
            }
        }
        self.kids.with(|kids| {
            self.entries.with(|es| {
                let mut out = Vec::with_capacity(es.len());
                add(&mut out, es, kids, 0);
                out
            })
        })
    }

    /// The items in the order they're shown (untracked), open folders' items included.
    pub(super) fn shown_rows(self) -> Vec<Entry> {
        untrack(|| if self.tree_on() { self.tree_rows().into_iter().map(|(e, _)| e).collect() } else { self.entries.get() })
    }

    /// Whether folder `path` is open in the tree (tracked).
    pub(super) fn is_open(self, path: &str) -> bool {
        self.kids.with(|k| k.contains_key(path))
    }
}

impl Ctx {
    /// Opens or closes folder `path` in the list's tree. Closing it closes the folders in it, and
    /// moves the selection out of it.
    pub(super) fn toggle_kids(self, p: Pane, path: String) {
        if !p.kids.with_untracked(|k| k.contains_key(&path)) {
            p.kids.update(|k| {
                k.insert(path.clone(), None);
            });
            return self.load_kids(p, path, false);
        }
        let under = format!("{}/", path.trim_end_matches('/'));
        p.kids.update(|k| k.retain(|x, _| *x != path && !x.starts_with(&under)));
        if p.marked.with_untracked(|m| m.iter().any(|x| x.starts_with(&under))) {
            p.marked.update(|m| m.retain(|x| !x.starts_with(&under)));
        }
        if p.selected.with_untracked(|s| s.as_ref().is_some_and(|s| s.starts_with(&under))) {
            p.selected.set(Some(path));
        }
    }

    /// Lists open folder `path` (quietly, when it's being listed again: it may be gone).
    fn load_kids(self, p: Pane, path: String, quiet: bool) {
        let src = p.src.get_untracked();
        spawn_local(async move {
            let got = call::<Vec<Entry>>("list", json!({ "src": src.key(), "path": path, "current": false })).await;
            // Closed, or the pane went elsewhere, meanwhile.
            if p.src.get_untracked() != src || !p.kids.with_untracked(|k| k.contains_key(&path)) {
                return;
            }
            match got {
                Ok(es) => {
                    let mut es = self.visible(es);
                    p.sort.get_untracked().apply(&mut es);
                    p.kids.update(|k| {
                        k.insert(path, Some(es));
                    });
                }
                Err(e) => {
                    p.kids.update(|k| {
                        k.remove(&path);
                    });
                    if !quiet {
                        p.error(e);
                    }
                }
            }
        });
    }

    /// Lists the open folders again.
    pub(super) fn reload_kids(self, p: Pane) {
        for path in p.kids.with_untracked(|k| k.keys().cloned().collect::<Vec<_>>()) {
            self.load_kids(p, path, true);
        }
    }

    /// Lists folder `dir` again in the panes where it's open in the tree.
    pub(super) fn kids_changed(self, src: Src, dir: &str) {
        for p in self.shown() {
            if p.src.get_untracked() == src && p.kids.with_untracked(|k| k.contains_key(dir)) {
                self.load_kids(p, dir.to_string(), true);
            }
        }
    }

    /// → and ← in the tree: open a folder, or go to its first item; close it, or go to the folder
    /// the item is in.
    pub(super) fn tree_key(self, p: Pane, at: &Entry, right: bool, rows: &[Entry]) {
        let open = p.kids.with_untracked(|k| k.contains_key(&at.path));
        if right && at.dir {
            if !open {
                self.toggle_kids(p, at.path.clone());
            } else if let Some(next) = rows.iter().position(|e| e.path == at.path).and_then(|i| rows.get(i + 1)) {
                if parent(&next.path) == at.path {
                    p.select_only(Some(next.path.clone()));
                }
            }
        } else if !right {
            if at.dir && open {
                self.toggle_kids(p, at.path.clone());
            } else {
                let up = parent(&at.path);
                if p.kids.with_untracked(|k| k.contains_key(&up)) {
                    p.select_only(Some(up));
                }
            }
        }
    }
}
