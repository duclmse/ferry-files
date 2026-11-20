//! The sidebar (workspaces, favourites, places, devices) and the settings window.

use super::*;

/// The sidebar's sections, top to bottom: (key, title). The computer's title is its name.
pub(super) const SECTIONS: [(&str, &str); 6] = [
    ("workspaces", "Workspaces"),
    ("favs", "Favourites"),
    ("mac", ""),
    ("pairs", "Sync pairs"),
    ("devices", "Devices"),
    ("servers", "Servers"),
];

/// The sections in the order chosen (`stored`, keys of `SECTIONS`), then any it doesn't name.
pub(super) fn section_order(stored: &[String]) -> Vec<&'static str> {
    let mut order: Vec<&'static str> = stored.iter().filter_map(|k| SECTIONS.iter().find(|s| s.0 == k).map(|s| s.0)).collect();
    order.dedup();
    order.extend(SECTIONS.iter().map(|s| s.0).filter(|k| !stored.iter().any(|s| s == k)));
    order
}

/// `order` with section `from` moved before (or `after`) section `to`.
pub(super) fn moved_section(order: &[&'static str], from: &str, to: &str, after: bool) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = order.to_vec();
    if from == to {
        return v;
    }
    let Some(at) = v.iter().position(|k| *k == from) else { return v };
    let key = v.remove(at);
    let Some(t) = v.iter().position(|k| *k == to) else { return order.to_vec() };
    v.insert(t + usize::from(after), key);
    v
}

/// Dragging sections to put them in another order, in the sidebar and in Settings > Sidebar.
/// Uses the pointer rather than HTML drag and drop, which Windows doesn't give the page while
/// the app takes files dropped from the file manager. Elements with `data-section` are the
/// places to drop; a press on a `[data-grip]` element starts a drag once it moves a few pixels.
#[derive(Clone, Copy)]
pub(super) struct Reorder {
    /// The section pressed on, and where (x, y).
    pressed: StoredValue<Option<(&'static str, f64, f64)>>,
    /// The section being dragged, once the pointer has moved.
    pub(super) dragging: RwSignal<Option<&'static str>>,
    /// Where it would drop: the section, and whether after it.
    pub(super) over: RwSignal<Option<(&'static str, bool)>>,
    /// A drag just ended, so the click that follows the release is ignored.
    just_moved: StoredValue<bool>,
}

impl Reorder {
    pub(super) fn new() -> Reorder {
        Reorder { pressed: StoredValue::new(None), dragging: RwSignal::new(None), over: RwSignal::new(None), just_moved: StoredValue::new(false) }
    }

    /// For a grip's pointerdown.
    pub(super) fn press(self, key: &'static str) -> impl Fn(web_sys::PointerEvent) + 'static {
        move |ev: web_sys::PointerEvent| {
            if ev.button() != 0 {
                return;
            }
            // Not from the buttons and boxes in a heading or row.
            let skip = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|e| e.closest("[data-no-grip], input").ok().flatten().is_some());
            if !skip {
                self.pressed.set_value(Some((key, ev.client_x() as f64, ev.client_y() as f64)));
            }
        }
    }

    /// For the list's pointermove.
    pub(super) fn track(self, ev: web_sys::PointerEvent) {
        let Some((key, x, y)) = self.pressed.get_value() else { return };
        let (cx, cy) = (ev.client_x() as f64, ev.client_y() as f64);
        if self.dragging.get_untracked().is_none() {
            if (cx - x).abs() + (cy - y).abs() < 5.0 {
                return;
            }
            self.dragging.set(Some(key));
            if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                let _ = el.set_pointer_capture(ev.pointer_id());
            }
        }
        ev.prevent_default();
        let target = document().element_from_point(cx as f32, cy as f32).and_then(|e| e.closest("[data-section]").ok().flatten());
        let over = target.and_then(|t| {
            let k = t.get_attribute("data-section")?;
            let key = SECTIONS.iter().find(|s| s.0 == k)?.0;
            let r = t.get_bounding_client_rect();
            Some((key, cy > r.top() + r.height() / 2.0))
        });
        if self.over.get_untracked() != over {
            self.over.set(over);
        }
    }

    /// For the list's pointerup and pointercancel: moves the section when it was dropped on one.
    pub(super) fn release(self, ctx: Ctx, drop: bool) {
        self.pressed.set_value(None);
        let Some(from) = self.dragging.get_untracked() else { return };
        if let (true, Some((to, after))) = (drop, self.over.get_untracked()) {
            ctx.set_section_order(moved_section(&ctx.section_order.get_untracked(), from, to, after));
        }
        self.dragging.set(None);
        self.over.set(None);
        self.just_moved.set_value(true);
        let _ = set_timeout_with_handle(move || { let _ = self.just_moved.try_set_value(false); }, std::time::Duration::from_millis(50));
    }

    /// True right after a drag, for click handlers to ignore the release.
    pub(super) fn just_moved(self) -> bool {
        self.just_moved.get_value()
    }

    /// The class for section `key`'s element while dragging.
    pub(super) fn class(self, key: &'static str) -> &'static str {
        if self.dragging.get() == Some(key) {
            return "dragging";
        }
        match self.over.get() {
            Some((k, false)) if k == key && self.dragging.get().is_some() => "drop-before",
            Some((k, true)) if k == key && self.dragging.get().is_some() => "drop-after",
            _ => "",
        }
    }
}

/// Places on the Mac (and its disks) and on the connected devices. Clicking one opens it in the
/// active pane. Right-click it to choose its sections.
#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // A section heading that folds its section away; remembered across launches.
    // `buttons` go at its right.
    let reorder = Reorder::new();
    let section = move |key: &'static str, title: &'static str, buttons: Option<ViewFn>| {
        let storage_key = format!("ferry-files.fold.{key}");
        let open = RwSignal::new(!stored_flag(&storage_key, false));
        let heading = move || {
            let storage_key = storage_key.clone();
            view! {
            <h2 data-grip on:pointerdown=reorder.press(key)>
            <button class="fold" aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    if reorder.just_moved() {
                        return;
                    }
                    let now = !open.get_untracked();
                    open.set(now);
                    store(&storage_key, if now { "0" } else { "1" });
                }>
                {chev(open)}{title}
            </button>
            <span class="head-buttons" data-no-grip>{buttons.as_ref().map(|b| b.run())}</span></h2>
            }
        };
        (heading, open)
    };
    let (fav_heading, fav_open) = section("favs", "Favourites", None);
    let (pairs_heading, pairs_open) = section("pairs", "Sync pairs", None);
    let (ws_heading, ws_open) = section("workspaces", "Workspaces", None);
    // Naming a workspace to save: None when not asked.
    let ws_name = RwSignal::new(None::<String>);
    let (mac_heading, mac_open) = section("mac", crate::os::computer(), None);
    let dev_buttons = ViewFn::from(move || view! {
        <button class="refresh" class:turning=move || ctx.looking.get()
            title="Look for devices and disks again" aria-label="Look for devices again"
            on:click=move |_| ctx.refresh_devices()>
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.5-3.6"/><path d="M13 2.5v3h-3"/></svg>
        </button>
        <button class="refresh" title="Connect a phone over Wi-Fi (adb Wireless debugging)" aria-label="Connect over Wi-Fi"
            on:click=move |_| ctx.wifi.set(true)>{ico(Icon::Wifi)}</button>
    });
    let (dev_heading, dev_open) = section("devices", "Devices", Some(dev_buttons));
    let open_conns = move || {
        ctx.settings_tab.set("connections");
        ctx.settings.set(true);
    };
    let srv_buttons = ViewFn::from(move || view! {
        <button class="refresh" title="Add a server: SFTP, SMB share or Everything" aria-label="Add a server"
            on:click=move |_| open_conns()>{ico(Icon::Plus)}</button>
    });
    let (srv_heading, srv_open) = section("servers", "Servers", Some(srv_buttons));
    // A section hidden with a right-click shows nothing, not even its title.
    let shown = move |key: &str| ctx.hidden_sections.with(|h| !h.iter().any(|x| x == key));
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
                {i.is_phone().then(|| {
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
                })}
                {match (i.transport.as_str(), i.usb.clone().unwrap_or_default()) {
                    ("sftp", _) => view! {
                        <button class="eject" title=format!("Remove {} from the connections", i.label) aria-label="Remove"
                            on:click=move |_| ctx.remove_conn_of(src)>{ico(Icon::Close)}</button>
                    }.into_any(),
                    ("everything", _) => view! {
                        <button class="eject" title=format!("Search all drives on {}", i.label) aria-label="Search all drives"
                            on:click=move |_| ctx.search_all(src)>{ico(Icon::Search)}</button>
                        <button class="eject" title=format!("Remove {} from the connections", i.label) aria-label="Remove"
                            on:click=move |_| ctx.remove_conn_of(src)>{ico(Icon::Close)}</button>
                    }.into_any(),
                    _ => view! {
                        <button class="eject" title=format!("Disconnect {}", i.label) aria-label="Disconnect"
                            on:click=move |_| ctx.disconnect(src)>{ico(Icon::Eject)}</button>
                    }.into_any(),
                }}
            </div>
            {move || open.get().then(|| view! {
                <div class="dev-dirs">
                    {dirs.clone().into_iter().map(|(name, path)| view! { <TreeNode src name path depth=1/> }).collect_view()}
                </div>
            })}
        }
    };
    // The sections, in the order chosen in Settings > Sidebar.
    let sec_workspaces = move || shown("workspaces").then(|| view! {
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
                            on:click=move |_| ctx.set_workspaces(ctx.workspaces.get_untracked().into_iter().filter(|x| x.name != name).collect())>{ico(Icon::Close)}</button>
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
                                on:contextmenu=|e| e.stop_propagation()
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
    });
    let sec_favs = move || (shown("favs") && !ctx.favs.with(|f| f.is_empty())).then(|| view! {
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
                            on:click=move |_| ctx.set_favs(ctx.favs.get_untracked().into_iter().filter(|x| *x != f).collect())>{ico(Icon::Close)}</button>
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
    });
    let sec_pairs = move || (shown("pairs") && !ctx.sync_pairs.with(|x| x.is_empty())).then(|| view! {
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
                            on:click=move |_| ctx.set_pairs(ctx.sync_pairs.get_untracked().into_iter().filter(|x| *x != p2).collect())>{ico(Icon::Close)}</button>
                    </div>
                }
            }).collect_view())}
    });
    // A mounted SMB share: a folder on the computer, with a button to disconnect it.
    let mounted = move |name: String, path: String| {
        let (tip, at, n) = (format!("Disconnect {name}"), path.clone(), name.clone());
        let extra = ViewFn::from(move || {
            let (tip, at, n) = (tip.clone(), at.clone(), n.clone());
            view! { <button class="act" title=tip aria-label="Disconnect" on:click=move |_| ctx.unmount_smb(at.clone(), n.clone())>{ico(Icon::Eject)}</button> }
        });
        view! { <TreeNode src=Src::Mac name path depth=1 class="drive" extra/> }
    };
    // A connection that isn't connected (or an SMB share not mounted): connects with a click.
    let off_row = move |c: servers::Conn| {
        let (id, c1) = (c.id.clone(), c.clone());
        let id2 = id.clone();
        let usual = if c.kind == servers::ConnKind::Smb { "Click to connect." } else { "Connecting… Click to try again." };
        let tip = move || ctx.conn_errors.with(|e| e.get(&id).cloned()).map_or_else(|| usual.to_string(), |e| e + " Click to try again.");
        view! {
            <div class="device">
                <span class="fold none"></span>
                <button class="name off" title=tip on:click=move |_| {
                    let c = c1.clone();
                    spawn_local(async move { let _ = ctx.connect_conn(c, true).await; });
                }><b>{c.name()}</b></button>
                <button class="eject" title="Remove from the connections" aria-label="Remove"
                    on:click=move |_| ctx.remove_conn(&id2)>{ico(Icon::Close)}</button>
            </div>
        }
    };
    let add_row = move |tip: &'static str| view! {
        <div class="tree-row" style="--depth: 1">
            <span class="twisty none"></span>
            <button class="item add" title=tip on:click=move |_| open_conns()>"Add a server…"</button>
        </div>
    };
    let sec_servers = move || shown("servers").then(|| view! {
        {srv_heading()}
        {move || srv_open.get().then(|| view! {
            {move || ctx.conns_of(servers::ConnKind::Sftp).into_iter().map(|c| match ctx.conn_src(&c).and_then(|s| ctx.src_info(s)) {
                Some(i) => device(i).into_any(),
                None => off_row(c).into_any(),
            }).collect_view()}
            {move || ctx.conns_of(servers::ConnKind::Smb).into_iter().map(|c| match ctx.smb_path(&c) {
                // Mounted: a folder on the computer.
                Some(path) => mounted(c.name(), path).into_any(),
                None => off_row(c).into_any(),
            }).collect_view()}
            {move || ctx.conns_of(servers::ConnKind::Everything).into_iter().map(|c| match ctx.conn_src(&c).and_then(|s| ctx.src_info(s)) {
                Some(i) => device(i).into_any(),
                None => off_row(c).into_any(),
            }).collect_view()}
            // Shares mounted outside the app (or before the list had them).
            {move || ctx.other_mounts().into_iter().map(|m| mounted(m.share.trim_start_matches("smb://").to_string(), m.path).into_any()).collect_view()}
            {move || (ctx.conns.with(Vec::is_empty) && ctx.smb_mounts.with(Vec::is_empty))
                .then(|| add_row("Add an SFTP server, an SMB share (Windows sharing, NAS) or a Windows computer running Everything"))}
        })}
    });
    let sec_mac = move || shown("mac").then(|| view! {
        {mac_heading()}
        {move || mac_open.get().then(|| view! {
            {ctx.mac.get().map(|i| i.quick_dirs).unwrap_or_default().into_iter()
                .map(|(name, path)| view! { <TreeNode src=Src::Mac name path depth=1/> }).collect_view()}
            {drives.get().into_iter().map(|(name, path)| {
                let (tip, at) = (format!("Eject {name}"), path.clone());
                let eject = ViewFn::from(move || {
                    let (tip, at) = (tip.clone(), at.clone());
                    view! { <button class="act" title=tip aria-label="Eject" on:click=move |_| ctx.eject(at.clone())>{ico(Icon::Eject)}</button> }
                });
                view! { <TreeNode src=Src::Mac name path depth=1 class="drive" extra=eject/> }
            }).collect_view()}
        })}
    });
    let sec_devices = move || shown("devices").then(|| view! {
        {dev_heading()}
        {move || dev_open.get().then(|| view! {
            {ctx.phones.get().into_iter().filter(Info::is_phone).map(device).collect_view()}
            <DeviceFinder/>
        })}
    });
    view! {
        <nav class="side" class:reordering=move || reorder.dragging.get().is_some() on:contextmenu=move |ev| ctx.show_sections_menu(&ev)>
            <div class="side-scroll"
                on:pointermove=move |ev| reorder.track(ev)
                on:pointerup=move |_| reorder.release(ctx, true)
                on:pointercancel=move |_| reorder.release(ctx, false)>
                {move || ctx.section_order.get().into_iter().map(|k| {
                    let body = match k {
                        "workspaces" => sec_workspaces().into_any(),
                        "favs" => sec_favs().into_any(),
                        "pairs" => sec_pairs().into_any(),
                        "servers" => sec_servers().into_any(),
                        "mac" => sec_mac().into_any(),
                        "devices" => sec_devices().into_any(),
                        _ => ().into_any(),
                    };
                    view! { <div class=move || format!("sec {}", reorder.class(k)) data-section=k>{body}</div> }
                }).collect_view()}
            </div>
        </nav>
    }
}

