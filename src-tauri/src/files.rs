//! Files on one device: listing, reading and writing text, new folders, delete, download and
//! upload, rename, opening in a Mac app or Quick Look, and finding by name.

use super::*;

/// Lists a folder. `current` (default true) says it's the folder `pane` (default 0) now shows;
/// the column view passes false for the folders to its left.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn list(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    current: Option<bool>,
    pane: Option<u32>,
) -> Result<Vec<Entry>, String> {
    let dev = state.source(src.as_deref())?;
    if current.unwrap_or(true) {
        let mut viewing = state.viewing.lock().unwrap_or_else(|e| e.into_inner());
        let pane = pane.unwrap_or(0);
        match src.as_deref() {
            Some("mac") | None => viewing.remove(&pane),
            Some(s) => viewing.insert(pane, (s.to_string(), device::norm(&path).map_err(msg)?)),
        };
    }
    blocking(move || dev.listdir(&path).map_err(msg)).await
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn refresh(state: State<'_, AppState>, src: Option<String>) -> Result<(), String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        dev.refresh();
        Ok(())
    })
    .await
}

#[derive(Serialize)]
pub(super) struct TextFile {
    text: String,
    version: String,
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn read_text(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<TextFile, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let st = dev.stat(&path).map_err(msg)?.ok_or_else(|| format!("No such file: {path}"))?;
        if st.dir {
            return Err(format!("{path} is a folder."));
        }
        if st.size > TEXT_MAX {
            return Err(format!("{path} is too big to edit here ({} MB). Download it instead.", st.size / 1024 / 1024));
        }
        let (data, version) = dev.read(&path).map_err(msg)?;
        let text = String::from_utf8(data).map_err(|_| format!("{path} isn't UTF-8 text. Download it instead."))?;
        Ok(TextFile { text, version })
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn write_text(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    text: String,
    base_version: Option<String>,
    create: bool,
) -> Result<WriteResult, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || dev.write(&path, text.as_bytes(), base_version.as_deref(), create).map_err(msg)).await
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn mkdir(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<(), String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || dev.mkdir(&path).map_err(msg)).await
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn remove(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<Option<String>, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || dev.remove(&path).map_err(msg)).await
}

/// Asks where to save, then copies the device file to the Mac. Returns the Mac path, or None if cancelled.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn download(app: AppHandle, state: State<'_, AppState>, src: String, path: String) -> Result<Option<String>, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let Some(dest) = app.dialog().file().set_file_name(device::basename(&path)).blocking_save_file() else {
            return Ok(None);
        };
        let dest = dest.into_path().map_err(|e| e.to_string())?;
        let (data, _) = dev.read(&path).map_err(msg)?;
        std::fs::write(&dest, data).map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
        Ok(Some(dest.display().to_string()))
    })
    .await
}

/// Asks for Mac files and copies them into a device folder. Existing files are replaced
/// (the old version is backed up first). Returns one line per file.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn upload(app: AppHandle, state: State<'_, AppState>, src: String, dir: String) -> Result<Vec<String>, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let Some(files) = app.dialog().file().blocking_pick_files() else { return Ok(vec![]) };
        let mut report = Vec::new();
        for f in files {
            let local = f.into_path().map_err(|e| e.to_string())?;
            let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let target = device::join(&dir, &name);
            let line = match std::fs::read(&local) {
                Err(e) => format!("{name}: can't read it ({e})"),
                Ok(data) => match dev.write(&target, &data, None, false) {
                    Ok(r) => match r.backup {
                        Some(_) => format!("{name}: replaced (old version backed up)"),
                        None => format!("{name}: uploaded"),
                    },
                    Err(e) => format!("{name}: {e}"),
                },
            };
            report.push(line);
        }
        Ok(report)
    })
    .await
}

/// Renames a file or folder in its folder. Returns the new path.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn rename(state: State<'_, AppState>, src: String, path: String, name: String) -> Result<String, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || dev.rename(&path, &name).map_err(msg)).await
}

