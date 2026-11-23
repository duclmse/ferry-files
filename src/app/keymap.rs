//! Keyboard shortcuts that can be changed in the Keyboard shortcuts dialog.
//!
//! A chord is written as "ctrl+alt+shift+cmd+Code": the modifiers in that order, then the key's
//! `KeyboardEvent.code` ("KeyK", "BracketRight", "F5"), so it doesn't depend on what the key types.
//! `cmd` is ⌘ on the Mac and Ctrl elsewhere; `ctrl` is only the Mac's ⌃ (elsewhere it's `cmd`).
//! The user's changes are kept in localStorage as a map from command to chords; commands not in it
//! have their defaults.

use super::*;

pub(super) const KEYS_KEY: &str = "ferry-files.keys";

pub(super) struct Command {
    pub id: &'static str,
    pub what: &'static str,
    pub keys: &'static [&'static str],
    /// Works even while typing in a field (the filter), when the chord has ⌘ or ⌃.
    pub anywhere: bool,
}

const fn c(id: &'static str, what: &'static str, keys: &'static [&'static str], anywhere: bool) -> Command {
    Command { id, what, keys, anywhere }
}

pub(super) const COMMANDS: &[Command] = &[
    c("open", "Open", &["Enter", "cmd+ArrowDown"], false),
    c("up", "Up a folder", &["cmd+ArrowUp", "Backspace"], false),
    c("quick_look", "Quick Look", &["Space"], false),
    c("media_play", "Play or pause the video or sound in the preview", &["KeyK"], false),
    c("media_back", "Back 5 seconds in the preview's video or sound", &["KeyJ"], false),
    c("media_forward", "Forward 10 seconds in the preview's video or sound (hold for 2x)", &["KeyL"], false),
    c("media_mute", "Sound on or off for the preview's video or sound", &["KeyM"], false),
    c("palette", "Commands: every action, favourites, devices, recent folders", &["cmd+KeyK"], true),
    c("filter", "Filter; Enter searches subfolders", &["cmd+KeyF", "Slash"], false),
    c("type_path", "Type a path", &["cmd+KeyL"], false),
    c("rename", "Rename (with several picked: rename them together)", &["F2"], false),
    c("replace", "Replace in names: the Replace with box under the search box", &["cmd+KeyR"], true),
    c("replace_names", "Find and replace in names (what's picked, or everything shown)", &["alt+cmd+KeyF"], false),
    c("delete", "Delete (on the computer: move to the Trash)", &["cmd+Backspace", "Delete"], false),
    c("delete_forever", "Delete permanently, with no backup (files on the computer)", &["shift+Delete", "shift+Backspace"], false),
    c("copy", "Copy to the other pane", &["F5"], false),
    c("move", "Move to the other pane", &["F6"], false),
    c("copy_items", "Copy, to paste in another folder (or in Finder)", &["cmd+KeyC"], false),
    c("cut_items", "Cut, to move to another folder with Paste", &["cmd+KeyX"], false),
    c("paste", "Paste what was copied (here or in Finder) into this folder", &["cmd+KeyV"], false),
    c("pick_all", "Pick everything", &["cmd+KeyA"], false),
    c("undo", "Undo the last rename, new item, delete, copy or move", &["cmd+KeyZ"], false),
    c("other_pane", "The other pane", &["Tab"], false),
    c("split", "Show or hide the second pane", &["cmd+Backslash"], false),
    c("terminal", "Show or hide the terminal (this computer, adb phones, SFTP servers)", &["ctrl+Backquote"], true),
    c("hidden", "Show or hide hidden files (names starting with a dot)", &["cmd+Period"], true),
    c("sidebar", "Show or hide the sidebar", &["alt+cmd+KeyS"], true),
    c("new_tab", "New tab", &["cmd+KeyT"], true),
    c("close_tab", "Close tab", &["cmd+KeyW"], true),
    c("next_tab", "Next tab", &["shift+cmd+BracketRight", "ctrl+Tab"], true),
    c("prev_tab", "Previous tab", &["shift+cmd+BracketLeft", "ctrl+shift+Tab"], true),
    c("help", "Keyboard shortcuts", &["shift+Slash"], false),
];

pub(super) fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// The modifiers and key of a chord.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    cmd: bool,
}

