//! What the preview reads: media headers, the first bytes, subtitles, files in a ZIP, PDF pages
//! and text, and EPUB chapters.

use super::*;

/// Width, height and length of a picture, video or sound file, from its header.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn media_info(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
) -> Result<crate::preview::media::MediaInfo, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        crate::preview::media::info(&name, &mut |at, len| {
            dev.read_range(&path, at, len).map_err(msg)
        })
    })
    .await
}

/// Bytes of any file, for the hex view: `PEEK_MAX` from `start`.
#[derive(Serialize)]
pub(crate) struct Bytes {
    bytes: Vec<u8>,
    next: Option<u64>,
}

#[tauri::command(rename_all = "snake_case")]
/// With `inner`, as for `peek_text`.
pub(crate) async fn peek_bytes(
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

/// A subtitle file found beside a video.
#[derive(Serialize)]
pub(crate) struct Subtitle {
    name: String,
    path: String,
}

/// The subtitle files in the folder of the video at `path` with a name like it (`subs::goes_with`).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn subtitles(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
) -> Result<Vec<Subtitle>, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let (dir, video) = match path.rsplit_once('/') {
            Some(("", v)) => ("/", v),
            Some((d, v)) => (d, v),
            None => return Ok(vec![]),
        };
        let mut found: Vec<Subtitle> = dev
            .listdir(dir)
            .map_err(msg)?
            .into_iter()
            .filter(|e| {
                !e.dir
                    && e.size <= crate::preview::subs::MAX
                    && crate::preview::subs::goes_with(video, &e.name)
            })
            .map(|e| Subtitle {
                name: e.name,
                path: e.path,
            })
            .collect();
        found.sort_by_key(|s| s.name.to_lowercase());
        Ok(found)
    })
    .await
}

/// The subtitle file at `path` (.srt, .sub or .vtt) as WebVTT, for the video player.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn subtitle(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
) -> Result<String, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let st = dev
            .stat(&path)
            .map_err(msg)?
            .ok_or_else(|| format!("No such file: {path}"))?;
        if st.dir || st.size > crate::preview::subs::MAX {
            return Err(format!("{path} isn't a subtitle file."));
        }
        let data = dev.read_range(&path, 0, st.size).map_err(msg)?;
        crate::preview::subs::to_vtt(&data, &path)
    })
    .await
}

/// Files packed in a ZIP are unpacked whole for the preview, up to this size.
pub(crate) const UNPACK_MAX: u64 = 64 * 1024 * 1024;

/// The file `name` packed in the ZIP at `path` (`size` bytes), unpacked. The last one is kept for a
/// minute after its last use, as the preview asks for it a piece at a time.
pub(crate) fn unpacked(
    dev: &Device,
    path: &str,
    size: u64,
    name: &str,
) -> Result<Arc<Vec<u8>>, String> {
    static LAST: crate::idle::Slot<(String, Arc<Vec<u8>>)> = Mutex::new(None);
    let key = format!("{}:{}\n{path}\n{size}\n{name}", dev.kind(), dev.label());
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(((k, data), used)) = last.as_mut() {
        if *k == key {
            *used = std::time::Instant::now();
            return Ok(data.clone());
        }
    }
    let watched = last.is_some();
    *last = None;
    let data = Arc::new(crate::preview::archive::unpack(
        size,
        &mut |at, len| dev.read_range(path, at, len).map_err(msg),
        name,
        UNPACK_MAX,
    )?);
    *last = Some(((key, data.clone()), std::time::Instant::now()));
    // Let go of after a minute unused: it can be up to UNPACK_MAX.
    if !watched {
        crate::idle::drop_when_idle(&LAST, std::time::Duration::from_secs(60), || {});
    }
    Ok(data)
}

