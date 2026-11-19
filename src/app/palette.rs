//! The command palette (⌘K): every action the menus offer for the active pane, and the places to
//! go (favourites, devices, recent folders, sync pairs), found by typing a few letters.

use super::*;

const RECENT_KEY: &str = "ferry-files.recent";
/// Recent folders kept.
const RECENT_MAX: usize = 20;

fn recent() -> Vec<Fav> {
    stored(RECENT_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

impl Ctx {
    /// Puts a folder at the top of the recent folders.
    pub(super) fn note_recent(self, src: Src, path: &str) {
        let place = self.place_key(src);
        if place.is_empty() || path.is_empty() {
            return;
        }
        let mut list = recent();
        list.retain(|f| !(f.place == place && f.path == path));
        let label = self.src_info(src).map_or_else(String::new, |i| i.label);
        list.insert(0, Fav { place, label, path: path.to_string() });
        list.truncate(RECENT_MAX);
        store(RECENT_KEY, &serde_json::to_string(&list).unwrap_or_default());
    }
}

/// What a choice does.
#[derive(Clone)]
enum Cmd {
    /// A menu action on the active pane, for its picked item when there is one.
    Act(Act, Option<Entry>),
    /// Opens a folder in the active pane.
    Go(Src, String),
    Pair(SyncPair),
    Workspace(Workspace),
    Do(fn(Ctx)),
}

/// An app-wide command: its label, its shortcut's command (keymap.rs), and what it does.
type AppCmd = (&'static str, &'static str, fn(Ctx));

#[derive(Clone)]
struct Choice {
    group: &'static str,
    label: String,
    key: &'static str,
    cmd: Cmd,
}

/// How well `query` (lowercase, no spaces) matches `text` (lowercase): its letters in order,
/// scoring runs of letters and word starts; `None` when they aren't all there.
fn score(query: &[char], t: &[char]) -> Option<i32> {
    let (mut at, mut total, mut prev) = (0usize, 0i32, usize::MAX);
    for &c in query {
        let i = (at..t.len()).find(|&i| t[i] == c)?;
        if prev != usize::MAX && i == prev + 1 {
            total += 4;
        }
        if i == 0 || !t[i - 1].is_alphanumeric() {
            total += 3;
        }
        total -= (i - at).min(4) as i32;
        prev = i;
        at = i + 1;
    }
    Some(total)
}

fn lower(s: &str) -> Vec<char> {
    s.to_lowercase().chars().collect()
}

/// A menu's items as choices: the enabled ones, with ✓ marks turned into words.
fn from_menu(group: &'static str, items: Vec<MenuItem>, entry: Option<&Entry>, out: &mut Vec<Choice>) {
    for it in items {
        let MenuItem::Do { label, key, act, enabled: true, .. } = it else { continue };
        let checked = label.starts_with('✓');
        let bare = label.trim_start_matches(['✓', '\u{2003}', ' ']).to_string();
        let label = match act {
            Act::View(_) if checked => continue,
            Act::View(_) => format!("View {bare}"),
            Act::Column(_) => format!("{} the {bare} column", if checked { "Hide" } else { "Show" }),
            Act::Go(_) => continue,
            _ => bare,
        };
        if out.iter().any(|c| c.label == label) {
            continue;
        }
        out.push(Choice { group, label, key, cmd: Cmd::Act(act, entry.cloned()) });
    }
}

impl Ctx {
    /// Everything the palette offers, for the active pane as it is now.
    fn choices(self) -> Vec<Choice> {
        let p = self.act();
        let mut out = vec![];
        let picked = p.picked();
        if let Some(e) = picked.first() {
            let group = if picked.len() > 1 { "Picked" } else if e.dir { "Folder" } else { "File" };
            from_menu(group, self.menu_items(p, Some(e)), Some(e), &mut out);
        }
        from_menu("Here", self.menu_items(p, None), None, &mut out);
        let app: [AppCmd; 7] = [
            ("Settings", "", |c| c.settings.set(true)),
            ("Keyboard shortcuts", "help", |c| c.show_shortcuts()),
            ("Backups", "", |c| c.panel.set(Some(Panel::Backups))),
            ("History", "", |c| c.panel.set(Some(Panel::History))),
            ("Refresh devices", "", |c| c.refresh_devices()),
            ("Connect over Wi-Fi…", "", |c| c.wifi.set(true)),
            ("Go up a folder", "up", |c| c.go_up(c.act())),
        ];
        out.extend(app.into_iter().map(|(label, key, f)| Choice { group: "App", label: label.into(), key, cmd: Cmd::Do(f) }));
        let here = (p.src.get_untracked(), p.cwd.get_untracked());
        let mut seen: HashSet<(Src, String)> = HashSet::from([here]);
        let mut go = |out: &mut Vec<Choice>, group: &'static str, src: Src, path: String, label: String| {
            if seen.insert((src, path.clone())) {
                out.push(Choice { group, label, key: "", cmd: Cmd::Go(src, path) });
            }
        };
        let at = |src: Src, path: &str| format!("{} on {}", if path == "/" { "All storage".to_string() } else { base(path) }, self.place(src));
        for f in self.favs.get_untracked() {
            if let Some(src) = self.fav_src(&f) {
                go(&mut out, "Favourite", src, f.path.clone(), at(src, &f.path));
            }
        }
        if let Some(mac) = self.mac.get_untracked() {
            go(&mut out, "Device", Src::Mac, mac.default_dir.clone(), at(Src::Mac, &mac.default_dir));
        }
        for i in self.phones.get_untracked() {
            let src = Src::of(&i.id);
            go(&mut out, "Device", src, "/".into(), at(src, "/"));
            for (name, path) in i.quick_dirs {
                go(&mut out, "Device", src, path, format!("{name} on {}", i.label));
            }
        }
        for f in recent() {
            if let Some(src) = self.fav_src(&f) {
                let label = format!("{} — {}", at(src, &f.path), f.path);
                go(&mut out, "Recent", src, f.path.clone(), label);
            }
        }
        for w in self.workspaces.get_untracked() {
            let label = format!("Open workspace {}", w.name);
            out.push(Choice { group: "Workspace", label, key: "", cmd: Cmd::Workspace(w) });
        }
        for pair in self.sync_pairs.get_untracked() {
            let label = format!("Sync {} on {} → {} on {}", base(&pair.from_dir), self.side_name(&pair.from), base(&pair.to_dir), self.side_name(&pair.to));
            out.push(Choice { group: "Sync pair", label, key: "", cmd: Cmd::Pair(pair) });
        }
        out
    }

    fn run_choice(self, c: Choice) {
        self.panel.set(None);
        let p = self.act();
        match c.cmd {
            Cmd::Act(act, entry) => self.run(p, act, entry),
            Cmd::Go(src, path) => self.open_place(src, path),
            Cmd::Pair(pair) => self.open_pair(&pair),
            Cmd::Workspace(w) => self.open_workspace(w),
            Cmd::Do(f) => f(self),
        }
    }
}

#[component]
pub(super) fn Palette() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let choices = StoredValue::new(ctx.choices());
    // What typing is matched against, per choice: the label, and the group with the label.
    let hay = StoredValue::new(choices.with_value(|cs| cs.iter().map(|c| (lower(&c.label), lower(&format!("{} {}", c.group, c.label)))).collect::<Vec<_>>()));
    let query = RwSignal::new(String::new());
    let at = RwSignal::new(0usize);
    // Indices of the choices that match, best first; all of them, in order, before typing.
    let shown = Memo::new(move |_| {
        let q: Vec<char> = query.with(|q| q.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect());
        hay.with_value(|hs| {
            if q.is_empty() {
                return (0..hs.len()).collect::<Vec<_>>();
            }
            let mut hits: Vec<(i32, usize)> = hs
                .iter()
                .enumerate()
                .filter_map(|(i, (own, with_group))| score(&q, own).max(score(&q, with_group).map(|s| s - 2)).map(|s| (s, i)))
                .collect();
            hits.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), i));
            hits.into_iter().map(|(_, i)| i).collect()
        })
    });
    let input = NodeRef::<html::Input>::new();
    input.on_load(move |el| {
        request_animation_frame(move || {
            let _ = el.focus();
        });
    });
    let run = move |i: usize| {
        if let Some(c) = choices.with_value(|cs| cs.get(i).cloned()) {
            ctx.run_choice(c);
        }
    };
    let reveal = move || {
        request_animation_frame(|| {
            if let Some(el) = document().query_selector(".palette .choice.on").ok().flatten() {
                el.scroll_into_view_with_bool(false);
            }
        });
    };
    let keys = move |e: web_sys::KeyboardEvent| {
        let n = shown.with_untracked(Vec::len);
        match e.key().as_str() {
            "ArrowDown" if n > 0 => {
                e.prevent_default();
                at.update(|a| *a = (*a + 1) % n);
                reveal();
            }
            "ArrowUp" if n > 0 => {
                e.prevent_default();
                at.update(|a| *a = (*a + n - 1) % n);
                reveal();
            }
            "Enter" => {
                e.prevent_default();
                if let Some(i) = shown.with_untracked(|s| s.get(at.get_untracked()).copied()) {
                    run(i);
                }
            }
            _ if chord_of(&e).and_then(|c| ctx.command_for(&c)).is_some_and(|c| c.id == "palette") => {
                e.prevent_default();
                // Not on to the window, which would open it again.
                e.stop_propagation();
                ctx.panel.set(None);
            }
            _ => {}
        }
    };
    view! {
        <div class="overlay palette-overlay" on:click=move |_| ctx.panel.set(None)>
            <div class="dialog palette" role="dialog" aria-label="Commands" on:click=|e| e.stop_propagation()>
                <input node_ref=input class="palette-q" type="text" placeholder="Type an action, folder or device"
                    aria-label="Search actions and places" spellcheck="false" autocomplete="off"
                    prop:value=move || query.get() on:keydown=keys
                    on:input=move |ev| { query.set(event_target_value(&ev)); at.set(0); }/>
                <div class="palette-list" role="listbox">
                    {move || {
                        let list = shown.get();
                        if list.is_empty() {
                            return view! { <p class="hint">"Nothing matches."</p> }.into_any();
                        }
                        list.into_iter().enumerate().map(|(row, i)| {
                            let c = choices.with_value(|cs| cs[i].clone());
                            view! {
                                <button class="choice" role="option" class:on=move || at.get() == row aria-selected=move || (at.get() == row).to_string()
                                    on:mousemove=move |_| if at.get_untracked() != row { at.set(row) }
                                    on:click=move |_| run(i)>
                                    <span class="group">{c.group}</span>
                                    <span class="label">{c.label}</span>
                                    <span class="key">{move || ctx.key_label(c.key)}</span>
                                </button>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
                <p class="hint">"↑ ↓ to choose · ↵ to run · Esc to close. Actions work on the active pane and what's picked in it."</p>
            </div>
        </div>
    }
}
