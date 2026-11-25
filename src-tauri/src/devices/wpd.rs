//! MTP transport on Windows, over Windows Portable Devices (WPD): Windows has its own MTP driver,
//! so phones work without libmtp or a driver swap. Same paths and API as `mtp.rs`:
//! /<storage description>/<folders...>/<file>, e.g. "/Internal storage/Download".
//!
//! WPD objects are COM objects. Every call here first makes sure COM is set up on the calling
//! thread (multithreaded), and the device is only used under its lock.

use crate::device::{basename, dirname, err, join, DevError, Entry, Result, Space, Transport};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{GUID, HSTRING, PWSTR};
use windows::Win32::Devices::PortableDevices::*;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IStream, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, STGC_DEFAULT, STGM_READ, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::VT_DATE;

const CACHE_TTL: Duration = Duration::from_secs(3);
/// The device itself, whose children are the storages.
const DEVICE: &str = "DEVICE";

pub const MTP_HINT: &str = "Check that the phone is unlocked, connected with a data cable, and set to \
\"File transfer\" (USB notification on the phone). If it still isn't found, unplug it and plug it in again.";

static QUITTING: AtomicBool = AtomicBool::new(false);

/// Stops the file copies under way, for quitting.
pub fn stop_copies() {
    QUITTING.store(true, Ordering::Relaxed);
}

