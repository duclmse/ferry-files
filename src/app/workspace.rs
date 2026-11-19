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

thread_local! {
    /// Whether the session may be saved: not before it's been brought back at launch.
    static READY: Cell<bool> = const { Cell::new(false) };
    /// Tabs on phones that aren't connected, by pane.
    static WAITING: RefCell<Vec<(usize, SavedTab)>> = const { RefCell::new(vec![]) };
}

fn view_of(key: &str) -> ViewMode {
    ViewMode::ALL.into_iter().find(|v| v.key() == key).unwrap_or(ViewMode::List)
}

/// Whether a saved tab is on the phone `info`: by serial, else by name.
fn is_on(t: &SavedTab, info: &Info) -> bool {
    match &t.side {
        Side::Phone { serial: Some(s), .. } => info.serial.as_deref() == Some(s.as_str()),
        Side::Phone { serial: None, label, .. } => *label == info.label,
        Side::Mac => false,
    }
}

pub(super) fn saved_workspaces() -> Vec<Workspace> {
    stored(WORKSPACES_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

impl Ctx {
    /// The workspace as it is now, named `name`; tabs waiting for their phone are kept.
    fn snapshot(self, name: String) -> Workspace {
        let panes = [0, 1].map(|i| {
            let p = self.panes[i];
            let shown = p.tab.get_untracked();
            let kept: Vec<Option<SavedTab>> = p.tabs.with_untracked(|ts| {
                ts.iter()
                    .enumerate()
                    .map(|(at, t)| {
                        // The shown tab's entry is brought up to date only when leaving it.
                        // While a listing is on its way, the tab is where it's going.
                        let (src, cwd, view) = if at == shown {
                            let (src, cwd) = p.going_to.get_value().unwrap_or_else(|| (p.src.get_untracked(), p.cwd.get_untracked()));
                            (src, cwd, p.view.get_untracked())
                        } else {
                            (t.src, t.cwd.clone(), t.view)
                        };
                        // A tab left on a phone that's gone can't be found again.
                        (src == Src::Mac || self.src_info(src).is_some()).then(|| SavedTab { side: self.side(src), cwd, view: view.key().into() })
                    })
                    .collect()
            });
            // The shown tab, counted among those kept.
            let tab = kept[..shown.min(kept.len())].iter().flatten().count();
            let mut tabs: Vec<SavedTab> = kept.into_iter().flatten().collect();
            WAITING.with_borrow(|w| tabs.extend(w.iter().filter(|(pane, _)| *pane == i).map(|(_, t)| t.clone())));
            SavedPane { tabs, tab }
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
        self.side_w.set(w.side_w);
        store(SIDE_W_KEY, &w.side_w.to_string());
        self.pane_w.set(w.pane_w);
        store(PANE_W_KEY, &w.pane_w.to_string());
        self.set_side(w.sidebar);
        self.split.set(w.split);
        store(SPLIT_KEY, if w.split { "1" } else { "0" });
        for (i, saved) in w.panes.into_iter().enumerate() {
            let p = self.panes[i];
            let mut tabs = vec![];
            let mut shown = 0;
            for (at, t) in saved.tabs.into_iter().enumerate() {
                match self.src_of(&t.side) {
                    Ok(src) => {
                        if at == saved.tab {
                            shown = tabs.len();
                        }
                        tabs.push(Tab { src, cwd: t.cwd.clone(), selected: None, view: view_of(&t.view), root: t.cwd });
                    }
                    Err(_) => WAITING.with_borrow_mut(|wt| wt.push((i, t))),
                }
            }
            if tabs.is_empty() {
                tabs.push(Tab { src: Src::Mac, cwd: String::new(), selected: None, view: p.view.get_untracked(), root: String::new() });
            }
            p.tabs.set(tabs);
            p.tab.set(shown);
            if i == 0 || w.split {
                self.go_to_tab(p);
            }
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

    /// A phone connected: its waiting tabs come back, after the pane's others.
    pub(super) fn tabs_back(self, info: &Info) {
        let back: Vec<(usize, SavedTab)> = WAITING.with_borrow_mut(|w| {
            let (on, rest): (Vec<_>, Vec<_>) = w.drain(..).partition(|(_, t)| is_on(t, info));
            *w = rest;
            on
        });
        let src = Src::of(&info.id);
        for (i, t) in back {
            let p = self.panes[i];
            p.tabs.update(|ts| ts.push(Tab { src, cwd: t.cwd.clone(), selected: None, view: view_of(&t.view), root: t.cwd }));
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
