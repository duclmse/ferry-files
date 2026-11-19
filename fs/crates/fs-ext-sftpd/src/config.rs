//! `[extensions.sftpd]` in the daemon's config file.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Loopback by default, like the daemon's own APIs.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Every session is confined to this folder, which clients see as `/`.
    pub root: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    /// OpenSSH private key; an Ed25519 key is generated here if missing.
    pub host_key: PathBuf,
    pub users: Vec<User>,
    /// Sessions idle this long are closed; 0 keeps them open.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    /// OpenSSH `authorized_keys` file; reread on every login.
    pub authorized_keys: PathBuf,
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 2222))
}

fn default_idle_timeout() -> u64 {
    600
}

pub fn load(path: &Path) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: toml::Table = toml::from_str(&text).context("config parse")?;
    let section = root
        .get("extensions")
        .and_then(|e| e.get("sftpd"))
        .cloned()
        .ok_or_else(|| anyhow!("missing [extensions.sftpd] section"))?;
    section
        .try_into()
        .map_err(|e: toml::de::Error| anyhow!("[extensions.sftpd]: {e}"))
}

/// Every problem found, one per entry; empty means valid.
pub fn validate(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();
    if !cfg.root.is_dir() {
        errors.push(format!("root is not a folder: {}", cfg.root.display()));
    }
    if cfg.host_key.exists() {
        if let Err(e) = russh::keys::load_secret_key(&cfg.host_key, None) {
            errors.push(format!("host_key {}: {e}", cfg.host_key.display()));
        }
    } else if !cfg.host_key.parent().is_some_and(Path::is_dir) {
        errors.push(format!(
            "host_key {} does not exist and its folder does not either",
            cfg.host_key.display()
        ));
    }
    if cfg.users.is_empty() {
        errors.push("no [[extensions.sftpd.users]]; nobody could log in".into());
    }
    let mut seen = HashSet::new();
    for user in &cfg.users {
        if user.name.is_empty() {
            errors.push("a user has an empty name".into());
        } else if !seen.insert(user.name.as_str()) {
            errors.push(format!("user {} is listed twice", user.name));
        }
        match crate::keys::read_authorized_keys(&user.authorized_keys) {
            Ok(keys) if keys.is_empty() => errors.push(format!(
                "user {}: no keys in {}",
                user.name,
                user.authorized_keys.display()
            )),
            Ok(_) => {}
            Err(e) => errors.push(format!("user {}: {e}", user.name)),
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn fwd(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn reads_its_section_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let d = fwd(dir.path());
        let cfg = write(
            dir.path(),
            "c.toml",
            &format!(
                r#"
watch_paths = []
[extensions.sftpd]
root = "{d}"
host_key = "{d}/host"
[[extensions.sftpd.users]]
name = "up"
authorized_keys = "{d}/ak"
"#
            ),
        );
        let c = load(&cfg).unwrap();
        assert_eq!(c.listen, default_listen());
        assert!(!c.read_only);
        assert_eq!(c.idle_timeout_secs, 600);
        assert_eq!(c.users[0].name, "up");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = write(
            dir.path(),
            "c.toml",
            "[extensions.sftpd]\nroot = \"/\"\nhost_key = \"/k\"\nusers = []\npassword = \"x\"\n",
        );
        let err = load(&cfg).unwrap_err().to_string();
        assert!(err.contains("password"), "{err}");
    }

    #[test]
    fn validate_reports_each_problem() {
        let dir = tempfile::tempdir().unwrap();
        let empty = write(dir.path(), "empty", "# nothing\n");
        let c = Config {
            listen: default_listen(),
            root: dir.path().join("missing"),
            read_only: false,
            host_key: dir.path().join("no/such/host"),
            users: vec![
                User {
                    name: "a".into(),
                    authorized_keys: empty.clone(),
                },
                User {
                    name: "a".into(),
                    authorized_keys: dir.path().join("none"),
                },
            ],
            idle_timeout_secs: 0,
        };
        let errs = validate(&c);
        for want in [
            "root is not a folder",
            "host_key",
            "listed twice",
            "no keys in",
            "user a: ",
        ] {
            assert!(errs.iter().any(|e| e.contains(want)), "{want}: {errs:?}");
        }
    }
}
