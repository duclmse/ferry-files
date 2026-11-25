//! The computer's list of USB devices, with each one's serial number, which adb also uses: so an
//! MTP device and an adb device can be told to be the same phone. From `ioreg` on macOS and
//! /sys/bus/usb on Linux. (On Windows, MTP devices come with their serial; see wpd.rs.)

// `parse` is tested everywhere; the rest is for libmtp, which Windows doesn't use.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[derive(Debug, Clone, PartialEq)]
pub struct UsbDev {
    /// The bus and address libusb (and so libmtp) uses: bus is the top byte of the locationID.
    pub bus: u32,
    pub address: u32,
    pub vendor: u16,
    pub product: u16,
    pub serial: String,
}

pub fn list() -> Vec<UsbDev> {
    try_list().unwrap_or_default()
}

/// The devices on USB that have a serial number, or `None` when they couldn't be looked at (so
/// a phone isn't taken to be gone when it's only that ioreg didn't run).
#[cfg(target_os = "macos")]
pub fn try_list() -> Option<Vec<UsbDev>> {
    let out = crate::platform::command("ioreg")
        .args(["-p", "IOUSB", "-l", "-w0"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Each device in /sys/bus/usb/devices that has a serial: its bus and address are the ones
/// libusb (and so libmtp) reports on Linux.
#[cfg(not(target_os = "macos"))]
pub fn try_list() -> Option<Vec<UsbDev>> {
    let read = |d: &std::path::Path, name: &str| {
        std::fs::read_to_string(d.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let dir = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    let devs = dir
        .flatten()
        .filter_map(|e| {
            let d = e.path();
            let serial = read(&d, "serial").filter(|s| !s.is_empty())?;
            Some(UsbDev {
                bus: read(&d, "busnum")?.parse().ok()?,
                address: read(&d, "devnum")?.parse().ok()?,
                vendor: u16::from_str_radix(&read(&d, "idVendor")?, 16).ok()?,
                product: u16::from_str_radix(&read(&d, "idProduct")?, 16).ok()?,
                serial,
            })
        })
        .collect();
    Some(devs)
}

/// Names of what's on USB now, to notice something plugged in or taken out.
#[cfg(not(target_os = "macos"))]
pub fn snapshot() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/bus/usb/devices")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Reads the `+-o Name@... <class IOUSBHostDevice ...>` entries and the properties under each.
/// USB controllers have a locationID too, so only device entries are kept.
pub(crate) fn parse(text: &str) -> Vec<UsbDev> {
    #[derive(Default)]
    struct Part {
        device: bool,
        location: Option<u32>,
        address: Option<u32>,
        vendor: Option<u16>,
        product: Option<u16>,
        serial: Option<String>,
    }
    fn finish(p: Part, out: &mut Vec<UsbDev>) {
        if let (true, Some(loc), Some(address), Some(serial)) =
            (p.device, p.location, p.address, p.serial)
        {
            out.push(UsbDev {
                bus: loc >> 24,
                address,
                vendor: p.vendor.unwrap_or(0),
                product: p.product.unwrap_or(0),
                serial,
            });
        }
    }
    let mut out = vec![];
    let mut cur = Part::default();
    for line in text.lines() {
        if let Some(rest) = line.split_once("+-o ").map(|(_, r)| r) {
            finish(std::mem::take(&mut cur), &mut out);
            cur.device =
                rest.contains("<class IOUSBHostDevice") || rest.contains("<class IOUSBDevice");
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        let key = key.trim_start_matches([' ', '|']).trim_matches('"');
        let num = || value.trim().parse::<u64>().ok();
        match key {
            "locationID" => cur.location = num().map(|n| n as u32),
            "USB Address" => cur.address = num().map(|n| n as u32),
            "idVendor" => cur.vendor = num().map(|n| n as u16),
            "idProduct" => cur.product = num().map(|n| n as u16),
            "USB Serial Number" | "kUSBSerialNumberString" => {
                let s = value.trim().trim_matches('"').to_string();
                if !s.is_empty() {
                    cur.serial = Some(s);
                }
            }
            _ => {}
        }
    }
    finish(cur, &mut out);
    out
}

/// A program that has a USB device, or one of its interfaces, to itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
}

/// The programs holding the USB device at libusb's (bus, address) or one of its interfaces (an
/// interface is what an MTP session claims), from ioreg; `None` if ioreg couldn't be read. macOS's
/// own drivers, which have no pid, aren't counted. libusb can be behind ioreg on the address (a
/// phone that was reset comes back at another), so failing that it's the only device on the bus
/// with this vendor and product id.
#[cfg(target_os = "macos")]
pub fn holders(bus: u32, address: u32, vendor: u16, product: u16) -> Option<Vec<Holder>> {
    let out = crate::platform::command("ioreg")
        .args(["-r", "-c", "IOUSBHostDevice", "-l", "-w0"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        parse_holders(
            &String::from_utf8_lossy(&out.stdout),
            bus,
            address,
            vendor,
            product,
        )
    })
}

/// `holders` from ioreg's text: each device entry, its properties, then the entries under it
/// (interfaces) with theirs.
pub(crate) fn parse_holders(
    text: &str,
    bus: u32,
    address: u32,
    vendor: u16,
    product: u16,
) -> Vec<Holder> {
    #[derive(Default)]
    struct Dev {
        location: Option<u32>,
        address: Option<u32>,
        vendor: Option<u16>,
        product: Option<u16>,
        holders: Vec<Holder>,
    }
    let mut devs: Vec<Dev> = vec![];
    for line in text.lines() {
        if let Some(rest) = line.split_once("+-o ").map(|(_, r)| r) {
            if rest.contains("<class IOUSBHostDevice") || rest.contains("<class IOUSBDevice") {
                devs.push(Dev::default());
            }
            continue;
        }
        let Some(d) = devs.last_mut() else { continue };
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        let key = key.trim_start_matches([' ', '|']).trim_matches('"');
        let num = || value.trim().parse::<u64>().ok();
        // The device's own properties come first; its interfaces repeat some of them.
        match key {
            "locationID" if d.location.is_none() => d.location = num().map(|n| n as u32),
            "USB Address" if d.address.is_none() => d.address = num().map(|n| n as u32),
            "idVendor" if d.vendor.is_none() => d.vendor = num().map(|n| n as u16),
            "idProduct" if d.product.is_none() => d.product = num().map(|n| n as u16),
            "UsbExclusiveOwner" => {
                // "pid 13235, ptpcamerad"
                let v = value.trim().trim_matches('"');
                if let Some((pid, name)) = v.strip_prefix("pid ").and_then(|r| r.split_once(", ")) {
                    if let Ok(pid) = pid.parse() {
                        let h = Holder {
                            pid,
                            name: name.to_string(),
                        };
                        if !d.holders.contains(&h) {
                            d.holders.push(h);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    devs.retain(|d| d.location.is_some_and(|l| l >> 24 == bus));
    if let Some(i) = devs.iter().position(|d| d.address == Some(address)) {
        return devs.swap_remove(i).holders;
    }
    let mut same = devs
        .into_iter()
        .filter(|d| d.vendor == Some(vendor) && d.product == Some(product));
    match (same.next(), same.next()) {
        (Some(d), None) => d.holders,
        _ => vec![],
    }
}

/// The serial of the USB device at libusb's (bus, address), or failing that, of the only device
/// with this vendor and product id.
pub fn serial_of(
    devs: &[UsbDev],
    bus: u32,
    address: u32,
    vendor: u16,
    product: u16,
) -> Option<String> {
    if let Some(d) = devs
        .iter()
        .find(|d| d.bus == bus && d.address == address && d.vendor == vendor)
    {
        return Some(d.serial.clone());
    }
    let mut same = devs
        .iter()
        .filter(|d| d.vendor == vendor && d.product == product);
    match (same.next(), same.next()) {
        (Some(d), None) => Some(d.serial.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"+-o Root  <class IORegistryEntry, id 0x100000100, retain 37>
  +-o AppleT6000USBXHCI@00000000  <class AppleT6000USBXHCI, id 0x10000043b, registered, matched, active, busy 0 (2983 ms), retain 75>
    | {
    |   "locationID" = 0
    | }
    |
    +-o SAMSUNG_Android@00100000  <class IOUSBHostDevice, id 0x100015f89, registered, matched, active, busy 0 (257 ms), retain 36>
        {
          "idProduct" = 26720
          "USB Product Name" = "SAMSUNG_Android"
          "locationID" = 1048576
          "kUSBSerialNumberString" = "R3CR406TEMA"
          "USB Address" = 1
          "idVendor" = 1256
          "USB Serial Number" = "R3CR406TEMA"
        }
  +-o AppleT6000USBXHCI@02000000  <class AppleT6000USBXHCI, id 0x10000039e, retain 41>
  | {
  |   "locationID" = 33554432
  | }
  | +-o Adapter@02100000  <class IOUSBHostDevice, id 0x100014e34, retain 24>
  |     {
  |       "USB Serial Number" = "000000000000"
  |       "idVendor" = 13372
  |       "idProduct" = 0
  |       "USB Address" = 1
  |       "locationID" = 34603008
  |     }
"#;

    #[test]
    fn reads_who_holds_a_device() {
        let text = r#"+-o SAMSUNG_Android@01100000  <class IOUSBHostDevice, id 0x1000222f4, registered>
  |   "USB Serial Number" = "R3CR406TEMA"
  |   "USB Address" = 1
  |   "idVendor" = 1256
  |   "idProduct" = 26720
  |   "UsbExclusiveOwner" = "pid 91884, ferry-files"
  |   "locationID" = 17825792
  +-o AppleUSBCDCCompositeDevice  <class AppleUSBCDCCompositeDevice, id 0x1000222f7>
  +-o MTP@0  <class IOUSBHostInterface, id 0x1000222fc, registered>
  | |   "locationID" = 17825792
  | |   "UsbExclusiveOwner" = "pid 13235, ptpcamerad"
  | +-o ptpcamerad  <class AppleUSBHostInterfaceUserClient, id 0x100022309>
  +-o CDC Abstract Control Model (ACM)@1  <class IOUSBHostInterface, id 0x1000222fe>
  | |   "locationID" = 17825792
  | |   "UsbExclusiveOwner" = "AppleUSBACMControl"
+-o Keyboard@02100000  <class IOUSBHostDevice, id 0x100014e34>
  |   "USB Address" = 1
  |   "UsbExclusiveOwner" = "pid 500, other"
  |   "locationID" = 34603008
"#;
        let h = |pid, name: &str| Holder {
            pid,
            name: name.into(),
        };
        assert_eq!(
            parse_holders(text, 1, 1, 0x04e8, 0x6860),
            [h(91884, "ferry-files"), h(13235, "ptpcamerad")]
        );
        assert_eq!(parse_holders(text, 2, 1, 0, 0), [h(500, "other")]);
        assert_eq!(parse_holders(text, 1, 2, 0, 0), []);
        // libusb still has the address from before the phone was reset: the only one with its id.
        assert_eq!(
            parse_holders(text, 1, 2, 0x04e8, 0x6860),
            [h(91884, "ferry-files"), h(13235, "ptpcamerad")]
        );
        assert_eq!(parse_holders(text, 2, 2, 0x04e8, 0x6860), []);
    }

    #[test]
    fn reads_usb_serials() {
        let devs = parse(SAMPLE);
        assert_eq!(
            devs,
            [
                UsbDev {
                    bus: 0,
                    address: 1,
                    vendor: 0x04e8,
                    product: 0x6860,
                    serial: "R3CR406TEMA".into()
                },
                UsbDev {
                    bus: 2,
                    address: 1,
                    vendor: 13372,
                    product: 0,
                    serial: "000000000000".into()
                },
            ]
        );
        assert_eq!(
            serial_of(&devs, 0, 1, 0x04e8, 0x6860).as_deref(),
            Some("R3CR406TEMA")
        );
        // Address unknown to ioreg: falls back to the only device with that id.
        assert_eq!(
            serial_of(&devs, 5, 9, 0x04e8, 0x6860).as_deref(),
            Some("R3CR406TEMA")
        );
        assert_eq!(serial_of(&devs, 5, 9, 1, 2), None);
    }
}
