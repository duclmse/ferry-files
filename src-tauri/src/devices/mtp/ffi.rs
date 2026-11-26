//! libmtp, loaded at runtime: the structs and functions used, finding the library, and listing
//! the devices it sees.

use super::*;

/// How to get libmtp, when it isn't there.
#[cfg(target_os = "macos")]
pub(super) const INSTALL: &str = "libmtp not found. Install it with: brew install libmtp";
#[cfg(not(target_os = "macos"))]
pub(super) const INSTALL: &str =
    "libmtp not found. Install it with your package manager (libmtp9 on Debian and Ubuntu, \
libmtp on Fedora and Arch).";

/// Where libmtp may be: next to the app first (a bundle carries its own), then where it's installed.
pub(super) fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    #[cfg(target_os = "macos")]
    {
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("../Frameworks")))
        {
            out.push(dir.join("libmtp.9.dylib"));
        }
        for p in [
            "/opt/homebrew/lib/libmtp.dylib",
            "/usr/local/lib/libmtp.dylib",
            "libmtp.dylib",
        ] {
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
pub(super) struct MtpFile {
    pub(super) item_id: u32,
    pub(super) parent_id: u32,
    pub(super) storage_id: u32,
    pub(super) filename: *mut c_char,
    pub(super) filesize: u64,
    pub(super) modificationdate: i64, // time_t
    pub(super) filetype: c_int,
    pub(super) next: *mut MtpFile,
}

#[repr(C)]
pub(super) struct MtpStorage {
    pub(super) id: u32,
    pub(super) storage_type: u16,
    pub(super) filesystem_type: u16,
    pub(super) access_capability: u16,
    pub(super) max_capacity: u64,
    pub(super) free_space_in_bytes: u64,
    pub(super) free_space_in_objects: u64,
    pub(super) storage_description: *mut c_char,
    pub(super) volume_identifier: *mut c_char,
    pub(super) next: *mut MtpStorage,
    pub(super) prev: *mut MtpStorage,
}

#[repr(C)]
pub(super) struct MtpError {
    pub(super) errornumber: c_int,
    pub(super) error_text: *mut c_char,
    pub(super) next: *mut MtpError,
}

#[repr(C)]
pub(super) struct MtpDevice {
    // Leading fields only; libmtp allocates this struct.
    pub(super) object_bitsize: u8,
    pub(super) params: *mut c_void,
    pub(super) usbinfo: *mut c_void,
    pub(super) storage: *mut MtpStorage,
    pub(super) errorstack: *mut MtpError,
}

#[repr(C)]
pub(super) struct DeviceEntry {
    pub(super) vendor: *mut c_char,
    pub(super) vendor_id: u16,
    pub(super) product: *mut c_char,
    pub(super) product_id: u16,
    pub(super) device_flags: u32,
}

#[repr(C)]
pub(super) struct RawDevice {
    pub(super) device_entry: DeviceEntry,
    pub(super) bus_location: u32,
    pub(super) devnum: u8,
}

pub(super) type Dev = *mut MtpDevice;

pub(super) struct Lib {
    pub(super) detect: unsafe extern "C" fn(*mut *mut RawDevice, *mut c_int) -> c_int,
    pub(super) open_uncached: unsafe extern "C" fn(*mut RawDevice) -> Dev,
    pub(super) release: unsafe extern "C" fn(Dev),
    pub(super) modelname: unsafe extern "C" fn(Dev) -> *mut c_char,
    pub(super) friendlyname: unsafe extern "C" fn(Dev) -> *mut c_char,
    pub(super) get_storage: unsafe extern "C" fn(Dev, c_int) -> c_int,
    pub(super) errorstack: unsafe extern "C" fn(Dev) -> *mut MtpError,
    pub(super) clear_errorstack: unsafe extern "C" fn(Dev),
    pub(super) files_and_folders: unsafe extern "C" fn(Dev, u32, u32) -> *mut MtpFile,
    pub(super) filemetadata: unsafe extern "C" fn(Dev, u32) -> *mut MtpFile,
    pub(super) destroy_file: unsafe extern "C" fn(*mut MtpFile),
    pub(super) get_file_to_file:
        unsafe extern "C" fn(Dev, u32, *const c_char, Option<Progress>, *const c_void) -> c_int,
    pub(super) send_file_from_file: unsafe extern "C" fn(
        Dev,
        *const c_char,
        *mut MtpFile,
        Option<Progress>,
        *const c_void,
    ) -> c_int,
    pub(super) set_file_name: unsafe extern "C" fn(Dev, *mut MtpFile, *const c_char) -> c_int,
    pub(super) delete_object: unsafe extern "C" fn(Dev, u32) -> c_int,
    pub(super) create_folder: unsafe extern "C" fn(Dev, *mut c_char, u32, u32) -> u32,
    /// `LIBMTP_GetPartialObject`; missing from old libmtp builds.
    pub(super) get_partial:
        Option<unsafe extern "C" fn(Dev, u32, u64, u32, *mut *mut u8, *mut u32) -> c_int>,
}

/// `LIBMTP_progressfunc_t`: called as a file is copied; anything but 0 stops the copy.
pub(super) type Progress = extern "C" fn(sent: u64, total: u64, data: *const c_void) -> c_int;

/// Set when the app quits: file copies to and from the phone stop at their next piece, so the
/// phone can be let go of without waiting for them.
pub(super) static QUITTING: AtomicBool = AtomicBool::new(false);

/// Stops the file copies under way, for quitting.
pub fn stop_copies() {
    QUITTING.store(true, Ordering::Relaxed);
}

/// `data` is the device's stop flag (`Link::stop`).
pub(super) extern "C" fn progress(_sent: u64, _total: u64, data: *const c_void) -> c_int {
    let stopped =
        !data.is_null() && unsafe { (*data.cast::<AtomicBool>()).load(Ordering::Relaxed) };
    (stopped || QUITTING.load(Ordering::Relaxed)) as c_int
}

/// `data` is a `[&AtomicBool; 2]`: the device's stop flag and the copy's own.
pub(super) extern "C" fn progress_either(_sent: u64, _total: u64, data: *const c_void) -> c_int {
    let flags = unsafe { &*data.cast::<[&AtomicBool; 2]>() };
    (flags.iter().any(|f| f.load(Ordering::Relaxed)) || QUITTING.load(Ordering::Relaxed)) as c_int
}

extern "C" {
    /// libmtp hands back buffers from malloc.
    pub(super) fn free(p: *mut c_void);
}

/// libmtp isn't thread-safe, and several devices can be open at once (each used from its own
/// thread), so every libmtp call holds this one lock. Taking it again on the same thread is a
/// no-op, so code that already holds it (e.g. `open` dropping a half-opened device) can't deadlock.
pub(super) struct LibLock(Option<std::sync::MutexGuard<'static, ()>>);

thread_local! {
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn lib_lock() -> LibLock {
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

pub(super) fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load)
        .as_ref()
        .map_err(|e| DevError::Msg(e.clone()))
}

pub(super) fn load() -> std::result::Result<Lib, String> {
    let library = candidates()
        .iter()
        .find_map(|p| unsafe { libloading::Library::new(p) }.ok())
        .ok_or(INSTALL)?;
    // The library stays loaded for the life of the app.
    let library: &'static libloading::Library = Box::leak(Box::new(library));
    macro_rules! sym {
        ($name:literal) => {
            *unsafe { library.get(concat!($name, "\0").as_bytes()) }
                .map_err(|e| format!("libmtp is missing {}: {e}", $name))?
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
        get_partial: unsafe { library.get(b"LIBMTP_GetPartialObject\0") }
            .ok()
            .map(|f| *f),
    };
    let init: unsafe extern "C" fn() = sym!("LIBMTP_Init");
    unsafe { init() };
    Ok(lib)
}

pub(super) fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

pub(super) fn raw_devices(lib: &Lib) -> Result<Vec<*mut RawDevice>> {
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
    Ok((0..count as usize)
        .map(|i| unsafe { raws.add(i) })
        .collect())
}

/// Where a raw device is on USB ("bus-devnum"): tells two connected devices apart.
pub(super) fn location(r: *mut RawDevice) -> String {
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
    let usb = if raws.is_empty() {
        vec![]
    } else {
        crate::devices::usb::list()
    };
    Ok(raws
        .into_iter()
        .map(|r| {
            let (e, bus, devnum) = unsafe { (&(*r).device_entry, (*r).bus_location, (*r).devnum) };
            let name = format!("{} {}", cstr(e.vendor), cstr(e.product))
                .trim()
                .to_string();
            let name = if name.is_empty() {
                format!("{:04x}:{:04x}", e.vendor_id, e.product_id)
            } else {
                name
            };
            Detected {
                location: location(r),
                name,
                serial: crate::devices::usb::serial_of(
                    &usb,
                    bus,
                    devnum as u32,
                    e.vendor_id,
                    e.product_id,
                ),
            }
        })
        .collect())
}
