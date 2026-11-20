//! SFTP servers, through the computer's own `ssh`: it's started with the sftp subsystem and the
//! app speaks SFTP (version 3) over its input and output. So `~/.ssh/config`, keys, the agent and
//! known hosts work as in a terminal. A server can also get a key file of its own, or a password
//! saved in the keychain, which `ssh` gets from the app as its askpass program (see `secrets`).
//! Without a saved password `ssh` runs in batch mode, so it never waits for one.
//!
//! On macOS and Linux the connection is shared (ControlMaster), so the terminal's commands don't
//! log in again.

use super::*;
use crate::device::{basename, dirname, err, join, Result, Transport};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::Duration;

const INIT: u8 = 1;
const VERSION: u8 = 2;
const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const REMOVE: u8 = 13;
const MKDIR: u8 = 14;
const RMDIR: u8 = 15;
const REALPATH: u8 = 16;
const STAT: u8 = 17;
const LSTAT: u8 = 7;
const RENAME: u8 = 18;
const EXTENDED: u8 = 200;
const STATUS: u8 = 101;
const HANDLE: u8 = 102;
const DATA: u8 = 103;
const NAME: u8 = 104;
const ATTRS: u8 = 105;

const FX_EOF: u32 = 1;
const FX_NO_SUCH_FILE: u32 = 2;

const OPEN_READ: u32 = 1;
const OPEN_WRITE: u32 = 2;
const OPEN_CREATE: u32 = 8;
const OPEN_TRUNC: u32 = 0x10;

/// Bytes asked for or sent per request. OpenSSH takes up to 256 KB; a server that sends less
/// is asked again for the rest.
const CHUNK: u32 = 64 * 1024;
/// Requests sent before waiting for the answers, so a copy isn't one round trip per chunk
/// (4 MB in flight, like OpenSSH's own sftp with its usual settings).
const AHEAD: usize = 64;

/// An SFTP address: what to give `ssh` (a host from `~/.ssh/config`, or user@host) and the port.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Address {
    pub host: String,
    pub port: Option<u16>,
    /// A private key file to log in with, instead of the ones `ssh` would try.
    pub key: Option<String>,
    /// The connection whose password (or the key's passphrase) is saved in the keychain.
    pub secret: Option<String>,
}

impl Address {
    /// From "host", "user@host", "user@host:2222" or "sftp://user@host:2222/...".
    pub fn parse(text: &str) -> std::result::Result<Address, String> {
        let t = text.trim();
        let t = t.strip_prefix("sftp://").or_else(|| t.strip_prefix("ssh://")).unwrap_or(t);
        let t = t.split('/').next().unwrap_or_default();
        let (host, port) = match t.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p.parse::<u16>().map_err(|_| format!("{p} isn't a port number."))?)),
            _ => (t, None),
        };
        if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
            return Err("Enter a server, like me@example.com or a host from ~/.ssh/config.".into());
        }
        Ok(Address { host: host.to_string(), port, ..Default::default() })
    }

    /// As it's shown and kept: "user@host" or "user@host:port".
    pub fn text(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.host),
            None => self.host.clone(),
        }
    }

    /// `ssh` with the options the app always uses, for this server.
    pub fn ssh(&self) -> std::process::Command {
        self.ssh_with(crate::platform::command("ssh"), &[])
    }

    /// `ssh` set up as `ssh`, with `options` before the server.
    fn ssh_with(&self, mut c: std::process::Command, options: &[&str]) -> std::process::Command {
        c.args(options);
        c.args(["-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15"]);
        match &self.secret {
            Some(id) => {
                c.args(["-o", "NumberOfPasswordPrompts=1"]);
                crate::secrets::ssh_asks(&mut c, id);
            }
            None => {
                c.args(["-o", "BatchMode=yes"]);
            }
        }
        if let Some(key) = &self.key {
            c.args(["-i", key, "-o", "IdentitiesOnly=yes"]);
        }
        if cfg!(unix) {
            c.args(["-o", "ControlMaster=auto", "-o", "ControlPath=/tmp/ferry-ssh-%C", "-o", "ControlPersist=60"]);
        }
        if let Some(p) = self.port {
            c.args(["-p", &p.to_string()]);
        }
        c.arg("--").arg(&self.host);
        c
    }
}

