//! FTP servers as drives, plain or over TLS (explicit FTPS, AUTH TLS). One control connection
//! is kept and started again when the server has dropped it. Listings use MLSD where the server
//! has it and LIST otherwise. Paths are the server's own ("/pub/a.txt").

use super::*;
use crate::device::{basename, dirname, join, Result, Transport};
use std::io::{Cursor, Read};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use suppaftp::list::{File, ListParser};
use suppaftp::rustls::{self, ClientConfig, RootCertStore};
use suppaftp::types::FileType;
use suppaftp::{FtpError, FtpStream, ImplFtpStream, RustlsConnector, RustlsFtpStream, Status, TlsStream};

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
            Fail::Ftp(FtpError::ConnectionError(_) | FtpError::BadResponse | FtpError::SecureError(_)) => true,
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
    let what = if path.is_empty() { String::new() } else { format!("{path}: ") };
    match e {
        FtpError::UnexpectedResponse(r) => {
            let text = r.as_string().unwrap_or_default();
            let text = text.trim();
            match r.status.code() {
                530 => "Wrong user name or password.".into(),
                421 => "The server closed the connection.".into(),
                550 => format!("{what}not found, or the server won't allow it. {text}").trim().to_string(),
                552 | 452 => "The server is out of space.".into(),
                553 => format!("{what}the server doesn't allow that name."),
                c => format!("{what}the server said {c} {text}").trim().to_string(),
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
    let s = s.strip_prefix("ftp://").or_else(|| s.strip_prefix("ftps://")).unwrap_or(s);
    if s.is_empty() || s.contains('/') {
        return Err("Type the address of the FTP server, like ftp.example.com or 192.168.1.5:2121.".into());
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
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
        .filter_map(|l| ListParser::parse_posix(l).or_else(|_| ListParser::parse_dos(l)).ok())
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
    let cant = || format!("Couldn't connect to {host}:{port}. Check the address and that the server is on.");
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

fn tls_config() -> std::result::Result<Arc<ClientConfig>, String> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let cfg = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Logs in and sets up the session: binary type, UTF-8 names, what the server offers.
fn start<T: TlsStream>(mut ftp: ImplFtpStream<T>, user: &str, password: &str) -> std::result::Result<(Session<T>, String), String> {
    ftp.login(user, password).map_err(|e| match &e {
        FtpError::UnexpectedResponse(r) if r.status.code() == 530 && user == "anonymous" => {
            "This server doesn't allow anonymous login. Add a user name.".to_string()
        }
        e => say(e, ""),
    })?;
    ftp.transfer_type(FileType::Binary).map_err(|e| say(&e, ""))?;
    let feat = ftp.feat().unwrap_or_default();
    let has = |k: &str| feat.keys().any(|f| f.eq_ignore_ascii_case(k));
    if has("UTF8") {
        let _ = ftp.opts("UTF8", Some("ON"));
    }
    let home = ftp.pwd().ok().filter(|p| p.starts_with('/')).unwrap_or_else(|| "/".into());
    Ok((Session { ftp, mlsx: has("MLST"), broken: false }, home))
}

/// A passive-mode data connection that gives up after a while.
fn data_stream(addr: std::net::SocketAddr) -> suppaftp::FtpResult<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, CONNECT_WAIT).map_err(FtpError::ConnectionError)?;
    let _ = s.set_read_timeout(Some(IO_WAIT));
    let _ = s.set_write_timeout(Some(IO_WAIT));
    Ok(s)
}

fn dial_plain(host: &str, port: u16, user: &str, password: &str) -> std::result::Result<(Session<NoTlsStream>, String), String> {
    let ftp = FtpStream::connect_with_stream(tcp(host, port)?).map_err(|e| say(&e, ""))?;
    start(ftp.passive_stream_builder(data_stream), user, password)
}

fn dial_tls(host: &str, port: u16, user: &str, password: &str, cfg: Arc<ClientConfig>) -> std::result::Result<(Session<RustlsStream>, String), String> {
    let ftp = RustlsFtpStream::connect_with_stream(tcp(host, port)?).map_err(|e| say(&e, ""))?;
    // The password goes only after the control connection is secured; no fall back to plain.
    let ftp = ftp.into_secure(RustlsConnector::from(cfg), host).map_err(|e| match e {
        FtpError::UnexpectedResponse(_) => "The server doesn't offer FTPS.".to_string(),
        FtpError::SecureError(m) => format!("Couldn't set up TLS with the server: {m}"),
        e => say(&e, ""),
    })?;
    start(ftp.passive_stream_builder(data_stream), user, password)
}

impl<T: TlsStream + Send + 'static> Ftp<T> {
    fn new(host: String, dial: Dial<T>) -> std::result::Result<Self, String> {
        let (s, home) = dial()?;
        Ok(Ftp { host, home, session: Mutex::new(Some(s)), dial })
    }

    /// Runs `f` on the connection. When the server has dropped it, connects again and runs `f`
    /// once more.
    fn with<R>(&self, path: &str, mut f: impl FnMut(&mut Session<T>) -> std::result::Result<R, Fail>) -> Result<R> {
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
        let r = s.ftp.custom_command(format!("MDTM {path}"), &[Status::File]);
        r.ok().and_then(|r| r.as_string().ok()).and_then(|t| parse_stamp(t.trim().split(' ').next_back().unwrap_or(""))).unwrap_or(0)
    }

    fn stat_of(s: &mut Session<T>, path: &str) -> std::result::Result<Option<Entry>, Fail> {
        if path == "/" {
            return Ok(Some(Entry { name: "/".into(), path: "/".into(), dir: true, size: 0, mtime: 0 }));
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
                Err(Fail::Ftp(FtpError::UnexpectedResponse(r))) if r.status.code() == 550 => return Ok(None),
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
            let e = Self::stat_of(s, path)?.ok_or_else(|| Fail::Msg(format!("{path}: no such file.")))?;
            let data = s
                .ftp
                .retr(path, |r| {
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
        self.with(path, |s| Ok(Self::stat_of(s, path)?.map(|e| Self::version_of(&e))))
    }

    fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> {
        let tmp = join(&dirname(path), &format!(".{}.ferry-tmp", basename(path)));
        self.with(path, |s| {
            s.ftp.put_file(&tmp, &mut Cursor::new(data))?;
            // Many servers refuse to rename over a file, so the old one goes first if that fails.
            let mut renamed = s.ftp.rename(tmp.as_str(), path);
            if renamed.is_err() && exists && s.ftp.rm(path).is_ok() {
                renamed = s.ftp.rename(tmp.as_str(), path);
            }
            if let Err(e) = renamed {
                let _ = s.ftp.rm(&tmp);
                return Err(e.into());
            }
            let e = Self::stat_of(s, path)?.ok_or_else(|| Fail::Msg(format!("{path}: gone after writing it.")))?;
            if e.size != data.len() as u64 {
                return Err(Fail::Msg(format!("{path}: wrote {} bytes but the file has {}.", data.len(), e.size)));
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
pub(super) async fn ftp_connect(
    state: State<'_, AppState>,
    server: String,
    tls: bool,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let (host, port) = split_server(&server)?;
    let user = user.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
    // The address as the app gave it, which is how it finds the connection again.
    let usb = Some(format!("ftp:{}{}", user.as_ref().map(|u| format!("{u}@")).unwrap_or_default(), server.trim()));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("{host}:{port} is already added."));
    }
    let dev = blocking(move || {
        let password = match &user {
            Some(_) => secret.as_deref().and_then(crate::secrets::get).unwrap_or_default(),
            None => "anonymous@".to_string(),
        };
        let user = user.unwrap_or_else(|| "anonymous".into());
        let t: Box<dyn Transport> = if tls {
            let cfg = tls_config()?;
            let h = host.clone();
            Box::new(Ftp::new(host.clone(), Box::new(move || dial_tls(&h, port, &user, &password, cfg.clone())))?)
        } else {
            let h = host.clone();
            Box::new(Ftp::new(host.clone(), Box::new(move || dial_plain(&h, port, &user, &password)))?)
        };
        Ok::<_, String>(Device::new(t))
    })
    .await?;
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn { dev: Arc::new(dev), usb, serial: None };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn addresses() {
        assert_eq!(split_server("ftp.example.com").unwrap(), ("ftp.example.com".into(), 21));
        assert_eq!(split_server(" host:2121 ").unwrap(), ("host".into(), 2121));
        assert_eq!(split_server("ftp://host/").unwrap(), ("host".into(), 21));
        assert_eq!(split_server("[::1]:2121").unwrap(), ("::1".into(), 2121));
        assert_eq!(split_server("::1").unwrap(), ("::1".into(), 21));
        assert!(split_server("").is_err());
        assert!(split_server("host:abc").is_err());
        assert!(split_server("host/dir").is_err());
    }

    #[test]
    fn stamps() {
        assert_eq!(parse_stamp("19700101000000"), Some(0));
        assert_eq!(parse_stamp("20240102030405"), Some(1_704_164_645));
        assert_eq!(parse_stamp("20240102030405.123"), Some(1_704_164_645));
        assert_eq!(parse_stamp("2024"), None);
        assert_eq!(parse_stamp("20241302030405"), None);
    }

    #[test]
    fn unix_lists() {
        let l = lines(&[
            "total 12",
            "drwxr-xr-x   2 ftp ftp  4096 Jan 02  2024 My Folder",
            "-rw-r--r--   1 ftp ftp  1234 Jan 02  2024 my file é.txt",
            "lrwxrwxrwx   1 ftp ftp     5 Jan 02  2024 latest -> a.txt",
            "drwxr-xr-x   2 ftp ftp  4096 Jan 02  2024 .",
            "drwxr-xr-x   2 ftp ftp  4096 Jan 02  2024 ..",
        ]);
        let v = parse_list("/pub", &l);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].entry.name.as_str(), v[0].entry.dir, v[0].entry.path.as_str()), ("My Folder", true, "/pub/My Folder"));
        assert_eq!((v[1].entry.name.as_str(), v[1].entry.size, v[1].entry.dir), ("my file é.txt", 1234, false));
        assert!(v[1].entry.mtime > 0);
        assert!(v[2].link);
        assert_eq!(v[2].entry.name, "latest");
    }

    #[test]
    fn windows_lists() {
        let l = lines(&["10-19-20  03:19PM       <DIR>          pub", "04-08-14  03:09PM                  403 read me.txt"]);
        let v = parse_list("/", &l);
        assert_eq!(v.len(), 2);
        assert!(v[0].entry.dir);
        assert_eq!((v[1].entry.name.as_str(), v[1].entry.size, v[1].entry.dir), ("read me.txt", 403, false));
        assert!(v[1].entry.mtime > 0);
    }

    #[test]
    fn mlsd_lists() {
        let l = lines(&[
            "type=cdir;modify=20240102030405; /pub",
            "type=pdir;modify=20240102030405; /",
            "type=dir;modify=20240102030405;UNIX.mode=0755; sub dir",
            "Type=file;Size=12;Modify=20240102030405.500; a; b é.txt",
        ]);
        let v = parse_mlsd("/pub", &l);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].entry.name.as_str(), v[0].entry.dir, v[0].entry.mtime), ("sub dir", true, 1_704_164_645));
        assert_eq!((v[1].entry.name.as_str(), v[1].entry.size, v[1].entry.path.as_str()), ("a; b é.txt", 12, "/pub/a; b é.txt"));
    }

    #[test]
    fn errors_in_words() {
        let r = |code: u32, text: &str| FtpError::UnexpectedResponse(suppaftp::types::Response::new(Status::from(code), text.as_bytes().to_vec()));
        assert_eq!(say(&r(530, "Login incorrect"), ""), "Wrong user name or password.");
        assert!(say(&r(550, "No such file"), "/a").starts_with("/a: not found"));
        assert_eq!(say(&FtpError::ConnectionError(std::io::Error::other("x")), ""), "Lost the connection to the server.");
    }

    /// A small FTP server on 127.0.0.1 that keeps its files in memory.
    struct Fake {
        port: u16,
        files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
        dirs: Arc<Mutex<Vec<String>>>,
        /// The next command is answered by closing the connection.
        drop_next: Arc<AtomicBool>,
        logins: Arc<AtomicUsize>,
    }

    const STAMP: &str = "20240102030405";

    impl Fake {
        fn start(mlsd: bool) -> Fake {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let f = Fake {
                port: l.local_addr().unwrap().port(),
                files: Arc::default(),
                dirs: Arc::new(Mutex::new(vec!["/".into()])),
                drop_next: Arc::default(),
                logins: Arc::default(),
            };
            let (files, dirs, drop_next, logins) = (f.files.clone(), f.dirs.clone(), f.drop_next.clone(), f.logins.clone());
            std::thread::spawn(move || {
                for c in l.incoming().flatten() {
                    let (files, dirs, drop_next, logins) = (files.clone(), dirs.clone(), drop_next.clone(), logins.clone());
                    std::thread::spawn(move || serve(c, files, dirs, drop_next, logins, mlsd));
                }
            });
            f
        }

        fn put(&self, path: &str, data: &[u8]) {
            self.files.lock().unwrap().insert(path.into(), data.to_vec());
        }
    }

    fn say_to(w: &Mutex<TcpStream>, text: &str) {
        let _ = w.lock().unwrap().write_all(format!("{text}\r\n").as_bytes());
    }

    fn fact_line(name: &str, dir: bool, size: usize) -> String {
        if dir {
            format!("type=dir;modify={STAMP}; {name}")
        } else {
            format!("type=file;size={size};modify={STAMP}; {name}")
        }
    }

    /// The fake's LIST line in Unix style.
    fn unix_line(name: &str, dir: bool, size: usize) -> String {
        format!("{}rw-r--r-- 1 ftp ftp {size} Jan 02 2024 {name}", if dir { "d" } else { "-" })
    }

    fn children(files: &BTreeMap<String, Vec<u8>>, dirs: &[String], dir: &str, line: fn(&str, bool, usize) -> String) -> Vec<String> {
        let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
        let mut out = vec![];
        for (p, d) in files {
            if let Some(n) = p.strip_prefix(&prefix) {
                if !n.contains('/') {
                    out.push(line(n, false, d.len()));
                }
            }
        }
        for p in dirs {
            if let Some(n) = p.strip_prefix(&prefix) {
                if !n.is_empty() && !n.contains('/') {
                    out.push(line(n, true, 0));
                }
            }
        }
        out
    }

    fn serve(c: TcpStream, files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>, dirs: Arc<Mutex<Vec<String>>>, drop_next: Arc<AtomicBool>, logins: Arc<AtomicUsize>, mlsd: bool) {
        let w = Arc::new(Mutex::new(c.try_clone().unwrap()));
        let mut r = BufReader::new(c);
        let mut data: Option<TcpListener> = None;
        let mut rest = 0usize;
        let mut from = String::new();
        say_to(&w, "220 fake ready");
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if drop_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let l = line.trim_end();
            let (cmd, arg) = l.split_once(' ').unwrap_or((l, ""));
            match cmd.to_uppercase().as_str() {
                "USER" => say_to(&w, "331 password please"),
                "PASS" => {
                    logins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    say_to(&w, "230 ok")
                }
                "FEAT" => {
                    let mut t = "211-Features:\r\n UTF8\r\n".to_string();
                    if mlsd {
                        t += " MLST type*;size*;modify*;\r\n";
                    }
                    t += "211 End";
                    say_to(&w, &t)
                }
                "OPTS" | "TYPE" => say_to(&w, "200 ok"),
                "PWD" => say_to(&w, "257 \"/\" is the folder"),
                "PASV" => {
                    let l = TcpListener::bind("127.0.0.1:0").unwrap();
                    let p = l.local_addr().unwrap().port();
                    data = Some(l);
                    say_to(&w, &format!("227 Entering Passive Mode (127,0,0,1,{},{})", p / 256, p % 256));
                }
                "REST" => {
                    rest = arg.parse().unwrap();
                    say_to(&w, "350 restarting");
                }
                "MLSD" | "LIST" => {
                    let dir = if arg.is_empty() { "/" } else { arg };
                    if !dirs.lock().unwrap().iter().any(|d| d == dir) {
                        say_to(&w, "550 no such folder");
                        continue;
                    }
                    say_to(&w, "150 here it comes");
                    let (mut d, _) = data.take().unwrap().accept().unwrap();
                    let mut rows = children(&files.lock().unwrap(), &dirs.lock().unwrap(), dir, if cmd == "MLSD" { fact_line } else { unix_line });
                    if cmd == "MLSD" {
                        rows.insert(0, format!("type=cdir;modify={STAMP}; {dir}"));
                    }
                    for row in rows {
                        let _ = d.write_all(format!("{row}\r\n").as_bytes());
                    }
                    drop(d);
                    say_to(&w, "226 done");
                }
                "MLST" => {
                    let f = files.lock().unwrap();
                    match f.get(arg) {
                        Some(d) => say_to(&w, &format!("250-Listing\r\n type=file;size={};modify={STAMP}; {arg}\r\n250 End", d.len())),
                        None if dirs.lock().unwrap().iter().any(|d| d == arg) => say_to(&w, &format!("250-Listing\r\n type=dir;modify={STAMP}; {arg}\r\n250 End")),
                        None => say_to(&w, "550 not found"),
                    }
                }
                "MDTM" => say_to(&w, &format!("213 {STAMP}")),
                "RETR" => {
                    let Some(body) = files.lock().unwrap().get(arg).cloned() else {
                        say_to(&w, "550 not found");
                        continue;
                    };
                    say_to(&w, "150 sending");
                    let (mut d, _) = data.take().unwrap().accept().unwrap();
                    let from = std::mem::take(&mut rest).min(body.len());
                    let w2 = w.clone();
                    std::thread::spawn(move || {
                        // Whoever closes the data connection early gets no 226 from here.
                        if d.write_all(&body[from..]).is_ok() {
                            drop(d);
                            say_to(&w2, "226 done");
                        }
                    });
                }
                "ABOR" => say_to(&w, "426 aborted\r\n226 ok"),
                "STOR" => {
                    say_to(&w, "150 go on");
                    let (mut d, _) = data.take().unwrap().accept().unwrap();
                    let mut body = vec![];
                    d.read_to_end(&mut body).unwrap();
                    files.lock().unwrap().insert(arg.into(), body);
                    say_to(&w, "226 stored");
                }
                "RNFR" => {
                    from = arg.into();
                    say_to(&w, "350 ready");
                }
                "RNTO" => {
                    let mut f = files.lock().unwrap();
                    if f.contains_key(arg) {
                        say_to(&w, "550 exists");
                    } else if let Some(b) = f.remove(&from) {
                        f.insert(arg.into(), b);
                        say_to(&w, "250 renamed");
                    } else {
                        say_to(&w, "550 no such file");
                    }
                }
                "DELE" => {
                    let gone = files.lock().unwrap().remove(arg).is_some();
                    say_to(&w, if gone { "250 deleted" } else { "550 no such file" });
                }
                "MKD" => {
                    dirs.lock().unwrap().push(arg.into());
                    say_to(&w, &format!("257 \"{arg}\" created"));
                }
                "RMD" => {
                    dirs.lock().unwrap().retain(|d| d != arg);
                    say_to(&w, "250 removed");
                }
                "CWD" => {
                    let ok = dirs.lock().unwrap().iter().any(|d| d == arg);
                    say_to(&w, if ok { "250 ok" } else { "550 no" });
                }
                "QUIT" => {
                    say_to(&w, "221 bye");
                    return;
                }
                _ => say_to(&w, "502 not here"),
            }
        }
    }

    fn connect(f: &Fake) -> Ftp<NoTlsStream> {
        let port = f.port;
        Ftp::new("127.0.0.1".into(), Box::new(move || dial_plain("127.0.0.1", port, "anonymous", "a@"))).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn lists_and_stats() {
        for mlsd in [true, false] {
            let f = Fake::start(mlsd);
            f.dirs.lock().unwrap().push("/My Dir".into());
            f.put("/My Dir/ünï cöde.txt", b"hello");
            f.put("/top.txt", b"abc");
            let t = connect(&f);
            assert_eq!(t.default_dir(), "/");
            let mut root = t.listdir("/").unwrap();
            root.sort_by(|a, b| a.name.cmp(&b.name));
            assert_eq!(root.iter().map(|e| (e.name.as_str(), e.dir, e.size)).collect::<Vec<_>>(), [("My Dir", true, 0), ("top.txt", false, 3)]);
            assert!(root[1].mtime > 0);
            let sub = t.listdir("/My Dir").unwrap();
            assert_eq!((sub[0].name.as_str(), sub[0].path.as_str(), sub[0].size), ("ünï cöde.txt", "/My Dir/ünï cöde.txt", 5));
            assert!(t.stat("/My Dir/ünï cöde.txt").unwrap().is_some());
            assert!(t.stat("/My Dir").unwrap().unwrap().dir);
            assert!(t.stat("/nope.txt").unwrap().is_none());
            assert!(t.stat("/nodir/x").unwrap().is_none());
            assert!(t.listdir("/nodir").is_err());
            let (data, v) = t.read("/My Dir/ünï cöde.txt").unwrap();
            assert_eq!(data, b"hello");
            assert_eq!(t.version("/My Dir/ünï cöde.txt").unwrap(), Some(v));
        }
    }

    #[test]
    fn reads_ranges() {
        let f = Fake::start(true);
        let big: Vec<u8> = (0..4_000_000u32).map(|i| (i % 251) as u8).collect();
        f.put("/big.bin", &big);
        f.put("/small.bin", b"0123456789");
        let t = connect(&f);
        assert_eq!(t.read_range("/big.bin", 1000, 16).unwrap(), &big[1000..1016]);
        // The connection still works after the abort, and is the same one.
        assert_eq!(t.read_range("/big.bin", 0, 5).unwrap(), &big[..5]);
        assert_eq!(t.read_range("/small.bin", 4, 100).unwrap(), b"456789");
        assert_eq!(t.read_range("/small.bin", 20, 5).unwrap(), b"");
        assert_eq!(t.listdir("/").unwrap().len(), 2);
        assert_eq!(f.logins.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn replaces_and_edits() {
        let f = Fake::start(true);
        f.put("/a.txt", b"old");
        let t = connect(&f);
        let v = t.replace("/a.txt", b"new contents", true).unwrap();
        assert_eq!(t.read("/a.txt").unwrap(), (b"new contents".to_vec(), v));
        assert!(f.files.lock().unwrap().keys().all(|k| !k.contains("ferry-tmp")));
        t.replace("/b b.txt", b"fresh", false).unwrap();
        assert_eq!(f.files.lock().unwrap()["/b b.txt"], b"fresh");
        t.rename("/b b.txt", "c c.txt").unwrap();
        assert!(t.stat("/b b.txt").unwrap().is_none());
        assert!(t.stat("/c c.txt").unwrap().is_some());
        t.make_dir("/new dir").unwrap();
        assert!(t.stat("/new dir").unwrap().unwrap().dir);
        t.delete("/c c.txt", false).unwrap();
        assert!(t.delete("/c c.txt", false).is_err());
        f.put("/new dir/x.txt", b"x");
        t.delete("/new dir", true).unwrap();
        assert!(t.stat("/new dir").unwrap().is_none());
        assert!(!f.files.lock().unwrap().contains_key("/new dir/x.txt"));
    }

    #[test]
    fn reconnects_when_dropped() {
        let f = Fake::start(true);
        f.put("/a.txt", b"x");
        let t = connect(&f);
        assert_eq!(t.listdir("/").unwrap().len(), 1);
        f.drop_next.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(t.listdir("/").unwrap().len(), 1);
        assert_eq!(f.logins.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn says_when_it_cant_connect() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let e = dial_plain("127.0.0.1", port, "a", "b").err().unwrap();
        assert!(e.starts_with("Couldn't connect to 127.0.0.1"), "{e}");
        // A plain server doesn't offer AUTH TLS.
        let f = Fake::start(true);
        let cfg = tls_config().unwrap();
        let e = dial_tls("127.0.0.1", f.port, "a", "b", cfg).err().unwrap();
        assert_eq!(e, "The server doesn't offer FTPS.");
        assert_eq!(f.logins.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
