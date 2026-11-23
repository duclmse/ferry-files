//! Moving around: listing folders, going to places and favourites, finding, views.

use super::*;

/// How many items a read-only server's folder (which can have hundreds of thousands) is listed
/// at a time; more are loaded at the bottom of the list.
pub(super) const PAGE: usize = 1000;

/// Part of a folder, and how many items it has in all.
#[derive(serde::Deserialize)]
struct Page {
    entries: Vec<Entry>,
    total: u64,
}

impl Ctx {
    /// Lists `path` on `src` (`more` are added to the arguments): all of it, or on a read-only
    /// server the first page in `sort` order, sorted by the server. With the folder's total
    /// when it's listed a page at a time.
    pub(super) async fn list_first(self, src: Src, path: &str, sort: Sort, more: serde_json::Value) -> Result<(Vec<Entry>, Option<u64>), String> {
        let mut args = json!({ "src": src.key(), "path": path });
        if let (Some(a), serde_json::Value::Object(m)) = (args.as_object_mut(), more) {
            a.extend(m);
        }
        if !self.read_only(src) {
            return call::<Vec<Entry>>("list", args).await.map(|es| (es, None));
        }
        args["offset"] = json!(0);
        args["count"] = json!(PAGE);
        args["sort"] = json!(sort.0.key());
        args["asc"] = json!(sort.1);
        call::<Page>("list_page", args).await.map(|pg| (pg.entries, Some(pg.total)))
    }

    /// Loads the next page of pane `p`'s folder, when it was listed a page at a time and has more.
    pub(super) fn load_more(self, p: Pane) {
        if !p.more.get_untracked() || p.loading_more.get_untracked() || p.loading.get_untracked() {
            return;
        }
        let (src, path, seq, offset) = (p.src.get_untracked(), p.cwd.get_untracked(), p.load_seq.get_value(), p.got.get_value());
        let Sort(key, asc) = p.sort.get_untracked();
        p.loading_more.set(true);
        spawn_local(async move {
            let args = json!({ "src": src.key(), "path": path, "pane": p.id, "offset": offset, "count": PAGE, "sort": key.key(), "asc": asc });
            let listed = call::<Page>("list_page", args).await;
            p.loading_more.set(false);
            // Went somewhere else meanwhile.
            if p.load_seq.get_value() != seq || p.src.get_untracked() != src || p.cwd.get_untracked() != path {
                return;
            }
            match listed {
                Ok(Page { entries: page, total }) => {
                    let got = offset + page.len();
                    p.more.set(!page.is_empty() && (got as u64) < total);
                    p.got.set_value(got);
                    p.total.set(total);
                    let page = self.visible(page);
                    let mut all = p.all.get_untracked();
                    all.extend(page);
                    p.sort.get_untracked().apply(&mut all);
                    p.cols.update(|c| {
                        c.insert(path.clone(), all.clone());
                    });
                    p.all.set(all);
                    p.arrange();
                }
                Err(e) => {
                    p.more.set(false);
                    p.error(e);
                }
            }
        });
    }

    /// Lists `path` on `src` and makes it pane `p`'s folder, with `select` selected.
    /// Keeps the old listing (and side) if it fails.
    pub(super) fn load_select(self, p: Pane, src: Src, path: String, select: Option<String>) {
        self.load_or(p, src, path, select, None);
    }