/// What `ssh` said when it gave up, in words that say what to do.
fn ssh_failed(addr: &Address, said: &str) -> String {
    let said = said.trim();
    let host = addr.text();
    if said.contains("Permission denied") && addr.secret.is_some() {
        format!("{host} didn't take the password. Check it in Settings > Connections.")
    } else if said.contains("Permission denied") && addr.key.is_some() {
        format!("{host} didn't take the key file. Check its path, and add a passphrase in Settings > Connections if it has one.")
    } else if said.contains("Permission denied") {
        format!("{host} wants a password or a key it knows. Add a password or a key file in Settings > Connections, or add your key with ssh-copy-id {host}.")
    } else if said.contains("Host key verification failed") || said.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        format!("{host}'s key isn't the one in ~/.ssh/known_hosts. If the server was set up again, remove the old key with ssh-keygen -R and try again.")
    } else if said.contains("Could not resolve") || said.contains("Connection refused") || said.contains("timed out") || said.contains("No route") {
        format!("Can't reach {host}: {}", said.lines().last().unwrap_or(said))
    } else if said.contains("subsystem request failed") {
        format!("{host} doesn't serve SFTP.")
    } else if said.is_empty() {
        format!("Lost the connection to {host}.")
    } else {
        format!("Couldn't connect to {host}: {}", said.lines().last().unwrap_or(said))
    }
}

/// Builds one request's bytes.
struct Out(Vec<u8>);

impl Out {
    fn new(kind: u8, id: u32) -> Out {
        let mut o = Out(vec![0, 0, 0, 0, kind]);
        o.u32(id);
        o
    }
    fn u32(&mut self, v: u32) -> &mut Out {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Out {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Out {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    fn str(&mut self, s: &str) -> &mut Out {
        self.bytes(s.as_bytes())
    }
    /// Empty attributes.
    fn no_attrs(&mut self) -> &mut Out {
        self.u32(0)
    }
    fn done(mut self) -> Vec<u8> {
        let n = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&n.to_be_bytes());
        self.0
    }
}

/// Reads one answer's fields.
struct In<'a>(&'a [u8]);

impl In<'_> {
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes(b.try_into().unwrap_or_default()))
    }
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return err("The server's answer was cut short.");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.bytes()?).into_owned())
    }
    fn attrs(&mut self) -> Result<Attrs> {
        let flags = self.u32()?;
        let mut a = Attrs::default();
        if flags & 1 != 0 {
            a.size = self.u64()?;
        }
        if flags & 2 != 0 {
            self.take(8)?;
        }
        if flags & 4 != 0 {
            a.perm = Some(self.u32()?);
        }
        if flags & 8 != 0 {
            self.take(4)?;
            a.mtime = self.u32()? as i64;
        }
        if flags & 0x8000_0000 != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(a)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Attrs {
    size: u64,
    perm: Option<u32>,
    mtime: i64,
}

impl Attrs {
    fn kind(&self) -> u32 {
        self.perm.unwrap_or(0) & 0o170000
    }
    fn is_dir(&self) -> bool {
        self.kind() == 0o040000
    }
    fn is_link(&self) -> bool {
        self.kind() == 0o120000
    }
}

/// An answer: its kind and what follows its id.
struct Reply {
    kind: u8,
    body: Vec<u8>,
}

/// A running `ssh -s sftp`.
struct Session {
    child: Child,
    to: ChildStdin,
    from: ChildStdout,
    next: u32,
    /// The server renames over an existing file in one step (posix-rename@openssh.com).
    posix_rename: bool,
    /// What `ssh` wrote to its error output, for saying why it stopped.
    said: Arc<Mutex<String>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn start(addr: &Address) -> std::result::Result<Session, String> {
        let mut child = addr
            .ssh()
            .args(["-s", "sftp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Can't run ssh: {e}. Install OpenSSH to use SFTP servers."))?;
        let said = Arc::new(Mutex::new(String::new()));
        if let Some(mut e) = child.stderr.take() {
            let said = said.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = e.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut s = said.lock().unwrap_or_else(|e| e.into_inner());
                    if s.len() < 8192 {
                        s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            });
        }
        let (Some(to), Some(from)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("Can't talk to ssh.".into());
        };
        let mut s = Session { child, to, from, next: 1, posix_rename: false, said };
        let mut hello = Out(vec![0, 0, 0, 0, INIT]);
        hello.u32(3);
        let failed = |s: &Session| {
            // ssh may still be writing why it stopped.
            std::thread::sleep(Duration::from_millis(100));
            ssh_failed(addr, &s.said.lock().unwrap_or_else(|e| e.into_inner()))
        };
        if s.to.write_all(&hello.done()).and_then(|_| s.to.flush()).is_err() {
            return Err(failed(&s));
        }
        let Ok((kind, body)) = s.read_packet() else { return Err(failed(&s)) };
        if kind != VERSION {
            return Err(format!("{} answered in a way the app doesn't understand.", addr.text()));
        }
        let mut r = In(&body);
        let _version = r.u32().map_err(|e| e.to_string())?;
        while !r.0.is_empty() {
            let (Ok(name), Ok(_)) = (r.string(), r.bytes()) else { break };
            if name == "posix-rename@openssh.com" {
                s.posix_rename = true;
            }
        }
        Ok(s)
    }

    fn read_packet(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut len = [0u8; 4];
        self.from.read_exact(&mut len)?;
        let n = u32::from_be_bytes(len) as usize;
        if n == 0 || n > 4 * 1024 * 1024 {
            return Err(std::io::Error::other("bad packet"));
        }
        let mut body = vec![0u8; n];
        self.from.read_exact(&mut body)?;
        let kind = body.remove(0);
        Ok((kind, body))
    }

    fn lost(&self) -> DevError {
        DevError::Msg(format!(
            "Lost the connection to the server. {}",
            self.said.lock().unwrap_or_else(|e| e.into_inner()).lines().last().unwrap_or_default()
        ))
    }

    fn id(&mut self) -> u32 {
        self.next = self.next.wrapping_add(1);
        self.next
    }

    fn send(&mut self, packet: Vec<u8>) -> Result<()> {
        self.to.write_all(&packet).and_then(|_| self.to.flush()).map_err(|_| self.lost())
    }

    /// The next answer, by id.
    fn recv(&mut self) -> Result<(u32, Reply)> {
        let (kind, body) = self.read_packet().map_err(|_| self.lost())?;
        if body.len() < 4 {
            return err("The server's answer was cut short.");
        }
        let id = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        Ok((id, Reply { kind, body: body[4..].to_vec() }))
    }

    /// Sends a request and waits for its answer.
    fn ask(&mut self, kind: u8, fill: impl FnOnce(&mut Out)) -> Result<Reply> {
        let id = self.id();
        let mut o = Out::new(kind, id);
        fill(&mut o);
        self.send(o.done())?;
        loop {
            let (got, reply) = self.recv()?;
            if got == id {
                return Ok(reply);
            }
        }
    }
}

