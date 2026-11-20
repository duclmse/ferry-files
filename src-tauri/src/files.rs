//! Files on one device: listing, reading and writing text, new folders, delete, download and
//! upload, rename, opening in the computer's apps or Quick Look, and finding by name.

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
    note_viewing(&state, src.as_deref(), &path, current, pane)?;
    blocking(move || dev.listdir(&path).map_err(msg)).await
}

/// Part of a folder listing, on devices whose folders can be too big to list at once.
#[derive(Serialize)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// How many items the folder has in all.
    pub total: u64,
}

/// Like `list`, but up to `count` items from `offset`, sorted by `sort` ("name", "size", "date"
/// or "kind"; ascending unless `asc` is false) where the device can sort.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn list_page(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    current: Option<bool>,
    pane: Option<u32>,
    offset: u64,
    count: u64,
    sort: Option<String>,
    asc: Option<bool>,
) -> Result<Page, String> {
    let dev = state.source(src.as_deref())?;
    note_viewing(&state, src.as_deref(), &path, current, pane)?;
    blocking(move || {
        let (entries, total) = dev.listdir_page(&path, offset, count, (sort.as_deref().unwrap_or("name"), asc.unwrap_or(true))).map_err(msg)?;
        Ok(Page { entries, total })
    })
    .await
}

