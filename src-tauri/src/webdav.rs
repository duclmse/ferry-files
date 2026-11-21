//! WebDAV servers (Nextcloud, ownCloud, NAS boxes, Box…) as drives, over http or https.
//!
//! The server address is the folder shown as the drive's root; paths are the app's ("/a/b"),
//! relative to it. Folders are listed with PROPFIND, files read with GET, written with PUT to a
//! temporary name and MOVE over the real one.

use super::*;
use crate::device::{basename, dirname, err, join, Result, Transport};
use crate::xml;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::io::Read;
use std::time::Duration;

/// What stays as it is in a part of an address.
const PART: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

/// Biggest folder listing taken, in bytes.
const LIST_MAX: u64 = 256 * 1024 * 1024;

const PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:getetag/></d:prop></d:propfind>"#;

/// Where the server is, from what was typed.
#[derive(Debug, PartialEq)]
struct Place {
    /// "https://host/dav/files/me", encoded, no trailing slash.
    base: String,
    /// The folder's path on the server, decoded, no trailing slash ("" for the server's root).
    prefix: String,
    /// What the drive is called.
    label: String,
}

fn parse_server(server: &str) -> std::result::Result<Place, String> {
    let s = server.trim();
    if s.is_empty() {
        return Err("Type the address of a WebDAV folder.".into());
    }
    let s = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
    let (scheme, rest) = s.split_once("://").unwrap_or(("https", ""));
    if scheme != "http" && scheme != "https" {
        return Err("A WebDAV address starts with http:// or https://.".into());
    }
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if host.is_empty() {
        return Err("The address has no server name.".into());
    }
    if host.contains('@') {
        return Err("Leave the user name out of the address and type it in the user field.".into());
    }
    let parts: Vec<String> = path.split('/').filter(|p| !p.is_empty()).map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned()).collect();
    let prefix = if parts.is_empty() { String::new() } else { format!("/{}", parts.join("/")) };
    let encoded: String = parts.iter().map(|p| format!("/{}", utf8_percent_encode(p, PART))).collect();
    Ok(Place { base: format!("{scheme}://{host}{encoded}"), label: format!("{host}{prefix}"), prefix })
}

/// One thing a PROPFIND answer told about.
#[derive(Debug, PartialEq)]
struct Item {
    /// The app's path for it.
    path: String,
    dir: bool,
    size: u64,
    mtime: i64,
    etag: Option<String>,
}

impl Item {
    fn entry(&self) -> Entry {
        Entry { name: basename(&self.path).to_string(), path: self.path.clone(), dir: self.dir, size: self.size, mtime: self.mtime }
    }

    /// Changes whenever the file does: its ETag, or its size and time when there's none.
    fn version(&self) -> String {
        self.etag.clone().unwrap_or_else(|| format!("{}:{}", self.size, self.mtime))
    }
}

/// An href from an answer (a path or a whole address) as the app's path, or None when it's
/// outside the folder the drive starts at.
fn app_path(href: &str, prefix: &str) -> Option<String> {
    let mut h = href.trim();
    if let Some((_, after)) = h.split_once("://") {
        h = after.find('/').map_or("", |i| &after[i..]);
    }
    let h = h.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode_str(h).decode_utf8_lossy().into_owned();
    let p = decoded.trim_end_matches('/');
    let rest = if prefix.is_empty() { p } else { p.strip_prefix(prefix).filter(|r| r.is_empty() || r.starts_with('/'))? };
    Some(if rest.is_empty() { "/".into() } else { rest.to_string() })
}

#[derive(Default)]
struct Raw {
    href: String,
    dir: bool,
    size: String,
    date: String,
    etag: String,
}

