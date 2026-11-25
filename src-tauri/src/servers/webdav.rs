//! WebDAV servers (Nextcloud, ownCloud, NAS boxes, Box…) as drives, over http or https.
//!
//! The server address is the folder shown as the drive's root; paths are the app's ("/a/b"),
//! relative to it. Folders are listed with PROPFIND, files read with GET, written with PUT to a
//! temporary name and MOVE over the real one.

use crate::books::xml;
use crate::device::{basename, dirname, err, join, Result, Transport};
use crate::devices::connect;
use crate::*;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::io::Read;
use std::time::Duration;

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
fn failed(e: ureq::Error, host: &str, login: bool) -> String {
    match e {
        ureq::Error::Status(401, _) if login => format!(
            "{host} didn't take the user name and password. Check them in Settings > Connections."
        ),
        ureq::Error::Status(401, _) => {
            format!("{host} asks for a user name and password. Add them in Settings > Connections.")
        }
        ureq::Error::Status(403, _) => format!("{host} doesn't allow that."),
        ureq::Error::Status(404, _) => format!("Not found on {host}."),
        ureq::Error::Status(405, _) => format!("{host} doesn't allow that here."),
        ureq::Error::Status(409, _) => format!("The folder isn't there on {host}."),
        ureq::Error::Status(412, _) => "Something with that name is already there.".to_string(),
        ureq::Error::Status(423, _) => "The server has that locked.".to_string(),
        ureq::Error::Status(507, _) => format!("{host} is out of space."),
        ureq::Error::Status(code @ 300..=399, r) => match r.header("location") {
            Some(l) => {
                format!("{host} sent us to {l}. Use that address in Settings > Connections.")
            }
            None => format!("{host} sent us somewhere else (error {code}). Check the address."),
        },
        ureq::Error::Status(code, _) => format!("{host} answered with error {code}."),
        ureq::Error::Transport(t) => {
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
    Http(ureq::Error),
    Bad(String),
}

impl From<ureq::Error> for Pf {
    fn from(e: ureq::Error) -> Pf {
        Pf::Http(e)
    }
}

/// What a server's `WWW-Authenticate: Digest` challenge asks for (RFC 7616).
#[derive(Debug, Clone, PartialEq)]
struct Digest {
    realm: String,
    nonce: String,
    opaque: Option<String>,
    /// SHA-256 (else MD5).
    sha256: bool,
    /// The "-sess" variant of the algorithm.
    sess: bool,
    /// The server wants a counter and client nonce (qop=auth). Old servers don't.
    qop: bool,
    /// The challenge said the nonce is out of date, not that the password is wrong.
    stale: bool,
    /// Requests sent with this nonce.
    count: u32,
}

/// Splits `key=value, key="quoted, value"` pairs.
fn params(s: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut rest = s.trim_start();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_lowercase();
        let after = rest[eq + 1..].trim_start();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            let mut v = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            v.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => v.push(c),
                }
            }
            (v, &q[end.min(q.len())..])
        } else {
            let e = after.find(',').unwrap_or(after.len());
            (after[..e].trim().to_string(), &after[e..])
        };
        out.push((key, value));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

/// The Digest challenge among the `WWW-Authenticate` values, the SHA-256 one when there are two.
fn digest_challenge(values: &[&str]) -> Option<Digest> {
    let mut best: Option<Digest> = None;
    for v in values {
        let v = v.trim();
        let Some(args) = v
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("digest "))
            .map(|_| &v[7..])
        else {
            continue;
        };
        let p = params(args);
        let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let algo = get("algorithm")
            .unwrap_or_else(|| "MD5".into())
            .to_uppercase();
        let (sha256, sess) = match algo.as_str() {
            "MD5" => (false, false),
            "MD5-SESS" => (false, true),
            "SHA-256" => (true, false),
            "SHA-256-SESS" => (true, true),
            _ => continue,
        };
        let qop =
            get("qop").is_some_and(|q| q.split(',').any(|x| x.trim().eq_ignore_ascii_case("auth")));
        if get("qop").is_some() && !qop {
            continue;
        }
        let Some(nonce) = get("nonce") else { continue };
        let d = Digest {
            realm: get("realm").unwrap_or_default(),
            nonce,
            opaque: get("opaque"),
            sha256,
            sess,
            qop,
            stale: get("stale").is_some_and(|s| s.eq_ignore_ascii_case("true")),
            count: 0,
        };
        if best.as_ref().is_none_or(|b| !b.sha256 && d.sha256) {
            best = Some(d);
        }
    }
    best
}