/// Keeps which folder `pane` shows, when it's the `current` one (the default).
fn note_viewing(state: &AppState, src: Option<&str>, path: &str, current: Option<bool>, pane: Option<u32>) -> Result<(), String> {
    if current.unwrap_or(true) {
        let mut viewing = state.viewing.lock().unwrap_or_else(|e| e.into_inner());
        let pane = pane.unwrap_or(0);
        match src {
            Some("mac") | None => viewing.remove(&pane),
            Some(s) => viewing.insert(pane, (s.to_string(), device::norm(path).map_err(msg)?)),
        };
    }
    Ok(())
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

/// A piece of a text file, for the preview.
#[derive(Serialize)]
pub(super) struct Peek {
    text: String,
    /// Where the next piece starts, when the file goes on.
    next: Option<u64>,
    /// Colours for code, as (kind, bytes) runs over `text` (see `highlight`); None when plain.
    runs: Option<Vec<(u8, u32)>>,
    /// Colours stopped in this piece: the rest of the file is plain.
    colours_stopped: bool,
}

/// Bytes the preview reads at a time: enough to fill it, quick even over MTP.
const PEEK_MAX: u64 = 64 * 1024;

/// The text in `data`, read from `start`. When the file goes on (`more`), the piece ends after its
/// last full line, so the next one starts on a fresh line (and colours carry over cleanly); a piece
/// that's all one line ends before a character cut in two. Bytes that aren't UTF-8 show as �.
fn peek_of(mut data: Vec<u8>, start: u64, more: bool) -> Peek {
    if more {
        match data.iter().rposition(|&b| b == b'\n') {
            Some(i) => data.truncate(i + 1),
            None => {
                if let Err(e) = std::str::from_utf8(&data) {
                    if e.error_len().is_none() {
                        data.truncate(e.valid_up_to());
                    }
                }
            }
        }
    }
    let next = more.then(|| start + data.len() as u64);
    Peek { text: String::from_utf8_lossy(&data).into_owned(), next, runs: None, colours_stopped: false }
}

/// Reads `PEEK_MAX` bytes of the file from `start` (0 when left out), never the whole of a big one.
/// With `inner` (and the ZIP's `zip_size`), the file of that name packed in the ZIP at `path`.
/// With `colour`, code is coloured (`highlight`).
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn peek_text(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    start: Option<u64>,
    inner: Option<String>,
    zip_size: Option<u64>,
    colour: Option<bool>,
) -> Result<Peek, String> {
    let dev = state.source(src.as_deref())?;
    let start = start.unwrap_or(0);
    blocking(move || {
        let mut data = read_part(&dev, &path, inner.as_deref(), zip_size, start, PEEK_MAX + 1)?;
        let more = data.len() as u64 > PEEK_MAX;
        data.truncate(PEEK_MAX as usize);
        let mut p = peek_of(data, start, more);
        if colour.unwrap_or(false) {
            let key = format!("{}:{}\n{path}\n{}", dev.kind(), dev.label(), inner.as_deref().unwrap_or(""));
            let c = crate::highlight::colour(&key, inner.as_deref().unwrap_or(&path), start, &p.text, p.next);
            (p.runs, p.colours_stopped) = (c.runs, c.stopped);
        }
        Ok(p)
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

/// Removes a file, or a folder with everything in it: each file backed up, like any delete, then
/// the folders, deepest first. Used to undo a copy. Stops at the first error.
pub(super) fn remove_all(dev: &Device, path: &str) -> Result<u32, String> {
    let Some(st) = dev.stat(path).map_err(msg)? else {
        return Err(format!("{path} isn't there any more."));
    };
    let mut backups = 0;
    if st.dir {
        for e in dev.listdir(path).map_err(msg)? {
            backups += remove_all(dev, &e.path)?;
        }
    }
    if dev.remove(path).map_err(msg)?.is_some() {
        backups += 1;
    }
    Ok(backups)
}

/// `remove_all` on a device. Returns how many files were backed up.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn remove_tree(state: State<'_, AppState>, src: String, path: String) -> Result<u32, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || remove_all(&dev, &path)).await
}

/// Asks where to save, then copies the device file to the computer. Returns where it went, or None if cancelled.
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

/// Asks for files on the computer and copies them into a device folder. Existing files are replaced
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

/// The file as a path on the computer: itself for the computer's files; device files are copied to
/// the app's cache first (`files::opened_dir`), so changes made to them don't go back to the device.
pub(super) fn local_copy(dev: &Device, mac: bool, path: &str) -> Result<std::path::PathBuf, String> {
    if mac {
        return Ok(crate::platform::native(path));
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

/// Opens a file in its default app (a device file: a copy of it). Returns the path opened.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn open_file(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<String, String> {
    let mac = src.as_deref() == Some("mac");
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let local = local_copy(&dev, mac, &path)?;
        crate::platform::open(&local)
            .map_err(|_| format!("No app on this {} opens {}.", crate::platform::COMPUTER, device::basename(&path)))?;
        Ok(local.display().to_string())
    })
    .await
}

/// Shows a file in Quick Look, in its own window (elsewhere than macOS, opens it in its default
/// app). A device file is copied first.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn quick_look(state: State<'_, AppState>, src: String, path: String) -> Result<(), String> {
    let mac = src == "mac";
    let dev = state.source(Some(&src))?;
    blocking(move || crate::platform::quick_look(&local_copy(&dev, mac, &path)?)).await
}

#[derive(Serialize)]
pub(super) struct Matches {
    pub(super) entries: Vec<Entry>,
    /// False when the search stopped early (too many results, or too long).
    pub(super) complete: bool,
}

/// What else a search asks of what it finds, besides its name.
#[derive(serde::Deserialize, Default)]
pub(super) struct Filters {
    /// "folder", "picture", "video", "audio", "document", "book" or "archive".
    kind: Option<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    /// Modified at or after / before, in seconds since 1970.
    after: Option<i64>,
    before: Option<i64>,
}

/// What kind of file a name is, by its extension, as the search's kind filter names them.
pub(super) fn kind_of(name: &str) -> &'static str {
    let x = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
    let is = |list: &[&str]| list.contains(&x.as_str());
    if is(&["jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "avif", "svg", "tif", "tiff", "dng", "raw"]) {
        "picture"
    } else if is(&["mp4", "m4v", "mov", "webm", "3gp", "mkv", "avi"]) {
        "video"
    } else if is(&["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "amr"]) {
        "audio"
    } else if is(&["epub", "mobi", "azw", "azw3", "fb2", "cbz", "cbr"]) {
        "book"
    } else if is(&["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "odp", "pages", "numbers", "key"]) {
        "document"
    } else if is(&["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "apk"]) {
        "archive"
    } else {
        ""
    }
}

impl Filters {
    fn accepts(&self, e: &device::Entry) -> bool {
        let file_only = self.min_size.is_some() || self.max_size.is_some() || self.after.is_some() || self.before.is_some();
        match self.kind.as_deref() {
            None | Some("") => {}
            Some("folder") => return e.dir,
            Some(k) => {
                if e.dir || kind_of(&e.name) != k {
                    return false;
                }
            }
        }
        if file_only && e.dir {
            return false;
        }
        self.min_size.is_none_or(|m| e.size >= m)
            && self.max_size.is_none_or(|m| e.size < m)
            && self.after.is_none_or(|t| e.mtime >= t)
            && self.before.is_none_or(|t| e.mtime < t)
    }
}

/// Whether `name` fits `pattern`, where * stands for any run of characters and ? for one. Both
/// already lowercased.
pub(super) fn glob(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|i| glob(rest, &name[i..])),
        Some(('?', rest)) => !name.is_empty() && glob(rest, &name[1..]),
        Some((c, rest)) => name.first() == Some(c) && glob(rest, &name[1..]),
    }
}

/// Files and folders under `dir` whose names contain `query` (any case; with * or ?, whose names
/// fit it as a pattern; with `regex`, that match it as a regular expression, any case) and that
/// pass `filters`, nearest first. An empty query finds everything
/// the filters let through. Stops at 1000 results or after 30 seconds. Folders starting with a dot
/// are skipped unless `hidden`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn find(state: State<'_, AppState>, src: String, dir: String, query: String, hidden: bool, filters: Option<Filters>, regex: Option<bool>) -> Result<Matches, String> {
    let dev = state.source(Some(&src))?;
    let filters = filters.unwrap_or_default();
    let regex = regex.unwrap_or(false);
    let text = query.clone();
    let query = if regex { Query::regex(&query)? } else { Query::text(&query) };
    blocking(move || {
        // From the device's own index when it has one (an Everything server), checked again here.
        if let Some(found) = dev.search(&dir, &text, regex, 1000) {
            let (entries, all) = found.map_err(msg)?;
            let under = device::norm(&dir).map_err(msg)?;
            let shown = |e: &Entry| hidden || !e.path[under.len()..].split('/').any(|p| p.starts_with('.'));
            let entries: Vec<Entry> = entries.into_iter().filter(|e| shown(e) && query.fits(&e.name) && filters.accepts(e)).collect();
            return Ok(Matches { entries, complete: all });
        }
        Ok(find_in(&dev, &dir, &query, hidden, &filters, 1000, std::time::Duration::from_secs(30)))
    })
    .await
}

/// What `find_in` looks for in names.
pub(super) enum Query {
    /// Contained in the name, any case.
    Has(String),
    /// A pattern with * and ?, lowercased.
    Glob(Vec<char>),
    Regex(regex::Regex),
}

impl Query {
    /// Plain text, or a pattern when it has * or ?.
    pub(super) fn text(query: &str) -> Self {
        let q = query.trim().to_lowercase();
        if q.contains('*') || q.contains('?') {
            Query::Glob(q.chars().collect())
        } else {
            Query::Has(q)
        }
    }

    /// A regular expression, any case, or why it isn't one.
    pub(super) fn regex(query: &str) -> Result<Self, String> {
        regex::RegexBuilder::new(query.trim())
            .case_insensitive(true)
            .size_limit(1 << 20)
            .build()
            .map(Query::Regex)
            .map_err(|e| format!("That regular expression doesn't work: {e}"))
    }

    fn fits(&self, name: &str) -> bool {
        match self {
            Query::Has(q) => name.to_lowercase().contains(q),
            Query::Glob(p) => glob(p, &name.to_lowercase().chars().collect::<Vec<_>>()),
            Query::Regex(re) => re.is_match(name),
        }
    }
}

pub(super) fn find_in(dev: &Device, dir: &str, query: &Query, hidden: bool, filters: &Filters, max: usize, time: std::time::Duration) -> Matches {
    let fits = |name: &str| query.fits(name);
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
            if out.len() < max && fits(&e.name) && filters.accepts(&e) {
                out.push(e);
            }
        }
    }
    Matches { entries: out, complete: true }
}

