//! What differs between macOS, Windows and Linux, kept in one place: the app's folders, opening
//! files, disks (listing, free space, ejecting) and how the computer's paths map to the app's.
//!
//! The app's paths are "/a/b" on every system. On Windows the top folder "/" holds the drives,
//! so "/C:/Users" is `C:\Users`; `native` and `app_path` convert between the two.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Once the window is open: on Windows, WebView2's own shortcuts are turned off (Ctrl+F's find
/// bar, Ctrl+R and F5 reloading, Ctrl+P printing), so the app's own shortcuts get those keys
/// wherever the focus is, a book's page included. Copy, paste and undo still work.
pub fn webview_ready(app: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        use tauri::Manager;
        for w in app.webview_windows().values() {
            let _ = w.with_webview(|wv| {
                use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3;
                use windows::core::Interface;
                // SAFETY: COM calls on the window's own WebView2, on the main thread, where
                // with_webview runs this.
                unsafe {
                    if let Ok(settings) = wv.controller().CoreWebView2().and_then(|c| c.Settings())
                    {
                        if let Ok(s3) = settings.cast::<ICoreWebView2Settings3>() {
                            let _ = s3.SetAreBrowserAcceleratorKeysEnabled(false);
                        }
                    }
                }
            });
        }
    }
    #[cfg(not(windows))]
    let _ = app;
}

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
pub const SCHEME_ORIGIN: &str = if cfg!(windows) {
    "http://ferry.localhost"
} else {
    "ferry://localhost"
};

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
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Caches/ferry-files/backups")
    } else {
        dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("ferry-files")
            .join("backups")
    }
}

