//! The EPUB editor: an open book is kept in memory by id until it’s saved or closed.

use super::epub;
use crate::*;

// A book is read from the device once, edited in memory, and written back by epub_save.

#[derive(Serialize)]
pub(crate) struct BookView {
    id: u32,
    /// "mac" or a device id: where the book is saved back to.
    src: String,
    path: String,
    name: String,
    dirty: bool,
    meta: epub::Meta,
    chapters: Vec<epub::ChapterRef>,
    /// The table of contents, its entries in order with their depths.
    toc: Vec<epub::TocEntry>,
    cover: Option<String>,
    stats: epub::Stats,
}

pub(crate) fn book_view(id: u32, src: &str, b: &Book) -> Result<BookView, String> {
    Ok(BookView {
        id,
        src: src.to_string(),
        path: b.source.clone(),
        name: device::basename(&b.source).to_string(),
        dirty: b.dirty,
        meta: b.meta()?,
        chapters: b.chapters()?,
        toc: b.toc().unwrap_or_default(),
        cover: b.cover_path().map(|p| protocol::book_url(id, &p, b.rev)),
        stats: b.stats()?,
    })
}

/// Runs `f` on open book `id`, then returns the refreshed view of the book.
pub(crate) fn with_book<T>(state: &AppState, id: u32, f: impl FnOnce(&mut Book) -> Result<T, String>) -> Result<(T, BookView), String> {
    let src = state.book_src.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned().unwrap_or_default();
    let mut books = state.books.lock().unwrap_or_else(|e| e.into_inner());
    let book = books.get_mut(&id).ok_or("This book is no longer open.")?;
    let out = f(book)?;
    Ok((out, book_view(id, &src, book)?))
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_open(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<BookView, String> {
    let dev = state.source(src.as_deref())?;
    let book = blocking(move || {
        let (data, version) = dev.read(&path).map_err(msg)?;
        Book::open(&data, path, version)
    })
    .await?;
    let id = state.next_book.fetch_add(1, Ordering::Relaxed);
    let src = src.unwrap_or_default();
    let src = src.as_str();
    let view = book_view(id, src, &book)?;
    state.books.lock().unwrap_or_else(|e| e.into_inner()).insert(id, book);
    state.book_src.lock().unwrap_or_else(|e| e.into_inner()).insert(id, src.to_string());
    Ok(view)
}

#[derive(Serialize)]
pub(crate) struct ChapterDoc {
    source: String,
    /// The chapter as HTML for the visual editor, or None when its XHTML is broken.
    html: Option<String>,
    problem: Option<String>,
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_chapter(state: State<'_, AppState>, id: u32, path: String) -> Result<ChapterDoc, String> {
    let (doc, _) = with_book(&state, id, |b| {
        let source = b.text(&path)?;
        let (html, problem) = match b.visual_html(&path, &protocol::book_base(id, &path)) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e)),
        };
        Ok(ChapterDoc { source, html, problem })
    })?;
    Ok(doc)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_set_source(state: State<'_, AppState>, id: u32, path: String, source: String) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.set_source(&path, &source))?.1)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_set_body(state: State<'_, AppState>, id: u32, path: String, body: String) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.set_body(&path, &body))?.1)
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_set_meta(state: State<'_, AppState>, id: u32, meta: epub::Meta) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.set_meta(&meta))?.1)
}

/// Adds an entry to the table of contents, for the cursor's place in a chapter (as `epub_move`
/// is given it: `block` and `names`, or `offset`).
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn epub_toc_add(
    state: State<'_, AppState>,
    id: u32,
    path: String,
    block: Option<usize>,
    offset: Option<usize>,
    names: Option<Vec<String>>,
    label: String,
    level: epub::Level,
) -> Result<BookView, String> {
    let cut = match (block, offset) {
        (Some(k), _) => epub::Cut::Block(k),
        (None, Some(o)) => epub::Cut::Offset(o, false),
        (None, None) => epub::Cut::Block(0),
    };
    Ok(with_book(&state, id, |b| b.add_toc(&path, cut, names.as_deref(), &label, level))?.1)
}

/// Takes entry `index` out of the table of contents.
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_toc_remove(state: State<'_, AppState>, id: u32, index: usize) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.remove_toc(index))?.1)
}

/// Renames a chapter in the table of contents.
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_rename_chapter(state: State<'_, AppState>, id: u32, path: String, label: String) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.rename_chapter(&path, &label))?.1)
}

