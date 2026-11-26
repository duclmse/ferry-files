//! Finding files: by name in fsd's index, or by walking a folder with filters for kind, size
//! and date.

use super::*;

/// Files and folders on the computer whose names have `text` in them, from fsd's index (the
/// palette's `:`): up to 200, in fsd's order. Err says why not (fsd isn't running, still indexing,
/// or this build has no fsd).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn find_files(text: String) -> Result<Vec<Entry>, String> {
    #[cfg(feature = "fsd")]
    {
        blocking(move || crate::fsd::find(&text, 200)).await
    }
    #[cfg(not(feature = "fsd"))]
    {
        let _ = text;
        Err("This build of the app has no fsd searches.".into())
    }
}

/// Shows a file or folder on this computer in its folder, in Finder (File Explorer, Files).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn reveal(path: String) -> Result<(), String> {
    blocking(move || {
        let local = crate::platform::native(&path);
        tauri_plugin_opener::reveal_item_in_dir(&local)
            .map_err(|e| format!("Can't show {}: {e}", local.display()))
    })
    .await
}

/// Shows a file in Quick Look, in its own window (elsewhere than macOS, opens it in its default
/// app). A device file is copied first.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn quick_look(
    state: State<'_, AppState>,
    src: String,
    path: String,
) -> Result<(), String> {
    let mac = src == "mac";
    let dev = state.source(Some(&src))?;
    blocking(move || crate::platform::quick_look(&local_copy(&dev, mac, &path)?)).await
}

#[derive(Serialize)]
pub(crate) struct Matches {
    pub(crate) entries: Vec<Entry>,
    /// False when the search stopped early (too many results, or too long).
    pub(crate) complete: bool,
}

/// What else a search asks of what it finds, besides its name.
#[derive(serde::Deserialize, Default)]
pub(crate) struct Filters {
    /// "folder", "picture", "video", "audio", "document", "book" or "archive".
    pub(super) kind: Option<String>,
    pub(super) min_size: Option<u64>,
    pub(super) max_size: Option<u64>,
    /// Modified at or after / before, in seconds since 1970.
    pub(super) after: Option<i64>,
    pub(super) before: Option<i64>,
}

/// The search's kinds of file and the extensions of each.
pub(super) const KINDS: [(&str, &[&str]); 6] = [
    (
        "picture",
        &[
            "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "avif", "svg", "tif",
            "tiff", "dng", "raw",
        ],
    ),
    ("video", &["mp4", "m4v", "mov", "webm", "3gp", "mkv", "avi"]),
    (
        "audio",
        &[
            "mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "amr",
        ],
    ),
    (
        "book",
        &["epub", "mobi", "azw", "azw3", "fb2", "cbz", "cbr"],
    ),
    (
        "document",
        &[
            "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt",
            "pptx", "odp", "pages", "numbers", "key",
        ],
    ),
    (
        "archive",
        &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "apk"],
    ),
];

/// What kind of file a name is, by its extension, as the search's kind filter names them.
pub(crate) fn kind_of(name: &str) -> &'static str {
    let x = name
        .rsplit_once('.')
        .map(|(_, x)| x.to_ascii_lowercase())
        .unwrap_or_default();
    KINDS
        .iter()
        .find(|(_, exts)| exts.contains(&x.as_str()))
        .map_or("", |(k, _)| k)
}

impl Filters {
    /// The filters as Everything's search terms, which fsd reads too, so an index search is
    /// narrowed by them. Dates are left to `accepts`.
    fn terms(&self) -> String {
        let mut t = vec![];
        match self.kind.as_deref() {
            None | Some("") => {}
            Some("folder") => t.push("folder:".to_string()),
            Some(k) => match KINDS.iter().find(|(name, _)| *name == k) {
                Some((_, exts)) => t.push(format!("file: ext:{}", exts.join(";"))),
                // Nothing has an unknown kind; `accepts` turns everything down.
                None => {}
            },
        }
        let file_only = self.min_size.is_some()
            || self.max_size.is_some()
            || self.after.is_some()
            || self.before.is_some();
        if file_only && !t.iter().any(|x| x.starts_with("file:")) {
            t.push("file:".into());
        }
        if let Some(m) = self.min_size {
            t.push(format!("size:>={m}"));
        }
        if let Some(m) = self.max_size {
            t.push(format!("size:<{m}"));
        }
        t.join(" ")
    }