/// Where copies of phone files opened in the computer's apps go.
pub(super) fn opened_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("ferry-files").join("opened")
}

/// Width, height and length of a picture, video or sound file, from its header.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn media_info(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<crate::media::MediaInfo, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        crate::media::info(&name, &mut |at, len| dev.read_range(&path, at, len).map_err(msg))
    })
    .await
}

/// Bytes of any file, for the hex view: `PEEK_MAX` from `start`.
#[derive(Serialize)]
pub(super) struct Bytes {
    bytes: Vec<u8>,
    next: Option<u64>,
}

#[tauri::command(rename_all = "snake_case")]
/// With `inner`, as for `peek_text`.
pub(super) async fn peek_bytes(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    start: Option<u64>,
    inner: Option<String>,
    zip_size: Option<u64>,
) -> Result<Bytes, String> {
    let dev = state.source(src.as_deref())?;
    let start = start.unwrap_or(0);
    blocking(move || {
        let mut bytes = read_part(&dev, &path, inner.as_deref(), zip_size, start, PEEK_MAX + 1)?;
        let next = (bytes.len() as u64 > PEEK_MAX).then_some(start + PEEK_MAX);
        bytes.truncate(PEEK_MAX as usize);
        Ok(Bytes { bytes, next })
    })
    .await
}

