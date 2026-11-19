//! Runs the server on a loopback port and talks to it with russh's client.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_ext_sftpd::config::{Config, User};
use fs_ext_sftpd::server;
use russh::client::{self, Handle};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

fn client_key(seed: u8) -> PrivateKey {
    PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[seed; 32])),
        "",
    )
    .unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    host_key: PathBuf,
    addr: SocketAddr,
    stop: CancellationToken,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn start(read_only: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let ak = dir.path().join("authorized_keys");
    std::fs::write(&ak, client_key(1).public_key().to_openssh().unwrap()).unwrap();
    let host_key = dir.path().join("keys").join("host");
    let cfg = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        root: root.clone(),
        read_only,
        host_key: host_key.clone(),
        users: vec![User {
            name: "up".into(),
            authorized_keys: ak,
        }],
        idle_timeout_secs: 30,
    };
    let running = server::bind(&cfg).await.unwrap();
    let addr = running.address().unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(running.serve(stop.clone()));
    Fixture {
        root: root.canonicalize().unwrap(),
        _dir: dir,
        host_key,
        addr,
        stop,
    }
}

struct Client {
    expected: PublicKey,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(key.key_data() == self.expected.key_data())
    }
}

async fn login(f: &Fixture, user: &str, key: PrivateKey) -> Result<Handle<Client>, String> {
    let host = russh::keys::load_secret_key(&f.host_key, None).unwrap();
    let handler = Client {
        expected: host.public_key().clone(),
    };
    let mut session = client::connect(Arc::new(client::Config::default()), f.addr, handler)
        .await
        .map_err(|e| e.to_string())?;
    let auth = session
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), None))
        .await
        .map_err(|e| e.to_string())?;
    if auth.success() {
        Ok(session)
    } else {
        Err("rejected".into())
    }
}

async fn sftp(session: &Handle<Client>) -> SftpSession {
    let channel = session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    SftpSession::new(channel.into_stream()).await.unwrap()
}

async fn exec(session: &Handle<Client>, command: &str) -> (String, u32) {
    let mut channel = session.channel_open_session().await.unwrap();
    channel.exec(true, command).await.unwrap();
    let mut out = Vec::new();
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            _ => {}
        }
    }
    (String::from_utf8(out).unwrap(), status.unwrap())
}

fn status_of(e: russh_sftp::client::error::Error) -> StatusCode {
    match e {
        russh_sftp::client::error::Error::Status(s) => s.status_code,
        other => panic!("expected a status, got {other}"),
    }
}

async fn put(sftp: &SftpSession, path: &str, data: &[u8]) {
    let mut file = sftp.create(path).await.unwrap();
    file.write_all(data).await.unwrap();
    file.shutdown().await.unwrap();
}

#[tokio::test]
async fn upload_read_list_rename_and_remove() {
    let f = start(false).await;
    let session = login(&f, "up", client_key(1)).await.unwrap();
    let sftp = sftp(&session).await;

    sftp.create_dir("/in").await.unwrap();
    let body: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    put(&sftp, "/in/a b.zip", &body).await;
    assert_eq!(std::fs::read(f.root.join("in/a b.zip")).unwrap(), body);
    assert_eq!(sftp.read("/in/a b.zip").await.unwrap(), body);
    assert_eq!(
        sftp.metadata("/in/a b.zip").await.unwrap().size,
        Some(700_000)
    );

    let names: Vec<String> = sftp
        .read_dir("/in")
        .await
        .unwrap()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names, ["a b.zip"]);
    assert_eq!(sftp.canonicalize("in/../in/./x").await.unwrap(), "/in/x");

    put(&sftp, "/in/other", b"x").await;
    let clash = sftp.rename("/in/a b.zip", "/in/other").await.unwrap_err();
    assert_eq!(
        status_of(clash),
        StatusCode::Failure,
        "rename never replaces"
    );
    sftp.rename("/in/a b.zip", "/in/c.zip").await.unwrap();
    assert!(f.root.join("in/c.zip").exists());

    let (out, code) = exec(&session, "sha256sum /in/other").await;
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881  /in/other\n"
    );
    let (_, code) = exec(&session, "rm -f /in/other").await;
    assert_eq!(code, 0);
    assert!(!f.root.join("in/other").exists());
    let (_, code) = exec(&session, "ls /").await;
    assert_eq!(code, 127, "no shell");

    sftp.remove_file("/in/c.zip").await.unwrap();
    sftp.remove_dir("/in").await.unwrap();
    assert_eq!(std::fs::read_dir(&f.root).unwrap().count(), 0);
    let root = sftp.remove_dir("/").await.unwrap_err();
    assert_eq!(status_of(root), StatusCode::PermissionDenied);
}

