//! What differs between macOS, Windows and Linux, kept in one place: the app's folders, opening
//! files, disks (listing, free space, ejecting) and how the computer's paths map to the app's.
//!
//! The app's paths are "/a/b" on every system. On Windows the top folder "/" holds the drives,
//! so "/C:/Users" is `C:\Users`; `native` and `app_path` convert between the two.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the app calls the computer it runs on, in sentences ("Backups are kept on the Mac").
pub const COMPUTER: &str = if cfg!(target_os = "macos") {
    "Mac"
} else if cfg!(windows) {
    "PC"
} else {
    "computer"
};

/// Where the web view reaches the app's `ferry` scheme: Windows' web view (WebView2) only loads
/// it over http, at `http://ferry.localhost/`. The page's `os::served` does the same.
pub const SCHEME_ORIGIN: &str = if cfg!(windows) { "http://ferry.localhost" } else { "ferry://localhost" };

/// A command that runs without flashing a console window on Windows.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// Why no iPhone can be found: Apple's usbmuxd (built into macOS) isn't there.
pub const IPHONE_HINT: &str = if cfg!(target_os = "macos") {
    "Can't reach the Mac's iPhone service (usbmuxd)."
} else if cfg!(windows) {
    "For an iPhone, install Apple Devices from the Microsoft Store (or iTunes)."
} else {
    "For an iPhone, install and start usbmuxd (e.g. sudo apt install usbmuxd)."
};

/// How to install adb, as a sentence.
pub const ADB_INSTALL: &str = if cfg!(target_os = "macos") {
    "Install it with: brew install --cask android-platform-tools"
} else if cfg!(windows) {
    "Install it with: winget install Google.PlatformTools"
} else {
    "Install it with your package manager (adb on Debian and Ubuntu, android-tools on Fedora and Arch)."
};

/// Where backups of replaced and deleted files go. On macOS it's the folder yomi-files.py uses
/// too, so backups from both are in one place. Elsewhere it's with the app's data rather than
/// its cache, since a cache may be cleared by the system.
pub fn backup_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        dirs::home_dir().unwrap_or_else(std::env::temp_dir).join("Library/Caches/ferry-files/backups")
    } else {
        dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("ferry-files").join("backups")
    }
}

/// The thumbnail Windows shows for the file in Explorer (pictures, videos, PDFs, documents and
/// whatever else has a thumbnail handler), about `px` on its longest side. None when there's
/// none: Windows' generic icons aren't used.
#[cfg(windows)]
pub fn system_thumb(path: &Path, px: u32) -> Option<image::DynamicImage> {
    let path = path.to_path_buf();
    // Thumbnail handlers expect a single-threaded COM apartment, which the caller's thread may
    // not be in (or may be in the other kind), so this runs on a thread of its own.
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        let inited = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let img = unsafe { shell_thumb(&path, px) };
        if inited {
            unsafe { CoUninitialize() };
        }
        img
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(windows)]
unsafe fn shell_thumb(path: &Path, px: u32) -> Option<image::DynamicImage> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY};
    let wide = HSTRING::from(path.as_os_str());
    let f: IShellItemImageFactory = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.ok()?;
    let side = px as i32;
    let hbm = unsafe { f.GetImage(SIZE { cx: side, cy: side }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK) }.ok()?;
    let read = || -> Option<image::DynamicImage> {
        let mut bm = BITMAP::default();
        let got = unsafe { GetObjectW(hbm.into(), std::mem::size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) };
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
        // Asked for top-down rows (a negative height) of 32-bit BGRA.
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; w as usize * h as usize * 4];
        let dc = unsafe { CreateCompatibleDC(None) };
        let lines = unsafe { GetDIBits(dc, hbm, 0, h, Some(px.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS) };
        let _ = unsafe { DeleteDC(dc) };
        if lines != h as i32 {
            return None;
        }
        // No alpha at all means an opaque picture; otherwise the colours are premultiplied.
        let opaque = px.chunks_exact(4).all(|p| p[3] == 0);
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2);
            if opaque {
                p[3] = 255;
            } else if p[3] > 0 && p[3] < 255 {
                let a = p[3] as u32;
                for c in &mut p[..3] {
                    *c = (*c as u32 * 255 / a).min(255) as u8;
                }
            }
        }
        image::RgbaImage::from_raw(w, h, px).map(image::DynamicImage::ImageRgba8)
    };
    let img = read();
    let _ = unsafe { DeleteObject(hbm.into()) };
    img
}

