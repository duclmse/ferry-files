//! adb over Wi-Fi: the Connect over Wi-Fi dialog, remembered addresses, and reconnecting to them
//! when the app starts.

use super::*;
use crate::api::WifiService;

/// Phones connected over Wi-Fi before, as (name, address), newest first.
fn remembered() -> Vec<(String, String)> {
    stored(WIFI_KEY)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn remember(list: &[(String, String)]) {
    store(WIFI_KEY, &serde_json::to_string(list).unwrap_or_default());
}

/// What `connect` takes for a phone adb reaches at `serial` (its address).
fn wifi_device(serial: &str, name: &str) -> UsbDevice {
    UsbDevice {
        usb: format!("adb:{serial}"),
        transport: "adb".into(),
        which: serial.into(),
        name: name.into(),
        serial: Some(serial.into()),
    }
}

impl Ctx {
    /// Connects adb to `address`, then the app to the phone. Errors go to `error`; on success the
    /// address is remembered under the phone's name and the dialog closes.
    fn connect_wifi(
        self,
        address: String,
        error: RwSignal<Option<String>>,
        working: RwSignal<bool>,
    ) {
        working.set(true);
        error.set(None);
        spawn_local(async move {
            match call::<String>("adb_connect", json!({ "address": address })).await {
                Ok(serial) => {
                    // Not again by the automatic connect, which would find it on its next look.
                    self.tried.update_value(|t| {
                        t.insert(serial.clone());
                    });
                    match call::<Info>(
                        "connect",
                        json!({ "transport": "adb", "which": serial, "serial": serial }),
                    )
                    .await
                    {
                        Ok(info) => {
                            let mut list = remembered();
                            list.retain(|(_, a)| *a != serial);
                            list.insert(0, (info.label.clone(), serial));
                            list.truncate(8);
                            remember(&list);
                            self.connected(info, true);
                            self.wifi.set(false);
                        }
                        Err(e) => error.set(Some(e)),
                    }
                    self.scan();
                }
                Err(e) => error.set(Some(e)),
            }
            working.set(false);
        });
    }

    /// At launch, with automatic connecting on: tries each remembered address once, quietly.
    /// (Android gives Wireless debugging a new port each time it's turned on, so an old address
    /// often won't answer; the dialog finds the new one.)
    pub(super) fn reconnect_wifi(self) {
        if !self.auto_connect.get_untracked() {
            return;
        }
        let list = remembered();
        if list.is_empty() {
            return;
        }
        spawn_local(async move {
            for (name, address) in list {
                if self.connected_keys().contains(&address) {
                    continue;
                }
                if let Ok(serial) =
                    call::<String>("adb_connect", json!({ "address": address })).await
                {
                    self.tried.update_value(|t| {
                        t.insert(serial.clone());
                    });
                    let f = wifi_device(&serial, &name);
                    if let Ok(info) = call::<Info>(
                        "connect",
                        json!({ "transport": "adb", "which": f.which, "serial": f.serial }),
                    )
                    .await
                    {
                        self.connected(info, false);
                    }
                }
            }
            self.scan();
        });
    }
}

/// Pairing and connecting a phone over Wi-Fi.
#[component]
pub(super) fn WifiDialog() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let close = move || ctx.wifi.set(false);
    let address = RwSignal::new(String::new());
    let pair_address = RwSignal::new(String::new());
    let code = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let note = RwSignal::new(None::<String>);
    let working = RwSignal::new(false);
    let found = RwSignal::new(None::<Vec<WifiService>>);
    let saved = RwSignal::new(remembered());
    let look = move || {
        found.set(None);
        spawn_local(async move {
            found.set(Some(
                call::<Vec<WifiService>>("adb_discover", json!({}))
                    .await
                    .unwrap_or_default(),
            ));
        });
    };
    look();
    let pair = move || {
        working.set(true);
        error.set(None);
        note.set(None);
        let (a, c) = (pair_address.get_untracked(), code.get_untracked());
        spawn_local(async move {
            match call::<()>("adb_pair", json!({ "address": a, "code": c })).await {
                Ok(()) => {
                    code.set(String::new());
                    // The connect port isn't the pairing port: it's on the phone's main
                    // Wireless debugging screen. Same host, so start the address with it.
                    if let Some((host, _)) = a.trim().rsplit_once(':') {
                        address.set(format!("{host}:"));
                    }
                    note.set(Some("Paired. Now connect: type the address and port shown under “IP address & Port” on the phone.".into()));
                    look();
                }
                Err(e) => error.set(Some(e)),
            }
            working.set(false);
        });
    };
    let connect = move |a: String| ctx.connect_wifi(a, error, working);
    let steps = [
        "On the phone, turn on Settings → Developer options → Wireless debugging. Both must be on the same network.",
        "First time: tap “Pair device with pairing code” and type its address and code below.",
        "Then connect with the address under “IP address & Port”.",
    ];
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog wifi" on:click=|e| e.stop_propagation()>
                <h2>"Connect over Wi-Fi"</h2>
                <ol class="hint steps">{steps.into_iter().map(|s| view! { <li>{s}</li> }).collect_view()}</ol>

                <div class="wifi-found">
                    <div class="wifi-head">
                        <b>"On this network"</b>
                        <button class="btn small icon-only" title="Look again" aria-label="Look again" disabled=move || found.with(Option::is_none) on:click=move |_| look()>{ico(Icon::Refresh)}</button>
                    </div>
                    {move || match found.get() {
                        None => view! { <p class="hint"><span class="spin"></span>" Looking…"</p> }.into_any(),
                        Some(v) if v.is_empty() => view! {
                            <p class="hint">"No phone with Wireless debugging open was found. Type its address below instead."</p>
                        }.into_any(),
                        Some(v) => v.into_iter().map(|s| {
                            let pairing = s.kind == "pair";
                            let a = s.address.clone();
                            view! {
                                <div class="wifi-row">
                                    <span class="wifi-name" title=s.name.clone()>{s.address.clone()}
                                        <span class="hint">{if pairing { " · waiting for a pairing code" } else { " · paired" }}</span></span>
                                    {if pairing {
                                        view! { <button class="btn small" on:click=move |_| { pair_address.set(a.clone()); note.set(None); }>"Pair…"</button> }.into_any()
                                    } else {
                                        view! { <button class="btn small" disabled=move || working.get() on:click=move |_| connect(a.clone())>"Connect"</button> }.into_any()
                                    }}
                                </div>
                            }
                        }).collect_view().into_any(),
                    }}
                </div>

                <form class="wifi-form" on:submit=move |e| { e.prevent_default(); pair(); }>
                    <b>"Pair"</b>
                    <input placeholder="192.168.1.20:41235" aria-label="Pairing address" prop:value=move || pair_address.get()
                        on:input=move |e| pair_address.set(event_target_value(&e))/>
                    <input class="code" placeholder="Code" aria-label="Pairing code" inputmode="numeric" autocomplete="off" prop:value=move || code.get()
                        on:input=move |e| code.set(event_target_value(&e))/>
                    <button class="btn" type="submit" disabled=move || working.get() || pair_address.with(String::is_empty) || code.with(String::is_empty)>"Pair"</button>
                </form>
                <form class="wifi-form" on:submit=move |e| { e.prevent_default(); connect(address.get_untracked()); }>
                    <b>"Connect"</b>
                    <input placeholder="192.168.1.20:37000" aria-label="Address" prop:value=move || address.get()
                        on:input=move |e| address.set(event_target_value(&e))/>
                    <button class="btn primary" type="submit" disabled=move || working.get() || address.with(String::is_empty)>"Connect"</button>
                </form>

                {move || (!saved.with(Vec::is_empty)).then(|| view! {
                    <div class="wifi-found">
                        <b>"Before"</b>
                        {saved.get().into_iter().map(|(name, a)| {
                            let (a1, a2) = (a.clone(), a.clone());
                            view! {
                                <div class="wifi-row">
                                    <span class="wifi-name">{name}<span class="hint">{format!(" · {a}")}</span></span>
                                    <button class="btn small" disabled=move || working.get() on:click=move |_| connect(a1.clone())>"Connect"</button>
                                    <button class="btn small" title="Forget this address" on:click=move |_| {
                                        let mut list = saved.get_untracked();
                                        list.retain(|(_, x)| *x != a2);
                                        remember(&list);
                                        saved.set(list);
                                    }>"Forget"</button>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                })}

                {move || working.get().then(|| view! { <p class="hint"><span class="spin"></span>" Working…"</p> })}
                {move || note.get().map(|n| view! { <p class="hint">{n}</p> })}
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                <div class="actions">
                    <span class="grow"></span>
                    <button class="btn" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}
