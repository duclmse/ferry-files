//! MTP transport over libmtp (loaded at runtime, so the app still starts without it).
//! Paths look like /<storage description>/<folders...>/<file>, e.g. "/Internal storage/Download".

use crate::device::{basename, dirname, err, join, DevError, Entry, Result, Transport};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ROOT_ID: u32 = 0xFFFF_FFFF; // LIBMTP_FILES_AND_FOLDERS_ROOT
const FILETYPE_FOLDER: c_int = 0; // LIBMTP_FILETYPE_FOLDER
const FILETYPE_UNKNOWN: c_int = 44; // LIBMTP_FILETYPE_UNKNOWN (libmtp 1.1.23)
const ERROR_NO_DEVICE_ATTACHED: c_int = 5;
const CACHE_TTL: Duration = Duration::from_secs(3);

/// How to get libmtp, when it isn't there.
#[cfg(target_os = "macos")]
const INSTALL: &str = "libmtp not found. Install it with: brew install libmtp";
#[cfg(not(target_os = "macos"))]
const INSTALL: &str = "libmtp not found. Install it with your package manager (libmtp9 on Debian and Ubuntu, \
libmtp on Fedora and Arch).";

/// Where libmtp may be: next to the app first (a bundle carries its own), then where it's installed.
fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    #[cfg(target_os = "macos")]
    {
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("../Frameworks"))) {
            out.push(dir.join("libmtp.9.dylib"));
        }
        for p in ["/opt/homebrew/lib/libmtp.dylib", "/usr/local/lib/libmtp.dylib", "libmtp.dylib"] {
            out.push(p.into());
        }
    }
    #[cfg(not(target_os = "macos"))]
    for p in ["libmtp.so.9", "libmtp.so"] {
        out.push(p.into());
    }
    out
}

// ---- libmtp structs (only what we use; layouts match libmtp 1.1.x on 64-bit macOS and Linux) ----------

#[repr(C)]
struct MtpFile {
    item_id: u32,
    parent_id: u32,
    storage_id: u32,
    filename: *mut c_char,
    filesize: u64,
    modificationdate: i64, // time_t
    filetype: c_int,
    next: *mut MtpFile,
}

#[repr(C)]
struct MtpStorage {
    id: u32,
    storage_type: u16,
    filesystem_type: u16,
    access_capability: u16,
    max_capacity: u64,
    free_space_in_bytes: u64,
    free_space_in_objects: u64,
    storage_description: *mut c_char,
    volume_identifier: *mut c_char,
    next: *mut MtpStorage,
    prev: *mut MtpStorage,
}

#[repr(C)]
struct MtpError {
    errornumber: c_int,
    error_text: *mut c_char,
    next: *mut MtpError,
}

#[repr(C)]
struct MtpDevice {
    // Leading fields only; libmtp allocates this struct.
    object_bitsize: u8,
    params: *mut c_void,
    usbinfo: *mut c_void,
    storage: *mut MtpStorage,
    errorstack: *mut MtpError,
}

#[repr(C)]
struct DeviceEntry {
    vendor: *mut c_char,
    vendor_id: u16,
    product: *mut c_char,
    product_id: u16,
    device_flags: u32,
}

#[repr(C)]
struct RawDevice {
    device_entry: DeviceEntry,
    bus_location: u32,
    devnum: u8,
}

type Dev = *mut MtpDevice;

struct Lib {
    detect: unsafe extern "C" fn(*mut *mut RawDevice, *mut c_int) -> c_int,
    open_uncached: unsafe extern "C" fn(*mut RawDevice) -> Dev,
    release: unsafe extern "C" fn(Dev),
    modelname: unsafe extern "C" fn(Dev) -> *mut c_char,
    friendlyname: unsafe extern "C" fn(Dev) -> *mut c_char,
    get_storage: unsafe extern "C" fn(Dev, c_int) -> c_int,
    errorstack: unsafe extern "C" fn(Dev) -> *mut MtpError,
    clear_errorstack: unsafe extern "C" fn(Dev),
    files_and_folders: unsafe extern "C" fn(Dev, u32, u32) -> *mut MtpFile,
    filemetadata: unsafe extern "C" fn(Dev, u32) -> *mut MtpFile,
    destroy_file: unsafe extern "C" fn(*mut MtpFile),
    get_file_to_file: unsafe extern "C" fn(Dev, u32, *const c_char, Option<Progress>, *const c_void) -> c_int,
    send_file_from_file: unsafe extern "C" fn(Dev, *const c_char, *mut MtpFile, Option<Progress>, *const c_void) -> c_int,
    set_file_name: unsafe extern "C" fn(Dev, *mut MtpFile, *const c_char) -> c_int,
    delete_object: unsafe extern "C" fn(Dev, u32) -> c_int,
    create_folder: unsafe extern "C" fn(Dev, *mut c_char, u32, u32) -> u32,
    /// `LIBMTP_GetPartialObject`; missing from old libmtp builds.
    get_partial: Option<unsafe extern "C" fn(Dev, u32, u64, u32, *mut *mut u8, *mut u32) -> c_int>,
}