/// Whether the computer can make thumbnails of its own files of any type (`system_thumb`).
pub const SYSTEM_THUMBS: bool = cfg!(windows);

/// The computer's path for an app path.
pub fn native(path: &str) -> PathBuf {
    #[cfg(windows)]
    {
        // "/C:/Users/me" is C:\Users\me, and "/C:" is the drive's top folder, C:\.
        let mut s = path.trim_start_matches('/').replace('/', "\\");
        if s.len() == 2 && s.ends_with(':') {
            s.push('\\');
        }
        PathBuf::from(s)
    }
    #[cfg(not(windows))]
    PathBuf::from(path)
}

/// The app path for a path on the computer (an absolute one).
pub fn app_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        // `canonicalize` gives \\?\C:\..., which is still C:\...
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
        format!("/{}", s.trim_end_matches('/'))
    }
    #[cfg(not(windows))]
    path.display().to_string()
}

/// Opens a file in its default app, or a folder in the file manager.
pub fn open(path: &Path) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>).map_err(|e| format!("Can't open {}: {e}", path.display()))
}

/// Shows a file in Quick Look (`qlmanage -p`) on macOS. Other systems have no such preview, so
/// the file opens in its default app instead.
pub fn quick_look(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        command("qlmanage")
            .arg("-p")
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Can't start Quick Look: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    open(path)
}

/// Disks other than the one the system is on (USB drives, SD cards, disk images), as
/// (name, app path).
pub fn drives() -> Vec<(String, String)> {
    let mut out = drives_here();
    out.sort();
    out
}

#[cfg(target_os = "macos")]
fn drives_here() -> Vec<(String, String)> {
    let root = std::fs::canonicalize("/").ok();
    std::fs::read_dir("/Volumes")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("com.apple.") {
                return None;
            }
            let path = e.path();
            // The startup disk shows in /Volumes as a link to "/".
            if std::fs::canonicalize(&path).ok() == root || !path.is_dir() {
                return None;
            }
            Some((name, path.display().to_string()))
        })
        .collect()
}

/// What's mounted under /media, /run/media or /mnt (where desktops and people put removable
/// disks), from /proc/self/mounts.
#[cfg(all(unix, not(target_os = "macos")))]
fn drives_here() -> Vec<(String, String)> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    mounts(&text)
        .into_iter()
        .filter(|(_, at)| ["/media/", "/run/media/", "/mnt/"].iter().any(|p| at.starts_with(p)))
        .filter(|(_, at)| Path::new(at).is_dir())
        .map(|(_, at)| (crate::device::basename(&at).to_string(), at))
        .collect()
}

/// (device, mount point) of each line of /proc/mounts, whose spaces and such are written `\040`.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn mounts(text: &str) -> Vec<(String, String)> {
    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
                out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            Some((unescape(f.next()?), unescape(f.next()?)))
        })
        .collect()
}

/// Every drive but the one Windows is on, named like File Explorer does: "USB stick (E:)".
#[cfg(windows)]
fn drives_here() -> Vec<(String, String)> {
    use windows::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW};
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()).to_uppercase();
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:", (b'A' + i) as char))
        .filter(|d| *d != system && Path::new(&format!("{d}\\")).is_dir())
        .map(|d| {
            let root: Vec<u16> = format!("{d}\\").encode_utf16().chain([0]).collect();
            let mut label = [0u16; 261];
            let ok = unsafe {
                GetVolumeInformationW(windows::core::PCWSTR(root.as_ptr()), Some(&mut label), None, None, None, None)
            }
            .is_ok();
            let len = label.iter().position(|&c| c == 0).unwrap_or(0);
            let label = if ok { String::from_utf16_lossy(&label[..len]) } else { String::new() };
            let name = if label.is_empty() { format!("Drive ({d})") } else { format!("{label} ({d})") };
            (name, format!("/{d}"))
        })
        .collect()
}

