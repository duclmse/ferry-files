//! FTP servers as drives, plain or over TLS (explicit FTPS, AUTH TLS). One control connection
//! is kept and started again when the server has dropped it. Listings use MLSD where the server
//! has it and LIST otherwise. Paths are the server's own ("/pub/a.txt").

use crate::device::{basename, dirname, join, Result, Transport};
use crate::devices::connect;
use crate::*;
use std::io::{Cursor, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use suppaftp::list::{File, ListParser};
use suppaftp::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use suppaftp::rustls::client::WebPkiServerVerifier;
use suppaftp::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use suppaftp::rustls::{self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use suppaftp::types::FileType;
use suppaftp::{
    FtpError, FtpStream, ImplFtpStream, RustlsConnector, RustlsFtpStream, Status, TlsStream,
};

mod tls;
use tls::*;

/// Names the TLS-stream type inside a client type (the crate doesn't export it itself).
trait Flavor {
    type T: TlsStream + Send + 'static;
}

impl<T: TlsStream + Send + 'static> Flavor for ImplFtpStream<T> {
    type T = T;
}

/// The stream types of plain and of TLS clients.
type NoTlsStream = <FtpStream as Flavor>::T;
type RustlsStream = <RustlsFtpStream as Flavor>::T;

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const IO_WAIT: Duration = Duration::from_secs(60);

/// What went wrong in one step: the server's or library's error, or our own message.
enum Fail {
    Ftp(FtpError),
    Msg(String),
}

impl From<FtpError> for Fail {
    fn from(e: FtpError) -> Fail {
        Fail::Ftp(e)
    }
}

/// The server's reply code when it answered with an error.
fn code(e: &FtpError) -> Option<u32> {
    match e {
        FtpError::UnexpectedResponse(r) => Some(r.status.code()),
        _ => None,
    }
}

impl Fail {
    /// The connection is gone (or the server said it is closing it).
    fn dropped(&self) -> bool {
        match self {
            Fail::Ftp(
                FtpError::ConnectionError(_) | FtpError::BadResponse | FtpError::SecureError(_),
            ) => true,
            Fail::Ftp(e) => code(e) == Some(421),
            Fail::Msg(_) => false,
        }
    }

    /// The control connection may be out of step, so it isn't used again.
    fn breaks(&self) -> bool {
        matches!(self, Fail::Ftp(e) if code(e).is_none())
    }

    fn say(&self, path: &str) -> String {
        match self {
            Fail::Msg(m) => m.clone(),
            Fail::Ftp(e) => say(e, path),
        }
    }
}

/// An error in words that say what to do. `path` is what was asked for ("" when none).
fn say(e: &FtpError, path: &str) -> String {
    let what = if path.is_empty() {
        String::new()
    } else {
        format!("{path}: ")
    };
    match e {
        FtpError::UnexpectedResponse(r) => {
            let text = r.as_string().unwrap_or_default();
            let text = text.trim();
            match r.status.code() {
                530 => "Wrong user name or password.".into(),
                421 => "The server closed the connection.".into(),
                550 => format!("{what}not found, or the server won't allow it. {text}")
                    .trim()
                    .to_string(),
                552 | 452 => "The server is out of space.".into(),
                553 => format!("{what}the server doesn't allow that name."),
                c => format!("{what}the server said {c} {text}")
                    .trim()
                    .to_string(),
            }
        }
        FtpError::ConnectionError(_) => "Lost the connection to the server.".into(),
        FtpError::SecureError(m) => format!("The secure connection failed: {m}"),
        FtpError::BadResponse => "The server sent an answer that isn't FTP.".into(),
        e => e.to_string(),
    }
}

/// "host" or "host:port" (also "[::1]:21" and "ftp://host") as host and port.
fn split_server(server: &str) -> std::result::Result<(String, u16), String> {
    let s = server.trim().trim_end_matches('/');
    let s = s
        .strip_prefix("ftp://")
        .or_else(|| s.strip_prefix("ftps://"))
        .unwrap_or(s);
    if s.is_empty() || s.contains('/') {
        return Err(
            "Type the address of the FTP server, like ftp.example.com or 192.168.1.5:2121.".into(),
        );
    }
    let bad_port = || "The port after the colon should be a number.".to_string();
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or("Missing ] in the address.")?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad_port())?,
            None if tail.is_empty() => 21,
            None => return Err("Type the address of the FTP server, like ftp.example.com.".into()),
        };
        return Ok((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        // More than one colon without brackets is a bare IPv6 address.
        Some((host, _)) if host.contains(':') => Ok((s.to_string(), 21)),
        Some((host, port)) => Ok((host.to_string(), port.parse().map_err(|_| bad_port())?)),
        None => Ok((s.to_string(), 21)),
    }
}

