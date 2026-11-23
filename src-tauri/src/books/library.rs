//! Books, many at a time: info for the library, checks, info changes and shrinking.
//! The work on each book is in batch.rs; these read and write it through the device.

use super::{batch, epub};
use crate::files::{find_in, Matches, Query};
use crate::*;

/// Every EPUB under `dir`, nearest first.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn library(state: State<'_, AppState>, src: String, dir: String, hidden: bool) -> Result<Matches, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let mut m = find_in(&dev, &dir, &Query::text(".epub"), hidden, &Default::default(), 5000, std::time::Duration::from_secs(90));
        m.entries.retain(|e| !e.dir && e.name.to_lowercase().ends_with(".epub"));
        Ok(m)
    })
    .await
}

/// A book's title, authors and so on, for the library. Remembered on the Mac until the file changes.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn book_info(state: State<'_, AppState>, src: String, path: String, size: u64, mtime: i64) -> Result<batch::Info, String> {
    if let Some(i) = batch::cached(&src, &path, size, mtime) {
        return Ok(i);
    }
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let info = match dev.read(&path) {
            Ok((data, _)) => batch::info(&data, &path),
            Err(e) => return Err(msg(e)),
        };
        batch::remember(&src, &path, size, mtime, &info);
        Ok(info)
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn book_check(state: State<'_, AppState>, src: String, path: String) -> Result<Vec<epub::Issue>, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let (data, _) = dev.read(&path).map_err(msg)?;
        Ok(batch::check(&data, &path))
    })
    .await
}

#[derive(Serialize)]
pub(crate) struct Changed {
    changed: bool,
    backup: bool,
}

/// Changes a book's info (see `batch::Change`) and writes it back, backing up the old file.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn book_change(state: State<'_, AppState>, src: String, path: String, change: batch::Change) -> Result<Changed, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let (data, version) = dev.read(&path).map_err(msg)?;
        let Some(new) = batch::change(&data, &path, &change)? else {
            return Ok(Changed { changed: false, backup: false });
        };
        let r = dev.write(&path, &new, Some(&version), false).map_err(msg)?;
        Ok(Changed { changed: true, backup: r.backup.is_some() })
    })
    .await
}

#[derive(Serialize)]
pub(crate) struct ShrinkResult {
    #[serde(flatten)]
    shrunk: batch::Shrunk,
    /// The smaller copy, made next to the book; None when it wouldn't be smaller.
    path: Option<String>,
}

/// Makes a smaller copy of a book next to it, named "… (smaller).epub". The book itself is left alone.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn book_shrink(state: State<'_, AppState>, src: String, path: String, max_side: u32, quality: u8) -> Result<ShrinkResult, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let (data, _) = dev.read(&path).map_err(msg)?;
        let (bytes, shrunk) = batch::shrink(&data, &path, max_side.max(200), quality.clamp(30, 95))?;
        if shrunk.after >= shrunk.before {
            return Ok(ShrinkResult { shrunk, path: None });
        }
        let to = device::join(&device::dirname(&path), &batch::smaller_name(device::basename(&path)));
        dev.write(&to, &bytes, None, true).map_err(|e| match e {
            DevError::Conflict(_) => format!("{} is already there. Delete or rename it first.", device::basename(&to)),
            e => e.to_string(),
        })?;
        Ok(ShrinkResult { shrunk, path: Some(to) })
    })
    .await
}