/// Reads a PROPFIND answer ("multistatus"). Prefixes (`d:`, `D:`, none) don't matter.
fn parse(body: &str, prefix: &str) -> std::result::Result<Vec<Item>, String> {
    let toks = xml::tokenize(body).map_err(|e| format!("The server's answer wasn't readable: {e}"))?;
    let mut raws: Vec<Raw> = vec![];
    let mut cur: Option<Raw> = None;
    let mut field: Option<&'static str> = None;
    for t in &toks {
        match t.kind {
            xml::Kind::Open | xml::Kind::Empty => match t.local() {
                "response" if t.kind == xml::Kind::Open => cur = Some(Raw::default()),
                "collection" => {
                    if let Some(c) = cur.as_mut() {
                        c.dir = true;
                    }
                }
                "href" if t.kind == xml::Kind::Open => field = Some("href"),
                "getcontentlength" if t.kind == xml::Kind::Open => field = Some("size"),
                "getlastmodified" if t.kind == xml::Kind::Open => field = Some("date"),
                "getetag" if t.kind == xml::Kind::Open => field = Some("etag"),
                _ => {}
            },
            xml::Kind::Close => {
                field = None;
                if t.local() == "response" {
                    raws.extend(cur.take());
                }
            }
            xml::Kind::Text | xml::Kind::CData => {
                if let (Some(c), Some(f)) = (cur.as_mut(), field) {
                    let text = xml::inner_text(body, std::slice::from_ref(t));
                    match f {
                        "href" => c.href.push_str(&text),
                        "size" => c.size.push_str(&text),
                        "date" => c.date.push_str(&text),
                        _ => c.etag.push_str(&text),
                    }
                }
            }
            _ => {}
        }
    }
    if raws.is_empty() && !body.contains("multistatus") {
        return Err("The server's answer wasn't a WebDAV listing.".into());
    }
    Ok(raws
        .into_iter()
        .filter_map(|r| {
            let path = app_path(&r.href, prefix)?;
            let etag = Some(r.etag.trim().to_string()).filter(|e| !e.is_empty());
            let mtime = chrono::DateTime::parse_from_rfc2822(r.date.trim()).map(|d| d.timestamp()).unwrap_or(0);
            Some(Item { path, dir: r.dir, size: if r.dir { 0 } else { r.size.trim().parse().unwrap_or(0) }, mtime, etag })
        })
        .collect())
}

/// What went wrong talking to the server, in words that say what to do.
fn failed(e: ureq::Error, host: &str, login: bool) -> String {
    match e {
        ureq::Error::Status(401, _) if login => format!("{host} didn't take the user name and password. Check them in Settings > Connections."),
        ureq::Error::Status(401, _) => format!("{host} asks for a user name and password. Add them in Settings > Connections."),
        ureq::Error::Status(403, _) => format!("{host} doesn't allow that."),
        ureq::Error::Status(404, _) => format!("Not found on {host}."),
        ureq::Error::Status(405, _) => format!("{host} doesn't allow that here."),
        ureq::Error::Status(409, _) => format!("The folder isn't there on {host}."),
        ureq::Error::Status(412, _) => "Something with that name is already there.".to_string(),
        ureq::Error::Status(423, _) => "The server has that locked.".to_string(),
        ureq::Error::Status(507, _) => format!("{host} is out of space."),
        ureq::Error::Status(code @ 300..=399, r) => match r.header("location") {
            Some(l) => format!("{host} sent us to {l}. Use that address in Settings > Connections."),
            None => format!("{host} sent us somewhere else (error {code}). Check the address."),
        },
        ureq::Error::Status(code, _) => format!("{host} answered with error {code}."),
        ureq::Error::Transport(t) => {
            let detail = t.to_string();
            let low = detail.to_lowercase();
            if low.contains("certificate") || low.contains("tls") || low.contains("handshake") || low.contains("ssl") {
                format!("Can't make a secure connection to {host}: {detail}")
            } else {
                format!("Can't reach {host}. Check the address and that the server is on.")
            }
        }
    }
}

/// Why a PROPFIND failed: the server said no, or its answer made no sense.
enum Pf {
    Http(ureq::Error),
    Bad(String),
}

