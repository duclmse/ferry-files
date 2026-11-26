//! MTP transport over libmtp (loaded at runtime, so the app still starts without it).
//! Paths look like /<storage description>/<folders...>/<file>, e.g. "/Internal storage/Download".

use crate::device::{basename, dirname, err, join, DevError, Entry, Result, Transport};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod ffi;
mod held;
mod link;

use ffi::*;
pub(crate) use ffi::{detect, stop_copies};
use held::*;
use link::*;

const ROOT_ID: u32 = 0xFFFF_FFFF; // LIBMTP_FILES_AND_FOLDERS_ROOT
const FILETYPE_FOLDER: c_int = 0; // LIBMTP_FILETYPE_FOLDER
const FILETYPE_UNKNOWN: c_int = 44; // LIBMTP_FILETYPE_UNKNOWN (libmtp 1.1.23)
const ERROR_NO_DEVICE_ATTACHED: c_int = 5;
const CACHE_TTL: Duration = Duration::from_secs(3);

fn temp_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "ferry-files-mtp-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
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
    Locked {
        inner: inner.lock().unwrap_or_else(|e| e.into_inner()),
        _lib: lib,
    }
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
        self.cache
            .insert((storage, parent), (Instant::now(), kids.clone()));
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
    fn put(
        &self,
        path: &str,
        exists: bool,
        size: u64,
        send: &dyn Fn(&Link, &str, u32, u32) -> Result<u32>,
    ) -> Result<String> {
        self.forget_streamed();
        let mut inner = self.inner();
        let name = basename(path).to_string();
        let (sid, pid) = self.parent_of(&mut inner, path)?;
        let old = if exists {
            self.resolve(&mut inner, path)
        } else {
            None
        };
        let new_id = match old {
            None => send(&inner.link, &name, sid, pid)?,
            Some(old) => {
                let tmp_name = format!(".{name}.ferry-tmp");
                inner.invalidate(sid, pid);
                if let Some(stale) = inner
                    .children(sid, pid)
                    .into_iter()
                    .find(|k| k.name == tmp_name)
                {
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
            let (bus, address, vendor, product) = unsafe {
                (
                    (*raw).bus_location,
                    (*raw).devnum as u32,
                    (*raw).device_entry.vendor_id,
                    (*raw).device_entry.product_id,
                )
            };
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
                return err(why_not_open(bus, address, vendor, product)
                    .unwrap_or_else(|| "Couldn't open the MTP device".to_string()));
            }
            drop(lock);
            std::thread::sleep(std::time::Duration::from_millis(1500));
        };
        let stop = Arc::new(AtomicBool::new(false));
        let link = Link {
            lib,
            dev,
            partial: true,
            stop: stop.clone(),
        };
        if unsafe { (lib.get_storage)(dev, 0) } != 0 {
            return Err(link.error("Reading the device's storage"));
        }
        let mut storages: Vec<(String, u32)> = Vec::new();
        let mut s = unsafe { (*dev).storage };
        while !s.is_null() {
            let (id, desc) = unsafe { ((*s).id, cstr((*s).storage_description)) };
            let desc = if desc.is_empty() {
                format!("Storage {id:08x}")
            } else {
                desc
            };
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
            return err(
                "The device shows no storage. Unlock the phone and choose \"File transfer\".",
            );
        }
        let model = [lib.friendlyname, lib.modelname]
            .iter()
            .map(|f| cstr(unsafe { f(dev) }))
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| "MTP device".into());
        Ok(Mtp {
            inner: Arc::new(Mutex::new(Inner {
                link,
                cache: HashMap::new(),
            })),
            streamed: Mutex::new(Vec::new()),
            storages,
            model,
            stop,
        })
    }

    /// The device, locked; also holds the libmtp lock while in use.
    fn inner(&self) -> Locked<'_> {
        lock(&self.inner)
    }

    /// Part of the file `s`, from its copy (started if need be).
    fn read_streamed(&self, s: Streamed, start: u64, len: u64) -> Result<Vec<u8>> {
        let inner = self.inner.clone();
        let fill = move |to: &std::path::Path, cancel: &AtomicBool| {
            lock(&inner)
                .link
                .download_until(s.id, to, cancel)
                .map_err(|e| e.to_string())
        };
        let (key, ext, size) = (s.key.clone(), s.ext.clone(), s.size);
        crate::preview::stream::read(&key, &ext, size, start, len, fill).map_err(DevError::Msg)
    }

    /// Writes change what's on the device: a copy being played is let go of.
    fn forget_streamed(&self) {
        self.streamed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn storage_id(&self, name: &str) -> Option<u32> {
        self.storages
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
    }

    fn resolve(&self, inner: &mut Inner, path: &str) -> Option<Node> {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let Some(first) = parts.first() else {
            return Some(Node {
                storage: None,
                id: ROOT_ID,
                dir: true,
                size: 0,
                mtime: 0,
                name: "/".into(),
                path: "/".into(),
            });
        };
        let sid = self.storage_id(first)?;
        let mut node = Node {
            storage: Some(sid),
            id: ROOT_ID,
            dir: true,
            size: 0,
            mtime: 0,
            name: first.to_string(),
            path: format!("/{first}"),
        };
        for seg in &parts[1..] {
            if !node.dir {
                return None;
            }
            let kid = inner
                .children(sid, node.id)
                .into_iter()
                .find(|k| k.name == *seg)?;
            node = Node {
                storage: Some(sid),
                id: kid.id,
                dir: kid.dir,
                size: kid.size,
                mtime: kid.mtime,
                path: join(&node.path, seg),
                name: kid.name,
            };
        }
        Some(node)
    }

    fn parent_of(&self, inner: &mut Inner, path: &str) -> Result<(u32, u32)> {
        match self.resolve(inner, &dirname(path)) {
            Some(Node {
                storage: Some(sid),
                id,
                dir: true,
                ..
            }) => Ok((sid, id)),
            _ => err(format!(
                "Files must go inside a storage folder, e.g. {}",
                self.default_dir()
            )),
        }
    }

    fn invalidate_parent(&self, inner: &mut Inner, path: &str) {
        if let Some(Node {
            storage: Some(sid),
            id,
            ..
        }) = self.resolve(inner, &dirname(path))
        {
            inner.invalidate(sid, id);
        }
    }

    /// A whole copy of the file in the app's "opened" folder, for devices that can't send part of a
    /// file. Only the latest one is kept.
    fn version_of(n: &Node) -> String {
        format!("{}:{}:{}", n.id, n.size, n.mtime)
    }

    fn check_size(
        inner: &mut Inner,
        sid: u32,
        pid: u32,
        id: u32,
        size: u64,
        name: &str,
    ) -> Result<Child> {
        inner.invalidate(sid, pid);
        match inner.children(sid, pid).into_iter().find(|k| k.id == id) {
            Some(k) if k.size == size && k.name == name => Ok(k),
            _ => err(format!("Verification failed after writing {name}.")),
        }
    }

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
        self.storages
            .iter()
            .map(|(n, _)| (n.clone(), format!("/{n}")))
            .collect()
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
                .map(|(n, _)| Entry {
                    name: n.clone(),
                    path: format!("/{n}"),
                    dir: true,
                    size: 0,
                    mtime: 0,
                    hidden: false,
                })
                .collect());
        };
        Ok(inner
            .children(sid, node.id)
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
        Ok(self.resolve(&mut inner, path).map(|n| Self::public(&n)))
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        let mut inner = self.inner();
        self.invalidate_parent(&mut inner, path);
        Ok(self
            .resolve(&mut inner, path)
            .filter(|n| !n.dir)
            .map(|n| Self::version_of(&n)))
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
            key: format!(
                "mtp|{}|{}|{}",
                self.model,
                node.path,
                Self::version_of(&node)
            ),
            ext: node
                .name
                .rsplit_once('.')
                .map_or(String::new(), |(_, x)| format!(".{x}")),
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
            return err(format!(
                "Read {} bytes but the device reports {}.",
                data.len(),
                node.size
            ));
        }
        Ok((data, Self::version_of(&node)))
    }

    /// MTP can't overwrite: upload under a temporary name, delete the old file, rename.
    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        self.put(path, exists, data.len() as u64, &|link, name, sid, pid| {
            link.upload(data, name, sid, pid)
        })
    }

    /// libmtp writes the file straight into `to`.
    fn read_to(
        &self,
        path: &str,
        to: &std::path::Path,
        read: &mut dyn FnMut(u64),
        stop: &AtomicBool,
    ) -> Result<String> {
        let mut inner = self.inner();
        self.invalidate_parent(&mut inner, path);
        let Some(node) = self.resolve(&mut inner, path).filter(|n| !n.dir) else {
            return err(format!("No such file: {path}"));
        };
        inner.link.download_until(node.id, to, stop)?;
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

    fn replace_from(&self, path: &str, from: &std::path::Path, exists: bool) -> Result<String> {
        let size = std::fs::metadata(from)
            .map_err(|e| DevError::Msg(format!("Can't read {}: {e}", from.display())))?
            .len();
        self.put(path, exists, size, &|link, name, sid, pid| {
            link.upload_file(from, size, name, sid, pid)
        })
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
            let (id, free, total) =
                unsafe { ((*s).id, (*s).free_space_in_bytes, (*s).max_capacity) };
            if let Some((name, _)) = self.storages.iter().find(|(_, sid)| *sid == id) {
                out.push(crate::device::Space {
                    name: name.clone(),
                    free,
                    total,
                    path: format!("/{name}"),
                });
            }
            s = unsafe { (*s).next };
        }
        out
    }
}
