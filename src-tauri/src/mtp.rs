//! MTP transport over libmtp (loaded at runtime, so the app still starts without it).
//! Paths look like /<storage description>/<folders...>/<file>, e.g. "/Internal storage/Download".

use crate::device::{basename, dirname, err, join, DevError, Entry, Result, Transport};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ROOT_ID: u32 = 0xFFFF_FFFF; // LIBMTP_FILES_AND_FOLDERS_ROOT
const FILETYPE_FOLDER: c_int = 0; // LIBMTP_FILETYPE_FOLDER
const FILETYPE_UNKNOWN: c_int = 44; // LIBMTP_FILETYPE_UNKNOWN (libmtp 1.1.23)
const ERROR_NO_DEVICE_ATTACHED: c_int = 5;
const CACHE_TTL: Duration = Duration::from_secs(3);

pub const MAC_MTP_HINT: &str = "Check that the phone is unlocked, connected with a data cable, and set to \
\"File transfer\" (USB notification on the phone). On macOS, quit Android File Transfer, Image Capture, \
Preview and yomi-files.py, and if it still fails run: killall ptpcamerad";

// ---- libmtp structs (only what we use; layouts match libmtp 1.1.x on 64-bit macOS) ----------

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
    get_file_to_file: unsafe extern "C" fn(Dev, u32, *const c_char, *mut c_void, *mut c_void) -> c_int,
    send_file_from_file: unsafe extern "C" fn(Dev, *const c_char, *mut MtpFile, *mut c_void, *mut c_void) -> c_int,
    set_file_name: unsafe extern "C" fn(Dev, *mut MtpFile, *const c_char) -> c_int,
    delete_object: unsafe extern "C" fn(Dev, u32) -> c_int,
    create_folder: unsafe extern "C" fn(Dev, *mut c_char, u32, u32) -> u32,
}

fn lib() -> Result<&'static Lib> {
    static LIB: OnceLock<std::result::Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(|e| DevError::Msg(e.clone()))
}

fn load() -> std::result::Result<Lib, String> {
    let candidates = ["/opt/homebrew/lib/libmtp.dylib", "/usr/local/lib/libmtp.dylib", "libmtp.dylib"];
    let library = candidates
        .iter()
        .find_map(|p| unsafe { libloading::Library::new(p) }.ok())
        .ok_or("libmtp not found. Install it with: brew install libmtp")?;
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
    };
    let init: unsafe extern "C" fn() = sym!("LIBMTP_Init");
    unsafe { init() };
    Ok(lib)
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
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

