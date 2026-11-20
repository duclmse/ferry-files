//! The connections added in Settings > Connections: Everything servers, SFTP servers and SMB
//! shares, in one list. Each has a type, an address and how it logs in; a password is kept in
//! the system's keychain under the connection's id, never here.
//!
//! Everything and SFTP servers are connected like phones, again at launch and every so often
//! while they're offline. An SMB share is mounted by the system; mounted, it's a folder on the
//! computer.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ConnKind {
    Sftp,
    Smb,
    Everything,
}

impl ConnKind {
    pub(super) const ALL: [ConnKind; 3] = [ConnKind::Sftp, ConnKind::Smb, ConnKind::Everything];

    pub(super) fn key(self) -> &'static str {
        match self {
            ConnKind::Sftp => "sftp",
            ConnKind::Smb => "smb",
            ConnKind::Everything => "everything",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            ConnKind::Sftp => "SFTP",
            ConnKind::Smb => "SMB",
            ConnKind::Everything => "Everything",
        }
    }
}

/// How a connection logs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Auth {
    /// SFTP: this computer's ssh keys and settings. SMB: the system asks. Everything: no login.
    #[default]
    System,
    /// A user name and a password saved in the keychain.
    Password,
    /// SFTP only: a key file, with its passphrase in the keychain when it has one.
    Key,
}

/// One connection in the list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Conn {
    /// Its id, also the name its password is kept under in the keychain.
    pub id: String,
    pub kind: ConnKind,
    /// The server, without user or port ("nas", "10.0.0.2", or a host from ~/.ssh/config).
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: String,
    /// SMB: the shared folder.
    #[serde(default)]
    pub share: String,
    /// SMB: the Windows domain or workgroup, when the server wants one.
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub auth: Auth,
    /// SFTP with `Auth::Key`: the key file.
    #[serde(default)]
    pub key_file: String,
    /// A password (or passphrase) is saved in the keychain.
    #[serde(default)]
    pub saved: bool,
}

impl Conn {
    pub(super) fn new(kind: ConnKind) -> Conn {
        let id = format!("c{}", js_sys::Date::now() as u64);
        Conn { id, kind, host: String::new(), port: None, user: String::new(), share: String::new(), domain: String::new(), auth: Auth::System, key_file: String::new(), saved: false }
    }

    /// The address the backend knows it by: "http://pc:8080", "me@box:2222", "smb://nas/Photos".
    pub(super) fn addr(&self) -> String {
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        match self.kind {
            ConnKind::Everything => format!("http://{}{port}", self.host),
            ConnKind::Sftp if self.user.is_empty() => format!("{}{port}", self.host),
            ConnKind::Sftp => format!("{}@{}{port}", self.user, self.host),
            // A user without a saved password goes in the address, so the system asks for theirs.
            ConnKind::Smb if !self.user.is_empty() && self.auth == Auth::System => format!("smb://{}@{}{port}/{}", self.user, self.host, self.share),
            ConnKind::Smb => format!("smb://{}{port}/{}", self.host, self.share),
        }
    }

    /// As it's shown: "me@box:2222", "nas/Photos", "pc:8080".
    pub(super) fn name(&self) -> String {
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
        match self.kind {
            ConnKind::Smb => format!("{user}{}{port}/{}", self.host, self.share),
            _ => format!("{user}{}{port}", self.host),
        }
    }

    /// Whether `text` is in its host, user name or share (ignoring case).
    pub(super) fn matches(&self, text: &str) -> bool {
        let t = text.trim().to_lowercase();
        t.is_empty() || [&self.host, &self.user, &self.share].iter().any(|f| f.to_lowercase().contains(&t)) || self.name().to_lowercase().contains(&t)
    }

    /// The id of the password to log in with, when one is saved and used.
    fn secret(&self) -> Option<&str> {
        (self.saved && self.auth != Auth::System).then_some(self.id.as_str())
    }

