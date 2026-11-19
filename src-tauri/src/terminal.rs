//! The terminal under a pane: one command at a time, run in the pane's folder on the device that
//! has a command line (this computer, a phone over adb, an SFTP server over ssh).

use super::*;
use crate::device::DevError;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What a command printed (its output, then its errors) and its exit code.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Ran {
    pub out: String,
    pub code: i32,
    /// Stopped for taking longer than `LIMIT`.
    pub timed_out: bool,
}

/// Longest a command may run; it's stopped then.
const LIMIT: Duration = Duration::from_secs(120);
/// Most output kept from a command.
const OUT_MAX: usize = 1024 * 1024;

fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if kept.len() < OUT_MAX {
                kept.extend_from_slice(&buf[..n.min(OUT_MAX - kept.len())]);
            }
        }
        kept
    })
}

/// Runs `c` with no input, for up to `LIMIT`.
pub fn run(mut c: Command) -> device::Result<Ran> {
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DevError::Msg(format!("Couldn't run the command: {e}")))?;
    let out = child.stdout.take().map(drain);
    let errs = child.stderr.take().map(drain);
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if started.elapsed() > LIMIT => {
                let _ = child.kill();
                timed_out = true;
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let mut text = out.and_then(|h| h.join().ok()).unwrap_or_default();
    text.extend(errs.and_then(|h| h.join().ok()).unwrap_or_default());
    Ok(Ran { out: String::from_utf8_lossy(&text).into_owned(), code: status.and_then(|s| s.code()).unwrap_or(-1), timed_out })
}

/// Runs `command` in `dir` on `src`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn run_command(state: State<'_, AppState>, src: Option<String>, dir: String, command: String) -> Result<Ran, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || match dev.run(&dir, &command) {
        Some(r) => r.map_err(|e| e.to_string()),
        None => Err("This device has no command line.".into()),
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn runs_commands() {
        let mut c = Command::new("sh");
        c.args(["-c", "echo hi; echo oops >&2; exit 3"]);
        assert_eq!(run(c).unwrap(), Ran { out: "hi\noops\n".into(), code: 3, timed_out: false });
    }
}
