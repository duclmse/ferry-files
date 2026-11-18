//! Devices: connecting, disconnecting, looking for them on USB, and the finder in the sidebar.

use super::*;

impl Ctx {
    /// A device is connected. With `show`, the active pane goes to it.
    pub(super) fn connected(self, info: Info, show: bool) {
        let src = Src::of(&info.id);
        self.phones.update(|ps| {
            if !ps.iter().any(|i| i.id == info.id) {
                ps.push(info);
            }
        });
        self.conn_error.set(None);
        if show {
            self.open_remembered(self.act(), src);
        }
    }

    /// Looks for devices on USB and disks on the Mac. When the app is busy or a look is under way,
    /// it's done later.
    pub(super) fn scan(self) {
        if self.busy.get_untracked() || self.scanning.get_value() || self.job.with_untracked(|j| j.is_some()) {
            self.scan_again.set_value(true);
            return;
        }
        self.scan_again.set_value(false);
        self.scanning.set_value(true);
        spawn_local(async move {
            if let Ok(d) = call::<Devices>("devices", json!({})).await {
                let pick = self.auto_pick(&d);
                if self.devices.get_untracked().as_ref() != Some(&d) {
                    self.devices.set(Some(d));
                }
                if let Some((f, show)) = pick {
                    self.connect(f, show);
                }
            }
            self.scanning.set_value(false);
            self.looking.set(false);
            if self.scan_again.get_value() {
                self.scan();
            }
        });
    }

    /// The refresh button: looks now.
    pub(super) fn refresh_devices(self) {
        self.looking.set(true);
        self.scan();
    }

    /// Keys of the connected phones: serial and USB place.
    pub(super) fn connected_keys(self) -> HashSet<String> {
        self.phones.with_untracked(|ps| ps.iter().flat_map(|i| [i.serial.clone(), i.usb.clone()]).flatten().collect())
    }

    /// The phone to connect automatically, if any, and whether to show it. Phones there when the
    /// app starts connect at once and don't take over the window; one plugged in later waits for a
    /// second look, so adb has time to show up next to MTP, and is shown.
    pub(super) fn auto_pick(self, d: &Devices) -> Option<(UsbDevice, bool)> {
        let connected = self.connected_keys();
        let now: HashSet<String> = d.found.iter().map(UsbDevice::key).collect();
        let (before, looks) = self.seen.get_value();
        self.seen.set_value((now.clone(), looks + 1));
        // Gone for two looks in a row: unplugged, so plugging it in again connects it again.
        self.tried.update_value(|t| t.retain(|k| now.contains(k) || before.contains(k) || connected.contains(k)));
        if !self.auto_connect.get_untracked() || self.busy.get_untracked() {
            return None;
        }
        let first = looks == 0;
        let f = d
            .found
            .iter()
            .filter(|f| {
                let k = f.key();
                !connected.contains(&k) && !self.tried.with_value(|t| t.contains(&k)) && (first || before.contains(&k))
            })
            // adb first for each phone.
            .min_by_key(|f| (f.key(), f.transport != "adb"))?
            .clone();
        self.tried.update_value(|t| {
            t.insert(f.key());
        });
        Some((f, !first))
    }

    /// Connects a device found on USB; `show` opens it in the active pane.
    pub(super) fn connect(self, f: UsbDevice, show: bool) {
        self.busy.set(true);
        self.connecting.set(Some(f.usb.clone()));
        self.conn_error.set(None);
        spawn_local(async move {
            match call::<Info>("connect", json!({ "transport": f.transport, "which": f.which, "serial": f.serial })).await {
                Ok(info) => {
                    self.failed.set_value(None);
                    self.connected(info, show);
                }
                Err(e) => {
                    self.failed.set_value(Some(f));
                    self.conn_error.set(Some(e));
                }
            }
            self.busy.set(false);
            self.connecting.set(None);
            self.scan();
        });
    }

    /// Lets go of a device. Panes showing it go back to the Mac.
    pub(super) fn disconnect(self, src: Src) {
        self.disconnect_then(src, None);
    }

    /// Connects a connected phone the other way (adb or MTP).
    pub(super) fn switch(self, src: Src, to: UsbDevice) {
        self.disconnect_then(src, Some(to));
    }

    pub(super) fn disconnect_then(self, src: Src, then: Option<UsbDevice>) {
        if self.editor.with_untracked(|e| e.as_ref().is_some_and(|e| e.src == src)) && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        spawn_local(async move {
            let _ = call::<()>("disconnect", json!({ "src": src.key() })).await;
            let id = src.key();
            self.phones.update(|ps| ps.retain(|i| i.id != id));
            if self.editor.with_untracked(|e| e.as_ref().is_some_and(|e| e.src == src)) {
                self.leave_editor();
            }
            for p in self.panes {
                p.last.update_value(|l| {
                    l.remove(&src);
                });
                if p.src.get_untracked() == src {
                    p.entries.set(vec![]);
                    p.cols.set(HashMap::new());
                    p.cwd.set(String::new());
                    p.selected.set(None);
                    p.notice.set(Notice::None);
                    p.src.set(Src::Mac);
                    if self.split.get_untracked() || p.id == 0 {
                        self.start(p);
                    }
                }
            }
            match then {
                Some(f) => self.connect(f, true),
                None => self.scan(),
            }
        });
    }
}

