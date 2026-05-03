use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub watch_folder:      PathBuf,
    pub failed_folder:     PathBuf,
    #[serde(default)]
    pub exclude_patterns:  Vec<String>,
    #[serde(default = "default_include_extensions")]
    pub include_extensions: Vec<String>,
    #[serde(default)]
    pub watch_subdirs:     Option<Vec<String>>,
    #[serde(default = "default_min_file_age")]
    pub min_file_age_secs: u64,

    pub conversion:    ConversionConfig,
    pub destinations:  Vec<Destination>,
    #[serde(default)]
    pub transfer:      TransferConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub smtp:          Option<SmtpConfig>,
}

fn default_include_extensions() -> Vec<String> {
    vec!["rar".into(), "7z".into(), "tar.gz".into()]
}
fn default_min_file_age() -> u64 { 5 }

#[derive(Debug, Clone, Deserialize)]
pub struct ConversionConfig {
    #[serde(default = "default_compression_level")]
    pub compression_level:               u8,
    #[serde(default)]
    pub archive_password:                Option<String>,
    /// Files larger than this (MB) use Strategy::Stream (no zip on disk).
    /// 0 = always use Disk strategy.
    #[serde(default = "default_stream_threshold")]
    pub stream_to_transfer_threshold_mb: u64,
    /// Multi-part groups older than this many hours are moved to failed/.
    /// 0 = never expire.
    #[serde(default = "default_multipart_timeout")]
    pub multipart_timeout_hours:         u64,
}

fn default_compression_level() -> u8 { 6 }
fn default_stream_threshold() -> u64 { 500 }
fn default_multipart_timeout() -> u64 { 24 }

impl Default for ConversionConfig {
    fn default() -> Self {
        Self {
            compression_level:               default_compression_level(),
            archive_password:                None,
            stream_to_transfer_threshold_mb: default_stream_threshold(),
            multipart_timeout_hours:         default_multipart_timeout(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Destination {
    pub host:         String,
    #[serde(default = "default_ssh_port")]
    pub port:         u16,
    pub user:         String,
    pub remote_path:  String,
    pub ssh_key_path: PathBuf,
}

fn default_ssh_port() -> u16 { 22 }

#[derive(Debug, Clone, Deserialize)]
pub struct TransferConfig {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent:       usize,
    #[serde(default = "default_max_retries")]
    pub max_retries:          u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_secs:     u64,
    #[serde(default)]
    pub bandwidth_limit_kbps: u64,
}

fn default_max_concurrent() -> usize { 3 }
fn default_max_retries() -> u32 { 3 }
fn default_retry_delay() -> u64 { 10 }

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_concurrent:       default_max_concurrent(),
            max_retries:          default_max_retries(),
            retry_delay_secs:     default_retry_delay(),
            bandwidth_limit_kbps: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "default_metrics_port")]
    pub metrics_port:                u16,
    #[serde(default = "default_dashboard_port")]
    pub dashboard_port:              u16,
    #[serde(default)]
    pub webhook_url:                 Option<String>,
    #[serde(default)]
    pub alert_email:                 Option<String>,
    #[serde(default = "default_queue_depth_threshold")]
    pub alert_queue_depth_threshold: usize,
}

fn default_metrics_port() -> u16 { 9090 }
fn default_dashboard_port() -> u16 { 8080 }
fn default_queue_depth_threshold() -> usize { 50 }

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics_port:                default_metrics_port(),
            dashboard_port:              default_dashboard_port(),
            webhook_url:                 None,
            alert_email:                 None,
            alert_queue_depth_threshold: default_queue_depth_threshold(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmtpConfig {
    pub host:     String,
    pub port:     u16,
    pub user:     String,
    pub password: String,
    pub from:     String,
}

// ---------------------------------------------------------------------------

/// Load config from a TOML file.
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).map_err(AppError::Io)?;
    toml::from_str(&text).map_err(|e| AppError::Config(e.to_string()))
}

/// Validate config. Returns a list of error strings; empty means valid.
pub fn validate(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();

    if !cfg.watch_folder.exists() {
        errors.push(format!(
            "watch_folder does not exist: {}",
            cfg.watch_folder.display()
        ));
    } else if !cfg.watch_folder.is_dir() {
        errors.push(format!(
            "watch_folder is not a directory: {}",
            cfg.watch_folder.display()
        ));
    }

    for dest in &cfg.destinations {
        if !dest.ssh_key_path.exists() {
            errors.push(format!(
                "ssh_key_path does not exist for host {}: {}",
                dest.host,
                dest.ssh_key_path.display()
            ));
        }
    }

    if cfg.conversion.compression_level > 9 {
        errors.push(format!(
            "compression_level must be 0–9 (got {})",
            cfg.conversion.compression_level
        ));
    }

    if cfg.observability.metrics_port == cfg.observability.dashboard_port {
        errors.push(format!(
            "metrics_port and dashboard_port must differ (both are {})",
            cfg.observability.metrics_port
        ));
    }

    errors
}