/// The bar down the window's left edge, as in VS Code: the sidebar, books and commands at the top;
/// backups, history and settings at the bottom.
#[component]
pub(super) fn ActivityBar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let side_title = move || ctx.with_key(&format!("{} the {}", if ctx.side.get() { "Hide" } else { "Show" }, crate::os::sidebar()), "sidebar");
    view! {
        <nav class="activity" aria-label="Tools">
            <button class="act" class:on=move || ctx.side.get() title=side_title aria-label="Sidebar"
                aria-pressed=move || ctx.side.get().to_string()
                on:click=move |_| ctx.set_side(!ctx.side.get_untracked())>{ico(Icon::Sidebar)}</button>
            <button class="act" title="Books on all devices" aria-label="Books on all devices"
                on:click=move |_| ctx.panel.set(Some(Panel::AllBooks))>{ico(Icon::Book)}</button>
            <button class="act" title=move || ctx.with_key("Commands", "palette") aria-label="Commands"
                on:click=move |_| ctx.panel.set(Some(Panel::Palette))>{ico(Icon::Search)}</button>
            <span class="grow"></span>
            <button class="act" title="Backups: files replaced or deleted by the app, which can be put back" aria-label="Backups"
                on:click=move |_| ctx.panel.set(Some(Panel::Backups))>{ico(Icon::Backups)}</button>
            <button class="act" title="History: what was done since the app started" aria-label="History"
                on:click=move |_| ctx.panel.set(Some(Panel::History))>{ico(Icon::History)}</button>
            <button class="act" title="Settings" aria-label="Settings" on:click=move |_| ctx.settings.set(true)>{ico(Icon::Settings)}</button>
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
    let prefs = expect_context::<VideoPrefs>();
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
        {check(ctx.colour_code.into(), Ctx::set_colour_code, "Colour code in the preview")}
        <p class="hint">"Colours keywords, strings and comments in code files, by the file's extension or its first line (#!)."</p>
        {check(ctx.verify.into(), Ctx::set_verify, "Check copies")}
        <p class="hint">"Reads each copied file back and compares it with the original; a copy that doesn't match is removed and reported, and a move keeps its original. Copies to a phone over MTP take about twice as long. (adb copies are always checked.)"</p>
    }.into_any();
    let phones = move || view! {
        {check(ctx.auto_connect.into(), Ctx::set_auto_connect, "Connect phones when they're plugged in (adb if allowed, else MTP)")}
        {check(ctx.phone_thumbs.into(), Ctx::set_phone_thumbs, "Thumbnails of phone files")}
        <p class="hint">{format!("Each phone thumbnail reads the whole image or EPUB over USB, once; then it's cached on {}. Files on {} always get thumbnails.", crate::os::the_computer(), crate::os::the_computer())}</p>
    }.into_any();
    let video = move || view! {
        <label class="row">
            "Hold to play at"
            <select prop:value=move || prefs.hold_speed.get().to_string()
                on:change=move |e| if let Ok(s) = event_target_value(&e).parse() { prefs.set_hold_speed(s) }>
                {HOLD_SPEEDS.iter().map(|s| view! { <option value=s.to_string()>{speed_label(*s)}</option> }).collect_view()}
            </select>
        </label>
        <p class="hint">"The speed while the video is held down, or → or L is held."</p>
        <label class="check">
            <input type="checkbox" prop:checked=move || prefs.turn_controls.get()
                on:change=move |e| prefs.set_turn_controls(event_target_checked(&e))/>
            "Turn the controls with the video"
        </label>
        <p class="hint">"On: the controls stay along the bottom of the picture, so they're on its side when the video is. Off: they stay level under it."</p>
    }.into_any();
    let storage = move || view! {
        {folder("backups", "Backups", "Copies of files from before they were replaced or deleted. Kept until you clear them in Backups, which moves them to the Trash.")}
        {folder("thumbs", "Thumbnail cache", "Made again when needed, so it's safe to clear.")}
        {folder("opened", "Opened phone files", "Copies made to open phone files in other apps. Changes to them don't go back to the phone.")}
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
    }.into_any();
    let storage_name = format!("On {}", crate::os::this_computer());
    let sections: [(&'static str, String); 8] = [
        ("general", "General".into()),
        ("sidebar", "Sidebar".into()),
        ("connections", "Connections".into()),
        ("files", "Files".into()),
        ("phones", "Phones".into()),
        ("video", "Video".into()),
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
                            "sidebar" => view! { <SidebarSettings/> }.into_any(),
                            "connections" => view! { <ConnectionsSettings/> }.into_any(),
                            "files" => files(),
                            "phones" => phones(),
                            "video" => video(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_sections() {
        let o = ["a", "b", "c", "d"];
        assert_eq!(moved_section(&o, "a", "c", false), ["b", "a", "c", "d"]);
        assert_eq!(moved_section(&o, "a", "c", true), ["b", "c", "a", "d"]);
        assert_eq!(moved_section(&o, "d", "a", false), ["d", "a", "b", "c"]);
        assert_eq!(moved_section(&o, "c", "d", true), ["a", "b", "d", "c"]);
        assert_eq!(moved_section(&o, "b", "b", true), o);
        assert_eq!(moved_section(&o, "x", "a", false), o);
    }
}
