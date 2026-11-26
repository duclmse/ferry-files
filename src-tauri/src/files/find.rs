//! Finding files: by name in fsd's index (the palette's `:`), or by walking a folder (the books
//! library).

use super::*;

/// A page of what fsd found for the palette's `:`.
#[derive(Serialize)]
pub(crate) struct FoundFiles {
    pub(crate) entries: Vec<Entry>,
    /// The offset to ask for more with; None when there are no more.
    pub(crate) next: Option<u32>,
}

/// Files and folders on the computer that fit `text` (fsd's search syntax), from fsd's index
/// (the palette's `:`): up to `max` (100 when not given) from fsd's `offset`th on, in fsd's
/// order. Err says why not (fsd isn't running, still indexing, can't read the search, or this
/// build has no fsd).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn find_files(
    text: String,
    offset: Option<u32>,
    max: Option<u32>,
) -> Result<FoundFiles, String> {
    #[cfg(feature = "fsd")]
    {
        let (offset, max) = (offset.unwrap_or(0), max.unwrap_or(100).clamp(1, 1000));
        blocking(move || {
            crate::fsd::find(&text, offset, max).map(|f| FoundFiles {
                entries: f.entries,
                next: f.next,
            })
        })
        .await
    }
    #[cfg(not(feature = "fsd"))]
    {
        let _ = (text, offset, max);
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

/// What `find_in` looks for in names.
pub(crate) enum Query {
    /// Contained in the name, any case.
    Has(String),
    /// A pattern with * and ?, lowercased.
    Glob(Vec<char>),
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

    fn fits(&self, name: &str) -> bool {
        match self {
            Query::Has(q) => name.to_lowercase().contains(q),
            Query::Glob(p) => glob(p, &name.to_lowercase().chars().collect::<Vec<_>>()),
        }
    }
}

pub(crate) fn find_in(
    dev: &Device,
    dir: &str,
    query: &Query,
    hidden: bool,
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
            if out.len() < max && fits(&e.name) {
                out.push(e);
            }
        }
    }
    Matches {
        entries: out,
        complete: true,
    }
}