/// The status in an answer: Ok(code) for OK, EOF and no such file; an error for the rest.
fn status(path: &str, r: &Reply) -> Result<u32> {
    if r.kind != STATUS {
        return err(format!("{path}: unexpected answer from the server."));
    }
    let mut i = In(&r.body);
    let code = i.u32()?;
    match code {
        0 | FX_EOF | FX_NO_SUCH_FILE => Ok(code),
        3 => err(format!("{path}: permission denied.")),
        _ => {
            let msg = i.string().unwrap_or_default();
            err(if msg.is_empty() { format!("{path}: the server refused (error {code}).") } else { format!("{path}: {msg}") })
        }
    }
}

/// Like `status`, but only OK will do.
fn ok(path: &str, r: &Reply) -> Result<()> {
    match status(path, r)? {
        0 => Ok(()),
        FX_NO_SUCH_FILE => err(format!("{path}: no such file or folder.")),
        _ => err(format!("{path}: the server refused.")),
    }
}

fn handle(path: &str, r: &Reply) -> Result<Vec<u8>> {
    if r.kind == HANDLE {
        return In(&r.body).bytes();
    }
    ok(path, r)?;
    err(format!("{path}: the server didn't open it."))
}

pub struct Sftp {
    addr: Address,
    home: String,
    /// None after the connection was lost: started again by the next use.
    session: Mutex<Option<Session>>,
}

