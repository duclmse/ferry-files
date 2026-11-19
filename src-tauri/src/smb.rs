//! SMB shares (Windows file sharing, NAS boxes): mounted by the system, which asks for the
//! password itself, so the app never sees it. A mounted share is a folder on this computer, so
//! everything that works on the computer's files works on it.

use super::*;
use percent_encoding::percent_decode_str;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SmbMount {
    /// "smb://[user@]host/share", as kept in the sidebar.
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

/// The host (without the user, in lower case, and without a Bonjour suffix) and the share (in
/// lower case, decoded) of "smb://me@host/share" or "//me@host/share".
fn key(share: &str) -> (String, String) {
    let t = share.trim_start_matches("smb:").trim_start_matches('/');
    let (host, name) = t.split_once('/').unwrap_or((t, ""));
    let host = host.rsplit('@').next().unwrap_or(host).to_lowercase();
    let host = host.trim_end_matches("._smb._tcp.local").trim_end_matches(".local").to_string();
    let name = percent_decode_str(name.split('/').next().unwrap_or_default()).decode_utf8_lossy().to_lowercase();
    (host, name)
}

fn mounted() -> Vec<SmbMount> {
    crate::platform::smb_mounted().into_iter().map(|(what, path)| SmbMount { share: format!("smb:{what}"), path }).collect()
}

/// Where `share` is mounted, if it is.
fn mounted_at(share: &str, mounts: &[SmbMount]) -> Option<String> {
    let k = key(share);
    mounts.iter().find(|m| key(&m.share) == k).map(|m| m.path.clone())
}

/// The shares mounted now, each with the address kept in the sidebar when it's one of `shares`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn smb_mounts(shares: Vec<String>) -> Result<Vec<SmbMount>, String> {
    blocking(move || {
        let mounts = mounted();
        Ok(shares.into_iter().filter_map(|s| mounted_at(&s, &mounts).map(|path| SmbMount { share: s, path })).collect())
    })
    .await
}

/// Mounts `share` (when it isn't already) and says where.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn smb_mount(share: String) -> Result<SmbMount, String> {
    blocking(move || {
        let share = share_url(&share)?;
        if let Some(path) = mounted_at(&share, &mounted()) {
            return Ok(SmbMount { share, path });
        }
        crate::platform::smb_mount(&share)?;
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

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn smb_unmount(path: String) -> Result<(), String> {
    blocking(move || crate::platform::smb_unmount(&path)).await
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
        assert_eq!(key("smb://me@NAS.local/My%20Photos"), ("nas".into(), "my photos".into()));
        let mounts = vec![SmbMount { share: "smb://me@nas._smb._tcp.local/Photos".into(), path: "/Volumes/Photos".into() }];
        assert_eq!(mounted_at("smb://nas/photos", &mounts).as_deref(), Some("/Volumes/Photos"));
        assert_eq!(mounted_at("smb://nas/Music", &mounts), None);
    }
}