    /// Like `load_select`, but if `path` can't be listed, lists `fallback` instead.
    pub(super) fn load_or(self, p: Pane, src: Src, path: String, select: Option<String>, fallback: Option<String>) {
        p.loading.set(true);
        let seq = p.load_seq.with_value(|n| n + 1);
        p.load_seq.set_value(seq);
        p.going_to.set_value(Some((src, path.clone())));
        spawn_local(async move {
            let listed = self.list_first(src, &path, p.sort.get_untracked(), json!({ "pane": p.id })).await;
            // A newer listing was asked for meanwhile: it decides what the pane shows (and
            // clears `loading`), so this one changes nothing.
            if p.load_seq.get_value() != seq {
                return;
            }
            p.going_to.set_value(None);
            match listed {
                Ok((entries, total)) => {
                    p.more.set(total.is_some_and(|t| (entries.len() as u64) < t));
                    p.got.set_value(entries.len());
                    p.total.set(total.unwrap_or(entries.len() as u64));
                    let mut entries = self.visible(entries);
                    p.sort.get_untracked().apply(&mut entries);
                    let new_src = p.src.get_untracked() != src;
                    if new_src {
                        p.cols.set(HashMap::new());
                        p.doc_peek.set(None);
                        p.peek_on.set(false);
                        p.src.set(src);
                    }
                    if new_src || !self.space.with_untracked(|m| m.contains_key(&src.key())) {
                        self.load_space(src);
                    }
                    // Only the load the sidebar asked for re-roots; another load that finishes
                    // first leaves the request for it.
                    let reroot = p.reroot.with_value(|r| r.as_deref() == Some(path.as_str()));
                    if reroot {
                        p.reroot.set_value(None);
                    }
                    // An empty root (nothing set yet, e.g. a launch straight into column view)
                    // counts as containing everything, so it has to be set here.
                    // A tab shown on another side brought its own root along.
                    let kept = p.root_src.with_value(|s| *s == Some(src));
                    p.root_src.set_value(None);
                    let root = p.root.get_untracked();
                    if reroot || (new_src && !kept) || root.is_empty() || !within(&path, &root) {
                        p.root.set(path.clone());
                    }
                    p.cols.update(|c| {
                        c.insert(path.clone(), entries.clone());
                    });
                    p.last.update_value(|l| {
                        l.insert(src, path.clone());
                    });
                    store(&self.last_key(p, src), &path);
                    let moved = p.cwd.get_untracked() != path || new_src;
                    if moved {
                        p.marked.set(HashSet::new());
                        p.filter.set(String::new());
                        p.kids.set(HashMap::new());
                    } else {
                        self.reload_kids(p);
                    }
                    // Marks from comparing hold for these two folders only; the same folder again
                    // (after a copy, say) is compared again.
                    let compared = p.cmp.with_untracked(Option::is_some);
                    if moved {
                        self.stop_compare();
                    }
                    p.found.set(None);
                    if moved {
                        self.note_recent(src, &path);
                        self.pane_moved(p);
                    }
                    p.cwd.set(path);
                    p.all.set(entries);
                    p.arrange();
                    p.selected.set(select);
                    if compared && !moved {
                        self.compare_marks(p);
                    }
                    if matches!(p.notice.get_untracked(), Notice::Error(_)) {
                        p.notice.set(Notice::None);
                    }
                }
                Err(_) if fallback.as_ref().is_some_and(|f| *f != path) => {
                    self.load_or(p, src, fallback.unwrap_or_default(), None, None);
                }
                Err(e) => {
                    p.reroot.update_value(|r| {
                        if r.as_deref() == Some(path.as_str()) {
                            *r = None;
                        }
                    });
                    p.error(e)
                }
            }
            p.loading.set(false);
        });
    }

