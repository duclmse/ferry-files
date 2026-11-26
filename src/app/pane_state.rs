//! A pane's state: its tabs, sorting and grouping, and selection.

use super::*;

impl Pane {
    /// Whether any search filter is set.
    pub(super) fn filtering(self) -> bool {
        self.search_kind.get() + self.search_size.get() + self.search_time.get() > 0
    }

    pub(super) fn new(id: usize) -> Self {
        Pane {
            id,
            src: RwSignal::new(Src::Mac),
            cwd: RwSignal::new(String::new()),
            entries: RwSignal::new(vec![]),
            loading: RwSignal::new(false),
            selected: RwSignal::new(None),
            marked: RwSignal::new(HashSet::new()),
            all: RwSignal::new(vec![]),
            filter: RwSignal::new(String::new()),
            regex: RwSignal::new(stored_flag(&format!("{REGEX_KEY}.{id}"), false)),
            bad_filter: RwSignal::new(None),
            sort: RwSignal::new(Sort::stored(id)),
            group: RwSignal::new(Group::stored(id)),
            found: RwSignal::new(None),
            search_kind: RwSignal::new(0),
            search_size: RwSignal::new(0),
            search_time: RwSignal::new(0),
            filters_open: RwSignal::new(false),
            search_options: RwSignal::new(stored_flag(&format!("{OPTIONS_KEY}.{id}"), false)),
            replacing: RwSignal::new(false),
            replace: RwSignal::new(String::new()),
            replace_rows: RwSignal::new(Ok(HashMap::new())),
            cmp: RwSignal::new(None),
            diff_only: RwSignal::new(false),
            notice: RwSignal::new(Notice::None),
            view: RwSignal::new(ViewMode::stored(id)),
            cols: RwSignal::new(HashMap::new()),
            last: StoredValue::new(HashMap::new()),
            root: RwSignal::new(String::new()),
            reroot: StoredValue::new(None),
            root_src: StoredValue::new(None),
            renaming: RwSignal::new(None),
            preview: RwSignal::new(stored_flag(&format!("ferry-files.preview.{id}"), true)),
            preview_w: stored_px(&format!("ferry-files.preview-w.{id}")),
            col_ws: StoredValue::new(HashMap::new()),
            list_w: [0, 1, 2].map(|n| stored_px(&format!("ferry-files.list-w.{id}.{n}"))),
            list_cols: RwSignal::new(stored_cols(id)),
            list_tree: RwSignal::new(stored_flag(&format!("ferry-files.list-tree.{id}"), false)),
            kids: RwSignal::new(HashMap::new()),
            peek_as: RwSignal::new(None),
            zip_pick: RwSignal::new(None),
            zip_list: StoredValue::new(None),
            coming: RwSignal::new(None),
            pics: StoredValue::new(vec![]),
            pending: StoredValue::new_local(None),
            editing_path: RwSignal::new(false),
            load_seq: StoredValue::new(0),
            going_to: StoredValue::new(None),
            more: RwSignal::new(false),
            loading_more: RwSignal::new(false),
            got: StoredValue::new(0),
            total: RwSignal::new(0),
            term: RwSignal::new(false),
            term_id: RwSignal::new(None),
            term_tick: RwSignal::new(0),
            term_starting: StoredValue::new(false),
            tabs: RwSignal::new(vec![Tab {
                src: Src::Mac,
                cwd: String::new(),
                selected: None,
                view: ViewMode::stored(id),
                root: String::new(),
            }]),
            tab: RwSignal::new(0),
            doc: RwSignal::new(None),
            doc_text: RwSignal::new(String::new()),
            doc_ask: RwSignal::new(false),
            doc_status: RwSignal::new((String::new(), "")),
            doc_find: RwSignal::new(false),
            docs: RwSignal::new(vec![]),
            doc_at: RwSignal::new(0),
            tree_open: RwSignal::new(HashSet::new()),
            tree_w: stored_px(&format!("ferry-files.tree-w.{id}")),
            tree_gen: RwSignal::new(0),
            doc_peek: RwSignal::new(None),
            peek_on: RwSignal::new(false),
        }
    }