impl From<ureq::Error> for Pf {
    fn from(e: ureq::Error) -> Pf {
        Pf::Http(e)
    }
}

pub struct Dav {
    place: Place,
    auth: Option<String>,
    agent: ureq::Agent,
}

impl Dav {
    /// Connects to the folder at `server`: it must answer a PROPFIND.
    /// With `login` (user name, password), logs in with HTTP Basic authentication.
    pub fn open(server: &str, login: Option<(&str, &str)>) -> std::result::Result<Self, String> {
        use base64::Engine;
        let place = parse_server(server)?;
        let auth = login.map(|(u, p)| format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))));
        // Redirects off: ureq would turn a redirected PROPFIND or MOVE into a GET.
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .timeout_write(Duration::from_secs(60))
            .build();
        let dav = Dav { place, auth, agent };
        match dav.propfind("/", "0") {
            Ok(_) => Ok(dav),
            Err(Pf::Http(ureq::Error::Status(404 | 405, _))) | Err(Pf::Bad(_)) => Err(format!("{} isn't a WebDAV folder.", dav.place.label)),
            Err(Pf::Http(e)) => Err(dav.fail(e)),
        }
    }

    fn fail(&self, e: ureq::Error) -> String {
        failed(e, &self.place.label, self.auth.is_some())
    }

    fn dev(&self, e: ureq::Error) -> DevError {
        let conflict = matches!(e, ureq::Error::Status(412, _));
        let msg = self.fail(e);
        if conflict { DevError::Conflict(msg) } else { DevError::Msg(msg) }
    }

    fn dev_pf(&self, e: Pf) -> DevError {
        match e {
            Pf::Http(e) => self.dev(e),
            Pf::Bad(m) => DevError::Msg(m),
        }
    }

    /// The address of an app path; folders get a slash at the end.
    fn url(&self, path: &str, slash: bool) -> String {
        let mut u = self.place.base.clone();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            u.push('/');
            u.push_str(&utf8_percent_encode(seg, PART).to_string());
        }
        if slash || path == "/" {
            u.push('/');
        }
        u
    }

    /// Sends a request. An answer of 3xx counts as an error, so nothing is taken as done.
    fn send(&self, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> std::result::Result<ureq::Response, ureq::Error> {
        let mut req = self.agent.request(method, url);
        if let Some(a) = &self.auth {
            req = req.set("Authorization", a);
        }
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let res = if method == "GET" { req.call()? } else { req.send_bytes(body)? };
        if (300..400).contains(&res.status()) {
            return Err(ureq::Error::Status(res.status(), res));
        }
        Ok(res)
    }

    /// The things PROPFIND tells about `path` (with `depth` "0" or "1").
    fn propfind(&self, path: &str, depth: &str) -> std::result::Result<Vec<Item>, Pf> {
        let headers = [("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")];
        let mut res = self.send("PROPFIND", &self.url(path, depth == "1"), &headers, PROPS.as_bytes());
        // Some servers want a folder's address with a slash at the end.
        if depth == "0" && path != "/" && matches!(res, Err(ureq::Error::Status(301 | 302 | 307 | 308, _))) {
            res = self.send("PROPFIND", &self.url(path, true), &headers, PROPS.as_bytes());
        }
        let mut body = String::new();
        res?.into_reader().take(LIST_MAX).read_to_string(&mut body).map_err(|e| Pf::Bad(format!("Couldn't read the server's answer: {e}")))?;
        parse(&body, &self.place.prefix).map_err(Pf::Bad)
    }

    /// What's at `path`, None when there's nothing.
    fn props(&self, path: &str) -> Result<Option<Item>> {
        match self.propfind(path, "0") {
            Ok(items) => Ok(items.into_iter().find(|i| i.path == path || path == "/")),
            Err(Pf::Http(ureq::Error::Status(404, _))) => Ok(None),
            Err(e) => Err(self.dev_pf(e)),
        }
    }

    fn get(&self, path: &str, range: Option<(u64, u64)>) -> Result<Option<ureq::Response>> {
        let value = range.map(|(start, len)| format!("bytes={start}-{}", start + len.max(1) - 1));
        let headers: Vec<(&str, &str)> = value.iter().map(|v| ("Range", v.as_str())).collect();
        match self.send("GET", &self.url(path, false), &headers, &[]) {
            Ok(r) => Ok(Some(r)),
            // Starting past the end of the file.
            Err(ureq::Error::Status(416, _)) => Ok(None),
            Err(e) => Err(self.dev(e)),
        }
    }

    fn move_to(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let dest = self.url(to, false);
        let headers = [("Destination", dest.as_str()), ("Overwrite", if overwrite { "T" } else { "F" })];
        self.send("MOVE", &self.url(from, false), &headers, &[]).map(|_| ()).map_err(|e| self.dev(e))
    }
}

