use axum::{response::IntoResponse, Json};
use serde::Serialize;

// ---------------------------------------------------------------------------
// GET /fs/drives - enumerate logical drives (Windows) / stub (other platforms)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DriveInfo {
    pub letter: String,
    pub root: String,
    pub kind: &'static str,
    pub label: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

pub(super) async fn drives_handler() -> impl IntoResponse {
    let drives = tokio::task::spawn_blocking(list_drives)
        .await
        .unwrap_or_default();
    Json(drives)
}

#[cfg(windows)]
pub(super) fn list_drives() -> Vec<DriveInfo> {
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        },
    };

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();

    for bit in 0u32..26 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let letter = (b'A' + bit as u8) as char;
        let root_str = format!("{letter}:\\");
        let root_wide: Vec<u16> = root_str.encode_utf16().chain(std::iter::once(0)).collect();
        let root_pcwstr = PCWSTR(root_wide.as_ptr());

        let kind = match unsafe { GetDriveTypeW(root_pcwstr) } {
            DRIVE_FIXED => "fixed",
            DRIVE_REMOVABLE => "removable",
            DRIVE_REMOTE => "network",
            DRIVE_CDROM => "cdrom",
            DRIVE_RAMDISK => "ramdisk",
            _ => "unknown",
        };

        // Volume label (may fail for empty optical drives, etc.)
        let mut label_buf = vec![0u16; 256];
        let label = unsafe {
            GetVolumeInformationW(root_pcwstr, Some(&mut label_buf), None, None, None, None)
        }
        .ok()
        .and_then(|_| {
            let len = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16(&label_buf[..len]).ok()
        })
        .filter(|s| !s.is_empty());

        // Disk space (may fail for ejected media)
        let mut total = 0u64;
        let mut free = 0u64;
        let space_ok =
            unsafe { GetDiskFreeSpaceExW(root_pcwstr, None, Some(&mut total), Some(&mut free)) }
                .is_ok();

        out.push(DriveInfo {
            letter: format!("{letter}:"),
            root: root_str,
            kind,
            label,
            total_bytes: space_ok.then_some(total),
            free_bytes: space_ok.then_some(free),
        });
    }
    out
}

#[cfg(not(windows))]
pub(super) fn list_drives() -> Vec<DriveInfo> {
    vec![]
}