    /// The command that connects it, and its arguments.
    fn connect_call(&self) -> (&'static str, serde_json::Value) {
        let user = (self.auth == Auth::Password && !self.user.is_empty()).then_some(self.user.as_str());
        match self.kind {
            ConnKind::Everything => ("everything_connect", json!({ "server": self.addr(), "user": user, "secret": self.secret() })),
            ConnKind::Sftp => {
                let key = (self.auth == Auth::Key).then_some(self.key_file.as_str());
                ("sftp_connect", json!({ "server": self.addr(), "key_file": key, "secret": self.secret() }))
            }
            ConnKind::Smb => ("smb_mount", json!({ "share": self.addr(), "user": user, "domain": self.domain, "secret": self.secret() })),
        }
    }

    /// What's wrong with it before trying to connect, if anything.
    pub(super) fn problem(&self) -> Option<&'static str> {
        if self.host.trim().is_empty() || self.host.contains(|c: char| c.is_whitespace() || "/\\@".contains(c)) || self.host.starts_with('-') {
            return Some("Enter the server's name or address, without a user or folder.");
        }
        if self.kind == ConnKind::Smb && self.share.trim().is_empty() {
            return Some("Enter the shared folder's name.");
        }
        if self.auth == Auth::Password && self.user.trim().is_empty() && self.kind != ConnKind::Sftp {
            return Some("Enter the user name to log in with.");
        }
        if self.auth == Auth::Key && self.key_file.trim().is_empty() {
            return Some("Enter the key file's path, like ~/.ssh/id_ed25519.");
        }
        None
    }

    /// A connection from an address kept by an older version: "http://pc:8080", "me@box:2222"
    /// or "smb://me@nas/Photos".
    fn from_old(kind: ConnKind, addr: &str, id: String) -> Option<Conn> {
        let t = addr.trim().trim_start_matches("http://").trim_start_matches("smb:").trim_start_matches('/');
        let (hostpart, share) = t.split_once('/').unwrap_or((t, ""));
        let (user, hostport) = match hostpart.rsplit_once('@') {
            Some((u, h)) => (u, h),
            None => ("", hostpart),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().ok()),
            _ => (hostport, None),
        };
        (!host.is_empty()).then(|| Conn {
            id,
            kind,
            host: host.into(),
            port,
            user: user.into(),
            share: share.split('/').next().unwrap_or_default().into(),
            domain: String::new(),
            auth: Auth::System,
            key_file: String::new(),
            saved: false,
        })
    }
}

/// "user@host:port" split up, from what's typed in the box at the top of the list.
pub(super) fn split_address(text: &str) -> (String, String, Option<u16>, String) {
    let t = text.trim().replace('\\', "/");
    let t = match t.split_once("://") {
        Some((_, rest)) => rest.to_string(),
        None => t.trim_start_matches('/').to_string(),
    };
    let (hostpart, share) = t.split_once('/').unwrap_or((&t, ""));
    let (user, hostport) = hostpart.rsplit_once('@').unwrap_or(("", hostpart));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()),
        None => (hostport, None),
    };
    (user.to_string(), host.to_string(), port, share.split('/').next().unwrap_or_default().to_string())
}

/// The type an address typed in the box at the top of the list suggests.
pub(super) fn kind_hint(text: &str) -> ConnKind {
    let t = text.trim().to_lowercase();
    if t.starts_with("smb:") || t.starts_with("\\\\") || (t.contains('/') && !t.starts_with("sftp:") && !t.starts_with("ssh:") && !t.starts_with("http")) {
        ConnKind::Smb
    } else if t.starts_with("http") {
        ConnKind::Everything
    } else {
        ConnKind::Sftp
    }
}