fn com() {
    thread_local!(static READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    READY.with(|r| {
        if !r.get() {
            // Fails harmlessly when the thread already has COM set up another way.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            r.set(true);
        }
    });
}

fn werr(what: &str, e: windows::core::Error) -> DevError {
    DevError::Msg(format!("{what}: {}", e.message()))
}

/// A string WPD allocated, as a String; frees it.
fn take(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn create<T: windows::core::Interface>(class: &GUID) -> Result<T> {
    unsafe { CoCreateInstance(class, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| werr("Windows Portable Devices isn't available", e))
}

/// An MTP device on USB, not opened.
pub struct Detected {
    /// The device's Plug and Play ID: what `open` takes.
    pub location: String,
    pub name: String,
    /// The USB serial number, which adb shows too: tells that both are the same phone.
    pub serial: Option<String>,
}

/// The USB serial in a Plug and Play ID, `\\?\usb#vid_18d1&pid_4ee1#SERIAL#{guid}`. A phone that is
/// also an adb device shows an interface ID there instead (with '&'), which isn't the serial.
fn serial_in(pnp: &str) -> Option<String> {
    let lower = pnp.to_ascii_lowercase();
    if !lower.contains("usb#vid_") {
        return None;
    }
    let part = pnp.split('#').nth(2)?;
    (!part.is_empty() && !part.contains('&')).then(|| part.to_string())
}

/// MTP devices on USB, without opening them. USB drives also show up in WPD; they are left out
/// (they're listed as drives).
pub fn detect() -> Result<Vec<Detected>> {
    com();
    let manager: IPortableDeviceManager = create(&PortableDeviceManager)?;
    let ids = unsafe {
        let _ = manager.RefreshDeviceList();
        let mut n = 0u32;
        manager
            .GetDevices(std::ptr::null_mut(), &mut n)
            .map_err(|e| werr("Couldn't list devices", e))?;
        let mut raw = vec![PWSTR::null(); n as usize];
        if n > 0 {
            manager
                .GetDevices(raw.as_mut_ptr(), &mut n)
                .map_err(|e| werr("Couldn't list devices", e))?;
        }
        raw.truncate(n as usize);
        raw.into_iter().map(take).collect::<Vec<_>>()
    };
    Ok(ids
        .into_iter()
        .filter(|id| id.to_ascii_lowercase().contains("usb#vid_"))
        .map(|id| {
            let name = friendly_name(&manager, &id).unwrap_or_else(|| "MTP device".into());
            Detected {
                serial: serial_in(&id),
                location: id,
                name,
            }
        })
        .collect())
}

fn friendly_name(manager: &IPortableDeviceManager, id: &str) -> Option<String> {
    let id = HSTRING::from(id);
    let mut len = 0u32;
    unsafe { manager.GetDeviceFriendlyName(&id, PWSTR::null(), &mut len) }.ok()?;
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    unsafe { manager.GetDeviceFriendlyName(&id, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    let s = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]);
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

/// A folder's contents, and how many of its items couldn't be read (those are left out, so
/// "not in `kids`" only means "not there" when `unreadable` is 0).
#[derive(Clone)]
struct Listing {
    kids: Vec<Child>,
    unreadable: usize,
}

#[derive(Clone)]
struct Child {
    id: String,
    name: String,
    dir: bool,
    size: u64,
    mtime: i64,
}

#[derive(Clone)]
struct Node {
    /// The storage's object ID; None for "/".
    storage: Option<String>,
    id: String,
    dir: bool,
    size: u64,
    mtime: i64,
    name: String,
    path: String,
}

/// The device's COM objects. They are made free-threaded (PortableDeviceFTM), so they can be used
/// from any thread with COM set up, one at a time (the lock in `Wpd`).
struct Link {
    device: IPortableDevice,
    content: IPortableDeviceContent,
    props: IPortableDeviceProperties,
}

unsafe impl Send for Link {}

impl Drop for Link {
    fn drop(&mut self) {
        com();
        let _ = unsafe { self.device.Close() };
    }
}

fn keys(list: &[PROPERTYKEY]) -> Result<IPortableDeviceKeyCollection> {
    let keys: IPortableDeviceKeyCollection = create(&PortableDeviceKeyCollection)?;
    for k in list {
        unsafe { keys.Add(k) }.map_err(|e| werr("Couldn't ask for properties", e))?;
    }
    Ok(keys)
}

fn string(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<String> {
    unsafe { v.GetStringValue(key) }
        .ok()
        .map(take)
        .filter(|s| !s.is_empty())
}

/// An OLE date (days since 30 December 1899) in seconds since 1970.
fn ole_date(v: &IPortableDeviceValues, key: &PROPERTYKEY) -> i64 {
    let Ok(mut pv) = (unsafe { v.GetValue(key) }) else {
        return 0;
    };
    let secs = unsafe {
        let inner = &pv.Anonymous.Anonymous;
        if inner.vt == VT_DATE {
            ((inner.Anonymous.date - 25569.0) * 86400.0) as i64
        } else {
            0
        }
    };
    let _ = unsafe { PropVariantClear(&mut pv) };
    secs
}

impl Link {
    fn values(&self) -> Result<IPortableDeviceValues> {
        create(&PortableDeviceValues)
    }

    fn get(&self, id: &str, list: &[PROPERTYKEY]) -> Result<IPortableDeviceValues> {
        unsafe { self.props.GetValues(&HSTRING::from(id), &keys(list)?) }
            .map_err(|e| werr("Couldn't read from the device", e))
    }

    fn ids(&self, parent: &str) -> Result<Vec<String>> {
        let found = unsafe { self.content.EnumObjects(0, &HSTRING::from(parent), None) }
            .map_err(|e| werr("Couldn't list the folder", e))?;
        let mut out = vec![];
        loop {
            let mut batch = [PWSTR::null(); 64];
            let mut n = 0u32;
            let hr = unsafe { found.Next(&mut batch, &mut n) };
            out.extend(batch[..n as usize].iter().map(|p| take(*p)));
            if hr.is_err() {
                return Err(werr("Couldn't list the folder", hr.into()));
            }
            if n == 0 || hr.0 == 1 {
                // S_FALSE: that was the last of them.
                return Ok(out);
            }
        }
    }

    /// The folder's contents. A folder that can't be listed is an error, not an empty folder:
    /// writes check what's there before replacing, and an empty answer would mislead them.
    fn children(&self, parent: &str) -> Result<Listing> {
        let ids = self.ids(parent)?;
        let total = ids.len();
        let want = [
            WPD_OBJECT_ORIGINAL_FILE_NAME,
            WPD_OBJECT_NAME,
            WPD_OBJECT_CONTENT_TYPE,
            WPD_OBJECT_SIZE,
            WPD_OBJECT_DATE_MODIFIED,
        ];
        let kids: Vec<Child> = ids
            .into_iter()
            .filter_map(|id| {
                let v = self.get(&id, &want).ok()?;
                let name = string(&v, &WPD_OBJECT_ORIGINAL_FILE_NAME)
                    .or_else(|| string(&v, &WPD_OBJECT_NAME))?;
                let kind = unsafe { v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE) }.unwrap_or_default();
                let dir =
                    kind == WPD_CONTENT_TYPE_FOLDER || kind == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT;
                let size = if dir {
                    0
                } else {
                    unsafe { v.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE) }.unwrap_or(0)
                };
                Some(Child {
                    name: name.replace('/', "_"),
                    dir,
                    size,
                    mtime: ole_date(&v, &WPD_OBJECT_DATE_MODIFIED),
                    id,
                })
            })
            .collect();
        Ok(Listing {
            unreadable: total - kids.len(),
            kids,
        })
    }

    fn stream(&self, id: &str) -> Result<IStream> {
        let res = unsafe { self.content.Transfer() }
            .map_err(|e| werr("Couldn't read from the device", e))?;
        let mut size = 0u32;
        let mut stream = None;
        unsafe {
            res.GetStream(
                &HSTRING::from(id),
                &WPD_RESOURCE_DEFAULT,
                STGM_READ.0,
                &mut size,
                &mut stream,
            )
        }
        .map_err(|e| werr("Couldn't read the file", e))?;
        stream.ok_or_else(|| DevError::Msg("The device sent no data.".into()))
    }

    /// Reads a stream to the end, into `out`; stops when any of `stops` is set.
    fn drain(stream: &IStream, out: &mut impl std::io::Write, stops: &[&AtomicBool]) -> Result<()> {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            if QUITTING.load(Ordering::Relaxed) {
                return err("Stopped: the app is quitting.");
            }
            if stops.iter().any(|f| f.load(Ordering::Relaxed)) {
                return err("Stopped.");
            }
            let mut n = 0u32;
            let hr =
                unsafe { stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut n)) };
            if hr.is_err() {
                return Err(werr("Couldn't read the file", hr.into()));
            }
            if n == 0 {
                return Ok(());
            }
            out.write_all(&buf[..n as usize])
                .map_err(|e| DevError::Msg(e.to_string()))?;
        }
    }

    fn download(&self, id: &str) -> Result<Vec<u8>> {
        let mut out = vec![];
        Self::drain(&self.stream(id)?, &mut out, &[])?;
        Ok(out)
    }

    /// Copies the file to `to`, stopping when any of `stops` is set.
    fn download_until(&self, id: &str, to: &std::path::Path, stops: &[&AtomicBool]) -> Result<()> {
        let mut f = std::fs::File::create(to).map_err(|e| DevError::Msg(e.to_string()))?;
        Self::drain(&self.stream(id)?, &mut f, stops)
    }

    /// `len` bytes from `start` of the file, or None when the device's stream can't seek.
    fn read_at(&self, id: &str, start: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let stream = self.stream(id)?;
        let mut at = 0u64;
        let sought = unsafe { stream.Seek(start as i64, STREAM_SEEK_SET, Some(&mut at)) };
        if sought.is_err() || at != start {
            return Ok(None);
        }
        let mut out = vec![0u8; len as usize];
        let mut got = 0usize;
        while got < out.len() {
            let mut n = 0u32;
            let want = (out.len() - got).min(1 << 20) as u32;
            let hr = unsafe { stream.Read(out[got..].as_mut_ptr().cast(), want, Some(&mut n)) };
            if hr.is_err() {
                return Err(werr("Couldn't read the file", hr.into()));
            }
            if n == 0 {
                break;
            }
            got += n as usize;
        }
        out.truncate(got);
        Ok(Some(out))
    }

    fn upload(&self, data: &[u8], name: &str, parent: &str) -> Result<String> {
        self.upload_from(&mut &data[..], data.len() as u64, name, parent)
    }

    /// Sends file `from` (`size` bytes) as `name` in folder `parent`, a piece at a time.
    fn upload_file(
        &self,
        from: &std::path::Path,
        size: u64,
        name: &str,
        parent: &str,
    ) -> Result<String> {
        let mut f = std::fs::File::open(from)
            .map_err(|e| DevError::Msg(format!("Can't read {}: {e}", from.display())))?;
        self.upload_from(&mut f, size, name, parent)
    }

    /// Sends `size` bytes read from `src` as `name` in folder `parent`.
    fn upload_from(
        &self,
        src: &mut dyn std::io::Read,
        size: u64,
        name: &str,
        parent: &str,
    ) -> Result<String> {
        let v = self.values()?;
        let name_w = HSTRING::from(name);
        unsafe {
            v.SetStringValue(&WPD_OBJECT_PARENT_ID, &HSTRING::from(parent))
                .map_err(|e| werr("Couldn't send the file", e))?;
            v.SetStringValue(&WPD_OBJECT_NAME, &name_w)
                .map_err(|e| werr("Couldn't send the file", e))?;
            v.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, &name_w)
                .map_err(|e| werr("Couldn't send the file", e))?;
            v.SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, size)
                .map_err(|e| werr("Couldn't send the file", e))?;
            v.SetGuidValue(&WPD_OBJECT_FORMAT, &WPD_OBJECT_FORMAT_UNSPECIFIED)
                .map_err(|e| werr("Couldn't send the file", e))?;
            v.SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_GENERIC_FILE)
                .map_err(|e| werr("Couldn't send the file", e))?;
        }
        let mut stream: Option<IStream> = None;
        let mut chunk = 0u32;
        unsafe {
            self.content.CreateObjectWithPropertiesAndData(
                &v,
                &mut stream,
                &mut chunk,
                std::ptr::null_mut(),
            )
        }
        .map_err(|e| werr(&format!("The device didn't take {name}"), e))?;
        let stream =
            stream.ok_or_else(|| DevError::Msg(format!("The device didn't take {name}.")))?;
        let mut buf = vec![0u8; (chunk as usize).clamp(16 * 1024, 1024 * 1024)];
        let mut sent = 0u64;
        loop {
            // Not committing drops the new file.
            if QUITTING.load(Ordering::Relaxed) {
                return err("Stopped: the app is quitting.");
            }
            let got = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return err(format!("Couldn't read {name} to send it: {e}")),
            };
            let piece = &buf[..got];
            sent += got as u64;
            let mut n = 0u32;
            let hr =
                unsafe { stream.Write(piece.as_ptr().cast(), piece.len() as u32, Some(&mut n)) };
            if hr.is_err() || n as usize != piece.len() {
                return Err(werr(&format!("Couldn't send {name}"), hr.into()));
            }
        }
        if sent != size {
            return err(format!("{name} changed while it was being sent."));
        }
        unsafe { stream.Commit(STGC_DEFAULT) }
            .map_err(|e| werr(&format!("Couldn't send {name}"), e))?;
        use windows::core::Interface;
        let data_stream: IPortableDeviceDataStream = stream
            .cast()
            .map_err(|e| werr("Couldn't send the file", e))?;
        Ok(take(
            unsafe { data_stream.GetObjectID() }.map_err(|e| werr("Couldn't send the file", e))?,
        ))
    }

    fn delete(&self, id: &str) -> Result<()> {
        // The object ID as a PROPVARIANT string, made by WPD itself so it frees it right.
        let v = self.values()?;
        let list: IPortableDevicePropVariantCollection =
            create(&PortableDevicePropVariantCollection)?;
        unsafe {
            v.SetStringValue(&WPD_OBJECT_ID, &HSTRING::from(id))
                .map_err(|e| werr("Couldn't delete", e))?;
            let mut pv: PROPVARIANT = v
                .GetValue(&WPD_OBJECT_ID)
                .map_err(|e| werr("Couldn't delete", e))?;
            let added = list.Add(&pv);
            let _ = PropVariantClear(&mut pv);
            added.map_err(|e| werr("Couldn't delete", e))?;
            self.content
                .Delete(
                    PORTABLE_DEVICE_DELETE_NO_RECURSION.0 as u32,
                    &list,
                    std::ptr::null_mut(),
                )
                .map_err(|e| werr("The device didn't delete it", e))
        }
    }

    fn rename(&self, id: &str, name: &str) -> Result<()> {
        let v = self.values()?;
        let name_w = HSTRING::from(name);
        unsafe {
            v.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, &name_w)
                .map_err(|e| werr("Couldn't rename", e))?;
            self.props
                .SetValues(&HSTRING::from(id), &v)
                .map_err(|e| werr("The device didn't rename it", e))?;
            // The name shown is a separate property on some devices; it doesn't matter if it fails.
            let shown = self.values()?;
            if shown.SetStringValue(&WPD_OBJECT_NAME, &name_w).is_ok() {
                let _ = self.props.SetValues(&HSTRING::from(id), &shown);
            }
        }
        Ok(())
    }

    fn mkdir(&self, name: &str, parent: &str) -> Result<()> {
        let v = self.values()?;
        let name_w = HSTRING::from(name);
        let mut id = PWSTR::null();
        unsafe {
            v.SetStringValue(&WPD_OBJECT_PARENT_ID, &HSTRING::from(parent))
                .map_err(|e| werr("Couldn't make the folder", e))?;
            v.SetStringValue(&WPD_OBJECT_NAME, &name_w)
                .map_err(|e| werr("Couldn't make the folder", e))?;
            v.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, &name_w)
                .map_err(|e| werr("Couldn't make the folder", e))?;
            v.SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &WPD_CONTENT_TYPE_FOLDER)
                .map_err(|e| werr("Couldn't make the folder", e))?;
            v.SetGuidValue(&WPD_OBJECT_FORMAT, &WPD_OBJECT_FORMAT_PROPERTIES_ONLY)
                .map_err(|e| werr("Couldn't make the folder", e))?;
            self.content
                .CreateObjectWithPropertiesOnly(&v, &mut id)
                .map_err(|e| werr(&format!("The device didn't make {name}"), e))?;
        }
        take(id);
        Ok(())
    }

    /// Each storage: its object ID and description.
    fn storages(&self) -> Result<Vec<(String, String)>> {
        let mut out = vec![];
        for id in self.ids(DEVICE)? {
            let Ok(v) = self.get(
                &id,
                &[
                    WPD_FUNCTIONAL_OBJECT_CATEGORY,
                    WPD_STORAGE_DESCRIPTION,
                    WPD_OBJECT_NAME,
                ],
            ) else {
                continue;
            };
            if unsafe { v.GetGuidValue(&WPD_FUNCTIONAL_OBJECT_CATEGORY) }.ok()
                != Some(WPD_FUNCTIONAL_CATEGORY_STORAGE)
            {
                continue;
            }
            let desc = string(&v, &WPD_STORAGE_DESCRIPTION)
                .or_else(|| string(&v, &WPD_OBJECT_NAME))
                .unwrap_or_default();
            out.push((id, desc));
        }
        Ok(out)
    }
}

