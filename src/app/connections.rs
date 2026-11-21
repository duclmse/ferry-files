//! Settings > Connections (the servers in the sidebar: a list filtered by the box at its top,
//! and a form to add or change one) and Settings > Sidebar (which
//! sections it shows, in what order).

use super::*;

use servers::{Auth, Conn, ConnKind};

/// Whether connection `c` is connected (or mounted), and what to say about it.
fn status(ctx: Ctx, c: &Conn) -> (bool, String) {
    match c.kind {
        ConnKind::Smb => match ctx.smb_path(c) {
            Some(path) => (true, format!("Connected at {path}")),
            None => (false, ctx.conn_errors.with(|e| e.get(&c.id).cloned()).unwrap_or_else(|| "Not connected. Click it in the sidebar to connect.".into())),
        },
        _ if ctx.conn_on(c) => (true, "Connected".into()),
        _ => (false, ctx.conn_errors.with(|e| e.get(&c.id).cloned()).unwrap_or_else(|| "Connecting…".into())),
    }
}

/// What a connection logs in with, as shown in its row.
fn auth_label(c: &Conn) -> &'static str {
    match (c.kind, c.auth) {
        (ConnKind::Everything, Auth::System) => "No login",
        (ConnKind::Sftp, Auth::System) => "ssh keys",
        (ConnKind::Smb, Auth::System) => "System asks",
        (ConnKind::WebDav, Auth::System) => "No login",
        (ConnKind::Ftp, Auth::System) => "Anonymous",
        (ConnKind::S3, _) if c.saved => "Key saved",
        (ConnKind::S3, _) => "Key not saved",
        (_, Auth::Password) if c.saved => "Password saved",
        (_, Auth::Password) => "Password not saved",
        (_, Auth::Key) => "Key file",
    }
}

/// The ways connections of `kind` can log in: (value, label).
fn auth_choices(kind: ConnKind) -> &'static [(Auth, &'static str)] {
    match kind {
        ConnKind::Sftp => &[(Auth::System, "This computer's ssh keys"), (Auth::Password, "Password"), (Auth::Key, "Key file")],
        ConnKind::Smb => &[(Auth::System, "Ask when connecting"), (Auth::Password, "Password")],
        ConnKind::Everything | ConnKind::WebDav => &[(Auth::System, "No login"), (Auth::Password, "User name and password")],
        ConnKind::Ftp => &[(Auth::System, "Anonymous"), (Auth::Password, "User name and password")],
        ConnKind::S3 => &[(Auth::Password, "Access key")],
    }
}

fn auth_key(a: Auth) -> &'static str {
    match a {
        Auth::System => "system",
        Auth::Password => "password",
        Auth::Key => "key",
    }
}

/// What each type is, under the form.
fn kind_about(kind: ConnKind) -> &'static str {
    match kind {
        ConnKind::Sftp => "A server you can reach with ssh. Its files open like a phone's, and its terminal opens under the pane.",
        ConnKind::Smb => "A shared folder on a Windows computer or NAS box. The system mounts it as a folder on this computer.",
        ConnKind::Everything => "A Windows computer's disks, through the HTTP server of Everything (voidtools). Read-only.",
        ConnKind::WebDav => "A folder shared over WebDAV: Nextcloud, ownCloud, a NAS box, or a phone app that serves files.",
        ConnKind::Ftp => "An FTP server: a NAS box, a router, or a phone app that serves files over Wi-Fi.",
        ConnKind::S3 => "A bucket on Amazon S3 or a service like it: Cloudflare R2, Backblaze B2, MinIO.",
    }
}

/// The server box's example for each type.
fn host_example(kind: ConnKind) -> &'static str {
    match kind {
        ConnKind::WebDav => "cloud.example.com or 10.0.0.2",
        ConnKind::Ftp => "ftp.example.com or 10.0.0.2",
        ConnKind::S3 => "s3.us-east-1.amazonaws.com",
        _ => "nas, 10.0.0.2 or a host from ~/.ssh/config",
    }
}

