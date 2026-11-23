//! Searches of the computer's own files from fsd's index. fsd is the file-search daemon of the
//! fs project (usually run as a service); Ferry asks it over its local socket or named pipe,
//! with the `fs-protocol` crate, instead of walking every folder.
//!
//! Whenever fsd can't answer for certain, `search` gives None and the folders are walked as
//! before: fsd isn't running, speaks another protocol version, doesn't index the folder, or
//! hasn't finished its first crawl.

use crate::device::{Entry, Search};
use crate::platform::{app_path, native};
use fs_protocol::attrib;
use fs_protocol::ipc_client::IpcClient;
use fs_protocol::ipc_protocol::{IpcRequest, IpcResponse, SearchHitDto};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

/// Results asked for at a time. Filters fsd can't apply (dates, hidden folders) are applied
/// here, so it can take several pages to find enough.
const PAGE: u32 = 2000;
/// A search stops after this long with what it has. The computer's other work waits on it
/// (`Device` runs one thing at a time), so it's kept short.
const TIME: Duration = Duration::from_secs(5);
/// Hidden folders looked up to leave out what's in them.
const HIDDEN_DIRS: u32 = 20_000;

/// Up to `s.max` entries under `dir` (an app path) that `s.keep` accepts, and whether that's
/// all of them.
pub fn search(dir: &str, s: &Search) -> Option<(Vec<Entry>, bool)> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    rt.block_on(async {
        match tokio::time::timeout(TIME + Duration::from_secs(1), ask(dir, s)).await {
            Ok(Ok(found)) => found,
            Ok(Err(e)) => {
                eprintln!("fsd: {e}; walking the folders instead");
                None
            }
            Err(_) => None,
        }
    })
}

async fn ask(dir: &str, s: &Search<'_>) -> Result<Option<(Vec<Entry>, bool)>, String> {
    let mut fsd = match IpcClient::connect(&fs_protocol::default_ipc_path()).await {
        Ok(c) => c,
        // Not running: the usual case, nothing to say.
        Err(_) => return Ok(None),
    };
    let roots = match fsd.request(IpcRequest::Roots).await.map_err(|e| e.to_string())? {
        IpcResponse::Roots(r) => r,
        _ => return Err("fsd answered something else".into()),
    };
    let native_dir = native(dir).to_string_lossy().into_owned();
    if !roots.crawled || !roots.roots.iter().any(|r| covers(r, &native_dir)) {
        return Ok(None);
    }
    let (Some(scope), Some(name)) = (scope_term(&native_dir), name_term(s.text, s.regex)) else {
        return Ok(None);
    };
    let under = dir.trim_end_matches('/');
    let start = Instant::now();

    let hidden_dirs = if s.hidden {
        HashSet::new()
    } else {
        let q = format!("{scope} folder: attrib:H");
        let mut dirs = HashSet::new();
        for hit in page(&mut fsd, &q, 0, HIDDEN_DIRS).await?.0 {
            if let Some(p) = within(&hit, under) {
                dirs.insert(fold(&p));
            }
        }
        dirs
    };

    let query = [scope.as_str(), name.as_str(), s.terms].iter().filter(|t| !t.is_empty()).copied().collect::<Vec<_>>().join(" ");
    let mut out = vec![];
    let mut offset = 0u32;
    loop {
        let (hits, total) = page(&mut fsd, &query, offset, PAGE).await?;
        let got = hits.len() as u32;
        for hit in hits {
            let Some(path) = within(&hit, under) else { continue };
            let e = Entry { name: hit.name, hidden: hit.attrib_bitmask & attrib::HIDDEN != 0, path, dir: hit.is_dir, size: hit.size_bytes, mtime: hit.mtime };
            if !s.hidden && (e.hidden || in_hidden(&e.path, under, &hidden_dirs)) {
                continue;
            }
            if (s.keep)(&e) {
                out.push(e);
                if out.len() >= s.max {
                    return Ok(Some((nearest_first(out), false)));
                }
            }
        }
        offset += got;
        if got < PAGE || u64::from(offset) >= total {
            return Ok(Some((nearest_first(out), true)));
        }
        if start.elapsed() > TIME {
            return Ok(Some((nearest_first(out), false)));
        }
    }
}

