//! fsd on another computer, as a read-only drive. fsd's REST API (`api.rest_port`, 7700 unless
//! changed) lists folders and serves files at `/fs/read`, gives Windows' drives at `/fs/drives`
//! and the folders it indexes at `/watch_paths`, and `/search` searches its index, so the filter
//! box searches below the folder as on an Everything server.
//!
//! fsd's REST API answers only on its own computer unless `api.rest_bind` says otherwise. With
//! `api.auth_mode = "all"` it wants its token on every request; the token is kept in the
//! keychain like a password. Files are only read: fsd's routes that change files aren't used, so
//! nothing on the other computer changes without the backups `Device` keeps for every write.
//!
//! Paths are the app's: "/home/me" from a Mac or Linux, "/C:/Users/me" from Windows, whose
//! drives are in "/".

use crate::device::{basename, dirname, err, Result, Transport};
use crate::devices::connect;
use crate::servers::web;
use crate::*;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use std::io::Read;
use std::time::{Duration, Instant};

/// "http://host:7700" from what was typed: "pc", "pc:8080" or a full address.
fn base_url(server: &str) -> std::result::Result<String, String> {
    let s = server.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("Type the address of a computer running fsd.".into());
    }
    let (scheme, rest) = match s.split_once("://") {
        Some((sc, rest)) if sc == "http" || sc == "https" => (sc, rest),
        Some(_) => return Err("fsd's address starts with http:// or https://.".into()),
        None => ("http", s),
    };
    // No port: fsd's own.
    let host_port = rest.split('/').next().unwrap_or(rest);
    let has_port = host_port
        .rsplit_once(':')
        .is_some_and(|(h, p)| !h.ends_with(':') && p.parse::<u16>().is_ok());
    Ok(if has_port {
        format!("{scheme}://{rest}")
    } else {
        format!("{scheme}://{rest}:{DEFAULT_PORT}")
    })
}

/// The port fsd's REST API listens on unless its `api.rest_port` says otherwise.
const DEFAULT_PORT: u16 = 7700;

/// The app's "/C:/Users/me" as Windows' "C:\Users\me" (a drive as "C:\"); other paths as they are.
fn native(path: &str) -> String {
    let p = path.trim_start_matches('/');
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'/')
    {
        let w = p.replace('/', "\\");
        if w.len() == 2 {
            format!("{w}\\")
        } else {
            w
        }
    } else {
        path.to_string()
    }
}

/// fsd's path ("C:\Users\me" or "/home/me") as the app's ("/C:/Users/me" or "/home/me").
fn app_path_of(native: &str) -> String {
    let b = native.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let p = native.replace('\\', "/");
        let p = p.trim_end_matches('/');
        format!("/{p}")
    } else if native.len() > 1 {
        native.trim_end_matches('/').to_string()
    } else {
        native.to_string()
    }
}

/// Whether the app's `path` is a Windows drive ("/C:").
fn is_drive(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() == 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':'
}

/// An item of a folder, as `/fs/read` lists it.
#[derive(Deserialize)]
struct Listed {
    name: String,
    is_dir: bool,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mtime_secs: i64,
}

/// A drive, as `/fs/drives` gives it (only on Windows).
#[derive(Deserialize)]
struct Drive {
    letter: String,
}

/// What `/watch_paths` says.
#[derive(Deserialize)]
struct Roots {
    watch_paths: Vec<String>,
}

/// A name `/search` found.
#[derive(Deserialize)]
struct Hit {
    path: String,
    name: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    mtime: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    archive_path: Option<String>,
}

#[derive(Deserialize)]
struct Searched {
    result: Vec<Hit>,
}

fn entry(name: String, path: String, dir: bool, size: u64, mtime: i64) -> Entry {
    Entry {
        hidden: name.starts_with('.'),
        name,
        path,
        dir,
        size: if dir { 0 } else { size },
        mtime,
    }
}