/// Where the user's Lua scripts are (Run script… in the context menu): with the app's data.
pub fn scripts_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ferry-files")
        .join("scripts")
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
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
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
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
        SIIGBF_THUMBNAILONLY,
    };
    let wide = HSTRING::from(path.as_os_str());
    let f: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.ok()?;
    let side = px as i32;
    let hbm = unsafe {
        f.GetImage(
            SIZE { cx: side, cy: side },
            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .ok()?;
    let read = || -> Option<image::DynamicImage> {
        let mut bm = BITMAP::default();
        let got = unsafe {
            GetObjectW(
                hbm.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut bm as *mut BITMAP).cast()),
            )
        };
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
        let lines = unsafe {
            GetDIBits(
                dc,
                hbm,
                0,
                h,
                Some(px.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
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
        // "//nas/Photos/a" is the network share's \\nas\Photos\a.
        if let Some(unc) = path
            .strip_prefix("//")
            .filter(|p| !p.is_empty() && !p.starts_with('/'))
        {
            return PathBuf::from(format!(r"\\{}", unc.replace('/', "\\")));
        }
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
        // A network share, \\nas\Photos (or `canonicalize`'s \\?\UNC\nas\Photos), is "//nas/Photos".
        if let Some(unc) = s
            .strip_prefix(r"\\?\UNC\")
            .or_else(|| s.strip_prefix(r"\\").filter(|r| !r.starts_with(['?', '.'])))
        {
            return format!("//{}", unc.replace('\\', "/").trim_end_matches('/'));
        }
        // `canonicalize` gives \\?\C:\..., which is still C:\...
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
        format!("/{}", s.trim_end_matches('/'))
    }
    #[cfg(not(windows))]
    path.display().to_string()
}

/// Opens a file in its default app, or a folder in the file manager.
pub fn open(path: &Path) -> Result<(), String> {
    tauri_plugin_opener::open_path(path, None::<&str>)
        .map_err(|e| format!("Can't open {}: {e}", path.display()))
}

/// Opens a web or mail address in its default app.
pub fn open_url(url: &str) -> Result<(), String> {
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|e| format!("Can't open {url}: {e}"))
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

/// The disks, as (name, app path), in the order of their paths (drive letters on Windows). On
/// Windows every drive, C: too; elsewhere those other than the one the system is on (USB drives,
/// SD cards, disk images). Network shares aren't disks: they're under Servers, with Unmount.
pub fn drives() -> Vec<(String, String)> {
    let mut out = drives_here();
    let net: Vec<String> = net_mounted().into_iter().map(|(_, at)| at).collect();
    out.retain(|(_, at)| !net.contains(at));
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Whether the disk at app path `path` (one of `drives()`) can be ejected: on Windows a
/// removable or optical drive, or a disk on USB, FireWire or an SD card reader (not one inside
/// the computer); elsewhere every disk `drives()` lists.
pub fn ejectable(path: &str) -> bool {
    if !is_drive(path) {
        return false;
    }
    #[cfg(windows)]
    {
        external_drive(path.trim_start_matches('/'))
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Whether drive `d` ("E:") can be taken out: GetDriveTypeW says removable or optical, or the
/// disk's bus (IOCTL_STORAGE_QUERY_PROPERTY) is USB, FireWire, SD or MMC.
#[cfg(windows)]
fn external_drive(d: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        BusType1394, BusTypeMmc, BusTypeSd, BusTypeUsb, CreateFileW, GetDriveTypeW,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    // GetDriveTypeW's documented return values.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_CDROM: u32 = 5;
    let root: Vec<u16> = format!("{d}\\").encode_utf16().chain([0]).collect();
    let kind = unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) };
    if kind == DRIVE_REMOVABLE || kind == DRIVE_CDROM {
        return true;
    }
    // The volume, opened with no access rights: enough to ask about its disk, without admin.
    let volume: Vec<u16> = format!("\\\\.\\{d}").encode_utf16().chain([0]).collect();
    let Ok(h) = (unsafe {
        CreateFileW(
            windows::core::PCWSTR(volume.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }) else {
        return false;
    };
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // The descriptor is followed by its strings; only its fixed part is read.
    let mut desc = STORAGE_DEVICE_DESCRIPTOR::default();
    let mut got = 0u32;
    let asked = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut desc as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32,
            Some(&mut got),
            None,
        )
    };
    unsafe {
        let _ = CloseHandle(h);
    }
    asked.is_ok()
        && got as usize >= std::mem::offset_of!(STORAGE_DEVICE_DESCRIPTOR, RawPropertiesLength)
        && [BusTypeUsb, BusType1394, BusTypeSd, BusTypeMmc].contains(&desc.BusType)
}

/// The largest file the disk holding app path `dir` can take, when it's smaller than files can
/// be: 4 GB less a byte on FAT (FAT32, as many USB sticks and SD cards come).
pub fn max_file_size(dir: &str) -> Option<u64> {
    let fs = fs_type(dir)?.to_ascii_lowercase();
    ["fat", "fat12", "fat16", "fat32", "vfat", "msdos"]
        .contains(&fs.as_str())
        .then_some(u32::MAX as u64)
}

/// The file system of the disk holding app path `dir`: "NTFS", "FAT32", "exFAT" on Windows;
/// "apfs", "msdos", "exfat" on macOS; "ext4", "vfat", "exfat" on Linux.
fn fs_type(dir: &str) -> Option<String> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let d = dir
            .trim_start_matches('/')
            .get(..2)
            .filter(|d| d.ends_with(':'))?;
        let root: Vec<u16> = format!("{d}\\").encode_utf16().chain([0]).collect();
        let mut name = [0u16; 64];
        unsafe {
            GetVolumeInformationW(
                windows::core::PCWSTR(root.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut name),
            )
        }
        .ok()?;
        let len = name.iter().position(|&c| c == 0).unwrap_or(0);
        Some(String::from_utf16_lossy(&name[..len]))
    }
    #[cfg(target_os = "macos")]
    {
        let out = command("mount").output().ok()?;
        fs_in_mount(&String::from_utf8_lossy(&out.stdout), dir)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let text = std::fs::read_to_string("/proc/self/mounts").ok()?;
        mounts_typed(&text)
            .into_iter()
            .filter(|(_, at, _)| holds(at, dir))
            .max_by_key(|(_, at, _)| at.len())
            .map(|(_, _, t)| t)
    }
}

/// Whether mount point `at` holds `dir`.
#[cfg(any(unix, test))]
fn holds(at: &str, dir: &str) -> bool {
    let at = at.trim_end_matches('/');
    at.is_empty() || dir == at || dir.starts_with(&format!("{at}/"))
}

/// The file system type of the mount holding `dir`, from macOS's `mount` output:
/// "/dev/disk4s1 on /Volumes/STICK (msdos, local, nodev, nosuid, noowners)".
#[cfg(any(target_os = "macos", test))]
fn fs_in_mount(out: &str, dir: &str) -> Option<String> {
    out.lines()
        .filter_map(|l| {
            let (_, rest) = l.split_once(" on ")?;
            let (at, opts) = rest.rsplit_once(" (")?;
            Some((
                at.to_string(),
                opts.split([',', ')']).next()?.trim().to_string(),
            ))
        })
        .filter(|(at, _)| holds(at, dir))
        .max_by_key(|(at, _)| at.len())
        .map(|(_, t)| t)
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
        .filter(|(_, at)| {
            ["/media/", "/run/media/", "/mnt/"]
                .iter()
                .any(|p| at.starts_with(p))
        })
        .filter(|(_, at)| Path::new(at).is_dir())
        .map(|(_, at)| (crate::device::basename(&at).to_string(), at))
        .collect()
}

/// (device, mount point) of each line of /proc/mounts.
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn mounts(text: &str) -> Vec<(String, String)> {
    mounts_typed(text)
        .into_iter()
        .map(|(dev, at, _)| (dev, at))
        .collect()
}

/// (device, mount point, file system type) of each line of /proc/mounts, whose spaces and such
/// are written `\040`.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn mounts_typed(text: &str) -> Vec<(String, String, String)> {
    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
            {
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
            Some((
                unescape(f.next()?),
                unescape(f.next()?),
                f.next().unwrap_or_default().to_string(),
            ))
        })
        .collect()
}

