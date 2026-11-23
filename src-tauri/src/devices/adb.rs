//! adb transport: runs the `adb` command. Paths are real device paths like /sdcard/Download.

use crate::device::{basename, dirname, err, join, DevError, Entry, Result, Transport};
use std::path::PathBuf;
use std::process::Stdio;

/// Apps started from the Finder (or a desktop menu) don't get the shell's PATH, so look where the
/// Android SDK and package managers put adb too.
pub fn adb_path() -> Option<PathBuf> {
    let exe = format!("adb{}", std::env::consts::EXE_SUFFIX);
    let home = dirs::home_dir().unwrap_or_default();
    let mut sdks: Vec<PathBuf> = ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    let mut bins: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        sdks.push(home.join("Library/Android/sdk"));
        bins.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    } else if cfg!(windows) {
        sdks.extend(dirs::data_local_dir().map(|d| d.join("Android").join("Sdk")));
        // Scoop's shims, and winget's links (on PATH, but not for an app started before the install).
        bins.push(home.join("scoop").join("shims"));
        bins.extend(dirs::data_local_dir().map(|d| d.join("Microsoft").join("WinGet").join("Links")));
    } else {
        sdks.push(home.join("Android/Sdk"));
        bins.extend(["/usr/bin", "/usr/local/bin", "/snap/bin"].map(PathBuf::from));
    }
    let mut candidates: Vec<PathBuf> = sdks.into_iter().map(|s| s.join("platform-tools").join(&exe)).collect();
    candidates.extend(bins.into_iter().map(|b| b.join(&exe)));
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join(&exe)));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Devices ready for adb, as (serial, model), the model from `adb devices -l` ("Pixel 7").
pub fn ready_devices() -> Vec<(String, String)> {
    let Some(adb) = adb_path() else { return vec![] };
    let Ok(out) = crate::platform::command(adb).args(["devices", "-l"]).stdin(Stdio::null()).output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let serial = words.next()?.to_string();
            let words: Vec<&str> = words.collect();
            if words.first() != Some(&"device") {
                return None;
            }
            let model = words.iter().find_map(|w| w.strip_prefix("model:")).map(|m| m.replace('_', " "));
            Some((serial.clone(), model.unwrap_or(serial)))
        })
        .collect()
}

/// Serials of devices that adb sees but can't use yet: the phone hasn't allowed this computer.
pub fn unauthorized() -> Vec<String> {
    let Some(adb) = adb_path() else { return vec![] };
    let Ok(out) = crate::platform::command(adb).arg("devices").stdin(Stdio::null()).output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once('\t').filter(|(_, st)| st.trim() == "unauthorized").map(|(s, _)| s.to_string()))
        .collect()
}

/// Every device adb lists, in whatever state (offline and unauthorized ones too); `None` when
/// adb isn't there or didn't answer, so a phone isn't taken to be gone because of that.
pub fn listed() -> Option<Vec<String>> {
    let adb = adb_path()?;
    let out = crate::platform::command(adb).arg("devices").stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).lines().skip(1).filter_map(|l| l.split_whitespace().next().map(String::from)).collect())
}

pub fn ready_serials() -> Vec<String> {
    let Some(adb) = adb_path() else { return vec![] };
    let Ok(out) = crate::platform::command(adb).arg("devices").stdin(Stdio::null()).output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter(|l| l.trim_end().ends_with("\tdevice"))
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .collect()
}