/// The connections kept, or those an older version kept (as three lists of addresses).
pub(super) fn stored_conns() -> Vec<Conn> {
    if let Some(list) = stored(CONNS_KEY).and_then(|s| serde_json::from_str(&s).ok()) {
        return list;
    }
    let old = |key: &str| stored(key).and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default();
    let now = js_sys::Date::now() as u64;
    let mut n = 0;
    let mut list = vec![];
    for (kind, key) in [(ConnKind::Everything, EVERYTHING_KEY), (ConnKind::Sftp, SFTP_KEY), (ConnKind::Smb, SMB_KEY)] {
        for a in old(key) {
            n += 1;
            list.extend(Conn::from_old(kind, &a, format!("c{now}-{n}")));
        }
    }
    if !list.is_empty() {
        store(CONNS_KEY, &serde_json::to_string(&list).unwrap_or_default());
    }
    list
}

/// The connection's address in what the backend says a device is ("sftp:me@box").
fn usb_addr(info: &Info) -> Option<&str> {
    let usb = info.usb.as_deref()?;
    usb.strip_prefix("sftp:").or_else(|| usb.strip_prefix("everything:"))
}

impl Ctx {
    pub(super) fn conns_of(self, kind: ConnKind) -> Vec<Conn> {
        self.conns.with(|cs| cs.iter().filter(|c| c.kind == kind).cloned().collect())
    }

    /// The device a connection (an Everything or SFTP server) is connected as, when it is.
    pub(super) fn conn_src(self, c: &Conn) -> Option<Src> {
        let (kind, addr) = (c.kind.key(), c.addr());
        self.phones.with(|ps| ps.iter().find(|i| i.transport == kind && usb_addr(i) == Some(addr.as_str())).map(|i| Src::of(&i.id)))
    }

    /// Where an SMB share is mounted, when it is.
    pub(super) fn smb_path(self, c: &Conn) -> Option<String> {
        let addr = c.addr();
        self.smb_mounts.with(|m| m.iter().find(|m| m.share == addr).map(|m| m.path.clone()))
    }

    /// Whether it's connected (or mounted).
    pub(super) fn conn_on(self, c: &Conn) -> bool {
        match c.kind {
            ConnKind::Smb => self.smb_path(c).is_some(),
            _ => self.conn_src(c).is_some(),
        }
    }

    /// The SMB shares mounted, in the app or not, that aren't in the list.
    pub(super) fn other_mounts(self) -> Vec<SmbMount> {
        let known: Vec<String> = self.conns_of(ConnKind::Smb).iter().map(Conn::addr).collect();
        self.smb_mounts.with(|m| m.iter().filter(|m| !known.contains(&m.share)).cloned().collect())
    }

    fn set_conns(self, list: Vec<Conn>) {
        store(CONNS_KEY, &serde_json::to_string(&list).unwrap_or_default());
        self.conns.set(list);
    }

    fn conn_error(self, id: &str, e: Option<String>) {
        self.conn_errors.update(|m| {
            match e {
                Some(e) => m.insert(id.to_string(), e),
                None => m.remove(id),
            };
        });
    }

    /// Saves `c` (new or changed), with `password` saved in the keychain when one was typed, and
    /// connects it. Why not, when it can't.
    pub(super) async fn save_conn(self, mut c: Conn, password: String) -> Result<(), String> {
        if let Some(p) = c.problem() {
            return Err(p.into());
        }
        c.host = c.host.trim().to_string();
        c.user = c.user.trim().to_string();
        c.share = c.share.trim().trim_matches('/').to_string();
        let old = self.conns.with_untracked(|cs| cs.iter().find(|x| x.id == c.id).cloned());
        if self.conns.with_untracked(|cs| cs.iter().any(|x| x.id != c.id && x.kind == c.kind && x.addr() == c.addr())) {
            return Err("That connection is already in the list.".into());
        }
        // The password: a new one saved, or the saved one forgotten when it's no longer used.
        if !password.is_empty() && c.auth != Auth::System {
            call::<()>("secret_set", json!({ "id": c.id, "secret": password })).await?;
            c.saved = true;
        } else if c.auth == Auth::System && c.saved {
            let _ = call::<()>("secret_clear", json!({ "id": c.id })).await;
            c.saved = false;
        }
        // A server connected with the old settings is disconnected first.
        if let Some(old) = &old {
            if let Some(src) = self.conn_src(old) {
                self.disconnect(src);
            }
        }
        self.set_conns(match old {
            Some(_) => self.conns.get_untracked().into_iter().map(|x| if x.id == c.id { c.clone() } else { x }).collect(),
            None => self.conns.get_untracked().into_iter().chain([c.clone()]).collect(),
        });
        self.connect_conn(c, true).await
    }

