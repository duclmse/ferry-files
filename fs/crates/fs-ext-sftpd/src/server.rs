//! SSH side: public-key login, the `sftp` subsystem, and the exec commands
//! in [`crate::exec`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PublicKey;
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, User};
use crate::jail::Jail;
use crate::keys;
use crate::sftp::SftpSession;

struct Shared {
    jail: Arc<Jail>,
    read_only: bool,
    users: Vec<User>,
}

#[derive(Clone)]
struct Server(Arc<Shared>);

impl russh::server::Server for Server {
    type Handler = Connection;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Connection {
        Connection {
            shared: Arc::clone(&self.0),
            peer,
            user: None,
            channels: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        tracing::debug!("session ended: {error}");
    }
}

pub struct Connection {
    shared: Arc<Shared>,
    peer: Option<SocketAddr>,
    user: Option<String>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl Connection {
    async fn key_allowed(&self, user: &str, key: &PublicKey) -> bool {
        let Some(entry) = self.shared.users.iter().find(|u| u.name == user) else {
            return false;
        };
        let path = entry.authorized_keys.clone();
        match tokio::task::spawn_blocking(move || keys::read_authorized_keys(&path)).await {
            Ok(Ok(allowed)) => allowed.iter().any(|k| keys::same_key(k, key)),
            Ok(Err(e)) => {
                tracing::warn!("user {user}: {e:#}");
                false
            }
            Err(_) => false,
        }
    }
}

impl russh::server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if self.key_allowed(user, key).await {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        if !self.key_allowed(user, key).await {
            tracing::info!(peer = ?self.peer, "refused key {} for {user}", keys::fingerprint(key));
            return Ok(Auth::reject());
        }
        tracing::info!(peer = ?self.peer, "{user} logged in with {}", keys::fingerprint(key));
        self.user = Some(user.to_owned());
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channels.insert(channel.id(), channel);
        Ok(true)
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (Some(user), "sftp") = (self.user.clone(), name) else {
            return session.channel_failure(channel_id);
        };
        let Some(channel) = self.channels.remove(&channel_id) else {
            return session.channel_failure(channel_id);
        };
        session.channel_success(channel_id)?;
        let sftp = SftpSession::new(Arc::clone(&self.shared.jail), self.shared.read_only, user);
        russh_sftp::server::run(channel.into_stream(), sftp).await;
        Ok(())
    }

    // russh's defaults neither grant nor refuse these, so a client asking for
    // an interactive shell would wait forever.
    async fn shell_request(
        &mut self,
        channel_id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel_id)?;
        session.close(channel_id)
    }

    async fn pty_request(
        &mut self,
        channel_id: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel_id)
    }

    async fn x11_request(
        &mut self,
        channel_id: ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel_id)
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.user.is_none() || self.channels.remove(&channel_id).is_none() {
            return session.channel_failure(channel_id);
        }
        session.channel_success(channel_id)?;
        let command = String::from_utf8_lossy(data).into_owned();
        let jail = Arc::clone(&self.shared.jail);
        let read_only = self.shared.read_only;
        let handle = session.handle();
        tracing::debug!(user = ?self.user, "exec {command}");
        // Hashing a large upload must not hold up this connection's other
        // channels, so the reply is sent from its own task.
        tokio::spawn(async move {
            let out =
                tokio::task::spawn_blocking(move || crate::exec::run(&command, &jail, read_only))
                    .await
                    .unwrap_or_else(|_| crate::exec::Output {
                        stdout: Vec::new(),
                        stderr: b"internal error\n".to_vec(),
                        status: 1,
                    });
            // The client may have gone; nothing is left to tell it then.
            if !out.stdout.is_empty() {
                let _ = handle.data(channel_id, out.stdout).await;
            }
            if !out.stderr.is_empty() {
                let _ = handle.extended_data(channel_id, 1, out.stderr).await;
            }
            let _ = handle.exit_status_request(channel_id, out.status).await;
            let _ = handle.eof(channel_id).await;
            let _ = handle.close(channel_id).await;
        });
        Ok(())
    }
}

/// A bound listener ready to serve; [`Running::address`] gives the port when
/// the config asked for port 0.
pub struct Running {
    listener: TcpListener,
    server: Server,
    config: Arc<russh::server::Config>,
}

impl Running {
    pub fn address(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves until `shutdown` is cancelled; open sessions are then closed.
    pub async fn serve(mut self, shutdown: CancellationToken) -> std::io::Result<()> {
        let running = self.server.run_on_socket(self.config, &self.listener);
        let handle = running.handle();
        tokio::select! {
            result = running => result,
            _ = shutdown.cancelled() => {
                handle.shutdown("server shutting down".into());
                Ok(())
            }
        }
    }
}

pub async fn bind(cfg: &Config) -> anyhow::Result<Running> {
    let host_key = {
        let path = cfg.host_key.clone();
        tokio::task::spawn_blocking(move || keys::load_or_create_host_key(&path)).await??
    };
    let jail = Arc::new(Jail::new(&cfg.root)?);
    let listener = TcpListener::bind(cfg.listen).await?;
    tracing::info!(
        "listening on {} for {}, serving {}{} with host key {}",
        listener.local_addr()?,
        cfg.users
            .iter()
            .map(|u| u.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        jail.root().display(),
        if cfg.read_only { " read-only" } else { "" },
        keys::fingerprint(host_key.public_key()),
    );
    let idle = (cfg.idle_timeout_secs > 0).then(|| Duration::from_secs(cfg.idle_timeout_secs));
    let config = russh::server::Config {
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![host_key],
        inactivity_timeout: idle,
        ..Default::default()
    };
    Ok(Running {
        listener,
        server: Server(Arc::new(Shared {
            jail,
            read_only: cfg.read_only,
            users: cfg.users.clone(),
        })),
        config: Arc::new(config),
    })
}