/// Where the file `name` packed in the ZIP at `path` (`size` bytes) lies in it, when it's stored
/// as it is (see `archive::stored_span`). The last answer is kept, as media asks a piece at a time.
pub(crate) fn stored_span(
    dev: &Device,
    path: &str,
    size: u64,
    name: &str,
) -> Result<Option<(u64, u64)>, String> {
    static LAST: Mutex<Option<(String, Option<(u64, u64)>)>> = Mutex::new(None);
    let key = format!("{}:{}\n{path}\n{size}\n{name}", dev.kind(), dev.label());
    if let Some((k, span)) = LAST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *k == key {
            return Ok(*span);
        }
    }
    let span = crate::preview::archive::stored_span(
        size,
        &mut |at, len| dev.read_range(path, at, len).map_err(msg),
        name,
    )?;
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, span));
    Ok(span)
}

/// `len` bytes from `start` of the file, or of the file `inner` packed in it.
pub(super) fn read_part(
    dev: &Device,
    path: &str,
    inner: Option<&str>,
    zip_size: Option<u64>,
    start: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    match inner {
        None => dev.read_range(path, start, len).map_err(msg),
        Some(name) => {
            let size = zip_size.ok_or("The ZIP's size is missing.")?;
            if let Some((at, n)) = stored_span(dev, path, size, name)? {
                return if start >= n {
                    Ok(vec![])
                } else {
                    dev.read_range(path, at + start, len.min(n - start))
                        .map_err(msg)
                };
            }
            let data = unpacked(dev, path, size, name)?;
            let from = (start as usize).min(data.len());
            let to = (start.saturating_add(len) as usize).min(data.len());
            Ok(data[from..to].to_vec())
        }
    }
}

/// What's in a ZIP file of `size` bytes, read from its end.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn zip_list(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
) -> Result<crate::preview::archive::Listing, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        crate::preview::archive::list(size, &mut |at, len| {
            dev.read_range(&path, at, len).map_err(msg)
        })
    })
    .await
}

/// A PDF's page count and outline. `inner`, `zip_size`: as for `peek_text`.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pdf_outline(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
    mtime: i64,
    inner: Option<String>,
    zip_size: Option<u64>,
) -> Result<crate::preview::pdf::Outline, String> {
    let key = src.clone().unwrap_or_else(|| "mac".into());
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        crate::preview::pdf::outline(&*pdf_doc(
            &dev,
            &key,
            &path,
            size,
            mtime,
            inner.as_deref(),
            zip_size,
        )?)
    })
    .await
}

/// What a PDF says about itself (title, author, dates, version …), for the preview's Info tab.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pdf_info(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
    mtime: i64,
    inner: Option<String>,
    zip_size: Option<u64>,
) -> Result<crate::preview::pdf::Info, String> {
    let key = src.clone().unwrap_or_else(|| "mac".into());
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        Ok(crate::preview::pdf::info(&*pdf_doc(
            &dev,
            &key,
            &path,
            size,
            mtime,
            inner.as_deref(),
            zip_size,
        )?))
    })
    .await
}

/// A PDF opened by PDFium for the preview, kept for the next page drawn or search
/// (`pdfium::doc`). `inner`, `zip_size`: as for `peek_text`; `mtime` is the PDF's own.
pub(crate) fn pdf_doc(
    dev: &Device,
    src: &str,
    path: &str,
    size: u64,
    mtime: i64,
    inner: Option<&str>,
    zip_size: Option<u64>,
) -> Result<std::sync::Arc<crate::preview::pdfium::Doc>, String> {
    if size > UNPACK_MAX {
        return Err("Too big to show.".into());
    }
    let key = crate::preview::pdfium::Key {
        src: src.to_string(),
        path: path.to_string(),
        size,
        mtime,
        inner: inner.map(str::to_string),
    };
    crate::preview::pdfium::doc(key, || read_part(dev, path, inner, zip_size, 0, size))
}

/// The matches of `query` in a PDF's text, page by page, for the preview's Search tab.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pdf_find(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
    mtime: i64,
    inner: Option<String>,
    zip_size: Option<u64>,
    query: crate::books::epub::Query,
) -> Result<crate::books::epub::Found, String> {
    let key = src.clone().unwrap_or_else(|| "mac".into());
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        crate::preview::pdf::find(
            &*pdf_doc(&dev, &key, &path, size, mtime, inner.as_deref(), zip_size)?,
            &query,
        )
    })
    .await
}