/// Files packed in a ZIP are unpacked whole for the preview, up to this size.
pub(super) const UNPACK_MAX: u64 = 64 * 1024 * 1024;

/// The file `name` packed in the ZIP at `path` (`size` bytes), unpacked. The last one is kept, as
/// the preview asks for it a piece at a time.
pub(super) fn unpacked(dev: &Device, path: &str, size: u64, name: &str) -> Result<Arc<Vec<u8>>, String> {
    static LAST: Mutex<Option<(String, Arc<Vec<u8>>)>> = Mutex::new(None);
    let key = format!("{}:{}\n{path}\n{size}\n{name}", dev.kind(), dev.label());
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, data)) = last.as_ref() {
        if *k == key {
            return Ok(data.clone());
        }
    }
    *last = None;
    let data = Arc::new(crate::archive::unpack(size, &mut |at, len| dev.read_range(path, at, len).map_err(msg), name, UNPACK_MAX)?);
    *last = Some((key, data.clone()));
    Ok(data)
}

/// `len` bytes from `start` of the file, or of the file `inner` packed in it.
fn read_part(dev: &Device, path: &str, inner: Option<&str>, zip_size: Option<u64>, start: u64, len: u64) -> Result<Vec<u8>, String> {
    match inner {
        None => dev.read_range(path, start, len).map_err(msg),
        Some(name) => {
            let size = zip_size.ok_or("The ZIP's size is missing.")?;
            let data = unpacked(dev, path, size, name)?;
            let from = (start as usize).min(data.len());
            let to = (start.saturating_add(len) as usize).min(data.len());
            Ok(data[from..to].to_vec())
        }
    }
}

/// What's in a ZIP file of `size` bytes, read from its end.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn zip_list(state: State<'_, AppState>, src: Option<String>, path: String, size: u64) -> Result<crate::archive::Listing, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || crate::archive::list(size, &mut |at, len| dev.read_range(&path, at, len).map_err(msg))).await
}

/// The files on the system clipboard (copied in Finder or File Explorer), as app paths.
#[tauri::command(rename_all = "snake_case")]
pub(super) fn clip_files() -> Vec<String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get().file_list())
        .map(|v| v.iter().map(|p| crate::platform::app_path(p)).collect())
        .unwrap_or_default()
}

/// Puts files on the system clipboard, for Finder to paste: `paths` (app paths on the
/// computer), or when there are none (they're on a phone), `text` (their names).
#[tauri::command(rename_all = "snake_case")]
pub(super) fn clip_set(paths: Vec<String>, text: String) -> Result<(), String> {
    let mut c = arboard::Clipboard::new().map_err(|e| format!("Couldn't use the clipboard: {e}"))?;
    let r = if paths.is_empty() {
        c.set_text(text)
    } else {
        c.set().file_list(&paths.iter().map(|p| crate::platform::native(p)).collect::<Vec<_>>())
    };
    r.map_err(|e| format!("Couldn't copy: {e}"))
}