/// Free and total bytes of the disk holding the folder at app path `dir`.
pub fn free_space(dir: &str) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let path: Vec<u16> = native(dir).as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
        let (mut free, mut total) = (0u64, 0u64);
        unsafe { GetDiskFreeSpaceExW(windows::core::PCWSTR(path.as_ptr()), Some(&mut free), Some(&mut total), None) }.ok()?;
        Some((free, total))
    }
    #[cfg(not(windows))]
    {
        // -P: one line per disk however long its name, the same on macOS and Linux.
        let out = command("df").args(["-Pk", dir]).output().ok()?;
        parse_df(&String::from_utf8_lossy(&out.stdout))
    }
}

/// The second line of `df -Pk` (or of adb's `df -k`): name, 1K-blocks, used, available, ...
pub(crate) fn parse_df(out: &str) -> Option<(u64, u64)> {
    let line = out.lines().nth(1)?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    let total: u64 = cols.get(1)?.parse().ok()?;
    let free: u64 = cols.get(3)?.parse().ok()?;
    Some((free * 1024, total * 1024))
}

/// Whether `path` is one of `drives()`, which are what can be ejected.
fn is_drive(path: &str) -> bool {
    drives().iter().any(|(_, p)| p == path)
}

/// Ejects a disk from `drives()` (its app path).
pub fn eject(path: &str) -> Result<(), String> {
    if !is_drive(path) {
        return Err(format!("{path} isn't a disk that can be ejected."));
    }
    let name = crate::device::basename(path).to_string();
    let failed = |why: String| {
        let why = if why.trim().is_empty() { "the disk is in use".to_string() } else { why.trim().to_string() };
        Err(format!("Couldn't eject {name}: {why}. Close files open on it and try again."))
    };
    #[cfg(target_os = "macos")]
    {
        let out = command("diskutil").args(["eject", path]).output().map_err(|e| format!("Can't run diskutil: {e}"))?;
        if out.status.success() { Ok(()) } else { failed(String::from_utf8_lossy(&out.stderr).into_owned()) }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // udisks (what desktops mount removable disks with) unmounts it, then powers the
        // disk off where it can; without it, `umount`, which works for disks in /etc/fstab.
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let dev = mounts(&text).into_iter().find(|(_, at)| at == path).map(|(d, _)| d);
        let unmounted = match &dev {
            Some(dev) => command("udisksctl").args(["unmount", "-b", dev]).output(),
            None => command("umount").arg(path).output(),
        };
        let out = match unmounted {
            Ok(out) => out,
            Err(_) => command("umount").arg(path).output().map_err(|e| format!("Can't run umount: {e}"))?,
        };
        if !out.status.success() {
            return failed(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        if let Some(dev) = dev {
            let _ = command("udisksctl").args(["power-off", "-b", &dev]).output();
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        // File Explorer's own Eject, through the shell.
        let drive = path.trim_start_matches('/');
        let script = format!("(New-Object -ComObject Shell.Application).Namespace(17).ParseName('{drive}').InvokeVerb('Eject')");
        let out = command("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .map_err(|e| format!("Can't run PowerShell: {e}"))?;
        if !out.status.success() {
            return failed(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        // The verb doesn't say whether it worked: the drive going away does.
        for _ in 0..20 {
            if !Path::new(&format!("{drive}\\")).exists() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        failed(String::new())
    }
}

/// The SMB shares mounted now, as (what `smb_share_mounts` read: "//[user@]host/share", the
/// app path it's at).
pub fn smb_mounted() -> Vec<(String, String)> {
    #[cfg(target_os = "macos")]
    {
        let out = command("mount").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        smb_in_mount(&out)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // gvfs (what `gio mount` uses) keeps each share at smb-share:server=host,share=name.
        let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        let dir = Path::new(&run).join("gvfs");
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let spec = name.strip_prefix("smb-share:")?;
                let field = |k: &str| spec.split(',').find_map(|f| f.strip_prefix(k)).map(String::from);
                let user = field("user=").map(|u| format!("{u}@")).unwrap_or_default();
                Some((format!("//{user}{}/{}", field("server=")?, field("share=")?), e.path().display().to_string()))
            })
            .collect()
    }
    #[cfg(windows)]
    {
        let out = command("net").arg("use").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        smb_in_net_use(&out)
    }
}

/// SMB mounts in `mount`'s output: "//me@nas/Share on /Volumes/Share (smbfs, ...)".
#[cfg(any(target_os = "macos", test))]
pub(crate) fn smb_in_mount(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter(|l| l.contains("(smbfs"))
        .filter_map(|l| {
            let (what, rest) = l.split_once(" on ")?;
            let at = rest.rsplit_once(" (")?.0;
            Some((what.to_string(), at.to_string()))
        })
        .collect()
}

/// Connected shares in `net use`'s output: "OK  Z:  \\nas\Share  Microsoft Windows Network".
#[cfg(any(windows, test))]
pub(crate) fn smb_in_net_use(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            let unc = words.iter().position(|w| w.starts_with(r"\\"))?;
            let letter = words[..unc].iter().find(|w| w.len() == 2 && w.ends_with(':'))?;
            Some((unc_to_slashes(words[unc]), format!("/{}", letter.to_uppercase())))
        })
        .collect()
}

#[cfg(any(windows, test))]
fn unc_to_slashes(unc: &str) -> String {
    unc.replace('\\', "/")
}

/// A login for an SMB share: user name, domain (may be empty) and password.
pub struct SmbLogin<'a> {
    pub user: &'a str,
    pub domain: &'a str,
    pub password: &'a str,
}

/// Runs `c` with `input` written to it, and waits for it.
fn run_with_input(mut c: Command, input: &str) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let Some(mut i) = child.stdin.take() {
        // A program that stops reading early isn't an error here: its exit says what happened.
        let _ = i.write_all(input.as_bytes());
    }
    child.wait_with_output()
}

/// Text in an AppleScript string literal.
#[cfg(any(target_os = "macos", test))]
fn applescript_text(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Text in a PowerShell single-quoted string.
#[cfg(any(windows, test))]
fn powershell_text(s: &str) -> String {
    s.replace('\'', "''")
}

/// Mounts the SMB share at `url` ("smb://[user@]host/share"). Without `login`, the system asks
/// for the password when the share needs one (and may keep it in its keychain). A login is
/// given to the system on its input, so it's never in a command line.
pub fn smb_mount(url: &str, login: Option<&SmbLogin>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let out = {
        let mut script = format!("mount volume \"{}\"", applescript_text(url));
        if let Some(l) = login {
            let user = if l.domain.is_empty() { l.user.to_string() } else { format!("{};{}", l.domain, l.user) };
            script += &format!(" as user name \"{}\" with password \"{}\"", applescript_text(&user), applescript_text(l.password));
        }
        let mut c = command("osascript");
        c.arg("-");
        run_with_input(c, &script)
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let out = {
        let mut c = command("gio");
        c.args(["mount", url]);
        // gio asks in turn for what the server wants: user, domain, password.
        let answers = login.map(|l| format!("{}\n{}\n{}\n", l.user, if l.domain.is_empty() { "WORKGROUP" } else { l.domain }, l.password));
        run_with_input(c, &answers.unwrap_or_default())
    };
    #[cfg(windows)]
    let out = {
        let unc = url.trim_start_matches("smb:").replace('/', "\\");
        match login {
            None => command("net").args(["use", "*", &unc, "/persistent:no"]).stdin(std::process::Stdio::null()).output(),
            Some(l) => {
                let user = if l.domain.is_empty() { l.user.to_string() } else { format!("{}\\{}", l.domain, l.user) };
                // The script (password included) goes in on PowerShell's input.
                let script = format!(
                    "$ErrorActionPreference = 'Stop'\n\
                     $p = ConvertTo-SecureString '{}' -AsPlainText -Force\n\
                     $c = New-Object System.Management.Automation.PSCredential('{}', $p)\n\
                     $l = [char[]](68..90) | Where-Object {{ -not (Test-Path \"$($_):\\\") }} | Select-Object -Last 1\n\
                     if (-not $l) {{ throw 'There is no free drive letter.' }}\n\
                     New-PSDrive -Name $l -PSProvider FileSystem -Root '{}' -Credential $c -Persist -Scope Global | Out-Null\n",
                    powershell_text(l.password),
                    powershell_text(&user),
                    powershell_text(&unc)
                );
                let mut c = command("powershell");
                c.args(["-NoProfile", "-NonInteractive", "-Command", "-"]);
                run_with_input(c, &script)
            }
        }
    };
    let out = out.map_err(|e| format!("Couldn't mount {url}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
    // osascript: "execution error: ... (-128)" is the password dialog's Cancel.
    if why.contains("(-128)") {
        return Err("Cancelled.".into());
    }
    Err(if why.is_empty() { format!("Couldn't mount {url}.") } else { format!("Couldn't mount {url}: {why}") })
}

/// Unmounts the SMB share mounted at `path` (an app path from `smb_mounted`).
pub fn smb_unmount(path: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let out = command("diskutil").args(["unmount", path]).output();
    #[cfg(all(unix, not(target_os = "macos")))]
    let out = command("gio").args(["mount", "-u", path]).output();
    #[cfg(windows)]
    let out = command("net").args(["use", path.trim_start_matches('/'), "/delete", "/y"]).output();
    let out = out.map_err(|e| format!("Couldn't unmount {path}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr }).trim().to_string();
    Err(format!("Couldn't unmount {path}: {}. Close files open on it and try again.", if why.is_empty() { "it's in use" } else { &why }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_df() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1 971350180 500000000 400000000 56% /\n";
        assert_eq!(parse_df(out), Some((400000000 * 1024, 971350180 * 1024)));
        assert_eq!(parse_df("header only\n"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn app_paths_are_the_computers() {
        assert_eq!(native("/a/b"), PathBuf::from("/a/b"));
        assert_eq!(app_path(Path::new("/a/b")), "/a/b");
    }

    #[cfg(windows)]
    #[test]
    fn drives_are_top_folders() {
        assert_eq!(native("/C:/Users/me"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(native("/C:"), PathBuf::from(r"C:\"));
        assert_eq!(app_path(Path::new(r"C:\Users\me")), "/C:/Users/me");
        assert_eq!(app_path(Path::new(r"\\?\D:\x\")), "/D:/x");
    }

    #[test]
    fn reads_smb_mounts() {
        let mount = "/dev/disk3s1s1 on / (apfs, sealed, local)\n//me@nas._smb._tcp.local/Photos on /Volumes/Photos 1 (smbfs, nodev, nosuid, mounted by me)\n";
        assert_eq!(smb_in_mount(mount), vec![("//me@nas._smb._tcp.local/Photos".to_string(), "/Volumes/Photos 1".to_string())]);
        let net = "Status       Local     Remote                    Network\n-----\nOK           Z:        \\\\nas\\Photos              Microsoft Windows Network\nThe command completed successfully.\n";
        assert_eq!(smb_in_net_use(net), vec![("//nas/Photos".to_string(), "/Z:".to_string())]);
        assert_eq!(applescript_text(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(powershell_text("it's"), "it''s");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn reads_mounts() {
        let text = "/dev/sdb1 /media/me/MY\\040STICK vfat rw 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(mounts(text)[0], ("/dev/sdb1".to_string(), "/media/me/MY STICK".to_string()));
    }
}