/// Watch the config file for changes. On a valid change the shared config is
/// atomically replaced. Invalid configs are logged and the current config is
/// kept. Exits when `shutdown` is cancelled.
pub async fn watch_config(
    path: PathBuf,
    config: Arc<RwLock<Config>>,
    shutdown: CancellationToken,
) {
    use notify::{recommended_watcher, RecursiveMode, Watcher};

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("failed to create config watcher: {e}");
            return;
        }
    };

    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        tracing::error!("failed to watch config path {}: {e}", path.display());
        return;
    }

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::debug!("config watcher shutting down");
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                let mut should_reload = false;
                while let Ok(Ok(event)) = rx.try_recv() {
                    use notify::EventKind;
                    if matches!(event.kind, EventKind::Modify(_)) {
                        should_reload = true;
                    }
                }
                if should_reload {
                    match load(&path) {
                        Ok(new_cfg) => {
                            let errs = validate(&new_cfg);
                            if errs.is_empty() {
                                *config.write().await = new_cfg;
                                tracing::info!("config reloaded from {}", path.display());
                            } else {
                                tracing::warn!(
                                    "config reload rejected — validation errors: {:?}", errs
                                );
                            }
                        }
                        Err(e) => tracing::warn!("config reload parse error: {e}"),
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    fn minimal_config_toml(watch_dir: &Path) -> String {
        format!(
            r#"
watch_folder  = "{watch}"
failed_folder = "{watch}/failed"
destinations  = []

[conversion]
compression_level = 6
"#,
            watch = watch_dir.display()
        )
    }

    #[test]
    fn load_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&minimal_config_toml(dir.path()));
        let cfg = load(f.path()).unwrap();
        assert_eq!(cfg.watch_folder, dir.path());
        assert_eq!(cfg.conversion.compression_level, 6);
        assert_eq!(cfg.conversion.multipart_timeout_hours, 24);
        assert_eq!(cfg.transfer.max_concurrent, 3);
        assert_eq!(cfg.observability.metrics_port, 9090);
    }

    #[test]
    fn load_missing_watch_folder() {
        let f = write_config(
            "failed_folder = \"/tmp\"\ndestinations = []\n[conversion]\ncompression_level = 6",
        );
        assert!(matches!(load(f.path()), Err(AppError::Config(_))));
    }

    #[test]
    fn validate_compression_level_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "watch_folder=\"{d}\"\nfailed_folder=\"{d}/f\"\ndestinations=[]\n\
             [conversion]\ncompression_level=10",
            d = dir.path().display()
        ));
        let cfg = load(f.path()).unwrap();
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.contains("compression_level")));
    }

    #[test]
    fn validate_duplicate_observability_ports() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "{}\n[observability]\nmetrics_port=9090\ndashboard_port=9090",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.contains("metrics_port")));
    }

    #[test]
    fn multipart_timeout_hours_defaults_to_24() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&minimal_config_toml(dir.path()));
        let cfg = load(f.path()).unwrap();
        assert_eq!(cfg.conversion.multipart_timeout_hours, 24);
    }

    #[tokio::test]
    async fn hot_reload_updates_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = NamedTempFile::new_in(dir.path()).unwrap();
        f.write_all(minimal_config_toml(dir.path()).as_bytes()).unwrap();
        f.flush().unwrap();

        let cfg = load(f.path()).unwrap();
        let shared = Arc::new(RwLock::new(cfg));
        let token = CancellationToken::new();

        let path = f.path().to_path_buf();
        let shared_clone = shared.clone();
        let token_clone = token.clone();
        tokio::spawn(async move {
            watch_config(path, shared_clone, token_clone).await;
        });

        tokio::time::sleep(Duration::from_millis(200)).await;

        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            write!(
                file,
                "{}\n[conversion]\ncompression_level=1\nmultipart_timeout_hours=48",
                minimal_config_toml(dir.path())
            )
            .unwrap();
        }

        tokio::time::sleep(Duration::from_secs(1)).await;

        let level = shared.read().await.conversion.compression_level;
        token.cancel();
        assert_eq!(level, 1);
    }
}
