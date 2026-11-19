//! Workspaces: the panes' tabs, the split and the widths. The current one is kept as it changes
//! and brought back at launch (a setting); named ones are saved and opened from the sidebar.
//! Phones are kept by serial: a tab on a phone that isn't connected waits, and comes back when
//! the phone does.

use super::*;
use std::cell::{Cell, RefCell};

pub(super) const WORKSPACES_KEY: &str = "ferry-files.workspaces";
const SESSION_KEY: &str = "ferry-files.session";
pub(super) const RESTORE_KEY: &str = "ferry-files.restore-tabs";

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct SavedTab {
    side: Side,
    cwd: String,
    view: String,
    /// The column view's first column; missing in workspaces saved before it was kept.
    #[serde(default)]
    root: String,
}

#[derive(Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
struct SavedPane {
    tabs: Vec<SavedTab>,
    /// The tab shown.
    tab: usize,
}

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Workspace {
    pub(super) name: String,
    panes: [SavedPane; 2],
    split: bool,
    active: usize,
    sidebar: bool,
    side_w: f64,
    pane_w: f64,
}

/// A tab on a phone that isn't connected: its pane, where it was among the pane's saved tabs,
/// and whether it was the one shown.
struct Waiting {
    pane: usize,
    at: usize,
    shown: bool,
    tab: SavedTab,
}

/// How `apply` left a pane, kept until the pane is used: where each tab was among the saved ones
/// (`None`: a stand-in on the computer while all of them wait for their phones), the tab shown,
/// and whether the pane has landed on its first folder since.
#[derive(Clone)]
struct Applied {
    order: Vec<Option<usize>>,
    tab: usize,
    landed: bool,
}

thread_local! {
    /// Whether the session may be saved: not before it's been brought back at launch.
    static READY: Cell<bool> = const { Cell::new(false) };
    /// Tabs on phones that aren't connected.
    static WAITING: RefCell<Vec<Waiting>> = const { RefCell::new(vec![]) };
    /// Per pane, how `apply` left it, until it's used.
    static APPLIED: RefCell<[Option<Applied>; 2]> = const { RefCell::new([None, None]) };
}

fn tab_of(src: Src, t: &SavedTab) -> Tab {
    let root = if t.root.is_empty() || !within(&t.cwd, &t.root) { &t.cwd } else { &t.root };
    Tab { src, cwd: t.cwd.clone(), selected: None, view: view_of(&t.view), root: root.clone() }
}

/// A hidden pane given other tabs: it lists its shown tab when it's shown (`set_split`), and
/// until then is where that tab says.
fn forget(p: Pane) {
    p.load_seq.update_value(|n| *n += 1);
    p.going_to.set_value(None);
    p.loading.set(false);
    p.cwd.set(String::new());
}

fn view_of(key: &str) -> ViewMode {
    ViewMode::ALL.into_iter().find(|v| v.key() == key).unwrap_or(ViewMode::List)
}

/// Whether a saved tab's side is the phone `info`: by serial, else by name (ids are handed out
/// afresh each launch).
fn is_on(side: &Side, info: &Info) -> bool {
    match side {
        Side::Phone { serial: Some(s), .. } => info.serial.as_deref() == Some(s.as_str()),
        Side::Phone { serial: None, label, .. } => *label == info.label,
        Side::Mac => false,
    }
}

