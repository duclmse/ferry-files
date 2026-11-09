use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::{self, Handle};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Destination};
use crate::convert::convert_to_bytes;
use crate::error::{AppError, Result};
use crate::metrics::Metrics;
use crate::queue::Job;

const CHUNK: usize = 65536;

// ---------------------------------------------------------------------------
// russh client handler
// Accept all server keys. Production deployments should verify against a
// pinned host key or a known_hosts file.
// ---------------------------------------------------------------------------

struct SshHandler;

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Disk strategy - zip already on disk, upload to all destinations
// ---------------------------------------------------------------------------

/// Upload `zip_path` to every destination. Returns `Ok(())` only when all
/// destinations succeed.
pub async fn transfer_all(
    zip_path: &Path,
    job: &Job,
    config: &Config,
    metrics: Arc<Metrics>,
    sem: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut errors = Vec::new();

    for dest in &config.destinations {
        let result = upload_with_retry(
            zip_path,
            dest,
            &config.transfer,
            Arc::clone(&metrics),
            Arc::clone(&sem),
            shutdown.clone(),
        )
        .await;

        if let Err(e) = result {
            tracing::error!(job_id = %job.id, dest = %dest.host, "transfer failed: {e}");
            errors.push(format!("{}: {e}", dest.host));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Transfer(errors.join("; ")))
    }
}