async fn page(fsd: &mut IpcClient, query: &str, offset: u32, limit: u32) -> Result<(Vec<SearchHitDto>, u64), String> {
    match fsd.request(IpcRequest::Search { query: query.to_string(), limit, offset }).await.map_err(|e| e.to_string())? {
        IpcResponse::Search(r) => Ok((r.hits, r.total_hits)),
        _ => Err("fsd answered something else".into()),
    }
}

/// Whether paths differ only in case here: they don't on Linux.
const ANY_CASE: bool = cfg!(any(windows, target_os = "macos"));

fn fold(p: &str) -> String {
    if ANY_CASE {
        p.to_lowercase()
    } else {
        p.to_string()
    }
}

/// Whether `rest` starts with `prefix`, in any case where the file system ignores it.
fn starts_with(rest: &str, prefix: &str) -> bool {
    rest.len() >= prefix.len()
        && rest.is_char_boundary(prefix.len())
        && if ANY_CASE { rest[..prefix.len()].eq_ignore_ascii_case(prefix) } else { rest.starts_with(prefix) }
}

/// Whether fsd's watch path `root` holds `dir` (both the computer's paths).
fn covers(root: &str, dir: &str) -> bool {
    let sep = std::path::MAIN_SEPARATOR;
    let root = root.trim_end_matches(sep);
    let dir = dir.trim_end_matches(sep);
    starts_with(dir, root) && (dir.len() == root.len() || dir[root.len()..].starts_with(sep))
}

/// The app path of what fsd found, spelled from `under` as Ferry has it, when it's in `under`
/// and is a file or folder (not something inside a zip).
fn within(hit: &SearchHitDto, under: &str) -> Option<String> {
    if hit.archive_path.is_some() {
        return None;
    }
    let path = app_path(Path::new(&hit.path));
    let below = format!("{under}/");
    if !starts_with(&path, &below) {
        return None;
    }
    Some(format!("{below}{}", &path[below.len()..]))
}

/// Whether a folder between `under` and `path` is one of `hidden` (folded app paths).
fn in_hidden(path: &str, under: &str, hidden: &HashSet<String>) -> bool {
    if hidden.is_empty() {
        return false;
    }
    let folded = fold(path);
    folded.char_indices().skip(under.len() + 1).filter(|(_, c)| *c == '/').any(|(i, _)| hidden.contains(&folded[..i]))
}

/// The query term that keeps fsd's search under `dir` (the computer's path); fsd's results are
/// checked against `dir` again anyway. None when it can't be written.
fn scope_term(dir: &str) -> Option<String> {
    let sep = std::path::MAIN_SEPARATOR;
    let dir = dir.trim_end_matches(sep);
    if dir.is_empty() {
        // The top of the file system: everything fsd has.
        return Some(String::new());
    }
    if dir.contains('"') {
        return None;
    }
    let with_sep = format!("{dir}{sep}");
    // A Windows path as one word ("C:\Users\me\") is a path prefix fsd looks up quickly. A word
    // ends at a space or |<>, and `#..:` or `quot:` in it would be read as escapes.
    let plain = cfg!(windows) && !with_sep.contains(|c: char| c.is_whitespace() || "|<>#".contains(c)) && with_sep.matches(':').count() == 1;
    // In quotes, a path anywhere in the path.
    Some(if plain { with_sep } else { format!("\"{with_sep}\"") })
}

/// The query term for the name: in any case contained, a pattern with * and ?, or a regular
/// expression. None when it can't be written.
fn name_term(text: &str, regex: bool) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return Some(String::new());
    }
    if text.contains('"') {
        return None;
    }
    Some(if regex { format!("regex:\"(?i){text}\"") } else { format!("\"{text}\"") })
}

