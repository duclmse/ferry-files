//! Run script ▸: the user's Lua scripts (the backend's `scripts.rs`), run on what's picked in a
//! pane, or on its folder. They're in the app's scripts folder and in the files and folders added
//! in Settings > Scripts. The pane's notice shows a script running (with Stop), then what it printed.

use super::*;
use crate::api::{ScriptInfo, ScriptReport};

/// The script files and folders added in Settings > Scripts (a JSON list of app paths).
pub(super) const PLACES_KEY: &str = "ferry-files.script-places";
/// The scripts folder picked in Settings > Scripts (an app path); none: the app's own.
pub(super) const FOLDER_KEY: &str = "ferry-files.scripts-folder";
/// Scripts' shortcuts: a JSON map from a script's path to its chord.
pub(super) const KEYS_KEY: &str = "ferry-files.script-keys";

impl Ctx {
    /// Finds the scripts again, for the menu.
    pub(super) fn load_scripts(self) {
        let places = self.script_places.get_untracked();
        let folder = self.script_folder.get_untracked();
        spawn_local(async move {
            match call::<Vec<ScriptInfo>>("scripts", json!({ "places": places, "folder": folder }))
                .await
            {
                Ok(list) => {
                    if self.scripts.with_untracked(|v| *v != list) {
                        self.scripts.set(list);
                    }
                    self.script_error.set(None);
                }
                Err(e) => self.script_error.set(Some(e)),
            }
        });
    }

    /// The script `chord` runs, if it's one that's found.
    pub(super) fn script_for(self, chord: &str) -> Option<ScriptInfo> {
        let path = self
            .script_keys
            .with_untracked(|k| k.iter().find(|(_, c)| *c == chord).map(|(p, _)| p.clone()))?;
        self.scripts
            .with_untracked(|v| v.iter().find(|s| s.path == path).cloned())
    }

    /// The shortcut of the script at `path`, as shown ("Ctrl+Shift+1"), or "".
    pub(super) fn script_key_label(self, path: &str) -> String {
        self.script_keys.with(|k| {
            k.get(path)
                .map(|c| super::keymap::label(c))
                .unwrap_or_default()
        })
    }

    fn set_script_keys(self, f: impl FnOnce(&mut HashMap<String, String>)) {
        self.script_keys.update(f);
        self.script_keys
            .with_untracked(|k| store(KEYS_KEY, &serde_json::to_string(k).unwrap_or_default()));
    }

    /// A key pressed while the script at `path` waits for its shortcut: Esc stops waiting, ⌫ or
    /// Delete takes the shortcut away, a chord one of the app's commands has is refused (it's
    /// changed under Keyboard), and any other becomes the script's, taken from another script.
    pub(super) fn capture_script_key(self, path: String, chord: String) {
        self.script_capturing.set(None);
        self.script_key_note.set(None);
        match chord.as_str() {
            "Escape" => return,
            "Backspace" | "Delete" => {
                self.set_script_keys(|k| {
                    k.remove(&path);
                });
                return;
            }
            _ => {}
        }
        if let Some(c) = self.command_for(&chord) {
            self.script_key_note.set(Some(format!(
                "{} is the shortcut of “{}”. Pick another, or change that one under Keyboard.",
                super::keymap::label(&chord),
                c.what
            )));
            return;
        }
        self.set_script_keys(|k| {
            k.retain(|_, c| *c != chord);
            k.insert(path, chord);
        });
    }

    /// Uses `folder` (an app path; empty: the app's own) as the scripts folder.
    fn set_script_folder(self, folder: String) {
        store(FOLDER_KEY, &folder);
        self.script_folder.set(folder);
        self.load_scripts();
    }

    fn set_script_places(self, places: Vec<String>) {
        store(
            PLACES_KEY,
            &serde_json::to_string(&places).unwrap_or_default(),
        );
        self.script_places.set(places);
        self.load_scripts();
    }

    /// Runs `script` on `items` in pane `p`'s folder (none: on the folder); the folder is listed
    /// again when the script changed something.
    pub(super) fn run_script(self, p: Pane, script: ScriptInfo, items: Vec<String>) {
        p.notice.set(Notice::ScriptRunning(script.name.clone()));
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        let again = items.clone();
        spawn_local(async move {
            let args =
                json!({ "src": src.key(), "dir": dir, "items": items, "script": script.path });
            match call::<ScriptReport>("run_script", args).await {
                Ok(report) => {
                    if report.changed > 0 {
                        self.reload(p);
                    }
                    p.notice.set(Notice::ScriptDone {
                        script,
                        items: again,
                        report,
                    });
                }
                Err(e) => p.error(e),
            }
        });
    }
}

