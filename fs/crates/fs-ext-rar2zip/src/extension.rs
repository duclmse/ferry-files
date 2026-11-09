//! Hooks rar2zip into the `fs` daemon.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::extension::{Extension, ExtensionFuture};
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};
use crate::error::Result;
use crate::queue::JobQueue;

const PING_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Rar2zipExtension {
    config_path: PathBuf,
    config: Config,
}

impl Rar2zipExtension {
    pub fn load(config_path: &Path) -> Result<Self> {
        Ok(Self {
            config_path: config_path.to_path_buf(),
            config: config::load(config_path)?,
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
    fn name(&self) -> &'static str {
        "rar2zip"
    }

    fn validate(&self) -> Vec<String> {
        let mut errors = config::validate(&self.config);
        for dest in &self.config.destinations {
            let addr = format!("{}:{}", dest.host, dest.port);
            if let Err(e) = ping(&addr) {
                errors.push(format!("destination {addr}: {e}"));
            }
        }
        errors
    }

    fn run(&self, shutdown: CancellationToken) -> ExtensionFuture {
        let config = self.config.clone();
        let config_path = self.config_path.clone();
        Box::pin(async move {
            crate::app::run(config, config_path, shutdown).await?;
            Ok(())
        })
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
    fn unknown_command_is_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(extension(&dir).command("nope").is_err());
    }
}