/// Every drive (a card reader without a card isn't one), named like File Explorer does:
/// "USB stick (E:)", "Local Disk (C:)".
#[cfg(windows)]
fn drives_here() -> Vec<(String, String)> {
    use windows::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW};
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:", (b'A' + i) as char))
        .filter(|d| Path::new(&format!("{d}\\")).is_dir())
        .map(|d| {
            let root: Vec<u16> = format!("{d}\\").encode_utf16().chain([0]).collect();
            let mut label = [0u16; 261];
            let ok = unsafe {
                GetVolumeInformationW(
                    windows::core::PCWSTR(root.as_ptr()),
                    Some(&mut label),
                    None,
                    None,
                    None,
                    None,
                )
            }
            .is_ok();
            let len = label.iter().position(|&c| c == 0).unwrap_or(0);
            let label = if ok {
                String::from_utf16_lossy(&label[..len])
            } else {
                String::new()
            };
            let name = if !label.is_empty() {
                format!("{label} ({d})")
            } else if fixed_disk(Path::new(&d)) {
                format!("Local Disk ({d})")
            } else {
                format!("Drive ({d})")
            };
            (name, format!("/{d}"))
        })
        .collect()
}

/// Moves `from` to `to` on one disk, never over something already at `to`: on Windows the
/// system refuses (MoveFileExW without MOVEFILE_REPLACE_EXISTING); elsewhere the caller has just
/// checked that `to` is free, under the device's lock. Fails across disks; see `other_disk`.
pub fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVE_FILE_FLAGS};
        let wide = |p: &Path| -> Vec<u16> {
            p.as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain([0])
                .collect()
        };
        let (f, t) = (wide(from), wide(to));
        unsafe {
            MoveFileExW(
                windows::core::PCWSTR(f.as_ptr()),
                windows::core::PCWSTR(t.as_ptr()),
                MOVE_FILE_FLAGS(0),
            )
        }
        // The Win32 error code is the HRESULT's low 16 bits.
        .map_err(|e| std::io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32))
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// Whether a move failed only because the two places are on different disks, so the files have
/// to be copied instead: EXDEV on macOS and Linux, ERROR_NOT_SAME_DEVICE on Windows.
pub fn other_disk(e: &std::io::Error) -> bool {
    #[cfg(windows)]
    const CODE: i32 = 17;
    #[cfg(not(windows))]
    const CODE: i32 = 18;
    e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(CODE)
}

/// The system's Trash, as used here: on the Mac through NSFileManager, since the default asks
/// the Finder through AppleScript, which can bring up a permission prompt.
pub fn trash_context() -> trash::TrashContext {
    #[allow(unused_mut)]
    let mut t = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        t.set_delete_method(DeleteMethod::NsFileManager);
    }
    t
}

/// Moves a file or folder on the computer to the Trash (the Recycle Bin on Windows). Ok(false)
/// when the disk has none, so nothing changed. On Windows only fixed disks are tried: a network
/// or removable drive has no Recycle Bin, and Windows would delete the file for good instead.
/// Elsewhere a disk with no Trash makes the move fail, which also gives Ok(false).
pub fn to_trash(path: &Path) -> Result<bool, String> {
    #[cfg(windows)]
    if !fixed_disk(path) {
        return Ok(false);
    }
    Ok(trash_context().delete(path).is_ok())
}