/// A script running, with Stop.
pub(super) fn running(name: String) -> AnyView {
    view! {
        <div class="notice"><span class="text">{format!("Running “{name}”…")}</span>
            <button class="btn" title="Stop the script" aria-label="Stop the script"
                on:click=move |_| spawn_local(async move { let _ = call::<()>("stop_script", json!({})).await; })>{ico(Icon::Stop)}"Stop"</button>
        </div>
    }
    .into_any()
}

/// What a script printed, why it stopped (when it didn't end by itself), and how much it changed.
pub(super) fn done(
    ctx: Ctx,
    p: Pane,
    script: ScriptInfo,
    items: Vec<String>,
    r: ScriptReport,
) -> AnyView {
    let name = &script.name;
    let n = r.changed;
    let s = if n == 1 { "" } else { "s" };
    let summary = match (&r.error, n) {
        (Some(_), 0) => format!("“{name}” stopped. Nothing was changed."),
        (Some(_), n) => format!("“{name}” stopped after changing {n} item{s}. Files it replaced or deleted are in Backups."),
        (None, 0) => format!("“{name}” ran. Nothing was changed."),
        (None, n) => format!("“{name}” ran and changed {n} item{s}. Files it replaced or deleted are in Backups."),
    };
    let failed = r.error.is_some();
    let again = StoredValue::new((script, items));
    view! {
        <div class="notice script-done" class:error=failed>
            <span class="text">{summary}</span>
            <button class="btn icon-only" title="Run the script again" aria-label="Run again"
                on:click=move |_| { let (s, i) = again.get_value(); ctx.run_script(p, s, i) }>{ico(Icon::Play)}</button>
            <button class="btn icon-only" title="Dismiss" aria-label="Dismiss" on:click=move |_| p.notice.set(Notice::None)>{ico(Icon::Close)}</button>
            {(!r.output.is_empty() || failed).then(|| view! {
                <pre class="script-out">
                    {r.output.join("\n")}
                    {r.error.map(|e| view! { <span class="script-error">{if r.output.is_empty() { e } else { format!("\n{e}") }}</span> })}
                </pre>
            })}
        </div>
    }
    .into_any()
}

