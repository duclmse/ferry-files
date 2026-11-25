//! Protocol between `fsd` and an extension executable.
//!
//! The daemon starts `<exe> --config <path> run` and exchanges one JSON object
//! per line: [`HostMessage`] on the child's stdin, [`ExtensionReply`] on its
//! stdout. Stdout belongs to the protocol, so extensions log to stderr, which
//! the daemon forwards to its own log. Stdin closing means the same as
//! [`HostMessage::Shutdown`], so a child never outlives a crashed daemon.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::extension::Extension;

pub const EXTENSION_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Command {
        id: u64,
        command: String,
    },
    /// Apply the config file again (`Extension::reload`); answered like a
    /// command, with empty output.
    Reload {
        id: u64,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionReply {
    pub id: u64,
    pub ok: bool,
    /// Text for the user when `ok`, otherwise the error message.
    pub output: String,
}

/// Runs `ext` as a child of the daemon until told to stop.
///
/// Callers must end with `std::process::exit`: Tokio's stdin reader occupies a
/// blocking thread that would otherwise hold runtime shutdown open until the
/// daemon closes the pipe.
pub async fn serve(ext: Arc<dyn Extension>) -> anyhow::Result<()> {
    ignore_termination_signals();
    serve_on(ext, tokio::io::stdin(), tokio::io::stdout()).await
}

// Ctrl-C and systemd's stop signal reach the whole process group. Dying here
// would abandon in-flight work, so only the daemon's shutdown message counts.
fn ignore_termination_signals() {
    tokio::spawn(async { while tokio::signal::ctrl_c().await.is_ok() {} });
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            while term.recv().await.is_some() {}
        }
    });
}

pub async fn serve_on<R, W>(ext: Arc<dyn Extension>, reader: R, mut writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let shutdown = CancellationToken::new();
    let (reply_tx, mut reply_rx) = mpsc::channel::<ExtensionReply>(32);

    let writer_task = tokio::spawn(async move {
        while let Some(reply) = reply_rx.recv().await {
            let Ok(mut line) = serde_json::to_string(&reply) else {
                continue;
            };
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });

    let reader_task = {
        let ext = Arc::clone(&ext);
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<HostMessage>(&line) {
                    Ok(HostMessage::Command { id, command }) => {
                        let ext = Arc::clone(&ext);
                        let run = move || ext.command(&command);
                        tokio::spawn(answer(id, run, reply_tx.clone()));
                    }
                    Ok(HostMessage::Reload { id }) => {
                        let ext = Arc::clone(&ext);
                        let run = move || ext.reload().map(|()| String::new());
                        tokio::spawn(answer(id, run, reply_tx.clone()));
                    }
                    Ok(HostMessage::Shutdown) => break,
                    Err(e) => tracing::warn!("ignoring malformed host message: {e}"),
                }
            }
            shutdown.cancel();
        })
    };

    let result = ext.run(shutdown).await;
    reader_task.abort();
    let _ = reader_task.await;
    let _ = writer_task.await;
    result
}

async fn answer(
    id: u64,
    run: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
    replies: mpsc::Sender<ExtensionReply>,
) {
    let reply = match tokio::task::spawn_blocking(run).await {
        Ok(Ok(output)) => ExtensionReply {
            id,
            ok: true,
            output,
        },
        Ok(Err(e)) => ExtensionReply {
            id,
            ok: false,
            output: format!("{e:#}"),
        },
        Err(e) => ExtensionReply {
            id,
            ok: false,
            output: e.to_string(),
        },
    };
    let _ = replies.send(reply).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::ExtensionFuture;
    use tokio::io::{duplex, AsyncBufReadExt};

    struct Echo;

    impl Extension for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn validate(&self) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, shutdown: CancellationToken) -> ExtensionFuture {
            Box::pin(async move {
                shutdown.cancelled().await;
                Ok(())
            })
        }
        fn command(&self, command: &str) -> anyhow::Result<String> {
            match command {
                "fail" => anyhow::bail!("boom"),
                other => Ok(format!("echo {other}")),
            }
        }
    }

    async fn send(pipe: &mut tokio::io::DuplexStream, message: &HostMessage) {
        let mut line = serde_json::to_string(message).unwrap();
        line.push('\n');
        pipe.write_all(line.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn answers_commands_then_stops_on_shutdown() {
        let (mut host_in, child_in) = duplex(4096);
        let (child_out, host_out) = duplex(4096);
        let server = tokio::spawn(serve_on(Arc::new(Echo), child_in, child_out));
        let mut replies = BufReader::new(host_out).lines();

        for (id, command) in [(1, "hi"), (2, "fail")] {
            let command = command.to_owned();
            send(&mut host_in, &HostMessage::Command { id, command }).await;
            let line = replies.next_line().await.unwrap().unwrap();
            let reply: ExtensionReply = serde_json::from_str(&line).unwrap();
            assert_eq!(reply.id, id);
            assert_eq!(reply.ok, id == 1);
            assert_eq!(reply.output, if id == 1 { "echo hi" } else { "boom" });
        }

        send(&mut host_in, &HostMessage::Reload { id: 3 }).await;
        let line = replies.next_line().await.unwrap().unwrap();
        let reply: ExtensionReply = serde_json::from_str(&line).unwrap();
        assert_eq!(reply.id, 3);
        assert!(!reply.ok, "Echo does not reload");
        assert!(reply.output.contains("restarts"), "{}", reply.output);

        send(&mut host_in, &HostMessage::Shutdown).await;
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn closed_stdin_stops_the_extension() {
        let (host_in, child_in) = duplex(64);
        let (child_out, _host_out) = duplex(64);
        let server = tokio::spawn(serve_on(Arc::new(Echo), child_in, child_out));
        drop(host_in);
        server.await.unwrap().unwrap();
    }
}
