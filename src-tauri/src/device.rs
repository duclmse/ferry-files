//! Device files over a transport (adb or MTP), with the same safety rules as yomi-files.py:
//! every write refuses if the file changed since it was read, keeps a backup of the old version
//! on the computer, and is verified afterwards.

use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// Marked hidden by the system (macOS UF_HIDDEN, the Windows hidden attribute), apart from a
    /// leading dot.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

impl Entry {
    /// Whether it's hidden: a dot name, or marked hidden by the system.
    pub fn is_hidden(&self) -> bool {
        self.hidden || self.name.starts_with('.')
    }
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
    /// Up to `len` bytes of the file from `start`, without reading the rest of it; fewer at the
    /// end of the file. For streaming media into the preview.
    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>>;
    fn version(&self, path: &str) -> Result<Option<String>>;
    /// Writes the whole file (under a temporary name, then swapped in). Returns the new version.
    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String>;
    /// Copies the file into `to` on this computer, so a big file isn't held in memory, and
    /// returns its version. `read` is told the size of each piece read, and `stop` cancels
    /// between pieces. By default the whole file is read first.
    fn read_to(&self, path: &str, to: &Path, read: &mut dyn FnMut(u64), _stop: &AtomicBool) -> Result<String> {
        let (data, version) = self.read(path)?;
        read(data.len() as u64);
        std::fs::write(to, &data).map_err(|e| DevError::Msg(format!("Can't write {}: {e}", to.display())))?;
        Ok(version)
    }
    /// As `replace`, with the new contents in file `from` on this computer. By default the whole
    /// file is read first.
    fn replace_from(&self, path: &str, from: &Path, exists: bool) -> Result<String> {
        let data = std::fs::read(from).map_err(|e| DevError::Msg(format!("Can't read {}: {e}", from.display())))?;
        self.replace(path, &data, exists)
    }
    fn delete(&self, path: &str, is_dir: bool) -> Result<()>;
    fn make_dir(&self, path: &str) -> Result<()>;
    /// Renames a file or folder within its folder. The caller has checked the new name is free.
    fn rename(&self, path: &str, new_name: &str) -> Result<()>;
    /// Each storage's free and total bytes, when the transport can tell.
    fn space(&self) -> Vec<Space> {
        vec![]
    }
    fn refresh(&self) {}
    /// Names under `dir` (any depth) matching `text`, up to `max`, and whether that's all of
    /// them, from an index the transport has. None when it has none, and folders are walked.
    fn search(&self, _dir: &str, _text: &str, _regex: bool, _max: usize) -> Option<Result<(Vec<Entry>, bool)>> {
        None
    }
    /// Stops the file copies under way and any started later, for letting go of the device:
    /// it's being disconnected. Called without the device's lock, which a copy holds.
    fn stop(&self) {}
    /// Up to `count` of the folder's items from `offset`, for transports whose folders can be
    /// too big to list at once. None when the transport lists whole folders.
    /// `sort` is ("name", "size", "date" or "kind", ascending), and the second item of the
    /// answer how many items the folder has in all.
    fn listdir_page(&self, _path: &str, _offset: u64, _count: u64, _sort: (&str, bool)) -> Option<Result<(Vec<Entry>, u64)>> {
        None
    }
    /// The command that starts an interactive shell in `dir`, for the terminal (which runs it in
    /// a pseudo-terminal). None when the device has no command line.
    fn shell(&self, _dir: &str) -> Option<std::process::Command> {
        None
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Space {
    pub name: String,
    pub free: u64,
    pub total: u64,
    /// The folder the storage starts at, for telling which one a folder is on; "" when unknown.
    pub path: String,
}

/// Copies `from` into a new file `to`, a piece at a time, for `Transport::read_to`: `read` is told
/// each piece's size, and `stop` cancels. `what` names the file in errors. Gives the bytes copied.
pub fn pour(from: &mut dyn std::io::Read, to: &Path, what: &str, read: &mut dyn FnMut(u64), stop: &AtomicBool) -> Result<u64> {
    use std::io::Write;
    let mut f = std::fs::File::create(to).map_err(|e| DevError::Msg(format!("Can't write {}: {e}", to.display())))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return err("Cancelled.");
        }
        let n = match from.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return err(format!("Couldn't read {what}: {e}")),
        };
        if n == 0 {
            return Ok(total);
        }
        f.write_all(&buf[..n]).map_err(|e| DevError::Msg(format!("Can't write {}: {e}", to.display())))?;
        total += n as u64;
        read(n as u64);
    }
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
        let line = format!("{}\t{}\t{}\t{}\n", clean(&self.file), clean(&self.place), clean(&self.path), clean(&self.reason));
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(INDEX))?;
        // One write per line: with O_APPEND it lands whole, even when two backups are noted at
        // once (writeln! may split it into several writes that interleave).
        f.write_all(line.as_bytes())
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
    crate::platform::backup_dir()
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

    /// See `Transport::stop`.
    pub fn stop(&self) {
        self.t.stop();
    }

    pub fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let path = norm(path)?;
        let _g = self.lock();
        let mut entries = self.t.listdir(&path)?;
        entries.sort_by_key(|e| (!e.dir, e.name.to_lowercase()));
        Ok(entries)
    }

    /// Up to `count` of the folder's items from `offset` in the order `sort` (see
    /// `Transport::listdir_page`), and how many it has in all. The whole folder when the
    /// transport doesn't page, from `offset` 0 only.
    pub fn listdir_page(&self, path: &str, offset: u64, count: u64, sort: (&str, bool)) -> Result<(Vec<Entry>, u64)> {
        let norm_path = norm(path)?;
        let paged = {
            let _g = self.lock();
            self.t.listdir_page(&norm_path, offset, count, sort)
        };
        match paged {
            Some(r) => r,
            None if offset == 0 => self.listdir(path).map(|es| {
                let n = es.len() as u64;
                (es, n)
            }),
            None => Ok((vec![], 0)),
        }
    }

    /// See `Transport::shell`.
    pub fn shell(&self, dir: &str) -> Option<Result<std::process::Command>> {
        match norm(dir) {
            Ok(dir) => self.t.shell(&dir).map(Ok),
            Err(e) => Some(Err(e)),
        }
    }

    /// See `Transport::search`.
    pub fn search(&self, dir: &str, text: &str, regex: bool, max: usize) -> Option<Result<(Vec<Entry>, bool)>> {
        let dir = match norm(dir) {
            Ok(d) => d,
            Err(e) => return Some(Err(e)),
        };
        let _g = self.lock();
        self.t.search(&dir, text, regex, max)
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

    /// Part of a file (see `Transport::read_range`). Doesn't wait for the device's other work: a
    /// read can't break a write's check-then-write, and media playback shouldn't stall behind a
    /// long copy. (MTP still takes turns inside libmtp.)
    pub fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let path = norm(path)?;
        self.t.read_range(&path, start, len)
    }

    /// Copies a file into `to` on this computer (see `Transport::read_to`). Returns its version.
    pub fn read_to(&self, path: &str, to: &Path, read: &mut dyn FnMut(u64), stop: &AtomicBool) -> Result<String> {
        let path = norm(path)?;
        let _g = self.lock();
        let got = self.t.read_to(&path, to, read, stop);
        if got.is_err() {
            let _ = std::fs::remove_file(to);
        }
        got
    }

    /// Writes a file. With `base_version`, refuses if the device copy changed since it was read.
    /// With `create`, refuses if the file already exists.
    pub fn write(&self, path: &str, data: &[u8], base_version: Option<&str>, create: bool) -> Result<WriteResult> {
        self.write_with(path, base_version, create, |t, p, exists| t.replace(p, data, exists))
    }

    /// As `write`, with the new contents in file `from` on this computer, which isn't read into
    /// memory where the transport can help it.
    pub fn write_from(&self, path: &str, from: &Path, base_version: Option<&str>, create: bool) -> Result<WriteResult> {
        self.write_with(path, base_version, create, |t, p, exists| t.replace_from(p, from, exists))
    }

    fn write_with(
        &self,
        path: &str,
        base_version: Option<&str>,
        create: bool,
        put: impl FnOnce(&dyn Transport, &str, bool) -> Result<String>,
    ) -> Result<WriteResult> {
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
        let version = put(self.t.as_ref(), &path, current.is_some())?;
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
        // Copied straight into the backup, so a big file isn't held in memory.
        if let Err(e) = self.t.read_to(path, &dest, &mut |_| {}, &AtomicBool::new(false)) {
            let _ = std::fs::remove_file(&dest);
            return Err(e);
        }
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
        "iphone" => match serial {
            Some(udid) => Ok(Device::new(Box::new(crate::iphone::IPhone::open(&udid)?))),
            None => err("Which iPhone?"),
        },
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