impl Sftp {
    pub fn open(addr: Address) -> std::result::Result<Sftp, String> {
        let mut s = Session::start(&addr)?;
        let r = s.ask(REALPATH, |o| {
            o.str(".");
        });
        let home = match r {
            Ok(r) if r.kind == NAME => {
                let mut i = In(&r.body);
                let _ = i.u32();
                i.string().unwrap_or_else(|_| "/".into())
            }
            _ => "/".into(),
        };
        Ok(Sftp { addr, home, session: Mutex::new(Some(s)) })
    }

    pub fn address(&self) -> &Address {
        &self.addr
    }

    /// Runs `f` with the session, starting it again when it was lost; a lost one is dropped.
    fn with<T>(&self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let mut g = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(Session::start(&self.addr).map_err(DevError::Msg)?);
        }
        let s = g.as_mut().expect("started above");
        let r = f(s);
        if matches!(s.child.try_wait(), Ok(Some(_))) {
            *g = None;
        }
        r
    }

    fn attrs_of(s: &mut Session, path: &str, kind: u8) -> Result<Option<Attrs>> {
        let r = s.ask(kind, |o| {
            o.str(path);
        })?;
        if r.kind == ATTRS {
            return Ok(Some(In(&r.body).attrs()?));
        }
        status(path, &r)?;
        Ok(None)
    }

    fn close(s: &mut Session, h: &[u8]) {
        let _ = s.ask(CLOSE, |o| {
            o.bytes(h);
        });
    }

    /// Up to `len` bytes from `start` of the open file `h`, a few requests ahead.
    fn read_from(s: &mut Session, path: &str, h: &[u8], start: u64, len: u64) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut asked: HashMap<u32, u64> = HashMap::new();
        let mut parts: std::collections::BTreeMap<u64, Vec<u8>> = std::collections::BTreeMap::new();
        let end = start.saturating_add(len);
        let mut next = start;
        let mut eof = false;
        loop {
            while !eof && asked.len() < AHEAD && next < end {
                let n = (end - next).min(CHUNK as u64) as u32;
                let id = s.id();
                let mut o = Out::new(READ, id);
                o.bytes(h).u64(next).u32(n);
                s.send(o.done())?;
                asked.insert(id, next);
                next += n as u64;
            }
            if asked.is_empty() {
                break;
            }
            let (id, r) = s.recv()?;
            let Some(at) = asked.remove(&id) else { continue };
            if r.kind == DATA {
                let chunk = In(&r.body).bytes()?;
                // A short read: ask again for the rest of that piece.
                let want = (end - at).min(CHUNK as u64) as usize;
                if chunk.len() < want && !chunk.is_empty() {
                    let id = s.id();
                    let mut o = Out::new(READ, id);
                    let from = at + chunk.len() as u64;
                    o.bytes(h).u64(from).u32((want - chunk.len()) as u32);
                    s.send(o.done())?;
                    asked.insert(id, from);
                }
                parts.insert(at, chunk);
            } else if status(path, &r)? == FX_EOF {
                eof = true;
            }
        }
        // Pieces in order, up to the first gap (the end of the file).
        let mut at = start;
        for (from, chunk) in parts {
            if from != at {
                break;
            }
            at += chunk.len() as u64;
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    fn write_all(s: &mut Session, path: &str, h: &[u8], data: &[u8]) -> Result<()> {
        let mut waiting = 0usize;
        let mut chunks = data.chunks(CHUNK as usize).enumerate();
        loop {
            while waiting < AHEAD {
                let Some((i, chunk)) = chunks.next() else { break };
                let id = s.id();
                let mut o = Out::new(WRITE, id);
                o.bytes(h).u64(i as u64 * CHUNK as u64).bytes(chunk);
                s.send(o.done())?;
                waiting += 1;
            }
            if waiting == 0 {
                return Ok(());
            }
            let (_, r) = s.recv()?;
            ok(path, &r)?;
            waiting -= 1;
        }
    }

    fn entry(path: &str, a: &Attrs) -> Entry {
        Entry { name: basename(path).to_string(), path: path.to_string(), dir: a.is_dir(), size: if a.is_dir() { 0 } else { a.size }, mtime: a.mtime }
    }

    fn version_of(a: &Attrs) -> String {
        format!("{}-{}", a.size, a.mtime)
    }
}

/// `text` quoted for a POSIX shell.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