    /// Where the Editor view keeps its file.
    pub(super) fn slot(self) -> Slot {
        Slot {
            editor: self.doc,
            text: self.doc_text,
            ask: self.doc_ask,
            status: self.doc_status,
            find: self.doc_find,
            pane: Some(self),
        }
    }

    pub(super) fn error(self, msg: impl Into<String>) {
        self.notice.set(Notice::Error(msg.into()));
    }

    pub(super) fn selected_entry(self) -> Option<Entry> {
        let sel = self.selected.get()?;
        self.entries
            .with(|es| es.iter().find(|e| e.path == sel).cloned())
            .or_else(|| {
                self.kids.with(|k| {
                    k.values()
                        .flatten()
                        .flat_map(|v| v.iter())
                        .find(|e| e.path == sel)
                        .cloned()
                })
            })
    }

    /// Sorts and filters `all` into `entries`, dropping marks on items no longer shown.
    pub(super) fn arrange(self) {
        let f = self.filter.get_untracked();
        let mut bad = None;
        // What the box asks for: a pattern, or else text the name contains in any case.
        let mut re = None;
        let mut text = None;
        if !f.is_empty() {
            if self.regex.get_untracked() {
                // A pattern that doesn't work filters nothing out, and says why.
                match regex(&f, true) {
                    Ok(r) => re = Some(r),
                    Err(e) => bad = Some(e),
                }
            } else if f.contains('*') || f.contains('?') {
                // A pattern, as the search takes it: * any run of characters, ? one.
                re = regex(&glob_source(&f), true).ok();
            } else {
                text = Some(f.to_lowercase());
            }
        }
        if self.bad_filter.get_untracked() != bad {
            self.bad_filter.set(bad);
        }
        let diff_only = self.diff_only.get_untracked();
        // Filtered before sorting, and only what's kept is copied.
        let mut es: Vec<Entry> = self.cmp.with_untracked(|cmp| {
            let cmp = cmp.as_ref().filter(|_| diff_only);
            self.all.with_untracked(|all| {
                all.iter()
                    .filter(|e| re.as_ref().is_none_or(|re| re.is_match(&e.name)))
                    .filter(|e| {
                        text.as_deref()
                            .is_none_or(|t| contains_any_case(&e.name, t))
                    })
                    .filter(|e| cmp.is_none_or(|m| m.get(&e.name).is_some_and(|x| x != "same")))
                    .cloned()
                    .collect()
            })
        });
        self.sort.get_untracked().apply(&mut es);
        // Grouped: each group together, in the groups' order, sorted within.
        let group = self.group.get_untracked();
        if group != Group::None {
            let now = (js_sys::Date::now() / 1000.0) as i64;
            es.sort_by_cached_key(|e| group.of(e, now).0);
        }
        if !self.marked.with_untracked(HashSet::is_empty) {
            // Items in folders open in the tree stay marked too.
            let kids = self.kids.get_untracked();
            let shown: HashSet<&str> = es
                .iter()
                .chain(kids.values().flatten().flatten())
                .map(|e| e.path.as_str())
                .collect();
            self.marked
                .update(|m| m.retain(|x| shown.contains(x.as_str())));
        }
        self.entries.set(es);
    }

    /// Sorts by `s` (remembered for the pane).
    pub(super) fn sort_to(self, s: Sort) {
        self.sort.set(s);
        store(
            &Sort::storage_key(self.id),
            &format!("{}:{}", s.0.key(), if s.1 { 1 } else { 0 }),
        );
        self.cols
            .update(|c| c.values_mut().for_each(|es| s.apply(es)));
        self.kids
            .update(|k| k.values_mut().flatten().for_each(|es| s.apply(es)));
        self.arrange();
    }