/// Each page's size in points, for the preview to lay the pages out and draw them with PDFium
/// (`ferry://…/pdfpage`). None when PDFium isn't there: the preview shows the web view's viewer.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pdf_pages(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
    mtime: i64,
    inner: Option<String>,
    zip_size: Option<u64>,
) -> Result<Option<Vec<[f32; 2]>>, String> {
    let key = src.clone().unwrap_or_else(|| "mac".into());
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        if crate::preview::pdfium::pdfium().is_none() {
            return Ok(None);
        }
        Ok(Some(crate::preview::pdfium::sizes(&*pdf_doc(
            &dev,
            &key,
            &path,
            size,
            mtime,
            inner.as_deref(),
            zip_size,
        )?)))
    })
    .await
}

/// Where the matches of `query` are on `page` (from 1), to mark them on the page drawn.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pdf_marks(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
    mtime: i64,
    inner: Option<String>,
    zip_size: Option<u64>,
    page: usize,
    query: crate::books::epub::Query,
) -> Result<Vec<Vec<[f32; 4]>>, String> {
    let key = src.clone().unwrap_or_else(|| "mac".into());
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        crate::preview::pdf::marks(
            &*pdf_doc(&dev, &key, &path, size, mtime, inner.as_deref(), zip_size)?,
            page,
            &query,
        )
    })
    .await
}

/// The text on `page` (from 1) in runs placed on the page, for the preview to lay over the page
/// drawn, so it can be selected and copied.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pdf_text(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    size: u64,
    mtime: i64,
    inner: Option<String>,
    zip_size: Option<u64>,
    page: usize,
) -> Result<crate::preview::pdfium::Layer, String> {
    let key = src.clone().unwrap_or_else(|| "mac".into());
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        crate::preview::pdf::layer(
            &*pdf_doc(&dev, &key, &path, size, mtime, inner.as_deref(), zip_size)?,
            page,
        )
    })
    .await
}

/// Opens a link in a PDF shown in the browser or mail app: web and mail addresses only.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn open_link(url: String) -> Result<(), String> {
    if !crate::preview::pdfium::openable(&url) {
        return Err("Only web and mail links open.".into());
    }
    crate::platform::open_url(&url)
}

#[derive(serde::Serialize)]
pub(crate) struct EpubContents {
    title: String,
    chapters: Vec<crate::books::epub::ChapterRef>,
}

/// An EPUB's title and chapters, from its ZIP directory, package document and table of contents
/// only: the chapters and pictures aren't read.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_contents(
    state: State<'_, AppState>,
    src: String,
    path: String,
    size: u64,
) -> Result<EpubContents, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let book = crate::preview::archive::unpack_each(
            size,
            &mut |at, len| dev.read_range(&path, at, len).map_err(msg),
            UNPACK_MAX,
            |get| crate::books::epub::Book::contents_only(get),
        )??;
        Ok(EpubContents {
            title: book.meta().map(|m| m.title).unwrap_or_default(),
            chapters: book.chapters()?,
        })
    })
    .await
}

/// One chapter of an EPUB, unopened, as HTML for a frame without scripts. Its pictures and styles
/// load from the `inzip` route, one file at a time.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_peek(
    state: State<'_, AppState>,
    src: String,
    path: String,
    size: u64,
    chapter: String,
) -> Result<String, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let data = crate::preview::archive::unpack(
            size,
            &mut |at, len| dev.read_range(&path, at, len).map_err(msg),
            &chapter,
            UNPACK_MAX,
        )?;
        crate::books::epub::Book::chapter_alone(
            &chapter,
            data,
            &crate::protocol::inzip_base(&src, size, &path, &chapter),
        )
    })
    .await
}

/// Finds text in an EPUB being read (not opened in the editor): its chapters' text is read for
/// it, by range, without the rest of the book.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn epub_peek_find(
    state: State<'_, AppState>,
    src: String,
    path: String,
    size: u64,
    query: crate::books::epub::Query,
) -> Result<crate::books::epub::Found, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let book = crate::preview::archive::unpack_each(
            size,
            &mut |at, len| dev.read_range(&path, at, len).map_err(msg),
            UNPACK_MAX,
            |get| crate::books::epub::Book::with_chapters(get),
        )??;
        book.find(&query)
    })
    .await
}
