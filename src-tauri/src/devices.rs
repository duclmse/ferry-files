//! Phones and tablets: adb (over USB and Wi-Fi), MTP, iPhones and iPads, the computer's USB list,
//! and finding, connecting and watching for them.

pub mod adb;
pub(crate) mod connect;
pub mod iphone;
/// MTP over libmtp on macOS and Linux; on Windows over Windows' own Portable Devices API, since
/// libmtp would need the phone's USB driver replaced there.
#[cfg(not(windows))]
pub mod mtp;
#[cfg(windows)]
#[path = "devices/wpd.rs"]
pub mod mtp;
pub mod usb;
pub(crate) mod watch;
pub(crate) mod wifi;
