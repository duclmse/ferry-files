//! rar2zip's implementation of the extension contract.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::extension::{Extension, ExtensionFuture};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};
use crate::error::Result;
use crate::hostkey::{self, HostKeyCheck};
use crate::queue::JobQueue;

const PING_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Rar2zipExtension {
    config_path: PathBuf,
    /// As loaded at startup: the queue and `validate` use it.
    config: Config,
    /// What jobs run with; `reload` replaces it.
    live: Arc<RwLock<Config>>,
}

impl Rar2zipExtension {
    pub fn load(config_path: &Path) -> Result<Self> {
        let config = config::load(config_path)?;
        Ok(Self {
            config_path: config_path.to_path_buf(),
            live: Arc::new(RwLock::new(config.clone())),
            config,
        })
    }

    fn queue(&self) -> Result<JobQueue> {
        JobQueue::new(&self.config.watch_folder.join(".rar2zip.db"))
    }

    fn status(&self) -> anyhow::Result<String> {
        let depth = self.queue()?.queue_depth()?;
        let mut out = format!("{:<14} COUNT\n{}", "STATUS", "-".repeat(20));
        for status in ["pending", "converting", "transferring", "done", "failed"] {
            let count = depth.get(status).copied().unwrap_or(0);
            out.push_str(&format!("\n{status:<14} {count}"));
        }
        Ok(out)
    }
}

impl Extension for Rar2zipExtension {
    fn name(&self) -> &str {
        "rar2zip"
    }

    fn validate(&self) -> Vec<String> {
        let mut errors = config::validate(&self.config);
        for dest in &self.config.destinations {
            let addr = format!("{}:{}", dest.host, dest.port);
            if let Err(e) = ping(&addr) {
                errors.push(format!("destination {addr}: {e}"));
            }
            let Ok(policy) = self.config.host_key_policy(dest) else {
                continue;
            };
            if policy.check == HostKeyCheck::Strict && policy.pins.is_empty() {
                match hostkey::is_known(&policy.known_hosts, &dest.host, dest.port) {
                    Ok(true) => {}
                    Ok(false) => errors.push(format!(
                        "destination {addr}: not in {}; add it (ssh-keyscan), pin host_keys, \
                         or set host_key_check = \"accept-new\"",
                        policy.known_hosts.display()
                    )),
                    Err(e) => errors.push(format!("destination {addr}: {e}")),
                }
            }
        }
        errors
    }

    fn run(&self, shutdown: CancellationToken) -> ExtensionFuture {
        let live = Arc::clone(&self.live);
        Box::pin(async move {
            crate::app::run(live, shutdown).await?;
            Ok(())
        })
    }

    fn reload(&self) -> anyhow::Result<()> {
        let next = config::load(&self.config_path)?;
        let errors = config::validate(&next);
        if !errors.is_empty() {
            anyhow::bail!("kept the running settings: {}", errors.join("; "));
        }
        *self.live.blocking_write() = next;
        tracing::info!("settings reloaded from {}", self.config_path.display());
        Ok(())
    }

    fn command(&self, command: &str) -> anyhow::Result<String> {
        match command {
            "status" => self.status(),
            "retry-failed" => {
                let reset = self.queue()?.reset_failed()?;
                Ok(format!("Reset {reset} failed job(s) to pending."))
            }
            other => anyhow::bail!("unknown rar2zip command: {other}"),
        }
    }
}

fn ping(addr: &str) -> std::io::Result<()> {
    let target = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "host did not resolve"))?;
    TcpStream::connect_timeout(&target, PING_TIMEOUT).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn extension(dir: &TempDir) -> Rar2zipExtension {
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        let folder = dir.path().to_string_lossy().replace('\\', "/");
        write!(
            f,
            r#"
[extensions.rar2zip]
watch_folder = "{folder}"
failed_folder = "{folder}"
destinations = []

[extensions.rar2zip.conversion]
"#
        )
        .unwrap();
        Rar2zipExtension::load(&path).unwrap()
    }

    #[test]
    fn status_lists_every_state() {
        let dir = TempDir::new().unwrap();
        let out = extension(&dir).command("status").unwrap();
        for status in ["pending", "converting", "transferring", "done", "failed"] {
            assert!(out.contains(status), "{out}");
        }
    }

    #[test]
    fn retry_failed_reports_count() {
        let dir = TempDir::new().unwrap();
        let out = extension(&dir).command("retry-failed").unwrap();
        assert_eq!(out, "Reset 0 failed job(s) to pending.");
    }

    #[test]
    fn reload_takes_valid_settings_and_keeps_the_running_ones_otherwise() {
        let dir = TempDir::new().unwrap();
        let ext = extension(&dir);
        let path = dir.path().join("config.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}compression_level = 1\n")).unwrap();
        ext.reload().unwrap();
        assert_eq!(ext.live.blocking_read().conversion.compression_level, 1);

        std::fs::write(&path, format!("{text}compression_level = 99\n")).unwrap();
        let err = ext.reload().unwrap_err().to_string();
        assert!(err.contains("kept the running settings"), "{err}");
        assert_eq!(ext.live.blocking_read().conversion.compression_level, 1);
    }

    #[test]
    fn unknown_command_is_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(extension(&dir).command("nope").is_err());
    }
}
