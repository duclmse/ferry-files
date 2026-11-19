//! The sidebar (places, devices, favourites) and the settings window.

use super::*;

/// Places on the Mac (and its disks) and on the connected devices. Clicking one opens it in the
/// active pane.
#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // A section heading that folds its section away; remembered across launches.
    // `refresh`: Devices has a button to look for devices again, at the right.
    let section = move |key: &'static str, title: &'static str, refresh: bool| {
        let storage_key = format!("ferry-files.fold.{key}");
        let open = RwSignal::new(!stored_flag(&storage_key, false));
        let heading = move || {
            let storage_key = storage_key.clone();
            view! {
            <h2>
            <button class="fold" aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    let now = !open.get_untracked();
                    open.set(now);
                    store(&storage_key, if now { "0" } else { "1" });
                }>
                {chev(open)}{title}
            </button>
            {refresh.then(|| view! {
                <button class="refresh" class:turning=move || ctx.looking.get()
                    title="Look for devices and disks again" aria-label="Look for devices again"
                    on:click=move |_| ctx.refresh_devices()>
                    <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.6"/><path d="M13 2.5v3h-3"/></svg>
                </button>
                <button class="refresh" title="Connect a phone over Wi-Fi (adb Wireless debugging)" aria-label="Connect over Wi-Fi"
                    on:click=move |_| ctx.wifi.set(true)>{ico(Icon::Wifi)}</button>
            })}</h2>
            }
        };
        (heading, open)
    };
    let (fav_heading, fav_open) = section("favs", "Favourites", false);
    let (pairs_heading, pairs_open) = section("pairs", "Sync pairs", false);
    let (ws_heading, ws_open) = section("workspaces", "Workspaces", false);
    // Naming a workspace to save: None when not asked.
    let ws_name = RwSignal::new(None::<String>);
    let (mac_heading, mac_open) = section("mac", crate::os::computer(), false);
    let (dev_heading, dev_open) = section("devices", "Devices", true);
    // Disks mounted on the Mac: USB drives, SD cards, disk images.
    let drives = Memo::new(move |_| ctx.devices.with(|d| d.as_ref().map(|d| d.drives.clone()).unwrap_or_default()));
    let device = move |i: Info| {
        let src = Src::of(&i.id);
        // Its folders fold away under its name; remembered for the device across launches.
        let fold_key = format!("ferry-files.fold.dev.{}:{}", i.transport, i.label);
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
                    {chev(open)}
                </button>
                // The device's name shows all its storage ("/").
                <button class="name" title=format!("All storage on {}", i.label)
                    class:active=move || {
                        let pane = ctx.panes[ctx.active.get()];
                        pane.src.get() == src && pane.cwd.get() == "/"
                    }
                    on:click=move |_| ctx.open_place(src, "/".into())
                    on:auxclick=move |ev| if ev.button() == 1 { ev.prevent_default(); ctx.new_tab(ctx.act(), src, "/".into()) }
                    on:contextmenu=move |ev| ctx.show_place_menu(&ev, src, "/".into())>
                    <b>{i.label.clone()}</b>
                </button>
                {
                    // The badge says how it's connected; when the same phone is also found the
                    // other way, clicking it reconnects that way.
                    let wireless = i.wireless();
                    let now = if wireless { "Wi-Fi" } else if i.transport == "adb" { "adb" } else { "MTP" };
                    let label = i.label.clone();
                    let serial = i.serial.clone();
                    let other = Memo::new(move |_| {
                        let serial = serial.clone()?;
                        ctx.devices.with(|d| d.as_ref()?.found.iter().find(|f| f.serial.as_deref() == Some(serial.as_str())).cloned())
                    });
                    let to = move || other.with(|o| o.as_ref().map(|o| if o.transport == "adb" { "adb" } else { "MTP" }));
                    let title = move || match to() {
                        Some(to) => format!("Connected with {now}. Click to connect with {to} instead."),
                        None if now == "MTP" => format!("Connected with MTP. To use adb, turn on USB debugging on {label} and allow {}.", crate::os::this_computer()),
                        None if wireless => "Connected with adb over Wi-Fi.".to_string(),
                        None => "Connected with adb.".to_string(),
                    };
                    view! {
                        <button class="kind" class:can=move || to().is_some() title=title
                            disabled=move || to().is_none() || ctx.busy.get()
                            on:click=move |_| if let Some(o) = other.get_untracked() { ctx.switch(src, o) }>
                            {now}
                            {move || to().map(|_| view! { <span class="swap" aria-hidden="true">"⇄"</span> })}
                        </button>
                    }
                }
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
                {fav_heading()}
                {move || fav_open.get().then(|| ctx.favs.get().into_iter().map(|f| {
                    let src = ctx.fav_src(&f);
                    let name = base(&f.path);
                    let on = if f.place == "mac" { crate::os::the_computer().to_string() } else { f.label.clone() };
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
                        Some(src) => view! { <TreeNode src name path=fav_path depth=1 class="fav" tip extra=unfav/> }.into_any(),
                        // Its device isn't connected: shown, but it can't be opened.
                        None => view! {
                            <div class="tree-row fav" style="--depth: 1">
                                <span class="twisty none"></span>
                                <button class="item off" title=tip disabled=true>{name}</button>
                                {unfav.run()}
                            </div>
                        }.into_any(),
                    }
                }).collect_view())}
            })}
            {move || (!ctx.sync_pairs.with(|x| x.is_empty())).then(|| view! {
                {pairs_heading()}
                {move || pairs_open.get().then(|| ctx.sync_pairs.get().into_iter().map(|pair| {
                    let how = SYNC_MODES.iter().find(|m| m.0 == pair.mode).map_or("Copy missing", |m| m.1);
                    let tip = format!("{how}: {} on {} into {} on {}. Click to see what it would do.",
                        pair.from_dir, ctx.side_name(&pair.from), pair.to_dir, ctx.side_name(&pair.to));
                    let name = format!("{} → {}", base(&pair.from_dir), base(&pair.to_dir));
                    let (p1, p2) = (pair.clone(), pair);
                    view! {
                        <div class="tree-row fav pair" style="--depth: 1">
                            <span class="twisty none"></span>
                            <button class="item" title=tip on:click=move |_| ctx.open_pair(&p1)>{name}</button>
                            <button class="unfav" title="Remove this sync pair" aria-label="Remove this sync pair"
                                on:click=move |_| ctx.set_pairs(ctx.sync_pairs.get_untracked().into_iter().filter(|x| *x != p2).collect())>"✕"</button>
                        </div>
                    }
                }).collect_view())}
            })}
            {ws_heading()}
            {move || ws_open.get().then(|| view! {
                {move || ctx.workspaces.get().into_iter().map(|w| {
                    let tip = ctx.workspace_tip(&w);
                    let (w1, name) = (w.clone(), w.name.clone());
                    view! {
                        <div class="tree-row fav" style="--depth: 1">
                            <span class="twisty none"></span>
                            <button class="item" title=tip on:click=move |_| ctx.open_workspace(w1.clone())>{w.name.clone()}</button>
                            <button class="unfav" title="Delete this workspace" aria-label="Delete this workspace"
                                on:click=move |_| ctx.set_workspaces(ctx.workspaces.get_untracked().into_iter().filter(|x| x.name != name).collect())>"✕"</button>
                        </div>
                    }
                }).collect_view()}
                {move || match ws_name.get() {
                    None => view! {
                        <div class="tree-row" style="--depth: 1">
                            <span class="twisty none"></span>
                            <button class="item add" title="Keep these tabs, the split and the widths under a name"
                                on:click=move |_| ws_name.set(Some(String::new()))>"Save these tabs…"</button>
                        </div>
                    }.into_any(),
                    Some(_) => {
                        let save = move || {
                            if let Some(n) = ws_name.get_untracked() {
                                ctx.save_workspace(&n);
                            }
                            ws_name.set(None);
                        };
                        let input = NodeRef::<html::Input>::new();
                        input.on_load(move |el| { request_animation_frame(move || { let _ = el.focus(); }); });
                        view! {
                            <div class="ws-save">
                                <input node_ref=input type="text" placeholder="Name, e.g. Phone photos" aria-label="Workspace name"
                                    prop:value=move || ws_name.get().unwrap_or_default()
                                    on:input=move |e| ws_name.set(Some(event_target_value(&e)))
                                    on:keydown=move |e| match e.key().as_str() {
                                        "Enter" => save(),
                                        "Escape" => { e.stop_propagation(); ws_name.set(None) }
                                        _ => {}
                                    }/>
                                <button class="btn small" disabled=move || ws_name.with(|n| n.as_deref().is_none_or(|n| n.trim().is_empty()))
                                    on:click=move |_| save()>"Save"</button>
                            </div>
                        }.into_any()
                    }
                }}
            })}
            {mac_heading()}
            {move || mac_open.get().then(|| view! {
                {ctx.mac.get().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                    .map(|(name, path)| view! { <TreeNode src=Src::Mac name path depth=1/> }).collect_view()}
                {drives.get().into_iter().map(|(name, path)| {
                    let (tip, at) = (format!("Eject {name}"), path.clone());
                    let eject = ViewFn::from(move || {
                        let (tip, at) = (tip.clone(), at.clone());
                        view! { <button class="act" title=tip aria-label="Eject" on:click=move |_| ctx.eject(at.clone())>"⏏"</button> }
                    });
                    view! { <TreeNode src=Src::Mac name path depth=1 class="drive" extra=eject/> }
                }).collect_view()}
            })}
            {dev_heading()}
            {move || dev_open.get().then(|| view! {
                {ctx.phones.get().into_iter().map(device).collect_view()}
                <DeviceFinder/>
            })}
            <span class="grow"></span>
            <div class="side-buttons">
                <button class="btn" title="Files replaced or deleted by the app, which can be put back"
                    on:click=move |_| ctx.panel.set(Some(Panel::Backups))>{ico(Icon::Backups)}"Backups"</button>
                <button class="btn" title="What was done since the app started"
                    on:click=move |_| ctx.panel.set(Some(Panel::History))>{ico(Icon::History)}"History"</button>
            </div>
            <button class="btn" on:click=move |_| ctx.settings.set(true)>{ico(Icon::Settings)}"Settings"</button>
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
        <div class=format!("tree-row {class}") style=format!("--depth: {depth}")>
            <button class="twisty" class:none=leaf title=fold_tip aria-label=fold_tip
                aria-expanded=move || open.get().to_string() on:click=toggle>
                {chev(open)}
            </button>
            // A middle click opens it in a new tab; right-click for a menu.
            <button class="item" class:active=active title=title
                on:click=move |_| ctx.open_place(src, at.get_value())
                on:auxclick=move |ev| if ev.button() == 1 { ev.prevent_default(); ctx.new_tab(ctx.act(), src, at.get_value()) }
                on:contextmenu=move |ev| ctx.show_place_menu(&ev, src, at.get_value())>{name}</button>
            {extra.map(|f| f.run())}
        </div>
        {move || open.get().then(|| kids.get().map(|k| k.into_iter().map(|(name, path)| {
            view! { <TreeNode src name path depth=depth + 1/> }.into_any()
        }).collect_view()))}
    }
}