/// The form that adds or changes a connection. `editing` holds it while it's open.
fn conn_form(ctx: Ctx, editing: RwSignal<Option<Conn>>, is_new: bool) -> impl IntoView {
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let trying = RwSignal::new(false);
    let get = move || editing.get().unwrap_or_else(|| Conn::new(ConnKind::Sftp));
    let set = move |f: &dyn Fn(&mut Conn)| editing.update(|e| if let Some(c) = e { f(c) });
    let kind = Memo::new(move |_| get().kind);
    let auth = Memo::new(move |_| get().auth);
    let tls = Memo::new(move |_| get().tls);
    let save = move || {
        let Some(c) = editing.get_untracked() else { return };
        if trying.get_untracked() {
            return;
        }
        error.set(None);
        trying.set(true);
        let pw = password.get_untracked();
        spawn_local(async move {
            match ctx.save_conn(c, pw).await {
                Ok(()) => editing.set(None),
                // Saved, but not connected: say why and keep the form, to fix it.
                Err(e) => error.set(Some(e)),
            }
            trying.set(false);
        });
    };
    let text = move |label: &'static str, placeholder: &'static str, value: Signal<String>, put: fn(&mut Conn, String)| view! {
        <label class="field">
            <span>{label}</span>
            <input type="text" spellcheck="false" autocomplete="off" placeholder=placeholder prop:value=move || value.get()
                on:input=move |e| { let v = event_target_value(&e); set(&|c| put(c, v.clone())) }
                on:keydown=move |e| if e.key() == "Enter" { save() }/>
        </label>
    };
    let secret_label = move || match (kind.get(), auth.get()) {
        (_, Auth::Key) => "Passphrase",
        (ConnKind::S3, _) => "Secret access key",
        _ => "Password",
    };
    let secret_hint = move || match (editing.with(|e| e.as_ref().is_some_and(|c| c.saved)), auth.get()) {
        (true, _) => "Saved. Type a new one to replace it.",
        (false, Auth::Key) => "Only if the key has one",
        _ => "",
    };
    view! {
        <div class="conn-form">
            <h3>{if is_new { "New connection" } else { "Change connection" }}</h3>
            <div class="seg" role="group" aria-label="Type">
                {ConnKind::ALL.into_iter().map(|k| view! {
                    <button class:active=move || kind.get() == k aria-pressed=move || (kind.get() == k).to_string()
                        disabled=!is_new
                        on:click=move |_| set(&|c| { c.set_kind(k); if !auth_choices(k).iter().any(|a| a.0 == c.auth) { c.auth = auth_choices(k)[0].0 } })>
                        {k.label()}
                    </button>
                }).collect_view()}
            </div>
            <p class="hint flush">{move || kind_about(kind.get())}</p>
            <div class="row2">
                {move || text("Server", host_example(kind.get()), Signal::derive(move || get().host), |c, v| c.host = v)}
                <label class="field port">
                    <span>"Port"</span>
                    <input type="text" inputmode="numeric" placeholder=move || match (kind.get(), tls.get()) {
                        (ConnKind::Sftp, _) => "22",
                        (ConnKind::Smb, _) => "445",
                        (ConnKind::Ftp, _) => "21",
                        (ConnKind::WebDav | ConnKind::S3, true) => "443",
                        _ => "80",
                    }
                        prop:value=move || get().port.map(|p| p.to_string()).unwrap_or_default()
                        on:input=move |e| { let v = event_target_value(&e); set(&|c| c.port = v.trim().parse().ok()) }
                        on:keydown=move |e| if e.key() == "Enter" { save() }/>
                </label>
            </div>
            {move || match kind.get() {
                ConnKind::Smb => Some(text("Shared folder", "Photos", Signal::derive(move || get().share), |c, v| c.share = v).into_any()),
                ConnKind::WebDav => Some(text("Folder", "Empty for the top, or like remote.php/dav/files/me", Signal::derive(move || get().share), |c, v| c.share = v).into_any()),
                ConnKind::S3 => Some(view! {
                    {text("Bucket", "photos", Signal::derive(move || get().share), |c, v| c.share = v)}
                    {text("Region", "us-east-1 when empty; auto for R2", Signal::derive(move || get().region), |c, v| c.region = v)}
                }.into_any()),
                _ => None,
            }}
            {move || matches!(kind.get(), ConnKind::WebDav | ConnKind::Ftp | ConnKind::S3).then(|| view! {
                <label class="check">
                    <input type="checkbox" prop:checked=move || tls.get() on:change=move |e| { let on = event_target_checked(&e); set(&|c| c.tls = on) }/>
                    {move || if kind.get() == ConnKind::Ftp { "Encrypted (FTPS)" } else { "Encrypted (https)" }}
                </label>
            })}
            <label class="field" class:hidden=move || auth_choices(kind.get()).len() < 2>
                <span>"Sign in with"</span>
                <select prop:value=move || auth_key(auth.get())
                    on:change=move |e| { let v = event_target_value(&e); set(&|c| c.auth = match v.as_str() { "password" => Auth::Password, "key" => Auth::Key, _ => Auth::System }) }>
                    {move || auth_choices(kind.get()).iter().map(|(a, label)| view! { <option value=auth_key(*a)>{*label}</option> }).collect_view()}
                </select>
            </label>
            {move || (kind.get() == ConnKind::Sftp || auth.get() != Auth::System || kind.get() == ConnKind::Smb)
                .then(|| match kind.get() {
                    ConnKind::S3 => text("Access key ID", "AKIA…", Signal::derive(move || get().user), |c, v| c.user = v),
                    k => text("User name", if k == ConnKind::Sftp { "As in ~/.ssh/config when empty" } else { "me" }, Signal::derive(move || get().user), |c, v| c.user = v),
                })}
            {move || (kind.get() == ConnKind::Smb && auth.get() == Auth::Password)
                .then(|| text("Domain or workgroup", "Only if the server wants one", Signal::derive(move || get().domain), |c, v| c.domain = v))}
            {move || (auth.get() == Auth::Key).then(|| text("Key file", "~/.ssh/id_ed25519", Signal::derive(move || get().key_file), |c, v| c.key_file = v))}
            {move || (auth.get() != Auth::System).then(|| view! {
                <label class="field">
                    <span>{secret_label}</span>
                    <input type="password" autocomplete="new-password" placeholder=secret_hint prop:value=move || password.get()
                        on:input=move |e| password.set(event_target_value(&e))
                        on:keydown=move |e| if e.key() == "Enter" { save() }/>
                </label>
                <p class="hint flush">"Kept in the system's keychain, not in the app's settings."</p>
            })}
            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
            <div class="buttons">
                <button class="btn" on:click=move |_| editing.set(None)>"Cancel"</button>
                <button class="btn primary" disabled=move || trying.get() on:click=move |_| save()>
                    {move || if trying.get() { "Connecting…" } else if is_new { "Add" } else { "Save" }}
                </button>
            </div>
        </div>
    }
}

