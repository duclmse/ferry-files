//! The computer's own files, as a transport. Wrapped in a `Device`, so writes and deletes get the
//! same rules as on the phone: refuse if the file changed since it was opened, back up the old
//! version. Paths are the app's ("/a/b"); `platform::native` turns them into the computer's.

use crate::device::{basename, dirname, err, DevError, Entry, Result, Transport};
use crate::platform::{app_path, native};
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

pub struct Local;

fn io(path: &str, e: std::io::Error) -> DevError {
    DevError::Msg(format!("{path}: {e}"))
}

fn secs(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
}

/// Size and modification time to the nanosecond: changes whenever the file is written.
fn version_of(m: &fs::Metadata) -> String {
    let ns = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    format!("{}-{ns}", m.len())
}

/// Writes the file next to `path` with `write`, then renames it over `path`, so a failed write
/// leaves the old file intact. Checks the new file has `size` bytes; returns its version.
fn swap_in(path: &str, size: u64, write: impl FnOnce(&Path) -> std::io::Result<()>) -> Result<String> {
    let tmp = crate::device::join(&dirname(path), &format!(".{}.ferry-tmp", basename(path)));
    if let Err(e) = write(&native(&tmp)) {
        let _ = fs::remove_file(native(&tmp));
        return Err(io(&tmp, e));
    }
    if let Err(e) = fs::rename(native(&tmp), native(path)) {
        let _ = fs::remove_file(native(&tmp));
        return Err(io(path, e));
    }
    let m = fs::metadata(native(path)).map_err(|e| io(path, e))?;
    if m.len() != size {
        return err(format!("{path}: wrote {size} bytes but the file has {}.", m.len()));
    }
    Ok(version_of(&m))
}

/// Whether the system marks it hidden: UF_HIDDEN on macOS (`~/Library`, `/usr`), the hidden
/// attribute on Windows.
fn flagged_hidden(m: &fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const UF_HIDDEN: u32 = 0x8000;
        m.st_flags() & UF_HIDDEN != 0
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = m;
        false
    }
}

fn entry(path: &str, m: &fs::Metadata) -> Entry {
    Entry {
        name: basename(path).to_string(),
        path: path.to_string(),
        dir: m.is_dir(),
        size: if m.is_dir() { 0 } else { m.len() },
        mtime: secs(m),
        hidden: flagged_hidden(m),
    }
}

