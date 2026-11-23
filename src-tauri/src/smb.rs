//! Network shares mounted by the system: SMB shares (Windows file sharing, NAS boxes) and NFS
//! exports. With a password saved in the keychain the app gives it to the system; without one
//! the system asks for it itself (NFS has none: it goes by this computer's user). A mounted
//! share is a folder on this computer, so everything that works on the computer's files works
//! on it. The shares the system mounted otherwise (WebDAV, FTP, AFP too) are listed, to unmount.

use super::*;
use percent_encoding::percent_decode_str;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SmbMount {
    /// "smb://[user@]host/share" or "nfs://host/export", as kept in the sidebar.
    pub share: String,
    /// Where it's mounted (an app path).
    pub path: String,
}

/// "smb://[user@]host/share" from "smb://host/share/folder", "\\host\share" or "host/share".
pub fn share_url(text: &str) -> std::result::Result<String, String> {
    let t = text.trim().replace('\\', "/");
    let t = t.strip_prefix("smb:").unwrap_or(&t).trim_start_matches('/');
    let mut parts = t.split('/').filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some(host), Some(share)) if !host.contains(char::is_whitespace) => Ok(format!("smb://{host}/{share}")),
        _ => Err("Enter the server and share, like smb://nas/Photos.".into()),
    }
}

/// "nfs://host/export/path" from that or "host:/export/path". The whole path is kept: an NFS
/// server may let any folder under an export be mounted.
pub fn nfs_url(text: &str) -> std::result::Result<String, String> {
    let t = text.trim();
    let (host, path) = match t.strip_prefix("nfs://") {
        Some(rest) => rest.split_once('/').unwrap_or((rest, "")),
        None => t.split_once(":/").unwrap_or((t, "")),
    };
    let path = path.trim_matches('/');
    if host.is_empty() || path.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '/') {
        return Err("Enter the server and exported folder, like nfs://nas/export/photos.".into());
    }
    Ok(format!("nfs://{host}/{path}"))
}

/// Whether `text` names an NFS export ("nfs://..." or "host:/path").
fn is_nfs(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("nfs://") || (!t.contains("://") && t.contains(":/"))
}

/// What two addresses of the same share have in common: the scheme, the host (without the
/// user, in lower case, and without a Bonjour suffix) and the share (decoded; in lower case but
/// for NFS, whose paths are a Unix computer's).
fn key(share: &str) -> (String, String, String) {
    let (scheme, t) = share.split_once("://").unwrap_or(("smb", share.trim_start_matches("smb:").trim_start_matches('/')));
    let (host, name) = t.split_once('/').unwrap_or((t, ""));
    let host = host.rsplit('@').next().unwrap_or(host).to_lowercase();
    let host = host.trim_end_matches("._smb._tcp.local").trim_end_matches("._afpovertcp._tcp.local").trim_end_matches(".local").to_string();
    let name = match scheme {
        "smb" => percent_decode_str(name.split('/').next().unwrap_or_default()).decode_utf8_lossy().to_lowercase(),
        "nfs" => percent_decode_str(name.trim_end_matches('/')).decode_utf8_lossy().into_owned(),
        _ => percent_decode_str(name.trim_end_matches('/')).decode_utf8_lossy().to_lowercase(),
    };
    (scheme.to_lowercase(), host, name)
}

fn mounted() -> Vec<SmbMount> {
    crate::platform::net_mounted().into_iter().map(|(share, path)| SmbMount { share, path }).collect()
}

/// Where `share` is mounted, if it is.
fn mounted_at(share: &str, mounts: &[SmbMount]) -> Option<String> {
    let k = key(share);
    mounts.iter().find(|m| key(&m.share) == k).map(|m| m.path.clone())
}

/// All the network shares mounted now, those that are one of `shares` with that address; the
/// others (mounted outside the app) as the system names them.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn smb_mounts(shares: Vec<String>) -> Result<Vec<SmbMount>, String> {
    blocking(move || Ok(name_mounts(&shares, mounted()))).await
}