impl Transport for Sftp {
    fn kind(&self) -> &'static str {
        "sftp"
    }

    fn label(&self) -> String {
        self.addr.text()
    }

    fn default_dir(&self) -> String {
        self.home.clone()
    }

    fn quick_dirs(&self) -> Vec<(String, String)> {
        let mut dirs = vec![("Home".to_string(), self.home.clone())];
        if self.home != "/" {
            dirs.push(("/".into(), "/".into()));
        }
        dirs
    }

    fn listdir(&self, path: &str) -> Result<Vec<Entry>> {
        self.with(|s| {
            let r = s.ask(OPENDIR, |o| {
                o.str(path);
            })?;
            let h = handle(path, &r)?;
            let mut out = vec![];
            let mut links = vec![];
            loop {
                let r = s.ask(READDIR, |o| {
                    o.bytes(&h);
                })?;
                if r.kind != NAME {
                    match status(path, &r) {
                        Ok(_) => break,
                        Err(e) => {
                            Self::close(s, &h);
                            return Err(e);
                        }
                    }
                }
                let mut i = In(&r.body);
                for _ in 0..i.u32()? {
                    let name = i.string()?;
                    let _long = i.bytes()?;
                    let a = i.attrs()?;
                    if name == "." || name == ".." {
                        continue;
                    }
                    let p = join(path, &name);
                    if a.is_link() {
                        links.push(p);
                    } else {
                        out.push(Self::entry(&p, &a));
                    }
                }
            }
            Self::close(s, &h);
            // Links are listed as what they point at; broken ones as files.
            for p in links {
                let a = Self::attrs_of(s, &p, STAT)?.unwrap_or_default();
                out.push(Self::entry(&p, &a));
            }
            Ok(out)
        })
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>> {
        self.with(|s| Ok(Self::attrs_of(s, path, STAT)?.map(|a| Self::entry(path, &a))))
    }

    fn read(&self, path: &str) -> Result<(Vec<u8>, String)> {
        self.with(|s| {
            let a = Self::attrs_of(s, path, STAT)?.ok_or_else(|| DevError::Msg(format!("{path}: no such file.")))?;
            let r = s.ask(OPEN, |o| {
                o.str(path).u32(OPEN_READ).no_attrs();
            })?;
            let h = handle(path, &r)?;
            let data = Self::read_from(s, path, &h, 0, u64::MAX);
            Self::close(s, &h);
            Ok((data?, Self::version_of(&a)))
        })
    }

    fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        self.with(|s| {
            let r = s.ask(OPEN, |o| {
                o.str(path).u32(OPEN_READ).no_attrs();
            })?;
            let h = handle(path, &r)?;
            let data = Self::read_from(s, path, &h, start, len);
            Self::close(s, &h);
            data
        })
    }

    fn version(&self, path: &str) -> Result<Option<String>> {
        self.with(|s| Ok(Self::attrs_of(s, path, STAT)?.map(|a| Self::version_of(&a))))
    }

    fn replace(&self, path: &str, data: &[u8], _exists: bool) -> Result<String> {
        self.with(|s| {
            let tmp = join(&dirname(path), &format!(".{}.ferry-tmp", basename(path)));
            let r = s.ask(OPEN, |o| {
                o.str(&tmp).u32(OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC).no_attrs();
            })?;
            let h = handle(&tmp, &r)?;
            let wrote = Self::write_all(s, &tmp, &h, data);
            Self::close(s, &h);
            let remove_tmp = |s: &mut Session| {
                let _ = s.ask(REMOVE, |o| {
                    o.str(&tmp);
                });
            };
            if let Err(e) = wrote {
                remove_tmp(s);
                return Err(e);
            }
            // In one step where the server can; else the old file goes first.
            let renamed = if s.posix_rename {
                s.ask(EXTENDED, |o| {
                    o.str("posix-rename@openssh.com").str(&tmp).str(path);
                })
            } else {
                let _ = s.ask(REMOVE, |o| {
                    o.str(path);
                });
                s.ask(RENAME, |o| {
                    o.str(&tmp).str(path);
                })
            };
            if let Err(e) = renamed.and_then(|r| ok(path, &r)) {
                remove_tmp(s);
                return Err(e);
            }
            let a = Self::attrs_of(s, path, STAT)?.unwrap_or_default();
            if a.size != data.len() as u64 {
                return err(format!("{path}: wrote {} bytes but the file has {}.", data.len(), a.size));
            }
            Ok(Self::version_of(&a))
        })
    }

    fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        self.with(|s| {
            // A link to a folder is removed as a file.
            let link = Self::attrs_of(s, path, LSTAT)?.is_some_and(|a| a.is_link());
            let r = s.ask(if is_dir && !link { RMDIR } else { REMOVE }, |o| {
                o.str(path);
            })?;
            ok(path, &r)
        })
    }

    fn make_dir(&self, path: &str) -> Result<()> {
        self.with(|s| {
            let r = s.ask(MKDIR, |o| {
                o.str(path).no_attrs();
            })?;
            ok(path, &r)
        })
    }

    fn rename(&self, path: &str, new_name: &str) -> Result<()> {
        self.with(|s| {
            let to = join(&dirname(path), new_name);
            let r = s.ask(RENAME, |o| {
                o.str(path).str(&to);
            })?;
            ok(path, &r)
        })
    }

    fn refresh(&self) {
        // A lost connection is started again on the next use.
        let mut g = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if g.as_mut().is_some_and(|s| matches!(s.child.try_wait(), Ok(Some(_)))) {
            *g = None;
        }
    }

    /// A login shell on the server, in `dir`.
    fn shell(&self, dir: &str) -> Option<std::process::Command> {
        let mut c = self.addr.ssh_with(std::process::Command::new("ssh"), &["-t"]);
        c.arg(format!("cd {}; exec \"${{SHELL:-sh}}\" -l", sh_quote(dir)));
        Some(c)
    }
}

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn sftp_connect(
    state: State<'_, AppState>,
    server: String,
    key_file: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let mut addr = Address::parse(&server)?;
    addr.key = key_file.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    addr.secret = secret.filter(|s| !s.is_empty());
    if let Some(k) = &addr.key {
        let path = crate::platform::native(&expand_home(k));
        if !path.is_file() {
            return Err(format!("There's no key file at {k}."));
        }
        addr.key = Some(path.display().to_string());
    }
    let s = blocking(move || Sftp::open(addr)).await?;
    let usb = Some(format!("sftp:{}", s.address().text()));
    if state.devs().values().any(|c| c.usb == usb) {
        return Err(format!("{} is already added.", s.address().text()));
    }
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let conn = Conn { dev: Arc::new(Device::new(Box::new(s))), usb, serial: None };
    let info = connect::info_of(&id, Some(&conn), &conn.dev);
    state.devs().insert(id, conn);
    Ok(info)
}