/// Names of MTP devices on USB, without opening them.
pub fn detect() -> Result<Vec<String>> {
    let lib = lib()?;
    Ok(raw_devices(lib)?
        .into_iter()
        .map(|r| {
            let e = unsafe { &(*r).device_entry };
            let name = format!("{} {}", cstr(e.vendor), cstr(e.product)).trim().to_string();
            if name.is_empty() { format!("{:04x}:{:04x}", e.vendor_id, e.product_id) } else { name }
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
}

// The raw device pointer is only touched while holding the mutex in `Mtp`.
unsafe impl Send for Link {}

impl Link {
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
                (
                    Child {
                        id: r.item_id,
                        name: cstr(r.filename),
                        dir: r.filetype == FILETYPE_FOLDER,
                        size: r.filesize,
                        mtime: r.modificationdate,
                    },
                    r.next,
                )
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
        let c = CString::new(tmp.to_string_lossy().as_bytes()).unwrap();
        let rc = unsafe { (self.lib.get_file_to_file)(self.dev, id, c.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut()) };
        let data = if rc == 0 { std::fs::read(&tmp).map_err(|e| DevError::Msg(e.to_string())) } else { Err(self.error("Reading the file")) };
        let _ = std::fs::remove_file(&tmp);
        data
    }

    fn upload(&self, data: &[u8], name: &str, storage: u32, parent: u32) -> Result<u32> {
        let tmp = temp_path("ul");
        std::fs::write(&tmp, data).map_err(|e| DevError::Msg(format!("Can't write temp file: {e}")))?;
        let path = CString::new(tmp.to_string_lossy().as_bytes()).unwrap();
        let cname = CString::new(name).map_err(|_| DevError::Msg("Bad file name.".into()))?;
        let mut meta = MtpFile {
            item_id: 0,
            parent_id: parent,
            storage_id: storage,
            filename: cname.as_ptr() as *mut c_char,
            filesize: data.len() as u64,
            modificationdate: 0,
            filetype: FILETYPE_UNKNOWN,
            next: std::ptr::null_mut(),
        };
        let rc = unsafe {
            (self.lib.send_file_from_file)(self.dev, path.as_ptr(), &mut meta, std::ptr::null_mut(), std::ptr::null_mut())
        };
        let _ = std::fs::remove_file(&tmp);
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
            unsafe { (self.lib.release)(self.dev) };
        }
    }
}

fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!("yomi-files-mtp-{}-{tag}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
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

pub struct Mtp {
    inner: Mutex<Inner>,
    storages: Vec<(String, u32)>,
    model: String,
}

impl Mtp {
    pub fn open() -> Result<Mtp> {
        let lib = lib()?;
        let raws = raw_devices(lib)?;
        let Some(&raw) = raws.first() else {
            return err(format!("No MTP device found. {MAC_MTP_HINT}"));
        };
        // Uncached: don't enumerate the whole phone on connect (slow on Android).
        let dev = unsafe { (lib.open_uncached)(raw) };
        if dev.is_null() {
            return err(format!("Couldn't open the MTP device. {MAC_MTP_HINT}"));
        }
        let link = Link { lib, dev };
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
        let model = [lib.friendlyname, lib.modelname]
            .iter()
            .map(|f| cstr(unsafe { f(dev) }))
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| "MTP device".into());
        Ok(Mtp { inner: Mutex::new(Inner { link, cache: HashMap::new() }), storages, model })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
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
        Entry { name: n.name.clone(), path: n.path.clone(), dir: n.dir, size: n.size, mtime: n.mtime }
    }
}

impl Transport for Mtp {
    fn kind(&self) -> &'static str { "mtp" }
    fn label(&self) -> String { self.model.clone() }
    fn default_dir(&self) -> String { format!("/{}", self.storages[0].0) }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        let mut dirs: Vec<(String, String)> = self.storages.iter().map(|(n, _)| (n.clone(), format!("/{n}"))).collect();
        dirs.push(("Download".into(), format!("{}/Download", self.default_dir())));
        dirs
    }

    fn refresh(&self) {
        self.inner().cache.clear();
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
                .map(|(n, _)| Entry { name: n.clone(), path: format!("/{n}"), dir: true, size: 0, mtime: 0 })
                .collect());
        };
        Ok(inner
            .children(sid, node.id)
            .into_iter()
            .map(|k| Entry { path: join(&node.path, &k.name), name: k.name, dir: k.dir, size: k.size, mtime: k.mtime })
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
        let mut inner = self.inner();
        let name = basename(path).to_string();
        let (sid, pid) = self.parent_of(&mut inner, path)?;
        let old = if exists { self.resolve(&mut inner, path) } else { None };
        let new_id = match old {
            None => inner.link.upload(data, &name, sid, pid)?,
            Some(old) => {
                let tmp_name = format!(".{name}.yomi-tmp");
                inner.invalidate(sid, pid);
                if let Some(stale) = inner.children(sid, pid).into_iter().find(|k| k.name == tmp_name) {
                    inner.link.delete(stale.id)?;
                }
                let tmp_id = inner.link.upload(data, &tmp_name, sid, pid)?;
                Self::check_size(&mut inner, sid, pid, tmp_id, data.len() as u64, &tmp_name)?;
                inner.link.delete(old.id)?;
                match inner.link.rename(tmp_id, &name) {
                    Ok(()) => tmp_id,
                    Err(_) => {
                        // Some devices can't rename: upload again under the real name.
                        inner.link.delete(tmp_id)?;
                        inner.link.upload(data, &name, sid, pid)?
                    }
                }
            }
        };
        let k = Self::check_size(&mut inner, sid, pid, new_id, data.len() as u64, &name)?;
        Ok(format!("{}:{}:{}", k.id, k.size, k.mtime))
    }

    fn delete(&self, path: &str, _is_dir: bool) -> Result<()> {
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
}
