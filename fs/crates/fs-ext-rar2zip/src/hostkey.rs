//! Verifies an SFTP server's host key before rar2zip authenticates to it.
//!
//! A destination is trusted through pinned fingerprints when it has any, and
//! otherwise through an OpenSSH `known_hosts` file.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use russh::keys::ssh_key::Fingerprint;
use russh::keys::{known_hosts, HashAlg, PublicKey};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HostKeyCheck {
    /// Only keys already in `known_hosts` (or pinned) are accepted.
    #[default]
    Strict,
    /// A host seen for the first time is recorded and trusted; a changed key
    /// is still rejected.
    AcceptNew,
    /// Any key is accepted.
    Off,
}

/// Everything needed to judge one destination's key.
#[derive(Debug, Clone)]
pub struct HostKeyPolicy {
    pub check: HostKeyCheck,
    pub known_hosts: PathBuf,
    pub pins: Vec<String>,
}

/// Two transfers to a new host under `accept-new` would otherwise both find
/// it missing and append it twice.
static KNOWN_HOSTS_LOCK: Mutex<()> = Mutex::new(());

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Checks that each pin is a fingerprint such as `SHA256:...`.
pub fn invalid_pins(pins: &[String]) -> Vec<String> {
    pins.iter()
        .filter(|p| Fingerprint::from_str(p).is_err())
        .cloned()
        .collect()
}

/// `Ok(())` when the key is trusted, otherwise why it is not.
pub fn verify(
    policy: &HostKeyPolicy,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<(), String> {
    let seen = fingerprint(key);
    if policy.check == HostKeyCheck::Off {
        tracing::warn!("{host}:{port}: host key {seen} accepted without verification (host_key_check = \"off\")");
        return Ok(());
    }

    if !policy.pins.is_empty() {
        let pinned = policy
            .pins
            .iter()
            .filter_map(|p| Fingerprint::from_str(p).ok())
            .any(|p| key.fingerprint(p.algorithm()) == p);
        return if pinned {
            Ok(())
        } else {
            Err(format!(
                "{host}:{port}: host key {seen} matches none of the pinned host_keys"
            ))
        };
    }

    let path = &policy.known_hosts;
    let _guard = KNOWN_HOSTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match known_hosts::check_known_hosts_path(host, port, key, path) {
        Ok(true) => Ok(()),
        Ok(false) if policy.check == HostKeyCheck::AcceptNew => {
            known_hosts::learn_known_hosts_path(host, port, key, path)
                .map_err(|e| format!("record {host}:{port} in {}: {e}", path.display()))?;
            tracing::info!(
                "{host}:{port}: first connection, recorded host key {seen} in {}",
                path.display()
            );
            Ok(())
        }
        Ok(false) => Err(format!(
            "{host}:{port}: host key {seen} is not in {}; add it (ssh-keyscan) or pin it in host_keys",
            path.display()
        )),
        Err(russh::keys::Error::KeyChanged { line }) => Err(format!(
            "{host}:{port}: HOST KEY CHANGED, now {seen}, which differs from {} line {line}; refusing to connect",
            path.display()
        )),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Whether `known_hosts` lists `host` at all, for `validate`.
pub fn is_known(path: &Path, host: &str, port: u16) -> Result<bool, String> {
    known_hosts::known_host_keys_path(host, port, path)
        .map(|keys| !keys.is_empty())
        .map_err(|e| format!("read {}: {e}", path.display()))
}

/// `~/.ssh/known_hosts`, the file OpenSSH itself uses.
pub fn default_known_hosts() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

/// Expands a leading `~/`, which TOML paths cannot do on their own.
pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
    use russh::keys::PrivateKey;

    fn key(seed: u8) -> PublicKey {
        let pair = Ed25519Keypair::from_seed(&[seed; 32]);
        PrivateKey::new(KeypairData::Ed25519(pair), "")
            .unwrap()
            .public_key()
            .clone()
    }

    fn policy(check: HostKeyCheck, known_hosts: &Path, pins: &[String]) -> HostKeyPolicy {
        HostKeyPolicy {
            check,
            known_hosts: known_hosts.to_path_buf(),
            pins: pins.to_vec(),
        }
    }

    #[test]
    fn strict_accepts_only_recorded_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("known_hosts");
        let p = policy(HostKeyCheck::Strict, &file, &[]);

        let err = verify(&p, "sftp.example", 2222, &key(1)).unwrap_err();
        assert!(err.contains("is not in"), "{err}");
        assert!(!file.exists(), "strict never writes known_hosts");

        known_hosts::learn_known_hosts_path("sftp.example", 2222, &key(1), &file).unwrap();
        assert!(verify(&p, "sftp.example", 2222, &key(1)).is_ok());
        assert!(
            verify(&p, "sftp.example", 22, &key(1)).is_err(),
            "the port is part of the host's identity"
        );
        let changed = verify(&p, "sftp.example", 2222, &key(2)).unwrap_err();
        assert!(changed.contains("HOST KEY CHANGED"), "{changed}");
    }

    #[test]
    fn accept_new_records_the_first_key_and_then_holds_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ssh").join("known_hosts");
        let p = policy(HostKeyCheck::AcceptNew, &file, &[]);

        assert!(verify(&p, "h", 22, &key(1)).is_ok());
        assert!(is_known(&file, "h", 22).unwrap());
        assert!(verify(&p, "h", 22, &key(1)).is_ok());
        let text = std::fs::read_to_string(&file).unwrap();
        // russh starts a new file with an empty line.
        let entries = text.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(entries, 1, "recorded once: {text}");

        let changed = verify(&p, "h", 22, &key(2)).unwrap_err();
        assert!(changed.contains("HOST KEY CHANGED"), "{changed}");
    }

    #[test]
    fn pins_replace_known_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("known_hosts");
        known_hosts::learn_known_hosts_path("h", 22, &key(1), &file).unwrap();

        let pinned = policy(HostKeyCheck::Strict, &file, &[fingerprint(&key(2))]);
        assert!(verify(&pinned, "h", 22, &key(2)).is_ok());
        let err = verify(&pinned, "h", 22, &key(1)).unwrap_err();
        assert!(
            err.contains("pinned"),
            "known_hosts is not consulted: {err}"
        );

        let either = policy(
            HostKeyCheck::AcceptNew,
            &file,
            &[fingerprint(&key(3)), fingerprint(&key(2))],
        );
        assert!(verify(&either, "other", 22, &key(2)).is_ok());
        assert!(verify(&either, "other", 22, &key(4)).is_err());
        assert!(!is_known(&file, "other", 22).unwrap(), "pins never learn");
    }

    #[test]
    fn off_accepts_anything() {
        let dir = tempfile::tempdir().unwrap();
        let p = policy(
            HostKeyCheck::Off,
            &dir.path().join("none"),
            &["bogus".into()],
        );
        assert!(verify(&p, "h", 22, &key(9)).is_ok());
    }

    #[test]
    fn pins_must_be_fingerprints() {
        let good = fingerprint(&key(1));
        assert!(good.starts_with("SHA256:"));
        let bad = vec!["SHA256:".to_owned(), "aa:bb".to_owned()];
        let all: Vec<String> = [vec![good], bad.clone()].concat();
        assert_eq!(invalid_pins(&all), bad);
    }

    #[test]
    fn leading_tilde_is_the_home_folder() {
        let home = home_dir().unwrap();
        assert_eq!(expand_home(Path::new("~/.ssh/kh")), home.join(".ssh/kh"));
        assert_eq!(expand_home(Path::new("/etc/kh")), PathBuf::from("/etc/kh"));
    }
}
