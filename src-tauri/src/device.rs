//! Device files over a transport (adb or MTP), with the same safety rules as yomi-files.py:
//! every write refuses if the file changed since it was read, keeps a backup of the old version
//! on the Mac, and is verified afterwards.

use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug)]
pub enum DevError {
    Msg(String),
    /// The file changed on the device, or already exists when creating.
    Conflict(String),
}

impl fmt::Display for DevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DevError::Msg(m) | DevError::Conflict(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, DevError>;

pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(DevError::Msg(msg.into()))
}

/// What each transport implements. Paths passed in are already normalised by `norm`.
pub trait Transport: Send + Sync {
    fn kind(&self) -> &'static str;
    fn label(&self) -> String;
    fn default_dir(&self) -> String;
    fn quick_dirs(&self) -> Vec<(String, String)>;
    fn listdir(&self, path: &str) -> Result<Vec<Entry>>;
    fn stat(&self, path: &str) -> Result<Option<Entry>>;
    /// The file's bytes and a version string that changes whenever the file changes.
    fn read(&self, path: &str) -> Result<(Vec<u8>, String)>;
    fn version(&self, path: &str) -> Result<Option<String>>;
    /// Writes the whole file (under a temporary name, then swapped in). Returns the new version.
    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String>;
    fn delete(&self, path: &str, is_dir: bool) -> Result<()>;
    fn make_dir(&self, path: &str) -> Result<()>;
    /// Renames a file or folder within its folder. The caller has checked the new name is free.
    fn rename(&self, path: &str, new_name: &str) -> Result<()>;
    /// Each storage's free and total bytes, when the transport can tell.
    fn space(&self) -> Vec<Space> {
        vec![]
    }
    fn refresh(&self) {}
}

#[derive(Clone, Debug, Serialize)]
pub struct Space {
    pub name: String,
    pub free: u64,
    pub total: u64,
}