/// The arrow that folds or unfolds a row, pointing down when open.
fn chev(open: RwSignal<bool>) -> impl IntoView {
    view! {
        <svg class="chev" class:open=move || open.get() viewBox="0 0 16 16" aria-hidden="true">
            <path d="M6 3.5 10.5 8 6 12.5"/>
        </svg>
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
    let restore = RwSignal::new(stored_flag(RESTORE_KEY, true));
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
                <button class="btn" on:click=move |_| open(which)>{ico(Icon::Folder)}"Open"</button>
                {(which != "backups").then(|| view! { <button class="btn" on:click=move |_| clear(which)>{ico(Icon::Trash)}"Clear"</button> })}
            </div>
        }
    };
    let check = move |on: Signal<bool>, set: fn(Ctx, bool), text: &'static str| view! {
        <label class="check">
            <input type="checkbox" prop:checked=move || on.get() on:change=move |e| set(ctx, event_target_checked(&e))/>
            {text}
        </label>
    };
    let general = move || view! {
        <label class="row">
            "Appearance"
            <select prop:value=move || ctx.theme.get() on:change=move |e| {
                let t = event_target_value(&e);
                apply_theme(&t);
                store(THEME_KEY, &t);
                ctx.theme.set(t);
            }>
                <option value="auto">{if crate::os::is_mac() { "As the Mac" } else { "As the system" }}</option>
                <option value="light">"Light"</option>
                <option value="dark">"Dark"</option>
            </select>
        </label>
        <label class="check">
            <input type="checkbox" prop:checked=restore.get_untracked()
                on:change=move |e| { let on = event_target_checked(&e); restore.set(on); store(RESTORE_KEY, if on { "1" } else { "0" }); }/>
            "Bring back the tabs at launch"
        </label>
        <p class="hint">"Opens the tabs, split and widths the app had when it quit. Tabs on a phone come back when it's connected."</p>
    }.into_any();
    let files = move || view! {
        {check(ctx.show_hidden.into(), Ctx::set_show_hidden, "Show hidden files (names starting with a dot)")}
        {check(ctx.verify.into(), Ctx::set_verify, "Check copies")}
        <p class="hint">"Reads each copied file back and compares it with the original; a copy that doesn't match is removed and reported, and a move keeps its original. Copies to a phone over MTP take about twice as long. (adb copies are always checked.)"</p>
    }.into_any();
    let phones = move || view! {
        {check(ctx.auto_connect.into(), Ctx::set_auto_connect, "Connect phones when they're plugged in (adb if allowed, else MTP)")}
        {check(ctx.phone_thumbs.into(), Ctx::set_phone_thumbs, "Thumbnails of phone files")}
        <p class="hint">{format!("Each phone thumbnail reads the whole image or EPUB over USB, once; then it's cached on {}. Files on {} always get thumbnails.", crate::os::the_computer(), crate::os::the_computer())}</p>
    }.into_any();
    let storage = move || view! {
        {folder("backups", "Backups", "Copies of files from before they were replaced or deleted. Kept until you clear them in Backups, which moves them to the Trash.")}
        {folder("thumbs", "Thumbnail cache", "Made again when needed, so it's safe to clear.")}
        {folder("opened", "Opened phone files", "Copies made to open phone files in other apps. Changes to them don't go back to the phone.")}
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
    }.into_any();
    let storage_name = format!("On {}", crate::os::this_computer());
    let sections: [(&'static str, String); 5] = [
        ("general", "General".into()),
        ("files", "Files".into()),
        ("phones", "Phones".into()),
        ("keys", "Keyboard".into()),
        ("storage", storage_name),
    ];
    let names = StoredValue::new(sections.clone());
    view! {
        <div class="overlay" on:click=move |_| ctx.settings.set(false)>
            <div class="dialog settings" role="dialog" aria-label="Settings" on:click=|e| e.stop_propagation()>
                <h2>"Settings"</h2>
                <div class="settings-main">
                    <nav class="settings-tabs" role="tablist" aria-orientation="vertical">
                        {sections.into_iter().map(|(id, name)| view! {
                            <button role="tab" class:on=move || ctx.settings_tab.get() == id
                                aria-selected=move || (ctx.settings_tab.get() == id).to_string()
                                on:click=move |_| ctx.settings_tab.set(id)>{name}</button>
                        }).collect_view()}
                    </nav>
                    <section class="settings-body" role="tabpanel"
                        aria-label=move || names.with_value(|n| n.iter().find(|s| s.0 == ctx.settings_tab.get()).map(|s| s.1.clone()))>
                        {move || match ctx.settings_tab.get() {
                            "files" => files(),
                            "phones" => phones(),
                            "keys" => view! { <ShortcutsEditor/> }.into_any(),
                            "storage" => storage(),
                            _ => general(),
                        }}
                    </section>
                </div>
                <div class="actions">
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| ctx.settings.set(false)>"Done"</button>
                </div>
            </div>
        </div>
    }
}
