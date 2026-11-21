//! FTP servers as drives, plain or over TLS (explicit FTPS).

use super::*;

/// Connects it as a drive. (Not written yet.)
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn ftp_connect(
    _state: State<'_, AppState>,
    server: String,
    tls: bool,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let _ = (server, tls, user, secret);
    Err("Not written yet.".into())
}
