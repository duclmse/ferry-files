//! Settings: the app’s folders on the computer, their sizes, opening and clearing them.

use super::*;
use crate::files::opened_dir;

#[derive(Serialize)]
pub(super) struct Folder {
    path: String,
    files: u64,
    bytes: u64,
}

/// The folders the app keeps on the Mac: "backups", "thumbs" (the thumbnail cache) or "opened"
/// (copies of phone files opened in the computer's apps).
pub(super) fn app_folder(which: &str) -> Result<std::path::PathBuf, String> {
    match which {
        "backups" => Ok(device::backup_dir()),
        "thumbs" => Ok(thumbs::cache_dir()),
        "opened" => Ok(opened_dir()),
        "scripts" => Ok(crate::platform::scripts_dir()),
        other => Err(format!("Unknown folder {other:?}.")),
    }
}

pub(super) fn folder_size(dir: &std::path::Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    for item in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match item.metadata() {
            Ok(m) if m.is_dir() => {
                let (f, b) = folder_size(&item.path());
                files += f;
                bytes += b;
            }
            Ok(m) => {
                files += 1;
                bytes += m.len();
            }
            Err(_) => {}
        }
    }
    (files, bytes)
}

/// Where the backups and the thumbnail cache are, and how much they hold.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn app_folders() -> Result<HashMap<String, Folder>, String> {
    blocking(|| {
        let mut out = HashMap::new();
        for which in ["backups", "thumbs", "opened", "scripts"] {
            let dir = app_folder(which)?;
            let (files, bytes) = folder_size(&dir);
            out.insert(which.to_string(), Folder { path: crate::platform::app_path(&dir), files, bytes });
        }
        Ok(out)
    })
    .await
}

/// Shows one of the app's folders in the file manager, creating it if it isn't there yet.
#[tauri::command(rename_all = "snake_case")]
pub(super) fn open_folder(which: String) -> Result<(), String> {
    let dir = app_folder(&which)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    crate::platform::open(&dir)
}

/// Empties the thumbnail cache (made again when shown) or the copies of opened phone files.
/// Backups are cleared from the Backups window instead (`safety::clear_backups`), to the Trash.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn clear_cache(which: String) -> Result<(), String> {
    blocking(move || {
        if !matches!(which.as_str(), "thumbs" | "opened") {
            return Err(format!("{which:?} can't be cleared."));
        }
        let dir = app_folder(&which)?;
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Can't clear {}: {e}", dir.display())),
            _ => Ok(()),
        }
    })
    .await
}
