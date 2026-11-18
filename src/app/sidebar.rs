//! The sidebar (places, devices, favourites) and the settings window.

use super::*;

/// Places on the Mac (and its disks) and on the connected devices. Clicking one opens it in the
/// active pane.
#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // A section heading that folds its section away; remembered across launches.
    let section = move |key: &'static str, title: &'static str| {
        let storage_key = format!("yomi-files.fold.{key}");
        let open = RwSignal::new(!stored_flag(&storage_key, false));
        let heading = view! {
            <h2><button class="fold" aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    let now = !open.get_untracked();
                    open.set(now);
                    store(&storage_key, if now { "0" } else { "1" });
                }>
                <span class="chev" class:open=move || open.get()>"▸"</span>{title}
            </button></h2>
        };
        (heading, open)
    };
    let (fav_heading, fav_open) = section("favs", "Favourites");
    let (mac_heading, mac_open) = section("mac", "Mac");
    let (dev_heading, dev_open) = section("devices", "Devices");
    // Disks mounted on the Mac: USB drives, SD cards, disk images.
    let drives = Memo::new(move |_| ctx.devices.with(|d| d.as_ref().map(|d| d.drives.clone()).unwrap_or_default()));
    let device = move |i: Info| {
        let src = Src::of(&i.id);
        // Its folders fold away under its name; remembered for the device across launches.
        let fold_key = format!("yomi-files.fold.dev.{}:{}", i.transport, i.label);
        let open = RwSignal::new(!stored_flag(&fold_key, false));
        let dirs = i.quick_dirs.clone();
        let foldable = !dirs.is_empty();
        let fold_tip = move || if open.get() { "Hide folders" } else { "Show folders" };
        view! {
            <div class="device">
                <button class="fold" class:none=!foldable disabled=!foldable title=fold_tip aria-label=fold_tip
                    aria-expanded=move || open.get().to_string()
                    on:click=move |_| {
                        let now = !open.get_untracked();
                        open.set(now);
                        store(&fold_key, if now { "0" } else { "1" });
                    }>
                    <span class="chev" class:open=move || open.get()>"▸"</span>
                </button>
                // The device's name shows all its storage ("/").
                <button class="name" title=format!("All storage on {}", i.label)
                    class:active=move || {
                        let pane = ctx.panes[ctx.active.get()];
                        pane.src.get() == src && pane.cwd.get() == "/"
                    }
                    on:click=move |_| ctx.open_place(src, "/".into())>
                    <b>{i.label.clone()}</b>
                    <span class="kind">{i.transport.clone()}</span>
                </button>
                {move || {
                    // The same phone, found the other way: offer to switch.
                    let serial = i.serial.clone()?;
                    let other = ctx.devices.with(|d| d.as_ref()?.found.iter().find(|f| f.serial.as_deref() == Some(serial.as_str())).cloned())?;
                    let to = if other.transport == "adb" { "adb" } else { "MTP" };
                    let title = format!("Connect with {to} instead");
                    Some(view! {
                        <button class="switch" title=title disabled=move || ctx.busy.get()
                            on:click=move |_| ctx.switch(src, other.clone())>{to}</button>
                    })
                }}
                <button class="eject" title=format!("Disconnect {}", i.label) aria-label="Disconnect"
                    on:click=move |_| ctx.disconnect(src)>"⏏"</button>
            </div>
            {move || open.get().then(|| view! {
                <div class="dev-dirs">
                    {dirs.clone().into_iter().map(|(name, path)| view! { <TreeNode src name path depth=1/> }).collect_view()}
                </div>
            })}
        }
    };
    view! {
        <nav class="side">
            {move || (!ctx.favs.with(|f| f.is_empty())).then(|| view! {
                {fav_heading.clone()}
                {move || fav_open.get().then(|| ctx.favs.get().into_iter().map(|f| {
                    let src = ctx.fav_src(&f);
                    let name = base(&f.path);
                    let on = if f.place == "mac" { "the Mac".to_string() } else { f.label.clone() };
                    let tip = match src {
                        Some(_) => format!("{} on {on}", f.path),
                        None => format!("{} on {on}. Connect {on} to open it.", f.path),
                    };
                    let fav_path = f.path.clone();
                    let unfav = ViewFn::from(move || {
                        let f = f.clone();
                        view! {
                            <button class="unfav" title="Remove from Favourites" aria-label="Remove from Favourites"
                                on:click=move |_| ctx.set_favs(ctx.favs.get_untracked().into_iter().filter(|x| *x != f).collect())>"✕"</button>
                        }
                    });
                    match src {
                        Some(src) => view! { <TreeNode src name path=fav_path class="fav" tip extra=unfav/> }.into_any(),
                        // Its device isn't connected: shown, but it can't be opened.
                        None => view! {
                            <div class="tree-row fav">
                                <span class="twisty none"></span>
                                <button class="item off" title=tip disabled=true>{name}</button>
                                {unfav.run()}
                            </div>
                        }.into_any(),
                    }
                }).collect_view())}
            })}
            {mac_heading}
            {move || mac_open.get().then(|| view! {
                {ctx.mac.get().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                    .map(|(name, path)| view! { <TreeNode src=Src::Mac name path/> }).collect_view()}
                {drives.get().into_iter().map(|(name, path)| {
                    let (tip, at) = (format!("Eject {name}"), path.clone());
                    let eject = ViewFn::from(move || {
                        let (tip, at) = (tip.clone(), at.clone());
                        view! { <button class="act" title=tip aria-label="Eject" on:click=move |_| ctx.eject(at.clone())>"⏏"</button> }
                    });
                    view! { <TreeNode src=Src::Mac name path class="drive" extra=eject/> }
                }).collect_view()}
            })}
            {dev_heading}
            {move || dev_open.get().then(|| view! {
                {ctx.phones.get().into_iter().map(device).collect_view()}
                <DeviceFinder/>
            })}
            <span class="grow"></span>
            <div class="side-buttons">
                <button class="btn" title="Files replaced or deleted by the app, which can be put back"
                    on:click=move |_| ctx.panel.set(Some(Panel::Backups))>"Backups"</button>
                <button class="btn" title="What was done since the app started"
                    on:click=move |_| ctx.panel.set(Some(Panel::History))>"History"</button>
            </div>
            <button class="btn" on:click=move |_| ctx.settings.set(true)>"Settings"</button>
        </nav>
    }
}

