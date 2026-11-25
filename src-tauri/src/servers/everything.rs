//! Everything (voidtools) servers as read-only drives. Everything's HTTP server answers
//! `/?search=…&json=1` with the names it has indexed on its Windows computer, and serves the
//! files themselves. Folders are listed with `parent:` searches, the drives with `root:`, and
//! searches in a pane use Everything's index instead of walking folders.
//!
//! Paths are the app's ("/C:/Users/me"); "/" holds the drives.

use crate::device::{basename, dirname, err, Result, Search, Transport};
use crate::devices::connect;
use crate::*;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::Value;
use std::io::Read;
use std::time::{Duration, Instant};

/// One name Everything found.
#[derive(Debug, PartialEq)]
struct Hit {
    name: String,
    /// The app's path for it ("/C:/Users/me/a.txt").
    path: String,
    dir: bool,
    size: u64,
    /// Seconds since 1970; 0 when not known.
    mtime: i64,
}

impl From<Hit> for Entry {
    fn from(h: Hit) -> Entry {
        Entry {
            name: h.name,
            path: h.path,
            dir: h.dir,
            size: h.size,
            mtime: h.mtime,
            hidden: false,
        }
    }
}

#[derive(Debug)]
struct Found {
    hits: Vec<Hit>,
    /// How many Everything found in all; `hits` holds up to the count asked for.
    total: u64,
}

/// "http://host:port" from what was typed: "localhost", "pc:8080" or a full address.
fn base_url(server: &str) -> std::result::Result<String, String> {
    let s = server.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("Type the address of a computer running Everything's HTTP server.".into());
    }
    if s.starts_with("https://") {
        return Err("Everything's HTTP server doesn't do https. Use an http:// address.".into());
    }
    Ok(if s.starts_with("http://") {
        s.to_string()
    } else {
        format!("http://{s}")
    })
}