fn temp_name(name: &str) -> String {
    format!(".{name}.ferry-tmp")
}

struct Inner {
    link: Link,
    cache: HashMap<String, (Instant, Listing)>,
    /// False once the device's stream has refused to seek; ranges then come from a whole copy.
    seek: bool,
}

/// `inner`, locked, on a thread set up for COM.
fn lock(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    com();
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// A file copied whole to be played (see `crate::preview::stream`), as the device can't send part
/// of a file: its parts are read from the copy without waiting for the device, which the copy
/// holds.
#[derive(Clone)]
struct Streamed {
    path: String,
    key: String,
    ext: String,
    size: u64,
    id: String,
}

impl Inner {
    /// Only complete listings are cached, so after a failure (or an item that couldn't be read)
    /// the next look tries the device again.
    fn listing(&mut self, parent: &str) -> Result<Listing> {
        if let Some((at, l)) = self.cache.get(parent) {
            if at.elapsed() < CACHE_TTL {
                return Ok(l.clone());
            }
        }
        let l = self.link.children(parent)?;
        if l.unreadable == 0 {
            self.cache
                .insert(parent.to_string(), (Instant::now(), l.clone()));
        }
        Ok(l)
    }

    /// The items that could be read, for browsing and for finding an item by ID.
    fn children(&mut self, parent: &str) -> Result<Vec<Child>> {
        Ok(self.listing(parent)?.kids)
    }

    /// The item called `name`, or None if it isn't there. If it wasn't found but some items
    /// couldn't be read, it may be one of those: that's an error, so a write doesn't put a
    /// second file of the same name next to it.
    fn find(&mut self, parent: &str, name: &str) -> Result<Option<Child>> {
        let l = self.listing(parent)?;
        let unreadable = l.unreadable;
        match l.kids.into_iter().find(|k| k.name == name) {
            Some(k) => Ok(Some(k)),
            None if unreadable > 0 => err(format!(
                "Couldn't read {unreadable} item{} in the folder, so can't tell whether {name} is there. Try again, or reconnect the device.",
                if unreadable == 1 { "" } else { "s" }
            )),
            None => Ok(None),
        }
    }

    fn invalidate(&mut self, parent: &str) {
        self.cache.remove(parent);
    }
}

pub struct Wpd {
    inner: Arc<Mutex<Inner>>,
    /// The last few files copied to be played (see `crate::preview::stream`).
    streamed: Mutex<Vec<Streamed>>,
    /// Set when the device is let go of: stops a copy being played.
    stop: Arc<AtomicBool>,
    /// (name in paths, object ID)
    storages: Vec<(String, String)>,
    model: String,
}

// A copy being played holds the device: stop it, so the device is let go of.
impl Drop for Wpd {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The name the rest of the app knows the MTP transport by.
pub type Mtp = Wpd;

impl Wpd {
    /// Opens the MTP device with Plug and Play ID `location` (from `detect`), or the first one.
    pub fn open(location: Option<&str>) -> Result<Wpd> {
        com();
        let found = detect()?;
        let dev = match location {
            Some(loc) => found.into_iter().find(|d| d.location == loc),
            None => found.into_iter().next(),
        };
        let Some(dev) = dev else {
            return err(format!("No MTP device found. {MTP_HINT}"));
        };
        let client = create::<IPortableDeviceValues>(&PortableDeviceValues)?;
        let device: IPortableDevice = create(&PortableDeviceFTM)?;
        unsafe {
            let _ = client.SetStringValue(&WPD_CLIENT_NAME, &HSTRING::from("Ferry Files"));
            let _ = client.SetUnsignedIntegerValue(&WPD_CLIENT_MAJOR_VERSION, 1);
            let _ = client.SetUnsignedIntegerValue(&WPD_CLIENT_MINOR_VERSION, 0);
            let _ = client.SetUnsignedIntegerValue(&WPD_CLIENT_REVISION, 0);
            device
                .Open(&HSTRING::from(dev.location.as_str()), &client)
                .map_err(|e| werr(&format!("Couldn't open {}. {MTP_HINT}", dev.name), e))?;
        }
        let content =
            unsafe { device.Content() }.map_err(|e| werr("Couldn't read the device", e))?;
        let props =
            unsafe { content.Properties() }.map_err(|e| werr("Couldn't read the device", e))?;
        let link = Link {
            device,
            content,
            props,
        };
        let mut storages: Vec<(String, String)> = Vec::new();
        for (id, desc) in link.storages()? {
            let base = if desc.is_empty() {
                "Storage".to_string()
            } else {
                desc.replace('/', "_")
            };
            let mut name = base.clone();
            let mut n = 2;
            while storages.iter().any(|(x, _)| *x == name) {
                name = format!("{base} ({n})");
                n += 1;
            }
            storages.push((name, id));
        }
        if storages.is_empty() {
            return err(
                "The device shows no storage. Unlock the phone and choose \"File transfer\".",
            );
        }
        Ok(Wpd {
            inner: Arc::new(Mutex::new(Inner {
                link,
                cache: HashMap::new(),
                seek: true,
            })),
            streamed: Mutex::new(Vec::new()),
            stop: Arc::default(),
            storages,
            model: dev.name,
        })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// Part of the file `s`, from its copy (started if need be).
    fn read_streamed(&self, s: Streamed, start: u64, len: u64) -> Result<Vec<u8>> {
        let (inner, stop, id) = (self.inner.clone(), self.stop.clone(), s.id.clone());
        let fill = move |to: &std::path::Path, cancel: &AtomicBool| {
            lock(&inner)
                .link
                .download_until(&id, to, &[&stop, cancel])
                .map_err(|e| e.to_string())
        };
        crate::preview::stream::read(&s.key, &s.ext, s.size, start, len, fill)
            .map_err(DevError::Msg)
    }

    /// Writes change what's on the device: a copy being played is let go of.
    fn forget_streamed(&self) {
        self.streamed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// The object at `path`: None if it isn't there, an error if a folder on the way can't be
    /// listed or it might be an item that couldn't be read.
    fn resolve(&self, inner: &mut Inner, path: &str) -> Result<Option<Node>> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let Some(first) = parts.first() else {
            return Ok(Some(Node {
                storage: None,
                id: DEVICE.into(),
                dir: true,
                size: 0,
                mtime: 0,
                name: "/".into(),
                path: "/".into(),
            }));
        };
        let Some(sid) = self
            .storages
            .iter()
            .find(|(n, _)| n == first)
            .map(|(_, id)| id.clone())
        else {
            return Ok(None);
        };
        let mut node = Node {
            storage: Some(sid.clone()),
            id: sid.clone(),
            dir: true,
            size: 0,
            mtime: 0,
            name: first.to_string(),
            path: format!("/{first}"),
        };
        for seg in &parts[1..] {
            if !node.dir {
                return Ok(None);
            }
            let Some(kid) = inner.find(&node.id, seg)? else {
                return Ok(None);
            };
            node = Node {
                storage: Some(sid.clone()),
                id: kid.id,
                dir: kid.dir,
                size: kid.size,
                mtime: kid.mtime,
                path: join(&node.path, seg),
                name: kid.name,
            };
        }
        Ok(Some(node))
    }

    /// The object ID of the folder `path` goes in.
    fn parent_of(&self, inner: &mut Inner, path: &str) -> Result<String> {
        match self.resolve(inner, &dirname(path))? {
            Some(Node {
                storage: Some(_),
                id,
                dir: true,
                ..
            }) => Ok(id),
            _ => err(format!(
                "Files must go inside a storage folder, e.g. {}",
                self.default_dir()
            )),
        }
    }

    fn version_of(n: &Node) -> String {
        format!("{}:{}:{}", n.id, n.size, n.mtime)
    }

    /// MTP can't overwrite: upload under a temporary name, delete the old file, rename. The new
    /// data stays on the device at every step: a copy is only deleted once another one is there.
    fn put(
        &self,
        path: &str,
        exists: bool,
        size: u64,
        send: &dyn Fn(&Link, &str, &str) -> Result<String>,
    ) -> Result<String> {
        self.forget_streamed();
        let mut inner = self.inner();
        let name = basename(path).to_string();
        let parent = self.parent_of(&mut inner, path)?;
        // A folder that can't be listed is an error here, not "no old file": uploading anyway
        // would put a second file of the same name next to the old one.
        let old = if exists {
            self.resolve(&mut inner, path)?
        } else {
            None
        };
        let new_id = match old {
            None => send(&inner.link, &name, &parent)?,
            Some(old) => {
                let tmp_name = temp_name(&name);
                inner.invalidate(&parent);
                if let Some(stale) = inner.find(&parent, &tmp_name)? {
                    inner.link.delete(&stale.id)?;
                }
                let tmp_id = send(&inner.link, &tmp_name, &parent)?;
                Self::check_size(&mut inner, &parent, &tmp_id, size, &tmp_name)?;
                inner.link.delete(&old.id)?;
                inner.invalidate(&parent);
                match inner.link.rename(&tmp_id, &name) {
                    // Checked below; if that fails the file is kept and the error reported.
                    Ok(()) => tmp_id,
                    Err(_) => {
                        // Some devices can't rename: upload again under the real name, and only
                        // then drop the temporary copy.
                        let id = send(&inner.link, &name, &parent).map_err(|e| {
                            let why = e.to_string();
                            DevError::Msg(format!(
                                "{}. The new version is on the device as {tmp_name}.",
                                why.trim_end_matches(['.', ' ', '\r', '\n'])
                            ))
                        })?;
                        let _ = inner.link.delete(&tmp_id);
                        inner.invalidate(&parent);
                        id
                    }
                }
            }
        };
        let k = Self::check_size(&mut inner, &parent, &new_id, size, &name)?;
        Ok(format!("{}:{}:{}", k.id, k.size, k.mtime))
    }

    fn check_size(
        inner: &mut Inner,
        parent: &str,
        id: &str,
        size: u64,
        name: &str,
    ) -> Result<Child> {
        inner.invalidate(parent);
        match inner.children(parent)?.into_iter().find(|k| k.id == id) {
            Some(k) if k.size == size && k.name == name => Ok(k),
            _ => err(format!("Verification failed after writing {name}.")),
        }
    }

    /// A whole copy of the file in the app's "opened" folder, for reading parts of it. Only the
    /// latest one is kept.
    fn public(n: &Node) -> Entry {
        Entry {
            name: n.name.clone(),
            path: n.path.clone(),
            dir: n.dir,
            size: n.size,
            mtime: n.mtime,
            hidden: false,
        }
    }
}

impl Transport for Wpd {
    fn kind(&self) -> &'static str {
        "mtp"
    }
    fn label(&self) -> String {
        self.model.clone()
    }
    fn default_dir(&self) -> String {
        format!("/{}", self.storages[0].0)
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        self.storages
            .iter()
            .map(|(n, _)| (n.clone(), format!("/{n}")))
            .collect()
    }

    fn refresh(&self) {
        self.inner().cache.clear();
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let mut inner = self.inner();
        let node = match self.resolve(&mut inner, path)? {
            Some(n) if n.dir => n,
            _ => return err(format!("Not a folder: {path}")),
        };
        if node.storage.is_none() {
            return Ok(self
                .storages
                .iter()
                .map(|(n, _)| Entry {
                    name: n.clone(),
                    path: format!("/{n}"),
                    dir: true,
                    size: 0,
                    mtime: 0,
                    hidden: false,
                })
                .collect());
        }
        Ok(inner
            .children(&node.id)?
            .into_iter()
            .map(|k| Entry {
                path: join(&node.path, &k.name),
                name: k.name,
                dir: k.dir,
                size: k.size,
                mtime: k.mtime,
                hidden: false,
            })
            .collect())
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let mut inner = self.inner();
        Ok(self.resolve(&mut inner, path)?.map(|n| Self::public(&n)))
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        let mut inner = self.inner();
        if let Ok(parent) = self.parent_of(&mut inner, path) {
            inner.invalidate(&parent);
        }
        Ok(self
            .resolve(&mut inner, path)?
            .filter(|n| !n.dir)
            .map(|n| Self::version_of(&n)))
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        // Being copied: read from the copy, without waiting for the device the copy holds.
        let streamed = self
            .streamed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|s| s.path == path)
            .cloned();
        if let Some(s) = streamed {
            return self.read_streamed(s, start, len);
        }
        let mut inner = self.inner();
        let Some(node) = self.resolve(&mut inner, path)?.filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        if start >= node.size || len == 0 {
            return Ok(vec![]);
        }
        let len = len.min(node.size - start);
        if inner.seek {
            match inner.link.read_at(&node.id, start, len)? {
                Some(data) => return Ok(data),
                None => inner.seek = false,
            }
        }
        // The device can't send part of a file: copy it whole, and read from the copy as it comes.
        drop(inner);
        let s = Streamed {
            path: path.to_string(),
            key: format!(
                "wpd|{}|{}|{}",
                self.model,
                node.path,
                Self::version_of(&node)
            ),
            ext: node
                .name
                .rsplit_once('.')
                .map_or(String::new(), |(_, x)| format!(".{x}")),
            size: node.size,
            id: node.id.clone(),
        };
        {
            let mut all = self.streamed.lock().unwrap_or_else(|e| e.into_inner());
            all.retain(|o| o.path != path);
            all.push(s.clone());
            // As many as `crate::preview::stream` keeps copies of.
            let extra = all.len().saturating_sub(3);
            all.drain(..extra);
        }
        self.read_streamed(s, start, len)
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let mut inner = self.inner();
        if let Ok(parent) = self.parent_of(&mut inner, path) {
            inner.invalidate(&parent);
        }
        let Some(node) = self.resolve(&mut inner, path)?.filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        let data = inner.link.download(&node.id)?;
        if data.len() as u64 != node.size {
            return err(format!(
                "Read {} bytes but the device reports {}.",
                data.len(),
                node.size
            ));
        }
        Ok((data, Self::version_of(&node)))
    }

    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        self.put(path, exists, data.len() as u64, &|link, name, parent| {
            link.upload(data, name, parent)
        })
    }

    fn replace_from(&self, path: &str, from: &std::path::Path, exists: bool) -> Result<String> {
        let size = std::fs::metadata(from)
            .map_err(|e| DevError::Msg(format!("Can't read {}: {e}", from.display())))?
            .len();
        self.put(path, exists, size, &|link, name, parent| {
            link.upload_file(from, size, name, parent)
        })
    }

    fn read_to(
        &self,
        path: &str,
        to: &std::path::Path,
        read: &mut dyn FnMut(u64),
        stop: &AtomicBool,
    ) -> Result<String> {
        let mut inner = self.inner();
        if let Ok(parent) = self.parent_of(&mut inner, path) {
            inner.invalidate(&parent);
        }
        let Some(node) = self.resolve(&mut inner, path)?.filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        inner
            .link
            .download_until(&node.id, to, &[&*self.stop, stop])?;
        let got = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
        if got != node.size {
            return err(format!(
                "Read {got} bytes but the device reports {}.",
                node.size
            ));
        }
        read(got);
        Ok(Self::version_of(&node))
    }

    fn delete(&self, path: &str, _is_dir: bool) -> Result<()> {
        self.forget_streamed();
        let mut inner = self.inner();
        let Some(node) = self.resolve(&mut inner, path)? else {
            return err(format!("No such file or folder: {path}"));
        };
        if node.storage.as_deref().is_none_or(|s| s == node.id) {
            return err("Storages can't be removed.");
        }
        let parent = self.parent_of(&mut inner, path)?;
        inner.link.delete(&node.id)?;
        inner.invalidate(&parent);
        Ok(())
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        let mut inner = self.inner();
        let parent = self.parent_of(&mut inner, path)?;
        inner.link.mkdir(basename(path), &parent)?;
        inner.invalidate(&parent);
        Ok(())
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        self.forget_streamed();
        let mut inner = self.inner();
        let node = match self.resolve(&mut inner, path)? {
            Some(n) if n.storage.as_deref().is_some_and(|s| s != n.id) => n,
            _ => return err(format!("{path} can't be renamed.")),
        };
        let parent = self.parent_of(&mut inner, path)?;
        inner.link.rename(&node.id, new_name)?;
        inner.invalidate(&parent);
        if !inner
            .children(&parent)?
            .iter()
            .any(|k| k.id == node.id && k.name == new_name)
        {
            return err(format!("The device didn't rename {}.", node.name));
        }
        Ok(())
    }

    fn space(&self) -> Vec<Space> {
        let inner = self.inner();
        self.storages
            .iter()
            .filter_map(|(name, id)| {
                let v = inner
                    .link
                    .get(id, &[WPD_STORAGE_FREE_SPACE_IN_BYTES, WPD_STORAGE_CAPACITY])
                    .ok()?;
                let free =
                    unsafe { v.GetUnsignedLargeIntegerValue(&WPD_STORAGE_FREE_SPACE_IN_BYTES) }
                        .ok()?;
                let total =
                    unsafe { v.GetUnsignedLargeIntegerValue(&WPD_STORAGE_CAPACITY) }.ok()?;
                Some(Space {
                    name: name.clone(),
                    free,
                    total,
                    path: format!("/{name}"),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_the_serial() {
        let id = r"\\?\usb#vid_18d1&pid_4ee1#0A1B2C3D#{6ac27878-a6fa-4155-ba85-f98f491d4f33}";
        assert_eq!(super::serial_in(id).as_deref(), Some("0A1B2C3D"));
        let composite = r"\\?\usb#vid_18d1&pid_4ee2&mi_00#6&2bd3f1a&0&0000#{6ac27878-a6fa-4155-ba85-f98f491d4f33}";
        assert_eq!(super::serial_in(composite), None);
        assert_eq!(
            super::serial_in(r"\\?\swd#wpdbusenum#_??_usbstor#disk"),
            None
        );
    }
}
