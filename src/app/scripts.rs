//! Run script ▸: the user's Lua scripts (the backend's `scripts.rs`), run on what's picked in a
//! pane, or on its folder. They're in the app's scripts folder and in the files and folders added
//! in Settings > Scripts. The pane's notice shows a script running (with Stop), then what it printed.

use super::*;
use crate::api::{ScriptInfo, ScriptReport};

/// The script files and folders added in Settings > Scripts (a JSON list of app paths).
pub(super) const PLACES_KEY: &str = "ferry-files.script-places";
/// The scripts folder picked in Settings > Scripts (an app path); none: the app's own.
pub(super) const FOLDER_KEY: &str = "ferry-files.scripts-folder";

impl Ctx {
    /// Finds the scripts again, for the menu.
    pub(super) fn load_scripts(self) {
        let places = self.script_places.get_untracked();
        let folder = self.script_folder.get_untracked();
        spawn_local(async move {
            match call::<Vec<ScriptInfo>>("scripts", json!({ "places": places, "folder": folder })).await {
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

    /// Uses `folder` (an app path; empty: the app's own) as the scripts folder.
    fn set_script_folder(self, folder: String) {
        store(FOLDER_KEY, &folder);
        self.script_folder.set(folder);
        self.load_scripts();
    }

    fn set_script_places(self, places: Vec<String>) {
        store(PLACES_KEY, &serde_json::to_string(&places).unwrap_or_default());
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
            let args = json!({ "src": src.key(), "dir": dir, "items": items, "script": script.path });
            match call::<ScriptReport>("run_script", args).await {
                Ok(report) => {
                    if report.changed > 0 {
                        self.reload(p);
                    }
                    p.notice.set(Notice::ScriptDone { script, items: again, report });
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
pub(super) fn done(ctx: Ctx, p: Pane, script: ScriptInfo, items: Vec<String>, r: ScriptReport) -> AnyView {
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
    let error = RwSignal::new(None::<String>);
    let add = move |folder: bool| {
        spawn_local(async move {
            match call::<Vec<String>>("pick_scripts", json!({ "folder": folder })).await {
                Ok(got) if !got.is_empty() => {
                    let mut places = ctx.script_places.get_untracked();
                    places.extend(got.into_iter().filter(|g| !places.contains(g)).collect::<Vec<_>>());
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
            match call::<String>("new_script", json!({ "name": name, "folder": ctx.script_folder.get_untracked() })).await {
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
                <span class="path">{move || match ctx.script_folder.get() {
                    f if f.is_empty() => "The scripts folder (the app's)".to_string(),
                    f => format!("The scripts folder: {}", shown(&f)),
                }}</span>
                <button class="btn icon-only" title=format!("Open in {}", crate::os::file_manager()) aria-label=format!("Open in {}", crate::os::file_manager())
                    on:click=open_folder>{ico(Icon::Folder)}</button>
                <button class="btn" title="Use another folder as the scripts folder (new scripts go there)" on:click=move |_| pick_folder()>"Change…"</button>
                {move || (!ctx.script_folder.get().is_empty()).then(|| view! {
                    <button class="btn" title="Use the app's own scripts folder again" on:click=move |_| ctx.set_script_folder(String::new())>"Use the app's"</button>
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
            <button class="btn" on:click=move |_| add(false)>{ico(Icon::Plus)}"Add scripts…"</button>
            <button class="btn" on:click=move |_| add(true)>{ico(Icon::Folder)}"Add a folder…"</button>
            <button class="btn" title="Look for scripts again" on:click=move |_| ctx.load_scripts()>{ico(Icon::Refresh)}"Reload"</button>
            <button class="btn" title="What a script can do, with examples, in the editor"
                on:click=move |_| spawn_local(async move {
                    match call::<String>("script_guide", json!({})).await {
                        Ok(path) => edit(path),
                        Err(e) => error.set(Some(e)),
                    }
                })>{ico(Icon::Book)}"Scripting guide"</button>
        </div>
        <div class="row-buttons">
            <input class="script-name" placeholder="New script's name" prop:value=move || new_name.get()
                on:input=move |ev| new_name.set(event_target_value(&ev))
                on:keydown=move |ev| if ev.key() == "Enter" { make() }/>
            <button class="btn" disabled=move || new_name.with(|n| n.trim().is_empty())
                title="Make a script in the scripts folder and open it in the editor" on:click=move |_| make()>{ico(Icon::Plus)}"New script"</button>
        </div>
        {move || error.get().or_else(|| ctx.script_error.get()).map(|e| view! { <p class="error">{e}</p> })}
        <h3>"Scripts"</h3>
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
