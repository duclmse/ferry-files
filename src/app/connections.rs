//! Settings > Connections (the servers added to the sidebar) and Settings > Sidebar (which
//! sections it shows, in what order).

use super::*;

/// One server in Settings > Connections: its address, whether it's connected (or why not), and a
/// button to take it out.
fn server_row(name: String, status: Signal<(bool, String)>, remove: impl Fn() + 'static) -> impl IntoView {
    view! {
        <div class="conn-row">
            <span class="dot" class:on=move || status.get().0 aria-hidden="true"></span>
            <div class="what">
                <b>{name}</b>
                <span class="about">{move || status.get().1}</span>
            </div>
            <button class="btn" on:click=move |_| remove()>{ico(Icon::Trash)}"Remove"</button>
        </div>
    }
}

/// An address box with an Add button, and why adding failed.
fn add_box(placeholder: &'static str, add: impl Fn(String, RwSignal<Option<String>>, RwSignal<bool>) + Copy + 'static) -> impl IntoView {
    let text = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let trying = RwSignal::new(false);
    let go = move || {
        let t = text.get_untracked();
        if t.trim().is_empty() || trying.get_untracked() {
            return;
        }
        error.set(None);
        trying.set(true);
        add(t, error, trying);
        text.set(String::new());
    };
    view! {
        <div class="conn-add">
            <input type="text" placeholder=placeholder prop:value=move || text.get()
                on:input=move |e| text.set(event_target_value(&e))
                on:keydown=move |e| if e.key() == "Enter" { go() }/>
            <button class="btn" disabled=move || trying.get() on:click=move |_| go()>
                {move || if trying.get() { "Adding…" } else { "Add" }}
            </button>
        </div>
        {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
    }
}

#[component]
pub(super) fn ConnectionsSettings() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let add_ev = move |server: String, error: RwSignal<Option<String>>, trying: RwSignal<bool>| {
        spawn_local(async move {
            if let Err(e) = ctx.add_everything(server).await {
                error.set(Some(e));
            }
            trying.set(false);
        })
    };
    let add_sftp = move |server: String, error: RwSignal<Option<String>>, trying: RwSignal<bool>| {
        spawn_local(async move {
            if let Err(e) = ctx.add_sftp(server).await {
                error.set(Some(e));
            }
            trying.set(false);
        })
    };
    let add_smb = move |share: String, error: RwSignal<Option<String>>, trying: RwSignal<bool>| {
        spawn_local(async move {
            if let Err(e) = ctx.add_smb(share).await {
                error.set(Some(e));
            }
            trying.set(false);
        })
    };
    view! {
        <h3>"Everything"</h3>
        <p class="hint flush">"Windows computers' disks, through the HTTP server of Everything (voidtools). Read-only."</p>
        {move || ctx.everything.get().into_iter().map(|base| {
            let b = base.clone();
            let status = Signal::derive(move || match ctx.everything_src(&b) {
                Some(_) => (true, "Connected".to_string()),
                None => (false, ctx.everything_errors.with(|e| e.get(&b).cloned()).unwrap_or_else(|| "Connecting…".into())),
            });
            let name = base.trim_start_matches("http://").to_string();
            server_row(name, status, move || ctx.remove_everything(&base))
        }).collect_view()}
        {move || ctx.everything.with(Vec::is_empty).then(|| view! { <p class="hint flush">"None added."</p> })}
        {add_box("Address, like pc:8080", add_ev)}

        <h3>"SFTP"</h3>
        <p class="hint flush">"Servers you can reach with ssh. The app uses this computer's ssh keys and settings; it can't use passwords."</p>
        {move || ctx.sftp.get().into_iter().map(|addr| {
            let a = addr.clone();
            let status = Signal::derive(move || match ctx.sftp_src(&a) {
                Some(_) => (true, "Connected".to_string()),
                None => (false, ctx.server_errors.with(|e| e.get(&a).cloned()).unwrap_or_else(|| "Connecting…".into())),
            });
            server_row(addr.clone(), status, move || ctx.remove_sftp(&addr))
        }).collect_view()}
        {move || ctx.sftp.with(Vec::is_empty).then(|| view! { <p class="hint flush">"None added."</p> })}
        {add_box("Address, like me@server or me@server:2222", add_sftp)}

        <h3>"SMB"</h3>
        <p class="hint flush">"Shared folders on Windows computers and NAS boxes. The system asks for the password and can keep it in its keychain."</p>
        {move || ctx.smb.get().into_iter().map(|share| {
            let s = share.clone();
            let status = Signal::derive(move || match ctx.smb_path(&s) {
                Some(path) => (true, format!("Connected at {path}")),
                None => (false, ctx.server_errors.with(|e| e.get(&s).cloned()).unwrap_or_else(|| "Not connected. Click it in the sidebar to connect.".into())),
            });
            server_row(share.trim_start_matches("smb://").to_string(), status, move || ctx.remove_smb(&share))
        }).collect_view()}
        {move || ctx.smb.with(Vec::is_empty).then(|| view! { <p class="hint flush">"None added."</p> })}
        {add_box("Share, like smb://nas/Photos", add_smb)}
    }
}

/// What a section is called in Settings > Sidebar.
fn section_title(key: &str) -> &'static str {
    match key {
        "mac" => crate::os::computer(),
        k => SECTIONS.iter().find(|s| s.0 == k).map_or("", |s| s.1),
    }
}

#[component]
pub(super) fn SidebarSettings() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let swap = move |at: usize, to: usize| {
        let mut order = ctx.section_order.get_untracked();
        if at < order.len() && to < order.len() {
            order.swap(at, to);
            ctx.set_section_order(order);
        }
    };
    let toggle = move |key: &'static str, show: bool| {
        let mut hidden = ctx.hidden_sections.get_untracked();
        hidden.retain(|h| h != key);
        if !show {
            hidden.push(key.to_string());
        }
        ctx.set_hidden_sections(hidden);
    };
    view! {
        <p class="hint flush">"The sidebar's sections, top to bottom. Favourites and Sync pairs show only when there are some."</p>
        <div class="order">
            {move || {
                let order = ctx.section_order.get();
                let n = order.len();
                order.into_iter().enumerate().map(|(i, key)| {
                    let title = section_title(key);
                    view! {
                        <div class="order-row">
                            <label class="check">
                                <input type="checkbox" prop:checked=move || ctx.hidden_sections.with(|h| !h.iter().any(|x| x == key))
                                    on:change=move |e| toggle(key, event_target_checked(&e))/>
                                {title}
                            </label>
                            <button class="btn icon-only" title=format!("Move {title} up") aria-label="Move up"
                                disabled=i == 0 on:click=move |_| swap(i, i.saturating_sub(1))>"↑"</button>
                            <button class="btn icon-only" title=format!("Move {title} down") aria-label="Move down"
                                disabled=i + 1 == n on:click=move |_| swap(i, i + 1)>"↓"</button>
                        </div>
                    }
                }).collect_view()
            }}
        </div>
        <button class="btn" disabled=move || ctx.section_order.with(|o| o.iter().copied().eq(SECTIONS.iter().map(|s| s.0)))
            on:click=move |_| ctx.set_section_order(SECTIONS.iter().map(|s| s.0).collect())>"Put back in the usual order"</button>
    }
}