impl Transport for Local {
    fn kind(&self) -> &'static str {
        "mac"
    }

    #[cfg(feature = "fsd")]
    fn search(&self, dir: &str, s: &crate::device::Search) -> Option<Result<(Vec<Entry>, bool)>> {
        crate::fsd::search(dir, s).map(Ok)
    }

    fn label(&self) -> String {
        format!("This {}", crate::platform::COMPUTER)
    }

    fn default_dir(&self) -> String {
        dirs::home_dir().map(|h| app_path(&h)).unwrap_or_else(|| "/".into())
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        let mut out = vec![];
        if let Some(home) = dirs::home_dir() {
            out.push(("Home".to_string(), app_path(&home)));
            for name in ["Desktop", "Documents", "Downloads"] {
                let p = home.join(name);
                if p.is_dir() {
                    out.push((name.to_string(), app_path(&p)));
                }
            }
        }
        out
    }

    /// Lists hidden files too, like the phone transports; the app hides them unless asked.
    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        if cfg!(windows) && path == "/" {
            return Ok(drive_letters().into_iter().map(|d| top(&d)).collect());
        }
        let rd = fs::read_dir(native(path)).map_err(|e| io(path, e))?;
        let mut out = vec![];
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let p = crate::device::join(path, &name);
            // Follows symlinks; broken ones are skipped.
            if let Ok(m) = fs::metadata(native(&p)) {
                out.push(entry(&p, &m));
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        if cfg!(windows) && path == "/" {
            return Ok(Some(top("/")));
        }
        match fs::metadata(native(path)) {
            Ok(m) => Ok(Some(entry(path, &m))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(path, e)),
        }
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let m = fs::metadata(native(path)).map_err(|e| io(path, e))?;
        if m.is_dir() {
            return err(format!("{path} is a folder."));
        }
        let data = fs::read(native(path)).map_err(|e| io(path, e))?;
        Ok((data, version_of(&m)))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = fs::File::open(native(path)).map_err(|e| io(path, e))?;
        f.seek(SeekFrom::Start(start)).map_err(|e| io(path, e))?;
        let mut out = Vec::new();
        f.take(len).read_to_end(&mut out).map_err(|e| io(path, e))?;
        Ok(out)
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        Ok(fs::metadata(native(path)).ok().filter(|m| m.is_file()).map(|m| version_of(&m)))
    }

    /// Writes next to the file, then renames over it, so a failed write leaves the old file intact.
    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        swap_in(path, data.len() as u64, |tmp| fs::write(tmp, data))
    }

    /// Copied by the system (a clone on APFS, which takes no time or space).
    fn read_to(&self, path: &str, to: &Path, read: &mut dyn FnMut(u64), _stop: &AtomicBool) -> Result<String> {
        let m = fs::metadata(native(path)).map_err(|e| io(path, e))?;
        if m.is_dir() {
            return err(format!("{path} is a folder."));
        }
        let n = fs::copy(native(path), to).map_err(|e| io(path, e))?;
        read(n);
        Ok(version_of(&m))
    }

    fn replace_from(&self, path: &str, from: &Path, _exists: bool) -> Result<String> {
        let size = fs::metadata(from).map_err(|e| DevError::Msg(format!("{}: {e}", from.display())))?.len();
        swap_in(path, size, |tmp| fs::copy(from, tmp).map(|_| ()))
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        let r = if is_dir { fs::remove_dir(native(path)) } else { fs::remove_file(native(path)) };
        r.map_err(|e| io(path, e))
    }

    fn move_to(&self, from: &str, to: &str) -> Result<bool> {
        match crate::platform::move_no_replace(&native(from), &native(to)) {
            Ok(()) => Ok(true),
            Err(e) if crate::platform::other_disk(&e) => Ok(false),
            Err(e) => Err(io(from, e)),
        }
    }

    fn to_trash(&self, path: &str) -> Result<bool> {
        crate::platform::to_trash(&native(path)).map_err(DevError::Msg)
    }

    fn delete_forever(&self, path: &str) -> Result<bool> {
        let p = native(path);
        if path == "/" || (cfg!(windows) && dirname(path) == "/") {
            return err("A whole drive can't be deleted.");
        }
        let meta = fs::symlink_metadata(&p).map_err(|e| io(path, e))?;
        // A link is removed, not what it points to; on Windows a link to a folder is a folder.
        let r = if meta.is_dir() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p).or_else(|e| if meta.file_type().is_symlink() { fs::remove_dir(&p) } else { Err(e) })
        };
        r.map_err(|e| io(path, e))?;
        Ok(true)
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        if dirname(path) == "/" && cfg!(windows) {
            return err("Folders can't be made next to the drives.");
        }
        fs::create_dir(native(path)).map_err(|e| io(path, e))
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        // Windows reads these as part of a path, or doesn't allow them in names.
        if cfg!(windows) && new_name.contains(['\\', ':', '*', '?', '"', '<', '>', '|']) {
            return err("Enter a name without \\ : * ? \" < > or |.");
        }
        fs::rename(native(path), native(&crate::device::join(&dirname(path), new_name))).map_err(|e| io(path, e))
    }

    /// The user's shell (a login shell, so PATH is as in a terminal), or PowerShell on Windows.
    fn shell(&self, dir: &str) -> Option<std::process::Command> {
        let mut c = if cfg!(windows) {
            let mut c = std::process::Command::new("powershell");
            c.arg("-NoLogo");
            c
        } else {
            let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
            let mut c = std::process::Command::new(shell);
            c.arg("-l");
            c
        };
        c.current_dir(native(dir));
        Some(c)
    }

    /// The disk holding the home folder (named after the computer), and each other disk.
    fn space(&self) -> Vec<crate::device::Space> {
        let mut dirs = vec![(crate::platform::COMPUTER.to_string(), self.default_dir())];
        dirs.extend(crate::platform::drives());
        dirs.into_iter()
            .filter_map(|(name, dir)| crate::platform::free_space(&dir).map(|(free, total)| crate::device::Space { name, free, total, path: dir }))
            .collect()
    }
}

