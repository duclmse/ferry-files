//! The computer's own files, as a transport. Wrapped in a `Device`, so writes and deletes get the
//! same rules as on the phone: refuse if the file changed since it was opened, back up the old
//! version. Paths are the app's ("/a/b"); `platform::native` turns them into the computer's.

use crate::device::{basename, dirname, err, DevError, Entry, Result, Transport};
use crate::platform::{app_path, native};
use std::fs;
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

fn entry(path: &str, m: &fs::Metadata) -> Entry {
    Entry {
        name: basename(path).to_string(),
        path: path.to_string(),
        dir: m.is_dir(),
        size: if m.is_dir() { 0 } else { m.len() },
        mtime: secs(m),
    }
}

impl Transport for Local {
    fn kind(&self) -> &'static str {
        "mac"
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
        let tmp = crate::device::join(&dirname(path), &format!(".{}.ferry-tmp", basename(path)));
        fs::write(native(&tmp), data).map_err(|e| io(&tmp, e))?;
        if let Err(e) = fs::rename(native(&tmp), native(path)) {
            let _ = fs::remove_file(native(&tmp));
            return Err(io(path, e));
        }
        let m = fs::metadata(native(path)).map_err(|e| io(path, e))?;
        if m.len() != data.len() as u64 {
            return err(format!("{path}: wrote {} bytes but the file has {}.", data.len(), m.len()));
        }
        Ok(version_of(&m))
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        let r = if is_dir { fs::remove_dir(native(path)) } else { fs::remove_file(native(path)) };
        r.map_err(|e| io(path, e))
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

    /// In the user's shell (a login shell, so PATH is as in a terminal), or cmd on Windows.
    fn run(&self, dir: &str, command: &str) -> Option<Result<crate::terminal::Ran>> {
        let mut c = if cfg!(windows) {
            let mut c = crate::platform::command("cmd");
            c.args(["/C", command]);
            c
        } else {
            let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
            let mut c = crate::platform::command(shell);
            c.args(["-l", "-c", command]);
            c
        };
        c.current_dir(native(dir));
        Some(crate::terminal::run(c))
    }

    /// The disk holding the home folder (named after the computer), and each other disk.
    fn space(&self) -> Vec<crate::device::Space> {
        let mut dirs = vec![(crate::platform::COMPUTER.to_string(), self.default_dir())];
        dirs.extend(crate::platform::drives());
        dirs.into_iter()
            .filter_map(|(name, dir)| {
                crate::platform::free_space(&dir).map(|(free, total)| crate::device::Space { name, free, total, path: dir })
            })
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
    Entry { name: name.to_string(), path, dir: true, size: 0, mtime: 0 }
}

#[cfg(test)]
mod tests {
    use crate::device::Device;

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
