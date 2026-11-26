//! When another app holds the phone (on macOS, Image Capture's helpers), and making it let go.

use super::*;

/// Image Capture's helpers, which take a phone's MTP interface (to import photos) as soon as
/// something asks for it, and start again by themselves when stopped.
#[cfg(target_os = "macos")]
pub(super) const CAMERA_HELPERS: [&str; 3] = ["ptpcamerad", "icdd", "mscamerad-xpc"];

/// Apps that keep Image Capture's helpers on a phone while they're open: stopping the helpers
/// is no use then, as they're back at once.
#[cfg(target_os = "macos")]
pub(super) const CAMERA_APPS: [&str; 2] = ["Image Capture", "Photos"];

/// Which of `CAMERA_APPS` are running.
#[cfg(target_os = "macos")]
pub(super) fn camera_apps_open() -> Vec<&'static str> {
    CAMERA_APPS
        .into_iter()
        .filter(|app| {
            crate::platform::command("pgrep")
                .args(["-xq", app])
                .stdin(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect()
}

/// What to tell someone about another program holding the phone.
#[cfg(target_os = "macos")]
pub(super) fn held_message(h: &crate::devices::usb::Holder) -> String {
    if CAMERA_HELPERS.contains(&h.name.as_str()) {
        match camera_apps_open().as_slice() {
            [] => format!("Image Capture's helper ({}) keeps taking the phone: quit Image Capture and Photos, then connect again.", h.name),
            apps => format!("{} is open and has the phone: quit it, then connect again.", apps.join(" and ")),
        }
    } else if h.name == "ferry-files" {
        format!(
            "Another Ferry Files (pid {}) has the phone: disconnect it there, or quit it.",
            h.pid
        )
    } else {
        format!(
            "{} (pid {}) is using the phone: quit it, then connect again.",
            h.name, h.pid
        )
    }
}

/// Gets the USB device at (bus, address) free of other programs before libmtp claims it: Image
/// Capture's helpers are stopped (for up to 3 s, as they come back), unless an app that brings them
/// straight back is open; anything else is an error naming it. When ioreg can't tell, it's tried
/// anyway.
#[cfg(target_os = "macos")]
pub(super) fn make_way(bus: u32, address: u32, vendor: u16, product: u16) -> Result<()> {
    let me = std::process::id();
    for _ in 0..15 {
        let Some(held) = crate::devices::usb::holders(bus, address, vendor, product) else {
            return Ok(());
        };
        let others: Vec<_> = held.into_iter().filter(|h| h.pid != me).collect();
        if others.is_empty() {
            return Ok(());
        }
        if let Some(h) = others
            .iter()
            .find(|h| !CAMERA_HELPERS.contains(&h.name.as_str()))
        {
            return err(held_message(h));
        }
        let apps = camera_apps_open();
        if !apps.is_empty() {
            return err(format!(
                "{} is open and has the phone: quit it, then connect again.",
                apps.join(" and ")
            ));
        }
        // They ignore SIGTERM.
        for h in &others {
            let _ = crate::platform::command("kill")
                .args(["-9", &h.pid.to_string()])
                .stdin(std::process::Stdio::null())
                .status();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    err("Image Capture keeps taking the phone (ptpcamerad): quit Image Capture and Photos, then connect again.".to_string())
}

#[cfg(not(target_os = "macos"))]
pub(super) fn make_way(_bus: u32, _address: u32, _vendor: u16, _product: u16) -> Result<()> {
    Ok(())
}

/// Why opening the USB device at (bus, address) failed, when ioreg tells: another program took it
/// (Image Capture's helper, back between `make_way` and the open), or this process still holds it
/// after libmtp failed to open it.
#[cfg(target_os = "macos")]
pub(super) fn why_not_open(bus: u32, address: u32, vendor: u16, product: u16) -> Option<String> {
    let held = crate::devices::usb::holders(bus, address, vendor, product)?;
    let me = std::process::id();
    if let Some(h) = held.iter().find(|h| h.pid != me) {
        return Some(held_message(h));
    }
    held.iter().any(|h| h.pid == me).then(|| {
        "Couldn't open the phone over MTP. Unplug it, plug it in again, unlock it and choose File transfer.".to_string()
    })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn why_not_open(
    _bus: u32,
    _address: u32,
    _vendor: u16,
    _product: u16,
) -> Option<String> {
    None
}
