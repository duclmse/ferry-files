//! sftpd's implementation of the extension contract.

use std::path::Path;

use fs_common::extension::{Extension, ExtensionFuture};
use tokio_util::sync::CancellationToken;

use crate::config::{self, Config};

pub struct SftpdExtension {
    config: Config,
}

impl SftpdExtension {
    pub fn load(config_path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            config: config::load(config_path)?,
        })
    }

    fn status(&self) -> anyhow::Result<String> {
        let c = &self.config;
        let key = if c.host_key.exists() {
            let key = russh::keys::load_secret_key(&c.host_key, None)?;
            crate::keys::fingerprint(key.public_key())
        } else {
            "not created yet".into()
        };
        let users: Vec<&str> = c.users.iter().map(|u| u.name.as_str()).collect();
        Ok(format!(
            "listen     {}\nroot       {}{}\nhost key   {key}\nusers      {}",
            c.listen,
            c.root.display(),
            if c.read_only { " (read-only)" } else { "" },
            users.join(", "),
        ))
    }
}

impl Extension for SftpdExtension {
    fn name(&self) -> &str {
        "sftpd"
    }

    fn validate(&self) -> Vec<String> {
        config::validate(&self.config)
    }

    fn run(&self, shutdown: CancellationToken) -> ExtensionFuture {
        let config = self.config.clone();
        Box::pin(async move {
            let running = crate::server::bind(&config).await?;
            running.serve(shutdown).await?;
            Ok(())
        })
    }

    fn command(&self, command: &str) -> anyhow::Result<String> {
        match command {
            "status" => self.status(),
            other => anyhow::bail!("unknown sftpd command: {other} (try status)"),
        }
    }
}