/// Quotes a path for the device shell.
fn q(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

fn md5_hex(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

pub struct Adb {
    adb: PathBuf,
    serial: Option<String>,
    model: String,
    /// The SD cards and USB drives in the phone, found when it's connected: "/storage/1234-ABCD".
    cards: Vec<String>,
}

/// The removable storage in `ls /storage`'s output: volumes named by their serial number
/// ("1234-ABCD"), not "emulated" (internal storage) or "self".
fn cards_in(ls: &str) -> Vec<String> {
    let card = |n: &str| {
        let b = n.as_bytes();
        b.len() == 9 && b[4] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || c.is_ascii_hexdigit())
    };
    let mut out: Vec<String> = ls.split_whitespace().filter(|n| card(n)).map(|n| format!("/storage/{n}")).collect();
    out.sort();
    out
}

impl Adb {
    pub fn open(serial: Option<String>) -> Result<Adb> {
        let Some(adb) = adb_path() else {
            return err(format!("adb not found. {}", crate::platform::ADB_INSTALL));
        };
        let mut dev = Adb { adb, serial, model: String::new(), cards: vec![] };
        dev.run(&["get-state"])?; // fails early with adb's own message if there's no device
        dev.model =
            dev.shell("getprop ro.product.model").map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "Android device".into());
        dev.cards = dev.shell("ls /storage 2>/dev/null; true").map(|s| cards_in(&s)).unwrap_or_default();
        Ok(dev)
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = crate::platform::command(&self.adb);
        if let Some(s) = &self.serial {
            cmd.args(["-s", s]);
        }
        let out = cmd.args(args).stdin(Stdio::null()).output().map_err(|e| DevError::Msg(format!("Couldn't run adb: {e}")))?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
            return err(if msg.is_empty() { format!("adb {} failed ({})", args[0], out.status) } else { msg });
        }
        Ok(out.stdout)
    }

    fn shell(&self, command: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.run(&["shell", command])?).into_owned())
    }

    /// Parses `stat -c '%F|%s|%Y|%n'` output.
    fn entry(line: &str, parent: Option<&str>) -> Option<Entry> {
        let mut parts = line.splitn(4, '|');
        let (kind, size, mtime, full) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let trimmed = full.trim_end_matches('/');
        let name = if trimmed.is_empty() { "/".to_string() } else { basename(trimmed).to_string() };
        Some(Entry {
            path: parent.map(|p| join(p, &name)).unwrap_or_else(|| full.to_string()),
            name,
            dir: kind == "directory",
            size: size.parse().unwrap_or(0),
            mtime: mtime.parse().unwrap_or(0),
            hidden: false,
        })
    }
}

impl Adb {
    /// Pushes local file `from` next to `path`, moves it over `path`, and checks the phone's md5
    /// of it is `expected` (the local file's). Returns the new version.
    fn push_in(&self, path: &str, from: &std::path::Path, expected: String) -> Result<String> {
        let tmp_remote = join(&dirname(path), &format!(".{}.ferry-tmp", basename(path)));
        self.run(&["push", &from.to_string_lossy(), &tmp_remote])?;
        self.shell(&format!("mv -f {} {}", q(&tmp_remote), q(path)))?;
        let version = self.version(path)?;
        if version.as_deref() != Some(expected.as_str()) {
            return err(format!("Verification failed after writing {path}."));
        }
        Ok(expected)
    }
}

/// The md5 of a file on this computer, read a piece at a time.
fn md5_file(path: &std::path::Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| DevError::Msg(format!("Can't read {}: {e}", path.display())))?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| DevError::Msg(format!("Can't read {}: {e}", path.display())))?;
        if n == 0 {
            return Ok(format!("{:x}", ctx.compute()));
        }
        ctx.consume(&buf[..n]);
    }
}

impl Transport for Adb {
    fn shell(&self, dir: &str) -> Option<std::process::Command> {
        let mut c = std::process::Command::new(&self.adb);
        if let Some(s) = &self.serial {
            c.args(["-s", s]);
        }
        c.args(["shell", "-t", &format!("cd {}; exec sh", crate::servers::sftp::sh_quote(dir))]);
        Some(c)
    }

