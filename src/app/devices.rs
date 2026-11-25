//! Devices: connecting, disconnecting, looking for them on USB, and the finder in the sidebar.

use super::*;

/// How often a phone whose files aren't allowed yet is connected again.
const ACCESS_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether connecting failed because the phone's storage couldn't be read: it's locked, or it
/// hasn't been allowed to share its files yet.
fn needs_access(e: &str) -> bool {
    e.contains("Reading the device's storage")
        || e.contains("shows no storage")
        || e.contains("tap Trust")
}

impl Ctx {
    /// A device is connected. With `show`, the active pane goes to it.
    pub(super) fn connected(self, info: Info, show: bool) {
        let src = Src::of(&info.id);
        let fresh = self
            .phones
            .with_untracked(|ps| !ps.iter().any(|i| i.id == info.id));
        if fresh {
            self.phones.update(|ps| ps.push(info.clone()));
            self.tabs_back(&info);
        }
        self.conn_error.set(None);
        if show {
            self.open_remembered(self.act(), src);
        }
    }

    /// Looks for devices on USB and disks on the Mac. When the app is busy or a look is under way,
    /// it's done later.
    pub(super) fn scan(self) {
        if self.busy.get_untracked() || self.scanning.get_value() || self.running() {
            self.scan_again.set_value(true);
            return;
        }
        self.scan_again.set_value(false);
        self.scanning.set_value(true);
        spawn_local(async move {
            if let Ok(d) = call::<Devices>("devices", json!({})).await {
                for (id, name) in &d.gone {
                    self.forget(Src::of(id));
                    self.conn_error.set(Some(format!(
                        "{name} is no longer on USB, so it was disconnected."
                    )));
                }
                if self
                    .awaiting
                    .with_untracked(|w| w.as_ref().is_some_and(|w| !d.found.contains(w)))
                {
                    self.stop_awaiting();
                }
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
        self.phones.with_untracked(|ps| {
            ps.iter()
                .flat_map(|i| [i.serial.clone(), i.usb.clone()])
                .flatten()
                .collect()
        })
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
        self.tried.update_value(|t| {
            t.retain(|k| now.contains(k) || before.contains(k) || connected.contains(k))
        });
        if !self.auto_connect.get_untracked() || self.busy.get_untracked() {
            return None;
        }
        let first = looks == 0;
        let f = d
            .found
            .iter()
            .filter(|f| {
                let k = f.key();
                !connected.contains(&k)
                    && !self.tried.with_value(|t| t.contains(&k))
                    && (first || before.contains(&k))
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
        // The last error stays until this one's done: an automatic connect (a phone that came back
        // at another USB address) mustn't wipe it before it's been read.
        spawn_local(async move {
            match call::<Info>(
                "connect",
                json!({ "transport": f.transport, "which": f.which, "serial": f.serial }),
            )
            .await
            {
                Ok(info) => {
                    self.stop_awaiting_for(&f);
                    self.failed.set_value(None);
                    self.connected(info, show);
                }
                Err(e) if needs_access(&e) => {
                    self.failed.set_value(Some(f.clone()));
                    self.conn_error.set(None);
                    self.await_access(f, show);
                }
                Err(e) => {
                    self.stop_awaiting_for(&f);
                    self.failed.set_value(Some(f));
                    self.conn_error.set(Some(e));
                }
            }
            self.busy.set(false);
            self.connecting.set(None);
            self.scan();
        });
    }

    /// Connects `f` again in `ACCESS_RETRY`, while it's still on USB: the phone may be waiting for
    /// someone to unlock it or allow access to its files.
    fn await_access(self, f: UsbDevice, show: bool) {
        self.stop_awaiting();
        self.awaiting.set(Some(f.clone()));
        let h = set_timeout_with_handle(
            move || {
                let _ = self.access_timer.try_set_value(None);
                if self
                    .awaiting
                    .try_with_untracked(|w| w.as_ref() != Some(&f))
                    .unwrap_or(true)
                {
                    return;
                }
                if !self
                    .devices
                    .with_untracked(|d| d.as_ref().is_some_and(|d| d.found.contains(&f)))
                {
                    self.stop_awaiting();
                } else if self.busy.get_untracked() {
                    self.await_access(f, show);
                } else {
                    self.connect(f, show);
                }
            },
            ACCESS_RETRY,
        );
        self.access_timer.set_value(h.ok());
    }

    /// Stops connecting `f` again, if it's the phone being connected again.
    fn stop_awaiting_for(self, f: &UsbDevice) {
        if self.awaiting.with_untracked(|w| w.as_ref() == Some(f)) {
            self.stop_awaiting();
        }
    }

    /// Stops connecting a phone again (see `await_access`).
    pub(super) fn stop_awaiting(self) {
        if let Some(h) = self.access_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        if self.awaiting.with_untracked(Option::is_some) {
            self.awaiting.set(None);
        }
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
        if self
            .editor
            .with_untracked(|e| e.as_ref().is_some_and(|e| e.src == src))
            && self.dirty()
        {
            self.confirm_discard.set(true);
            return;
        }
        spawn_local(async move {
            // Done once the device is let go of, so connecting it again (`then`) can claim it.
            let _ = call::<()>("disconnect", json!({ "src": src.key() })).await;
            self.forget(src);
            match then {
                Some(f) => self.connect(f, true),
                None => self.scan(),
            }
        });
    }

    /// Takes a device that's been disconnected out of the window: panes showing it go back to the
    /// Mac. An editor on it with unsaved changes stays open, so they can still be copied out.
    fn forget(self, src: Src) {
        let id = src.key();
        self.phones.update(|ps| ps.retain(|i| i.id != id));
        if self
            .editor
            .with_untracked(|e| e.as_ref().is_some_and(|e| e.src == src))
            && !self.dirty()
        {
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
    }
}

/// Devices on USB that aren't connected yet, each with a button per way to connect. A phone with
/// both USB debugging and File transfer on shows up once for adb and once for MTP; when exactly one
/// of each is found they're taken to be the same phone and shown as one row.
#[component]
pub(super) fn DeviceFinder() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let auto = AutoDismiss::new(
        move || ctx.conn_error.with(Option::is_some),
        move || ctx.conn_error.set(None),
    );
    move || {
        // With Try again, when the device that failed is still there.
        let error = move || {
            ctx.conn_error.get().map(|e| {
                let again = ctx.failed.get_value().filter(|f| ctx.devices.with(|d| d.as_ref().is_some_and(|d| d.found.contains(f))));
                view! {
                    <div class="conn-error" on:mouseover=move |_| auto.hover() on:mouseleave=move |_| auto.leave()>
                        <p class="error">{e}</p>
                        {again.map(|f| view! {
                            <button class="btn small" disabled=move || ctx.busy.get()
                                on:click=move |_| ctx.connect(f.clone(), true)>{ico(Icon::Refresh)}"Try again"</button>
                        })}
                        <button class="btn small icon-only" title="Dismiss" aria-label="Dismiss" on:click=move |_| ctx.conn_error.set(None)>{ico(Icon::Close)}</button>
                    </div>
                }
            })
        };
        // A phone that's connected again until it lets its files be read.
        let access = move || {
            ctx.awaiting.get().map(|f| {
                view! {
                    <div class="conn-error">
                        <p class="hint">{if f.transport == "iphone" {
                            format!("{} doesn't trust {} yet. Unlock it and tap Trust; trying again every 5 seconds.", f.name, crate::os::this_computer())
                        } else {
                            format!("{} won't share its files yet. Unlock it and allow access to its data; trying again every 5 seconds.", f.name)
                        }}</p>
                        <button class="btn small" on:click=move |_| ctx.stop_awaiting()>"Stop"</button>
                    </div>
                }
            })
        };
        let Some(d) = ctx.devices.get() else {
            return view! { <p class="hint">"Looking for devices…"</p> }.into_any();
        };
        // A connected phone found the other way is offered as a switch on its own row instead.
        let connected = ctx.phones.with(|ps| {
            ps.iter()
                .flat_map(|i| [i.serial.clone(), i.usb.clone()])
                .flatten()
                .collect::<HashSet<_>>()
        });
        let (iphones, found): (Vec<UsbDevice>, Vec<UsbDevice>) = d
            .found
            .iter()
            .filter(|f| !connected.contains(&f.key()))
            .cloned()
            .partition(|f| f.transport == "iphone");
        // iPhones connect one way only: a row with one button.
        let iphones = iphones
            .into_iter()
            .map(|f| {
                let usb = f.usb.clone();
                let connecting = move || ctx.connecting.get().as_ref() == Some(&usb);
                let (name, tip) = (f.name.clone(), f.name.clone());
                view! {
                    <div class="found">
                        <b title=tip>{name}</b>
                        {move || if connecting() {
                            view! { <span class="hint"><span class="spin"></span>" Connecting…"</span> }.into_any()
                        } else {
                            let f = f.clone();
                            view! {
                                <div class="seg connect-as">
                                    <button class="pref" title="Connect to the iPhone: its photos, downloads and apps' shared files"
                                        aria-label="Connect" disabled=move || ctx.busy.get()
                                        on:click=move |_| ctx.connect(f.clone(), true)>"Connect"</button>
                                </div>
                            }.into_any()
                        }}
                    </div>
                }
            })
            .collect_view();
        let waiting = d.adb_waiting.then(|| view! { <p class="hint">"A phone is waiting: allow USB debugging on it to use adb."</p> });
        if found.is_empty() {
            if ctx.phones.with(|p| p.iter().any(Info::is_phone))
                || d.found.iter().any(|f| f.transport == "iphone")
            {
                return view! { {iphones} {waiting} {access} {error} }.into_any();
            }
            let mut notes =
                vec!["No phone found. Plug one in by USB: an Android phone with debugging (adb) or File transfer mode (MTP), or an iPhone.".to_string()];
            if !d.adb_found {
                notes.push(format!("adb isn't installed. {}", crate::os::adb_install()));
            }
            notes.extend(d.mtp_error.clone());
            notes.extend(d.iphone_error.clone());
            return view! {
                {notes.into_iter().map(|n| view! { <p class="hint">{n}</p> }).collect_view()}
                {waiting}
                {access}
                {error}
            }
            .into_any();
        }
        // (name, adb, mtp) per row: one row per phone, by serial number.
        let mut rows: Vec<(String, Option<UsbDevice>, Option<UsbDevice>)> = vec![];
        for f in found {
            let row = rows.iter_mut().find(|(_, a, m)| {
                a.iter()
                    .chain(m.iter())
                    .any(|g| g.serial.is_some() && g.serial == f.serial)
            });
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
        if let [(_, Some(_), None), (_, None, Some(m))] | [(_, None, Some(m)), (_, Some(_), None)] =
            rows.as_slice()
        {
            if m.serial.is_none()
                || rows
                    .iter()
                    .any(|r| r.1.as_ref().is_some_and(|a| a.serial.is_none()))
            {
                let m = m.clone();
                let a = rows.iter().find_map(|r| r.1.clone());
                rows = vec![(m.name.clone(), a, Some(m))];
            }
        }
        let adb_off = if d.adb_found {
            "Not available: allow USB debugging on the phone"
        } else {
            "Not available: adb isn't installed"
        };
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
            {iphones}
            {waiting}
            {access}
            {error}
        }.into_any()
    }
}