/// Devices on USB that aren't connected yet, each with a button per way to connect. A phone with
/// both USB debugging and File transfer on shows up once for adb and once for MTP; when exactly one
/// of each is found they're taken to be the same phone and shown as one row.
#[component]
pub(super) fn DeviceFinder() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || {
        // With Try again, when the device that failed is still there.
        let error = move || {
            ctx.conn_error.get().map(|e| {
                let again = ctx.failed.get_value().filter(|f| ctx.devices.with(|d| d.as_ref().is_some_and(|d| d.found.contains(f))));
                view! {
                    <div class="conn-error">
                        <p class="error">{e}</p>
                        {again.map(|f| view! {
                            <button class="btn small" disabled=move || ctx.busy.get()
                                on:click=move |_| ctx.connect(f.clone(), true)>"Try again"</button>
                        })}
                        <button class="btn small" on:click=move |_| ctx.conn_error.set(None)>"Dismiss"</button>
                    </div>
                }
            })
        };
        let Some(d) = ctx.devices.get() else {
            return view! { <p class="hint">"Looking for devices…"</p> }.into_any();
        };
        // A connected phone found the other way is offered as a switch on its own row instead.
        let connected = ctx.phones.with(|ps| ps.iter().flat_map(|i| [i.serial.clone(), i.usb.clone()]).flatten().collect::<HashSet<_>>());
        let found: Vec<UsbDevice> = d.found.iter().filter(|f| !connected.contains(&f.key())).cloned().collect();
        let waiting = d.adb_waiting.then(|| view! { <p class="hint">"A phone is waiting: allow USB debugging on it to use adb."</p> });
        if found.is_empty() {
            if !ctx.phones.with(|p| p.is_empty()) {
                return view! { {waiting} {error} }.into_any();
            }
            let mut notes = vec!["No phone found. Plug one in by USB, then allow USB debugging (for adb) or set the USB mode to File transfer (for MTP).".to_string()];
            if !d.adb_found {
                notes.push("adb isn't installed: brew install --cask android-platform-tools".into());
            }
            notes.extend(d.mtp_error.clone());
            return view! {
                {notes.into_iter().map(|n| view! { <p class="hint">{n}</p> }).collect_view()}
                {waiting}
                {error}
            }.into_any();
        }
        // (name, adb, mtp) per row: one row per phone, by serial number.
        let mut rows: Vec<(String, Option<UsbDevice>, Option<UsbDevice>)> = vec![];
        for f in found {
            let row = rows.iter_mut().find(|(_, a, m)| a.iter().chain(m.iter()).any(|g| g.serial.is_some() && g.serial == f.serial));
            let row = match row {
                Some(r) => r,
                None => {
                    rows.push((f.name.clone(), None, None));
                    rows.last_mut().expect("just pushed")
                }
            };
            if f.transport == "adb" {
                row.1 = Some(f);
            } else {
                row.0 = f.name.clone();
                row.2 = Some(f);
            }
        }
        // Without serials, one adb phone and one MTP phone are most likely the same.
        if let [(_, Some(_), None), (_, None, Some(m))] | [(_, None, Some(m)), (_, Some(_), None)] = rows.as_slice() {
            if m.serial.is_none() || rows.iter().any(|r| r.1.as_ref().is_some_and(|a| a.serial.is_none())) {
                let m = m.clone();
                let a = rows.iter().find_map(|r| r.1.clone());
                rows = vec![(m.name.clone(), a, Some(m))];
            }
        }
        let adb_off = if d.adb_found { "Not available: allow USB debugging on the phone" } else { "Not available: adb isn't installed" };
        view! {
            {rows.into_iter().map(|(name, a, m)| {
                let ids: Vec<String> = a.iter().chain(m.iter()).map(|f| f.usb.clone()).collect();
                let connecting = move || ctx.connecting.get().is_some_and(|u| ids.contains(&u));
                let adb_title = if a.is_some() { "Connect with adb (USB debugging): faster, and can create files at the top level" } else { adb_off };
                let mtp_title = if m.is_some() { "Connect with MTP (File transfer). Close Android File Transfer first: only one app can use a device's MTP at a time." } else { "Not available: set the phone's USB mode to File transfer" };
                let (has_a, has_m) = (a.is_some(), m.is_some());
                let tip = name.clone();
                view! {
                    <div class="found">
                        <b title=tip>{name}</b>
                        {move || if connecting() {
                            view! { <span class="hint"><span class="spin"></span>" Connecting…"</span> }.into_any()
                        } else {
                            let (a, m) = (a.clone(), m.clone());
                            view! {
                                <div class="seg connect-as" role="group" aria-label="Connect with">
                                    <button title=adb_title disabled=move || !has_a || ctx.busy.get() class:pref=has_a
                                        on:click=move |_| if let Some(f) = a.clone() { ctx.connect(f, true) }>"adb"</button>
                                    <button title=mtp_title disabled=move || !has_m || ctx.busy.get() class:pref=has_m && !has_a
                                        on:click=move |_| if let Some(f) = m.clone() { ctx.connect(f, true) }>"MTP"</button>
                                </div>
                            }.into_any()
                        }}
                    </div>
                }
            }).collect_view()}
            {waiting}
            {error}
        }.into_any()
    }
}