#[component]
pub(super) fn ConnectionsSettings() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let query = RwSignal::new(String::new());
    // The connection being added or changed, and whether it's new.
    let editing = RwSignal::new(None::<Conn>);
    let is_new = RwSignal::new(true);
    let filter = NodeRef::<html::Input>::new();
    filter.on_load(move |el| { request_animation_frame(move || { let _ = el.focus(); }); });
    let start = move |text: String| {
        let (kind, tls) = servers::kind_and_tls(&text);
        let mut c = Conn::new(kind);
        let (user, host, port, share) = servers::split_address(&text);
        (c.user, c.host, c.port, c.share) = (user, host, port, share);
        match kind {
            ConnKind::WebDav => (c.tls, c.share) = (tls, servers::split_path(&text)),
            ConnKind::Ftp => c.tls = tls,
            _ => {}
        }
        if kind == ConnKind::Ftp && !c.user.is_empty() {
            c.auth = Auth::Password;
        }
        is_new.set(true);
        editing.set(Some(c));
    };
    let shown = Memo::new(move |_| {
        let q = query.get();
        ctx.conns.with(|cs| cs.iter().filter(|c| c.matches(&q)).cloned().collect::<Vec<_>>())
    });
    let open_id = Memo::new(move |_| editing.with(|e| e.as_ref().map(|c| c.id.clone())));
    view! {
        <div class="conn-filter">
            <input node_ref=filter type="search" spellcheck="false" autocomplete="off"
                placeholder="Find by server or user, or type an address to add"
                aria-label="Find or add a connection"
                prop:value=move || query.get()
                on:input=move |e| query.set(event_target_value(&e))
                on:keydown=move |e| if e.key() == "Enter" {
                    let q = query.get_untracked();
                    match shown.get_untracked().as_slice() {
                        [one] => { is_new.set(false); editing.set(Some(one.clone())) }
                        [] if !q.trim().is_empty() => start(q),
                        _ => {}
                    }
                }/>
            <button class="btn" on:click=move |_| start(query.get_untracked())>{ico(Icon::Plus)}"New"</button>
        </div>
        {move || (editing.with(Option::is_some) && is_new.get()).then(|| conn_form(ctx, editing, true))}
        <div class="conn-list">
            {move || shown.get().into_iter().map(|c| {
                let id = c.id.clone();
                let c2 = c.clone();
                let status = Signal::derive({ let c = c.clone(); move || status(ctx, &c) });
                let is_open = { let id = id.clone(); move || !is_new.get() && open_id.get().as_deref() == Some(id.as_str()) };
                let (is_open2, is_open3) = (is_open.clone(), is_open.clone());
                view! {
                    <div class="conn-row">
                        <span class="dot" class:on=move || status.get().0 aria-hidden="true"></span>
                        <span class="badge">{c.kind.label()}</span>
                        <div class="what">
                            <b>{c.name()}</b>
                            <span class="about">{move || status.get().1}" · "{auth_label(&c)}</span>
                        </div>
                        <button class="btn" aria-expanded=move || is_open().to_string()
                            on:click=move |_| if is_open2() { editing.set(None) } else { is_new.set(false); editing.set(Some(c2.clone())) }>"Edit"</button>
                        <button class="btn" on:click=move |_| { if open_id.get_untracked().as_deref() == Some(id.as_str()) { editing.set(None) } ctx.remove_conn(&id) }>
                            {ico(Icon::Trash)}"Remove"</button>
                    </div>
                    {move || is_open3().then(|| conn_form(ctx, editing, false))}
                }
            }).collect_view()}
        </div>
        {move || shown.with(Vec::is_empty).then(|| {
            let q = query.get();
            if q.trim().is_empty() {
                view! { <p class="hint flush">"No connections yet. Type a server's address above, like me@server, nas/Photos or pc:8080."</p> }.into_any()
            } else {
                let label = format!("Add {}…", q.trim());
                view! {
                    <p class="hint flush">"No connection matches."</p>
                    <button class="btn" on:click=move |_| start(q.clone())>{ico(Icon::Plus)}{label}</button>
                }.into_any()
            }
        })}
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
    let reorder = super::sidebar::Reorder::new();
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
        <p class="hint flush">"The sidebar's sections, top to bottom. Drag them by ⠿ (or their titles in the sidebar) to move them. Favourites and Sync pairs show only when there are some."</p>
        <div class="order" class:reordering=move || reorder.dragging.get().is_some()
            on:pointermove=move |ev| reorder.track(ev)
            on:pointerup=move |_| reorder.release(ctx, true)
            on:pointercancel=move |_| reorder.release(ctx, false)>
            {move || {
                let order = ctx.section_order.get();
                let n = order.len();
                order.into_iter().enumerate().map(|(i, key)| {
                    let title = section_title(key);
                    view! {
                        <div class=move || format!("order-row {}", reorder.class(key)) data-section=key>
                            <span class="grip" data-grip title=format!("Drag to move {title}") on:pointerdown=reorder.press(key)>"⠿"</span>
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