/// Asks for an image on the Mac and makes it the cover. None if cancelled.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_cover(app: AppHandle, state: State<'_, AppState>, id: u32) -> Result<Option<BookView>, String> {
    let picked = blocking(move || {
        let Some(f) = app.dialog().file().add_filter("Images", &["jpg", "jpeg", "png", "gif", "webp", "svg"]).blocking_pick_file() else {
            return Ok(None);
        };
        let local = f.into_path().map_err(|e| e.to_string())?;
        let ext = local.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let data = std::fs::read(&local).map_err(|e| format!("Can't read {}: {e}", local.display()))?;
        Ok(Some((data, ext)))
    })
    .await?;
    let Some((data, ext)) = picked else { return Ok(None) };
    Ok(Some(with_book(&state, id, |b| b.set_cover(data, &ext))?.1))
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_find(state: State<'_, AppState>, id: u32, query: epub::Query) -> Result<epub::Found, String> {
    Ok(with_book(&state, id, |b| b.find(&query))?.0)
}

#[derive(Serialize)]
pub(crate) struct Replaced {
    count: usize,
    book: BookView,
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_replace(state: State<'_, AppState>, id: u32, query: epub::Query) -> Result<Replaced, String> {
    let (count, book) = with_book(&state, id, |b| b.replace_all(&query))?;
    Ok(Replaced { count, book })
}

#[derive(Serialize)]
pub(crate) struct ReplacedOne {
    /// Where the replacement ends in the chapter's XHTML; None when no match was left there.
    at: Option<usize>,
    book: BookView,
}

/// Replaces one match in a chapter: the first at or after byte `from` of its XHTML.
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_replace_one(state: State<'_, AppState>, id: u32, query: epub::Query, path: String, from: usize) -> Result<ReplacedOne, String> {
    let (at, book) = with_book(&state, id, |b| b.replace_one(&query, &path, from))?;
    Ok(ReplacedOne { at, book })
}

#[derive(Serialize)]
pub(crate) struct MovedView {
    #[serde(flatten)]
    moved: epub::Moved,
    book: BookView,
}

/// Moves part of chapter `path` to the chapter before it (`next` false: what comes before the
/// cut) or after it (`next` true: the cut and what follows). The cut is `block`, a top-level
/// element's index, checked against `names` (what the visual view shows), or `offset`, a place
/// in the source; `through` takes the element at the offset with the part moved to the previous chapter.
#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_move(
    state: State<'_, AppState>,
    id: u32,
    path: String,
    next: bool,
    block: Option<usize>,
    offset: Option<usize>,
    through: Option<bool>,
    names: Option<Vec<String>>,
) -> Result<MovedView, String> {
    let cut = match (block, offset) {
        (Some(k), _) => epub::Cut::Block(k),
        (None, Some(o)) => epub::Cut::Offset(o, through.unwrap_or(false)),
        (None, None) => return Err("Put the cursor where the chapter should be cut.".into()),
    };
    let (moved, book) = with_book(&state, id, |b| b.move_part(&path, next, cut, names.as_deref()))?;
    Ok(MovedView { moved, book })
}

#[derive(Serialize)]
pub(crate) struct Saved {
    book: BookView,
    backup: bool,
}

/// Writes the book back where it came from. Refuses if that copy changed since it was opened.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_save(state: State<'_, AppState>, id: u32) -> Result<Saved, String> {
    let src = state.book_src.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    let dev = state.source(src.as_deref())?;
    let (bytes, path, version) = with_book(&state, id, |b| {
        b.touch_modified();
        Ok((b.to_bytes()?, b.source.clone(), b.version.clone()))
    })?
    .0;
    let r = blocking(move || dev.write(&path, &bytes, Some(&version), false).map_err(msg)).await?;
    let (_, book) = with_book(&state, id, |b| {
        b.version = r.version.clone();
        b.dirty = false;
        Ok(())
    })?;
    Ok(Saved { book, backup: r.backup.is_some() })
}

/// Saves a copy of the book (with any unsaved edits) on the Mac. Returns its path, or None if cancelled.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_export(app: AppHandle, state: State<'_, AppState>, id: u32) -> Result<Option<String>, String> {
    let ((bytes, name), _) = with_book(&state, id, |b| Ok((b.to_bytes()?, device::basename(&b.source).to_string())))?;
    blocking(move || {
        let Some(dest) = app.dialog().file().set_file_name(name).blocking_save_file() else { return Ok(None) };
        let dest = dest.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&dest, bytes).map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
        Ok(Some(dest.display().to_string()))
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) fn epub_close(state: State<'_, AppState>, id: u32) {
    state.books.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    state.book_src.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}