/// Absolute, normalised device path ("/a/b", never a trailing slash except "/").
pub fn norm(path: &str) -> Result<String> {
    if !path.starts_with('/') {
        return err(format!("Use an absolute device path, not {path:?}."));
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

pub fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// One line of the backups index (`index.tsv` in the backups folder): the backup's file name,
/// where the file was ("mac" or "adb:Pixel 7"), its path there, and "replaced" or "deleted".
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BackupNote {
    pub file: String,
    pub place: String,
    pub path: String,
    pub reason: String,
}

const INDEX: &str = "index.tsv";

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl BackupNote {
    fn append(&self, dir: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(INDEX))?;
        writeln!(f, "{}\t{}\t{}\t{}", clean(&self.file), clean(&self.place), clean(&self.path), clean(&self.reason))
    }

    /// The notes in the index, by backup file name.
    pub fn read_all(dir: &std::path::Path) -> std::collections::HashMap<String, BackupNote> {
        let text = std::fs::read_to_string(dir.join(INDEX)).unwrap_or_default();
        text.lines()
            .filter_map(|l| {
                let mut c = l.split('\t');
                let n = BackupNote { file: c.next()?.into(), place: c.next()?.into(), path: c.next()?.into(), reason: c.next()?.into() };
                Some((n.file.clone(), n))
            })
            .collect()
    }
}

pub fn backup_dir() -> PathBuf {
    // Same folder as yomi-files.py, so backups from both tools are in one place.
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("Library/Caches/yomi-files/backups")
}

#[derive(Serialize)]
pub struct WriteResult {
    pub version: String,
    pub backup: Option<String>,
}

/// A connected device. All operations are serialised: libmtp isn't thread-safe, and the
/// check-then-write sequence must not interleave with another write.
pub struct Device {
    t: Box<dyn Transport>,
    op: Mutex<()>,
}

impl Device {
    pub fn new(t: Box<dyn Transport>) -> Self {
        Device { t, op: Mutex::new(()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.op.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn kind(&self) -> &'static str { self.t.kind() }
    pub fn label(&self) -> String { self.t.label() }
    pub fn default_dir(&self) -> String { self.t.default_dir() }
    pub fn quick_dirs(&self) -> Vec<(String, String)> { self.t.quick_dirs() }

    pub fn refresh(&self) {
        let _g = self.lock();
        self.t.refresh();
    }

    pub fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let path = norm(path)?;
        let _g = self.lock();
        let mut entries = self.t.listdir(&path)?;
        entries.sort_by_key(|e| (!e.dir, e.name.to_lowercase()));
        Ok(entries)
    }

    pub fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let path = norm(path)?;
        let _g = self.lock();
        self.t.stat(&path)
    }

    pub fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let path = norm(path)?;
        let _g = self.lock();
        self.t.read(&path)
    }

    /// Writes a file. With `base_version`, refuses if the device copy changed since it was read.
    /// With `create`, refuses if the file already exists.
    pub fn write(&self, path: &str, data: &[u8], base_version: Option<&str>, create: bool) -> Result<WriteResult> {
        let path = norm(path)?;
        let _g = self.lock();
        let st = self.t.stat(&path)?;
        if st.as_ref().is_some_and(|e| e.dir) {
            return err(format!("{path} is a folder."));
        }
        let current = if st.is_some() { self.t.version(&path)? } else { None };
        if create && current.is_some() {
            return Err(DevError::Conflict(format!("{path} already exists on the device.")));
        }
        if let Some(base) = base_version {
            if current.as_deref() != Some(base) {
                return Err(DevError::Conflict(format!(
                    "{path} changed on the device since it was opened. Not overwritten."
                )));
            }
        }
        let parent = dirname(&path);
        if !self.t.stat(&parent)?.is_some_and(|e| e.dir) {
            return err(format!("Folder doesn't exist: {parent}"));
        }
        let backup = if current.is_some() { Some(self.backup(&path, "replaced")?) } else { None };
        let version = self.t.replace(&path, data, current.is_some())?;
        Ok(WriteResult { version, backup: backup.map(|p| p.display().to_string()) })
    }

    /// Deletes a file (backed up first) or an empty folder. Returns the backup path.
    pub fn remove(&self, path: &str) -> Result<Option<String>> {
        let path = norm(path)?;
        let _g = self.lock();
        let Some(st) = self.t.stat(&path)? else {
            return err(format!("No such file or folder: {path}"));
        };
        if st.dir {
            if !self.t.listdir(&path)?.is_empty() {
                return err(format!("{path} isn't empty. Only empty folders can be removed."));
            }
            self.t.delete(&path, true)?;
            return Ok(None);
        }
        let backup = self.backup(&path, "deleted")?;
        self.t.delete(&path, false)?;
        Ok(Some(backup.display().to_string()))
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        let path = norm(path)?;
        let _g = self.lock();
        if self.t.stat(&path)?.is_some() {
            return err(format!("{path} already exists."));
        }
        let parent = dirname(&path);
        if !self.t.stat(&parent)?.is_some_and(|e| e.dir) {
            return err(format!("Folder doesn't exist: {parent}"));
        }
        self.t.make_dir(&path)
    }

    /// Renames a file or folder in place. Refuses if the new name is taken. Returns the new path.
    pub fn rename(&self, path: &str, new_name: &str) -> Result<String> {
        let path = norm(path)?;
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.contains('/') || new_name == "." || new_name == ".." {
            return err("Enter a name without /.");
        }
        let target = join(&dirname(&path), new_name);
        let _g = self.lock();
        if self.t.stat(&path)?.is_none() {
            return err(format!("No such file or folder: {path}"));
        }
        // A change of case only ("a.txt" to "A.txt") is the same file on the Mac's disk.
        if target.to_lowercase() != path.to_lowercase() && self.t.stat(&target)?.is_some() {
            return Err(DevError::Conflict(format!("{new_name} already exists there.")));
        }
        self.t.rename(&path, new_name)?;
        self.t.refresh();
        Ok(target)
    }

    pub fn space(&self) -> Vec<Space> {
        let _g = self.lock();
        self.t.space()
    }

    /// "mac" for the Mac, otherwise the transport and device name ("adb:Pixel 7"), as the
    /// frontend names places.
    pub fn place(&self) -> String {
        match self.t.kind() {
            "mac" => "mac".into(),
            k => format!("{k}:{}", self.t.label()),
        }
    }

    /// The current version of a file, or None if there's no file there.
    pub fn version(&self, path: &str) -> Result<Option<String>> {
        let path = norm(path)?;
        let _g = self.lock();
        self.t.version(&path)
    }

    fn backup(&self, path: &str, reason: &str) -> Result<PathBuf> {
        let (data, _) = self.t.read(path)?;
        let dir = backup_dir();
        std::fs::create_dir_all(&dir).map_err(|e| DevError::Msg(format!("Can't create {}: {e}", dir.display())))?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let name = basename(path);
        let mut dest = dir.join(format!("{stamp}-{name}"));
        let mut n = 1;
        while dest.exists() {
            dest = dir.join(format!("{stamp}-{n}-{name}"));
            n += 1;
        }
        std::fs::write(&dest, data).map_err(|e| DevError::Msg(format!("Can't write backup {}: {e}", dest.display())))?;
        // Where it came from, so it can be put back. The backup itself is what matters, so a
        // failure to note it isn't an error.
        let line = BackupNote { file: file_name(&dest), place: self.place(), path: path.to_string(), reason: reason.into() };
        let _ = line.append(&dir);
        Ok(dest)
    }
}

/// Connects over "adb", "mtp" or "auto" (adb when a device is ready for it). `which` picks the
/// device: an adb serial, or an MTP location from `mtp::detect`; None takes the first one.
pub fn connect(transport: &str, which: Option<String>) -> Result<Device> {
    let serial = which;
    let use_adb = transport == "adb"
        || (transport == "auto" && (serial.is_some() || !crate::adb::ready_serials().is_empty()));
    if use_adb {
        return Ok(Device::new(Box::new(crate::adb::Adb::open(serial)?)));
    }
    match transport {
        "mtp" => Ok(Device::new(Box::new(crate::mtp::Mtp::open(serial.as_deref())?))),
        "auto" => crate::mtp::Mtp::open(None).map(|d| Device::new(Box::new(d))).map_err(|e| {
            DevError::Msg(format!("No device found. For adb: turn on USB debugging. For MTP: {e}"))
        }),
        other => err(format!("Unknown transport {other:?}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_paths() {
        assert_eq!(norm("/").unwrap(), "/");
        assert_eq!(norm("/a/./b/../c/").unwrap(), "/a/c");
        assert_eq!(norm("/..").unwrap(), "/");
        assert!(norm("a/b").is_err());
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(basename("/a/b.txt"), "b.txt");
        assert_eq!(join("/", "x"), "/x");
        assert_eq!(join("/a", "x"), "/a/x");
    }
}
