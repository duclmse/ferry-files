//! Devices: finding them on USB and the computer’s disks, connecting and disconnecting.

use super::{adb, mtp};
use crate::*;

/// A device on USB that isn't connected yet.
#[derive(Serialize)]
pub(crate) struct Found {
    /// "adb:SERIAL", "mtp:BUS-DEV" or "iphone:UDID".
    usb: String,
    transport: &'static str,
    /// What `connect` takes as `which`: the adb serial, the MTP location or the iPhone's UDID.
    which: String,
    name: String,
    /// The USB serial number when known; an adb device and an MTP device with the same one are
    /// the same phone.
    serial: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct Devices {
    found: Vec<Found>,
    adb_found: bool,
    mtp_error: Option<String>,
    /// Why iPhones can't be looked for (Apple's device service is missing).
    iphone_error: Option<String>,
    /// A phone is on USB with adb but hasn't allowed this computer yet.
    adb_waiting: bool,
    /// Disks mounted on the computer (USB drives, SD cards, disk images), as (name, path).
    drives: Vec<(String, String)>,
    /// The paths of those that can be ejected (not the disks inside the computer).
    ejectable: Vec<String>,
    /// Connected devices that went from USB (unplugged, or the Mac slept), now disconnected, as
    /// (id, name).
    gone: Vec<(String, String)>,
}

#[derive(Serialize)]
pub(crate) struct Info {
    /// "mac", or the id ("d1", ...) that commands take as `src`.
    id: String,
    usb: Option<String>,
    serial: Option<String>,
    label: String,
    transport: &'static str,
    default_dir: String,
    quick_dirs: Vec<(String, String)>,
    backup_dir: String,
}

pub(crate) fn info_of(id: &str, c: Option<&Conn>, d: &Device) -> Info {
    Info {
        id: id.to_string(),
        usb: c.and_then(|c| c.usb.clone()),
        serial: c.and_then(|c| c.serial.clone()),
        label: d.label(),
        transport: d.kind(),
        default_dir: d.default_dir(),
        quick_dirs: d.quick_dirs(),
        backup_dir: crate::platform::app_path(&device::backup_dir()),
    }
}

/// Disks other than the one the system is on.
pub(crate) fn drives() -> Vec<(String, String)> {
    crate::platform::drives()
}

/// Whether a connected device is still there: an adb one while adb lists it (in any state, and
/// over Wi-Fi too), an MTP one while its serial number is on USB, an iPhone while usbmuxd lists
/// it. `None` when that can't be told (adb or usbmuxd didn't answer, the USB devices couldn't be
/// listed, or the device has no serial number).
fn still_there(
    c: &Conn,
    adb: &Option<Vec<String>>,
    usb: &Option<Vec<crate::devices::usb::UsbDev>>,
    iphones: &Option<Vec<String>>,
) -> Option<bool> {
    let place = c.usb.as_deref()?;
    if let Some(serial) = place.strip_prefix("adb:") {
        return adb.as_ref().map(|l| l.iter().any(|s| s == serial));
    }
    if let Some(udid) = place.strip_prefix("iphone:") {
        return iphones.as_ref().map(|l| l.iter().any(|s| s == udid));
    }
    // Windows' MTP devices aren't in `usb::try_list`, which then gives nothing.
    if place.starts_with("mtp:") && !cfg!(windows) {
        let serial = c.serial.as_deref()?;
        return usb.as_ref().map(|l| l.iter().any(|d| d.serial == serial));
    }
    None
}

/// Disconnects the connected devices that have gone from USB: missing at two looks in a row,
/// since adb loses a phone for a moment now and then. Returns them as (id, name).
fn drop_gone(app: &AppHandle) -> Vec<(String, String)> {
    let st = state(app);
    let (adb, usb) = (adb::listed(), crate::devices::usb::try_list());
    let any_iphone = st
        .devs()
        .values()
        .any(|c| c.usb.as_deref().is_some_and(|u| u.starts_with("iphone:")));
    let iphones = if any_iphone {
        crate::devices::iphone::listed()
    } else {
        None
    };
    // Named after the locks are let go of: a name may take asking the device.
    let looked: Vec<(String, Option<bool>, Arc<Device>)> = st
        .devs()
        .iter()
        .map(|(id, c)| {
            (
                id.clone(),
                still_there(c, &adb, &usb, &iphones),
                c.dev.clone(),
            )
        })
        .collect();
    let gone: Vec<(String, Arc<Device>)> = {
        let mut missing = st.missing.lock().unwrap_or_else(|e| e.into_inner());
        let was = std::mem::take(&mut *missing);
        looked
            .into_iter()
            .filter(|(id, there, _)| {
                let lost = *there == Some(false);
                if lost && !was.contains(id) {
                    missing.insert(id.clone());
                }
                lost && was.contains(id)
            })
            .map(|(id, _, d)| (id, d))
            .collect()
    };
    let gone: Vec<(String, String)> = gone.into_iter().map(|(id, d)| (id, d.label())).collect();
    if !gone.is_empty() {
        st.close(&gone.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>());
    }
    gone
}

/// Devices on USB that aren't connected yet, and the computer's mounted disks. Connected devices
/// no longer on USB are disconnected (`gone`).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn devices(app: AppHandle, state: State<'_, AppState>) -> Result<Devices, String> {
    // A connected phone can still show here the other way (same serial), so the app can offer to
    // switch; `connect` refuses to connect it twice.
    let taken: Vec<Option<String>> = state.devs().values().map(|c| c.usb.clone()).collect();
    blocking(move || {
        let gone = drop_gone(&app);
        let mut found: Vec<Found> = adb::ready_devices()
            .into_iter()
            .map(|(serial, name)| Found {
                usb: format!("adb:{serial}"),
                transport: "adb",
                which: serial.clone(),
                name,
                serial: Some(serial),
            })
            .collect();
        let mtp_error = match mtp::detect() {
            Ok(v) => {
                found.extend(v.into_iter().map(|d| Found {
                    usb: format!("mtp:{}", d.location),
                    transport: "mtp",
                    serial: d.serial,
                    which: d.location,
                    name: d.name,
                }));
                None
            }
            Err(e) => Some(e.to_string()),
        };
        let iphone_error = match crate::devices::iphone::detect() {
            Ok(v) => {
                found.extend(v.into_iter().map(|d| Found {
                    usb: format!("iphone:{}", d.udid),
                    transport: "iphone",
                    which: d.udid.clone(),
                    name: d.name,
                    serial: Some(d.udid),
                }));
                None
            }
            Err(e) => Some(e.to_string()),
        };
        found.retain(|f| !taken.contains(&Some(f.usb.clone())));
        let adb_waiting = !adb::unauthorized().is_empty();
        let drives = drives();
        let ejectable = drives
            .iter()
            .map(|(_, p)| p.clone())
            .filter(|p| crate::platform::ejectable(p))
            .collect();
        Ok(Devices {
            found,
            adb_found: adb::adb_path().is_some(),
            mtp_error,
            iphone_error,
            adb_waiting,
            drives,
            ejectable,
            gone,
        })
    })
    .await
}