fn join(m: Mods, code: &str) -> String {
    let mut s = String::new();
    for (on, name) in [(m.ctrl, "ctrl+"), (m.alt, "alt+"), (m.shift, "shift+"), (m.cmd, "cmd+")] {
        if on {
            s.push_str(name);
        }
    }
    s.push_str(code);
    s
}

fn split(chord: &str) -> (Mods, &str) {
    let mut m = Mods::default();
    let mut rest = chord;
    while let Some((head, tail)) = rest.split_once('+') {
        match head {
            "ctrl" => m.ctrl = true,
            "alt" => m.alt = true,
            "shift" => m.shift = true,
            "cmd" => m.cmd = true,
            _ => break,
        }
        rest = tail;
    }
    (m, rest)
}

/// A chord as this platform reads it: away from the Mac, `ctrl` is `cmd`.
fn norm(chord: &str) -> String {
    let (mut m, code) = split(chord);
    if !crate::os::is_mac() && m.ctrl {
        m.ctrl = false;
        m.cmd = true;
    }
    join(m, code)
}

/// The chord of a key press, or `None` for a modifier on its own.
pub(super) fn chord_of(e: &web_sys::KeyboardEvent) -> Option<String> {
    let code = e.code();
    let lone = matches!(
        code.as_str(),
        "" | "ShiftLeft"
            | "ShiftRight"
            | "ControlLeft"
            | "ControlRight"
            | "AltLeft"
            | "AltRight"
            | "MetaLeft"
            | "MetaRight"
            | "CapsLock"
            | "Fn"
            | "OSLeft"
            | "OSRight"
    );
    if lone {
        return None;
    }
    let mac = crate::os::is_mac();
    let m = Mods { ctrl: mac && e.ctrl_key(), alt: e.alt_key(), shift: e.shift_key(), cmd: crate::os::cmd(e.meta_key(), e.ctrl_key()) };
    Some(join(m, &code))
}

/// Whether a chord holds ⌘ or ⌃ (Ctrl elsewhere), so it doesn't type anything into a field.
pub(super) fn held(chord: &str) -> bool {
    let (m, _) = split(chord);
    m.cmd || m.ctrl
}

/// A chord as shown: "⇧⌘]" on the Mac, "Ctrl+Shift+]" elsewhere.
pub(super) fn label(chord: &str) -> String {
    let (m, code) = split(chord);
    let mac = crate::os::is_mac();
    // ? is how people know Shift+/.
    if code == "Slash" && m == (Mods { shift: true, ..Mods::default() }) {
        return "?".into();
    }
    let key = match code {
        "Backslash" => "\\",
        "Slash" => "/",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Comma" => ",",
        "Period" => ".",
        "Semicolon" => ";",
        "Quote" => "'",
        "Backquote" => "`",
        "Minus" => "-",
        "Equal" => "=",
        "Space" => "Space",
        "Enter" | "NumpadEnter" => "↵",
        "Backspace" if mac => "⌫",
        "Delete" if mac => "⌦",
        "Escape" => "Esc",
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        c => c.strip_prefix("Key").or_else(|| c.strip_prefix("Digit")).unwrap_or(c),
    };
    let key = match code.strip_prefix("Numpad") {
        Some(n) if code != "NumpadEnter" => format!("Num {n}"),
        _ => key.to_string(),
    };
    let mut s = String::new();
    if mac {
        for (on, sym) in [(m.ctrl, "⌃"), (m.alt, "⌥"), (m.shift, "⇧"), (m.cmd, "⌘")] {
            if on {
                s.push_str(sym);
            }
        }
    } else {
        for (on, name) in [(m.cmd || m.ctrl, "Ctrl+"), (m.alt, "Alt+"), (m.shift, "Shift+")] {
            if on {
                s.push_str(name);
            }
        }
    }
    s.push_str(&key);
    s
}

