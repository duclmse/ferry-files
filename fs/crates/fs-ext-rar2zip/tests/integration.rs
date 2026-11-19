//! Integration tests for rar2zip.
//!
//! These tests require a live SFTP server and are gated behind the
//! `integration` feature flag. They are skipped gracefully (not failed)
//! when the required environment variables are absent.
//!
//! Required env vars:
//!   TEST_SFTP_HOST - hostname or IP of the SFTP server
//!   TEST_SFTP_USER - SSH username
//!   TEST_SFTP_REMOTE_PATH - absolute remote directory for uploads
//!   TEST_SFTP_KEY - path to the private key file
//!   TEST_SFTP_PORT - optional, default 22
//!
//! The server's host key must be in `~/.ssh/known_hosts`, or pinned with
//! TEST_SFTP_HOST_KEY (`SHA256:...`).
//!
//! Run with:
//!   cargo test -p fs-ext-rar2zip --features integration --test integration

#[cfg(feature = "integration")]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use fs_ext_rar2zip::config::{
        Config, ConversionConfig, Destination, ObservabilityConfig, TransferConfig,
    };

    // -----------------------------------------------------------------------
    // Helper: read SFTP env vars or return None (caller skips the test).
    // -----------------------------------------------------------------------

    struct SftpEnv {
        host: String,
        port: u16,
        user: String,
        remote_path: String,
        key_path: PathBuf,
    }

    fn sftp_env() -> Option<SftpEnv> {
        let host = std::env::var("TEST_SFTP_HOST").ok()?;
        let user = std::env::var("TEST_SFTP_USER").ok()?;
        let remote_path = std::env::var("TEST_SFTP_REMOTE_PATH").ok()?;
        let key_path = PathBuf::from(std::env::var("TEST_SFTP_KEY").ok()?);
        let port = match std::env::var("TEST_SFTP_PORT") {
            Ok(p) => p.parse().expect("TEST_SFTP_PORT must be a port number"),
            Err(_) => 22,
        };
        Some(SftpEnv {
            host,
            port,
            user,
            remote_path,
            key_path,
        })
    }

    // -----------------------------------------------------------------------
    // Helper: build a minimal Config for the given temp directories.
    // -----------------------------------------------------------------------

    fn make_config(
        watch: &TempDir,
        failed: &TempDir,
        sftp: &SftpEnv,
        stream_threshold_mb: u64,
    ) -> Config {
        Config {
            watch_folder: watch.path().to_path_buf(),
            failed_folder: failed.path().to_path_buf(),
            include_extensions: vec!["tar".into()],
            exclude_patterns: vec![],
            watch_subdirs: None,
            min_file_age_secs: 0,
            conversion: ConversionConfig {
                compression_level: 6,
                archive_password: None,
                stream_to_transfer_threshold_mb: stream_threshold_mb,
                multipart_timeout_hours: 24,
            },
            destinations: vec![Destination {
                host: sftp.host.clone(),
                port: sftp.port,
                user: sftp.user.clone(),
                remote_path: sftp.remote_path.clone(),
                ssh_key_path: sftp.key_path.clone(),
                host_key_check: None,
                host_keys: std::env::var("TEST_SFTP_HOST_KEY").into_iter().collect(),
            }],
            transfer: TransferConfig {
                max_concurrent: 1,
                max_retries: 1,
                retry_delay_secs: 1,
                bandwidth_limit_kbps: 0,
                ..TransferConfig::default()
            },
            observability: ObservabilityConfig::default(),
            smtp: None,
        }
    }

    // -----------------------------------------------------------------------
    // Helper: create a tiny synthetic ZIP fixture in the watch folder.
    // -----------------------------------------------------------------------

    /// A tar archive holding `size_bytes` of data, so the stream threshold
    /// can be tested. rar2zip converts tar like rar and 7z.
    fn place_tar_fixture(watch: &TempDir, name: &str, size_bytes: usize) -> PathBuf {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(size_bytes as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "data.txt", &vec![b'A'; size_bytes][..])
            .unwrap();
        let path = watch.path().join(name);
        std::fs::write(&path, tar.into_inner().unwrap()).unwrap();
        path
    }

    // -----------------------------------------------------------------------
    // Helper: poll until `predicate` returns true or timeout expires.
    // Returns false on timeout so callers can assert.
    // -----------------------------------------------------------------------

    async fn wait_for(timeout_secs: u64, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            if predicate() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    // -----------------------------------------------------------------------
    // Test: Disk strategy - small archive → ZIP created locally, then uploaded.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_end_to_end_disk_strategy() {
        let Some(sftp) = sftp_env() else {
            eprintln!("SKIP test_end_to_end_disk_strategy - TEST_SFTP_* env vars not set");
            return;
        };

        let watch = TempDir::new().unwrap();
        let failed = TempDir::new().unwrap();

        // Threshold 500 MB → a tiny fixture uses Disk strategy.
        let config = make_config(&watch, &failed, &sftp, 500);
        let shutdown = CancellationToken::new();
        let shutdown_c = shutdown.clone();

        tokio::spawn(async move {
            let _ =
                fs_ext_rar2zip::app::run(config, PathBuf::from("config.toml"), shutdown_c).await;
        });

        // Give the service a moment to start the watcher.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Drop a tiny ZIP into the watch folder.
        let fixture = place_tar_fixture(&watch, "small.tar", 1024);

        // Wait up to 60 s for the source file to be removed (job completed).
        let removed = wait_for(60, || !fixture.exists()).await;

        shutdown.cancel();

        assert!(
            removed,
            "source archive must be removed after successful Disk-strategy job"
        );

        // Verify no failed/ entries.
        let failed_count = std::fs::read_dir(failed.path()).unwrap().count();
        assert_eq!(
            failed_count, 0,
            "no jobs should be in failed/ for a clean transfer"
        );
    }

    // -----------------------------------------------------------------------
    // Test: Stream strategy - large archive → no local ZIP created.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_end_to_end_stream_strategy() {
        let Some(sftp) = sftp_env() else {
            eprintln!("SKIP test_end_to_end_stream_strategy - TEST_SFTP_* env vars not set");
            return;
        };

        let watch = TempDir::new().unwrap();
        let failed = TempDir::new().unwrap();

        // Threshold 1 MB → the 2 MB fixture uses Stream (0 would mean Disk).
        let config = make_config(&watch, &failed, &sftp, 1);
        let shutdown = CancellationToken::new();
        let shutdown_c = shutdown.clone();

        // Snapshot the watch dir before placing the fixture.
        let watch_path = watch.path().to_path_buf();

        tokio::spawn(async move {
            let _ =
                fs_ext_rar2zip::app::run(config, PathBuf::from("config.toml"), shutdown_c).await;
        });

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let fixture = place_tar_fixture(&watch, "large.tar", 2 * 1024 * 1024);
        let mut saw_zip = false;

        // Wait for source to be removed (job completed), watching for a
        // local zip the whole time: Disk would create one and then delete it.
        let removed = wait_for(60, || {
            saw_zip |= watch_path.join("large.zip").exists();
            !fixture.exists()
        })
        .await;

        shutdown.cancel();

        assert!(
            removed,
            "source archive must be removed after successful Stream-strategy job"
        );

        // No .zip files should have been created locally (stream goes directly to SFTP).
        let zip_count = std::fs::read_dir(&watch_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "zip").unwrap_or(false))
            .count();
        assert!(!saw_zip, "Stream strategy must not create a local ZIP file");
        assert_eq!(
            zip_count, 0,
            "Stream strategy must not create a local ZIP file"
        );

        let failed_count = std::fs::read_dir(failed.path()).unwrap().count();
        assert_eq!(failed_count, 0, "no jobs should be in failed/");
    }
}