/// "~/.ssh/id_ed25519" as an app path in the home folder.
fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => {
            let home = dirs::home_dir().map(|h| crate::platform::app_path(&h)).unwrap_or_default();
            format!("{}/{}", home.trim_end_matches('/'), rest.replace('\\', "/"))
        }
        None => crate::platform::app_path(std::path::Path::new(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(Address::parse("me@box").unwrap(), Address { host: "me@box".into(), ..Default::default() });
        assert_eq!(Address::parse(" sftp://me@box:2222/home ").unwrap(), Address { host: "me@box".into(), port: Some(2222), ..Default::default() });
        assert_eq!(Address::parse("me@box:2222").unwrap().text(), "me@box:2222");
        assert!(Address::parse("").is_err());
        assert!(Address::parse("-oProxyCommand=x").is_err());
        assert!(Address::parse("box:port").is_err());
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn packets() {
        let mut o = Out::new(OPEN, 7);
        o.str("/a").u32(OPEN_READ).no_attrs();
        let p = o.done();
        assert_eq!(&p[..5], &[0, 0, 0, 19, OPEN]);
        let mut i = In(&p[5..]);
        assert_eq!((i.u32().unwrap(), i.string().unwrap(), i.u32().unwrap(), i.u32().unwrap()), (7, "/a".into(), 1, 0));

        let mut a = Out(vec![]);
        a.u32(1 | 4 | 8).u64(42).u32(0o040755).u32(1).u32(1_700_000_000);
        let attrs = In(&a.0).attrs().unwrap();
        assert!(attrs.is_dir());
        assert_eq!((attrs.size, attrs.mtime), (42, 1_700_000_000));
        assert!(In(&[0, 0]).u32().is_err());
    }
}