    /// Where the pane's last folder on `src` is kept across launches.
    pub(super) fn last_key(self, p: Pane, src: Src) -> String {
        format!("ferry-files.last.{}.{}", p.id, self.place_key(src))
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

    /// Searches the pane's folder and its subfolders for names containing the search text (or
    /// fitting it, with * and ?, or matching it with regex on), with the pane's search filters.
    pub(super) fn find(self, p: Pane) {
        let query = p.filter.get_untracked().trim().to_string();
        let (k, s, t) = (p.search_kind.get_untracked(), p.search_size.get_untracked(), p.search_time.get_untracked());
        let filtered = k + s + t > 0;
        if query.is_empty() && !filtered {
            return;
        }
        self.stop_compare();
        let day = 86_400;
        let now = (js_sys::Date::now() / 1000.0) as i64;
        let days = SEARCH_TIMES[t].1;
        let filters = json!({
            "kind": SEARCH_KINDS[k].1,
            "min_size": SEARCH_SIZES[s].1,
            "max_size": SEARCH_SIZES[s].2,
            "after": (days > 0).then(|| now - days * day),
            "before": (days < 0).then(|| now + days * day),
        });
        // How the status bar names it: the text, then the filters in use.
        let mut label = vec![];
        if !query.is_empty() {
            label.push(if p.regex.get_untracked() { format!("/{query}/") } else { format!("“{query}”") });
        }
        for (i, name) in [(k, SEARCH_KINDS[k].0), (s, SEARCH_SIZES[s].0), (t, SEARCH_TIMES[t].0)] {
            if i > 0 {
                label.push(name.to_lowercase());
            }
        }
        let label = label.join(", ");
        if !query.is_empty() {
            self.searches.update(|v| {
                v.retain(|q| *q != query);
                v.insert(0, query.clone());
                v.truncate(10);
                store(SEARCHES_KEY, &serde_json::to_string(v).unwrap_or_default());
            });
        }
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        p.loading.set(true);
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Matches {
                entries: Vec<Entry>,
                complete: bool,
            }
            let args = json!({ "src": src.key(), "dir": dir, "query": query, "hidden": self.show_hidden.get_untracked(), "filters": filters, "regex": p.regex.get_untracked() });
            match call::<Matches>("find", args).await {
                Ok(m) if p.cwd.get_untracked() == dir && p.src.get_untracked() == src => {
                    // The search text stays, and filters the results as they're the same names.
                    p.all.set(m.entries);
                    p.more.set(false);
                    p.found.set(Some((label, m.complete)));
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
        for p in self.panes {
            if p.src.get_untracked() == src {
                p.tree_gen.update(|g| *g += 1);
            }
        }
        for p in self.shown() {
            if p.src.get_untracked() == src && p.cwd.get_untracked() == dir {
                self.reload(p);
            }
        }
        self.kids_changed(src, dir);
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
                // This navigation is dropped, so a sidebar place's re-root must not carry over.
                p.reroot.set_value(None);
                self.confirm_discard.set(true);
                return;
            }
            self.leave_editor();
        }
        self.load_select(p, src, path, select);
    }

    /// Opens a sidebar place in the active pane; the column view starts from it.
    pub(super) fn open_place(self, src: Src, path: String) {
        let p = self.act();
        p.reroot.set_value(Some(path.clone()));
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

    /// Shows or hides the sidebar; remembered across launches.
    pub(super) fn set_side(self, on: bool) {
        self.side.set(on);
        store(SIDE_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_split(self, on: bool) {
        self.split.set(on);
        store(SPLIT_KEY, if on { "1" } else { "0" });
        if !on {
            self.stop_compare();
        }
        if on {
            let p = self.panes[1];
            // Its shown tab: a folder brought back from the last session, or the remembered one.
            if p.cwd.get_untracked().is_empty() {
                self.go_to_tab(p);
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

    /// Leaves the editor before the pane shows something else; false (and asks) if it has
    /// unsaved changes.
    pub(super) fn may_leave(self) -> bool {
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return false;
            }
            self.leave_editor();
        }
        true
    }

    /// Brings the shown tab's entry up to date with where the pane is.
    pub(super) fn save_tab(self, p: Pane) {
        // While a listing is on its way, the tab is where it's going, not the folder still shown.
        let (src, cwd, selected) = match p.going_to.get_value() {
            Some((src, path)) => (src, path, None),
            None => (p.src.get_untracked(), p.cwd.get_untracked(), p.selected.get_untracked()),
        };
        let here = Tab { src, cwd, selected, view: p.view.get_untracked(), root: p.root.get_untracked() };
        let i = p.tab.get_untracked();
        p.tabs.update(|ts| {
            if let Some(t) = ts.get_mut(i) {
                *t = here;
            }
        });
    }

    /// Opens folder `path` on `src` in a new tab after the shown one, and shows it.
    pub(super) fn new_tab(self, p: Pane, src: Src, path: String) {
        if path.is_empty() || !self.may_leave() {
            return;
        }
        self.save_tab(p);
        let at = p.tab.get_untracked() + 1;
        let view = p.view.get_untracked();
        p.tabs.update(|ts| ts.insert(at, Tab { src, cwd: path.clone(), selected: None, view, root: path.clone() }));
        p.tab.set(at);
        p.reroot.set_value(Some(path.clone()));
        self.load_select(p, src, path, None);
    }

    /// Shows tab `i`.
    pub(super) fn show_tab(self, p: Pane, i: usize) {
        if i == p.tab.get_untracked() || i >= p.tabs.with_untracked(Vec::len) || !self.may_leave() {
            return;
        }
        self.save_tab(p);
        p.tab.set(i);
        self.go_to_tab(p);
    }

    /// Shows the tab `delta` places along, wrapping around.
    pub(super) fn step_tab(self, p: Pane, delta: isize) {
        let n = p.tabs.with_untracked(Vec::len) as isize;
        if n > 1 {
            self.show_tab(p, (p.tab.get_untracked() as isize + delta).rem_euclid(n) as usize);
        }
    }

    /// Closes tab `i`; the last one stays.
    pub(super) fn close_tab(self, p: Pane, i: usize) {
        let (n, shown) = (p.tabs.with_untracked(Vec::len), p.tab.get_untracked());
        if n < 2 || i >= n {
            return;
        }
        if i == shown && !self.may_leave() {
            return;
        }
        p.tabs.update(|ts| {
            ts.remove(i);
        });
        if i == shown {
            p.tab.set(i.min(n - 2));
            self.go_to_tab(p);
        } else if i < shown {
            p.tab.set(shown - 1);
        }
    }

    /// Moves tab `i` of pane `from` to place `to` of pane `into` (where it's then shown, and the
    /// pane active): along the strip in the same pane, or across to the other. A pane's only tab
    /// closes the pane it leaves, when Settings says so (`close_after_last_tab`); otherwise it's
    /// copied across, as a pane always has one.
    pub(super) fn move_tab(self, from: Pane, i: usize, into: Pane, to: usize) {
        let (n, shown) = (from.tabs.with_untracked(Vec::len), from.tab.get_untracked());
        if i >= n {
            return;
        }
        if from.id == into.id {
            // Along the strip: the place is counted before the tab leaves its own.
            let to = if to > i { to - 1 } else { to }.min(n - 1);
            if to == i {
                return;
            }
            self.save_tab(from);
            from.tabs.update(|ts| {
                let t = ts.remove(i);
                ts.insert(to, t);
            });
            // The tab shown stays shown, wherever it went.
            let now = if shown == i {
                to
            } else if i < shown && to >= shown {
                shown - 1
            } else if i > shown && to <= shown {
                shown + 1
            } else {
                shown
            };
            from.tab.set(now);
            return;
        }
        // Leaving the tab shown: as for closing it, unsaved changes in the editor are asked about.
        if i == shown && n > 1 && !self.may_leave() {
            return;
        }
        if i == shown {
            self.save_tab(from);
        }
        let Some(t) = from.tabs.with_untracked(|ts| ts.get(i).cloned()) else { return };
        if n == 1 && stored_flag(super::workspace::LAST_TAB_CLOSES_KEY, true) {
            self.close_after_last_tab(t, into, to);
            return;
        }
        self.close_tab(from, i);
        self.save_tab(into);
        let to = to.min(into.tabs.with_untracked(Vec::len));
        into.tabs.update(|ts| ts.insert(to, t));
        into.tab.set(to);
        self.active.set(into.id);
        self.go_to_tab(into);
    }

    /// A pane's only tab `t` dragged to place `to` of pane `into`: one pane is left, with
    /// `into`'s tabs and `t` among them, shown at `to`. That's the first pane, so when the second
    /// is the one left, its tabs move over. The second pane keeps `t` for when it's shown again.
    fn close_after_last_tab(self, t: Tab, into: Pane, to: usize) {
        self.save_tab(into);
        let first = self.panes[0];
        let mut tabs = into.tabs.get_untracked();
        let to = to.min(tabs.len());
        tabs.insert(to, t.clone());
        first.tabs.set(tabs);
        first.tab.set(to);
        let second = self.panes[1];
        second.tabs.set(vec![t]);
        second.tab.set(0);
        self.set_split(false);
        self.go_to_tab(first);
    }

    /// Lists the shown tab's folder. A tab whose device is gone opens the Mac instead.
    pub(super) fn go_to_tab(self, p: Pane) {
        let Some(t) = p.tabs.with_untracked(|ts| ts.get(p.tab.get_untracked()).cloned()) else { return };
        p.view.set(t.view);
        p.root.set(t.root);
        p.root_src.set_value(Some(t.src));
        match self.src_info(t.src) {
            Some(info) if !t.cwd.is_empty() => self.load_or(p, t.src, t.cwd, t.selected, Some(info.default_dir)),
            Some(_) => self.open_remembered(p, t.src),
            None => self.open_remembered(p, Src::Mac),
        }
    }

    /// New files and folders can't go in "/" over MTP: it lists the device's storages. Nor
    /// anywhere on an Everything server.
    pub(super) fn in_storage(self, p: Pane) -> bool {
        !self.read_only(p.src.get()) && (p.cwd.get() != "/" || self.src_info(p.src.get()).is_some_and(|i| i.transport != "mtp"))
    }
}