/// A folder in the sidebar that opens in the active pane, with ▸ to show the folders in it
/// (listed when it's first opened, and again each time it's reopened), and so on down.
#[component]
fn TreeNode(
    src: Src,
    name: String,
    path: String,
    #[prop(default = 0)] depth: usize,
    /// "fav" or "drive", for their marks.
    #[prop(default = "")]
    class: &'static str,
    #[prop(optional)] tip: Option<String>,
    /// Buttons at the end of the row (eject, remove).
    #[prop(optional)]
    extra: Option<ViewFn>,
) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let open = RwSignal::new(false);
    // The folders in it: None until listed. A listing that fails shows nothing under it.
    let kids = RwSignal::new(None::<Vec<(String, String)>>);
    let at = StoredValue::new(path.clone());
    let toggle = move |ev: web_sys::MouseEvent| {
        ev.stop_propagation();
        let now = !open.get_untracked();
        open.set(now);
        if !now {
            return;
        }
        spawn_local(async move {
            let args = json!({ "src": src.key(), "path": at.get_value(), "current": false });
            let found = match call::<Vec<Entry>>("list", args).await {
                Ok(es) => {
                    let mut dirs: Vec<(String, String)> = ctx.visible(es).into_iter().filter(|e| e.dir).map(|e| (e.name, e.path)).collect();
                    dirs.sort_by_key(|(n, _)| n.to_lowercase());
                    dirs
                }
                Err(_) => vec![],
            };
            let _ = kids.try_set(Some(found));
        });
    };
    let active = move || {
        let pane = ctx.panes[ctx.active.get()];
        pane.src.get() == src && at.with_value(|p| pane.cwd.with(|c| c == p))
    };
    // Nothing to show under it once listed and empty.
    let leaf = move || kids.with(|k| k.as_ref().is_some_and(Vec::is_empty));
    let fold_tip = move || if open.get() { "Hide folders" } else { "Show folders" };
    let title = tip.unwrap_or_else(|| path.clone());
    view! {
        <div class=format!("tree-row {class}") style=format!("padding-left: {}px", depth * 14)>
            <button class="twisty" class:none=leaf title=fold_tip aria-label=fold_tip
                aria-expanded=move || open.get().to_string() on:click=toggle>
                <span class="chev" class:open=move || open.get()>"▸"</span>
            </button>
            <button class="item" class:active=active title=title
                on:click=move |_| ctx.open_place(src, at.get_value())>{name}</button>
            {extra.map(|f| f.run())}
        </div>
        {move || open.get().then(|| kids.get().map(|k| k.into_iter().map(|(name, path)| {
            view! { <TreeNode src name path depth=depth + 1/> }.into_any()
        }).collect_view()))}
    }
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct Folder {
    path: String,
    files: u64,
    bytes: u64,
}