fn agent() -> ureq::Agent {
    web::config()
        .timeout_connect(Some(Duration::from_secs(3)))
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// A query value, encoded.
fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// Most names one search gives.
const FIND_MAX: usize = 1000;
/// How long a folder listing is reused for its pages and for looking up the files in it.
const KEEP: Duration = Duration::from_secs(10);

pub struct FsdRemote {
    base: String,
    /// "Bearer …", when fsd wants its token.
    auth: Option<String>,
    /// The places shown under it in the sidebar: Windows' drives, then the folders fsd indexes.
    places: Mutex<Vec<(String, String)>>,
    /// Whether it runs on Windows: it has drives.
    windows: bool,
    /// The last folder listed, for its pages and `stat` of the files in it.
    last: Mutex<Option<(String, Vec<Entry>, Instant)>>,
}

impl FsdRemote {
    /// Connects to the fsd at `server`: it must answer. With `token`, sends it with every request.
    pub fn open(server: &str, token: Option<&str>) -> std::result::Result<Self, String> {
        let base = base_url(server)?;
        let mut fsd = FsdRemote {
            base,
            auth: token
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| format!("Bearer {t}")),
            places: Mutex::new(vec![]),
            windows: false,
            last: Mutex::new(None),
        };
        let places = fsd.find_places()?;
        fsd.windows = places.iter().any(|(_, p)| is_drive(p));
        *fsd.places.lock().unwrap_or_else(|e| e.into_inner()) = places;
        Ok(fsd)
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// What went wrong talking to fsd, in words that say what to do.
    fn failed(&self, e: web::Error) -> String {
        let base = &self.base;
        match e {
            web::Error::Status(401, _) if self.auth.is_some() => {
                format!("fsd at {base} didn't take the token. Check it in Settings > Connections.")
            }
            web::Error::Status(401, _) => format!(
                "fsd at {base} asks for its token. Add it in Settings > Connections."
            ),
            web::Error::Status(404, _) => {
                format!("fsd at {base} doesn't have that, or it was built without its REST API.")
            }
            web::Error::Status(code, res) => {
                let why = web::text(res)
                    .ok()
                    .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                    .and_then(|v| v["error"].as_str().map(str::to_string));
                match why {
                    Some(why) => format!("fsd at {base} answered with error {code}: {why}."),
                    None => format!("fsd at {base} answered with error {code}."),
                }
            }
            web::Error::Transport(_) => format!(
                "Can't reach fsd at {base}. Its REST API answers other computers only when api.rest_bind lets it."
            ),
        }
    }

    fn get(
        &self,
        route: &str,
        range: Option<(u64, u64)>,
    ) -> std::result::Result<web::Response, web::Error> {
        let mut req = agent().get(&format!("{}{route}", self.base));
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a);
        }
        if let Some((start, len)) = range {
            req = req.header("Range", format!("bytes={start}-{}", start + len.max(1) - 1));
        }
        web::checked(req.call())
    }

    fn json<T: serde::de::DeserializeOwned>(&self, route: &str) -> std::result::Result<T, String> {
        let res = self.get(route, None).map_err(|e| self.failed(e))?;
        let body = web::text(res).map_err(|e| format!("Couldn't read fsd's answer: {e}"))?;
        serde_json::from_str(&body).map_err(|_| {
            format!(
                "fsd at {} answered something else. Is the address fsd's REST API?",
                self.base
            )
        })
    }

    fn file_route(path: &str) -> String {
        format!("/fs/read?path={}", enc(&native(path)))
    }

    fn find_places(&self) -> std::result::Result<Vec<(String, String)>, String> {
        let drives: Vec<Drive> = self.json("/fs/drives")?;
        let roots: Roots = self.json("/watch_paths")?;
        let mut places: Vec<(String, String)> = drives
            .into_iter()
            .map(|d| {
                let letter = d.letter.trim_end_matches(['\\', '/']).to_string();
                (letter.clone(), format!("/{letter}"))
            })
            .collect();
        places.sort();
        for r in roots.watch_paths {
            let path = app_path_of(&r);
            if !places.iter().any(|(_, p)| *p == path) {
                places.push((basename(&path).to_string(), path));
            }
        }
        Ok(places)
    }

    fn places(&self) -> Vec<(String, String)> {
        self.places
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The folder's items: kept for a moment, so its pages and lookups don't list it again.
    fn list(&self, path: &str, again: bool) -> Result<Vec<Entry>> {
        if !again {
            let kept = self
                .last
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .filter(|(p, _, at)| p == path && at.elapsed() < KEEP)
                .map(|(_, es, _)| es.clone());
            if let Some(es) = kept {
                return Ok(es);
            }
        }
        let entries: Vec<Entry> = if path == "/" && self.windows {
            self.places()
                .into_iter()
                .filter(|(_, p)| is_drive(p))
                .map(|(name, path)| entry(name, path, true, 0, 0))
                .collect()
        } else {
            let listed: Vec<Listed> = self.json(&Self::file_route(path)).map_err(DevError::Msg)?;
            let dir = path.trim_end_matches('/');
            listed
                .into_iter()
                .map(|l| {
                    let p = format!("{dir}/{}", l.name);
                    entry(l.name, p, l.is_dir, l.size.unwrap_or(0), l.mtime_secs)
                })
                .collect()
        };
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((path.to_string(), entries.clone(), Instant::now()));
        Ok(entries)
    }

    fn open_file(&self, path: &str, range: Option<(u64, u64)>) -> Result<web::Response> {
        self.get(&Self::file_route(path), range)
            .map_err(|e| DevError::Msg(self.failed(e)))
    }

    /// Names under `dir` (any depth) that fsd's index has for `text`: up to `max`, and whether
    /// that's all of them.
    fn find(
        &self,
        dir: &str,
        text: &str,
        regex: bool,
        case: bool,
        max: usize,
    ) -> Result<(Vec<Entry>, bool)> {
        let text = text.trim().replace('"', "");
        let case = if case { "case: " } else { "" };
        let term = if regex {
            format!("{case}regex: \"{text}\"")
        } else {
            format!("{case}\"{text}\"")
        };
        let under = if dir == "/" {
            "/".to_string()
        } else {
            format!("{}/", dir.trim_end_matches('/'))
        };
        // The folder in quotes: a path anywhere in the path, checked again below.
        let query = if dir == "/" {
            term
        } else {
            let sep = if self.windows { '\\' } else { '/' };
            format!("\"{}{sep}\" {term}", native(dir).trim_end_matches(sep))
        };
        let route = format!("/search?q={}&limit={max}", enc(&query));
        let found: Searched = self.json(&route).map_err(DevError::Msg)?;
        let all = found.result.len() < max;
        Ok((
            found
                .result
                .into_iter()
                .filter(|h| h.archive_path.is_none())
                .map(|h| {
                    let path = app_path_of(&h.path);
                    entry(h.name, path, h.kind == "dir", h.size_bytes, h.mtime)
                })
                .filter(|e| e.path.starts_with(&under))
                .collect(),
            all,
        ))
    }
}

