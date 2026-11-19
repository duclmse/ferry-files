//! rar2zip's transfers against the sftpd extension on a loopback port:
//! host key checking, upload with remote checksum, and the Stream strategy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_ext_rar2zip::config::{
    Config, ConversionConfig, Destination, ObservabilityConfig, TransferConfig,
};
use fs_ext_rar2zip::hostkey::HostKeyCheck;
use fs_ext_rar2zip::metrics::Metrics;
use fs_ext_rar2zip::queue::{Job, Status, Strategy};
use fs_ext_rar2zip::transfer::{stream_convert_and_transfer, transfer_all};
use fs_ext_sftpd::{config as sftpd_config, keys, server};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::ssh_key::LineEnding;
use russh::keys::PrivateKey;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

struct Fixture {
    dir: tempfile::TempDir,
    remote: PathBuf,
    port: u16,
    fingerprint: String,
    client_key: PathBuf,
    stop: CancellationToken,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn start() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("remote");
    std::fs::create_dir_all(root.join("in")).unwrap();
    let key = PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
        "",
    )
    .unwrap();
    let client_key = dir.path().join("id_ed25519");
    key.write_openssh_file(&client_key, LineEnding::LF).unwrap();
    let ak = dir.path().join("authorized_keys");
    std::fs::write(&ak, key.public_key().to_openssh().unwrap()).unwrap();
    let host_key = dir.path().join("host_key");

    let running = server::bind(&sftpd_config::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        root: root.clone(),
        read_only: false,
        host_key: host_key.clone(),
        users: vec![sftpd_config::User {
            name: "rar2zip".into(),
            authorized_keys: ak,
        }],
        idle_timeout_secs: 30,
    })
    .await
    .unwrap();
    let port = running.address().unwrap().port();
    let stop = CancellationToken::new();
    tokio::spawn(running.serve(stop.clone()));
    let host = russh::keys::load_secret_key(&host_key, None).unwrap();
    Fixture {
        remote: root.join("in"),
        port,
        fingerprint: keys::fingerprint(host.public_key()),
        client_key,
        stop,
        dir,
    }
}

impl Fixture {
    fn known_hosts(&self) -> PathBuf {
        self.dir.path().join("known_hosts")
    }

    fn config(&self, check: HostKeyCheck, pins: &[&str]) -> Config {
        let local = self.dir.path().join("local");
        Config {
            watch_folder: local.clone(),
            failed_folder: local.join("failed"),
            exclude_patterns: vec![],
            include_extensions: vec!["tar".into()],
            watch_subdirs: None,
            min_file_age_secs: 0,
            conversion: ConversionConfig::default(),
            destinations: vec![Destination {
                host: "127.0.0.1".into(),
                port: self.port,
                user: "rar2zip".into(),
                remote_path: "/in".into(),
                ssh_key_path: self.client_key.clone(),
                host_key_check: None,
                host_keys: pins.iter().map(|p| p.to_string()).collect(),
            }],
            transfer: TransferConfig {
                max_retries: 0,
                retry_delay_secs: 0,
                host_key_check: check,
                known_hosts: Some(self.known_hosts()),
                ..TransferConfig::default()
            },
            observability: ObservabilityConfig::default(),
            smtp: None,
        }
    }

    fn zip(&self, name: &str, body: &[u8]) -> PathBuf {
        let local = self.dir.path().join("local");
        std::fs::create_dir_all(&local).unwrap();
        let path = local.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }
}

fn job(path: &Path) -> Job {
    Job {
        id: "job".into(),
        file_path: path.to_path_buf(),
        file_hash: String::new(),
        status: Status::Transferring,
        strategy: Strategy::Disk,
        destinations: vec![],
        retry_count: 0,
        created_at: 0,
        updated_at: 0,
        error: None,
    }
}

async fn send(config: &Config, zip: &Path) -> Result<(), String> {
    transfer_all(
        zip,
        &job(zip),
        config,
        Metrics::new(),
        Arc::new(Semaphore::new(1)),
        CancellationToken::new(),
    )
    .await
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn strict_refuses_an_unknown_host_until_it_is_recorded() {
    let f = start().await;
    let zip = f.zip("it's a $test.zip", b"zip bytes");

    let err = send(&f.config(HostKeyCheck::Strict, &[]), &zip)
        .await
        .unwrap_err();
    assert!(err.contains("is not in"), "{err}");
    assert!(!f.remote.join("it's a $test.zip").exists());

    send(&f.config(HostKeyCheck::AcceptNew, &[]), &zip)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(f.remote.join("it's a $test.zip")).unwrap(),
        b"zip bytes"
    );
    let recorded = std::fs::read_to_string(f.known_hosts()).unwrap();
    assert!(
        recorded.contains(&format!("[127.0.0.1]:{}", f.port)),
        "{recorded}"
    );

    send(&f.config(HostKeyCheck::Strict, &[]), &zip)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_changed_host_key_is_refused_even_under_accept_new() {
    let f = start().await;
    let other = PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[9; 32])),
        "",
    )
    .unwrap();
    russh::keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        f.port,
        other.public_key(),
        f.known_hosts(),
    )
    .unwrap();
    let zip = f.zip("a.zip", b"x");
    let err = send(&f.config(HostKeyCheck::AcceptNew, &[]), &zip)
        .await
        .unwrap_err();
    assert!(err.contains("HOST KEY CHANGED"), "{err}");
}

#[tokio::test]
async fn pinned_fingerprints_decide_alone() {
    let f = start().await;
    let zip = f.zip("p.zip", b"pinned");
    let wrong = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let err = send(&f.config(HostKeyCheck::Strict, &[wrong]), &zip)
        .await
        .unwrap_err();
    assert!(err.contains("pinned"), "{err}");

    send(&f.config(HostKeyCheck::Strict, &[&f.fingerprint]), &zip)
        .await
        .unwrap();
    assert!(f.remote.join("p.zip").exists());
    assert!(!f.known_hosts().exists(), "pins never touch known_hosts");
}

#[tokio::test]
async fn off_skips_the_check() {
    let f = start().await;
    let zip = f.zip("o.zip", b"off");
    send(&f.config(HostKeyCheck::Off, &[]), &zip).await.unwrap();
    assert!(f.remote.join("o.zip").exists());
}

#[tokio::test]
async fn stream_strategy_uploads_without_a_local_zip() {
    let f = start().await;
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "hello.txt", &b"hello"[..])
        .unwrap();
    let src = f.zip("bundle.tar", &tar.into_inner().unwrap());
    let config = f.config(HostKeyCheck::Strict, &[&f.fingerprint]);

    stream_convert_and_transfer(
        &src,
        &config.destinations[0],
        &config,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let uploaded = std::fs::read(f.remote.join("bundle.zip")).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(uploaded)).unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("hello.txt").unwrap(), &mut text).unwrap();
    assert_eq!(text, "hello");
    let local: Vec<_> = std::fs::read_dir(src.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(local, ["bundle.tar"], "no zip written locally");
}
