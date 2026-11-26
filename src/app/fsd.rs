//! Settings > Search: fsd, the fs project's search service, installed as a service so it starts
//! by itself and keeps its index of file names up to date. The backend (`fsd_service.rs`) finds
//! the program, installs and removes the service, and asks fsd how far its index is.

use super::*;
use crate::api::{FsdSaved, FsdSettings, FsdShown, FsdStatus};
use crate::os::Os;

/// How often the status is asked for again while fsd is starting or indexing.
const EVERY: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Clone, Copy)]
struct Service {
    status: RwSignal<Option<Result<FsdStatus, String>>>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    /// Whether a check is due already, so there's only ever one.
    due: StoredValue<bool>,
}

impl Service {
    fn load(self) {
        spawn_local(async move { self.show(call("fsd_status", json!({})).await) });
    }

    fn show(self, got: Result<FsdStatus, String>) {
        let again = got.as_ref().is_ok_and(FsdStatus::changing);
        // Given back when the settings have closed.
        if self.status.try_set(Some(got)).is_some() {
            return;
        }
        if again && !self.due.get_value() {
            self.due.set_value(true);
            set_timeout(
                move || {
                    if self.due.try_set_value(false).is_none() {
                        self.load();
                    }
                },
                EVERY,
            );
        }
    }

    /// Installs or removes the service.
    fn act(self, cmd: &'static str) {
        self.busy.set(true);
        self.error.set(None);
        spawn_local(async move {
            match call::<FsdStatus>(cmd, json!({})).await {
                Ok(s) => self.show(Ok(s)),
                Err(e) => {
                    let _ = self.error.try_set(Some(e));
                    self.load();
                }
            }
            let _ = self.busy.try_set(false);
        });
    }
}

/// Where fsd can be put for the app to find it.
fn where_to_put(os: Os) -> String {
    let put = match os {
        Os::Mac => "Or put the fsd program in /opt/homebrew/bin, /usr/local/bin or ~/.cargo/bin (from the fs project: cargo install --path fs/crates/fs-server), then check again.",
        Os::Windows => "Or put fsd.exe in a folder on the PATH or in %USERPROFILE%\\.cargo\\bin (from the fs project: cargo install --path fs/crates/fs-server), then check again.",
        Os::Linux => "Or put the fsd program in ~/.local/bin, ~/.cargo/bin or /usr/local/bin (from the fs project: cargo install --path fs/crates/fs-server), then check again.",
    };
    format!("This copy of Ferry Files came without fsd: the app bundle carries it, and `cargo tauri dev` builds it. {put}")
}

/// What fsd has, in a sentence.
fn index_line(s: &FsdStatus) -> String {
    let what = if s.roots.is_empty() {
        String::new()
    } else {
        format!(", in {}", s.roots.join(", "))
    };
    if s.state == "indexing" {
        format!("{} files and folders so far{what}.", s.entries)
    } else {
        format!("{} files and folders{what}.", s.entries)
    }
}

/// The status's title and its explanation.
fn describe(s: &FsdStatus, os: Os) -> (String, String) {
    let problem = s
        .problem
        .as_ref()
        .map(|p| format!(" {p}"))
        .unwrap_or_default();
    let (title, about) = match (&s.service, s.ours, s.state.as_str()) {
        (_, _, "unknown") => (
            "This build of the app has no fsd searches".into(),
            "Build it with the fsd feature (cargo tauri dev does) to search with fsd.".into(),
        ),
        (Some(place), false, _) => (
            "fsd is set up as a service with its own settings".into(),
            format!(
                "{place}. Ferry Files leaves it as it is, and searches with it when it's running. {}",
                if s.running() { index_line(s) } else { "It isn't answering now.".into() }
            ),
        ),
        (None, _, "indexing" | "ready") => (
            "fsd is running, started outside Ferry Files".into(),
            format!(
                "{} Stop it to install it as a service here.",
                index_line(s)
            ),
        ),
        (None, _, _) if s.exe.is_none() => (
            format!("fsd isn't on {}", crate::os::the_computer_on(os)),
            where_to_put(os),
        ),
        (None, _, _) => (
            "fsd isn't installed as a service".into(),
            if os == Os::Windows {
                "Install it to run it for this PC from startup. Windows asks for an administrator's permission.".into()
            } else {
                "Install it to start it now and each time you sign in.".into()
            },
        ),
        (Some(_), true, "ready") => ("fsd is running".into(), index_line(s)),
        (Some(_), true, "indexing") => ("fsd is indexing".into(), index_line(s)),
        (Some(_), true, _) => (
            "fsd is installed, but isn't answering".into(),
            "It may still be starting. If this stays, look at its logs in its folder.".into(),
        ),
    };
    (title, format!("{about}{problem}"))
}

