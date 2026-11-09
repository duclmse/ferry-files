//! Runs one extension executable and keeps it alive.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use common::extension::{Extension, ExtensionFuture};
use common::extension_stdio::{ExtensionReply, HostMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
// A child that survived this long was healthy, so its next crash starts over.
const STABLE_AFTER: Duration = Duration::from_secs(60);
// Below the daemon's drain limit, so a stuck child is killed rather than left.
const STOP_GRACE: Duration = Duration::from_secs(33);

type ReplySender = std_mpsc::Sender<anyhow::Result<String>>;

pub(super) struct Spec {
    pub name: String,
    pub version: String,
    pub executable: PathBuf,
    pub config_path: PathBuf,
}

struct Request {
    command: String,
    reply: ReplySender,
}

pub(super) struct ProcessExtension {
    spec: std::sync::Arc<Spec>,
    requests: mpsc::Sender<Request>,
    receiver: Mutex<Option<mpsc::Receiver<Request>>>,
}

impl ProcessExtension {
    pub(super) fn new(spec: Spec) -> Self {
        let (requests, receiver) = mpsc::channel(16);
        Self {
            spec: std::sync::Arc::new(spec),
            requests,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

impl Extension for ProcessExtension {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn validate(&self) -> Vec<String> {
        let output = std::process::Command::new(&self.spec.executable)
            .arg("--config")
            .arg(&self.spec.config_path)
            .arg("validate")
            .output();
        match output {
            Err(e) => vec![format!("could not run: {e}")],
            Ok(out) if out.status.success() => Vec::new(),
            Ok(out) => {
                let errors: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect();
                if errors.is_empty() {
                    vec![format!(
                        "validation failed ({}): {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    )]
                } else {
                    errors
                }
            }
        }
    }

    fn run(&self, shutdown: CancellationToken) -> ExtensionFuture {
        let spec = std::sync::Arc::clone(&self.spec);
        let receiver = self.receiver.lock().ok().and_then(|mut r| r.take());
        Box::pin(async move {
            let receiver = receiver.context("extension already started")?;
            supervise(&spec, receiver, shutdown).await;
            Ok(())
        })
    }

    fn command(&self, command: &str) -> anyhow::Result<String> {
        let (reply, response) = std_mpsc::channel();
        self.requests
            .blocking_send(Request {
                command: command.to_owned(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("extension is not running"))?;
        response
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|e| anyhow::anyhow!("no reply from extension: {e}"))?
    }
}

async fn supervise(
    spec: &Spec,
    mut requests: mpsc::Receiver<Request>,
    shutdown: CancellationToken,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        tracing::info!(extension = %spec.name, version = %spec.version, "starting");
        let started = Instant::now();
        match run_child(spec, &mut requests, &shutdown).await {
            Ok(()) => return,
            Err(e) => tracing::error!(extension = %spec.name, "stopped: {e:#}"),
        }
        if started.elapsed() >= STABLE_AFTER {
            backoff = MIN_BACKOFF;
        }
        tracing::warn!(extension = %spec.name, "restarting in {backoff:?}");

        let wait = tokio::time::sleep(backoff);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = &mut wait => break,
                Some(request) = requests.recv() => {
                    let _ = request.reply.send(Err(anyhow::anyhow!("extension is restarting")));
                }
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Returns `Ok` only after a requested shutdown; any other exit is an error.
async fn run_child(
    spec: &Spec,
    requests: &mut mpsc::Receiver<Request>,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    let mut child = Command::new(&spec.executable)
        .arg("--config")
        .arg(&spec.config_path)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", spec.executable.display()))?;

    let mut stdin = child.stdin.take().context("child stdin")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let stderr = child.stderr.take().context("child stderr")?;

    let name = spec.name.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            forward_log(&name, &line);
        }
    });

    let mut replies = BufReader::new(stdout).lines();
    let mut pending: HashMap<u64, ReplySender> = HashMap::new();
    let mut next_id = 0u64;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                stop_child(spec, child, stdin).await;
                return Ok(());
            }
            Some(request) = requests.recv() => {
                next_id += 1;
                let message = HostMessage::Command { id: next_id, command: request.command };
                match send(&mut stdin, &message).await {
                    Ok(()) => { pending.insert(next_id, request.reply); }
                    Err(e) => { let _ = request.reply.send(Err(e)); }
                }
            }
            line = replies.next_line() => match line.context("read child stdout")? {
                Some(line) => deliver(spec, &line, &mut pending),
                None => {
                    let status = child.wait().await.context("wait for child")?;
                    anyhow::bail!("exited unexpectedly ({status})");
                }
            },
        }
    }
}

// Keeps an extension's own severity, so log filters and alerts still see it.
fn forward_log(name: &str, line: &str) {
    let trimmed = line.trim_start();
    match trimmed.split_once(' ') {
        Some(("ERROR", rest)) => tracing::error!(extension = %name, "{}", rest.trim_start()),
        Some(("WARN", rest)) => tracing::warn!(extension = %name, "{}", rest.trim_start()),
        Some(("INFO", rest)) => tracing::info!(extension = %name, "{}", rest.trim_start()),
        Some(("DEBUG" | "TRACE", rest)) => {
            tracing::debug!(extension = %name, "{}", rest.trim_start())
        }
        _ => tracing::info!(extension = %name, "{trimmed}"),
    }
}

fn deliver(spec: &Spec, line: &str, pending: &mut HashMap<u64, ReplySender>) {
    let reply: ExtensionReply = match serde_json::from_str(line) {
        Ok(reply) => reply,
        Err(e) => {
            tracing::warn!(extension = %spec.name, "ignoring malformed reply: {e}");
            return;
        }
    };
    if let Some(sender) = pending.remove(&reply.id) {
        let result = if reply.ok {
            Ok(reply.output)
        } else {
            Err(anyhow::anyhow!(reply.output))
        };
        let _ = sender.send(result);
    }
}

async fn send(stdin: &mut ChildStdin, message: &HostMessage) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn stop_child(spec: &Spec, mut child: Child, mut stdin: ChildStdin) {
    let _ = send(&mut stdin, &HostMessage::Shutdown).await;
    drop(stdin);
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(Ok(status)) => tracing::info!(extension = %spec.name, "stopped ({status})"),
        Ok(Err(e)) => tracing::warn!(extension = %spec.name, "wait failed: {e}"),
        Err(_) => {
            tracing::warn!(extension = %spec.name, "did not stop in time, killing");
            let _ = child.kill().await;
        }
    }
}