    fn accepts(&self, e: &device::Entry) -> bool {
        let file_only = self.min_size.is_some()
            || self.max_size.is_some()
            || self.after.is_some()
            || self.before.is_some();
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
pub(crate) fn glob(pattern: &[char], name: &[char]) -> bool {
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
pub(crate) async fn find(
    state: State<'_, AppState>,
    src: String,
    dir: String,
    query: String,
    hidden: bool,
    filters: Option<Filters>,
    regex: Option<bool>,
) -> Result<Matches, String> {
    let dev = state.source(Some(&src))?;
    let filters = filters.unwrap_or_default();
    let regex = regex.unwrap_or(false);
    let text = query.clone();
    let query = if regex {
        Query::regex(&query)?
    } else {
        Query::text(&query)
    };
    blocking(move || {
        // From the device's own index when it has one (fsd for the computer, an Everything
        // server), checked again here.
        let under = device::norm(&dir).map_err(msg)?;
        let shown = |e: &Entry| {
            hidden
                || !e
                    .path
                    .get(under.len()..)
                    .unwrap_or_default()
                    .split('/')
                    .any(|p| p.starts_with('.'))
        };
        let keep = |e: &Entry| shown(e) && query.fits(&e.name) && filters.accepts(e);
        let terms = filters.terms();
        let search = device::Search {
            text: &text,
            regex,
            hidden,
            terms: &terms,
            keep: &keep,
            max: 1000,
        };
        if let Some(found) = dev.search(&dir, &search) {
            let (entries, all) = found.map_err(msg)?;
            let entries: Vec<Entry> = entries.into_iter().filter(|e| keep(e)).collect();
            return Ok(Matches {
                entries,
                complete: all,
            });
        }
        Ok(find_in(
            &dev,
            &dir,
            &query,
            hidden,
            &filters,
            1000,
            std::time::Duration::from_secs(30),
        ))
    })
    .await
}

/// What `find_in` looks for in names.
pub(crate) enum Query {
    /// Contained in the name, any case.
    Has(String),
    /// A pattern with * and ?, lowercased.
    Glob(Vec<char>),
    Regex(regex::Regex),
}

impl Query {
    /// Plain text, or a pattern when it has * or ?.
    pub(crate) fn text(query: &str) -> Self {
        let q = query.trim().to_lowercase();
        if q.contains('*') || q.contains('?') {
            Query::Glob(q.chars().collect())
        } else {
            Query::Has(q)
        }
    }

    /// A regular expression, any case, or why it isn't one.
    pub(crate) fn regex(query: &str) -> Result<Self, String> {
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

pub(crate) fn find_in(
    dev: &Device,
    dir: &str,
    query: &Query,
    hidden: bool,
    filters: &Filters,
    max: usize,
    time: std::time::Duration,
) -> Matches {
    let fits = |name: &str| query.fits(name);
    let start = std::time::Instant::now();
    let mut out = vec![];
    let mut queue = std::collections::VecDeque::from([dir.to_string()]);
    while let Some(d) = queue.pop_front() {
        if out.len() >= max || start.elapsed() > time {
            return Matches {
                entries: out,
                complete: false,
            };
        }
        let Ok(mut items) = dev.listdir(&d) else {
            continue;
        };
        items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        for e in items {
            if !hidden && e.is_hidden() {
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
    Matches {
        entries: out,
        complete: true,
    }
}