/// `LIBMTP_progressfunc_t`: called as a file is copied; anything but 0 stops the copy.
type Progress = extern "C" fn(sent: u64, total: u64, data: *const c_void) -> c_int;

/// Set when the app quits: file copies to and from the phone stop at their next piece, so the
/// phone can be let go of without waiting for them.
static QUITTING: AtomicBool = AtomicBool::new(false);

/// Stops the file copies under way, for quitting.
pub fn stop_copies() {
    QUITTING.store(true, Ordering::Relaxed);
}

/// `data` is the device's stop flag (`Link::stop`).
extern "C" fn progress(_sent: u64, _total: u64, data: *const c_void) -> c_int {
    let stopped = !data.is_null() && unsafe { (*data.cast::<AtomicBool>()).load(Ordering::Relaxed) };
    (stopped || QUITTING.load(Ordering::Relaxed)) as c_int
}

/// `data` is a `[&AtomicBool; 2]`: the device's stop flag and the copy's own.
extern "C" fn progress_either(_sent: u64, _total: u64, data: *const c_void) -> c_int {
    let flags = unsafe { &*data.cast::<[&AtomicBool; 2]>() };
    (flags.iter().any(|f| f.load(Ordering::Relaxed)) || QUITTING.load(Ordering::Relaxed)) as c_int
}

extern "C" {
    /// libmtp hands back buffers from malloc.
    fn free(p: *mut c_void);
}

