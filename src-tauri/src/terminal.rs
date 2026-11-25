//! Terminals under a pane: a shell in a pseudo-terminal (ConPTY on Windows) on the device that
//! has a command line (this computer, a phone over adb, an SFTP server over ssh), so programs
//! can ask for input and draw full-screen.
//!
//! What a shell prints goes to the window as "term" events (base64, as it may cut a character in
//! two), and its end as a "term_exit" event. The window keeps the output, so a terminal it hides
//! keeps running and shows everything when it's opened again.

use super::*;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};

use std::sync::LazyLock;
use tauri::Emitter;

struct Term {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
}

static TERMS: LazyLock<Mutex<HashMap<u32, Arc<Term>>>> = LazyLock::new(Default::default);
static NEXT: AtomicU32 = AtomicU32::new(1);

fn terms() -> std::sync::MutexGuard<'static, HashMap<u32, Arc<Term>>> {
    TERMS.lock().unwrap_or_else(|e| e.into_inner())
}

fn term(id: u32) -> Result<Arc<Term>, String> {
    terms()
        .get(&id)
        .cloned()
        .ok_or_else(|| "This terminal has ended.".to_string())
}

#[derive(Clone, Serialize)]
struct TermOut {
    id: u32,
    /// The bytes printed, in base64.
    data: String,
}

#[derive(Clone, Serialize)]
struct TermExit {
    id: u32,
    /// The shell's exit code, when it ended by itself.
    code: Option<u32>,
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows: rows.clamp(2, 500),
        cols: cols.clamp(10, 1000),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// `c` (from `Transport::shell`) as a command for the pseudo-terminal.
fn builder(c: &std::process::Command) -> CommandBuilder {
    let mut b = CommandBuilder::new(c.get_program());
    b.args(c.get_args());
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => b.env(k, v),
            None => b.env_remove(k),
        }
    }
    if let Some(d) = c.get_current_dir() {
        b.cwd(d);
    }
    b.env("TERM", "xterm-256color");
    b.env("COLORTERM", "truecolor");
    b
}

/// Starts a shell in `dir` on `src`, `rows` by `cols` characters. Answers the terminal's id.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn term_open(
    app: AppHandle,
    state: State<'_, AppState>,
    src: Option<String>,
    dir: String,
    rows: u16,
    cols: u16,
) -> Result<u32, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let cmd = dev
            .shell(&dir)
            .ok_or("This device has no command line.")?
            .map_err(|e| e.to_string())?;
        let pair = portable_pty::native_pty_system()
            .openpty(size(rows, cols))
            .map_err(|e| format!("Couldn't open a terminal: {e}"))?;
        let mut child = pair.slave.spawn_command(builder(&cmd)).map_err(|e| {
            format!(
                "Couldn't start {}: {e}",
                cmd.get_program().to_string_lossy()
            )
        })?;
        // The shell has its own copy now; holding ours would keep the terminal open after it ends.
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let t = Term {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(child.clone_killer()),
        };
        terms().insert(id, Arc::new(t));
        std::thread::spawn(move || {
            use base64::Engine;
            let mut buf = vec![0u8; 16 * 1024];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let data = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                let _ = app.emit("term", TermOut { id, data });
            }
            let code = child.wait().ok().map(|s| s.exit_code());
            terms().remove(&id);
            let _ = app.emit("term_exit", TermExit { id, code });
        });
        Ok(id)
    })
    .await
}

/// Types `data` (text and the keys' escape sequences) into terminal `id`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn term_write(id: u32, data: String) -> Result<(), String> {
    let t = term(id)?;
    blocking(move || {
        let mut w = t.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(data.as_bytes())
            .and_then(|_| w.flush())
            .map_err(|e| format!("Couldn't type into the terminal: {e}"))
    })
    .await
}

/// Tells terminal `id` it's now `rows` by `cols` characters.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn term_resize(id: u32, rows: u16, cols: u16) -> Result<(), String> {
    let t = term(id)?;
    let r = t
        .master
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .resize(size(rows, cols));
    r.map_err(|e| e.to_string())
}

/// Ends terminal `id`'s shell. Fine when it has already ended.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn term_close(id: u32) -> Result<(), String> {
    if let Some(t) = terms().remove(&id) {
        let _ = t.killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
    Ok(())
}

// The test runs `sh`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn runs_a_shell_in_a_terminal() {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "stty size; printf 'hi\\n'; read x; echo got $x"])
            .current_dir("/");
        let pair = portable_pty::native_pty_system()
            .openpty(size(24, 80))
            .unwrap();
        let mut child = pair.slave.spawn_command(builder(&c)).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut w = pair.master.take_writer().unwrap();
        w.write_all(b"yes\r").unwrap();
        let mut out = String::new();
        let _ = reader.read_to_string(&mut out);
        assert_eq!(child.wait().unwrap().exit_code(), 0);
        assert!(out.contains("24 80"), "{out:?}");
        assert!(out.contains("got yes"), "{out:?}");
    }
}