/// Connects a device found by `devices` (`which` is its adb serial or MTP location, `serial` its
/// USB serial number when known), alongside any already connected.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn connect(
    state: State<'_, AppState>,
    transport: String,
    which: Option<String>,
    serial: Option<String>,
) -> Result<Info, String> {
    let usb = which.as_ref().map(|w| format!("{transport}:{w}"));
    let serial = serial.or_else(|| which.clone().filter(|_| transport == "adb"));
    let same =
        |c: &Conn| (usb.is_some() && c.usb == usb) || (serial.is_some() && c.serial == serial);
    if state.devs().values().any(same) {
        return Err("That device is already connected.".into());
    }
    let dev = blocking(move || device::connect(&transport, which).map_err(msg)).await?;
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn {
        dev: Arc::new(dev),
        usb,
        serial,
    };
    let info = info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

/// Returns once the device is let go of, so connecting it again (the other way too) right after
/// finds it free.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn disconnect(app: AppHandle, src: String) -> Result<(), String> {
    blocking(move || {
        state(&app).close(&[src]);
        Ok(())
    })
    .await
}

/// The connected devices, oldest first (a reloaded window picks them up from here).
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn connections(state: State<'_, AppState>) -> Vec<Info> {
    let devs = state.devs();
    let mut out: Vec<Info> = devs
        .iter()
        .map(|(id, c)| info_of(id, Some(c), &c.dev))
        .collect();
    out.sort_by_key(|i| i.id[1..].parse::<u32>().unwrap_or(0));
    out
}