pub(super) fn saved_keys() -> HashMap<String, Vec<String>> {
    stored(KEYS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn defaults(id: &str) -> Vec<String> {
    command(id).map(|c| c.keys.iter().map(|k| norm(k)).collect()).unwrap_or_default()
}

fn chords_in(k: &HashMap<String, Vec<String>>, id: &str) -> Vec<String> {
    k.get(id).cloned().unwrap_or_else(|| defaults(id))
}

impl Ctx {
    /// A command's chords now: the user's, or its defaults.
    pub(super) fn chords(self, id: &str) -> Vec<String> {
        self.keymap.with(|k| chords_in(k, id))
    }

    /// The first of a command's chords as shown ("" when it has none, or for no command). Tracked,
    /// so a label follows changes.
    pub(super) fn key_label(self, id: &str) -> String {
        if id.is_empty() {
            return String::new();
        }
        self.chords(id).first().map(|c| label(c)).unwrap_or_default()
    }

    /// "New tab (⌘T)", or the bare name when it has no shortcut.
    pub(super) fn with_key(self, name: &str, id: &str) -> String {
        match self.key_label(id) {
            k if k.is_empty() => name.to_string(),
            k => format!("{name} ({k})"),
        }
    }

    /// The command a chord runs, if any.
    pub(super) fn command_for(self, chord: &str) -> Option<&'static Command> {
        self.keymap.with_untracked(|k| COMMANDS.iter().find(|c| chords_in(k, c.id).iter().any(|m| m == chord)))
    }

    fn set_keys(self, f: impl FnOnce(&mut HashMap<String, Vec<String>>)) {
        self.keymap.update(|k| {
            f(k);
            // Back to its defaults is no change.
            k.retain(|id, chords| command(id).is_some() && defaults(id) != *chords);
        });
        self.keymap.with_untracked(|k| match k.is_empty() {
            true => store(KEYS_KEY, ""),
            false => store(KEYS_KEY, &serde_json::to_string(k).unwrap_or_default()),
        });
    }

    /// Gives `chord` to a command, taking it from any other that had it.
    pub(super) fn bind(self, id: &str, chord: String) {
        self.set_keys(|k| {
            for c in COMMANDS {
                let (other, mut chords) = (c.id, chords_in(k, c.id));
                let had = chords.contains(&chord);
                chords.retain(|c| *c != chord);
                if other == id {
                    chords.push(chord.clone());
                    k.insert(other.into(), chords);
                } else if had {
                    k.insert(other.into(), chords);
                }
            }
        });
    }

    pub(super) fn unbind(self, id: &str, chord: &str) {
        self.set_keys(|k| {
            let mut chords = chords_in(k, id);
            chords.retain(|c| c != chord);
            k.insert(id.into(), chords);
        });
    }

    pub(super) fn reset_keys(self, id: Option<&str>) {
        self.set_keys(|k| match id {
            Some(id) => {
                k.remove(id);
            }
            None => k.clear(),
        });
    }
}

/// The Keyboard section of Settings (also opened by ?). Each command's keys can be changed: + waits
/// for the next key press (Esc stops waiting), × removes one, and Reset puts back the defaults.
#[component]
pub(super) fn ShortcutsEditor() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    const FIXED: &[(&str, &str)] = &[
        ("↑ ↓", "Move through the files"),
        ("← →", "Previous or next picture, video or sound (list); left and right (thumbnails, columns)"),
        ("⌘-click, Shift-click", "Pick some"),
        ("Esc", "Unpick, or close"),
        ("Middle-click", "Open a sidebar folder in a new tab"),
        ("⌘S", "Save, in the editor"),
    ];
    let what = |w: &str| match w {
        "Quick Look" => crate::os::quick_look().to_string(),
        "Show or hide the sidebar" => format!("Show or hide the {}", crate::os::sidebar()),
        "Open a sidebar folder in a new tab" => format!("Open a {} folder in a new tab", crate::os::sidebar()),
        w => w.to_string(),
    };
    on_cleanup(move || ctx.capturing.set(None));
    let changed = move || ctx.keymap.with(|k| !k.is_empty());
    let query = RwSignal::new(String::new());
    // Keys given to more than one command: chord → the commands that have it.
    let dups = Memo::new(move |_| {
        let mut by: HashMap<String, Vec<&'static str>> = HashMap::new();
        ctx.keymap.with(|k| {
            for c in COMMANDS {
                for ch in chords_in(k, c.id) {
                    let ids = by.entry(ch).or_default();
                    if !ids.contains(&c.id) {
                        ids.push(c.id);
                    }
                }
            }
        });
        by.retain(|_, ids| ids.len() > 1);
        by
    });
    let matches = move |text: &str, keys: &[String]| {
        query.with(|q| {
            let q = q.trim().to_lowercase();
            q.is_empty() || text.to_lowercase().contains(&q) || keys.iter().any(|k| k.to_lowercase().contains(&q))
        })
    };
    let shows = move |c: &Command| matches(&what(c.what), &ctx.chords(c.id).iter().map(|ch| label(ch)).collect::<Vec<_>>());
    let fixed_shows = move |(k, w): &(&'static str, &'static str)| matches(&what(w), &[crate::os::keys(k).to_string()]);
    let any_command = move || COMMANDS.iter().any(shows);
    let any_fixed = move || FIXED.iter().any(fixed_shows);
    view! {
        <div class="shortcuts">
            <input class="key-search" type="search" placeholder="Search commands or keys" aria-label="Search commands or keys"
                prop:value=move || query.get() on:input=move |e| query.set(event_target_value(&e))/>
            {move || match dups.with(HashMap::len) {
                0 => None,
                1 => Some(view! { <p class="dup-warn">"One key is on more than one command, so only the first one runs. Remove it from the others."</p> }.into_any()),
                n => Some(view! { <p class="dup-warn">{format!("{n} keys are on more than one command, so only the first one runs. Remove them from the others.")}</p> }.into_any()),
            }}
            <dl>
                {COMMANDS.iter().map(|c| {
                    let id = c.id;
                    let waiting = move || ctx.capturing.get() == Some(id);
                    let mine = move || ctx.keymap.with(|k| k.contains_key(id));
                    // "⌘K is also on: Open", once per key this command shares.
                    let clashes = move || {
                        dups.with(|d| {
                            ctx.chords(id).iter().filter_map(|ch| {
                                let others: Vec<String> = d.get(ch)?.iter().filter(|o| **o != id).filter_map(|o| command(o)).map(|o| what(o.what)).collect();
                                Some(format!("{} is also on: {}", label(ch), others.join("; ")))
                            }).collect::<Vec<_>>()
                        })
                    };
                    view! {
                        <div class="key-row" class:hidden=move || !shows(c)>
                            <dt>{what(c.what)}</dt>
                            <dd>
                                {move || ctx.chords(id).into_iter().map(|ch| {
                                    let shown = label(&ch);
                                    let dup = dups.with(|d| d.contains_key(&ch));
                                    view! {
                                        <span class="chip" class:dup=dup>
                                            <kbd>{shown.clone()}</kbd>
                                            <button class="unbind" title=format!("Remove {shown}") aria-label=format!("Remove {shown}")
                                                on:click=move |_| ctx.unbind(id, &ch)>{ico(Icon::Close)}</button>
                                        </span>
                                    }
                                }).collect_view()}
                                <button class="bind" class:waiting=waiting
                                    title=move || if waiting() { "Press the keys, or Esc to stop" } else { "Add a key" } aria-label="Add a key"
                                    on:click=move |_| ctx.capturing.set(if waiting() { None } else { Some(id) })>
                                    {move || if waiting() { "Press keys…".into_any() } else { ico(Icon::Plus).into_any() }}
                                </button>
                                {move || mine().then(|| view! {
                                    <button class="reset" title="Reset: back to the default keys" aria-label="Reset" on:click=move |_| ctx.reset_keys(Some(id))>{ico(Icon::Restore)}</button>
                                })}
                            </dd>
                            {move || clashes().into_iter().map(|t| view! { <p class="dup-warn">{t}</p> }).collect_view()}
                        </div>
                    }
                }).collect_view()}
            </dl>
            <h3 class:hidden=move || !any_fixed()>"Fixed"</h3>
            <dl>
                {FIXED.iter().map(|f| {
                    let (k, w) = *f;
                    view! { <div class="key-row" class:hidden=move || !fixed_shows(f)><dt>{what(w)}</dt><dd><kbd>{crate::os::keys(k)}</kbd></dd></div> }
                }).collect_view()}
            </dl>
            {move || (!any_command() && !any_fixed()).then(|| view! { <p class="hint">"No command or key matches."</p> })}
            <button class="btn reset-all icon-only" title="Reset all shortcuts" aria-label="Reset all shortcuts" disabled=move || !changed() on:click=move |_| ctx.reset_keys(None)>{ico(Icon::Restore)}</button>
        </div>
    }
}
