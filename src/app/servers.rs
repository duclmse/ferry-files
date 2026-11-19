//! SFTP servers and SMB shares, added in the sidebar's Servers section.
//!
//! An SFTP server is connected like a phone (through the system's ssh, with its keys), again at
//! launch and every so often while it's offline. An SMB share is mounted by the system, which
//! asks for the password itself; mounted, it's a folder on the computer.

use super::*;

/// The SFTP server a connection is to, by address.
fn sftp_of(info: &Info) -> Option<&str> {
    info.usb.as_deref()?.strip_prefix("sftp:")
}

/// "me@box:2222" from "sftp://me@box:2222/home", as the backend keeps it.
pub(super) fn sftp_address(text: &str) -> String {
    let t = text.trim();
    let t = t.strip_prefix("sftp://").or_else(|| t.strip_prefix("ssh://")).unwrap_or(t);
    t.split('/').next().unwrap_or(t).to_string()
}

/// "smb://[user@]host/share" from what was typed, as the backend makes it.
pub(super) fn smb_share(text: &str) -> Option<String> {
    let t = text.trim().replace('\\', "/");
    let t = t.strip_prefix("smb:").unwrap_or(&t).trim_start_matches('/').to_string();
    let mut parts = t.split('/').filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some(host), Some(share)) if !host.contains(char::is_whitespace) => Some(format!("smb://{host}/{share}")),
        _ => None,
    }
}

impl Ctx {
    /// The connection to the SFTP server at `addr`, when there is one.
    pub(super) fn sftp_src(self, addr: &str) -> Option<Src> {
        self.phones.with(|ps| ps.iter().find(|i| sftp_of(i) == Some(addr)).map(|i| Src::of(&i.id)))
    }

    /// Where the SMB share is mounted, when it is.
    pub(super) fn smb_path(self, share: &str) -> Option<String> {
        self.smb_mounts.with(|m| m.iter().find(|m| m.share == share).map(|m| m.path.clone()))
    }

    fn set_sftp(self, servers: Vec<String>) {
        store(SFTP_KEY, &serde_json::to_string(&servers).unwrap_or_default());
        self.sftp.set(servers);
    }

    fn set_smb(self, shares: Vec<String>) {
        store(SMB_KEY, &serde_json::to_string(&shares).unwrap_or_default());
        self.smb.set(shares);
    }

    fn server_ok(self, key: &str) {
        self.server_errors.update(|e| {
            e.remove(key);
        });
    }

    /// Adds the SFTP server at `server` and shows it. Why not, when it can't.
    pub(super) async fn add_sftp(self, server: String) -> Result<(), String> {
        if self.sftp.with_untracked(|s| s.contains(&sftp_address(&server))) {
            return Err("That server is already in the list.".into());
        }
        let info = call::<Info>("sftp_connect", json!({ "server": server })).await?;
        let addr = sftp_of(&info).map_or_else(|| sftp_address(&server), String::from);
        if !self.sftp.with_untracked(|s| s.contains(&addr)) {
            self.set_sftp(self.sftp.get_untracked().into_iter().chain([addr.clone()]).collect());
        }
        self.server_ok(&addr);
        self.connected(info, true);
        Ok(())
    }

    /// Takes the SFTP server at `addr` out of the sidebar, disconnecting it.
    pub(super) fn remove_sftp(self, addr: &str) {
        self.set_sftp(self.sftp.get_untracked().into_iter().filter(|s| s != addr).collect());
        self.server_ok(addr);
        if let Some(src) = self.sftp_src(addr) {
            self.disconnect(src);
        }
    }

    /// Adds the SMB share at `text`, mounts it and shows it. Why not, when it can't.
    pub(super) async fn add_smb(self, text: String) -> Result<(), String> {
        let Some(share) = smb_share(&text) else {
            return Err("Enter the server and share, like smb://nas/Photos.".into());
        };
        if self.smb.with_untracked(|s| s.contains(&share)) {
            return Err("That share is already in the list.".into());
        }
        self.set_smb(self.smb.get_untracked().into_iter().chain([share.clone()]).collect());
        self.mount_smb(share).await
    }

    /// Mounts the SMB share (the system may ask for its password) and shows it.
    pub(super) async fn mount_smb(self, share: String) -> Result<(), String> {
        match call::<SmbMount>("smb_mount", json!({ "share": share })).await {
            Ok(m) => {
                self.server_ok(&share);
                let path = m.path.clone();
                self.smb_mounts.update(|ms| {
                    ms.retain(|x| x.share != share);
                    ms.push(SmbMount { share, path: m.path });
                });
                self.open_place(Src::Mac, path);
                Ok(())
            }
            Err(e) => {
                self.server_errors.update(|m| {
                    m.insert(share, e.clone());
                });
                Err(e)
            }
        }
    }

    /// Unmounts the SMB share. Panes showing it go back to the home folder first.
    pub(super) fn unmount_smb(self, share: &str) {
        let Some(path) = self.smb_path(share) else { return };
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
        let share = share.to_string();
        spawn_local(async move {
            match call::<()>("smb_unmount", json!({ "path": path })).await {
                Ok(()) => {
                    self.log(format!("Disconnected {share}"));
                    self.smb_mounts.update(|ms| ms.retain(|m| m.share != share));
                }
                Err(e) => self.act().error(e),
            }
        });
    }

    /// Takes the SMB share out of the sidebar. It stays mounted until it's ejected.
    pub(super) fn remove_smb(self, share: &str) {
        self.set_smb(self.smb.get_untracked().into_iter().filter(|s| s != share).collect());
        self.server_ok(share);
        self.smb_mounts.update(|ms| ms.retain(|m| m.share != share));
    }

    /// Connects the SFTP servers that aren't, and looks at which SMB shares are mounted. At
    /// launch and every so often.
    pub(super) fn check_servers(self) {
        for addr in self.sftp.get_untracked() {
            if self.sftp_src(&addr).is_some() {
                continue;
            }
            spawn_local(async move {
                match call::<Info>("sftp_connect", json!({ "server": addr })).await {
                    Ok(info) => {
                        // Taken out of the sidebar while connecting.
                        if !self.sftp.with_untracked(|s| s.contains(&addr)) {
                            let _ = call::<()>("disconnect", json!({ "src": info.id })).await;
                            return;
                        }
                        self.server_ok(&addr);
                        self.connected(info, false);
                    }
                    Err(e) if e.contains("already added") => {}
                    Err(e) => self.server_errors.update(|m| {
                        m.insert(addr, e);
                    }),
                }
            });
        }
        let shares = self.smb.get_untracked();
        if !shares.is_empty() {
            spawn_local(async move {
                if let Ok(m) = call::<Vec<SmbMount>>("smb_mounts", json!({ "shares": shares })).await {
                    if self.smb_mounts.with_untracked(|now| *now != m) {
                        self.smb_mounts.set(m);
                    }
                }
            });
        }
    }
}
