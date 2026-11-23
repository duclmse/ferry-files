//! The command palette (⌘K): every action the menus offer for the active pane, and the places to
//! go (favourites, devices, recent folders, sync pairs), found by typing a few letters. Typing `>`
//! first keeps to the actions; `:` finds files and folders on the computer by name, from fsd's
//! index.

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
    /// A file or folder on the computer that `:` found: a folder opens, a file is shown in its.
    Found(Entry),
}

/// How long typing after `:` pauses before fsd is asked.
const FIND_WAIT_MS: u64 = 200;

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

/// A menu's items as choices: the enabled ones, with ✓ marks turned into words, and those of its
/// submenus after the submenu's name ("Sort by: Size").
fn from_menu(group: &'static str, items: Vec<MenuItem>, entry: Option<&Entry>, out: &mut Vec<Choice>) {
    from_menu_in(group, "", items, entry, out)
}

fn from_menu_in(group: &'static str, under: &str, items: Vec<MenuItem>, entry: Option<&Entry>, out: &mut Vec<Choice>) {
    for it in items {
        let (label, key, act) = match it {
            MenuItem::Do { label, key, act, enabled: true, .. } => (label, key, act),
            MenuItem::Sub { label, items } => {
                from_menu_in(group, &label, items, entry, out);
                continue;
            }
            _ => continue,
        };
        let checked = label.starts_with('✓');
        // Without a shortcut after a tab (a script's).
        let label = label.split('\t').next().unwrap_or_default().to_string();
        let bare = label.trim_start_matches(['✓', '\u{2003}', ' ']).to_string();
        let label = match act {
            Act::View(_) if checked => continue,
            Act::View(_) => format!("View {bare}"),
            Act::Column(_) => format!("{} the {bare} column", if checked { "Hide" } else { "Show" }),
            Act::Go(_) => continue,
            // In a submenu, what's ticked is already so.
            _ if checked && !under.is_empty() => continue,
            _ if !under.is_empty() => format!("{under}: {bare}"),
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
            let group = if picked.len() > 1 {
                "Picked"
            } else if e.dir {
                "Folder"
            } else {
                "File"
            };
            from_menu(group, self.menu_items(p, Some(e)), Some(e), &mut out);
        }
        from_menu("Here", self.menu_items(p, None), None, &mut out);
        // The views, which the menu leaves to the buttons above the files.
        let now = p.view.get_untracked();
        from_menu("Here", ViewMode::ALL.into_iter().filter(|m| *m != now).map(|m| item(m.label(), "", Act::View(m))).collect(), None, &mut out);
        let app: [AppCmd; 8] = [
            ("Settings", "", |c| c.settings.set(true)),
            ("Keyboard shortcuts", "help", |c| c.show_shortcuts()),
            ("Backups", "", |c| c.panel.set(Some(Panel::Backups))),
            ("History", "", |c| c.panel.set(Some(Panel::History))),
            ("Refresh devices", "", |c| c.refresh_devices()),
            ("Connect over Wi-Fi…", "", |c| c.wifi.set(true)),
            ("Go up a folder", "up", |c| c.go_up(c.act())),
            ("Terminal", "terminal", |c| c.toggle_terminal(c.act())),
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
            Cmd::Found(e) if e.dir => self.open_place(Src::Mac, e.path),
            Cmd::Found(e) => self.navigate_select(p, Src::Mac, parent(&e.path), Some(e.path)),
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
    // After `:`, what fsd found for the text (or why it couldn't look), and the text asked last.
    let found = RwSignal::new(None::<Result<Vec<Entry>, String>>);
    let asked = StoredValue::new(String::new());
    let finding = move || query.with(|q| q.trim_start().starts_with(':'));
    // The choices that match, best first; all of them, in order, before typing. After `>` only
    // the actions; after `:` what fsd found.
    let shown = Signal::derive(move || {
        let q = query.get();
        let q = q.trim_start();
        if let Some(text) = q.strip_prefix(':') {
            let _ = text;
            return found.with(|f| match f {
                Some(Ok(es)) => es
                    .iter()
                    .map(|e| Choice {
                        group: if e.dir { "Folder" } else { "File" },
                        label: format!("{} — {}", e.name, crate::os::native_path(&parent(&e.path))),
                        key: "",
                        cmd: Cmd::Found(e.clone()),
                    })
                    .collect(),
                _ => vec![],
            });
        }
        let (only_acts, q) = match q.strip_prefix('>') {
            Some(rest) => (true, rest),
            None => (false, q),
        };
        let q: Vec<char> = q.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        choices.with_value(|cs| {
            hay.with_value(|hs| {
                let act = |i: usize| !only_acts || matches!(cs[i].cmd, Cmd::Act(..) | Cmd::Do(_));
                let mut hits: Vec<(i32, usize)> = hs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| act(*i))
                    .filter_map(|(i, (own, with_group))| {
                        if q.is_empty() {
                            return Some((0, i));
                        }
                        score(&q, own).max(score(&q, with_group).map(|s| s - 2)).map(|s| (s, i))
                    })
                    .collect();
                hits.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), i));
                hits.into_iter().map(|(_, i)| cs[i].clone()).collect::<Vec<_>>()
            })
        })
    });
    // Asks fsd once typing after `:` pauses.
    let timer = StoredValue::new_local(None::<TimeoutHandle>);
    Effect::new(move |_| {
        let q = query.get();
        let Some(text) = q.trim_start().strip_prefix(':').map(|t| t.trim().to_string()) else { return };
        if let Some(t) = timer.try_update_value(Option::take).flatten() {
            t.clear();
        }
        if text.is_empty() {
            asked.set_value(String::new());
            found.set(None);
            return;
        }
        let t = set_timeout_with_handle(
            move || {
                asked.set_value(text.clone());
                spawn_local(async move {
                    let r = call::<Vec<Entry>>("find_files", json!({ "text": text })).await;
                    // Only the last text typed counts.
                    if asked.try_with_value(|a| *a == text).unwrap_or(false) {
                        let _ = found.try_set(Some(r));
                        let _ = at.try_set(0);
                    }
                });
            },
            std::time::Duration::from_millis(FIND_WAIT_MS),
        )
        .ok();
        timer.set_value(t);
    });
    let input = NodeRef::<html::Input>::new();
    input.on_load(move |el| {
        request_animation_frame(move || {
            let _ = el.focus();
        });
    });
    let run = move |c: Choice| ctx.run_choice(c);
    let reveal = move || {
        request_animation_frame(|| {
            if let Some(el) = document().query_selector(".palette .choice.on").ok().flatten() {
                el.scroll_into_view_with_bool(false);
            }
        });
    };
    let keys = move |e: web_sys::KeyboardEvent| {
        let n = shown.with_untracked(|s| s.len());
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
                if let Some(c) = shown.with_untracked(|s| s.get(at.get_untracked()).cloned()) {
                    run(c);
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
                <input node_ref=input class="palette-q" type="text" placeholder="An action, folder or device; > for actions only, : to find files"
                    aria-label="Search actions and places" spellcheck="false" autocomplete="off"
                    prop:value=move || query.get() on:keydown=keys
                    on:input=move |ev| { query.set(event_target_value(&ev)); at.set(0); }/>
                <div class="palette-list" role="listbox">
                    {move || {
                        let list = shown.get();
                        if list.is_empty() {
                            let note = if !finding() {
                                "Nothing matches.".to_string()
                            } else {
                                match found.get() {
                                    _ if query.with(|q| q.trim_start()[1..].trim().is_empty()) => "Type part of a file's name to find it on the computer.".to_string(),
                                    None => "Looking…".to_string(),
                                    Some(Err(e)) => e,
                                    Some(Ok(_)) => "No file or folder has that in its name.".to_string(),
                                }
                            };
                            return view! { <p class="hint">{note}</p> }.into_any();
                        }
                        list.into_iter().enumerate().map(|(row, c)| {
                            let chosen = c.clone();
                            view! {
                                <button class="choice" role="option" class:on=move || at.get() == row aria-selected=move || (at.get() == row).to_string()
                                    on:mousemove=move |_| if at.get_untracked() != row { at.set(row) }
                                    on:click=move |_| run(chosen.clone())>
                                    <span class="group">{c.group}</span>
                                    <span class="label">{c.label}</span>
                                    <span class="key">{move || ctx.key_label(c.key)}</span>
                                </button>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
                <p class="hint">{move || if finding() {
                    "↑ ↓ to choose · ↵ to open · Esc to close. Files and folders on the computer, by name, from fsd's index."
                } else {
                    "↑ ↓ to choose · ↵ to run · Esc to close. > for actions only, : to find files. Actions work on the active pane and what's picked in it."
                }}</p>
            </div>
        </div>
    }
}