/// Whether `path` is on a fixed disk (not a network, removable or optical drive).
#[cfg(windows)]
fn fixed_disk(path: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    // DRIVE_FIXED, from GetDriveTypeW's documented return values.
    const DRIVE_FIXED: u32 = 3;
    let s = path.to_string_lossy();
    let Some(d) = s.get(..2).filter(|d| d.ends_with(':')) else {
        return false;
    };
    let root: Vec<u16> = format!("{d}\\").encode_utf16().chain([0]).collect();
    unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) == DRIVE_FIXED }
}

/// Free and total bytes of the disk holding the folder at app path `dir`.
pub fn free_space(dir: &str) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let path: Vec<u16> = native(dir)
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        let (mut free, mut total) = (0u64, 0u64);
        unsafe {
            GetDiskFreeSpaceExW(
                windows::core::PCWSTR(path.as_ptr()),
                Some(&mut free),
                Some(&mut total),
                None,
            )
        }
        .ok()?;
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

/// Whether `path` is one of `drives()`.
fn is_drive(path: &str) -> bool {
    drives().iter().any(|(_, p)| p == path)
}

/// Ejects a disk from `drives()` (its app path) that's `ejectable`.
pub fn eject(path: &str) -> Result<(), String> {
    if !ejectable(path) {
        return Err(format!("{path} isn't a disk that can be ejected."));
    }
    let name = crate::device::basename(path).to_string();
    let failed = |why: String| {
        let why = if why.trim().is_empty() {
            "the disk is in use".to_string()
        } else {
            why.trim().to_string()
        };
        Err(format!(
            "Couldn't eject {name}: {why}. Close files open on it and try again."
        ))
    };
    #[cfg(target_os = "macos")]
    {
        let out = command("diskutil")
            .args(["eject", path])
            .output()
            .map_err(|e| format!("Can't run diskutil: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            failed(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // udisks (what desktops mount removable disks with) unmounts it, then powers the
        // disk off where it can; without it, `umount`, which works for disks in /etc/fstab.
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let dev = mounts(&text)
            .into_iter()
            .find(|(_, at)| at == path)
            .map(|(d, _)| d);
        let unmounted = match &dev {
            Some(dev) => command("udisksctl").args(["unmount", "-b", dev]).output(),
            None => command("umount").arg(path).output(),
        };
        let out = match unmounted {
            Ok(out) => out,
            Err(_) => command("umount")
                .arg(path)
                .output()
                .map_err(|e| format!("Can't run umount: {e}"))?,
        };
        if !out.status.success() {
            return failed(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        if let Some(dev) = dev {
            let _ = command("udisksctl")
                .args(["power-off", "-b", &dev])
                .output();
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

/// The network shares mounted now (SMB, NFS, WebDAV, FTP, AFP; by the app or not), as (address
/// like "smb://me@nas/Share" or "nfs://nas/export/photos", the app path it's at).
pub fn net_mounted() -> Vec<(String, String)> {
    #[cfg(target_os = "macos")]
    {
        let out = command("mount")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        net_in_mount(&out)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let mut out = net_in_proc(&text);
        // gvfs (what `gio mount` uses) keeps each share in a folder like smb-share:server=host,share=name.
        let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        let dir = Path::new(&run).join("gvfs");
        out.extend(
            std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let url = gvfs_url(&e.file_name().to_string_lossy())?;
                    Some((url, e.path().display().to_string()))
                }),
        );
        out
    }
    #[cfg(windows)]
    {
        let out = command("net")
            .arg("use")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        net_in_net_use(&out)
    }
}

/// "host:/path" (how NFS names what's mounted) as "nfs://host/path".
#[cfg(any(unix, test))]
fn nfs_of(what: &str) -> Option<String> {
    let (host, path) = what.split_once(":/")?;
    (!host.is_empty() && !host.contains('/')).then(|| {
        format!("nfs://{host}/{}", path.trim_end_matches('/'))
            .trim_end_matches('/')
            .to_string()
    })
}

/// Network mounts in `mount`'s output: "//me@nas/Share on /Volumes/Share (smbfs, ...)",
/// "nas:/export on /Volumes/export (nfs, ...)", "https://cloud/dav on /Volumes/dav (webdav, ...)".
#[cfg(any(target_os = "macos", test))]
pub(crate) fn net_in_mount(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (what, rest) = l.split_once(" on ")?;
            let (at, kind) = rest.rsplit_once(" (")?;
            let kind = kind.split([',', ')']).next()?.trim();
            let url = match kind {
                "smbfs" => format!("smb:{what}"),
                "afpfs" => format!("afp:{what}"),
                "nfs" => nfs_of(what)?,
                "webdav" | "ftp" => what.to_string(),
                _ => return None,
            };
            Some((url, at.to_string()))
        })
        .collect()
}

/// Network mounts in /proc/self/mounts, wherever they're mounted: NFS, SMB (cifs), sshfs, davfs.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn net_in_proc(text: &str) -> Vec<(String, String)> {
    mounts_typed(text)
        .into_iter()
        .filter_map(|(what, at, kind)| {
            let url = match kind.as_str() {
                "nfs" | "nfs4" => nfs_of(&what)?,
                "cifs" | "smb3" | "smbfs" => format!("smb:{}", what.replace('\\', "/")),
                "fuse.sshfs" => {
                    let (host, path) = what.split_once(':')?;
                    format!("sftp://{host}/{}", path.trim_start_matches('/'))
                }
                "davfs" | "fuse.davfs2" => what,
                _ => return None,
            };
            Some((url, at))
        })
        .collect()
}

/// The address of a gvfs mount folder: "smb-share:server=nas,share=photos,user=me",
/// "nfs:host=nas,prefix=%2Fexport", "dav:host=cloud,ssl=true,prefix=%2Fdav", "ftp:host=box".
#[cfg(any(all(unix, not(target_os = "macos")), test))]
pub(crate) fn gvfs_url(name: &str) -> Option<String> {
    let (kind, spec) = name.split_once(':')?;
    let field = |k: &str| {
        spec.split(',')
            .find_map(|f| f.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .map(|v| {
                percent_encoding::percent_decode_str(v)
                    .decode_utf8_lossy()
                    .into_owned()
            })
    };
    let user = field("user").map(|u| format!("{u}@")).unwrap_or_default();
    let port = field("port").map(|p| format!(":{p}")).unwrap_or_default();
    let prefix = field("prefix").unwrap_or_default();
    let prefix = prefix.trim_matches('/');
    let rest = if prefix.is_empty() {
        String::new()
    } else {
        format!("/{prefix}")
    };
    Some(match kind {
        "smb-share" => format!("smb://{user}{}{port}/{}", field("server")?, field("share")?),
        "nfs" => format!("nfs://{}{port}{rest}", field("host")?),
        "dav" | "davs" => {
            let tls = kind == "davs" || field("ssl").as_deref() == Some("true");
            format!(
                "{}://{user}{}{port}{rest}",
                if tls { "https" } else { "http" },
                field("host")?
            )
        }
        "ftp" | "sftp" | "afp-volume" => {
            let scheme = if kind == "afp-volume" { "afp" } else { kind };
            let vol = field("volume").map(|v| format!("/{v}")).unwrap_or_default();
            format!("{scheme}://{user}{}{port}{vol}{rest}", field("host")?)
        }
        _ => return None,
    })
}

/// Connected shares in `net use`'s output: "OK  Z:  \\nas\Share  Microsoft Windows Network",
/// or "... NFS Network" for an NFS export.
#[cfg(any(windows, test))]
pub(crate) fn net_in_net_use(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let words: Vec<&str> = l.split_whitespace().collect();
            let unc = words.iter().position(|w| w.starts_with(r"\\"))?;
            let letter = words[..unc]
                .iter()
                .find(|w| w.len() == 2 && w.ends_with(':'))?;
            let path = words[unc].trim_start_matches('\\').replace('\\', "/");
            let scheme = if words[unc + 1..]
                .iter()
                .any(|w| w.eq_ignore_ascii_case("NFS"))
            {
                "nfs"
            } else {
                "smb"
            };
            Some((
                format!("{scheme}://{path}"),
                format!("/{}", letter.to_uppercase()),
            ))
        })
        .collect()
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
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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

/// Mounts the SMB share at `url` ("smb://[user@]host/share"), or the NFS export at
/// "nfs://host/export" (always without `login`: NFS goes by this computer's user). Without
/// `login`, the system asks for the password when the share needs one (and may keep it in its
/// keychain). A login is given to the system on its input, so it's never in a command line.
pub fn smb_mount(url: &str, login: Option<&SmbLogin>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let out = {
        let mut script = format!("mount volume \"{}\"", applescript_text(url));
        if let Some(l) = login {
            let user = if l.domain.is_empty() {
                l.user.to_string()
            } else {
                format!("{};{}", l.domain, l.user)
            };
            script += &format!(
                " as user name \"{}\" with password \"{}\"",
                applescript_text(&user),
                applescript_text(l.password)
            );
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
        let answers = login.map(|l| {
            format!(
                "{}\n{}\n{}\n",
                l.user,
                if l.domain.is_empty() {
                    "WORKGROUP"
                } else {
                    l.domain
                },
                l.password
            )
        });
        run_with_input(c, &answers.unwrap_or_default())
    };
    #[cfg(windows)]
    let out = {
        let unc = url
            .trim_start_matches("smb:")
            .trim_start_matches("nfs:")
            .replace('/', "\\");
        // NFS: the NFS client's mount, on the next free letter, without a login.
        match (url.starts_with("nfs:"), login) {
            (true, _) => command("mount")
                .args(["-o", "anon", &unc, "*"])
                .stdin(std::process::Stdio::null())
                .output(),
            (false, None) => command("net")
                .args(["use", "*", &unc, "/persistent:no"])
                .stdin(std::process::Stdio::null())
                .output(),
            (false, Some(l)) => {
                let user = if l.domain.is_empty() {
                    l.user.to_string()
                } else {
                    format!("{}\\{}", l.domain, l.user)
                };
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
    let why = String::from_utf8_lossy(if out.stderr.is_empty() {
        &out.stdout
    } else {
        &out.stderr
    })
    .trim()
    .to_string();
    // osascript: "execution error: ... (-128)" is the password dialog's Cancel.
    if why.contains("(-128)") {
        return Err("Cancelled.".into());
    }
    Err(if why.is_empty() {
        format!("Couldn't mount {url}.")
    } else {
        format!("Couldn't mount {url}: {why}")
    })
}

/// Unmounts the network share mounted at `path` (an app path from `net_mounted`).
pub fn smb_unmount(path: &str) -> Result<(), String> {
    let tries: Vec<Command> = {
        #[cfg(target_os = "macos")]
        {
            let mut d = command("diskutil");
            d.args(["unmount", path]);
            let mut u = command("umount");
            u.arg(path);
            vec![d, u]
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // gio for what gvfs mounted; umount for NFS or SMB mounted by the system (which
            // works when /etc/fstab lets users unmount it).
            let mut g = command("gio");
            g.args(["mount", "-u", path]);
            let mut u = command("umount");
            u.arg(path);
            vec![g, u]
        }
        #[cfg(windows)]
        {
            let letter = path.trim_start_matches('/');
            let mut n = command("net");
            n.args(["use", letter, "/delete", "/y"]);
            // NFS drives (Windows' NFS client) are also unmounted with its own umount.
            let mut u = command("umount");
            u.args(["-f", letter]);
            vec![n, u]
        }
    };
    let mut why = String::new();
    for mut c in tries {
        match c.output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) if why.is_empty() => {
                why = String::from_utf8_lossy(if out.stderr.is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                })
                .trim()
                .to_string()
            }
            Ok(_) => {}
            Err(e) if why.is_empty() => why = e.to_string(),
            Err(_) => {}
        }
    }
    Err(format!(
        "Couldn't unmount {path}: {}. Close files open on it and try again.",
        if why.is_empty() { "it's in use" } else { &why }
    ))
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

    #[test]
    fn finds_the_file_system_holding_a_folder() {
        let mount = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                     /dev/disk4s1 on /Volumes/STICK (msdos, local, nodev, nosuid, noowners)\n\
                     /dev/disk5s1 on /Volumes/STICK 2 (exfat, local, nodev, nosuid, noowners)\n";
        assert_eq!(
            fs_in_mount(mount, "/Volumes/STICK/DCIM").as_deref(),
            Some("msdos")
        );
        assert_eq!(
            fs_in_mount(mount, "/Volumes/STICK").as_deref(),
            Some("msdos")
        );
        assert_eq!(
            fs_in_mount(mount, "/Volumes/STICK 2/a").as_deref(),
            Some("exfat")
        );
        assert_eq!(
            fs_in_mount(mount, "/Volumes/STICKS").as_deref(),
            Some("apfs")
        );
        assert_eq!(fs_in_mount(mount, "/Users/me").as_deref(), Some("apfs"));
    }

    #[cfg(windows)]
    #[test]
    fn drives_are_top_folders() {
        assert_eq!(native("/C:/Users/me"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(native("/C:"), PathBuf::from(r"C:\"));
        assert_eq!(app_path(Path::new(r"C:\Users\me")), "/C:/Users/me");
        assert_eq!(app_path(Path::new(r"\\?\D:\x\")), "/D:/x");
    }

    #[cfg(windows)]
    #[test]
    fn network_shares_keep_their_server() {
        assert_eq!(
            app_path(Path::new(r"\\nas\Photos\a.jpg")),
            "//nas/Photos/a.jpg"
        );
        assert_eq!(app_path(Path::new(r"\\?\UNC\nas\Photos\")), "//nas/Photos");
        assert_eq!(
            native("//nas/Photos/a.jpg"),
            PathBuf::from(r"\\nas\Photos\a.jpg")
        );
        assert_eq!(native("/C:/x"), PathBuf::from(r"C:\x"));
    }

    #[test]
    fn reads_network_mounts() {
        let mount = "/dev/disk3s1s1 on / (apfs, sealed, local)\n//me@nas._smb._tcp.local/Photos on /Volumes/Photos 1 (smbfs, nodev, nosuid, mounted by me)\n\
                     nas:/export/media/ on /Volumes/media (nfs, nodev, nosuid, mounted by me)\n\
                     https://cloud.example.com/dav/ on /Volumes/dav (webdav, nodev, noexec, nosuid, mounted by me)\n\
                     map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";
        assert_eq!(
            net_in_mount(mount),
            vec![
                (
                    "smb://me@nas._smb._tcp.local/Photos".to_string(),
                    "/Volumes/Photos 1".to_string()
                ),
                (
                    "nfs://nas/export/media".to_string(),
                    "/Volumes/media".to_string()
                ),
                (
                    "https://cloud.example.com/dav/".to_string(),
                    "/Volumes/dav".to_string()
                ),
            ]
        );
        let net =
            "Status       Local     Remote                    Network\n-----\nOK           Z:        \\\\nas\\Photos              Microsoft Windows Network\n\
                   OK           Y:        \\\\nas\\export\\media        NFS Network\nThe command completed successfully.\n";
        assert_eq!(
            net_in_net_use(net),
            vec![
                ("smb://nas/Photos".to_string(), "/Z:".to_string()),
                ("nfs://nas/export/media".to_string(), "/Y:".to_string()),
            ]
        );
        let proc = "/dev/sdb1 /media/me/STICK vfat rw 0 0\nnas:/export /home/me/nas nfs4 rw 0 0\n//nas/Photos /mnt/photos cifs rw 0 0\nme@box:/srv /home/me/box fuse.sshfs rw 0 0\n";
        assert_eq!(
            net_in_proc(proc),
            vec![
                ("nfs://nas/export".to_string(), "/home/me/nas".to_string()),
                ("smb://nas/Photos".to_string(), "/mnt/photos".to_string()),
                ("sftp://me@box/srv".to_string(), "/home/me/box".to_string()),
            ]
        );
        assert_eq!(
            gvfs_url("smb-share:server=nas,share=photos,user=me").as_deref(),
            Some("smb://me@nas/photos")
        );
        assert_eq!(
            gvfs_url("nfs:host=nas,prefix=%2Fexport%2Fmedia").as_deref(),
            Some("nfs://nas/export/media")
        );
        assert_eq!(
            gvfs_url("dav:host=cloud,ssl=true,prefix=%2Fdav").as_deref(),
            Some("https://cloud/dav")
        );
        assert_eq!(
            gvfs_url("ftp:host=box,port=2121").as_deref(),
            Some("ftp://box:2121")
        );
        assert_eq!(gvfs_url("mtp:host=Phone"), None);
        assert_eq!(applescript_text(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(powershell_text("it's"), "it''s");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn reads_mounts() {
        let text = "/dev/sdb1 /media/me/MY\\040STICK vfat rw 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(
            mounts(text)[0],
            ("/dev/sdb1".to_string(), "/media/me/MY STICK".to_string())
        );
    }
}