    /// Connects `c` (mounts an SMB share); with `show`, opens it in the active pane.
    pub(super) async fn connect_conn(self, c: Conn, show: bool) -> Result<(), String> {
        let (cmd, args) = c.connect_call();
        let done = if c.kind == ConnKind::Smb {
            call::<SmbMount>(cmd, args).await.map(|m| {
                let path = m.path.clone();
                self.smb_mounts.update(|ms| {
                    ms.retain(|x| x.path != m.path);
                    ms.push(m);
                });
                if show {
                    self.open_place(Src::Mac, path);
                }
            })
        } else {
            match call::<Info>(cmd, args).await {
                Ok(info) => {
                    // Taken out of the list while connecting.
                    if !self.conns.with_untracked(|cs| cs.iter().any(|x| x.id == c.id)) {
                        let _ = call::<()>("disconnect", json!({ "src": info.id })).await;
                        return Ok(());
                    }
                    self.connected(info, show);
                    Ok(())
                }
                // Connected meanwhile (a check already under way).
                Err(e) if e.contains("already added") => Ok(()),
                Err(e) => Err(e),
            }
        };
        self.conn_error(&c.id, done.as_ref().err().cloned());
        done
    }

    /// Takes connection `id` out of the list: disconnects it and forgets its password. A
    /// mounted SMB share stays mounted until it's ejected.
    pub(super) fn remove_conn(self, id: &str) {
        let Some(c) = self.conns.with_untracked(|cs| cs.iter().find(|x| x.id == id).cloned()) else { return };
        self.set_conns(self.conns.get_untracked().into_iter().filter(|x| x.id != id).collect());
        self.conn_error(id, None);
        if let Some(src) = self.conn_src(&c) {
            self.disconnect(src);
        }
        if c.saved {
            let id = id.to_string();
            spawn_local(async move {
                let _ = call::<()>("secret_clear", json!({ "id": id })).await;
            });
        }
    }

    /// Takes the server connected as `src` out of the list.
    pub(super) fn remove_conn_of(self, src: Src) {
        let id = self.conns.with_untracked(|cs| cs.iter().find(|c| self.conn_src(c) == Some(src)).map(|c| c.id.clone()));
        match id {
            Some(id) => self.remove_conn(&id),
            None => self.disconnect(src),
        }
    }

    /// Unmounts the SMB share mounted at `path`. Panes showing it go back to the home folder first.
    pub(super) fn unmount_smb(self, path: String, name: String) {
        if self.running() {
            self.act().error("Wait for the copy to finish before disconnecting.");
            return;
        }
        for p in self.panes {
            if p.src.get_untracked() == Src::Mac && within(&p.cwd.get_untracked(), &path) {
                if let Some(home) = self.mac.get_untracked().map(|i| i.default_dir) {
                    self.load_select(p, Src::Mac, home, None);
                }
            }
        }
        spawn_local(async move {
            match call::<()>("smb_unmount", json!({ "path": path })).await {
                Ok(()) => {
                    self.log(format!("Disconnected {name}"));
                    self.smb_mounts.update(|ms| ms.retain(|m| m.path != path));
                }
                Err(e) => self.act().error(e),
            }
        });
    }