/// What the service means on this system, under the status.
fn os_hint(s: &FsdStatus, os: Os) -> Option<&'static str> {
    if !s.ours {
        return None;
    }
    Some(match os {
        // Not once fsd has shown it can look in them.
        Os::Mac if s.sees_protected == Some(true) => return None,
        Os::Mac => "macOS keeps background programs out of Desktop, Documents and Downloads. To search them with fsd, add the program above under System Settings > Privacy & Security > Full Disk Access.",
        Os::Windows => "It runs for the whole PC, so other people who sign in to it can search the names of the files it indexes.",
        Os::Linux => "It runs while you're signed in (a systemd user service).",
    })
}

/// Whether Ferry's settings for fsd are the ones it runs with, or would be once installed: not
/// when another fsd service has its own.
fn settings_apply(s: &FsdStatus) -> bool {
    s.ours || s.service.is_none()
}

/// The patterns as the box shows them, one a line.
fn lines(skip: &[String]) -> String {
    skip.join("\n")
}

/// The usual patterns the box doesn't have, in their order.
fn missing(text: &str, usual: &[String]) -> Vec<String> {
    usual
        .iter()
        .filter(|u| !text.lines().any(|l| l.trim() == u.as_str()))
        .cloned()
        .collect()
}

/// fsd's folders and what it leaves out, from Ferry's settings file for it. Edits stay here
/// until Save, while the status above changes.
#[component]
fn FsdForm(svc: Service) -> impl IntoView {
    let folders = RwSignal::new(Vec::<String>::new());
    let skip = RwSignal::new(String::new());
    let hidden = RwSignal::new(false);
    let loaded = RwSignal::new(None::<Result<FsdSettings, String>>);
    let saving = RwSignal::new(false);
    let note = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let usual = RwSignal::new(Vec::<String>::new());
    // try_set: an answer can come after the settings have closed.
    let show = move |s: FsdSettings| {
        let _ = folders.try_set(s.folders.clone());
        let _ = skip.try_set(lines(&s.skip));
        let _ = hidden.try_set(s.hidden);
        let _ = loaded.try_set(Some(Ok(s)));
    };
    let load = move || {
        spawn_local(async move {
            match call::<FsdShown>("fsd_settings", json!({})).await {
                Ok(s) => {
                    let _ = usual.try_set(s.usual);
                    show(s.settings);
                }
                Err(e) => {
                    let _ = loaded.try_set(Some(Err(e)));
                }
            }
        })
    };
    load();
    let now = move || FsdSettings {
        folders: folders.get(),
        skip: skip.with(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        }),
        hidden: hidden.get(),
    };
    let changed = Memo::new(move |_| match loaded.get() {
        Some(Ok(was)) => now() != was,
        _ => false,
    });
    let add = move || {
        spawn_local(async move {
            match call::<Vec<String>>("pick_scripts", json!({ "folder": true })).await {
                Ok(got) => folders.update(|f| {
                    f.extend(
                        got.into_iter()
                            .filter(|g| !f.contains(g))
                            .collect::<Vec<_>>(),
                    )
                }),
                Err(e) => error.set(Some(e)),
            }
        })
    };
    let save = move || {
        let settings = now();
        saving.set(true);
        note.set(None);
        error.set(None);
        spawn_local(async move {
            match call::<FsdSaved>("fsd_set_settings", json!({ "settings": settings })).await {
                Ok(saved) => {
                    if note.try_set(Some(saved.note)).is_none() {
                        show(saved.settings);
                        svc.load();
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            let _ = saving.try_set(false);
        })
    };
    let to_add = Memo::new(move |_| skip.with(|t| usual.with(|u| missing(t, u))));
    let add_usual = move || {
        let more = to_add.get_untracked();
        skip.update(|t| {
            let mut all: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
            all.extend(more.iter().map(String::as_str));
            *t = all.join("\n");
        });
    };
    let shown = |path: &str| crate::os::native_path(path);
    view! {
        <div class="section-head">
            <h3>"What fsd indexes"</h3>
            <button class="btn icon-only" title="Add a folder for fsd to index…" aria-label="Add a folder"
                disabled=move || !matches!(loaded.get(), Some(Ok(_))) on:click=move |_| add()>{ico(Icon::FolderPlus)}</button>
        </div>
        {move || match loaded.get() {
            None => view! { <p class="hint">"Reading fsd's settings…"</p> }.into_any(),
            Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
            Some(Ok(_)) => view! {
                <div class="script-places">
                    {move || folders.get().into_iter().map(|f| {
                        let gone = f.clone();
                        view! {
                            <div class="script-place">
                                <span class="path" title=shown(&f)>{shown(&f)}</span>
                                <button class="btn icon-only" title="Stop indexing this folder" aria-label="Stop indexing this folder"
                                    on:click=move |_| folders.update(|all| all.retain(|p| *p != gone))>{ico(Icon::Close)}</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
                <label class="check">
                    <input type="checkbox" prop:checked=move || hidden.get()
                        on:change=move |e| hidden.set(event_target_checked(&e))/>
                    "Leave out hidden files and folders"
                </label>
                <label class="stack" for="fsd-skip">"Leave out paths that match"</label>
                <textarea id="fsd-skip" class="patterns" rows="10" spellcheck="false" autocomplete="off"
                    prop:value=move || skip.get() on:input=move |e| skip.set(event_target_value(&e))></textarea>
                <p class="hint flush">"One pattern a line. ** stands for any folders and * for any part of a name, so **/node_modules/** leaves out every node_modules folder."</p>
                <div class="row-buttons">
                    {move || {
                        let n = to_add.get().len();
                        let title = if n == 0 {
                            "The usual build, cache and version control folders are all left out".to_string()
                        } else {
                            format!("Leave out the usual build, cache and version control folders too ({n} more patterns)")
                        };
                        view! {
                            <button class="btn icon-only" title=title.clone() aria-label=title
                                disabled=n == 0 on:click=move |_| add_usual()>{ico(Icon::Plus)}</button>
                        }
                    }}
                    <button class="btn icon-only" title="Save, and have fsd follow these settings" aria-label="Save"
                        disabled=move || !changed.get() || saving.get() on:click=move |_| save()>{ico(Icon::Save)}</button>
                    <button class="btn icon-only" title="Put back the saved settings" aria-label="Put back the saved settings"
                        disabled=move || !changed.get() || saving.get() on:click=move |_| {
                            note.set(None);
                            error.set(None);
                            load();
                        }>{ico(Icon::Restore)}</button>
                </div>
                <p class="hint flush">"Other settings stay as they are in the settings file."</p>
            }.into_any(),
        }}
        {move || saving.get().then(|| view! { <p class="hint">"Saving…"</p> })}
        {move || note.get().map(|n| view! { <p class="hint">{n}</p> })}
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
    }
}

#[component]
pub(super) fn SearchSettings() -> impl IntoView {
    let svc = Service {
        status: RwSignal::new(None),
        busy: RwSignal::new(false),
        error: RwSignal::new(None),
        due: StoredValue::new(false),
    };
    svc.load();
    // Only changes when it must, so the form isn't made again as the status changes.
    let editable = Memo::new(move |_| {
        svc.status
            .with(|s| matches!(s, Some(Ok(s)) if settings_apply(s)))
    });
    let open = move || {
        spawn_local(async move {
            if let Err(e) = call::<()>("open_folder", json!({ "which": "fsd" })).await {
                let _ = svc.error.try_set(Some(e));
            }
        })
    };
    let shown = move |s: FsdStatus| {
        let (title, about) = describe(&s, crate::os::os());
        let installed = s.service.is_some();
        let can_install = s.exe.is_some() && (s.ours || (!installed && !s.running()));
        let can_remove = installed && s.ours;
        let (install_title, install_label) = if s.ours {
            (
                "Install again: restarts fsd with its settings",
                "Install again",
            )
        } else {
            ("Install fsd as a service and start it", "Install")
        };
        let manager = format!(
            "Open fsd's settings, index and logs in {}",
            crate::os::file_manager()
        );
        view! {
            <div class="folder">
                <div class="what">
                    <b>{title}</b>
                    <span class="about">{about}</span>
                    {s.exe.clone().map(|e| view! { <span class="path">{format!("Program: {e}")}</span> })}
                    {s.ours.then(|| view! { <span class="path">{format!("Settings: {}", s.config)}</span> })}
                </div>
                <button class="btn icon-only" title="Check again" aria-label="Check again"
                    disabled=move || svc.busy.get() on:click=move |_| svc.load()>{ico(Icon::Refresh)}</button>
                {can_install.then(|| view! {
                    <button class="btn icon-only" title=install_title aria-label=install_label
                        disabled=move || svc.busy.get() on:click=move |_| svc.act("fsd_install")>{ico(Icon::Plus)}</button>
                })}
                {s.ours.then(|| view! {
                    <button class="btn icon-only" title=manager.clone() aria-label=manager on:click=move |_| open()>{ico(Icon::Folder)}</button>
                })}
                {can_remove.then(|| view! {
                    <button class="btn icon-only" title="Remove the service: stops fsd. Its settings and index stay in its folder."
                        aria-label="Remove the service"
                        disabled=move || svc.busy.get() on:click=move |_| svc.act("fsd_uninstall")>{ico(Icon::Trash)}</button>
                })}
            </div>
            {os_hint(&s, crate::os::os()).map(|h| view! { <p class="hint">{h}</p> })}
        }
        .into_any()
    };
    view! {
        <p class="hint flush">{format!(
            "Searches in {}'s folders, and the palette's :, are much faster with fsd, the fs project's search service. It keeps an index of file names, and Ferry Files asks it whenever it's running.",
            crate::os::the_computer()
        )}</p>
        {move || match svc.status.get() {
            None => view! { <p class="hint">"Checking…"</p> }.into_any(),
            Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
            Some(Ok(s)) => shown(s),
        }}
        {move || svc.busy.get().then(|| view! { <p class="hint">"Working…"</p> })}
        {move || svc.error.get().map(|e| view! { <p class="error">{e}</p> })}
        {move || editable.get().then(|| view! { <FsdForm svc=svc/> })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(service: Option<&str>, ours: bool, state: &str) -> FsdStatus {
        FsdStatus {
            exe: Some("/usr/local/bin/fsd".into()),
            service: service.map(Into::into),
            ours,
            config: "/c/config.toml".into(),
            state: state.into(),
            roots: vec!["/Users/me".into()],
            entries: 12,
            sees_protected: None,
            problem: None,
        }
    }

    #[test]
    fn says_where_fsd_is() {
        assert_eq!(
            describe(&status(Some("x"), true, "ready"), Os::Mac).0,
            "fsd is running"
        );
        assert_eq!(
            describe(&status(Some("x"), true, "indexing"), Os::Mac).1,
            "12 files and folders so far, in /Users/me."
        );
        assert!(describe(&status(Some("x"), false, "ready"), Os::Mac)
            .0
            .contains("its own settings"));
        assert!(describe(&status(None, false, "ready"), Os::Mac)
            .0
            .contains("outside"));
        assert!(describe(&status(None, false, "off"), Os::Mac)
            .0
            .contains("isn't installed"));
        let mut gone = status(None, false, "off");
        gone.exe = None;
        assert!(describe(&gone, Os::Mac).0.starts_with("fsd isn't on"));
    }

    #[test]
    fn settings_show_unless_another_service_has_its_own() {
        assert!(settings_apply(&status(Some("x"), true, "ready")));
        assert!(settings_apply(&status(None, false, "off")));
        assert!(!settings_apply(&status(Some("x"), false, "ready")));
        assert_eq!(lines(&["a".into(), "b".into()]), "a\nb");
        let usual = ["**/.git/**".to_string(), "**/build/**".to_string()];
        assert_eq!(missing("  **/.git/**\nx", &usual), ["**/build/**"]);
        assert!(missing("**/build/**\n**/.git/**", &usual).is_empty());
    }

    #[test]
    fn the_full_disk_access_hint_goes_once_fsd_can_look() {
        let mut s = status(Some("x"), true, "ready");
        assert!(os_hint(&s, Os::Mac).is_some());
        s.sees_protected = Some(false);
        assert!(os_hint(&s, Os::Mac).unwrap().contains("Full Disk Access"));
        s.sees_protected = Some(true);
        assert!(os_hint(&s, Os::Mac).is_none());
        assert!(os_hint(&s, Os::Linux).is_some());
    }

    #[test]
    fn asks_again_only_while_changing() {
        assert!(status(Some("x"), true, "indexing").changing());
        assert!(status(Some("x"), true, "off").changing());
        assert!(!status(Some("x"), true, "ready").changing());
        assert!(!status(None, false, "off").changing());
    }
}
