//! The one setting the client needs from the shared config file.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Deserialize, Default)]
struct ClientConfig {
    #[serde(default)]
    api: ClientApi,
}

#[derive(Deserialize)]
struct ClientApi {
    #[serde(default = "fs_protocol::default_ipc_path")]
    ipc_path: PathBuf,
}

impl Default for ClientApi {
    fn default() -> Self {
        Self {
            ipc_path: fs_protocol::default_ipc_path(),
        }
    }
}

pub fn load_ipc_path(path: &Path) -> anyhow::Result<PathBuf> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("config load error: {}", path.display()))?;
    parse_ipc_path(&text)
}

fn parse_ipc_path(text: &str) -> anyhow::Result<PathBuf> {
    let cfg: ClientConfig = toml::from_str(text).context("config parse error")?;
    Ok(cfg.api.ipc_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ipc_path_and_ignores_server_settings() {
        let text = r#"
watch_paths = ["/data"]
[api]
rest_port = 1
ipc_path = "/run/custom.sock"
[extensions.rar2zip]
watch_folder = "/in"
"#;
        assert_eq!(
            parse_ipc_path(text).unwrap(),
            PathBuf::from("/run/custom.sock")
        );
    }

    #[test]
    fn missing_api_section_uses_the_default_path() {
        assert_eq!(
            parse_ipc_path("watch_paths = []").unwrap(),
            fs_protocol::default_ipc_path()
        );
    }
}