/// libmtp isn't thread-safe, and several devices can be open at once (each used from its own
/// thread), so every libmtp call holds this one lock. Taking it again on the same thread is a
/// no-op, so code that already holds it (e.g. `open` dropping a half-opened device) can't deadlock.
struct LibLock(Option<std::sync::MutexGuard<'static, ()>>);

thread_local! {
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn lib_lock() -> LibLock {
    static LOCK: Mutex<()> = Mutex::new(());
    if HELD.with(|h| h.get()) {
        return LibLock(None);
    }
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    HELD.with(|h| h.set(true));
    LibLock(Some(g))
}

impl Drop for LibLock {
    fn drop(&mut self) {
        if self.0.is_some() {
            HELD.with(|h| h.set(false));
        }
    }
}

fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(|e| DevError::Msg(e.clone()))
}

fn load() -> std::result::Result<Lib, String> {
    let library = candidates().iter().find_map(|p| unsafe { libloading::Library::new(p) }.ok()).ok_or(INSTALL)?;
    // The library stays loaded for the life of the app.
    let library: &'static libloading::Library = Box::leak(Box::new(library));
    macro_rules! sym {
        ($name:literal) => {
            *unsafe { library.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("libmtp is missing {}: {e}", $name))?
        };
    }
    let lib = Lib {
        detect: sym!("LIBMTP_Detect_Raw_Devices"),
        open_uncached: sym!("LIBMTP_Open_Raw_Device_Uncached"),
        release: sym!("LIBMTP_Release_Device"),
        modelname: sym!("LIBMTP_Get_Modelname"),
        friendlyname: sym!("LIBMTP_Get_Friendlyname"),
        get_storage: sym!("LIBMTP_Get_Storage"),
        errorstack: sym!("LIBMTP_Get_Errorstack"),
        clear_errorstack: sym!("LIBMTP_Clear_Errorstack"),
        files_and_folders: sym!("LIBMTP_Get_Files_And_Folders"),
        filemetadata: sym!("LIBMTP_Get_Filemetadata"),
        destroy_file: sym!("LIBMTP_destroy_file_t"),
        get_file_to_file: sym!("LIBMTP_Get_File_To_File"),
        send_file_from_file: sym!("LIBMTP_Send_File_From_File"),
        set_file_name: sym!("LIBMTP_Set_File_Name"),
        delete_object: sym!("LIBMTP_Delete_Object"),
        create_folder: sym!("LIBMTP_Create_Folder"),
        get_partial: unsafe { library.get(b"LIBMTP_GetPartialObject\0") }.ok().map(|f| *f),
    };
    let init: unsafe extern "C" fn() = sym!("LIBMTP_Init");
    unsafe { init() };
    Ok(lib)
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn raw_devices(lib: &Lib) -> Result<Vec<*mut RawDevice>> {
    let mut raws: *mut RawDevice = std::ptr::null_mut();
    let mut count: c_int = 0;
    let rc = unsafe { (lib.detect)(&mut raws, &mut count) };
    if rc == ERROR_NO_DEVICE_ATTACHED || count == 0 {
        return Ok(vec![]);
    }
    if rc != 0 {
        return err(format!("libmtp couldn't scan USB devices (error {rc})."));
    }
    // The array is malloc'd by libmtp and small; it's deliberately not freed (open copies from it).
    Ok((0..count as usize).map(|i| unsafe { raws.add(i) }).collect())
}

/// Where a raw device is on USB ("bus-devnum"): tells two connected devices apart.
fn location(r: *mut RawDevice) -> String {
    unsafe { format!("{}-{}", (*r).bus_location, (*r).devnum) }
}

/// An MTP device on USB, not opened.
pub struct Detected {
    /// "bus-devnum": what `open` takes.
    pub location: String,
    pub name: String,
    /// The USB serial number, which adb shows too: tells that both are the same phone.
    pub serial: Option<String>,
}

/// MTP devices on USB, without opening them.
pub fn detect() -> Result<Vec<Detected>> {
    let lib = lib()?;
    let _lib = lib_lock();
    let raws = raw_devices(lib)?;
    let usb = if raws.is_empty() { vec![] } else { crate::devices::usb::list() };
    Ok(raws
        .into_iter()
        .map(|r| {
            let (e, bus, devnum) = unsafe { (&(*r).device_entry, (*r).bus_location, (*r).devnum) };
            let name = format!("{} {}", cstr(e.vendor), cstr(e.product)).trim().to_string();
            let name = if name.is_empty() { format!("{:04x}:{:04x}", e.vendor_id, e.product_id) } else { name };
            Detected { location: location(r), name, serial: crate::devices::usb::serial_of(&usb, bus, devnum as u32, e.vendor_id, e.product_id) }
        })
        .collect())
}

#[derive(Clone)]
struct Child {
    id: u32,
    name: String,
    dir: bool,
    size: u64,
    mtime: i64,
}

/// One open libmtp device. Only used behind `Mtp::inner`'s mutex.
struct Link {
    lib: &'static Lib,
    dev: Dev,
    /// False once the device has refused a partial read; ranges then come from a whole copy.
    partial: bool,
    /// Set when the device is being let go of: its copies stop (see `progress`).
    stop: Arc<AtomicBool>,
}

// The raw device pointer is only touched while holding the mutex in `Mtp`.
unsafe impl Send for Link {}

impl Link {
    /// What `progress` gets as its data: lives as long as the link.
    fn stop_flag(&self) -> *const c_void {
        Arc::as_ptr(&self.stop).cast()
    }

    fn error(&self, what: &str) -> DevError {
        let mut last = None;
        let mut e = unsafe { (self.lib.errorstack)(self.dev) };
        while !e.is_null() {
            let text = cstr(unsafe { (*e).error_text });
            if !text.is_empty() {
                last = Some(text);
            }
            e = unsafe { (*e).next };
        }
        unsafe { (self.lib.clear_errorstack)(self.dev) };
        DevError::Msg(match last {
            Some(t) => format!("{what} failed: {t}"),
            None => format!("{what} failed."),
        })
    }

    fn children(&self, storage: u32, parent: u32) -> Vec<Child> {
        let mut out = Vec::new();
        let mut f = unsafe { (self.lib.files_and_folders)(self.dev, storage, parent) };
        while !f.is_null() {
            // Read everything (including `next`) before freeing the node.
            let (c, next) = unsafe {
                let r = &*f;
                (Child { id: r.item_id, name: cstr(r.filename), dir: r.filetype == FILETYPE_FOLDER, size: r.filesize, mtime: r.modificationdate }, r.next)
            };
            unsafe { (self.lib.destroy_file)(f) };
            out.push(c);
            f = next;
        }
        unsafe { (self.lib.clear_errorstack)(self.dev) };
        out
    }

    fn download(&self, id: u32) -> Result<Vec<u8>> {
        let tmp = temp_path("dl");
        let data = self.download_to(id, &tmp).and_then(|_| std::fs::read(&tmp).map_err(|e| DevError::Msg(e.to_string())));
        let _ = std::fs::remove_file(&tmp);
        data
    }

    fn download_to(&self, id: u32, to: &std::path::Path) -> Result<()> {
        let c = CString::new(to.to_string_lossy().as_bytes()).unwrap();
        let rc = unsafe { (self.lib.get_file_to_file)(self.dev, id, c.as_ptr(), Some(progress), self.stop_flag()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(self.error("Reading the file"))
        }
    }

    /// As `download_to`, also stopping when `cancel` is set.
    fn download_until(&self, id: u32, to: &std::path::Path, cancel: &AtomicBool) -> Result<()> {
        let c = CString::new(to.to_string_lossy().as_bytes()).unwrap();
        let flags: [&AtomicBool; 2] = [&self.stop, cancel];
        let data: *const [&AtomicBool; 2] = &flags;
        let rc = unsafe { (self.lib.get_file_to_file)(self.dev, id, c.as_ptr(), Some(progress_either), data.cast()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(self.error("Reading the file"))
        }
    }

    /// Part of a file straight from the device, or None when it (or libmtp) can't do that.
    fn read_partial(&mut self, id: u32, start: u64, len: u32) -> Option<Vec<u8>> {
        let f = self.lib.get_partial.filter(|_| self.partial)?;
        let (mut data, mut size) = (std::ptr::null_mut::<u8>(), 0u32);
        let rc = unsafe { f(self.dev, id, start, len, &mut data, &mut size) };
        let out = (rc == 0).then(|| if data.is_null() { vec![] } else { unsafe { std::slice::from_raw_parts(data, size as usize) }.to_vec() });
        if !data.is_null() {
            unsafe { free(data.cast()) };
        }
        if out.is_none() {
            unsafe { (self.lib.clear_errorstack)(self.dev) };
            self.partial = false;
        }
        out
    }

    fn upload(&self, data: &[u8], name: &str, storage: u32, parent: u32) -> Result<u32> {
        let tmp = temp_path("ul");
        std::fs::write(&tmp, data).map_err(|e| DevError::Msg(format!("Can't write temp file: {e}")))?;
        let done = self.upload_file(&tmp, data.len() as u64, name, storage, parent);
        let _ = std::fs::remove_file(&tmp);
        done
    }

    /// Sends file `from` on this computer (`size` bytes) as `name`. Returns its id.
    fn upload_file(&self, from: &std::path::Path, size: u64, name: &str, storage: u32, parent: u32) -> Result<u32> {
        let path = CString::new(from.to_string_lossy().as_bytes()).map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let cname = CString::new(name).map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let mut meta = MtpFile {
            item_id: 0,
            parent_id: parent,
            storage_id: storage,
            filename: cname.as_ptr() as *mut c_char,
            filesize: size,
            modificationdate: 0,
            filetype: FILETYPE_UNKNOWN,
            next: std::ptr::null_mut(),
        };
        let rc = unsafe { (self.lib.send_file_from_file)(self.dev, path.as_ptr(), &mut meta, Some(progress), self.stop_flag()) };
        if rc != 0 {
            return Err(self.error(&format!("Uploading {name}")));
        }
        Ok(meta.item_id)
    }

    fn delete(&self, id: u32) -> Result<()> {
        if unsafe { (self.lib.delete_object)(self.dev, id) } != 0 {
            return Err(self.error("Deleting"));
        }
        Ok(())
    }

    fn rename(&self, id: u32, name: &str) -> Result<()> {
        let meta = unsafe { (self.lib.filemetadata)(self.dev, id) };
        if meta.is_null() {
            return Err(self.error("Reading file details"));
        }
        let cname = CString::new(name).map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let rc = unsafe { (self.lib.set_file_name)(self.dev, meta, cname.as_ptr()) };
        unsafe { (self.lib.destroy_file)(meta) };
        if rc != 0 {
            return Err(self.error(&format!("Renaming to {name}")));
        }
        Ok(())
    }

    fn mkdir(&self, name: &str, storage: u32, parent: u32) -> Result<u32> {
        // libmtp may rewrite the name in place, so hand it a buffer it owns for the call.
        let raw = CString::new(name).map_err(|_| DevError::Msg("Bad folder name.".into()))?.into_raw();
        let id = unsafe { (self.lib.create_folder)(self.dev, raw, parent, storage) };
        drop(unsafe { CString::from_raw(raw) });
        if id == 0 {
            return Err(self.error(&format!("Creating folder {name}")));
        }
        Ok(id)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if !self.dev.is_null() {
            let _lib = lib_lock();
            unsafe { (self.lib.release)(self.dev) };
        }
    }
}

/// Image Capture's helpers, which take a phone's MTP interface (to import photos) as soon as
/// something asks for it, and start again by themselves when stopped.
#[cfg(target_os = "macos")]
const CAMERA_HELPERS: [&str; 3] = ["ptpcamerad", "icdd", "mscamerad-xpc"];

/// Apps that keep Image Capture's helpers on a phone while they're open: stopping the helpers
/// is no use then, as they're back at once.
#[cfg(target_os = "macos")]
const CAMERA_APPS: [&str; 2] = ["Image Capture", "Photos"];

/// Which of `CAMERA_APPS` are running.
#[cfg(target_os = "macos")]
fn camera_apps_open() -> Vec<&'static str> {
    CAMERA_APPS
        .into_iter()
        .filter(|app| crate::platform::command("pgrep").args(["-xq", app]).stdin(std::process::Stdio::null()).status().is_ok_and(|s| s.success()))
        .collect()
}

/// What to tell someone about another program holding the phone.
#[cfg(target_os = "macos")]
fn held_message(h: &crate::devices::usb::Holder) -> String {
    if CAMERA_HELPERS.contains(&h.name.as_str()) {
        match camera_apps_open().as_slice() {
            [] => format!("Image Capture's helper ({}) keeps taking the phone: quit Image Capture and Photos, then connect again.", h.name),
            apps => format!("{} is open and has the phone: quit it, then connect again.", apps.join(" and ")),
        }
    } else if h.name == "ferry-files" {
        format!("Another Ferry Files (pid {}) has the phone: disconnect it there, or quit it.", h.pid)
    } else {
        format!("{} (pid {}) is using the phone: quit it, then connect again.", h.name, h.pid)
    }
}

/// Gets the USB device at (bus, address) free of other programs before libmtp claims it: Image
/// Capture's helpers are stopped (for up to 3 s, as they come back), unless an app that brings them
/// straight back is open; anything else is an error naming it. When ioreg can't tell, it's tried
/// anyway.
#[cfg(target_os = "macos")]
fn make_way(bus: u32, address: u32, vendor: u16, product: u16) -> Result<()> {
    let me = std::process::id();
    for _ in 0..15 {
        let Some(held) = crate::devices::usb::holders(bus, address, vendor, product) else { return Ok(()) };
        let others: Vec<_> = held.into_iter().filter(|h| h.pid != me).collect();
        if others.is_empty() {
            return Ok(());
        }
        if let Some(h) = others.iter().find(|h| !CAMERA_HELPERS.contains(&h.name.as_str())) {
            return err(held_message(h));
        }
        let apps = camera_apps_open();
        if !apps.is_empty() {
            return err(format!("{} is open and has the phone: quit it, then connect again.", apps.join(" and ")));
        }
        // They ignore SIGTERM.
        for h in &others {
            let _ = crate::platform::command("kill").args(["-9", &h.pid.to_string()]).stdin(std::process::Stdio::null()).status();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    err("Image Capture keeps taking the phone (ptpcamerad): quit Image Capture and Photos, then connect again.".to_string())
}

#[cfg(not(target_os = "macos"))]
fn make_way(_bus: u32, _address: u32, _vendor: u16, _product: u16) -> Result<()> {
    Ok(())
}

/// Why opening the USB device at (bus, address) failed, when ioreg tells: another program took it
/// (Image Capture's helper, back between `make_way` and the open), or this process still holds it
/// after libmtp failed to open it.
#[cfg(target_os = "macos")]
fn why_not_open(bus: u32, address: u32, vendor: u16, product: u16) -> Option<String> {
    let held = crate::devices::usb::holders(bus, address, vendor, product)?;
    let me = std::process::id();
    if let Some(h) = held.iter().find(|h| h.pid != me) {
        return Some(held_message(h));
    }
    held.iter().any(|h| h.pid == me).then(|| {
        "Couldn't open the MTP device, and the failed tries still hold it. Unplug the phone, plug it in again, unlock it and choose File transfer.".to_string()
    })
}

#[cfg(not(target_os = "macos"))]
fn why_not_open(_bus: u32, _address: u32, _vendor: u16, _product: u16) -> Option<String> {
    None
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("ferry-files-mtp-{}-{tag}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

#[derive(Clone)]
struct Node {
    storage: Option<u32>,
    id: u32,
    dir: bool,
    size: u64,
    mtime: i64,
    name: String,
    path: String,
}

/// A device in use. Field order matters: the device's lock is released before the libmtp lock.
// A copy being played holds the link: stop it, so the device is let go of.
impl Drop for Mtp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `inner`, locked; also holds the libmtp lock while in use.
fn lock(inner: &Mutex<Inner>) -> Locked<'_> {
    let lib = lib_lock();
    Locked { inner: inner.lock().unwrap_or_else(|e| e.into_inner()), _lib: lib }
}

struct Locked<'a> {
    inner: std::sync::MutexGuard<'a, Inner>,
    _lib: LibLock,
}

impl std::ops::Deref for Locked<'_> {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

impl std::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut Inner {
        &mut self.inner
    }
}

struct Inner {
    link: Link,
    cache: HashMap<(u32, u32), (Instant, Vec<Child>)>,
}

impl Inner {
    fn children(&mut self, storage: u32, parent: u32) -> Vec<Child> {
        if let Some((at, kids)) = self.cache.get(&(storage, parent)) {
            if at.elapsed() < CACHE_TTL {
                return kids.clone();
            }
        }
        let kids = self.link.children(storage, parent);
        self.cache.insert((storage, parent), (Instant::now(), kids.clone()));
        kids
    }

    fn invalidate(&mut self, storage: u32, parent: u32) {
        self.cache.remove(&(storage, parent));
    }
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
    id: u32,
}

pub struct Mtp {
    inner: Arc<Mutex<Inner>>,
    /// The last few files copied to be played (see `crate::preview::stream`).
    streamed: Mutex<Vec<Streamed>>,
    storages: Vec<(String, u32)>,
    model: String,
    /// The link's stop flag, reachable while a copy holds `inner`.
    stop: Arc<AtomicBool>,
}

impl Mtp {
    /// Writes the file with `send` (which sends it under the name given, returning its id): under
    /// a temporary name when there's a file to replace, swapped in once it's all there.
    fn put(&self, path: &str, exists: bool, size: u64, send: &dyn Fn(&Link, &str, u32, u32) -> Result<u32>) -> Result<String> {
        self.forget_streamed();
        let mut inner = self.inner();
        let name = basename(path).to_string();
        let (sid, pid) = self.parent_of(&mut inner, path)?;
        let old = if exists { self.resolve(&mut inner, path) } else { None };
        let new_id = match old {
            None => send(&inner.link, &name, sid, pid)?,
            Some(old) => {
                let tmp_name = format!(".{name}.ferry-tmp");
                inner.invalidate(sid, pid);
                if let Some(stale) = inner.children(sid, pid).into_iter().find(|k| k.name == tmp_name) {
                    inner.link.delete(stale.id)?;
                }
                let tmp_id = send(&inner.link, &tmp_name, sid, pid)?;
                Self::check_size(&mut inner, sid, pid, tmp_id, size, &tmp_name)?;
                inner.link.delete(old.id)?;
                match inner.link.rename(tmp_id, &name) {
                    Ok(()) => tmp_id,
                    Err(_) => {
                        // Some devices can't rename: upload again under the real name.
                        inner.link.delete(tmp_id)?;
                        send(&inner.link, &name, sid, pid)?
                    }
                }
            }
        };
        let k = Self::check_size(&mut inner, sid, pid, new_id, size, &name)?;
        Ok(format!("{}:{}:{}", k.id, k.size, k.mtime))
    }

    /// Opens the MTP device at `location` (from `detect`), or the first one found.
    pub fn open(location: Option<&str>) -> Result<Mtp> {
        let lib = lib()?;
        // Opening fails for a moment after the phone is plugged in, or switched to File transfer,
        // while macOS (ptpcamerad) looks at it; so it's tried a few times.
        let mut tries = 0;
        let (dev, _lib) = loop {
            let lock = lib_lock();
            let raws = raw_devices(lib)?;
            let found = match location {
                Some(loc) => raws.iter().copied().find(|&r| self::location(r) == loc),
                None => raws.first().copied(),
            };
            let Some(raw) = found else {
                return err("No MTP device found".to_string());
            };
            let (bus, address, vendor, product) =
                unsafe { ((*raw).bus_location, (*raw).devnum as u32, (*raw).device_entry.vendor_id, (*raw).device_entry.product_id) };
            // libmtp doesn't let go of the USB device when claiming it fails (as it does while
            // another program has it), so this process would hold the phone without using it.
            make_way(bus, address, vendor, product)?;
            // Uncached: don't enumerate the whole phone on connect (slow on Android).
            let dev = unsafe { (lib.open_uncached)(raw) };
            if !dev.is_null() {
                break (dev, lock);
            }
            tries += 1;
            if tries == 4 {
                return err(why_not_open(bus, address, vendor, product).unwrap_or_else(|| "Couldn't open the MTP device".to_string()));
            }
            drop(lock);
            std::thread::sleep(std::time::Duration::from_millis(1500));
        };
        let stop = Arc::new(AtomicBool::new(false));
        let link = Link { lib, dev, partial: true, stop: stop.clone() };
        if unsafe { (lib.get_storage)(dev, 0) } != 0 {
            return Err(link.error("Reading the device's storage"));
        }
        let mut storages: Vec<(String, u32)> = Vec::new();
        let mut s = unsafe { (*dev).storage };
        while !s.is_null() {
            let (id, desc) = unsafe { ((*s).id, cstr((*s).storage_description)) };
            let desc = if desc.is_empty() { format!("Storage {id:08x}") } else { desc };
            let base = desc.replace('/', "_");
            let mut name = base.clone();
            let mut n = 2;
            while storages.iter().any(|(x, _)| *x == name) {
                name = format!("{base} ({n})");
                n += 1;
            }
            storages.push((name, id));
            s = unsafe { (*s).next };
        }
        if storages.is_empty() {
            return err("The device shows no storage. Unlock the phone and choose \"File transfer\".");
        }
        let model = [lib.friendlyname, lib.modelname].iter().map(|f| cstr(unsafe { f(dev) })).find(|s| !s.is_empty()).unwrap_or_else(|| "MTP device".into());
        Ok(Mtp { inner: Arc::new(Mutex::new(Inner { link, cache: HashMap::new() })), streamed: Mutex::new(Vec::new()), storages, model, stop })
    }

    /// The device, locked; also holds the libmtp lock while in use.
    fn inner(&self) -> Locked<'_> {
        lock(&self.inner)
    }

    /// Part of the file `s`, from its copy (started if need be).
    fn read_streamed(&self, s: Streamed, start: u64, len: u64) -> Result<Vec<u8>> {
        let inner = self.inner.clone();
        let fill = move |to: &std::path::Path, cancel: &AtomicBool| lock(&inner).link.download_until(s.id, to, cancel).map_err(|e| e.to_string());
        let (key, ext, size) = (s.key.clone(), s.ext.clone(), s.size);
        crate::preview::stream::read(&key, &ext, size, start, len, fill).map_err(DevError::Msg)
    }

    /// Writes change what's on the device: a copy being played is let go of.
    fn forget_streamed(&self) {
        self.streamed.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn storage_id(&self, name: &str) -> Option<u32> {
        self.storages.iter().find(|(n, _)| n == name).map(|(_, id)| *id)
    }

    fn resolve(&self, inner: &mut Inner, path: &str) -> Option<Node> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let Some(first) = parts.first() else {
            return Some(Node { storage: None, id: ROOT_ID, dir: true, size: 0, mtime: 0, name: "/".into(), path: "/".into() });
        };
        let sid = self.storage_id(first)?;
        let mut node = Node { storage: Some(sid), id: ROOT_ID, dir: true, size: 0, mtime: 0, name: first.to_string(), path: format!("/{first}") };
        for seg in &parts[1..] {
            if !node.dir {
                return None;
            }
            let kid = inner.children(sid, node.id).into_iter().find(|k| k.name == *seg)?;
            node = Node { storage: Some(sid), id: kid.id, dir: kid.dir, size: kid.size, mtime: kid.mtime, path: join(&node.path, seg), name: kid.name };
        }
        Some(node)
    }

    fn parent_of(&self, inner: &mut Inner, path: &str) -> Result<(u32, u32)> {
        match self.resolve(inner, &dirname(path)) {
            Some(Node { storage: Some(sid), id, dir: true, .. }) => Ok((sid, id)),
            _ => err(format!("Files must go inside a storage folder, e.g. {}", self.default_dir())),
        }
    }

    fn invalidate_parent(&self, inner: &mut Inner, path: &str) {
        if let Some(Node { storage: Some(sid), id, .. }) = self.resolve(inner, &dirname(path)) {
            inner.invalidate(sid, id);
        }
    }

    /// A whole copy of the file in the app's "opened" folder, for devices that can't send part of a
    /// file. Only the latest one is kept.
    fn version_of(n: &Node) -> String {
        format!("{}:{}:{}", n.id, n.size, n.mtime)
    }

    fn check_size(inner: &mut Inner, sid: u32, pid: u32, id: u32, size: u64, name: &str) -> Result<Child> {
        inner.invalidate(sid, pid);
        match inner.children(sid, pid).into_iter().find(|k| k.id == id) {
            Some(k) if k.size == size && k.name == name => Ok(k),
            _ => err(format!("Verification failed after writing {name}.")),
        }
    }

    fn public(n: &Node) -> Entry {
        Entry { name: n.name.clone(), path: n.path.clone(), dir: n.dir, size: n.size, mtime: n.mtime, hidden: false }
    }
}

impl Transport for Mtp {
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
        self.storages.iter().map(|(n, _)| (n.clone(), format!("/{n}"))).collect()
    }

    fn refresh(&self) {
        self.inner().cache.clear();
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let mut inner = self.inner();
        let node = match self.resolve(&mut inner, path) {
            Some(n) if n.dir => n,
            _ => return err(format!("Not a folder: {path}")),
        };
        let Some(sid) = node.storage else {
            return Ok(self
                .storages
                .iter()
                .map(|(n, _)| Entry { name: n.clone(), path: format!("/{n}"), dir: true, size: 0, mtime: 0, hidden: false })
                .collect());
        };
        Ok(inner
            .children(sid, node.id)
            .into_iter()
            .map(|k| Entry { path: join(&node.path, &k.name), name: k.name, dir: k.dir, size: k.size, mtime: k.mtime, hidden: false })
            .collect())
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let mut inner = self.inner();
        Ok(self.resolve(&mut inner, path).map(|n| Self::public(&n)))
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        let mut inner = self.inner();
        self.invalidate_parent(&mut inner, path);
        Ok(self.resolve(&mut inner, path).filter(|n| !n.dir).map(|n| Self::version_of(&n)))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        // Being copied: read from the copy, without waiting for the device the copy holds.
        let streamed = self.streamed.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|s| s.path == path).cloned();
        if let Some(s) = streamed {
            return self.read_streamed(s, start, len);
        }
        let mut inner = self.inner();
        let Some(node) = self.resolve(&mut inner, path).filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        if start >= node.size || len == 0 {
            return Ok(vec![]);
        }
        let len = len.min(node.size - start).min(u32::MAX as u64);
        if let Some(data) = inner.link.read_partial(node.id, start, len as u32) {
            return Ok(data);
        }
        // The device can't send part of a file: copy it whole, and read from the copy as it comes.
        drop(inner);
        let s = Streamed {
            path: path.to_string(),
            key: format!("mtp|{}|{}|{}", self.model, node.path, Self::version_of(&node)),
            ext: node.name.rsplit_once('.').map_or(String::new(), |(_, x)| format!(".{x}")),
            size: node.size,
            id: node.id,
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
        self.invalidate_parent(&mut inner, path);
        let Some(node) = self.resolve(&mut inner, path).filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        let data = inner.link.download(node.id)?;
        if data.len() as u64 != node.size {
            return err(format!("Read {} bytes but the device reports {}.", data.len(), node.size));
        }
        Ok((data, Self::version_of(&node)))
    }

    /// MTP can't overwrite: upload under a temporary name, delete the old file, rename.
    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        self.put(path, exists, data.len() as u64, &|link, name, sid, pid| link.upload(data, name, sid, pid))
    }

    /// libmtp writes the file straight into `to`.
    fn read_to(&self, path: &str, to: &std::path::Path, read: &mut dyn FnMut(u64), stop: &AtomicBool) -> Result<String> {
        let mut inner = self.inner();
        self.invalidate_parent(&mut inner, path);
        let Some(node) = self.resolve(&mut inner, path).filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        inner.link.download_until(node.id, to, stop)?;
        let got = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
        if got != node.size {
            return err(format!("Read {got} bytes but the device reports {}.", node.size));
        }
        read(got);
        Ok(Self::version_of(&node))
    }

    fn replace_from(&self, path: &str, from: &std::path::Path, exists: bool) -> Result<String> {
        let size = std::fs::metadata(from).map_err(|e| DevError::Msg(format!("Can't read {}: {e}", from.display())))?.len();
        self.put(path, exists, size, &|link, name, sid, pid| link.upload_file(from, size, name, sid, pid))
    }

    fn delete(&self, path: &str, _is_dir: bool) -> Result<()> {
        self.forget_streamed();
        let mut inner = self.inner();
        let Some(node) = self.resolve(&mut inner, path) else {
            return err(format!("No such file or folder: {path}"));
        };
        if node.storage.is_none() || node.id == ROOT_ID {
            return err("Storages can't be removed.");
        }
        let (sid, pid) = self.parent_of(&mut inner, path)?;
        inner.link.delete(node.id)?;
        inner.invalidate(sid, pid);
        Ok(())
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        let mut inner = self.inner();
        let (sid, pid) = self.parent_of(&mut inner, path)?;
        inner.link.mkdir(basename(path), sid, pid)?;
        inner.invalidate(sid, pid);
        Ok(())
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        self.forget_streamed();
        let mut inner = self.inner();
        let node = match self.resolve(&mut inner, path) {
            Some(n) if n.storage.is_some() && n.id != ROOT_ID => n,
            _ => return err(format!("{path} can't be renamed.")),
        };
        let (sid, pid) = self.parent_of(&mut inner, path)?;
        inner.link.rename(node.id, new_name)?;
        inner.invalidate(sid, pid);
        Ok(())
    }

    /// Free space as libmtp last read it (refreshed first).
    fn space(&self) -> Vec<crate::device::Space> {
        let inner = self.inner();
        let link = &inner.link;
        if unsafe { (link.lib.get_storage)(link.dev, 0) } != 0 {
            unsafe { (link.lib.clear_errorstack)(link.dev) };
            return vec![];
        }
        let mut out = vec![];
        let mut s = unsafe { (*link.dev).storage };
        while !s.is_null() {
            let (id, free, total) = unsafe { ((*s).id, (*s).free_space_in_bytes, (*s).max_capacity) };
            if let Some((name, _)) = self.storages.iter().find(|(_, sid)| *sid == id) {
                out.push(crate::device::Space { name: name.clone(), free, total, path: format!("/{name}") });
            }
            s = unsafe { (*s).next };
        }
        out
    }
}