pub(super) fn saved_workspaces() -> Vec<Workspace> {
    stored(WORKSPACES_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

impl Ctx {
    /// The pane's tabs as `apply` left them, while it hasn't been used since.
    fn as_applied(self, i: usize) -> Option<Applied> {
        let p = self.panes[i];
        APPLIED
            .with_borrow(|a| a[i].clone())
            .filter(|a| a.tab == p.tab.get_untracked() && a.order.len() == p.tabs.with_untracked(Vec::len))
    }

    /// Pane `p` went to another folder: the first time after `apply`, that's it landing on the
    /// shown tab's folder; after that it's in use, and tabs coming back no longer move it.
    pub(super) fn pane_moved(self, p: Pane) {
        APPLIED.with_borrow_mut(|a| {
            let slot = &mut a[p.id];
            match slot {
                Some(x) if !x.landed => x.landed = true,
                _ => *slot = None,
            }
        });
    }

    /// The workspace as it is now, named `name`; tabs waiting for their phone are kept, in their
    /// places while the pane hasn't been used since.
    fn snapshot(self, name: String) -> Workspace {
        let panes = [0, 1].map(|i| {
            let p = self.panes[i];
            let shown = p.tab.get_untracked();
            // A pane not listed yet (hidden since its tabs were set) is where its shown tab says.
            let live = p.going_to.with_value(Option::is_some) || p.cwd.with_untracked(|c| !c.is_empty());
            let applied = self.as_applied(i);
            // Each tab with where it goes and how much it wants to be the one shown: a waiting tab
            // that was shown beats the tab shown in its place.
            let mut all: Vec<(usize, SavedTab, u8)> = vec![];
            p.tabs.with_untracked(|ts| {
                for (at, t) in ts.iter().enumerate() {
                    // The shown tab's entry is brought up to date only when leaving it.
                    // While a listing is on its way, the tab is where it's going.
                    let (src, cwd, view, root) = if at == shown && live {
                        let (src, cwd) = p.going_to.get_value().unwrap_or_else(|| (p.src.get_untracked(), p.cwd.get_untracked()));
                        (src, cwd, p.view.get_untracked(), p.root.get_untracked())
                    } else {
                        (t.src, t.cwd.clone(), t.view, t.root.clone())
                    };
                    // A tab left on a phone that's gone can't be found again.
                    if src != Src::Mac && self.src_info(src).is_none() {
                        continue;
                    }
                    let place = match &applied {
                        Some(a) => match a.order[at] {
                            Some(k) => k,
                            None => continue,
                        },
                        None => at,
                    };
                    all.push((place, SavedTab { side: self.side(src), cwd, view: view.key().into(), root }, u8::from(at == shown)));
                }
            });
            WAITING.with_borrow(|w| {
                for x in w.iter().filter(|x| x.pane == i) {
                    let (place, want) = match applied {
                        Some(_) => (x.at, if x.shown { 2 } else { 0 }),
                        None => (usize::MAX, 0),
                    };
                    all.push((place, x.tab.clone(), want));
                }
            });
            all.sort_by_key(|x| x.0);
            let tab = all.iter().enumerate().max_by_key(|(k, x)| (x.2, std::cmp::Reverse(*k))).map_or(0, |(k, _)| k);
            SavedPane { tabs: all.into_iter().map(|x| x.1).collect(), tab }
        });
        Workspace {
            name,
            panes,
            split: self.split.get_untracked(),
            active: self.active.get_untracked(),
            sidebar: self.side.get_untracked(),
            side_w: self.side_w.get_untracked(),
            pane_w: self.pane_w.get_untracked(),
        }
    }

    /// Shows workspace `w`: its tabs on the computer and on connected phones now, the others
    /// when their phones connect.
    fn apply(self, w: Workspace) {
        WAITING.with_borrow_mut(Vec::clear);
        self.stop_compare();
        self.side_w.set(w.side_w);
        store(SIDE_W_KEY, &w.side_w.to_string());
        self.pane_w.set(w.pane_w);
        store(PANE_W_KEY, &w.pane_w.to_string());
        self.set_side(w.sidebar);
        self.split.set(w.split);
        store(SPLIT_KEY, if w.split { "1" } else { "0" });
        for (i, saved) in w.panes.into_iter().enumerate() {
            let p = self.panes[i];
            let (mut tabs, mut order) = (vec![], vec![]);
            let mut shown = 0;
            for (at, t) in saved.tabs.into_iter().enumerate() {
                let src = match &t.side {
                    Side::Mac => Some(Src::Mac),
                    side => self.phones.with_untracked(|ps| ps.iter().find(|i| is_on(side, i)).map(|i| Src::of(&i.id))),
                };
                match src {
                    Some(src) => {
                        if at == saved.tab {
                            shown = tabs.len();
                        }
                        tabs.push(tab_of(src, &t));
                        order.push(Some(at));
                    }
                    None => WAITING.with_borrow_mut(|wt| wt.push(Waiting { pane: i, at, shown: at == saved.tab, tab: t })),
                }
            }
            if tabs.is_empty() {
                tabs.push(Tab { src: Src::Mac, cwd: String::new(), selected: None, view: p.view.get_untracked(), root: String::new() });
                order.push(None);
            }
            p.tabs.set(tabs);
            p.tab.set(shown);
            if i == 0 || w.split {
                self.go_to_tab(p);
            } else {
                forget(p);
            }
            APPLIED.with_borrow_mut(|a| a[i] = Some(Applied { order, tab: shown, landed: false }));
        }
        self.active.set(if w.split { w.active.min(1) } else { 0 });
    }

    /// At launch: the last session's tabs when the setting is on, else the remembered folders.
    /// From then on, the session is kept as it changes.
    pub(super) fn restore_session(self) {
        let last = stored_flag(RESTORE_KEY, true).then(|| stored(SESSION_KEY).and_then(|s| serde_json::from_str::<Workspace>(&s).ok())).flatten();
        match last {
            Some(w) => self.apply(w),
            None => {
                self.start(self.panes[0]);
                if self.split.get_untracked() {
                    self.start(self.panes[1]);
                }
            }
        }
        READY.set(true);
    }

    /// Keeps the session, whenever tabs, folders, views, the split or widths change.
    pub(super) fn keep_session(self) {
        Effect::new(move |_| {
            for p in self.panes {
                p.tabs.track();
                p.tab.track();
                p.cwd.track();
                p.src.track();
                p.view.track();
                p.root.track();
            }
            self.split.track();
            self.active.track();
            self.side.track();
            self.side_w.track();
            self.pane_w.track();
            if READY.get() {
                store(SESSION_KEY, &serde_json::to_string(&self.snapshot(String::new())).unwrap_or_default());
            }
        });
    }

    /// A phone connected: its waiting tabs come back. In a pane not used since its tabs were
    /// set, they go back in their places, a stand-in gives way, and the one that was shown is
    /// shown; otherwise they go after the pane's others.
    pub(super) fn tabs_back(self, info: &Info) {
        let back: Vec<Waiting> = WAITING.with_borrow_mut(|w| {
            let (on, rest): (Vec<_>, Vec<_>) = w.drain(..).partition(|x| is_on(&x.tab.side, info));
            *w = rest;
            on
        });
        let src = Src::of(&info.id);
        for i in 0..2 {
            let mine: Vec<&Waiting> = back.iter().filter(|x| x.pane == i).collect();
            if mine.is_empty() {
                continue;
            }
            let p = self.panes[i];
            let Some(a) = self.as_applied(i).filter(|_| self.editor.with_untracked(Option::is_none)) else {
                p.tabs.update(|ts| ts.extend(mine.iter().map(|x| tab_of(src, &x.tab))));
                continue;
            };
            let visible = i == 0 || self.split.get_untracked();
            let stand_in = a.order == [None];
            if visible && !stand_in {
                self.save_tab(p);
            }
            let (mut tabs, mut order) = (p.tabs.get_untracked(), a.order);
            // The saved place of the tab shown, and whether another is to be shown.
            let (mut shown, mut switch) = (order[a.tab], stand_in);
            if stand_in {
                tabs.clear();
                order.clear();
            }
            for x in mine {
                let pos = order.iter().position(|o| o.is_some_and(|k| k > x.at)).unwrap_or(order.len());
                order.insert(pos, Some(x.at));
                tabs.insert(pos, tab_of(src, &x.tab));
                if x.shown {
                    (shown, switch) = (Some(x.at), true);
                }
            }
            let tab = shown.and_then(|k| order.iter().position(|o| *o == Some(k))).unwrap_or(0);
            p.tabs.set(tabs);
            p.tab.set(tab);
            if switch {
                if visible {
                    self.go_to_tab(p);
                } else {
                    forget(p);
                }
            }
            APPLIED.with_borrow_mut(|ap| ap[i] = Some(Applied { order, tab, landed: a.landed && !switch }));
        }
    }

    pub(super) fn set_workspaces(self, list: Vec<Workspace>) {
        store(WORKSPACES_KEY, &serde_json::to_string(&list).unwrap_or_default());
        self.workspaces.set(list);
    }

    /// Saves the workspace as it is now under `name`, replacing one of that name.
    pub(super) fn save_workspace(self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let mut list = self.workspaces.get_untracked();
        let w = self.snapshot(name.clone());
        match list.iter_mut().find(|x| x.name == name) {
            Some(x) => *x = w,
            None => list.push(w),
        }
        self.set_workspaces(list);
    }

    pub(super) fn open_workspace(self, w: Workspace) {
        if self.may_leave() {
            self.apply(w);
        }
    }

    /// What a saved workspace holds, in words.
    pub(super) fn workspace_tip(self, w: &Workspace) -> String {
        let panes = if w.split { 2 } else { 1 };
        let tabs: Vec<String> = w.panes[..panes]
            .iter()
            .flat_map(|p| p.tabs.iter())
            .map(|t| {
                let on = match &t.side {
                    Side::Mac => crate::os::the_computer().to_string(),
                    s => self.side_name(s),
                };
                format!("{} on {on}", if t.cwd.is_empty() || t.cwd == "/" { "All storage".into() } else { base(&t.cwd) })
            })
            .collect();
        format!("Open: {}", tabs.join(", "))
    }
}