/// Settings > Scripts: where scripts are found, and the scripts found there.
#[component]
pub(super) fn ScriptSettings() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    ctx.load_scripts();
    // Leaving Settings stops waiting for a shortcut.
    on_cleanup(move || {
        let _ = ctx.script_capturing.try_set(None);
        let _ = ctx.script_key_note.try_set(None);
    });
    let error = RwSignal::new(None::<String>);
    // Where the app's own scripts folder is: the one the guide is in.
    let own = RwSignal::new(String::new());
    spawn_local(async move {
        if let Ok(guide) = call::<String>("script_guide", json!({})).await {
            let _ = own.try_set(parent(&guide));
        }
    });
    let add = move |folder: bool| {
        spawn_local(async move {
            match call::<Vec<String>>("pick_scripts", json!({ "folder": folder })).await {
                Ok(got) if !got.is_empty() => {
                    let mut places = ctx.script_places.get_untracked();
                    places.extend(
                        got.into_iter()
                            .filter(|g| !places.contains(g))
                            .collect::<Vec<_>>(),
                    );
                    ctx.set_script_places(places);
                }
                Ok(_) => {}
                Err(e) => error.set(Some(e)),
            }
        });
    };
    // The app's folder through Settings' own command; another one shown in its folder.
    let open_folder = move |_| {
        let custom = ctx.script_folder.get_untracked();
        spawn_local(async move {
            let r = if custom.is_empty() {
                call::<()>("open_folder", json!({ "which": "scripts" })).await
            } else {
                call::<()>("reveal", json!({ "path": custom })).await
            };
            if let Err(e) = r {
                error.set(Some(e));
            }
        })
    };
    let pick_folder = move || {
        spawn_local(async move {
            match call::<Vec<String>>("pick_scripts", json!({ "folder": true })).await {
                Ok(got) => {
                    if let Some(f) = got.into_iter().next() {
                        ctx.set_script_folder(f);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let shown = |path: &str| crate::os::native_path(path);
    // Opens a script in the editor, over the active pane.
    let edit = move |path: String| {
        ctx.settings.set(false);
        ctx.open_text_on(ctx.act(), Src::Mac, path);
    };
    let new_name = RwSignal::new(String::new());
    let make = move || {
        let name = new_name.get_untracked();
        spawn_local(async move {
            match call::<String>(
                "new_script",
                json!({ "name": name, "folder": ctx.script_folder.get_untracked() }),
            )
            .await
            {
                Ok(path) => {
                    new_name.set(String::new());
                    ctx.load_scripts();
                    edit(path);
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    view! {
        <p class="hint flush">"Lua scripts to run on the files picked (or the folder), from Run script in the right-click menu. They're found in the scripts folder and in the files and folders added here. The scripting guide says what a script can do."</p>
        <div class="script-places">
            <div class="script-place">
                <span class="path" title=move || { let f = ctx.script_folder.get(); shown(&if f.is_empty() { own.get() } else { f }) }>
                    <b>"The scripts folder (the app's)"</b>
                    <br/>
                    <span class="about">{move || { let f = ctx.script_folder.get(); shown(&if f.is_empty() { own.get() } else { f }) }}</span>
                </span>
                <button class="btn icon-only" title=format!("Open in {}", crate::os::file_manager()) aria-label=format!("Open in {}", crate::os::file_manager())
                    on:click=open_folder>{ico(Icon::Folder)}</button>
                <button class="btn icon-only" title="Use another folder as the scripts folder (new scripts go there)" aria-label="Use another folder"
                    on:click=move |_| pick_folder()>{ico(Icon::FolderSwap)}</button>
                {move || (!ctx.script_folder.get().is_empty()).then(|| view! {
                    <button class="btn icon-only" title="Use the app's own scripts folder again" aria-label="Use the app's folder"
                        on:click=move |_| ctx.set_script_folder(String::new())>{ico(Icon::Restore)}</button>
                })}
            </div>
            {move || ctx.script_places.get().into_iter().map(|place| {
                let gone = place.clone();
                view! {
                    <div class="script-place">
                        <span class="path" title=shown(&place)>{shown(&place)}</span>
                        <button class="btn icon-only" title="Remove from the list (the files stay)" aria-label="Remove"
                            on:click=move |_| ctx.set_script_places(ctx.script_places.get_untracked().into_iter().filter(|p| *p != gone).collect())>{ico(Icon::Close)}</button>
                    </div>
                }
            }).collect_view()}
        </div>
        <div class="row-buttons">
            <button class="btn icon-only" title="Add script files…" aria-label="Add script files" on:click=move |_| add(false)>{ico(Icon::Plus)}</button>
            <button class="btn icon-only" title="Add a folder of scripts…" aria-label="Add a folder of scripts" on:click=move |_| add(true)>{ico(Icon::FolderPlus)}</button>
            <button class="btn icon-only" title="Look for scripts again" aria-label="Look for scripts again" on:click=move |_| ctx.load_scripts()>{ico(Icon::Refresh)}</button>
            <button class="btn icon-only" title="Scripting guide: what a script can do, with examples" aria-label="Scripting guide"
                on:click=move |_| spawn_local(async move {
                    match call::<String>("script_guide", json!({})).await {
                        Ok(path) => edit(path),
                        Err(e) => error.set(Some(e)),
                    }
                })>{ico(Icon::Book)}</button>
        </div>
        <div class="row-buttons">
            <input class="script-name" placeholder="New script's name" prop:value=move || new_name.get()
                on:input=move |ev| new_name.set(event_target_value(&ev))
                on:keydown=move |ev| if ev.key() == "Enter" { make() }/>
            <button class="btn icon-only" aria-label="New script" disabled=move || new_name.with(|n| n.trim().is_empty())
                title="Make a script in the scripts folder and open it in the editor" on:click=move |_| make()>{ico(Icon::Plus)}</button>
        </div>
        {move || error.get().or_else(|| ctx.script_error.get()).map(|e| view! { <p class="error">{e}</p> })}
        <h3>"Scripts"</h3>
        <p class="hint flush">"A script with a shortcut runs on what's picked (or the folder) when it's pressed."</p>
        {move || ctx.script_key_note.get().map(|e| view! { <p class="error">{e}</p> })}
        {move || {
            let list = ctx.scripts.get();
            if list.is_empty() {
                return view! { <p class="hint">"None found yet."</p> }.into_any();
            }
            view! {
                <div class="script-places">
                    {list.into_iter().map(|s| {
                        let path = s.path.clone();
                        view! {
                            <div class="script-place" title=shown(&s.path)>
                                <span class="path"><b>{s.name.clone()}</b>" "<span class="about">{s.about.clone()}</span></span>
                                {
                                    let at = StoredValue::new(s.path.clone());
                                    let waiting = move || at.with_value(|a| ctx.script_capturing.with(|c| c.as_deref() == Some(a.as_str())));
                                    let key = move || at.with_value(|a| ctx.script_key_label(a));
                                    view! {
                                        <button class="btn script-key" class:waiting=waiting
                                            title="Click, then press the shortcut (Esc to stop, ⌫ to take it away)"
                                            on:click=move |_| { ctx.script_key_note.set(None); ctx.script_capturing.set(Some(at.get_value())) }>
                                            {move || if waiting() { "Press a key…".to_string() } else {
                                                match key() { k if k.is_empty() => "Add shortcut".to_string(), k => k }
                                            }}
                                        </button>
                                    }
                                }
                                <button class="btn icon-only" title=format!("View and edit {}", s.name) aria-label="View and edit"
                                    on:click=move |_| edit(path.clone())>{ico(Icon::Edit)}</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            }.into_any()
        }}
    }
}
