//! Devices: finding them on USB and the Mac’s disks, connecting and disconnecting.

use super::*;

/// A device on USB that isn't connected yet.
#[derive(Serialize)]
pub(super) struct Found {
    /// "adb:SERIAL" or "mtp:BUS-DEV".
    usb: String,
    transport: &'static str,
    /// What `connect` takes as `which`: the adb serial or the MTP location.
    which: String,
    name: String,
    /// The USB serial number when known; an adb device and an MTP device with the same one are
    /// the same phone.
    serial: Option<String>,
}

#[derive(Serialize)]
pub(super) struct Devices {
    found: Vec<Found>,
    adb_found: bool,
    mtp_error: Option<String>,
    /// A phone is on USB with adb but hasn't allowed this Mac yet.
    adb_waiting: bool,
    /// Disks mounted on the Mac (USB drives, SD cards, disk images), as (name, path).
    drives: Vec<(String, String)>,
}

#[derive(Serialize)]
pub(super) struct Info {
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

pub(super) fn info_of(id: &str, c: Option<&Conn>, d: &Device) -> Info {
    Info {
        id: id.to_string(),
        usb: c.and_then(|c| c.usb.clone()),
        serial: c.and_then(|c| c.serial.clone()),
        label: d.label(),
        transport: d.kind(),
        default_dir: d.default_dir(),
        quick_dirs: d.quick_dirs(),
        backup_dir: device::backup_dir().display().to_string(),
    }
}

/// Mounted disks other than the Mac's own startup disk.
pub(super) fn drives() -> Vec<(String, String)> {
    let root = std::fs::canonicalize("/").ok();
    let mut out: Vec<(String, String)> = std::fs::read_dir("/Volumes")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("com.apple.") {
                return None;
            }
            let path = e.path();
            // The startup disk shows in /Volumes as a link to "/".
            if std::fs::canonicalize(&path).ok() == root || !path.is_dir() {
                return None;
            }
            Some((name, path.display().to_string()))
        })
        .collect();
    out.sort();
    out
}

/// Devices on USB that aren't connected yet, and the Mac's mounted disks.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn devices(state: State<'_, AppState>) -> Result<Devices, String> {
    // A connected phone can still show here the other way (same serial), so the app can offer to
    // switch; `connect` refuses to connect it twice.
    let taken: Vec<Option<String>> = state.devs().values().map(|c| c.usb.clone()).collect();
    blocking(move || {
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
                let usb = if v.is_empty() { vec![] } else { usb::list() };
                found.extend(v.into_iter().map(|d| Found {
                    usb: format!("mtp:{}", d.location),
                    transport: "mtp",
                    serial: usb::serial_of(&usb, d.bus, d.devnum, d.vendor_id, d.product_id),
                    which: d.location,
                    name: d.name,
                }));
                None
            }
            Err(e) => Some(e.to_string()),
        };
        found.retain(|f| !taken.contains(&Some(f.usb.clone())));
        let adb_waiting = !adb::unauthorized().is_empty();
        Ok(Devices { found, adb_found: adb::adb_path().is_some(), mtp_error, adb_waiting, drives: drives() })
    })
    .await
}

/// Connects a device found by `devices` (`which` is its adb serial or MTP location, `serial` its
/// USB serial number when known), alongside any already connected.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn connect(
    state: State<'_, AppState>,
    transport: String,
    which: Option<String>,
    serial: Option<String>,
) -> Result<Info, String> {
    let usb = which.as_ref().map(|w| format!("{transport}:{w}"));
    let serial = serial.or_else(|| which.clone().filter(|_| transport == "adb"));
    let same = |c: &Conn| (usb.is_some() && c.usb == usb) || (serial.is_some() && c.serial == serial);
    if state.devs().values().any(same) {
        return Err("That device is already connected.".into());
    }
    let dev = blocking(move || device::connect(&transport, which).map_err(msg)).await?;
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn { dev: Arc::new(dev), usb, serial };
    let info = info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[tauri::command(rename_all = "snake_case")]
pub(super) fn disconnect(state: State<'_, AppState>, src: String) {
    state.devs().remove(&src);
    state.viewing.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (s, _)| *s != src);
}

/// The connected devices, oldest first (a reloaded window picks them up from here).
#[tauri::command(rename_all = "snake_case")]
pub(super) fn connections(state: State<'_, AppState>) -> Vec<Info> {
    let devs = state.devs();
    let mut out: Vec<Info> = devs.iter().map(|(id, c)| info_of(id, Some(c), &c.dev)).collect();
    out.sort_by_key(|i| i.id[1..].parse::<u32>().unwrap_or(0));
    out
}

/// A connected device, or with `src: "mac"`, the Mac's own files.
#[tauri::command(rename_all = "snake_case")]
pub(super) fn info(state: State<'_, AppState>, src: Option<String>) -> Option<Info> {
    match src.as_deref() {
        Some("mac") => Some(info_of("mac", None, &state.mac)),
        Some(id) => state.devs().get(id).map(|c| info_of(id, Some(c), &c.dev)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn drives_skip_the_startup_disk() {
        let root = std::fs::canonicalize("/").unwrap();
        for (name, path) in super::drives() {
            assert!(!name.starts_with('.'), "{name}");
            assert_ne!(std::fs::canonicalize(&path).unwrap(), root, "{path}");
        }
    }
}