impl Transport for Dav {
    fn kind(&self) -> &'static str {
        "webdav"
    }

    fn label(&self) -> String {
        self.place.label.clone()
    }

    fn default_dir(&self) -> String {
        "/".into()
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        vec![]
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        let items = self.propfind(path, "1").map_err(|e| self.dev_pf(e))?;
        Ok(items.iter().filter(|i| i.path != path && dirname(&i.path) == path).map(Item::entry).collect())
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        Ok(self.props(path)?.map(|i| i.entry()))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        // The version first: a file that changes in between then looks changed, never unchanged.
        let item = self.props(path)?;
        let Some(res) = self.get(path, None)? else { return err(format!("Couldn't read {path}.")) };
        let mut data = vec![];
        res.into_reader().read_to_end(&mut data).map_err(|e| DevError::Msg(format!("Couldn't read {path}: {e}")))?;
        let version = item.map(|i| i.version()).unwrap_or_else(|| format!("{}:0", data.len()));
        Ok((data, version))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let Some(res) = self.get(path, Some((start, len)))? else { return Ok(vec![]) };
        // A server that ignores the range sends the whole file: copy it to the computer in the
        // background, and read from the copy as it comes (or, not knowing its size, skip to `start`).
        let whole = (res.status() != 206).then(|| res.header("Content-Length").and_then(|n| n.parse::<u64>().ok())).flatten();
        if let Some(size) = whole {
            let key = format!("webdav|{}|{path}|{size}", self.place.base);
            let ext = path.rsplit_once('.').filter(|(_, x)| !x.contains('/')).map_or(String::new(), |(_, x)| format!(".{x}"));
            let mut body = res.into_reader();
            let fill = move |to: &std::path::Path, stop: &std::sync::atomic::AtomicBool| crate::stream::copy_until(&mut body, to, stop);
            return crate::stream::read(&key, &ext, size, start, len, fill).map_err(DevError::Msg);
        }
        let skip = if res.status() == 206 { 0 } else { start };
        let mut reader = res.into_reader();
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink()).map_err(|e| DevError::Msg(e.to_string()))?;
        let mut data = vec![];
        reader.take(len).read_to_end(&mut data).map_err(|e| DevError::Msg(format!("Couldn't read {path}: {e}")))?;
        Ok(data)
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        Ok(self.props(path)?.filter(|i| !i.dir).map(|i| i.version()))
    }

    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tmp = join(&dirname(path), &format!(".{}.ferry-{nanos}.tmp", basename(path)));
        self.send("PUT", &self.url(&tmp, false), &[], data).map_err(|e| self.dev(e))?;
        if let Err(e) = self.move_to(&tmp, path, true) {
            // Don't leave the temporary file behind.
            let _ = self.send("DELETE", &self.url(&tmp, false), &[], &[]);
            return Err(e);
        }
        match self.props(path)? {
            Some(i) => Ok(i.version()),
            None => err(format!("{path} wasn't there after it was written.")),
        }
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        // WebDAV deletes a folder with what's in it.
        self.send("DELETE", &self.url(path, is_dir), &[], &[]).map(|_| ()).map_err(|e| self.dev(e))
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        self.send("MKCOL", &self.url(path, true), &[], &[]).map(|_| ()).map_err(|e| match e {
            ureq::Error::Status(405, _) => DevError::Conflict(format!("{path} already exists.")),
            e => self.dev(e),
        })
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        self.move_to(path, &join(&dirname(path), new_name), false)
    }
}