impl Transport for FsdRemote {
    fn kind(&self) -> &'static str {
        "fsd"
    }

    fn label(&self) -> String {
        self.base
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_end_matches(&format!(":{DEFAULT_PORT}"))
            .to_string()
    }

    fn default_dir(&self) -> String {
        self.places()
            .into_iter()
            .find(|(_, p)| !is_drive(p))
            .or_else(|| self.places().into_iter().next())
            .map(|(_, p)| p)
            .unwrap_or_else(|| "/".into())
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        self.places()
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        self.list(path, true)
    }

    fn listdir_page(
        &self,
        path: &str,
        offset: u64,
        count: u64,
        sort: (&str, bool),
    ) -> Option<Result<(Vec<Entry>, u64)>> {
        // fsd lists a whole folder at once: the first page lists it, the others reuse it.
        let mut all = match self.list(path, offset == 0) {
            Ok(es) => es,
            Err(e) => return Some(Err(e)),
        };
        let by_name = |a: &Entry, b: &Entry| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        };
        match sort.0 {
            "size" => all.sort_by(|a, b| {
                b.dir
                    .cmp(&a.dir)
                    .then(a.size.cmp(&b.size))
                    .then_with(|| by_name(a, b))
            }),
            "date" => all.sort_by(|a, b| {
                b.dir
                    .cmp(&a.dir)
                    .then(a.mtime.cmp(&b.mtime))
                    .then_with(|| by_name(a, b))
            }),
            _ => all.sort_by(by_name),
        }
        if !sort.1 {
            // Folders stay first.
            let split = all.iter().position(|e| !e.dir).unwrap_or(all.len());
            all[..split].reverse();
            all[split..].reverse();
        }
        let total = all.len() as u64;
        Some(Ok((
            all.into_iter()
                .skip(offset as usize)
                .take(count as usize)
                .collect(),
            total,
        )))
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        if path == "/" {
            return Ok(Some(entry(String::new(), "/".into(), true, 0, 0)));
        }
        let parent = dirname(path);
        let name = basename(path);
        let found = self.list(&parent, false)?.into_iter().find(|e| {
            if self.windows {
                e.name.eq_ignore_ascii_case(name)
            } else {
                e.name == name
            }
        });
        Ok(found)
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let res = self.open_file(path, None)?;
        let mut data = vec![];
        web::reader(res)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path} from fsd: {e}")))?;
        let version = self
            .version(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("{}:0", data.len()));
        Ok((data, version))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let res = self.open_file(path, Some((start, len)))?;
        // fsd answers a range with 206; a 200 is the whole file, read up to `start` and dropped.
        let skip = if res.status() == 206 { 0 } else { start };
        let mut reader = web::reader(res);
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink())
            .map_err(|e| DevError::Msg(e.to_string()))?;
        let mut data = vec![];
        reader
            .take(len)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path} from fsd: {e}")))?;
        Ok(data)
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .stat(path)?
            .filter(|e| !e.dir)
            .map(|e| format!("{}:{}", e.size, e.mtime)))
    }

    /// Streamed into the file a piece at a time.
    fn read_to(
        &self,
        path: &str,
        to: &std::path::Path,
        read: &mut dyn FnMut(u64),
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<String> {
        let mut r = web::reader(self.open_file(path, None)?);
        let total = crate::device::pour(&mut r, to, &format!("{path} from fsd"), read, stop)?;
        Ok(self
            .version(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("{total}:0")))
    }

    fn replace(&self, _: &str, _: &[u8], _: bool) -> Result<String> {
        read_only()
    }

    fn delete(&self, _: &str, _: bool) -> Result<()> {
        read_only()
    }

    fn make_dir(&self, _: &str) -> Result<()> {
        read_only()
    }

    fn rename(&self, _: &str, _: &str) -> Result<()> {
        read_only()
    }

    fn refresh(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Ok(p) = self.find_places() {
            *self.places.lock().unwrap_or_else(|e| e.into_inner()) = p;
        }
    }

    fn search(
        &self,
        dir: &str,
        text: &str,
        regex: bool,
        case: bool,
        max: usize,
    ) -> Option<Result<(Vec<Entry>, bool)>> {
        Some(self.find(dir, text, regex, case, max.min(FIND_MAX)))
    }
}

