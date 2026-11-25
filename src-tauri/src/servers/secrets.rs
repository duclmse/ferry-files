//! Passwords saved for connections, kept in the system's keychain (Keychain on macOS, Credential
//! Manager on Windows, the Secret Service on Linux) under the connection's id. The frontend sends
//! a password only to save it; nothing gives one back to it.
//!
//! `ssh` gets a password from an askpass program: the app runs itself with `ASKPASS_ENV` set to
//! the connection's id, and `askpass` prints the saved password and exits. So the password is
//! never in a command line or the environment.

use crate::*;
use std::process::Command;

const SERVICE: &str = "Ferry Files";
/// Set (to a connection's id) when the app is started as `ssh`'s askpass program.
const ASKPASS_ENV: &str = "FERRY_FILES_ASKPASS";

fn entry(id: &str) -> std::result::Result<keyring::Entry, String> {
    if id.is_empty() || id.len() > 100 {
        return Err("That isn't a connection's id.".into());
    }
    keyring::Entry::new(SERVICE, id).map_err(|e| format!("Can't use the keychain: {e}"))
}

/// The password saved for connection `id`, if there is one.
pub fn get(id: &str) -> Option<String> {
    entry(id).ok()?.get_password().ok()
}

/// Saves `secret` as connection `id`'s password, replacing one saved before.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn secret_set(id: String, secret: String) -> Result<(), String> {
    blocking(move || {
        entry(&id)?
            .set_password(&secret)
            .map_err(|e| format!("Couldn't save the password in the keychain: {e}"))
    })
    .await
}

/// Forgets connection `id`'s password; fine when there was none.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn secret_clear(id: String) -> Result<(), String> {
    blocking(move || match entry(&id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!(
            "Couldn't remove the password from the keychain: {e}"
        )),
    })
    .await
}

/// What `ssh` needs in its environment to ask the app for connection `id`'s password.
pub fn askpass_env(id: &str) -> Vec<(String, String)> {
    let me = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    vec![
        ("SSH_ASKPASS".into(), me),
        // OpenSSH 8.4 and later: use it even with a terminal.
        ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
        // Older ssh uses askpass only when there's a display.
        (
            "DISPLAY".into(),
            std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
        ),
        (ASKPASS_ENV.into(), id.into()),
    ]
}

/// Sets up `ssh` to get connection `id`'s password from the app.
pub fn ssh_asks(c: &mut Command, id: &str) {
    c.envs(askpass_env(id));
}

/// When the app was started as `ssh`'s askpass program: answers and exits. Only password and
/// passphrase prompts get the password; anything else (like a new host key) is refused.
pub fn askpass() {
    let Ok(id) = std::env::var(ASKPASS_ENV) else {
        return;
    };
    let prompt = std::env::args().nth(1).unwrap_or_default().to_lowercase();
    if prompt.contains("password") || prompt.contains("passphrase") {
        if let Some(p) = get(&id) {
            println!("{p}");
            std::process::exit(0);
        }
    }
    std::process::exit(1);
}