/// Seconds since 1970 of a timestamp as MDTM and MLSD write it ("20240102030405" or with ".123").
fn parse_stamp(s: &str) -> Option<i64> {
    let digits = s.trim().split('.').next()?;
    if digits.len() != 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |a: usize, b: usize| digits[a..b].parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(4, 6)?, n(6, 8)?);
    let (h, mi, sec) = (n(8, 10)?, n(10, 12)?, n(12, 14)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days since 1970 (Howard Hinnant's civil-from-days, reversed).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One listed name; `link` when the server says it is a symbolic link.
struct Item {
    entry: Entry,
    link: bool,
}

fn item(dir: &str, f: &File) -> Item {
    let dir_like = f.is_directory();
    Item {
        entry: Entry {
            name: f.name().to_string(),
            path: join(dir, f.name()),
            dir: dir_like,
            size: if dir_like { 0 } else { f.size() as u64 },
            mtime: secs(f.modified()),
            hidden: false,
        },
        link: f.is_symlink(),
    }
}

/// The names in MLSD lines. Lines for the folder itself ("cdir", "pdir") are left out.
fn parse_mlsd(dir: &str, lines: &[String]) -> Vec<Item> {
    lines
        .iter()
        .filter(|l| {
            let facts = l.split("; ").next().unwrap_or("").to_lowercase();
            !(facts.contains("type=cdir") || facts.contains("type=pdir"))
        })
        .filter_map(|l| ListParser::parse_mlsd(l).ok())
        .filter(|f| f.name() != "." && f.name() != "..")
        .map(|f| item(dir, &f))
        .collect()
}

/// The names in LIST lines, Unix `ls -l` style or Windows/IIS style. Other lines ("total 8") are skipped.
fn parse_list(dir: &str, lines: &[String]) -> Vec<Item> {
    lines
        .iter()
        .filter_map(|l| {
            ListParser::parse_posix(l)
                .or_else(|_| ListParser::parse_dos(l))
                .ok()
        })
        .filter(|f| f.name() != "." && f.name() != "..")
        .map(|f| item(dir, &f))
        .collect()
}

/// A logged-in control connection and what the server can do.
struct Session<T: TlsStream> {
    ftp: ImplFtpStream<T>,
    /// MLSD and MLST are offered (from FEAT).
    mlsx: bool,
    /// The connection may be out of step; it's dropped after this call.
    broken: bool,
}

type Dial<T> = Box<dyn Fn() -> std::result::Result<(Session<T>, String), String> + Send + Sync>;

pub struct Ftp<T: TlsStream> {
    host: String,
    home: String,
    session: Mutex<Option<Session<T>>>,
    dial: Dial<T>,
}

fn tcp(host: &str, port: u16) -> std::result::Result<TcpStream, String> {
    let cant = || {
        format!("Couldn't connect to {host}:{port}. Check the address and that the server is on.")
    };
    let addrs = (host, port).to_socket_addrs().map_err(|_| cant())?;
    for a in addrs {
        if let Ok(s) = TcpStream::connect_timeout(&a, CONNECT_WAIT) {
            let _ = s.set_read_timeout(Some(IO_WAIT));
            let _ = s.set_write_timeout(Some(IO_WAIT));
            let _ = s.set_nodelay(true);
            return Ok(s);
        }
    }
    Err(cant())
}

/// Logs in and sets up the session: binary type, UTF-8 names, what the server offers.
fn start<T: TlsStream>(
    mut ftp: ImplFtpStream<T>,
    user: &str,
    password: &str,
) -> std::result::Result<(Session<T>, String), String> {
    ftp.login(user, password).map_err(|e| match &e {
        FtpError::UnexpectedResponse(r) if r.status.code() == 530 && user == "anonymous" => {
            "This server doesn't allow anonymous login. Add a user name.".to_string()
        }
        e => say(e, ""),
    })?;
    ftp.transfer_type(FileType::Binary)
        .map_err(|e| say(&e, ""))?;
    let feat = ftp.feat().unwrap_or_default();
    let has = |k: &str| feat.keys().any(|f| f.eq_ignore_ascii_case(k));
    if has("UTF8") {
        let _ = ftp.opts("UTF8", Some("ON"));
    }
    let home = ftp
        .pwd()
        .ok()
        .filter(|p| p.starts_with('/'))
        .unwrap_or_else(|| "/".into());
    Ok((
        Session {
            ftp,
            mlsx: has("MLST"),
            broken: false,
        },
        home,
    ))
}

/// A passive-mode data connection that gives up after a while.
fn data_stream(addr: std::net::SocketAddr) -> suppaftp::FtpResult<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, CONNECT_WAIT).map_err(FtpError::ConnectionError)?;
    let _ = s.set_read_timeout(Some(IO_WAIT));
    let _ = s.set_write_timeout(Some(IO_WAIT));
    Ok(s)
}