/// Settings, and the folders the app keeps on the Mac.
#[component]
pub(super) fn Settings() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let folders = RwSignal::new(None::<Result<HashMap<String, Folder>, String>>);
    let load = move || spawn_local(async move { folders.set(Some(call("app_folders", json!({})).await)) });
    load();
    let error = RwSignal::new(None::<String>);
    let open = move |which: &'static str| {
        spawn_local(async move {
            if let Err(e) = call::<()>("open_folder", json!({ "which": which })).await {
                error.set(Some(e));
            }
        })
    };
    let clear = move |which: &'static str| {
        spawn_local(async move {
            match call::<()>("clear_cache", json!({ "which": which })).await {
                Ok(()) => load(),
                Err(e) => error.set(Some(e)),
            }
        })
    };
    let folder = move |which: &'static str, title: &'static str, about: &'static str| {
        view! {
            <div class="folder">
                <div class="what">
                    <b>{title}</b>
                    <span class="about">{about}</span>
                    <span class="path">{move || match folders.get() {
                        None => "…".to_string(),
                        Some(Err(e)) => e,
                        Some(Ok(f)) => f.get(which).map(|f| format!("{} · {} files, {}", f.path, f.files, human_size(f.bytes))).unwrap_or_default(),
                    }}</span>
                </div>
                <button class="btn" on:click=move |_| open(which)>"Open"</button>
                {(which != "backups").then(|| view! { <button class="btn" on:click=move |_| clear(which)>"Clear"</button> })}
            </div>
        }
    };
    view! {
        <div class="overlay" on:click=move |_| ctx.settings.set(false)>
            <div class="dialog" on:click=|e| e.stop_propagation()>
                <h2>"Settings"</h2>
                <label class="row">
                    "Appearance"
                    <select prop:value=move || ctx.theme.get() on:change=move |e| {
                        let t = event_target_value(&e);
                        apply_theme(&t);
                        store(THEME_KEY, &t);
                        ctx.theme.set(t);
                    }>
                        <option value="auto">"As the Mac"</option>
                        <option value="light">"Light"</option>
                        <option value="dark">"Dark"</option>
                    </select>
                </label>
                <label class="check">
                    <input type="checkbox" prop:checked=move || ctx.show_hidden.get()
                        on:change=move |e| ctx.set_show_hidden(event_target_checked(&e))/>
                    "Show hidden files (names starting with a dot)"
                </label>
                <label class="check">
                    <input type="checkbox" prop:checked=move || ctx.phone_thumbs.get()
                        on:change=move |e| ctx.set_phone_thumbs(event_target_checked(&e))/>
                    "Thumbnails of phone files"
                </label>
                <p class="hint">"Each phone thumbnail reads the whole image or EPUB over USB, once; then it's cached on the Mac. Mac files always get thumbnails."</p>
                <label class="check">
                    <input type="checkbox" prop:checked=move || ctx.auto_connect.get()
                        on:change=move |e| ctx.set_auto_connect(event_target_checked(&e))/>
                    "Connect phones when they're plugged in (adb if allowed, else MTP)"
                </label>
                <h3>"On this Mac"</h3>
                {folder("backups", "Backups", "Copies of files from before they were replaced or deleted. Never removed by the app.")}
                {folder("thumbs", "Thumbnail cache", "Made again when needed, so it's safe to clear.")}
                {folder("opened", "Opened phone files", "Copies made to open phone files in Mac apps. Changes to them don't go back to the phone.")}
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="actions">
                    <button class="btn" on:click=move |_| { ctx.settings.set(false); ctx.help.set(true); }>"Keyboard shortcuts"</button>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| ctx.settings.set(false)>"Done"</button>
                </div>
            </div>
        </div>
    }
}

/// The keyboard shortcuts, from ? or Settings.
#[component]
pub(super) fn Shortcuts() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    const KEYS: &[(&str, &str)] = &[
        ("↑ ↓", "Move through the files"),
        ("← →", "Previous or next picture, video or sound (list); left and right (thumbnails, columns)"),
        ("↵ or ⌘↓", "Open"),
        ("⌫ or ⌘↑", "Up a folder"),
        ("Space", "Quick Look"),
        ("/ or ⌘F or ⌘K", "Filter; Enter searches subfolders"),
        ("⌘L", "Type a path"),
        ("F2", "Rename"),
        ("⌘⌫ or Delete", "Delete"),
        ("F5 / F6", "Copy or move to the other pane"),
        ("⌘A", "Pick everything"),
        ("⌘-click, Shift-click", "Pick some"),
        ("Esc", "Unpick, or close"),
        ("Tab", "The other pane"),
        ("⌘\\", "Show or hide the second pane"),
        ("⌘S", "Save, in the editor"),
        ("?", "This list"),
    ];
    view! {
        <div class="overlay" on:click=move |_| ctx.help.set(false)>
            <div class="dialog shortcuts" on:click=|e| e.stop_propagation()>
                <h2>"Keyboard shortcuts"</h2>
                <dl>
                    {KEYS.iter().map(|(k, what)| view! { <dt><kbd>{*k}</kbd></dt><dd>{*what}</dd> }).collect_view()}
                </dl>
                <div class="actions">
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| ctx.help.set(false)>"Done"</button>
                </div>
            </div>
        </div>
    }
}
