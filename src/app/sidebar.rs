//! The sidebar (places, devices, favourites) and the settings window.

use super::*;

/// Places on the Mac (and its disks) and on the connected devices. Clicking one opens it in the
/// active pane.
#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let place = move |src: Src, name: String, path: String| {
        let p = path.clone();
        let active = move || {
            let pane = ctx.panes[ctx.active.get()];
            pane.src.get() == src && pane.cwd.get() == p
        };
        view! {
            <button class="item" class:active=active title=path.clone()
                on:click=move |_| ctx.open_place(src, path.clone())>{name}</button>
        }
    };
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
                    {dirs.clone().into_iter().map(|(name, path)| place(src, name, path)).collect_view()}
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
                    let (f1, f2) = (f.clone(), f.clone());
                    let active = move || src.is_some_and(|s| {
                        let pane = ctx.panes[ctx.active.get()];
                        pane.src.get() == s && pane.cwd.get() == f1.path
                    });
                    view! {
                        <div class="fav">
                            <button class="item" class:active=active class:off=src.is_none() title=tip
                                on:click=move |_| if let Some(s) = src { ctx.open_place(s, f2.path.clone()) }>{name}</button>
                            <button class="unfav" title="Remove from Favourites" aria-label="Remove from Favourites"
                                on:click=move |_| ctx.set_favs(ctx.favs.get_untracked().into_iter().filter(|x| *x != f).collect())>"✕"</button>
                        </div>
                    }
                }).collect_view())}
            })}
            {mac_heading}
            {move || mac_open.get().then(|| view! {
                {ctx.mac.get().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                    .map(|(name, path)| place(Src::Mac, name, path)).collect_view()}
                {drives.get().into_iter().map(|(name, path)| {
                    let (tip, at) = (format!("Eject {name}"), path.clone());
                    view! {
                        <div class="line drive">
                            {place(Src::Mac, name, path)}
                            <button class="act" title=tip aria-label="Eject" on:click=move |_| ctx.eject(at.clone())>"⏏"</button>
                        </div>
                    }
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
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| ctx.settings.set(false)>"Done"</button>
                </div>
            </div>
        </div>
    }
}