/// Opens a file in its default Mac app. Phone files are copied to the app's cache first
/// (`~/Library/Caches/yomi-files/opened`), so changes made there don't go back to the phone.
/// Returns the path that was opened.
/// The file as a path on the Mac: itself for Mac files; device files are copied to the app's cache
/// first (`~/Library/Caches/yomi-files/opened`), so changes made to them don't go back to the device.
pub(super) fn local_copy(dev: &Device, mac: bool, path: &str) -> Result<std::path::PathBuf, String> {
    if mac {
        return Ok(std::path::PathBuf::from(path));
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (dev.label(), path).hash(&mut h);
    // One folder per device and path, so files with the same name don't overwrite each other.
    let dir = opened_dir().join(format!("{:016x}", h.finish()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    let (data, _) = dev.read(path).map_err(msg)?;
    let local = dir.join(device::basename(path));
    std::fs::write(&local, data).map_err(|e| format!("Can't write {}: {e}", local.display()))?;
    Ok(local)
}

/// Opens a file in its default Mac app (a device file: a copy of it). Returns the path opened.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn open_file(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<String, String> {
    let mac = src.as_deref() == Some("mac");
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let local = local_copy(&dev, mac, &path)?;
        let status = std::process::Command::new("open").arg(&local).status().map_err(|e| format!("Can't open {}: {e}", local.display()))?;
        if !status.success() {
            return Err(format!("No app on this Mac opens {}.", device::basename(&path)));
        }
        Ok(local.display().to_string())
    })
    .await
}

/// Shows a file in Quick Look (`qlmanage -p`), in its own window. A device file is copied first.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn quick_look(state: State<'_, AppState>, src: String, path: String) -> Result<(), String> {
    let mac = src == "mac";
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let local = local_copy(&dev, mac, &path)?;
        std::process::Command::new("qlmanage")
            .arg("-p")
            .arg(&local)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Can't start Quick Look: {e}"))?;
        Ok(())
    })
    .await
}

#[derive(Serialize)]
pub(super) struct Matches {
    pub(super) entries: Vec<Entry>,
    /// False when the search stopped early (too many results, or too long).
    pub(super) complete: bool,
}

/// Files and folders under `dir` whose names contain `query` (any case), nearest first.
/// Stops at 300 results or after 20 seconds. Folders starting with a dot are skipped unless `hidden`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn find(state: State<'_, AppState>, src: String, dir: String, query: String, hidden: bool) -> Result<Matches, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || Ok(find_in(&dev, &dir, &query, hidden, 300, std::time::Duration::from_secs(20)))).await
}

pub(super) fn find_in(dev: &Device, dir: &str, query: &str, hidden: bool, max: usize, time: std::time::Duration) -> Matches {
    let q = query.trim().to_lowercase();
    let start = std::time::Instant::now();
    let mut out = vec![];
    let mut queue = std::collections::VecDeque::from([dir.to_string()]);
    while let Some(d) = queue.pop_front() {
        if out.len() >= max || start.elapsed() > time {
            return Matches { entries: out, complete: false };
        }
        let Ok(mut items) = dev.listdir(&d) else { continue };
        items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        for e in items {
            if !hidden && e.name.starts_with('.') {
                continue;
            }
            if e.dir {
                queue.push_back(e.path.clone());
            }
            if e.name.to_lowercase().contains(&q) && out.len() < max {
                out.push(e);
            }
        }
    }
    Matches { entries: out, complete: true }
}

/// Where copies of phone files opened in Mac apps go.
pub(super) fn opened_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("yomi-files").join("opened")
}

#[cfg(test)]
mod tests {
    #[test]
    fn find_searches_subfolders_and_rename_refuses_taken_names() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("yomi-find-test-{}", std::process::id()));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join(".hid")).unwrap();
        for f in ["Book one.epub", "a/b/book two.EPUB", "a/notes.txt", ".hid/book.epub"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let r = root.display().to_string();
        let names = |hidden| -> Vec<String> {
            super::find_in(&mac, &r, "BOOK", hidden, 100, std::time::Duration::from_secs(5)).entries.into_iter().map(|e| e.name).collect()
        };
        assert_eq!(names(false), ["Book one.epub", "book two.EPUB"]);
        assert_eq!(names(true).len(), 3);
        let f = super::find_in(&mac, &r, "o", true, 2, std::time::Duration::from_secs(5));
        assert!(!f.complete && f.entries.len() == 2);
        // Rename.
        assert!(mac.rename(&format!("{r}/a/notes.txt"), "Book one.epub").is_ok(), "other folder, free");
        assert!(mac.rename(&format!("{r}/a/Book one.epub"), "b").is_err(), "taken by a folder");
        assert!(mac.rename(&format!("{r}/a/Book one.epub"), "x/y").is_err());
        let new = mac.rename(&format!("{r}/a/Book one.epub"), "BOOK ONE.epub").unwrap();
        assert!(new.ends_with("/a/BOOK ONE.epub"), "a change of case is allowed");
        assert!(crate::local::parse_df("Filesystem 1024-blocks Used Available Capacity\n/dev/disk3 100 40 60 40% /\n") == Some((60 * 1024, 100 * 1024)));
        fs::remove_dir_all(&root).unwrap();
    }
}
