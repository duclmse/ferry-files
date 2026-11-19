//! Host key and `authorized_keys` handling.

use std::path::Path;

use anyhow::Context;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};

/// Loads the host key, creating an Ed25519 one on first start so the
/// fingerprint clients pin stays the same across restarts.
pub fn load_or_create_host_key(path: &Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        return russh::keys::load_secret_key(path, None)
            .with_context(|| format!("host_key {}", path.display()));
    }
    let key =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).context("generate host key")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Mode 0600 on Unix.
    key.write_openssh_file(path, LineEnding::LF)
        .with_context(|| format!("write host_key {}", path.display()))?;
    tracing::info!(
        "generated host key {} ({})",
        path.display(),
        fingerprint(key.public_key())
    );
    Ok(key)
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The keys in an OpenSSH `authorized_keys` file. Lines that carry options
/// (`from="..." ssh-ed25519 ...`) are read for the key only; the options
/// are not enforced, so such lines are logged.
pub fn read_authorized_keys(path: &Path) -> anyhow::Result<Vec<PublicKey>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse_authorized_keys(&text, path))
}

pub fn parse_authorized_keys(text: &str, source: &Path) -> Vec<PublicKey> {
    let mut keys = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(key) = PublicKey::from_openssh(line) {
            keys.push(key);
            continue;
        }
        let key_at = line
            .match_indices(' ')
            .map(|(i, _)| i + 1)
            .find(|&i| PublicKey::from_openssh(&line[i..]).is_ok());
        match key_at.and_then(|i| PublicKey::from_openssh(&line[i..]).ok()) {
            Some(key) => {
                tracing::warn!(
                    "{} line {}: key options are ignored",
                    source.display(),
                    n + 1
                );
                keys.push(key);
            }
            None => tracing::warn!("{} line {}: not a public key", source.display(), n + 1),
        }
    }
    keys
}

/// Compares key material only; comments differ between copies of one key.
pub fn same_key(a: &PublicKey, b: &PublicKey) -> bool {
    a.key_data() == b.key_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};

    fn public(seed: u8, comment: &str) -> PublicKey {
        let pair = Ed25519Keypair::from_seed(&[seed; 32]);
        PrivateKey::new(KeypairData::Ed25519(pair), comment)
            .unwrap()
            .public_key()
            .clone()
    }

    #[test]
    fn host_key_is_created_once_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys").join("host");
        let first = load_or_create_host_key(&path).unwrap();
        let again = load_or_create_host_key(&path).unwrap();
        assert!(same_key(first.public_key(), again.public_key()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn authorized_keys_skip_comments_junk_and_options() {
        let a = public(1, "a@host").to_openssh().unwrap();
        let b = public(2, "").to_openssh().unwrap();
        let text = format!("# admins\n\n{a}\nnot a key\nfrom=\"10.0.0.0/8\",no-pty {b}\n");
        let keys = parse_authorized_keys(&text, Path::new("ak"));
        assert_eq!(keys.len(), 2);
        assert!(same_key(&keys[0], &public(1, "other comment")));
        assert!(same_key(&keys[1], &public(2, "")));
        assert!(!same_key(&keys[0], &keys[1]));
    }
}