fn name_mounts(shares: &[String], mounts: Vec<SmbMount>) -> Vec<SmbMount> {
    mounts
        .into_iter()
        .map(|m| {
            let k = key(&m.share);
            match shares.iter().find(|s| key(s) == k) {
                Some(s) => SmbMount { share: s.clone(), path: m.path },
                None => m,
            }
        })
        .collect()
}

/// Mounts `share` ("smb://..." or "nfs://...", when it isn't already) and says where. With
/// `user`, logs in to an SMB share as that user (in `domain`, if given) with the password saved
/// for connection `secret`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn smb_mount(share: String, user: Option<String>, domain: Option<String>, secret: Option<String>) -> Result<SmbMount, String> {
    blocking(move || {
        let nfs = is_nfs(&share);
        let share = if nfs { nfs_url(&share)? } else { share_url(&share)? };
        if let Some(path) = mounted_at(&share, &mounted()) {
            return Ok(SmbMount { share, path });
        }
        let user = user.map(|u| u.trim().to_string()).filter(|u| !u.is_empty() && !nfs);
        let password = secret.as_deref().and_then(crate::secrets::get).unwrap_or_default();
        let domain = domain.unwrap_or_default();
        let login = user.as_deref().map(|user| crate::platform::SmbLogin { user, domain: domain.trim(), password: &password });
        crate::platform::smb_mount(&share, login.as_ref())?;
        // The system may take a moment to show it.
        for _ in 0..20 {
            if let Some(path) = mounted_at(&share, &mounted()) {
                return Ok(SmbMount { share, path });
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Err(format!("{share} was mounted, but the app can't find where."))
    })
    .await
}

/// Unmounts the network share mounted at `path`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn smb_unmount(path: String) -> Result<(), String> {
    blocking(move || {
        if !mounted().iter().any(|m| m.path == path) {
            return Err(format!("{path} isn't a network share."));
        }
        crate::platform::smb_unmount(&path)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares() {
        assert_eq!(share_url("smb://me@nas/Photos/2024").unwrap(), "smb://me@nas/Photos");
        assert_eq!(share_url(r"\\nas\Photos").unwrap(), "smb://nas/Photos");
        assert_eq!(share_url("nas/Photos").unwrap(), "smb://nas/Photos");
        assert!(share_url("smb://nas").is_err());
        assert_eq!(key("smb://me@NAS.local/My%20Photos"), ("smb".into(), "nas".into(), "my photos".into()));
        let mounts = vec![SmbMount { share: "smb://me@nas._smb._tcp.local/Photos".into(), path: "/Volumes/Photos".into() }];
        assert_eq!(mounted_at("smb://nas/photos", &mounts).as_deref(), Some("/Volumes/Photos"));
        assert_eq!(mounted_at("smb://nas/Music", &mounts), None);
        assert_eq!(mounted_at("nfs://nas/Photos", &mounts), None);
        let named = name_mounts(
            &["smb://nas/Photos".into()],
            vec![
                SmbMount { share: "smb://me@nas._smb._tcp.local/Photos".into(), path: "/Volumes/Photos".into() },
                SmbMount { share: "smb://pc/Music".into(), path: "/Volumes/Music".into() },
            ],
        );
        assert_eq!(named[0].share, "smb://nas/Photos");
        assert_eq!(named[1].share, "smb://pc/Music");
    }

    #[test]
    fn nfs_exports() {
        assert_eq!(nfs_url("nfs://nas/export/photos/").unwrap(), "nfs://nas/export/photos");
        assert_eq!(nfs_url("nas:/export/photos").unwrap(), "nfs://nas/export/photos");
        assert!(nfs_url("nfs://nas").is_err());
        assert!(nfs_url("nfs://nas/").is_err());
        assert!(is_nfs("nas:/export") && is_nfs("nfs://nas/x") && !is_nfs("smb://nas/x") && !is_nfs("nas/x"));
        let mounts = vec![SmbMount { share: "nfs://NAS.local/export/Photos".into(), path: "/Volumes/Photos".into() }];
        assert_eq!(mounted_at("nfs://nas/export/Photos", &mounts).as_deref(), Some("/Volumes/Photos"));
        // NFS paths keep their case.
        assert_eq!(mounted_at("nfs://nas/export/photos", &mounts), None);
    }
}