#[tokio::test]
async fn paths_cannot_leave_the_root() {
    let f = start(false).await;
    let session = login(&f, "up", client_key(1)).await.unwrap();
    let sftp = sftp(&session).await;

    put(&sftp, "../../escape.txt", b"x").await;
    assert!(f.root.join("escape.txt").exists(), "`..` stops at the root");
    let parent = f.root.parent().unwrap();
    assert!(!parent.join("escape.txt").exists());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(parent, f.root.join("up")).unwrap();
        let read = sftp.read("/up/authorized_keys").await.unwrap_err();
        assert_eq!(status_of(read), StatusCode::PermissionDenied);
        let write = sftp.create("/up/planted").await.map(|_| ()).unwrap_err();
        assert_eq!(status_of(write), StatusCode::PermissionDenied);
        assert!(!parent.join("planted").exists());
        let (_, code) = exec(&session, "sha256sum /up/authorized_keys").await;
        assert_eq!(code, 1);
    }
}

#[tokio::test]
async fn read_only_serves_but_refuses_changes() {
    let f = start(true).await;
    std::fs::write(f.root.join("r.txt"), b"hello").unwrap();
    let session = login(&f, "up", client_key(1)).await.unwrap();
    let sftp = sftp(&session).await;

    let mut file = sftp.open("/r.txt").await.unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).await.unwrap();
    assert_eq!(text, "hello");

    let denied = [
        sftp.create("/new").await.map(|_| ()).unwrap_err(),
        sftp.remove_file("/r.txt").await.unwrap_err(),
        sftp.create_dir("/d").await.unwrap_err(),
        sftp.rename("/r.txt", "/s.txt").await.unwrap_err(),
    ];
    for e in denied {
        assert_eq!(status_of(e), StatusCode::PermissionDenied);
    }
    let (_, code) = exec(&session, "rm -f /r.txt").await;
    assert_eq!(code, 1);
    assert!(f.root.join("r.txt").exists());
}

#[tokio::test]
async fn only_listed_users_with_listed_keys_log_in() {
    let f = start(false).await;
    assert!(login(&f, "up", client_key(1)).await.is_ok());
    assert!(login(&f, "up", client_key(2)).await.is_err(), "unknown key");
    assert!(
        login(&f, "root", client_key(1)).await.is_err(),
        "unknown user"
    );
}

#[tokio::test]
async fn host_key_survives_a_restart() {
    let f = start(false).await;
    let first = std::fs::read(&f.host_key).unwrap();
    let cfg_key = Path::new(&f.host_key);
    let again = fs_ext_sftpd::keys::load_or_create_host_key(cfg_key).unwrap();
    assert_eq!(
        russh::keys::load_secret_key(cfg_key, None)
            .unwrap()
            .public_key(),
        again.public_key()
    );
    assert_eq!(std::fs::read(&f.host_key).unwrap(), first);
}

#[tokio::test]
async fn shell_and_terminal_requests_are_refused_not_left_hanging() {
    let f = start(false).await;
    let session = login(&f, "up", client_key(1)).await.unwrap();
    let mut channel = session.channel_open_session().await.unwrap();
    channel
        .request_pty(true, "xterm", 80, 24, 0, 0, &[])
        .await
        .unwrap();
    channel.request_shell(true).await.unwrap();
    let mut failures = 0;
    let wait = async {
        while let Some(msg) = channel.wait().await {
            if matches!(msg, ChannelMsg::Failure) {
                failures += 1;
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .expect("the channel closes instead of hanging");
    assert_eq!(failures, 2);
}