/// Everything's numbers come as text ("1234") or as numbers; empty or missing is 0.
fn number(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// A Windows FILETIME (100 ns steps since 1601) as seconds since 1970.
fn unix_time(filetime: u64) -> i64 {
    if filetime == 0 {
        return 0;
    }
    (filetime / 10_000_000) as i64 - 11_644_473_600
}

/// "C:\Users\me" and "a.txt" as the app's path, "/C:/Users/me/a.txt".
fn app_path_of(folder: &str, name: &str) -> String {
    let folder = folder.replace('\\', "/");
    let folder = folder.trim_end_matches('/');
    if folder.is_empty() {
        format!("/{name}")
    } else {
        format!("/{}/{name}", folder.trim_start_matches('/'))
    }
}

/// The app's "/C:/Users/me" as Windows' "C:\Users\me"; a drive as "C:\".
fn windows_path(path: &str) -> String {
    let p = path.trim_start_matches('/').replace('/', "\\");
    if p.ends_with(':') {
        format!("{p}\\")
    } else {
        p
    }
}

/// Reads Everything's JSON answer.
fn parse(body: &str) -> std::result::Result<Found, String> {
    let v: Value = serde_json::from_str(body).map_err(|_| {
        "Everything's answer wasn't JSON. Is the address its HTTP server?".to_string()
    })?;
    let hits = v["results"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    let name = r["name"].as_str()?.to_string();
                    let path = app_path_of(r["path"].as_str().unwrap_or(""), &name);
                    let dir = r["type"].as_str().is_some_and(|t| t != "file");
                    Some(Hit {
                        size: if dir { 0 } else { number(r.get("size")) },
                        mtime: unix_time(number(r.get("date_modified"))),
                        name,
                        path,
                        dir,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let total = match number(v.get("totalResults")) {
        0 => hits.len() as u64,
        n => n,
    };
    Ok(Found { hits, total })
}

/// Where an Everything server is, and the Authorization header for it when it wants a login.
#[derive(Clone)]
struct Server {
    base: String,
    auth: Option<String>,
}

/// How a page of a folder is sorted: Everything's sort name and whether ascending.
pub struct Order<'a>(pub &'a str, pub bool);

impl Order<'_> {
    const NAME: Order<'static> = Order("name", true);

    /// Everything's name for the app's sort ("name", "size", "date"; others by name).
    fn param(&self) -> &'static str {
        match self.0 {
            "size" => "size",
            "date" => "date_modified",
            _ => "name",
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(60))
        .build()
}

/// What went wrong talking to Everything, in words that say what to do.
fn failed(e: ureq::Error, srv: &Server) -> String {
    let base = &srv.base;
    match e {
        ureq::Error::Status(401, _) if srv.auth.is_some() => {
            format!("Everything at {base} didn't take the user name and password. Check them in Settings > Connections.")
        }
        ureq::Error::Status(401, _) => {
            format!("Everything at {base} asks for a user name and password. Add them in Settings > Connections.")
        }
        ureq::Error::Status(code, _) => format!("Everything at {base} answered with error {code}."),
        ureq::Error::Transport(_) => {
            format!("Can't reach Everything at {base}. Turn on its HTTP server (Tools > Options > HTTP Server) and check the address.")
        }
    }
}

/// Asks the Everything at `srv` for `query`, `count` results from `offset`, in `order`.
fn ask(
    srv: &Server,
    query: &str,
    offset: u64,
    count: u64,
    order: &Order,
) -> std::result::Result<Found, String> {
    let mut req = agent().get(&format!("{}/", srv.base));
    if let Some(a) = &srv.auth {
        req = req.set("Authorization", a);
    }
    let body = req
        .query("search", query)
        .query("json", "1")
        .query("path_column", "1")
        .query("size_column", "1")
        .query("date_modified_column", "1")
        .query("sort", order.param())
        .query("ascending", if order.1 { "1" } else { "0" })
        .query("offset", &offset.to_string())
        .query("count", &count.to_string())
        .call()
        .map_err(|e| failed(e, srv))?
        .into_string()
        .map_err(|e| format!("Couldn't read Everything's answer: {e}"))?;
    parse(&body)
}

/// What stays as it is in a part of a file's address.
const PART: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// The address Everything serves the file at: its Windows path, each part encoded.
fn file_url(base: &str, path: &str) -> String {
    let parts: Vec<String> = path
        .trim_start_matches('/')
        .split('/')
        .map(|p| utf8_percent_encode(p, PART).to_string())
        .collect();
    format!("{base}/{}", parts.join("/"))
}

/// Most names one folder listing takes.
const LIST_MAX: u64 = 100_000;
/// How long a folder listing is reused for looking up the files in it.
const KEEP: Duration = Duration::from_secs(10);

pub struct Everything {
    srv: Server,
    /// The drives, as ("C:", "/C:"). Looked for again by `refresh`.
    drives: Mutex<Vec<(String, String)>>,
    /// The last folder listed, for `stat` of the files in it.
    last: Mutex<Option<(String, Vec<Entry>, Instant)>>,
}

impl Everything {
    /// Connects to the Everything at `server`: it must answer.
    /// With `login` (user name, password), logs in with HTTP Basic authentication.
    pub fn open(server: &str, login: Option<(&str, &str)>) -> std::result::Result<Self, String> {
        use base64::Engine;
        let base = base_url(server)?;
        let auth = login.map(|(u, p)| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
            )
        });
        let srv = Server { base, auth };
        let drives = Self::find_drives(&srv)?;
        Ok(Everything {
            srv,
            drives: Mutex::new(drives),
            last: Mutex::new(None),
        })
    }

    /// The address its HTTP server is at ("http://pc:8080").
    pub fn base(&self) -> &str {
        &self.srv.base
    }

    fn find_drives(srv: &Server) -> std::result::Result<Vec<(String, String)>, String> {
        let mut drives: Vec<(String, String)> = ask(srv, "root:", 0, 1000, &Order::NAME)?
            .hits
            .into_iter()
            .filter(|h| h.dir)
            .map(|h| (h.name, h.path))
            .collect();
        drives.sort();
        Ok(drives)
    }

    fn drives(&self) -> Vec<(String, String)> {
        self.drives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>> {
        Ok(self.list_range(path, 0, LIST_MAX, &Order::NAME)?.0)
    }

    /// `count` of the folder's items from `offset`, in `order`, and how many it has in all.
    fn list_range(
        &self,
        path: &str,
        offset: u64,
        count: u64,
        order: &Order,
    ) -> Result<(Vec<Entry>, u64)> {
        if path == "/" {
            let all = self.drives();
            let total = all.len() as u64;
            let drives = all.into_iter().skip(offset as usize).take(count as usize);
            return Ok((
                drives
                    .map(|(name, path)| Entry {
                        name,
                        path,
                        dir: true,
                        size: 0,
                        mtime: 0,
                        hidden: false,
                    })
                    .collect(),
                total,
            ));
        }
        let win = windows_path(path);
        let mut found = ask(
            &self.srv,
            &format!("parent:\"{win}\""),
            offset,
            count,
            order,
        )
        .map_err(DevError::Msg)?;
        // A drive's own path may be kept as "C:" rather than "C:\".
        if found.hits.is_empty() && offset == 0 && win.ends_with(":\\") {
            found = ask(
                &self.srv,
                &format!("parent:\"{}\"", win.trim_end_matches('\\')),
                offset,
                count,
                order,
            )
            .map_err(DevError::Msg)?;
        }
        let total = found.total;
        let entries: Vec<Entry> = found.hits.into_iter().map(Entry::from).collect();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        match last.as_mut() {
            // A later page of the folder kept: added to it.
            Some((p, kept, at)) if offset > 0 && p == path => {
                kept.extend(entries.iter().cloned());
                *at = Instant::now();
            }
            _ => *last = Some((path.to_string(), entries.clone(), Instant::now())),
        }
        Ok((entries, total))
    }

    fn get(&self, url: &str, range: Option<(u64, u64)>) -> Result<ureq::Response> {
        let mut req = agent().get(url);
        if let Some(a) = &self.srv.auth {
            req = req.set("Authorization", a);
        }
        if let Some((start, len)) = range {
            req = req.set(
                "Range",
                &format!("bytes={start}-{}", start + len.max(1) - 1),
            );
        }
        req.call().map_err(|e| match e {
            ureq::Error::Status(403 | 404, _) => DevError::Msg(format!(
                "Everything at {} doesn't give out this file. Allow file downloads in Everything (Tools > Options > HTTP Server).",
                self.srv.base
            )),
            e => DevError::Msg(failed(e, &self.srv)),
        })
    }

    /// Names under `dir` (any depth) that Everything's index has for `text`: up to `max`, and
    /// whether that's all of them. With `regex`, `text` is a regular expression.
    pub fn search(
        &self,
        dir: &str,
        text: &str,
        regex: bool,
        max: usize,
    ) -> Result<(Vec<Entry>, bool)> {
        let text = text.trim().replace('"', "");
        let term = if regex {
            format!("regex:\"{text}\"")
        } else {
            format!("\"{text}\"")
        };
        let query = if dir == "/" {
            term
        } else {
            format!("\"{}\\\" {term}", windows_path(dir).trim_end_matches('\\'))
        };
        let found = ask(&self.srv, &query, 0, max as u64, &Order::NAME).map_err(DevError::Msg)?;
        let all = found.total <= found.hits.len() as u64;
        let under = format!("{}/", dir.trim_end_matches('/'));
        Ok((
            found
                .hits
                .into_iter()
                .filter(|h| h.path.starts_with(&under))
                .map(Entry::from)
                .collect(),
            all,
        ))
    }
}

impl Transport for Everything {
    fn kind(&self) -> &'static str {
        "everything"
    }

    fn label(&self) -> String {
        self.srv.base.trim_start_matches("http://").to_string()
    }

    fn default_dir(&self) -> String {
        self.drives()
            .first()
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| "/".into())
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        self.drives()
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        self.list(path)
    }

    fn listdir_page(
        &self,
        path: &str,
        offset: u64,
        count: u64,
        sort: (&str, bool),
    ) -> Option<Result<(Vec<Entry>, u64)>> {
        Some(self.list_range(path, offset, count, &Order(sort.0, sort.1)))
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        if path == "/" {
            return Ok(Some(Entry {
                name: String::new(),
                path: "/".into(),
                dir: true,
                size: 0,
                mtime: 0,
                hidden: false,
            }));
        }
        let parent = dirname(path);
        let kept = self
            .last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(p, _, at)| *p == parent && at.elapsed() < KEEP)
            .map(|(_, es, _)| es.clone());
        let entries = match kept {
            Some(es) => es,
            None => self.list(&parent)?,
        };
        let name = basename(path);
        Ok(entries
            .into_iter()
            .find(|e| e.name.eq_ignore_ascii_case(name)))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let mut data = vec![];
        self.get(&file_url(&self.srv.base, path), None)?
            .into_reader()
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path} from Everything: {e}")))?;
        let version = self
            .version(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("{}:0", data.len()));
        Ok((data, version))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let res = self.get(&file_url(&self.srv.base, path), Some((start, len)))?;
        // A server that ignores the range sends the whole file: copy it to the computer in the
        // background, and read from the copy as it comes (or, not knowing its size, skip to `start`).
        let whole = (res.status() != 206)
            .then(|| {
                res.header("Content-Length")
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .flatten();
        if let Some(size) = whole {
            let key = format!("everything|{}|{path}|{size}", self.srv.base);
            let ext = path
                .rsplit_once('.')
                .filter(|(_, x)| !x.contains('/'))
                .map_or(String::new(), |(_, x)| format!(".{x}"));
            let mut body = res.into_reader();
            let fill = move |to: &std::path::Path, stop: &std::sync::atomic::AtomicBool| {
                crate::preview::stream::copy_until(&mut body, to, stop)
            };
            return crate::preview::stream::read(&key, &ext, size, start, len, fill)
                .map_err(DevError::Msg);
        }
        let skip = if res.status() == 206 { 0 } else { start };
        let mut reader = res.into_reader();
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink())
            .map_err(|e| DevError::Msg(e.to_string()))?;
        let mut data = vec![];
        reader
            .take(len)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path} from Everything: {e}")))?;
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
        let mut r = self
            .get(&file_url(&self.srv.base, path), None)?
            .into_reader();
        let total =
            crate::device::pour(&mut r, to, &format!("{path} from Everything"), read, stop)?;
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
        if let Ok(d) = Self::find_drives(&self.srv) {
            *self.drives.lock().unwrap_or_else(|e| e.into_inner()) = d;
        }
    }

    fn search(&self, dir: &str, s: &Search) -> Option<Result<(Vec<Entry>, bool)>> {
        Some(Everything::search(self, dir, s.text, s.regex, s.max))
    }
}

