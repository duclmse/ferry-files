//! Finding the computer's files by name from fsd's index (the palette's `:`), and what fsd says
//! about its index. fsd is the file-search daemon of the fs project (usually run as a service);
//! Ferry asks it over its local socket or named pipe with the fs-protocol crate's client. Built
//! with the `fsd` feature.

use crate::device::Entry;
use crate::platform::app_path;
use fs_protocol::ipc_client::{IpcClient, IpcClientError};
use fs_protocol::ipc_protocol::{
    ConfigChange, DecodeError, IpcRequest as Request, IpcResponse as Response,
    SearchHitDto as SearchHit, PROTOCOL_VERSION,
};
use std::path::Path;
use std::time::Duration;

/// How long fsd has to answer a search.
const TIME: Duration = Duration::from_secs(5);

/// A connection to fsd at the path it listens on unless its config says otherwise, whose
/// errors are Ferry's messages.
struct Client(IpcClient);

impl Client {
    async fn connect() -> Result<Client, IpcClientError> {
        IpcClient::connect(&fs_protocol::default_ipc_path())
            .await
            .map(Client)
    }

    async fn request(&mut self, request: Request) -> Result<Response, String> {
        self.0.request(request).await.map_err(said)
    }
}

/// `e` in plain words. fsd's own errors read "fsd: {message} ({code})", which
/// `plain_query_error` looks for.
fn said(e: IpcClientError) -> String {
    match e {
        IpcClientError::DaemonError { code, message } => format!("fsd: {message} ({code})"),
        IpcClientError::Decode(DecodeError::VersionMismatch(v)) => format!(
            "fsd speaks protocol version {v} and Ferry Files version {PROTOCOL_VERSION}; update the older one"
        ),
        IpcClientError::Decode(DecodeError::BadMagic(_)) => {
            "Something other than fsd answered on fsd's socket.".into()
        }
        IpcClientError::Decode(e) => format!("couldn't read fsd's answer: {e}"),
        IpcClientError::FrameTooLarge(n) => format!("fsd's answer is too big ({n} bytes)"),
        e => format!("talking to fsd: {e}"),
    }
}

/// Up to `max` files and folders anywhere fsd indexes that fit `text`, in fsd's order, from
/// fsd's `offset`th on: the palette's `:`. `text` is fsd's search syntax, as typed: words the name contains in any case,
/// * and ? patterns, and fsd's modifiers and functions (`case:`, `ww:`, `regex:`, `ext:`,
/// `size:`, `dm:`, `path:`, `|`, `!`, `< >` …). Err says why there's nothing: fsd isn't
/// running, hasn't finished indexing, or can't read the search.
pub fn find(text: &str, offset: u32, max: u32) -> Result<Found, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        match tokio::time::timeout(TIME, find_in(text, offset, max)).await {
            Ok(r) => r,
            Err(_) => Err("fsd took too long to answer.".into()),
        }
    })
}

/// A page of what fsd found.
pub struct Found {
    pub entries: Vec<Entry>,
    /// The offset to ask for the next page with; None when this was the last.
    pub next: Option<u32>,
}

async fn find_in(text: &str, offset: u32, max: u32) -> Result<Found, String> {
    let mut fsd = Client::connect().await.map_err(|_| {
        "fsd isn't running. Install in Settings > Search starts it again.".to_string()
    })?;
    match fsd.request(Request::Roots).await? {
        Response::Roots(r) if !r.crawled => {
            return Err("fsd is still indexing. Try again in a while.".into())
        }
        Response::Roots(_) => {}
        _ => return Err("fsd answered something else.".into()),
    }
    let query = text.trim();
    if query.is_empty() {
        return Ok(Found {
            entries: vec![],
            next: None,
        });
    }
    let (hits, _) = page(&mut fsd, query, offset, max)
        .await
        .map_err(plain_query_error)?;
    // fsd's total counts only the rows it read for this page, so a full page may have more
    // after it. Counted before files inside archives are dropped, as fsd's offsets count them.
    let next = (hits.len() as u32 >= max).then(|| offset + hits.len() as u32);
    let entries = hits
        .into_iter()
        .filter(|h| h.archive_path.is_none())
        .map(|h| Entry {
            path: app_path(Path::new(&h.path)),
            name: h.name,
            hidden: h.attrib_bitmask & fs_protocol::attrib::HIDDEN != 0,
            dir: h.is_dir,
            size: h.size_bytes,
            mtime: h.mtime,
        })
        .collect();
    Ok(Found { entries, next })
}