/// The way the walk gives them: nearest folders first, then by name.
fn nearest_first(mut out: Vec<Entry>) -> Vec<Entry> {
    out.sort_by_cached_key(|e| (e.path.matches('/').count(), e.path.to_lowercase()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, archive: bool) -> SearchHitDto {
        SearchHitDto {
            path: path.into(),
            name: path.rsplit(['/', '\\']).next().unwrap_or_default().into(),
            extension: None,
            size_bytes: 1,
            mtime: 0,
            ctime: 0,
            is_dir: false,
            attrib_bitmask: 0,
            archive_path: archive.then(|| "x.zip".into()),
            snippet: None,
        }
    }

    #[test]
    fn names_are_quoted_and_regexes_ignore_case() {
        assert_eq!(name_term("  ", false).unwrap(), "");
        assert_eq!(name_term("my notes", false).unwrap(), "\"my notes\"");
        assert_eq!(name_term("*.md", false).unwrap(), "\"*.md\"");
        assert_eq!(name_term(r"^a\d", true).unwrap(), "regex:\"(?i)^a\\d\"");
        assert!(name_term("say \"hi\"", false).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_scopes() {
        assert_eq!(scope_term(r"C:\Users\me").unwrap(), r"C:\Users\me\");
        assert_eq!(scope_term(r"C:\").unwrap(), r"C:\");
        assert_eq!(scope_term(r"C:\Program Files").unwrap(), "\"C:\\Program Files\\\"");
        assert_eq!(scope_term(r"C:\a#1:b").unwrap(), "\"C:\\a#1:b\\\"");
        assert!(covers(r"C:\Users", r"c:\users\me"));
        assert!(covers(r"C:\", r"C:\Users"));
        assert!(covers(r"C:\Users\", r"C:\Users"));
        assert!(!covers(r"C:\Users", r"C:\UsersX"));
        assert!(!covers(r"C:\Users\me", r"C:\Users"));
    }

    #[cfg(windows)]
    #[test]
    fn hits_are_spelled_from_the_folder_searched() {
        assert_eq!(within(&hit(r"C:\Users\Me\a.txt", false), "/C:/users/me").unwrap(), "/C:/users/me/a.txt");
        assert!(within(&hit(r"C:\Users\Me\a.zip!/b.txt", true), "/C:/Users/Me").is_none());
        assert!(within(&hit(r"C:\Users\Mex\a.txt", false), "/C:/Users/Me").is_none());
        assert!(within(&hit(r"C:\Users\Me", false), "/C:/Users/Me").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_scopes() {
        assert_eq!(scope_term("/").unwrap(), "");
        assert_eq!(scope_term("/home/me").unwrap(), "\"/home/me/\"");
        assert!(covers("/home", "/home/me"));
        assert!(!covers("/home/me", "/home"));
        assert_eq!(within(&hit("/home/me/a.txt", false), "/home/me").unwrap(), "/home/me/a.txt");
        assert!(within(&hit("/mnt/home/me/a.txt", false), "/home/me").is_none());
    }

    #[test]
    fn whats_in_hidden_folders_is_left_out() {
        let hidden: HashSet<String> = [fold("/r/AppData")].into();
        assert!(in_hidden("/r/AppData/x.txt", "/r", &hidden));
        assert!(in_hidden("/r/AppData/a/x.txt", "/r", &hidden));
        assert!(!in_hidden("/r/AppDataX/x.txt", "/r", &hidden));
        // The folder itself has its own hidden mark.
        assert!(!in_hidden("/r/AppData", "/r", &hidden));
        assert!(!in_hidden("/r/x.txt", "/r", &HashSet::new()));
    }

    /// Against a running fsd that indexes the folder:
    /// FERRY_FSD_DIR=<the computer's path> [FERRY_FSD_TEXT=..] [FERRY_FSD_TERMS=..] [FERRY_FSD_HIDDEN=1]
    /// cargo test --lib fsd::tests::live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live() {
        let dir = app_path(Path::new(&std::env::var("FERRY_FSD_DIR").expect("FERRY_FSD_DIR")));
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let (text, terms) = (var("FERRY_FSD_TEXT"), var("FERRY_FSD_TERMS"));
        let keep = |_: &Entry| true;
        let s = Search { text: &text, regex: false, hidden: !var("FERRY_FSD_HIDDEN").is_empty(), terms: &terms, keep: &keep, max: 1000 };
        let (found, all) = search(&dir, &s).expect("fsd didn't answer");
        for e in &found {
            println!("found {}{} {}", e.path, if e.dir { "/" } else { "" }, e.size);
        }
        println!("complete {all}");
    }

    #[test]
    fn nearest_come_first() {
        let e = |p: &str| Entry { name: String::new(), path: p.into(), dir: false, size: 0, mtime: 0, hidden: false };
        let sorted = nearest_first(vec![e("/a/b/c"), e("/a/z"), e("/a/B")]);
        assert_eq!(sorted.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["/a/B", "/a/z", "/a/b/c"]);
    }
}