    fn kind(&self) -> &'static str {
        "adb"
    }
    fn label(&self) -> String {
        self.model.clone()
    }
    fn default_dir(&self) -> String {
        "/sdcard".into()
    }

    /// Internal storage and its folders, then each SD card or USB drive in the phone.
    fn quick_dirs(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = [("Storage", "/sdcard"), ("Documents", "/sdcard/Documents"), ("Books", "/sdcard/Books")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let one = self.cards.len() == 1;
        out.extend(self.cards.iter().map(|c| (if one { "SD card".to_string() } else { format!("SD card ({})", basename(c)) }, c.clone())));
        out
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let d = q(&format!("{}/", path.trim_end_matches('/')));
        let out = self.shell(&format!(
            "[ -d {d} ] || {{ echo 'Not a folder: {}' >&2; exit 2; }}; \
             find {d} -mindepth 1 -maxdepth 1 -exec stat -L -c '%F|%s|%Y|%n' {{}} + 2>/dev/null; true",
            path.replace('\'', "")
        ))?;
        Ok(out.lines().filter_map(|l| Self::entry(l, Some(path))).collect())
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let out = self.shell(&format!("stat -L -c '%F|%s|%Y|%n' {} 2>/dev/null; true", q(path)))?;
        let line = out.trim();
        Ok(if line.is_empty() {
            None
        } else {
            Self::entry(line, None).map(|mut e| {
                e.path = path.to_string();
                if path == "/" {
                    e.name = "/".into()
                }
                e
            })
        })
    }

    /// `dd` in whole blocks (byte offsets need flags not every Android `dd` has), trimmed here.
    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        const BS: u64 = 64 * 1024;
        if len == 0 {
            return Ok(vec![]);
        }
        let first = start / BS;
        let blocks = (start + len).div_ceil(BS) - first;
        let cmd = format!("dd if={} bs={BS} skip={first} count={blocks} 2>/dev/null", q(path));
        let data = self.run(&["exec-out", &cmd])?;
        let from = ((start - first * BS) as usize).min(data.len());
        let to = (from + len as usize).min(data.len());
        Ok(data[from..to].to_vec())
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        let out = self.shell(&format!("if [ -f {p} ]; then md5sum {p}; fi", p = q(path)))?;
        Ok(out.split_whitespace().next().map(String::from))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let Some(st) = self.stat(path)?.filter(|e| !e.dir) else {
            return err(format!("No such file (or no permission): {path}"));
        };
        let data = self.run(&["exec-out", &format!("cat {}", q(path))])?;
        if data.len() as u64 != st.size {
            return err(format!("Read {} bytes but the file has {}. Is it readable?", data.len(), st.size));
        }
        let v = md5_hex(&data);
        Ok((data, v))
    }

    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        let tmp_local = std::env::temp_dir().join(format!("ferry-files-{}-{}", std::process::id(), md5_hex(path.as_bytes())));
        std::fs::write(&tmp_local, data).map_err(|e| DevError::Msg(format!("Can't write temp file: {e}")))?;
        let done = self.push_in(path, &tmp_local, md5_hex(data));
        let _ = std::fs::remove_file(&tmp_local);
        done
    }

    /// `adb pull` straight into the file, then checked against the phone's size and md5.
    fn read_to(&self, path: &str, to: &std::path::Path, read: &mut dyn FnMut(u64), _stop: &std::sync::atomic::AtomicBool) -> Result<String> {
        let Some(st) = self.stat(path)?.filter(|e| !e.dir) else {
            return err(format!("No such file (or no permission): {path}"));
        };
        self.run(&["pull", path, &to.to_string_lossy()])?;
        let got = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
        if got != st.size {
            return err(format!("Read {got} bytes but the file has {}. Is it readable?", st.size));
        }
        read(got);
        md5_file(to)
    }

    fn replace_from(&self, path: &str, from: &std::path::Path, _exists: bool) -> Result<String> {
        self.push_in(path, from, md5_file(from)?)
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        self.shell(&if is_dir { format!("rmdir {}", q(path)) } else { format!("rm -f {}", q(path)) })?;
        Ok(())
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        self.shell(&format!("mkdir {}", q(path)))?;
        Ok(())
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        self.shell(&format!("mv -n {} {}", q(path), q(&join(&dirname(path), new_name))))?;
        Ok(())
    }

    fn space(&self) -> Vec<crate::device::Space> {
        let out = self.shell("df -k /sdcard 2>/dev/null").unwrap_or_default();
        crate::platform::parse_df(&out)
            .map(|(free, total)| vec![crate::device::Space { name: "Storage".into(), free, total, path: "/".into() }])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sd_cards() {
        assert_eq!(cards_in("emulated\nself\n1234-ABCD\n0F1E-2D3C\n"), ["/storage/0F1E-2D3C", "/storage/1234-ABCD"]);
        assert!(cards_in("emulated  self").is_empty());
        assert!(cards_in("12345-ABC 1234-ABCG").is_empty());
    }
}
