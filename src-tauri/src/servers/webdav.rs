//! WebDAV servers (Nextcloud, ownCloud, NAS boxes, Box…) as drives, over http or https.
//!
//! The server address is the folder shown as the drive's root; paths are the app's ("/a/b"),
//! relative to it. Folders are listed with PROPFIND, files read with GET, written with PUT to a
//! temporary name and MOVE over the real one.

use crate::books::xml;
use crate::device::{basename, dirname, err, join, Result, Transport};
use crate::devices::connect;
use crate::servers::web;
use crate::*;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::io::Read;
use std::time::Duration;

mod digest;
use digest::*;

/// What stays as it is in a part of an address.
const PART: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

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
    let s = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
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
    let parts: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
        .collect();
    let prefix = if parts.is_empty() {
        String::new()
    } else {
        format!("/{}", parts.join("/"))
    };
    let encoded: String = parts
        .iter()
        .map(|p| format!("/{}", utf8_percent_encode(p, PART)))
        .collect();
    Ok(Place {
        base: format!("{scheme}://{host}{encoded}"),
        label: format!("{host}{prefix}"),
        prefix,
    })
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
        Entry {
            name: basename(&self.path).to_string(),
            path: self.path.clone(),
            dir: self.dir,
            size: self.size,
            mtime: self.mtime,
            hidden: false,
        }
    }

    /// Changes whenever the file does: its ETag, or its size and time when there's none.
    fn version(&self) -> String {
        self.etag
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.size, self.mtime))
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
    let rest = if prefix.is_empty() {
        p
    } else {
        p.strip_prefix(prefix)
            .filter(|r| r.is_empty() || r.starts_with('/'))?
    };
    Some(if rest.is_empty() {
        "/".into()
    } else {
        rest.to_string()
    })
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
    let toks =
        xml::tokenize(body).map_err(|e| format!("The server's answer wasn't readable: {e}"))?;
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
            let mtime = chrono::DateTime::parse_from_rfc2822(r.date.trim())
                .map(|d| d.timestamp())
                .unwrap_or(0);
            Some(Item {
                path,
                dir: r.dir,
                size: if r.dir {
                    0
                } else {
                    r.size.trim().parse().unwrap_or(0)
                },
                mtime,
                etag,
            })
        })
        .collect())
}

/// What went wrong talking to the server, in words that say what to do.
fn failed(e: web::Error, host: &str, login: bool) -> String {
    match e {
        web::Error::Status(401, _) if login => format!(
            "{host} didn't take the user name and password. Check them in Settings > Connections."
        ),
        web::Error::Status(401, _) => {
            format!("{host} asks for a user name and password. Add them in Settings > Connections.")
        }
        web::Error::Status(403, _) => format!("{host} doesn't allow that."),
        web::Error::Status(404, _) => format!("Not found on {host}."),
        web::Error::Status(405, _) => format!("{host} doesn't allow that here."),
        web::Error::Status(409, _) => format!("The folder isn't there on {host}."),
        web::Error::Status(412, _) => "Something with that name is already there.".to_string(),
        web::Error::Status(423, _) => "The server has that locked.".to_string(),
        web::Error::Status(507, _) => format!("{host} is out of space."),
        web::Error::Status(code @ 300..=399, r) => match web::header(&r, "location") {
            Some(l) => {
                format!("{host} sent us to {l}. Use that address in Settings > Connections.")
            }
            None => format!("{host} sent us somewhere else (error {code}). Check the address."),
        },
        web::Error::Status(code, _) => format!("{host} answered with error {code}."),
        web::Error::Transport(t) => {
            let detail = t.to_string();
            let low = detail.to_lowercase();
            if low.contains("certificate")
                || low.contains("tls")
                || low.contains("handshake")
                || low.contains("ssl")
            {
                format!("Can't make a secure connection to {host}: {detail}")
            } else {
                format!("Can't reach {host}. Check the address and that the server is on.")
            }
        }
    }
}

/// Why a PROPFIND failed: the server said no, or its answer made no sense.
enum Pf {
    Http(web::Error),
    Bad(String),
}

impl From<web::Error> for Pf {
    fn from(e: web::Error) -> Pf {
        Pf::Http(e)
    }
}

/// The scheme, host and port of an address, and the rest from the path on.
fn split_url(url: &str) -> Option<(&str, &str, u16, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let (auth, path) = rest
        .find('/')
        .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
    let default = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    let (host, port) = match auth
        .rsplit_once(':')
        .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
    {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (auth, default),
    };
    Some((scheme, host, port, path))
}