/// Starts telling the window ("devices-changed") when a USB device or disk comes or goes. Whether
/// USB devices are watched; if not, the window keeps looking every few seconds.
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn watch_devices(app: AppHandle) -> bool {
    crate::devices::watch::start(&app)
}

/// A connected device, or with `src: "mac"`, the Mac's own files.
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn info(state: State<'_, AppState>, src: Option<String>) -> Option<Info> {
    match src.as_deref() {
        Some("mac") => Some(info_of("mac", None, &state.mac)),
        Some(id) => state.devs().get(id).map(|c| info_of(id, Some(c), &c.dev)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_when_a_connected_device_is_gone() {
        use crate::devices::usb::UsbDev;
        let conn = |usb: &str, serial: Option<&str>| Conn {
            dev: Arc::new(Device::new(Box::new(crate::local::Local))),
            usb: Some(usb.into()),
            serial: serial.map(String::from),
        };
        let on_usb = |serial: &str| UsbDev {
            bus: 0,
            address: 1,
            vendor: 0x04e8,
            product: 0x6860,
            serial: serial.into(),
        };
        let adb = Some(vec![
            "R3CR406TEMA".to_string(),
            "192.168.1.5:5555".to_string(),
        ]);
        let usb = Some(vec![on_usb("R3CR406TEMA")]);
        let phones = Some(vec!["00008110-001A2B3C4D5E801E".to_string()]);
        let there = |c: &Conn| still_there(c, &adb, &usb, &phones);
        assert_eq!(
            there(&conn("adb:R3CR406TEMA", Some("R3CR406TEMA"))),
            Some(true)
        );
        assert_eq!(there(&conn("adb:192.168.1.5:5555", None)), Some(true));
        assert_eq!(there(&conn("adb:OTHER", Some("OTHER"))), Some(false));
        if !cfg!(windows) {
            assert_eq!(there(&conn("mtp:0-1", Some("R3CR406TEMA"))), Some(true));
            // Plugged in again elsewhere is still there: the serial is what counts.
            assert_eq!(there(&conn("mtp:0-9", Some("R3CR406TEMA"))), Some(true));
            assert_eq!(there(&conn("mtp:0-2", Some("OTHER"))), Some(false));
        }
        // Can't tell: no serial, or adb or the USB list not answering.
        assert_eq!(there(&conn("mtp:0-2", None)), None);
        assert_eq!(
            still_there(&conn("adb:R3CR406TEMA", None), &None, &usb, &phones),
            None
        );
        assert_eq!(
            still_there(&conn("mtp:0-1", Some("R3CR406TEMA")), &adb, &None, &phones),
            None
        );
        assert_eq!(
            there(&conn("iphone:00008110-001A2B3C4D5E801E", None)),
            Some(true)
        );
        assert_eq!(
            there(&conn("iphone:00008030-000000000000002E", None)),
            Some(false)
        );
        assert_eq!(
            still_there(
                &conn("iphone:00008110-001A2B3C4D5E801E", None),
                &adb,
                &usb,
                &None
            ),
            None
        );
    }

    #[test]
    fn drives_are_disks_in_order() {
        let drives = super::drives();
        assert!(drives.windows(2).all(|w| w[0].1 <= w[1].1), "{drives:?}");
        for (name, path) in &drives {
            assert!(!name.starts_with('.'), "{name}");
            assert!(crate::platform::native(path).is_dir(), "{path}");
        }
        if cfg!(windows) {
            // Every drive, the system's too, which is never one to eject.
            let system = format!(
                "/{}",
                std::env::var("SystemDrive")
                    .unwrap_or_else(|_| "C:".into())
                    .to_uppercase()
            );
            assert!(drives.iter().any(|(_, p)| *p == system), "{drives:?}");
            assert!(!crate::platform::ejectable(&system));
        } else {
            // Not the disk the system is on.
            let root = std::fs::canonicalize("/").unwrap();
            for (_, path) in &drives {
                assert_ne!(
                    std::fs::canonicalize(crate::platform::native(path)).unwrap(),
                    root,
                    "{path}"
                );
            }
        }
    }
}