fn read_only<T>() -> Result<T> {
    err("Files on an Everything server can only be read. Copy them to this computer to change them.")
}

/// Connects the Everything server at `server` as a drive.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn everything_connect(
    state: State<'_, AppState>,
    server: String,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let ev = blocking(move || {
        let user = user.filter(|u| !u.trim().is_empty());
        let password = secret
            .as_deref()
            .and_then(crate::servers::secrets::get)
            .unwrap_or_default();
        Everything::open(
            &server,
            user.as_deref().map(|u| (u.trim(), password.as_str())),
        )
    })
    .await?;
    let usb = Some(format!("everything:{}", ev.base()));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("Everything at {} is already added.", ev.base()));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn {
        dev: Arc::new(Device::new(Box::new(ev))),
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
        assert_eq!(base_url("localhost").unwrap(), "http://localhost");
        assert_eq!(base_url(" pc:8080/ ").unwrap(), "http://pc:8080");
        assert_eq!(
            base_url("http://10.0.0.2:81").unwrap(),
            "http://10.0.0.2:81"
        );
        assert!(base_url("").is_err());
        assert!(base_url("https://pc").is_err());
        assert_eq!(windows_path("/C:"), "C:\\");
        assert_eq!(windows_path("/C:/Users/me"), "C:\\Users\\me");
    }

    #[test]
    fn reads_answers() {
        let body = r#"{"totalResults":2,"results":[
            {"type":"folder","name":"Photos","path":"C:\\Users\\me","date_modified":"133000000000000000"},
            {"type":"file","name":"a b.txt","path":"D:\\","size":"12","date_modified":""}]}"#;
        let f = parse(body).unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(f.hits[0].path, "/C:/Users/me/Photos");
        assert!(f.hits[0].dir);
        assert_eq!(f.hits[0].mtime, 13_300_000_000 - 11_644_473_600);
        assert_eq!(
            f.hits[1],
            Hit {
                name: "a b.txt".into(),
                path: "/D:/a b.txt".into(),
                dir: false,
                size: 12,
                mtime: 0
            }
        );
        assert!(parse("<html>").is_err());
    }

    /// A server on a free port giving `answers` in turn: what it was asked (first lines), and
    /// the address to ask it at.
    fn serve(
        answers: Vec<(&'static str, &'static str)>,
    ) -> (std::thread::JoinHandle<Vec<String>>, String) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let asked = std::thread::spawn(move || {
            let mut lines = vec![];
            for (status, body) in answers {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap();
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(reply.as_bytes()).unwrap();
                lines.push(String::from_utf8_lossy(&buf[..n]).to_string());
            }
            lines
        });
        (asked, at)
    }

    #[test]
    fn lists_a_page_at_a_time() {
        let (asked, at) = serve(vec![
            (
                "200 OK",
                r#"{"results":[{"type":"folder","name":"C:","path":""}]}"#,
            ),
            (
                "200 OK",
                r#"{"results":[{"type":"file","name":"b.md","path":"C:\\Big","size":"2"}]}"#,
            ),
        ]);
        let ev = Everything::open(&at, Some(("me", "pw"))).unwrap();
        let (page, total) = ev
            .listdir_page("/C:/Big", 1000, 1000, ("date", false))
            .unwrap()
            .unwrap();
        assert_eq!(
            page.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["/C:/Big/b.md"]
        );
        assert_eq!(total, 1);
        assert_eq!(
            ev.listdir_page("/", 0, 1, ("name", true))
                .unwrap()
                .unwrap()
                .0
                .len(),
            1
        );
        let lines = asked.join().unwrap();
        let first = lines[1].lines().next().unwrap_or_default().to_string();
        assert!(
            first.contains("sort=date_modified&ascending=0&offset=1000&count=1000"),
            "{first}"
        );
        // "me:pw" in base64.
        assert!(
            lines[1].contains("Authorization: Basic bWU6cHc="),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn works_as_a_drive() {
        let (asked, at) = serve(vec![
            (
                "200 OK",
                r#"{"totalResults":2,"results":[{"type":"folder","name":"D:","path":""},{"type":"folder","name":"C:","path":""}]}"#,
            ),
            ("200 OK", r#"{"results":[]}"#),
            (
                "200 OK",
                r#"{"results":[{"type":"file","name":"x.md","path":"C:","size":"5"},{"type":"folder","name":"Users","path":"C:"}]}"#,
            ),
            ("200 OK", "hello"),
            ("206 Partial Content", "ell"),
            (
                "200 OK",
                r#"{"totalResults":"9","results":[{"type":"file","name":"x.md","path":"C:\\Users","size":"3"},{"type":"file","name":"x.md","path":"C:\\Other","size":"3"}]}"#,
            ),
        ]);
        let ev = Everything::open(&at, None).unwrap();
        assert_eq!(
            ev.quick_dirs(),
            vec![
                ("C:".to_string(), "/C:".to_string()),
                ("D:".to_string(), "/D:".to_string())
            ]
        );
        assert_eq!(ev.default_dir(), "/C:");
        assert_eq!(ev.listdir("/").unwrap().len(), 2);
        let items = ev.listdir("/C:").unwrap();
        assert_eq!(
            items.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["/C:/x.md", "/C:/Users"]
        );
        // From the listing just made: no asking.
        assert_eq!(ev.stat("/C:/x.md").unwrap().unwrap().size, 5);
        assert_eq!(
            ev.read("/C:/x.md").unwrap(),
            (b"hello".to_vec(), "5:0".to_string())
        );
        assert_eq!(ev.read_range("/C:/x.md", 1, 3).unwrap(), b"ell");
        let (found, all) = ev.search("/C:/Users", "x.md", false, 10).unwrap();
        assert_eq!((found.len(), all), (1, false));
        assert!(ev.replace("/C:/x.md", b"", true).is_err());

        let lines = asked.join().unwrap();
        let first = |i: usize| lines[i].lines().next().unwrap_or_default().to_string();
        assert!(
            first(0).starts_with("GET /?search=root%3A&json=1"),
            "{}",
            first(0)
        );
        assert!(
            first(1).contains("search=parent%3A%22C%3A%5C%22"),
            "{}",
            first(1)
        );
        assert!(
            first(2).contains("search=parent%3A%22C%3A%22"),
            "{}",
            first(2)
        );
        assert!(first(3).starts_with("GET /C%3A/x.md "), "{}", first(3));
        assert!(
            lines[4].contains("Range: bytes=1-3") || lines[4].contains("range: bytes=1-3"),
            "{}",
            lines[4]
        );
        assert!(
            first(5).contains("search=%22C%3A%5CUsers%5C%22"),
            "{}",
            first(5)
        );
    }

    #[test]
    fn skips_to_the_range_when_the_server_sends_it_all() {
        let (asked, at) = serve(vec![("200 OK", r#"{"results":[]}"#), ("200 OK", "hello")]);
        let ev = Everything::open(&at, None).unwrap();
        assert_eq!(ev.read_range("/C:/x.md", 1, 3).unwrap(), b"ell");
        asked.join().unwrap();
    }

    #[test]
    fn says_what_went_wrong() {
        let (asked, at) = serve(vec![("401 Unauthorized", "")]);
        assert!(Everything::open(&at, None)
            .err()
            .unwrap()
            .contains("user name and password"));
        asked.join().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(Everything::open(&format!("127.0.0.1:{port}"), None)
            .err()
            .unwrap()
            .starts_with("Can't reach Everything"));
    }

    #[test]
    fn file_addresses() {
        assert_eq!(
            file_url("http://pc", "/C:/My Files/ä.txt"),
            "http://pc/C%3A/My%20Files/%C3%A4.txt"
        );
    }
}