/// The address a `Location` header means, from the address that was asked.
fn resolve(from: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.contains("://") {
        return Some(location.to_string());
    }
    let (scheme, _, _, path) = split_url(from)?;
    let origin = &from[..from.len() - path.len()];
    if let Some(rest) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    if location.starts_with('/') {
        return Some(format!("{origin}{location}"));
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    Some(format!("{origin}{}/{location}", dir.trim_end_matches('/')))
}

/// Whether a redirect from `from` to `to` may be followed: the same server, or the same host
/// upgraded to https. Anything else could send the user name and password elsewhere.
fn may_follow(from: &str, to: &str) -> bool {
    let (Some((fs, fh, fp, _)), Some((ts, th, tp, _))) = (split_url(from), split_url(to)) else {
        return false;
    };
    let (fs, ts) = (fs.to_lowercase(), ts.to_lowercase());
    if !fh.eq_ignore_ascii_case(th) {
        return false;
    }
    (fs == ts && fp == tp) || (fs == "http" && ts == "https")
}

/// Most redirects followed for one request.
const HOPS: usize = 5;

pub struct Dav {
    place: Place,
    /// User name and password.
    login: Option<(String, String)>,
    /// The Digest challenge in use; with none, Basic goes out.
    digest: std::sync::Mutex<Option<Digest>>,
    agent: ureq::Agent,
}

/// What a request sends: bytes, or a file on this computer, read as it's sent.
#[derive(Clone, Copy)]
enum Body<'a> {
    Bytes(&'a [u8]),
    File(&'a std::path::Path),
}

impl Dav {
    /// Connects to the folder at `server`: it must answer a PROPFIND.
    /// With `login` (user name, password), logs in with HTTP Basic authentication, or Digest
    /// when the server asks for that.
    pub fn open(server: &str, login: Option<(&str, &str)>) -> std::result::Result<Self, String> {
        let place = parse_server(server)?;
        let login = login.map(|(u, p)| (u.to_string(), p.to_string()));
        // Redirects off in ureq, which would turn a redirected PROPFIND or MOVE into a GET:
        // `send` follows them itself.
        let agent = web::config()
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(60)))
            .timeout_per_write(Some(Duration::from_secs(60)))
            .build()
            .into();
        let dav = Dav {
            place,
            login,
            digest: Default::default(),
            agent,
        };
        match dav.propfind("/", "0") {
            Ok(_) => Ok(dav),
            Err(Pf::Http(web::Error::Status(404 | 405, _))) | Err(Pf::Bad(_)) => {
                Err(format!("{} isn't a WebDAV folder.", dav.place.label))
            }
            Err(Pf::Http(e)) => Err(dav.fail(e)),
        }
    }

    fn fail(&self, e: web::Error) -> String {
        failed(e, &self.place.label, self.login.is_some())
    }

    fn dev(&self, e: web::Error) -> DevError {
        let conflict = matches!(e, web::Error::Status(412, _));
        let msg = self.fail(e);
        if conflict {
            DevError::Conflict(msg)
        } else {
            DevError::Msg(msg)
        }
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

    /// The Authorization header for a request, None without a login.
    fn authorization(&self, method: &str, url: &str) -> Option<String> {
        use base64::Engine;
        let (user, pass) = self.login.as_ref()?;
        let mut state = self.digest.lock().unwrap_or_else(|e| e.into_inner());
        let Some(d) = state.as_mut() else {
            return Some(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
            ));
        };
        d.count += 1;
        let uri = split_url(url).map_or("/", |(_, _, _, p)| if p.is_empty() { "/" } else { p });
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.as_nanos());
        let cnonce = d.hash(&format!("{nanos}:{}:{}", d.nonce, d.count))[..16].to_string();
        Some(d.header_with(user, pass, method, uri, d.count, &cnonce))
    }

    /// One request to one address, with the login. On a 401 that asks for Digest, the challenge is
    /// kept and the request sent again, once.
    fn attempt(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Body,
    ) -> std::result::Result<web::Response, web::Error> {
        let mut retried = false;
        loop {
            let auth = self.authorization(method, url);
            let mut hdrs: Vec<(&str, &str)> = Vec::with_capacity(headers.len() + 1);
            if let Some(a) = &auth {
                hdrs.push(("Authorization", a));
            }
            hdrs.extend(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            let (mut bytes, mut file);
            let send: Option<(&mut dyn Read, u64)> = match body {
                _ if method == "GET" => None,
                Body::Bytes(b) => {
                    bytes = b;
                    Some((&mut bytes, b.len() as u64))
                }
                // Opened again for each try: a retry or redirect sends it from the start.
                Body::File(from) => {
                    file = std::fs::File::open(from).map_err(|e| {
                        web::Error::from(std::io::Error::new(
                            e.kind(),
                            format!("{}: {e}", from.display()),
                        ))
                    })?;
                    let size = file.metadata().map_err(web::Error::from)?.len();
                    Some((&mut file, size))
                }
            };
            let res = web::run(&self.agent, method, url, &hdrs, send);
            let Err(web::Error::Status(401, r)) = res else {
                return res;
            };
            if !retried && self.login.is_some() {
                if let Some(new) = digest_challenge(&web::headers(&r, "www-authenticate")) {
                    let mut state = self.digest.lock().unwrap_or_else(|e| e.into_inner());
                    // A new nonce, or the same Digest again after a stale one: worth another try.
                    let worth = match state.as_ref() {
                        None => true,
                        Some(old) => new.stale || old.nonce != new.nonce,
                    };
                    if worth {
                        *state = Some(new);
                        retried = true;
                        continue;
                    }
                }
            }
            return Err(web::Error::Status(401, r));
        }
    }

    /// Sends a request, following redirects (301, 302, 307, 308) with the same method and body, up
    /// to `HOPS` times, to the same server or the same host over https. Any other 3xx counts as an
    /// error, so nothing is taken as done. The login only goes where the request first went.
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> std::result::Result<web::Response, web::Error> {
        self.send_body(method, url, headers, Body::Bytes(body))
    }

    /// As `send`, with the body in bytes or in a file on this computer.
    fn send_body(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> std::result::Result<web::Response, web::Error> {
        let mut url = url.to_string();
        let mut headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut hops = 0;
        loop {
            let res = self.attempt(method, &url, &headers, body)?;
            let code = res.status().as_u16();
            if !(300..400).contains(&code) {
                return Ok(res);
            }
            let next = web::header(&res, "location").and_then(|l| resolve(&url, l));
            match next {
                Some(next)
                    if matches!(code, 301 | 302 | 307 | 308)
                        && hops < HOPS
                        && may_follow(&url, &next) =>
                {
                    // MOVE says where to in a header: the same server as the request's.
                    let old = url.len() - split_url(&url).map_or(0, |(_, _, _, p)| p.len());
                    let new = next.len() - split_url(&next).map_or(0, |(_, _, _, p)| p.len());
                    for (k, v) in headers.iter_mut() {
                        if k.eq_ignore_ascii_case("destination") && v.starts_with(&url[..old]) {
                            *v = format!("{}{}", &next[..new], &v[old..]);
                        }
                    }
                    url = next;
                    hops += 1;
                }
                _ => return Err(web::Error::Status(code, res)),
            }
        }
    }

    /// The things PROPFIND tells about `path` (with `depth` "0" or "1").
    fn propfind(&self, path: &str, depth: &str) -> std::result::Result<Vec<Item>, Pf> {
        let headers = [
            ("Depth", depth),
            ("Content-Type", "application/xml; charset=utf-8"),
        ];
        // A server that wants a folder's address with a slash at the end redirects to it.
        let res = self.send(
            "PROPFIND",
            &self.url(path, depth == "1"),
            &headers,
            PROPS.as_bytes(),
        );
        let mut body = String::new();
        web::reader(res?)
            .take(LIST_MAX)
            .read_to_string(&mut body)
            .map_err(|e| Pf::Bad(format!("Couldn't read the server's answer: {e}")))?;
        parse(&body, &self.place.prefix).map_err(Pf::Bad)
    }

    /// What's at `path`, None when there's nothing.
    fn props(&self, path: &str) -> Result<Option<Item>> {
        match self.propfind(path, "0") {
            Ok(items) => Ok(items.into_iter().find(|i| i.path == path || path == "/")),
            Err(Pf::Http(web::Error::Status(404, _))) => Ok(None),
            Err(e) => Err(self.dev_pf(e)),
        }
    }

    fn get(&self, path: &str, range: Option<(u64, u64)>) -> Result<Option<web::Response>> {
        let value = range.map(|(start, len)| format!("bytes={start}-{}", start + len.max(1) - 1));
        let headers: Vec<(&str, &str)> = value.iter().map(|v| ("Range", v.as_str())).collect();
        match self.send("GET", &self.url(path, false), &headers, &[]) {
            Ok(r) => Ok(Some(r)),
            // Starting past the end of the file.
            Err(web::Error::Status(416, _)) => Ok(None),
            Err(e) => Err(self.dev(e)),
        }
    }

    /// Writes the file under a temporary name and moves it over `path`. Gives the new version.
    fn put(&self, path: &str, body: Body) -> Result<String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tmp = join(
            &dirname(path),
            &format!(".{}.ferry-{nanos}.tmp", basename(path)),
        );
        self.send_body("PUT", &self.url(&tmp, false), &[], body)
            .map_err(|e| self.dev(e))?;
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

    fn move_to(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let dest = self.url(to, false);
        let headers = [
            ("Destination", dest.as_str()),
            ("Overwrite", if overwrite { "T" } else { "F" }),
        ];
        self.send("MOVE", &self.url(from, false), &headers, &[])
            .map(|_| ())
            .map_err(|e| self.dev(e))
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
        Ok(items
            .iter()
            .filter(|i| i.path != path && dirname(&i.path) == path)
            .map(Item::entry)
            .collect())
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        Ok(self.props(path)?.map(|i| i.entry()))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        // The version first: a file that changes in between then looks changed, never unchanged.
        let item = self.props(path)?;
        let Some(res) = self.get(path, None)? else {
            return err(format!("Couldn't read {path}."));
        };
        let mut data = vec![];
        web::reader(res)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path}: {e}")))?;
        let version = item
            .map(|i| i.version())
            .unwrap_or_else(|| format!("{}:0", data.len()));
        Ok((data, version))
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let Some(res) = self.get(path, Some((start, len)))? else {
            return Ok(vec![]);
        };
        // A server that ignores the range sends the whole file: copy it to the computer in the
        // background, and read from the copy as it comes (or, not knowing its size, skip to `start`).
        let whole = (res.status() != 206)
            .then(|| web::header(&res, "Content-Length").and_then(|n| n.parse::<u64>().ok()))
            .flatten();
        if let Some(size) = whole {
            let key = format!("webdav|{}|{path}|{size}", self.place.base);
            let ext = path
                .rsplit_once('.')
                .filter(|(_, x)| !x.contains('/'))
                .map_or(String::new(), |(_, x)| format!(".{x}"));
            let mut body = web::reader(res);
            let fill = move |to: &std::path::Path, stop: &std::sync::atomic::AtomicBool| {
                crate::preview::stream::copy_until(&mut body, to, stop)
            };
            return crate::preview::stream::read(&key, &ext, size, start, len, fill)
                .map_err(DevError::Msg);
        }
        let skip = if res.status() == 206 { 0 } else { start };
        let mut reader = web::reader(res);
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink())
            .map_err(|e| DevError::Msg(e.to_string()))?;
        let mut data = vec![];
        reader
            .take(len)
            .read_to_end(&mut data)
            .map_err(|e| DevError::Msg(format!("Couldn't read {path}: {e}")))?;
        Ok(data)
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        Ok(self.props(path)?.filter(|i| !i.dir).map(|i| i.version()))
    }

    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        self.put(path, Body::Bytes(data))
    }

    fn replace_from(&self, path: &str, from: &std::path::Path, _exists: bool) -> Result<String> {
        self.put(path, Body::File(from))
    }

    fn read_to(
        &self,
        path: &str,
        to: &std::path::Path,
        read: &mut dyn FnMut(u64),
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<String> {
        // The version first, as in `read`.
        let item = self.props(path)?;
        let Some(res) = self.get(path, None)? else {
            return err(format!("Couldn't read {path}."));
        };
        let total = crate::device::pour(&mut web::reader(res), to, path, read, stop)?;
        Ok(item
            .map(|i| i.version())
            .unwrap_or_else(|| format!("{total}:0")))
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        // WebDAV deletes a folder with what's in it.
        self.send("DELETE", &self.url(path, is_dir), &[], &[])
            .map(|_| ())
            .map_err(|e| self.dev(e))
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        self.send("MKCOL", &self.url(path, true), &[], &[])
            .map(|_| ())
            .map_err(|e| match e {
                web::Error::Status(405, _) => DevError::Conflict(format!("{path} already exists.")),
                e => self.dev(e),
            })
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        self.move_to(path, &join(&dirname(path), new_name), false)
    }
}

/// Connects the WebDAV folder at `server` as a drive.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn webdav_connect(
    state: State<'_, AppState>,
    server: String,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let key = server.trim().trim_end_matches('/').to_string();
    let dav = blocking(move || {
        let user = user.filter(|u| !u.trim().is_empty());
        let password = secret
            .as_deref()
            .and_then(crate::servers::secrets::get)
            .unwrap_or_default();
        Dav::open(
            &server,
            user.as_deref().map(|u| (u.trim(), password.as_str())),
        )
    })
    .await?;
    // The address as the app gave it, which is how it finds the connection again.
    let usb = Some(format!("webdav:{}", key));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("{} is already added.", dav.label()));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn {
        dev: Arc::new(Device::new(Box::new(dav))),
        usb,
        serial: None,
    };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests;