fn read_only<T>() -> Result<T> {
    err("Files on a computer reached through fsd can only be read. Copy them to this computer to change them.")
}

/// Connects the fsd at `server` as a drive, with the token kept in the keychain as `secret`.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn fsd_connect(
    state: State<'_, AppState>,
    server: String,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let fsd = blocking(move || {
        let token = secret.as_deref().and_then(crate::servers::secrets::get);
        FsdRemote::open(&server, token.as_deref())
    })
    .await?;
    let usb = Some(format!("fsd:{}", fsd.base()));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("fsd at {} is already added.", fsd.base()));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn {
        dev: Arc::new(Device::new(Box::new(fsd))),
        usb,
        serial: None,
    };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(base_url("pc").unwrap(), "http://pc:7700");
        assert_eq!(base_url(" pc:8080/ ").unwrap(), "http://pc:8080");
        assert_eq!(base_url("https://nas").unwrap(), "https://nas:7700");
        assert!(base_url("").is_err());
        assert!(base_url("ftp://pc").is_err());
    }

    #[test]
    fn paths() {
        assert_eq!(native("/C:"), "C:\\");
        assert_eq!(native("/C:/Users/me"), "C:\\Users\\me");
        assert_eq!(native("/home/me"), "/home/me");
        assert_eq!(native("/"), "/");
        assert_eq!(app_path_of("C:\\Users\\me"), "/C:/Users/me");
        assert_eq!(app_path_of("C:\\"), "/C:");
        assert_eq!(app_path_of("/home/me/"), "/home/me");
        assert_eq!(app_path_of("/"), "/");
    }

    /// Against a scratch fsd's REST API: FERRY_FSD_REST is its address, FERRY_FSD_DIR a folder
    /// it indexes, holding one.txt.
    #[test]
    #[ignore]
    fn reads_a_live_fsd() {
        let at = std::env::var("FERRY_FSD_REST").unwrap();
        let dir = std::env::var("FERRY_FSD_DIR").unwrap();
        let fsd = FsdRemote::open(&at, None).unwrap();
        assert!(fsd.quick_dirs().iter().any(|(_, p)| *p == dir));
        let names: Vec<String> = fsd
            .listdir(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"one.txt".to_string()), "{names:?}");
        let (page, total) = fsd
            .listdir_page(&dir, 0, 1, ("name", true))
            .unwrap()
            .unwrap();
        assert_eq!((page.len(), total as usize), (1, names.len()));
        let one = format!("{dir}/one.txt");
        let (data, version) = fsd.read(&one).unwrap();
        let st = fsd.stat(&one).unwrap().unwrap();
        assert_eq!(st.size as usize, data.len());
        assert_eq!(version, format!("{}:{}", st.size, st.mtime));
        assert_eq!(fsd.read_range(&one, 1, 3).unwrap(), data[1..4].to_vec());
        let (found, all) = fsd.find(&dir, "one", false, false, 10).unwrap();
        assert!(
            all && found.iter().any(|e| e.path == one),
            "{:?}",
            found.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
        assert!(fsd.replace(&one, b"x", true).is_err());
        assert!(fsd.stat(&format!("{dir}/nope")).unwrap().is_none());
    }
}