impl Digest {
    fn hash(&self, s: &str) -> String {
        if self.sha256 {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(s.as_bytes()))
        } else {
            format!("{:x}", md5::compute(s.as_bytes()))
        }
    }

    /// The Authorization header for a request, with `nc` and `cnonce` given (for tests).
    fn header_with(
        &self,
        user: &str,
        pass: &str,
        method: &str,
        uri: &str,
        nc: u32,
        cnonce: &str,
    ) -> String {
        let mut a1 = self.hash(&format!("{user}:{}:{pass}", self.realm));
        if self.sess {
            a1 = self.hash(&format!("{a1}:{}:{cnonce}", self.nonce));
        }
        let ha2 = self.hash(&format!("{method}:{uri}"));
        let nc_hex = format!("{nc:08x}");
        let response = if self.qop {
            self.hash(&format!("{a1}:{}:{nc_hex}:{cnonce}:auth:{ha2}", self.nonce))
        } else {
            self.hash(&format!("{a1}:{}:{ha2}", self.nonce))
        };
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut h = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", algorithm={}{}, response=\"{response}\"",
            q(user),
            q(&self.realm),
            q(&self.nonce),
            q(uri),
            if self.sha256 { "SHA-256" } else { "MD5" },
            if self.sess { "-sess" } else { "" },
        );
        if self.qop {
            h.push_str(&format!(", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\""));
        }
        if let Some(o) = &self.opaque {
            h.push_str(&format!(", opaque=\"{}\"", q(o)));
        }
        h
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
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .timeout_write(Duration::from_secs(60))
            .build();
        let dav = Dav {
            place,
            login,
            digest: Default::default(),
            agent,
        };
        match dav.propfind("/", "0") {
            Ok(_) => Ok(dav),
            Err(Pf::Http(ureq::Error::Status(404 | 405, _))) | Err(Pf::Bad(_)) => {
                Err(format!("{} isn't a WebDAV folder.", dav.place.label))
            }
            Err(Pf::Http(e)) => Err(dav.fail(e)),
        }
    }

    fn fail(&self, e: ureq::Error) -> String {
        failed(e, &self.place.label, self.login.is_some())
    }

    fn dev(&self, e: ureq::Error) -> DevError {
        let conflict = matches!(e, ureq::Error::Status(412, _));
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
    ) -> std::result::Result<ureq::Response, ureq::Error> {
        let mut retried = false;
        loop {
            let mut req = self.agent.request(method, url);
            if let Some(a) = self.authorization(method, url) {
                req = req.set("Authorization", &a);
            }
            for (k, v) in headers {
                req = req.set(k, v);
            }
            let res = match body {
                _ if method == "GET" => req.call(),
                Body::Bytes(b) => req.send_bytes(b),
                // Opened again for each try: a retry or redirect sends it from the start.
                Body::File(from) => {
                    let f = std::fs::File::open(from).map_err(|e| {
                        ureq::Error::from(std::io::Error::new(
                            e.kind(),
                            format!("{}: {e}", from.display()),
                        ))
                    })?;
                    let size = f.metadata().map_err(ureq::Error::from)?.len();
                    req.set("Content-Length", &size.to_string()).send(f)
                }
            };
            let Err(ureq::Error::Status(401, r)) = res else {
                return res;
            };
            if !retried && self.login.is_some() {
                if let Some(new) = digest_challenge(&r.all("www-authenticate")) {
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
            return Err(ureq::Error::Status(401, r));
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
    ) -> std::result::Result<ureq::Response, ureq::Error> {
        self.send_body(method, url, headers, Body::Bytes(body))
    }

    /// As `send`, with the body in bytes or in a file on this computer.
    fn send_body(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> std::result::Result<ureq::Response, ureq::Error> {
        let mut url = url.to_string();
        let mut headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut hops = 0;
        loop {
            let res = self.attempt(method, &url, &headers, body)?;
            let code = res.status();
            if !(300..400).contains(&code) {
                return Ok(res);
            }
            let next = res.header("location").and_then(|l| resolve(&url, l));
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
                _ => return Err(ureq::Error::Status(code, res)),
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
        res?.into_reader()
            .take(LIST_MAX)
            .read_to_string(&mut body)
            .map_err(|e| Pf::Bad(format!("Couldn't read the server's answer: {e}")))?;
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
        res.into_reader()
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
            .then(|| {
                res.header("Content-Length")
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .flatten();
        if let Some(size) = whole {
            let key = format!("webdav|{}|{path}|{size}", self.place.base);
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
        let total = crate::device::pour(&mut res.into_reader(), to, path, read, stop)?;
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
                ureq::Error::Status(405, _) => {
                    DevError::Conflict(format!("{path} already exists."))
                }
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
    fn serve(
        answers: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (std::thread::JoinHandle<Vec<String>>, String) {
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
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
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
        req.lines()
            .any(|l| l.to_lowercase() == header.to_lowercase())
    }

    #[test]
    fn addresses() {
        let p = parse_server(" cloud.example.com/remote.php/dav/files/my%20me/ ").unwrap();
        assert_eq!(
            p.base,
            "https://cloud.example.com/remote.php/dav/files/my%20me"
        );
        assert_eq!(p.prefix, "/remote.php/dav/files/my me");
        assert_eq!(p.label, "cloud.example.com/remote.php/dav/files/my me");
        let p = parse_server("http://nas:5005").unwrap();
        assert_eq!(
            (p.base.as_str(), p.prefix.as_str(), p.label.as_str()),
            ("http://nas:5005", "", "nas:5005")
        );
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
        assert_eq!(
            (items[2].dir, items[2].size, items[2].mtime),
            (false, 1234, 1_668_499_951)
        );
        assert_eq!(items[2].version(), "\"abc123\"");
        // Outside the folder: left out.
        assert!(parse(NEXTCLOUD, "/other").unwrap().is_empty());
    }

    #[test]
    fn reads_an_answer_with_other_prefixes() {
        let items = parse(APACHE, "/share").unwrap();
        assert_eq!(
            items,
            vec![Item {
                path: "/a.txt".into(),
                dir: false,
                size: 7,
                mtime: 1_668_499_951,
                etag: None
            }]
        );
        assert_eq!(items[0].version(), "7:1668499951");
        assert!(parse("<html>nope</html>", "").is_err());
    }

    #[test]
    fn lists_and_stats() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("207 Multi-Status", "", NEXTCLOUD),
            ("404 Not Found", "", ""),
        ]);
        // The first answer is the check at connect.
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), Some(("me", "pw"))).unwrap();
        let names: Vec<String> = dav
            .listdir("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["My Books", "café.epub"]);
        assert!(dav.stat("/nope").unwrap().is_none());
        let reqs = asked.join().unwrap();
        assert!(
            first(&reqs[0]).starts_with("PROPFIND /remote.php/dav/files/me/ "),
            "{}",
            reqs[0]
        );
        assert!(has_header(&reqs[0], "Depth: 0"), "{}", reqs[0]);
        assert!(has_header(&reqs[1], "Depth: 1"), "{}", reqs[1]);
        // "me:pw" in base64.
        assert!(
            has_header(&reqs[0], "Authorization: Basic bWU6cHc="),
            "{}",
            reqs[0]
        );
        assert!(reqs[0].contains("getetag"));
        assert!(
            first(&reqs[2]).starts_with("PROPFIND /remote.php/dav/files/me/nope "),
            "{}",
            reqs[2]
        );
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
        assert!(
            first(&reqs[2]).starts_with("PROPFIND /share/a.txt/ "),
            "{}",
            reqs[2]
        );
    }

    #[test]
    fn reads_ranges() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            (
                "206 Partial Content",
                "Content-Range: bytes 1-3/5\r\n",
                "ell",
            ),
            ("200 OK", "", "hello"),
            ("416 Range Not Satisfiable", "", ""),
        ]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
        assert_eq!(dav.read_range("/a b.txt", 1, 3).unwrap(), b"ell");
        // The server sent it all: copied to the computer, and read from there.
        assert_eq!(dav.read_range("/c.txt", 1, 3).unwrap(), b"ell");
        assert!(dav.read_range("/c.txt", 99, 3).unwrap().is_empty());
        let reqs = asked.join().unwrap();
        assert!(
            first(&reqs[1]).starts_with("GET /remote.php/dav/files/me/a%20b.txt "),
            "{}",
            reqs[1]
        );
        assert!(has_header(&reqs[1], "Range: bytes=1-3"), "{}", reqs[1]);
    }

    #[test]
    fn reads_a_file_with_its_version() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("207 Multi-Status", "", NEXTCLOUD),
            ("200 OK", "", "bytes"),
        ]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
        assert_eq!(
            dav.read("/café.epub").unwrap(),
            (b"bytes".to_vec(), "\"abc123\"".to_string())
        );
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
        assert!(
            first(&reqs[1]).starts_with("PUT /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"),
            "{}",
            reqs[1]
        );
        assert!(reqs[1].ends_with("new text"), "{}", reqs[1]);
        assert!(
            first(&reqs[2]).starts_with("MOVE /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"),
            "{}",
            reqs[2]
        );
        assert!(has_header(&reqs[2], "Overwrite: T"), "{}", reqs[2]);
        assert!(
            reqs[2].contains(&format!(
                "Destination: {at}/remote.php/dav/files/me/caf%C3%A9.epub"
            )),
            "{}",
            reqs[2]
        );
    }

    #[test]
    fn copies_to_and_from_files_on_this_computer() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("207 Multi-Status", "", NEXTCLOUD),
            ("200 OK", "", "the file's bytes"),
            ("201 Created", "", ""),
            ("204 No Content", "", ""),
            ("207 Multi-Status", "", NEXTCLOUD),
        ]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), None).unwrap();
        let dir = std::env::temp_dir().join(format!("ferry-dav-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (to, from) = (dir.join("got"), dir.join("send"));
        let mut seen = 0;
        let v = dav
            .read_to(
                "/café.epub",
                &to,
                &mut |n| seen += n,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            (std::fs::read(&to).unwrap(), seen, v.as_str()),
            (b"the file's bytes".to_vec(), 16, "\"abc123\"")
        );
        std::fs::write(&from, b"sent from a file").unwrap();
        assert_eq!(
            dav.replace_from("/café.epub", &from, true).unwrap(),
            "\"abc123\""
        );
        std::fs::remove_dir_all(&dir).unwrap();
        let reqs = asked.join().unwrap();
        assert!(
            first(&reqs[3]).starts_with("PUT /remote.php/dav/files/me/.caf%C3%A9.epub.ferry-"),
            "{}",
            reqs[3]
        );
        assert!(
            has_header(&reqs[3], "Content-Length: 16") && reqs[3].ends_with("sent from a file"),
            "{}",
            reqs[3]
        );
        assert!(first(&reqs[4]).starts_with("MOVE "), "{}", reqs[4]);
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
        assert!(matches!(
            dav.rename("/a/b.txt", "x"),
            Err(DevError::Conflict(_))
        ));
        let reqs = asked.join().unwrap();
        assert!(
            first(&reqs[1]).starts_with("DELETE /dav/old/ "),
            "{}",
            reqs[1]
        );
        assert!(
            first(&reqs[2]).starts_with("MKCOL /dav/new/ "),
            "{}",
            reqs[2]
        );
        assert!(
            first(&reqs[4]).starts_with("MOVE /dav/a/b.txt "),
            "{}",
            reqs[4]
        );
        assert!(has_header(&reqs[4], "Overwrite: F"), "{}", reqs[4]);
        assert!(
            reqs[4].contains(&format!("Destination: {at}/dav/a/c%20d.txt")),
            "{}",
            reqs[4]
        );
    }

    const NONCE: &str = "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v";
    const CNONCE: &str = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";

    fn challenge(algo: &str) -> Digest {
        let line = format!("Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm={algo}, nonce=\"{NONCE}\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"");
        digest_challenge(&[&line]).unwrap()
    }

    #[test]
    fn digest_matches_the_rfc_examples() {
        // RFC 7616, section 3.9.
        let h = challenge("SHA-256").header_with(
            "Mufasa",
            "Circle of Life",
            "GET",
            "/dir/index.html",
            1,
            CNONCE,
        );
        assert!(
            h.contains(
                "response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""
            ),
            "{h}"
        );
        assert!(
            h.contains("algorithm=SHA-256, ")
                && h.contains("qop=auth, nc=00000001")
                && h.contains("opaque=\"FQhe/"),
            "{h}"
        );
        let h = challenge("MD5").header_with(
            "Mufasa",
            "Circle of Life",
            "GET",
            "/dir/index.html",
            1,
            CNONCE,
        );
        assert!(
            h.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\""),
            "{h}"
        );
    }

    #[test]
    fn reads_digest_challenges() {
        let both = [
            "Basic realm=\"x\"",
            "Digest realm=\"a, b\", nonce=\"n1\", algorithm=MD5",
            "Digest realm=\"a, b\", nonce=\"n2\", algorithm=SHA-256, stale=TRUE",
        ];
        let d = digest_challenge(&both).unwrap();
        assert_eq!(
            (d.realm.as_str(), d.nonce.as_str(), d.sha256, d.stale, d.qop),
            ("a, b", "n2", true, true, false)
        );
        assert!(digest_challenge(&["Basic realm=\"x\""]).is_none());
        assert!(
            digest_challenge(&["Digest realm=\"x\", nonce=\"n\", algorithm=SHA-512-256"]).is_none()
        );
    }

    /// The `key="value"` parts of an Authorization header the client sent.
    fn sent_params(req: &str) -> Vec<(String, String)> {
        let line = req
            .lines()
            .find(|l| l.starts_with("Authorization: Digest "))
            .unwrap_or_else(|| panic!("no Digest in {req}"));
        params(&line["Authorization: Digest ".len()..])
    }

    #[test]
    fn logs_in_with_digest_and_retries_when_stale() {
        let (asked, at) = serve(vec![
            ("401 Unauthorized", "WWW-Authenticate: Digest realm=\"nas\", nonce=\"n1\", qop=\"auth\", algorithm=SHA-256\r\n", ""),
            ("207 Multi-Status", "", NEXTCLOUD),
            ("401 Unauthorized", "WWW-Authenticate: Digest realm=\"nas\", nonce=\"n2\", qop=\"auth\", algorithm=SHA-256, stale=true\r\n", ""),
            ("207 Multi-Status", "", NEXTCLOUD),
        ]);
        let dav = Dav::open(&format!("{at}/remote.php/dav/files/me"), Some(("me", "pw"))).unwrap();
        dav.listdir("/").unwrap();
        let reqs = asked.join().unwrap();
        // Basic first, then Digest.
        assert!(
            has_header(&reqs[0], "Authorization: Basic bWU6cHc="),
            "{}",
            reqs[0]
        );
        for (i, nonce) in [(1, "n1"), (3, "n2")] {
            let p = sent_params(&reqs[i]);
            let get = |k: &str| {
                p.iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.clone())
                    .unwrap()
            };
            assert_eq!(
                (get("username"), get("realm"), get("nonce"), get("qop")),
                ("me".into(), "nas".into(), nonce.into(), "auth".into())
            );
            assert_eq!(get("algorithm"), "SHA-256");
            // The server could work out the same answer from what was sent.
            let d = Digest {
                realm: "nas".into(),
                nonce: nonce.into(),
                opaque: None,
                sha256: true,
                sess: false,
                qop: true,
                stale: false,
                count: 0,
            };
            let want = d.header_with(
                "me",
                "pw",
                reqs[i].split(' ').next().unwrap(),
                &get("uri"),
                u32::from_str_radix(&get("nc"), 16).unwrap(),
                &get("cnonce"),
            );
            assert_eq!(
                sent_params(&reqs[i]),
                params(want.strip_prefix("Digest ").unwrap()),
                "{}",
                reqs[i]
            );
            assert!(get("uri").starts_with("/remote.php/dav/files/me/"));
        }
        // The first Digest request used nc 1, the next 2 (the retry after the stale nonce, 1 again).
        assert!(sent_params(&reqs[1]).contains(&("nc".to_string(), "00000001".to_string())));
        assert!(sent_params(&reqs[2]).contains(&("nc".to_string(), "00000002".to_string())));
        assert!(sent_params(&reqs[3]).contains(&("nc".to_string(), "00000001".to_string())));
    }

    #[test]
    fn a_wrong_digest_password_fails_once() {
        let ch = "WWW-Authenticate: Digest realm=\"nas\", nonce=\"n1\", qop=\"auth\"\r\n";
        let (asked, at) = serve(vec![
            ("401 Unauthorized", ch, ""),
            ("401 Unauthorized", ch, ""),
        ]);
        let e = Dav::open(&at, Some(("me", "bad"))).err().unwrap();
        assert!(e.contains("user name and password"), "{e}");
        assert_eq!(asked.join().unwrap().len(), 2);
    }

    #[test]
    fn follows_redirects_keeping_method_and_body() {
        let (asked, at) = serve(vec![
            ("207 Multi-Status", "", NEXTCLOUD),
            ("307 Temporary Redirect", "Location: /moved/a.txt\r\n", ""),
            ("308 Permanent Redirect", "Location: b.txt\r\n", ""),
            ("201 Created", "", ""),
            ("307 Temporary Redirect", "Location: /moved/new\r\n", ""),
            ("201 Created", "", ""),
        ]);
        let dav = Dav::open(&format!("{at}/dav"), Some(("me", "pw"))).unwrap();
        // Two hops for the PUT, and the MOVE's Destination stays on the same server.
        dav.send("PUT", &format!("{at}/dav/a.txt"), &[], b"body")
            .unwrap();
        dav.move_to("/x", "/y", true).unwrap();
        let reqs = asked.join().unwrap();
        assert!(
            first(&reqs[2]).starts_with("PUT /moved/a.txt "),
            "{}",
            reqs[2]
        );
        assert!(
            first(&reqs[3]).starts_with("PUT /moved/b.txt "),
            "{}",
            reqs[3]
        );
        assert!(
            reqs[3].ends_with("body") && has_header(&reqs[3], "Authorization: Basic bWU6cHc="),
            "{}",
            reqs[3]
        );
        assert!(
            first(&reqs[5]).starts_with("MOVE /moved/new "),
            "{}",
            reqs[5]
        );
    }

    #[test]
    fn redirects_stay_on_the_server() {
        assert!(may_follow("http://h/a", "http://h/b"));
        assert!(may_follow("http://h/a", "https://H/b"));
        assert!(may_follow("https://h:8443/a", "https://h:8443/b"));
        assert!(!may_follow("https://h/a", "http://h/b"));
        assert!(!may_follow("http://h/a", "http://other/b"));
        assert!(!may_follow("http://h:1/a", "http://h:2/b"));
        assert_eq!(
            resolve("http://h:1/dav/x/y", "z").unwrap(),
            "http://h:1/dav/x/z"
        );
        assert_eq!(
            resolve("http://h:1/dav/x/y?q", "/root").unwrap(),
            "http://h:1/root"
        );
        assert_eq!(resolve("http://h/a", "https://o/b").unwrap(), "https://o/b");
    }

    #[test]
    fn gives_up_on_loops_and_other_servers() {
        let (asked, at) = serve(vec![
            ("307 Temporary Redirect", "Location: /again\r\n", "");
            6
        ]);
        let e = Dav::open(&at, None).err().unwrap();
        assert!(e.contains("sent us to"), "{e}");
        assert_eq!(asked.join().unwrap().len(), 6);
    }

    #[test]
    fn says_what_went_wrong() {
        let (asked, at) = serve(vec![("401 Unauthorized", "", "")]);
        let e = Dav::open(&at, Some(("me", "bad"))).err().unwrap();
        assert!(e.contains("user name and password"), "{e}");
        asked.join().unwrap();
        let (asked, at) = serve(vec![("401 Unauthorized", "", "")]);
        assert!(Dav::open(&at, None)
            .err()
            .unwrap()
            .contains("asks for a user name"));
        asked.join().unwrap();
        let (asked, at) = serve(vec![("404 Not Found", "", "")]);
        assert!(Dav::open(&at, None)
            .err()
            .unwrap()
            .contains("isn't a WebDAV folder"));
        asked.join().unwrap();
        let (asked, at) = serve(vec![(
            "301 Moved Permanently",
            "Location: https://x.example/dav/\r\n",
            "",
        )]);
        assert!(Dav::open(&at, None)
            .err()
            .unwrap()
            .contains("https://x.example/dav/"));
        asked.join().unwrap();
        let (asked, at) = serve(vec![("200 OK", "", "<html>hi</html>")]);
        assert!(Dav::open(&at, None)
            .err()
            .unwrap()
            .contains("isn't a WebDAV folder"));
        asked.join().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(Dav::open(&format!("http://127.0.0.1:{port}"), None)
            .err()
            .unwrap()
            .starts_with("Can't reach"));
    }
}