/// Connects the WebDAV folder at `server` as a drive.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn webdav_connect(
    state: State<'_, AppState>,
    server: String,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let key = server.trim().trim_end_matches('/').to_string();
    let dav = blocking(move || {
        let user = user.filter(|u| !u.trim().is_empty());
        let password = secret.as_deref().and_then(crate::secrets::get).unwrap_or_default();
        Dav::open(&server, user.as_deref().map(|u| (u.trim(), password.as_str())))
    })
    .await?;
    // The address as the app gave it, which is how it finds the connection again.
    let usb = Some(format!("webdav:{}", key));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("{} is already added.", dav.label()));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn { dev: Arc::new(Device::new(Box::new(dav))), usb, serial: None };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const NEXTCLOUD: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/me/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;root1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/My%20Books/</d:href>
  <d:propstat><d:prop><d:getlastmodified>Mon, 10 Oct 2022 08:00:00 GMT</d:getlastmodified><d:resourcetype><d:collection/></d:resourcetype><d:getetag>&quot;f1&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/caf%C3%A9.epub</d:href>
  <d:propstat><d:prop><d:getlastmodified>Tue, 15 Nov 2022 08:12:31 GMT</d:getlastmodified><d:getcontentlength>1234</d:getcontentlength><d:resourcetype/><d:getetag>&quot;abc123&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;

    const APACHE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:"><D:response xmlns:lp1="DAV:"><D:href>http://nas:5005/share/a.txt</D:href>