/// The drives, "C:" and so on, on Windows; nothing elsewhere.
fn drive_letters() -> Vec<String> {
    (b'A'..=b'Z').map(|c| format!("{}:", c as char)).filter(|d| cfg!(windows) && native(&format!("/{d}")).is_dir()).collect()
}

/// "/" on Windows, which holds the drives, or one of the drives in it.
fn top(name: &str) -> Entry {
    let path = if name == "/" { "/".to_string() } else { format!("/{name}") };
    Entry { name: name.to_string(), path, dir: true, size: 0, mtime: 0, hidden: false }
}

#[cfg(test)]
mod tests {
    use crate::device::Device;

    #[cfg(target_os = "macos")]
    #[test]
    fn files_macos_marks_hidden_are_hidden() {
        let dir = std::env::temp_dir().join(format!("ferry-hidden-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shown"), b"").unwrap();
        std::fs::write(dir.join("flagged"), b"").unwrap();
        assert!(std::process::Command::new("chflags").arg("hidden").arg(dir.join("flagged")).status().unwrap().success());
        let dev = Device::new(Box::new(super::Local));
        let mut got: Vec<_> = dev.listdir(&crate::platform::app_path(&dir)).unwrap().into_iter().map(|e| (e.name.clone(), e.is_hidden())).collect();
        got.sort();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(got, [("flagged".to_string(), true), ("shown".to_string(), false)]);
    }

    #[test]
    fn deleting_permanently_takes_folders_whole_but_never_a_drive() {
        let dir = std::env::temp_dir().join(format!("ferry-forever-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("folder/inner")).unwrap();
        std::fs::write(dir.join("folder/inner/a.txt"), b"a").unwrap();
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        let root = crate::platform::app_path(&dir);
        let dev = Device::new(Box::new(super::Local));
        dev.remove_forever(&format!("{root}/folder")).unwrap();
        dev.remove_forever(&format!("{root}/b.txt")).unwrap();
        let left = std::fs::read_dir(&dir).unwrap().count();
        let missing = dev.remove_forever(&format!("{root}/b.txt")).is_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(left, 0);
        assert!(missing);
        assert!(dev.remove_forever("/").is_err());
    }

    #[test]
    fn mac_files_follow_the_device_rules() {
        let dir = std::env::temp_dir().join(format!("ferry-local-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = crate::platform::app_path(&dir);
        let dev = Device::new(Box::new(super::Local));
        let f = format!("{root}/a.txt");
        let w = dev.write(&f, b"one", None, true).unwrap();
        assert!(dev.write(&f, b"x", None, true).is_err(), "create refuses an existing file");
        let (data, v) = dev.read(&f).unwrap();
        assert_eq!((data.as_slice(), &v), (&b"one"[..], &w.version));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let w2 = dev.write(&f, b"two!", Some(&v), false).unwrap();
        assert!(w2.backup.is_some());
        assert!(dev.write(&f, b"three", Some(&v), false).is_err(), "stale version refused");
        dev.mkdir(&format!("{root}/sub")).unwrap();
        std::fs::write(dir.join(".hidden"), b"").unwrap();
        let names: Vec<_> = dev.listdir(&root).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["sub", ".hidden", "a.txt"]);
        let backups: Vec<_> = [w2.backup, dev.remove(&f).unwrap()].into_iter().flatten().collect();
        dev.remove(&format!("{root}/sub")).unwrap();
        std::fs::remove_file(dir.join(".hidden")).unwrap();
        for b in backups {
            std::fs::remove_file(b).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