    /// Connects the servers that aren't, looks again at the drives of the Everything servers
    /// that are, and at which SMB shares are mounted. At launch and every `EVERYTHING_EVERY`.
    pub(super) fn check_conns(self) {
        for c in self.conns.get_untracked() {
            match (c.kind, self.conn_src(&c)) {
                (ConnKind::Smb, _) => {}
                (ConnKind::Everything, Some(src)) => self.refresh_drives(src),
                (_, Some(_)) => {}
                (_, None) => spawn_local(async move {
                    let _ = self.connect_conn(c, false).await;
                }),
            }
        }
        let shares: Vec<String> = self.conns_of(ConnKind::Smb).iter().map(Conn::addr).collect();
        spawn_local(async move {
            if let Ok(m) = call::<Vec<SmbMount>>("smb_mounts", json!({ "shares": shares })).await {
                if self.smb_mounts.with_untracked(|now| *now != m) {
                    self.smb_mounts.set(m);
                }
            }
        });
    }

    /// Looks again at an Everything server's drives.
    fn refresh_drives(self, src: Src) {
        spawn_local(async move {
            if call::<()>("refresh", json!({ "src": src.key() })).await.is_err() {
                return;
            }
            if let Ok(Some(info)) = call::<Option<Info>>("info", json!({ "src": src.key() })).await {
                // Only when the drives changed, so the sidebar isn't drawn again for nothing.
                let changed = self.phones.with_untracked(|ps| ps.iter().any(|i| i.id == info.id && i.quick_dirs != info.quick_dirs));
                if changed {
                    self.phones.update(|ps| {
                        if let Some(i) = ps.iter_mut().find(|i| i.id == info.id) {
                            i.quick_dirs = info.quick_dirs;
                        }
                    });
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(kind: ConnKind, host: &str, user: &str, port: Option<u16>, share: &str) -> Conn {
        Conn { id: "c1".into(), kind, host: host.into(), port, user: user.into(), share: share.into(), domain: String::new(), auth: Auth::System, key_file: String::new(), saved: false }
    }

    #[test]
    fn addresses() {
        assert_eq!(conn(ConnKind::Everything, "pc", "", Some(8080), "").addr(), "http://pc:8080");
        assert_eq!(conn(ConnKind::Sftp, "box", "me", Some(2222), "").addr(), "me@box:2222");
        assert_eq!(conn(ConnKind::Sftp, "box", "", None, "").addr(), "box");
        assert_eq!(conn(ConnKind::Smb, "nas", "me", None, "Photos").addr(), "smb://me@nas/Photos");
        let mut c = conn(ConnKind::Smb, "nas", "me", None, "Photos");
        c.auth = Auth::Password;
        assert_eq!(c.addr(), "smb://nas/Photos");
        assert_eq!(c.name(), "me@nas/Photos");
        assert!(c.matches("NAS") && c.matches("me") && c.matches("photo") && !c.matches("box"));
        assert!(conn(ConnKind::Sftp, "me@box", "", None, "").problem().is_some());
        assert!(conn(ConnKind::Smb, "nas", "", None, "").problem().is_some());
    }

    #[test]
    fn typed_addresses() {
        assert_eq!(split_address("me@box:2222"), ("me".into(), "box".into(), Some(2222), String::new()));
        assert_eq!(split_address(r"\\nas\Photos"), (String::new(), "nas".into(), None, "Photos".into()));
        assert_eq!(split_address("smb://me@nas/Photos/x"), ("me".into(), "nas".into(), None, "Photos".into()));
        assert_eq!(kind_hint(r"\\nas\Photos"), ConnKind::Smb);
        assert_eq!(kind_hint("http://pc:8080"), ConnKind::Everything);
        assert_eq!(kind_hint("me@box"), ConnKind::Sftp);
        let old = Conn::from_old(ConnKind::Sftp, "me@box:2222", "c1".into()).unwrap();
        assert_eq!((old.user.as_str(), old.host.as_str(), old.port), ("me", "box", Some(2222)));
        let old = Conn::from_old(ConnKind::Smb, "smb://nas/Photos", "c2".into()).unwrap();
        assert_eq!((old.host.as_str(), old.share.as_str()), ("nas", "Photos"));
        let old = Conn::from_old(ConnKind::Everything, "http://pc:8080", "c3".into()).unwrap();
        assert_eq!(old.addr(), "http://pc:8080");
    }
}