    /// Groups the list by `g`.
    pub(super) fn set_group(self, g: Group) {
        self.group.set(g);
        store(&Group::storage_key(self.id), g.key());
        self.arrange();
    }

    /// The list's rows: its items, each with its depth in the tree (0 when it isn't one), and when
    /// it's grouped, each group's heading before it (`None`, with the heading and how many are
    /// in the group). Tracked.
    pub(super) fn list_rows(self) -> Vec<(Option<Entry>, usize, String)> {
        if self.tree_on() {
            return self
                .tree_rows()
                .into_iter()
                .map(|(e, d)| (Some(e), d, String::new()))
                .collect();
        }
        let group = self.group.get();
        self.entries.with(|es| {
            if group == Group::None {
                return es
                    .iter()
                    .map(|e| (Some(e.clone()), 0, String::new()))
                    .collect();
            }
            let now = (js_sys::Date::now() / 1000.0) as i64;
            let groups: Vec<(u32, String)> = es.iter().map(|e| group.of(e, now)).collect();
            let mut out = Vec::with_capacity(es.len() + 8);
            for (i, e) in es.iter().enumerate() {
                if i == 0 || groups[i - 1].0 != groups[i].0 {
                    let n = groups[i..]
                        .iter()
                        .take_while(|g| g.0 == groups[i].0)
                        .count();
                    out.push((None, n, groups[i].1.clone()));
                }
                out.push((Some(e.clone()), 0, String::new()));
            }
            out
        })
    }

    pub(super) fn set_sort(self, key: SortKey) {
        let Sort(now, asc) = self.sort.get_untracked();
        // Clicking the same column again reverses it; dates and sizes start biggest first.
        self.sort_to(if now == key {
            Sort(key, !asc)
        } else {
            Sort(key, matches!(key, SortKey::Name | SortKey::Kind))
        });
    }

    /// Whether `path` is one of the items actions act on (tracked, for highlighting).
    pub(super) fn is_picked(self, path: &str) -> bool {
        let marked = self
            .marked
            .with(|m| (!m.is_empty()).then(|| m.contains(path)));
        marked.unwrap_or_else(|| self.selected.get().as_deref() == Some(path))
    }

    /// The items actions act on: the marked ones, or else the selected one.
    pub(super) fn picked(self) -> Vec<Entry> {
        let marked = self.marked.get_untracked();
        let sel = self.selected.get_untracked();
        self.shown_rows()
            .into_iter()
            .filter(|e| {
                if marked.is_empty() {
                    sel.as_deref() == Some(e.path.as_str())
                } else {
                    marked.contains(&e.path)
                }
            })
            .collect()
    }

    /// Selects one item, unmarking the rest.
    pub(super) fn select_only(self, path: Option<String>) {
        self.marked.set(HashSet::new());
        self.selected.set(path);
    }

    /// A click on an item: ⌘ adds or removes it, Shift picks everything from the selected item to it.
    pub(super) fn click(self, ev: &web_sys::MouseEvent, path: &str) {
        if crate::os::cmd(ev.meta_key(), ev.ctrl_key()) {
            let sel = self.selected.get_untracked();
            self.marked.update(|m| {
                if m.is_empty() {
                    m.extend(sel);
                }
                if !m.remove(path) {
                    m.insert(path.to_string());
                }
            });
            self.selected.set(Some(path.to_string()));
        } else if ev.shift_key() && self.selected.get_untracked().is_some() {
            let anchor = self.selected.get_untracked().unwrap_or_default();
            let es = self.shown_rows();
            let at = |p: &str| es.iter().position(|e| e.path == p);
            let range = match (at(&anchor), at(path)) {
                (Some(a), Some(b)) => es[a.min(b)..=a.max(b)]
                    .iter()
                    .map(|e| e.path.clone())
                    .collect(),
                _ => HashSet::from([path.to_string()]),
            };
            self.marked.set(range);
        } else {
            self.select_only(Some(path.to_string()));
        }
    }
}
