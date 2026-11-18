//! The Mac's own files, as a transport. Wrapped in a `Device`, so writes and deletes get the same
//! rules as on the phone: refuse if the file changed since it was opened, back up the old version.

use crate::device::{basename, dirname, err, DevError, Entry, Result, Transport};
use std::fs;
use std::path::Path;
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
        "This Mac".into()
    }

    fn default_dir(&self) -> String {
        dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_else(|| "/".into())
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        let mut out = vec![];
        if let Some(home) = dirs::home_dir() {
            out.push(("Home".to_string(), home.display().to_string()));
            for name in ["Desktop", "Documents", "Downloads"] {
                let p = home.join(name);
                if p.is_dir() {
                    out.push((name.to_string(), p.display().to_string()));
                }
            }
        }
        out
    }

    /// Lists hidden files too, like the phone transports; the app hides them unless asked.
    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let rd = fs::read_dir(path).map_err(|e| io(path, e))?;
        let mut out = vec![];
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let p = crate::device::join(path, &name);
            // Follows symlinks; broken ones are skipped.
            if let Ok(m) = fs::metadata(&p) {
                out.push(entry(&p, &m));
            }
        }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        match fs::metadata(path) {
            Ok(m) => Ok(Some(entry(path, &m))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(path, e)),
        }
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let m = fs::metadata(path).map_err(|e| io(path, e))?;
        if m.is_dir() {
            return err(format!("{path} is a folder."));
        }
        let data = fs::read(path).map_err(|e| io(path, e))?;
        Ok((data, version_of(&m)))
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        Ok(fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| version_of(&m)))
    }

    /// Writes next to the file, then renames over it, so a failed write leaves the old file intact.
    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        let tmp = crate::device::join(&dirname(path), &format!(".{}.yomi-tmp", basename(path)));
        fs::write(&tmp, data).map_err(|e| io(&tmp, e))?;
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(io(path, e));
        }
        let m = fs::metadata(path).map_err(|e| io(path, e))?;
        if m.len() != data.len() as u64 {
            return err(format!("{path}: wrote {} bytes but the file has {}.", data.len(), m.len()));
        }
        Ok(version_of(&m))
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        let r = if is_dir { fs::remove_dir(path) } else { fs::remove_file(path) };
        r.map_err(|e| io(path, e))
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        fs::create_dir(Path::new(path)).map_err(|e| io(path, e))
    }
}

#[cfg(test)]
mod tests {
    use crate::device::Device;

    #[test]
    fn mac_files_follow_the_device_rules() {
        let dir = std::env::temp_dir().join(format!("yomi-local-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.display().to_string();
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