/// How far fsd's index is, for Settings > Search.
pub struct Index {
    /// False until fsd has crawled its folders once.
    pub crawled: bool,
    /// The folders it indexes, app paths.
    pub roots: Vec<String>,
    /// Files and folders in the index.
    pub entries: u64,
    /// Whether fsd can look inside the folders the system keeps it out of until it's let in
    /// (`platform::protected_dirs`): true when it has found something in one, or indexes none
    /// of them; false when it has crawled them and found nothing; None while it's still
    /// crawling and hasn't found anything yet.
    pub sees_protected: Option<bool>,
}

/// What fsd says about its index; None when it isn't running.
pub fn index() -> Result<Option<Index>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let ask = async {
            let Ok(mut fsd) = Client::connect().await else {
                return Ok(None);
            };
            let r = match fsd.request(Request::Roots).await? {
                Response::Roots(r) => r,
                _ => return Err("fsd answered something else.".to_string()),
            };
            let seen = sees_inside(&mut fsd, &r.roots).await?;
            Ok(Some(Index {
                crawled: r.crawled,
                roots: r.roots.iter().map(|p| app_path(Path::new(p))).collect(),
                entries: r.status.iter().map(|s| s.entries).sum(),
                sees_protected: if seen || r.crawled { Some(seen) } else { None },
            }))
        };
        match tokio::time::timeout(Duration::from_secs(3), ask).await {
            Ok(r) => r,
            Err(_) => Err("fsd took too long to answer.".into()),
        }
    })
}

/// Whether fsd has anything inside one of the protected folders it indexes (under `roots`),
/// or indexes none of them.
async fn sees_inside(fsd: &mut Client, roots: &[String]) -> Result<bool, String> {
    let sep = std::path::MAIN_SEPARATOR;
    let mut covered = 0;
    for dir in crate::platform::protected_dirs() {
        let dir = dir.to_string_lossy().into_owned();
        if !roots.iter().any(|r| covers(r, &dir)) {
            continue;
        }
        covered += 1;
        let Some(scope) = scope_term(&dir) else {
            continue;
        };
        let inside = format!("{dir}{sep}");
        // The term matches the path anywhere in a path, so look for one that starts with it.
        let (hits, _) = page(fsd, &scope, 0, 20).await?;
        if hits.iter().any(|h| starts_with(&h.path, &inside)) {
            return Ok(true);
        }
    }
    Ok(covered == 0)
}

/// Asks a running fsd to follow its settings file as it is now. None when it isn't running;
/// otherwise the settings it takes only when it restarts.
pub fn reload() -> Result<Option<Vec<String>>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let ask = async {
            let Ok(mut fsd) = Client::connect().await else {
                return Ok(None);
            };
            let change = ConfigChange::Reload;
            match fsd.request(Request::ConfigChange { change }).await? {
                Response::ConfigApplied(a) => Ok(Some(a.restart)),
                _ => Err("fsd answered something else.".to_string()),
            }
        };
        match tokio::time::timeout(Duration::from_secs(5), ask).await {
            Ok(r) => r,
            Err(_) => Err("fsd took too long to answer.".into()),
        }
    })
}

async fn page(
    fsd: &mut Client,
    query: &str,
    offset: u32,
    limit: u32,
) -> Result<(Vec<SearchHit>, u64), String> {
    match fsd
        .request(Request::Search {
            query: query.to_string(),
            limit,
            offset,
        })
        .await?
    {
        Response::Search(r) => Ok((r.hits, r.total_hits)),
        _ => Err("fsd answered something else".into()),
    }
}