fn dial_plain(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
) -> std::result::Result<(Session<NoTlsStream>, String), String> {
    let ftp = FtpStream::connect_with_stream(tcp(host, port)?).map_err(|e| say(&e, ""))?;
    start(ftp.passive_stream_builder(data_stream), user, password)
}

fn dial_tls(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    (cfg, why): (Arc<ClientConfig>, Why),
) -> std::result::Result<(Session<RustlsStream>, String), String> {
    let ftp = RustlsFtpStream::connect_with_stream(tcp(host, port)?).map_err(|e| say(&e, ""))?;
    *why.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // The password goes only after the control connection is secured; no fall back to plain.
    let ftp = ftp
        .into_secure(RustlsConnector::from(cfg), host)
        .map_err(|e| {
            // A certificate that was refused says why in its own words.
            if let Some(m) = why.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return m;
            }
            match e {
                FtpError::UnexpectedResponse(_) => "The server doesn't offer FTPS.".to_string(),
                FtpError::SecureError(m) => format!("Couldn't set up TLS with the server: {m}"),
                e => say(&e, ""),
            }
        })?;
    start(ftp.passive_stream_builder(data_stream), user, password)
}

impl<T: TlsStream + Send + 'static> Ftp<T> {
    fn new(host: String, dial: Dial<T>) -> std::result::Result<Self, String> {
        let (s, home) = dial()?;
        Ok(Ftp {
            host,
            home,
            session: Mutex::new(Some(s)),
            dial,
        })
    }

    /// Runs `f` on the connection. When the server has dropped it, connects again and runs `f`
    /// once more.
    fn with<R>(
        &self,
        path: &str,
        mut f: impl FnMut(&mut Session<T>) -> std::result::Result<R, Fail>,
    ) -> Result<R> {
        let mut g = self.session.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let fresh = g.is_none();
            if fresh {
                *g = Some((self.dial)().map_err(DevError::Msg)?.0);
            }
            let s = g.as_mut().expect("connected above");
            s.broken = false;
            let r = f(s);
            let broken = s.broken;
            match r {
                Ok(v) => {
                    if broken {
                        *g = None;
                    }
                    return Ok(v);
                }
                Err(e) => {
                    if broken || e.breaks() {
                        *g = None;
                    }
                    if e.dropped() && !fresh {
                        continue;
                    }
                    return Err(DevError::Msg(e.say(path)));
                }
            }
        }
    }

    /// The folder's names, as the server lists them (links not followed).
    fn items(s: &mut Session<T>, path: &str) -> std::result::Result<Vec<Item>, Fail> {
        if s.mlsx {
            match s.ftp.mlsd(Some(path)) {
                Ok(lines) => return Ok(parse_mlsd(path, &lines)),
                Err(e) if matches!(code(&e), Some(500..=504)) => s.mlsx = false,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(parse_list(path, &s.ftp.list(Some(path))?))
    }

    fn entries(s: &mut Session<T>, path: &str) -> std::result::Result<Vec<Entry>, Fail> {
        let mut out = vec![];
        for mut it in Self::items(s, path)? {
            // A link is listed as what it points at: a folder when the server can enter it.
            if it.link {
                it.entry.dir = s.ftp.cwd(&it.entry.path).is_ok();
            }
            out.push(it.entry);
        }
        Ok(out)
    }

    fn mdtm(s: &mut Session<T>, path: &str) -> i64 {
        let r = s
            .ftp
            .custom_command(format!("MDTM {path}"), &[Status::File]);
        r.ok()
            .and_then(|r| r.as_string().ok())
            .and_then(|t| parse_stamp(t.trim().split(' ').next_back().unwrap_or("")))
            .unwrap_or(0)
    }

    fn stat_of(s: &mut Session<T>, path: &str) -> std::result::Result<Option<Entry>, Fail> {
        if path == "/" {
            return Ok(Some(Entry {
                name: "/".into(),
                path: "/".into(),
                dir: true,
                size: 0,
                mtime: 0,
                hidden: false,
            }));
        }
        let mut found = None;
        if s.mlsx {
            if let Ok(line) = s.ftp.mlst(Some(path)) {
                if let Ok(f) = ListParser::parse_mlst(&line) {
                    let mut it = item(&dirname(path), &f);
                    it.entry.name = basename(path).to_string();
                    it.entry.path = path.to_string();
                    found = Some(it.entry);
                }
            }
        }
        if found.is_none() {
            let parent = dirname(path);
            let list = match Self::entries(s, &parent) {
                Ok(l) => l,
                Err(Fail::Ftp(FtpError::UnexpectedResponse(r))) if r.status.code() == 550 => {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            };
            found = list.into_iter().find(|e| e.name == basename(path));
        }
        let Some(mut e) = found else { return Ok(None) };
        if e.mtime == 0 && !e.dir {
            e.mtime = Self::mdtm(s, path);
        }
        Ok(Some(e))
    }

    fn version_of(e: &Entry) -> String {
        format!("{}-{}", e.size, e.mtime)
    }

    /// Writes `size` bytes, sent by `send` to the name it's given, under a temporary name, then
    /// puts them in place of `path`. Gives the new version.
    fn put(
        &self,
        path: &str,
        exists: bool,
        size: u64,
        mut send: impl FnMut(&mut Session<T>, &str) -> std::result::Result<(), Fail>,
    ) -> Result<String> {
        let tmp = join(&dirname(path), &format!(".{}.ferry-tmp", basename(path)));
        let backup = join(&dirname(path), &format!(".{}.ferry-old", basename(path)));
        self.with(path, |s| {
            send(s, &tmp)?;
            // The new file goes in place by renaming over the old one: the old one is never gone
            // unless the server can't do that.
            if let Err(e) = s.ftp.rename(tmp.as_str(), path) {
                if !exists {
                    let _ = s.ftp.rm(&tmp);
                    return Err(e.into());
                }
                // Many servers refuse to rename over a file. The old one waits under another
                // name, and comes back if the new one can't be put in place.
                let _ = s.ftp.rm(&backup);
                if let Err(e) = s.ftp.rename(path, backup.as_str()) {
                    let _ = s.ftp.rm(&tmp);
                    return Err(e.into());
                }
                if let Err(e) = s.ftp.rename(tmp.as_str(), path) {
                    let _ = s.ftp.rename(backup.as_str(), path);
                    let _ = s.ftp.rm(&tmp);
                    return Err(e.into());
                }
                let _ = s.ftp.rm(&backup);
            }
            let e = Self::stat_of(s, path)?
                .ok_or_else(|| Fail::Msg(format!("{path}: gone after writing it.")))?;
            if e.size != size {
                return Err(Fail::Msg(format!(
                    "{path}: wrote {size} bytes but the file has {}.",
                    e.size
                )));
            }
            Ok(Self::version_of(&e))
        })
    }

    /// Removes a folder's contents and then the folder. Links are removed, not followed.
    fn remove_tree(s: &mut Session<T>, path: &str) -> std::result::Result<(), Fail> {
        for it in Self::items(s, path)? {
            if it.entry.dir && !it.link {
                Self::remove_tree(s, &it.entry.path)?;
            } else {
                s.ftp.rm(&it.entry.path)?;
            }
        }
        s.ftp.rmdir(path)?;
        Ok(())
    }
}

impl<T: TlsStream + Send + 'static> Transport for Ftp<T> {
    fn kind(&self) -> &'static str {
        "ftp"
    }

    fn label(&self) -> String {
        self.host.clone()
    }

    fn default_dir(&self) -> String {
        self.home.clone()
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        vec![]
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        self.with(path, |s| Self::entries(s, path))
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        self.with(path, |s| Self::stat_of(s, path))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        self.with(path, |s| {
            let e = Self::stat_of(s, path)?
                .ok_or_else(|| Fail::Msg(format!("{path}: no such file.")))?;
            let data = s.ftp.retr(path, |r| {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf).map_err(FtpError::ConnectionError)?;
                Ok(buf)
            })?;
            Ok((data, Self::version_of(&e)))
        })
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(vec![]);
        }
        self.with(path, |s| {
            s.ftp.resume_transfer(start as usize)?;
            let mut stream = s.ftp.retr_as_stream(path)?;
            let mut buf = Vec::new();
            let got = (&mut stream).take(len).read_to_end(&mut buf);
            if let Err(e) = got {
                s.broken = true;
                return Err(Fail::Ftp(FtpError::ConnectionError(e)));
            }
            if (buf.len() as u64) < len {
                // The file ended: the server completes the transfer by itself.
                stream.finish()?;
            } else if s.ftp.abort(stream).is_err() {
                // The server answered the abort oddly; start a fresh connection next time.
                s.broken = true;
            }
            Ok(buf)
        })
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        self.with(path, |s| {
            Ok(Self::stat_of(s, path)?.map(|e| Self::version_of(&e)))
        })
    }

    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        self.put(path, exists, data.len() as u64, |s, tmp| {
            s.ftp.put_file(tmp, &mut Cursor::new(data))?;
            Ok(())
        })
    }

    fn replace_from(&self, path: &str, from: &std::path::Path, exists: bool) -> Result<String> {
        let size = std::fs::metadata(from)
            .map_err(|e| DevError::Msg(format!("Can't read {}: {e}", from.display())))?
            .len();
        self.put(path, exists, size, |s, tmp| {
            // Opened again if the connection drops and the upload starts over.
            let mut f = std::fs::File::open(from)
                .map_err(|e| Fail::Msg(format!("Can't read {}: {e}", from.display())))?;
            s.ftp.put_file(tmp, &mut f)?;
            Ok(())
        })
    }

    fn read_to(
        &self,
        path: &str,
        to: &std::path::Path,
        read: &mut dyn FnMut(u64),
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<String> {
        self.with(path, |s| {
            let e = Self::stat_of(s, path)?
                .ok_or_else(|| Fail::Msg(format!("{path}: no such file.")))?;
            let mut stream = s.ftp.retr_as_stream(path)?;
            if let Err(err) = crate::device::pour(&mut stream, to, path, read, stop) {
                // Abort, or dropping the stream waits for the end of a transfer that won't come;
                // and start a fresh connection next time, in case the server answers it oddly.
                let _ = s.ftp.abort(stream);
                s.broken = true;
                return Err(Fail::Msg(err.to_string()));
            }
            stream.finish()?;
            let got = std::fs::metadata(to).map(|m| m.len()).unwrap_or(0);
            if got != e.size {
                return Err(Fail::Msg(format!(
                    "{path}: read {got} bytes but the file has {}.",
                    e.size
                )));
            }
            Ok(Self::version_of(&e))
        })
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        self.with(path, |s| {
            if is_dir {
                Self::remove_tree(s, path)
            } else {
                Ok(s.ftp.rm(path)?)
            }
        })
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        self.with(path, |s| Ok(s.ftp.mkdir(path)?))
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        let to = join(&dirname(path), new_name);
        self.with(path, |s| Ok(s.ftp.rename(path, &to)?))
    }
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn ftp_connect(
    state: State<'_, AppState>,
    server: String,
    tls: bool,
    trust: Option<bool>,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let (host, port) = split_server(&server)?;
    let user = user.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
    // The address as the app gave it, which is how it finds the connection again.
    let usb = Some(format!(
        "ftp:{}{}",
        user.as_ref().map(|u| format!("{u}@")).unwrap_or_default(),
        server.trim()
    ));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("{host}:{port} is already added."));
    }
    let dev = blocking(move || {
        let password = match &user {
            Some(_) => secret
                .as_deref()
                .and_then(crate::servers::secrets::get)
                .unwrap_or_default(),
            None => "anonymous@".to_string(),
        };
        let user = user.unwrap_or_else(|| "anonymous".into());
        let t: Box<dyn Transport> = if tls {
            let tc = tls_config(trust.unwrap_or(false), &host, port, pin_file())?;
            let h = host.clone();
            Box::new(Ftp::new(
                host.clone(),
                Box::new(move || dial_tls(&h, port, &user, &password, tc.clone())),
            )?)
        } else {
            let h = host.clone();
            Box::new(Ftp::new(
                host.clone(),
                Box::new(move || dial_plain(&h, port, &user, &password)),
            )?)
        };
        Ok::<_, String>(Device::new(t))
    })
    .await?;
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn {
        dev: Arc::new(dev),
        usb,
        serial: None,
    };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests;