async fn upload_with_retry(
    zip_path: &Path,
    dest: &Destination,
    cfg: &crate::config::TransferConfig,
    metrics: Arc<Metrics>,
    sem: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<()> {
    let base_delay = cfg.retry_delay_secs;

    for attempt in 0..=cfg.max_retries {
        let _permit = sem
            .acquire()
            .await
            .map_err(|e| AppError::Transfer(e.to_string()))?;

        // Read the zip file once per attempt (re-reads on retry)
        let zip_bytes = read_file_bytes(zip_path).await?;
        let local_hash = sha256_of(&zip_bytes);

        let result = upload_bytes_to_dest(
            &zip_bytes,
            &local_hash,
            zip_path,
            dest,
            cfg.bandwidth_limit_kbps,
        )
        .await;

        match result {
            Ok(bytes) => {
                metrics
                    .bytes_transferred
                    .fetch_add(bytes, Ordering::Relaxed);
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(attempt, dest = %dest.host, "upload attempt failed: {e}");
                metrics.transfer_failures.fetch_add(1, Ordering::Relaxed);

                if attempt < cfg.max_retries {
                    let delay = backoff_secs(base_delay, attempt);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                        _ = shutdown.cancelled() => {
                            return Err(AppError::Transfer("cancelled on shutdown".into()));
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }
    unreachable!()
}

/// Upload `zip_bytes` to a single destination. Returns bytes sent.
async fn upload_bytes_to_dest(
    zip_bytes: &[u8],
    local_hash: &str,
    local_path: &Path,
    dest: &Destination,
    limit_kbps: u64,
) -> Result<u64> {
    let session = connect(dest).await?;

    let remote = remote_path(dest, local_path);

    // Open SFTP and upload
    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?;
    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?;

    let bytes = upload_sftp(&sftp, &remote, zip_bytes, limit_kbps).await?;
    drop(sftp);

    // Verify remote checksum over a fresh exec channel
    verify_checksum(&session, &remote, local_hash).await?;

    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Stream strategy - convert in memory, upload, no zip on disk
// ---------------------------------------------------------------------------

/// Convert `src` directly into SFTP without writing a zip file to disk.
/// The source archive remains on disk until the remote checksum passes.
///
/// Phase 4 note: zip bytes are buffered in RAM before upload because
/// `zip::ZipWriter` requires `Seek`. A future upgrade to `async_zip` would
/// enable true pipe-based streaming with bounded memory.
pub async fn stream_convert_and_transfer(
    src: &Path,
    dest: &Destination,
    config: &Config,
    shutdown: CancellationToken,
) -> Result<()> {
    for attempt in 0..=config.transfer.max_retries {
        let src_c = src.to_path_buf();
        let conv_cfg = config.conversion.clone();

        // Convert on a blocking thread (CPU-bound)
        let zip_bytes = tokio::task::spawn_blocking(move || convert_to_bytes(&src_c, &conv_cfg))
            .await
            .map_err(|e| AppError::Conversion(e.to_string()))??;

        let local_hash = sha256_of(&zip_bytes);

        // Derive remote filename (strip archive extension, add .zip)
        let zip_name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("archive");
        let remote = format!("{}/{zip_name}.zip", dest.remote_path.trim_end_matches('/'));

        let result = async {
            let session = connect(dest).await?;

            let channel = session
                .channel_open_session()
                .await
                .map_err(|e| AppError::Ssh(e.to_string()))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| AppError::Ssh(e.to_string()))?;
            let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
                .await
                .map_err(|e| AppError::Ssh(e.to_string()))?;

            let upload_result = upload_sftp(
                &sftp,
                &remote,
                &zip_bytes,
                config.transfer.bandwidth_limit_kbps,
            )
            .await;
            drop(sftp);

            if let Err(e) = upload_result {
                // Best-effort cleanup of partial remote file
                delete_remote(&session, &remote).await;
                return Err(e);
            }

            verify_checksum(&session, &remote, &local_hash).await
        }
        .await;

        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(attempt, dest = %dest.host, "stream attempt failed: {e}");
                if attempt < config.transfer.max_retries {
                    let delay = backoff_secs(config.transfer.retry_delay_secs, attempt);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                        _ = shutdown.cancelled() => {
                            return Err(AppError::Transfer("cancelled on shutdown".into()));
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

async fn connect(dest: &Destination) -> Result<Handle<SshHandler>> {
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, (dest.host.as_str(), dest.port), SshHandler)
        .await
        .map_err(|e| AppError::Ssh(format!("connect to {}: {e}", dest.host)))?;

    let key = load_secret_key(&dest.ssh_key_path, None)
        .map_err(|e| AppError::Ssh(format!("load key {:?}: {e}", dest.ssh_key_path)))?;

    // best_supported_rsa_hash negotiates the hash alg; None is fine for Ed25519/ECDSA
    let hash_alg = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?
        .flatten();

    let auth_res = session
        .authenticate_publickey(
            &dest.user,
            PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
        )
        .await
        .map_err(|e| AppError::Ssh(format!("auth for {}: {e}", dest.user)))?;

    if !auth_res.success() {
        return Err(AppError::Ssh(format!(
            "public-key authentication failed for {}@{}",
            dest.user, dest.host
        )));
    }
    Ok(session)
}

async fn upload_sftp(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
    data: &[u8],
    limit_kbps: u64,
) -> Result<u64> {
    let mut file = sftp
        .create(remote)
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?;

    let mut total = 0u64;
    let start = Instant::now();

    for chunk in data.chunks(CHUNK) {
        file.write_all(chunk).await.map_err(AppError::Io)?;
        total += chunk.len() as u64;
        throttle_async(total, limit_kbps, &start).await;
    }

    file.flush().await.map_err(AppError::Io)?;
    Ok(total)
}

async fn verify_checksum(session: &Handle<SshHandler>, remote: &str, expected: &str) -> Result<()> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?;
    channel
        .exec(true, format!("sha256sum {remote}"))
        .await
        .map_err(|e| AppError::Ssh(e.to_string()))?;

    let mut output = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { ref data }) => output.extend_from_slice(data),
            Some(ChannelMsg::ExitStatus { .. }) | None => break,
            _ => {}
        }
    }

    let stdout = String::from_utf8_lossy(&output);
    let actual = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| AppError::Transfer("remote sha256sum returned empty output".into()))?;

    if actual != expected {
        return Err(AppError::Transfer(format!(
            "checksum mismatch: expected {expected} got {actual}"
        )));
    }
    Ok(())
}

async fn delete_remote(session: &Handle<SshHandler>, remote: &str) {
    if let Ok(mut ch) = session.channel_open_session().await {
        let _ = ch.exec(true, format!("rm -f {remote}")).await;
        loop {
            match ch.wait().await {
                Some(ChannelMsg::ExitStatus { .. }) | None => break,
                _ => {}
            }
        }
    }
}

fn remote_path(dest: &Destination, local: &Path) -> String {
    let name = local
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive.zip");
    format!("{}/{name}", dest.remote_path.trim_end_matches('/'))
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

async fn read_file_bytes(path: &Path) -> Result<Vec<u8>> {
    tokio::fs::read(path).await.map_err(AppError::Io)
}

fn sha256_of(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn throttle_delay(bytes_sent: u64, limit_kbps: u64, elapsed_secs: f64) -> Option<Duration> {
    if limit_kbps == 0 {
        return None;
    }
    let rate = limit_kbps as f64 * 1024.0;
    let allowed = (rate * elapsed_secs.max(f64::EPSILON)) as u64;
    if bytes_sent > allowed {
        let sleep_s = (bytes_sent - allowed) as f64 / rate;
        Some(Duration::from_secs_f64(sleep_s))
    } else {
        None
    }
}

async fn throttle_async(bytes_sent: u64, limit_kbps: u64, start: &Instant) {
    if let Some(d) = throttle_delay(bytes_sent, limit_kbps, start.elapsed().as_secs_f64()) {
        tokio::time::sleep(d).await;
    }
}

fn backoff_secs(base: u64, attempt: u32) -> u64 {
    base.saturating_mul(1u64 << attempt.min(8)).min(300)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence() {
        let delays: Vec<u64> = (0..7).map(|a| backoff_secs(10, a)).collect();
        assert_eq!(delays, [10, 20, 40, 80, 160, 300, 300]);
    }

    #[test]
    fn sha256sum_output_parse() {
        let output = "abc123ef  /remote/file.zip\n";
        let actual = output.split_whitespace().next().unwrap();
        assert_eq!(actual, "abc123ef");
    }

    #[test]
    fn backoff_cap() {
        for a in 0..20 {
            assert!(backoff_secs(10, a) <= 300);
        }
    }

    #[test]
    fn backoff_base_zero_always_zero() {
        for a in 0..10 {
            assert_eq!(backoff_secs(0, a), 0);
        }
    }

    #[test]
    fn sha256_of_produces_64_hex_chars_and_is_deterministic() {
        let d1 = sha256_of(b"hello");
        let d2 = sha256_of(b"hello");
        assert_eq!(d1.len(), 64, "SHA-256 must be 64 hex chars");
        assert_eq!(d1, d2, "same input must produce same digest");
        assert!(d1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_of_different_inputs_differ() {
        assert_ne!(sha256_of(b"abc"), sha256_of(b"abd"));
        assert_ne!(sha256_of(b""), sha256_of(b"a"));
    }

    #[test]
    fn remote_path_strips_trailing_slash() {
        let dest = crate::config::Destination {
            host: "server".into(),
            port: 22,
            user: "user".into(),
            remote_path: "/remote/".into(),
            ssh_key_path: "/key".into(),
        };
        let local = Path::new("/tmp/archive.zip");
        assert_eq!(remote_path(&dest, local), "/remote/archive.zip");
    }

    #[test]
    fn remote_path_no_trailing_slash() {
        let dest = crate::config::Destination {
            host: "server".into(),
            port: 22,
            user: "user".into(),
            remote_path: "/remote".into(),
            ssh_key_path: "/key".into(),
        };
        let local = Path::new("/tmp/archive.zip");
        assert_eq!(remote_path(&dest, local), "/remote/archive.zip");
    }

    #[tokio::test]
    async fn throttle_zero_kbps_returns_immediately() {
        let start = std::time::Instant::now();
        throttle_async(u64::MAX, 0, &start).await; // limit=0 → immediate return
        assert!(
            start.elapsed().as_millis() < 50,
            "should return without sleeping"
        );
    }

    // ── throttle_delay (pure computation) ────────────────────────────────────

    #[test]
    fn throttle_delay_200kb_in_half_second_at_100kbps() {
        // rate = 100 * 1024 = 102 400 B/s
        // allowed in 0.5 s = 51 200 B;  sent = 200 * 1024 = 204 800 B
        // excess = 153 600 B  →  sleep = 153 600 / 102 400 = 1.5 s
        let d = throttle_delay(200 * 1024, 100, 0.5).unwrap();
        let secs = d.as_secs_f64();
        assert!(
            (secs - 1.5).abs() < 0.01,
            "expected ~1.5 s, got {secs:.4} s"
        );
    }

    #[test]
    fn throttle_delay_within_budget_returns_none() {
        // 10 KB in 0.5 s at 100 kbps: allowed = 51 200 B, sent = 10 240 → no sleep
        assert!(throttle_delay(10 * 1024, 100, 0.5).is_none());
    }

    #[test]
    fn throttle_delay_zero_limit_returns_none() {
        assert!(throttle_delay(u64::MAX, 0, 1.0).is_none());
    }
}
