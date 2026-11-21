//! Safety: free space, the backups made before a change or delete, restoring them, and eject.

use super::*;

/// Free and total space of each storage on a device (each disk, for the Mac).
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn space(state: State<'_, AppState>, src: String) -> Result<Vec<device::Space>, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || Ok(dev.space())).await
}

#[derive(Serialize)]
pub(super) struct Backup {
    /// The backup's path on the computer (an app path).
    file: String,
    /// The file's name when it was backed up.
    name: String,
    size: u64,
    /// When the backup was made (seconds).
    mtime: i64,
    /// Where it came from, for backups made since the app started noting it.
    note: Option<device::BackupNote>,
}

/// The backups, newest first.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn backups() -> Result<Vec<Backup>, String> {
    blocking(|| Ok(list_backups(&device::backup_dir()))).await
}

pub(super) fn list_backups(dir: &std::path::Path) -> Vec<Backup> {
    let notes = device::BackupNote::read_all(dir);
    let mut out: Vec<Backup> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let m = e.metadata().ok().filter(|m| m.is_file())?;
            if file == "index.tsv" || file.starts_with('.') {
                return None;
            }
            let note = notes.get(&file).cloned();
            let name = note.as_ref().map(|n| device::basename(&n.path).to_string()).unwrap_or_else(|| backup_name(&file));
            let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
            Some(Backup { file: crate::platform::app_path(&e.path()), name, size: m.len(), mtime, note })
        })
        .collect();
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.file.cmp(&a.file)));
    out
}

/// The original name in a backup's file name: "20250101-120000-a.txt" or "20250101-120000-2-a.txt".
pub(super) fn backup_name(file: &str) -> String {
    let mut rest = file;
    for _ in 0..2 {
        match rest.split_once('-') {
            Some((head, tail)) if head.chars().all(|c| c.is_ascii_digit()) => rest = tail,
            _ => return file.to_string(),
        }
    }
    if let Some((n, tail)) = rest.split_once('-') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return tail.to_string();
        }
    }
    rest.to_string()
}

/// Puts a backup back at `path` on `src`. A file there now is replaced, and backed up first like
/// any other replaced file.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn restore(state: State<'_, AppState>, file: String, src: String, path: String) -> Result<WriteResult, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let dir = device::backup_dir();
        let f = crate::platform::native(&file);
        if f.parent() != Some(dir.as_path()) {
            return Err("That isn't one of the app's backups.".into());
        }
        if !f.is_file() {
            return Err(format!("Can't find the backup {file}."));
        }
        let parent = device::dirname(&path);
        if !dev.stat(&parent).map_err(msg)?.is_some_and(|e| e.dir) {
            return Err(format!("The folder {parent} isn't there any more. Restore it into another folder."));
        }
        let now = dev.version(&path).map_err(msg)?;
        dev.write_from(&path, &f, now.as_deref(), now.is_none()).map_err(msg)
    })
    .await
}

/// Moves the whole backups folder to the Trash, so it can still be got back from there. The app
/// starts a new one with the next backup.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn clear_backups() -> Result<(), String> {
    blocking(|| {
        let dir = device::backup_dir();
        if !dir.exists() {
            return Ok(());
        }
        #[allow(unused_mut)]
        let mut trash = trash::TrashContext::default();
        // The default asks the Finder through AppleScript, which can bring up a permission prompt.
        #[cfg(target_os = "macos")]
        {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            trash.set_delete_method(DeleteMethod::NsFileManager);
        }
        trash.delete(&dir).map_err(|e| format!("Couldn't move the backups to the Trash: {e}"))
    })
    .await
}

/// Ejects a disk (one of `connect::drives`).
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn eject(path: String) -> Result<(), String> {
    blocking(move || crate::platform::eject(&path)).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn backups_know_where_they_came_from() {
        assert_eq!(super::backup_name("20250101-120000-a-b.txt"), "a-b.txt");
        assert_eq!(super::backup_name("20250101-120000-2-a.txt"), "a.txt");
        assert_eq!(super::backup_name("odd.txt"), "odd.txt");
        let dir = std::env::temp_dir().join(format!("ferry-backup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let f = crate::platform::app_path(&dir.join("note.txt"));
        let v = mac.write(&f, b"one", None, true).unwrap().version;
        let b = mac.write(&f, b"two", Some(&v), false).unwrap().backup.unwrap();
        let listed = super::list_backups(&crate::device::backup_dir());
        let mine = listed.iter().find(|x| crate::platform::native(&x.file) == std::path::Path::new(&b)).expect("listed");
        let note = mine.note.as_ref().expect("noted");
        assert_eq!((note.place.as_str(), note.path.as_str(), note.reason.as_str()), ("mac", f.as_str(), "replaced"));
        assert_eq!(mine.name, "note.txt");
        std::fs::remove_file(&b).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