<D:propstat><D:prop><lp1:resourcetype/><lp1:getcontentlength>7</lp1:getcontentlength><lp1:getlastmodified>Tue, 15 Nov 2022 08:12:31 GMT</lp1:getlastmodified></D:prop></D:propstat></D:response></D:multistatus>"#;

    /// A server on a free port giving `answers` (status, extra headers, body) in turn: what it
    /// was asked (the request with its body), and the address to ask it at.
    fn serve(answers: Vec<(&'static str, &'static str, &'static str)>) -> (std::thread::JoinHandle<Vec<String>>, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let asked = std::thread::spawn(move || {
            let mut lines = vec![];
            for (status, headers, body) in answers {
                let (mut s, _) = listener.accept().unwrap();
                let mut got = vec![];
                let mut buf = [0u8; 4096];
                loop {
                    let n = s.read(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let want = text[..h]
                            .lines()
                            .find_map(|l| l.to_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                            .unwrap_or(0);
                        if got.len() >= h + 4 + want {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let reply = format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                s.write_all(reply.as_bytes()).unwrap();
                lines.push(String::from_utf8_lossy(&got).to_string());
            }
            lines
        });
        (asked, at)
    }

    fn first(req: &str) -> &str {
        req.lines().next().unwrap_or_default()
    }

    fn has_header(req: &str, header: &str) -> bool {
        req.lines().any(|l| l.to_lowercase() == header.to_lowercase())
    }

    #[test]
    fn addresses() {
        let p = parse_server(" cloud.example.com/remote.php/dav/files/my%20me/ ").unwrap();
        assert_eq!(p.base, "https://cloud.example.com/remote.php/dav/files/my%20me");
        assert_eq!(p.prefix, "/remote.php/dav/files/my me");
        assert_eq!(p.label, "cloud.example.com/remote.php/dav/files/my me");
        let p = parse_server("http://nas:5005").unwrap();
        assert_eq!((p.base.as_str(), p.prefix.as_str(), p.label.as_str()), ("http://nas:5005", "", "nas:5005"));
        assert!(parse_server("").is_err());
        assert!(parse_server("ftp://x").is_err());
        assert!(parse_server("https://me@x/dav").is_err());
    }

    #[test]
    fn reads_a_nextcloud_answer() {
        let items = parse(NEXTCLOUD, "/remote.php/dav/files/me").unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!((items[0].path.as_str(), items[0].dir), ("/", true));
        assert_eq!(items[1].path, "/My Books");
        assert!(items[1].dir);
        assert_eq!(items[1].mtime, 1_665_388_800);
        assert_eq!(items[2].path, "/café.epub");
        assert_eq!((items[2].dir, items[2].size, items[2].mtime), (false, 1234, 1_668_499_951));
        assert_eq!(items[2].version(), "\"abc123\"");
        // Outside the folder: left out.
        assert!(parse(NEXTCLOUD, "/other").unwrap().is_empty());
    }

    #[test]
    fn reads_an_answer_with_other_prefixes() {
        let items = parse(APACHE, "/share").unwrap();
        assert_eq!(items, vec![Item { path: "/a.txt".into(), dir: false, size: 7, mtime: 1_668_499_951, etag: None }]);
        assert_eq!(items[0].version(), "7:1668499951");
        assert!(parse("<html>nope</html>", "").is_err());
    }

    #[test]
    fn lists_and_stats() {
        let (asked, at) = serve(vec![("207 Multi-Status", "", NEXTCLOUD), ("207 Multi-Status", "", NEXTCLOUD), ("404 Not Found", "", "")]);
        // The first answer is the check at connect.
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), Some(("me", "pw"))).unwrap();
        let names: Vec<String> = dav.listdir("/").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["My Books", "café.epub"]);
        assert!(dav.stat("/nope").unwrap().is_none());
        let reqs = asked.join().unwrap();
        assert!(first(&reqs[0]).starts_with("PROPFIND /remote.php/dav/files/me/ "), "{}", reqs[0]);
        assert!(has_header(&reqs[0], "Depth: 0"), "{}", reqs[0]);
        assert!(has_header(&reqs[1], "Depth: 1"), "{}", reqs[1]);
        // "me:pw" in base64.
        assert!(has_header(&reqs[0], "Authorization: Basic bWU6cHc="), "{}", reqs[0]);
        assert!(reqs[0].contains("getetag"));
        assert!(first(&reqs[2]).starts_with("PROPFIND /remote.php/dav/files/me/nope "), "{}", reqs[2]);
    }

    #[test]
    fn asks_again_at_the_address_with_a_slash() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", APACHE),
            ("301 Moved Permanently", "Location: /share/a.txt/\r\n", ""),
            ("207 Multi-Status", "", APACHE),
        ]);
        let dav = Dav::open(&format!("{at}/share"), None).unwrap();
        assert_eq!(dav.stat("/a.txt").unwrap().unwrap().size, 7);
        let reqs = asked.join().unwrap();
        assert!(first(&reqs[2]).starts_with("PROPFIND /share/a.txt/ "), "{}", reqs[2]);
    }

    #[test]
    fn reads_ranges() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("206 Partial Content", "Content-Range: bytes 1-3/5\r\n", "ell"),
            ("200 OK", "", "hello"),
            ("416 Range Not Satisfiable", "", ""),
        ]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
        assert_eq!(dav.read_range("/a b.txt", 1, 3).unwrap(), b"ell");
        // The server sent it all: copied to the computer, and read from there.
        assert_eq!(dav.read_range("/c.txt", 1, 3).unwrap(), b"ell");
        assert!(dav.read_range("/c.txt", 99, 3).unwrap().is_empty());
        let reqs = asked.join().unwrap();
        assert!(first(&reqs[1]).starts_with("GET /remote.php/dav/files/me/a%20b.txt "), "{}", reqs[1]);
        assert!(has_header(&reqs[1], "Range: bytes=1-3"), "{}", reqs[1]);
    }

    #[test]
    fn reads_a_file_with_its_version() {
        let (asked, at) = serve(vec![("207 Multi-Status", "", NEXTCLOUD), ("207 Multi-Status", "", NEXTCLOUD), ("200 OK", "", "bytes")]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
        assert_eq!(dav.read("/café.epub").unwrap(), (b"bytes".to_vec(), "\"abc123\"".to_string()));
        asked.join().unwrap();
    }

    #[test]
    fn writes_through_a_temporary_file() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("201 Created", "", ""),
            ("204 No Content", "", ""),
            ("207 Multi-Status", "", NEXTCLOUD),
        ]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
        let v = dav.replace("/café.epub", b"new text", true).unwrap();
        assert_eq!(v, "\"abc123\"");
        let reqs = asked.join().unwrap();
        assert!(first(&reqs[1]).starts_with("PUT /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"), "{}", reqs[1]);
        assert!(reqs[1].ends_with("new text"), "{}", reqs[1]);
        assert!(first(&reqs[2]).starts_with("MOVE /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"), "{}", reqs[2]);
        assert!(has_header(&reqs[2], "Overwrite: T"), "{}", reqs[2]);
        assert!(reqs[2].contains(&format!("Destination: {at}/remote.php/dav/files/me/caf%C3%A9.epub")), "{}", reqs[2]);
    }

    #[test]
    fn deletes_makes_and_renames() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("204 No Content", "", ""),
            ("201 Created", "", ""),
            ("405 Method Not Allowed", "", ""),
            ("201 Created", "", ""),
            ("412 Precondition Failed", "", ""),
        ]);
        let dav = Dav::open(&format!("{at}/dav"), None).unwrap();
        dav.delete("/old", true).unwrap();
        dav.make_dir("/new").unwrap();
        assert!(matches!(dav.make_dir("/new"), Err(DevError::Conflict(_))));
        dav.rename("/a/b.txt", "c d.txt").unwrap();
        assert!(matches!(dav.rename("/a/b.txt", "x"), Err(DevError::Conflict(_))));
        let reqs = asked.join().unwrap();
        assert!(first(&reqs[1]).starts_with("DELETE /dav/old/ "), "{}", reqs[1]);
        assert!(first(&reqs[2]).starts_with("MKCOL /dav/new/ "), "{}", reqs[2]);
        assert!(first(&reqs[4]).starts_with("MOVE /dav/a/b.txt "), "{}", reqs[4]);
        assert!(has_header(&reqs[4], "Overwrite: F"), "{}", reqs[4]);
        assert!(reqs[4].contains(&format!("Destination: {at}/dav/a/c%20d.txt")), "{}", reqs[4]);
    }

    #[test]
    fn says_what_went_wrong() {
        let (asked, at) = serve(vec![("401 Unauthorized", "", "")]);
        let e = Dav::open(&at, Some(("me", "bad"))).err().unwrap();
        assert!(e.contains("user name and password"), "{e}");
        asked.join().unwrap();
        let (asked, at) = serve(vec![("401 Unauthorized", "", "")]);
        assert!(Dav::open(&at, None).err().unwrap().contains("asks for a user name"));
        asked.join().unwrap();
        let (asked, at) = serve(vec![("404 Not Found", "", "")]);
        assert!(Dav::open(&at, None).err().unwrap().contains("isn't a WebDAV folder"));
        asked.join().unwrap();
        let (asked, at) = serve(vec![("301 Moved Permanently", "Location: https://x.example/dav/\r\n", "")]);
        assert!(Dav::open(&at, None).err().unwrap().contains("https://x.example/dav/"));
        asked.join().unwrap();
        let (asked, at) = serve(vec![("200 OK", "", "<html>hi</html>")]);
        assert!(Dav::open(&at, None).err().unwrap().contains("isn't a WebDAV folder"));
        asked.join().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(Dav::open(&format!("http://127.0.0.1:{port}"), None).err().unwrap().starts_with("Can't reach"));
    }
}
