//! Moving around: listing folders, going to places and favourites, finding, views.

use super::*;

impl Ctx {
    /// Lists `path` on `src` and makes it pane `p`'s folder, with `select` selected.
    /// Keeps the old listing (and side) if it fails.
    pub(super) fn load_select(self, p: Pane, src: Src, path: String, select: Option<String>) {
        self.load_or(p, src, path, select, None);
    }

    /// Like `load_select`, but if `path` can't be listed, lists `fallback` instead.
    pub(super) fn load_or(self, p: Pane, src: Src, path: String, select: Option<String>, fallback: Option<String>) {
        p.loading.set(true);
        spawn_local(async move {
            match call::<Vec<Entry>>("list", json!({ "src": src.key(), "path": path, "pane": p.id })).await {
                Ok(entries) => {
                    let mut entries = self.visible(entries);
                    p.sort.get_untracked().apply(&mut entries);
                    let new_src = p.src.get_untracked() != src;
                    if new_src {
                        p.cols.set(HashMap::new());
                        p.src.set(src);
                    }
                    if new_src || !self.space.with_untracked(|m| m.contains_key(&src.key())) {
                        self.load_space(src);
                    }
                    let reroot = std::mem::take(&mut *p.reroot.write_value());
                    if reroot || new_src || !within(&path, &p.root.get_untracked()) {
                        p.root.set(path.clone());
                    }
                    p.cols.update(|c| {
                        c.insert(path.clone(), entries.clone());
                    });
                    p.last.update_value(|l| {
                        l.insert(src, path.clone());
                    });
                    store(&self.last_key(p, src), &path);
                    if p.cwd.get_untracked() != path || new_src {
                        p.marked.set(vec![]);
                        p.filter.set(String::new());
                    }
                    p.found.set(None);
                    p.cwd.set(path);
                    p.all.set(entries);
                    p.arrange();
                    p.selected.set(select);
                    if matches!(p.notice.get_untracked(), Notice::Error(_)) {
                        p.notice.set(Notice::None);
                    }
                }
                Err(_) if fallback.as_ref().is_some_and(|f| *f != path) => {
                    self.load_or(p, src, fallback.unwrap_or_default(), None, None);
                }
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    /// Where the pane's last folder on `src` is kept across launches.
    pub(super) fn last_key(self, p: Pane, src: Src) -> String {
        format!("yomi-files.last.{}.{}", p.id, self.place_key(src))
    }

    /// "mac", or a device's transport and name ("adb:Pixel 7"), which stay the same across connections.
    pub(super) fn place_key(self, src: Src) -> String {
        match src {
            Src::Mac => "mac".into(),
            _ => self.src_info(src).map_or_else(String::new, |i| format!("{}:{}", i.transport, i.label)),
        }
    }

    /// Shows `src` in pane `p`, in the folder the pane last showed there (this session or before),
    /// or its default folder.
    pub(super) fn open_remembered(self, p: Pane, src: Src) {
        let Some(info) = self.src_info(src) else { return };
        let path = p.last.with_value(|l| l.get(&src).cloned()).or_else(|| stored(&self.last_key(p, src)));
        match path {
            Some(path) => self.load_or(p, src, path, None, Some(info.default_dir)),
            None => self.load_select(p, src, info.default_dir, None),
        }
    }

    /// Searches the pane's folder and its subfolders for names containing the filter's text.
    pub(super) fn find(self, p: Pane) {
        let query = p.filter.get_untracked().trim().to_string();
        if query.is_empty() {
            return;
        }
        self.searches.update(|v| {
            v.retain(|q| *q != query);
            v.insert(0, query.clone());
            v.truncate(10);
            store(SEARCHES_KEY, &serde_json::to_string(v).unwrap_or_default());
        });
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        p.loading.set(true);
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Matches {
                entries: Vec<Entry>,
                complete: bool,
            }
            let args = json!({ "src": src.key(), "dir": dir, "query": query, "hidden": self.show_hidden.get_untracked() });
            match call::<Matches>("find", args).await {
                Ok(m) if p.cwd.get_untracked() == dir && p.src.get_untracked() == src => {
                    p.all.set(m.entries);
                    p.filter.set(String::new());
                    p.found.set(Some((query, m.complete)));
                    p.select_only(None);
                    p.arrange();
                }
                Ok(_) => {}
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    pub(super) fn quick_look(self, p: Pane) {
        let picked = p.picked();
        let [e] = picked.as_slice() else { return };
        if e.dir {
            return;
        }
        let e = e.clone();
        let src = p.src.get_untracked();
        spawn_local(async move {
            if let Err(err) = call::<()>("quick_look", json!({ "src": src.key(), "path": e.path })).await {
                p.error(err);
            }
        });
    }

    /// Closes the open window; the reader goes back to the library it came from.
    pub(super) fn close_panel(self) {
        match self.panel.get_untracked() {
            Some(Panel::Reader(src, _, Some(dir))) => self.panel.set(Some(Panel::Library(src, dir))),
            _ => self.panel.set(None),
        }
    }

    pub(super) fn set_favs(self, favs: Vec<Fav>) {
        store(FAVS_KEY, &serde_json::to_string(&favs).unwrap_or_default());
        self.favs.set(favs);
    }

    /// Adds a folder to Favourites (or removes it, if it's there).
    pub(super) fn toggle_fav(self, src: Src, path: String) {
        let place = self.place_key(src);
        let mut favs = self.favs.get_untracked();
        if let Some(i) = favs.iter().position(|f| f.place == place && f.path == path) {
            favs.remove(i);
        } else {
            let label = self.src_info(src).map_or_else(String::new, |i| i.label);
            favs.push(Fav { place, label, path });
        }
        self.set_favs(favs);
    }

    pub(super) fn is_fav(self, src: Src, path: &str) -> bool {
        let place = self.place_key(src);
        self.favs.with_untracked(|f| f.iter().any(|f| f.place == place && f.path == path))
    }

    /// The connected device (or the Mac) a favourite is on.
    pub(super) fn fav_src(self, f: &Fav) -> Option<Src> {
        if f.place == "mac" {
            return Some(Src::Mac);
        }
        self.phones.with(|ps| ps.iter().find(|i| format!("{}:{}", i.transport, i.label) == f.place).map(|i| Src::of(&i.id)))
    }

    /// Lists the pane's folder again, keeping the selection. Other folders' listings are dropped too.
    pub(super) fn reload(self, p: Pane) {
        let cwd = p.cwd.get_untracked();
        if cwd.is_empty() {
            return;
        }
        p.cols.set(HashMap::new());
        self.load_select(p, p.src.get_untracked(), cwd, p.selected.get_untracked());
    }

    /// Lists again every shown pane that's in folder `dir` on `src`, after something changed there.
    pub(super) fn changed(self, src: Src, dir: &str) {
        self.load_space(src);
        for p in self.shown() {
            if p.src.get_untracked() == src && p.cwd.get_untracked() == dir {
                self.reload(p);
            }
        }
    }

    pub(super) fn reload_all(self) {
        for p in self.shown() {
            self.reload(p);
        }
    }

    /// Asks the phone to re-read its folders (MTP caches them), then lists the folder again.
    pub(super) fn refresh(self, p: Pane) {
        let src = p.src.get_untracked();
        spawn_local(async move {
            let _ = call::<()>("refresh", json!({ "src": src.key() })).await;
            self.reload(p);
        });
    }

    pub(super) fn navigate(self, p: Pane, path: String) {
        self.navigate_select(p, p.src.get_untracked(), path, None);
    }

    /// Navigation that would leave the editor asks first when there are unsaved changes.
    pub(super) fn navigate_select(self, p: Pane, src: Src, path: String, select: Option<String>) {
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return;
            }
            self.leave_editor();
        }
        self.load_select(p, src, path, select);
    }

    /// Shows the phone or the Mac in pane `p`, in the folder it last showed there.
    pub(super) fn switch_src(self, p: Pane, src: Src) {
        if p.src.get_untracked() == src {
            return;
        }
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return;
            }
            self.leave_editor();
        }
        self.open_remembered(p, src);
    }

    /// Opens a sidebar place in the active pane; the column view starts from it.
    pub(super) fn open_place(self, src: Src, path: String) {
        let p = self.act();
        p.reroot.set_value(true);
        self.navigate_select(p, src, path, None);
    }

    /// Up one folder, with the folder we were in selected.
    pub(super) fn go_up(self, p: Pane) {
        let cwd = p.cwd.get_untracked();
        if cwd != "/" {
            self.navigate_select(p, p.src.get_untracked(), parent(&cwd), Some(cwd));
        }
    }

    pub(super) fn set_view(self, p: Pane, v: ViewMode) {
        if v == ViewMode::Columns && p.view.get_untracked() != v {
            p.root.set(p.cwd.get_untracked());
        }
        p.view.set(v);
        store(&ViewMode::storage_key(p.id), v.key());
    }

    pub(super) fn set_split(self, on: bool) {
        self.split.set(on);
        store(SPLIT_KEY, if on { "1" } else { "0" });
        if on {
            let p = self.panes[1];
            if p.cwd.get_untracked().is_empty() {
                self.start(p);
            } else {
                self.reload(p);
            }
        } else {
            self.active.set(0);
        }
    }

    /// Shows the pane's starting folder on its side (the Mac when there's no phone).
    pub(super) fn start(self, p: Pane) {
        let mut src = p.src.get_untracked();
        if self.src_info(src).is_none() {
            src = Src::Mac;
            p.src.set(src);
        }
        self.open_remembered(p, src);
    }

    /// New files and folders can't go in "/" over MTP: it lists the device's storages.
    pub(super) fn in_storage(self, p: Pane) -> bool {
        p.cwd.get() != "/" || self.src_info(p.src.get()).is_some_and(|i| i.transport != "mtp")
    }
}