/// Puts text on the clipboard.
#[tauri::command(rename_all = "snake_case")]
pub(super) fn copy_text(app: AppHandle, text: String) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard().write_text(text).map_err(|e| format!("Couldn't copy: {e}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn peek_drops_a_cut_character() {
        // "é" is two bytes; cut after the first, it starts the next piece.
        let p = super::peek_of(vec![b'a', 0xC3], 10, true);
        assert_eq!((p.text.as_str(), p.next), ("a", Some(11)));
        let p = super::peek_of(vec![b'a', 0xC3, 0xA9], 0, false);
        assert_eq!((p.text.as_str(), p.next), ("aé", None));
        let p = super::peek_of(vec![b'a', 0xFF, b'b'], 0, true);
        assert_eq!((p.text.as_str(), p.next), ("a\u{FFFD}b", Some(3)));
        // With lines, it ends after the last full one.
        let p = super::peek_of(b"one\ntwo\nthr".to_vec(), 5, true);
        assert_eq!((p.text.as_str(), p.next), ("one\ntwo\n", Some(13)));
        let p = super::peek_of(b"one\ntwo\nthr".to_vec(), 5, false);
        assert_eq!((p.text.as_str(), p.next), ("one\ntwo\nthr", None));
    }

    #[test]
    fn find_searches_subfolders_and_rename_refuses_taken_names() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("ferry-find-test-{}", std::process::id()));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join(".hid")).unwrap();
        for f in ["Book one.epub", "a/b/book two.EPUB", "a/notes.txt", ".hid/book.epub"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let r = crate::platform::app_path(&root);
        let names = |hidden| -> Vec<String> {
            super::find_in(&mac, &r, &super::Query::text("BOOK"), hidden, &Default::default(), 100, std::time::Duration::from_secs(5)).entries.into_iter().map(|e| e.name).collect()
        };
        assert_eq!(names(false), ["Book one.epub", "book two.EPUB"]);
        assert_eq!(names(true).len(), 3);
        let f = super::find_in(&mac, &r, &super::Query::text("o"), true, &Default::default(), 2, std::time::Duration::from_secs(5));
        assert!(!f.complete && f.entries.len() == 2);
        // Patterns and filters.
        let with = |q: &str, f: super::Filters| -> Vec<String> {
            let mut v: Vec<String> = super::find_in(&mac, &r, &super::Query::text(q), false, &f, 100, std::time::Duration::from_secs(5)).entries.into_iter().map(|e| e.name).collect();
            v.sort();
            v
        };
        assert_eq!(with("book*.epub", Default::default()), ["Book one.epub", "book two.EPUB"]);
        assert_eq!(with("?otes.*", Default::default()), ["notes.txt"]);
        assert_eq!(with("", super::Filters { kind: Some("book".into()), ..Default::default() }), ["Book one.epub", "book two.EPUB"]);
        assert_eq!(with("", super::Filters { kind: Some("folder".into()), ..Default::default() }), ["a", "b"]);
        assert_eq!(with("", super::Filters { min_size: Some(2), ..Default::default() }), Vec::<String>::new());
        assert_eq!(with("o", super::Filters { after: Some(0), ..Default::default() }).len(), 3, "files only, no folders");
        // Regular expressions, any case.
        let re = |q: &str| -> Vec<String> {
            let mut v: Vec<String> = super::find_in(&mac, &r, &super::Query::regex(q).unwrap(), false, &Default::default(), 100, std::time::Duration::from_secs(5)).entries.into_iter().map(|e| e.name).collect();
            v.sort();
            v
        };
        assert_eq!(re(r"^book \w+\.epub$"), ["Book one.epub", "book two.EPUB"]);
        assert_eq!(re(r"^.$"), ["a", "b"]);
        assert!(super::Query::regex("(").is_err());
        // Rename.
        assert!(mac.rename(&format!("{r}/a/notes.txt"), "Book one.epub").is_ok(), "other folder, free");
        assert!(mac.rename(&format!("{r}/a/Book one.epub"), "b").is_err(), "taken by a folder");
        assert!(mac.rename(&format!("{r}/a/Book one.epub"), "x/y").is_err());
        let new = mac.rename(&format!("{r}/a/Book one.epub"), "BOOK ONE.epub").unwrap();
        assert!(new.ends_with("/a/BOOK ONE.epub"), "a change of case is allowed");
        assert!(crate::platform::parse_df("Filesystem 1024-blocks Used Available Capacity\n/dev/disk3 100 40 60 40% /\n") == Some((60 * 1024, 100 * 1024)));
        fs::remove_dir_all(&root).unwrap();
    }
}