/// Whether paths differ only in case here: they don't on Linux.
const ANY_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// Whether `rest` starts with `prefix`, in any case where the file system ignores it.
fn starts_with(rest: &str, prefix: &str) -> bool {
    rest.len() >= prefix.len()
        && rest.is_char_boundary(prefix.len())
        && if ANY_CASE {
            rest[..prefix.len()].eq_ignore_ascii_case(prefix)
        } else {
            rest.starts_with(prefix)
        }
}

/// Whether fsd's watch path `root` holds `dir` (both the computer's paths).
fn covers(root: &str, dir: &str) -> bool {
    let sep = std::path::MAIN_SEPARATOR;
    let root = root.trim_end_matches(sep);
    let dir = dir.trim_end_matches(sep);
    starts_with(dir, root) && (dir.len() == root.len() || dir[root.len()..].starts_with(sep))
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
    let plain = cfg!(windows)
        && !with_sep.contains(|c: char| c.is_whitespace() || "|<>#".contains(c))
        && with_sep.matches(':').count() == 1;
    // In quotes, a path anywhere in the path.
    Some(if plain {
        with_sep
    } else {
        format!("\"{with_sep}\"")
    })
}

/// fsd's answer to a search it can't read (a bad regex, a modifier without a value) as the
/// palette shows it; other errors as they are.
fn plain_query_error(e: String) -> String {
    e.strip_prefix("fsd: ")
        .and_then(|m| {
            m.strip_suffix(" (PARSE_ERROR)")
                .or_else(|| m.strip_suffix(" (PLAN_ERROR)"))
        })
        .map_or(e.clone(), |m| format!("That search doesn't work: {m}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_version_is_named() {
        let e = said(IpcClientError::Decode(DecodeError::VersionMismatch(10)));
        assert!(e.contains("version 10"), "{e}");
        let e = said(IpcClientError::DaemonError {
            code: "PARSE_ERROR".into(),
            message: "bad".into(),
        });
        assert_eq!(plain_query_error(e), "That search doesn't work: bad");
    }

    #[test]
    fn bad_searches_say_so() {
        assert_eq!(
            plain_query_error("fsd: invalid regex '(': unclosed group (PARSE_ERROR)".into()),
            "That search doesn't work: invalid regex '(': unclosed group"
        );
        assert_eq!(
            plain_query_error("talking to fsd: gone".into()),
            "talking to fsd: gone"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_scopes() {
        assert_eq!(scope_term(r"C:\Users\me").unwrap(), r"C:\Users\me\");
        assert_eq!(scope_term(r"C:\").unwrap(), r"C:\");
        assert_eq!(
            scope_term(r"C:\Program Files").unwrap(),
            "\"C:\\Program Files\\\""
        );
        assert_eq!(scope_term(r"C:\a#1:b").unwrap(), "\"C:\\a#1:b\\\"");
        assert!(covers(r"C:\Users", r"c:\users\me"));
        assert!(covers(r"C:\", r"C:\Users"));
        assert!(covers(r"C:\Users\", r"C:\Users"));
        assert!(!covers(r"C:\Users", r"C:\UsersX"));
        assert!(!covers(r"C:\Users\me", r"C:\Users"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_scopes() {
        assert_eq!(scope_term("/").unwrap(), "");
        assert_eq!(scope_term("/home/me").unwrap(), "\"/home/me/\"");
        assert!(covers("/home", "/home/me"));
        assert!(!covers("/home/me", "/home"));
    }

    /// The palette's `:` against a running fsd:
    /// FERRY_FSD_TEXT=<part of a name> cargo test --lib fsd::tests::find_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn find_live() {
        match find(
            &std::env::var("FERRY_FSD_TEXT").unwrap_or_else(|_| "readme".into()),
            0,
            20,
        ) {
            Ok(found) => found
                .entries
                .iter()
                .for_each(|e| println!("found {}{}", e.path, if e.dir { "/" } else { "" })),
            Err(e) => println!("no: {e}"),
        }
    }
}
