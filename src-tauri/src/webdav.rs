//! WebDAV servers (Nextcloud, ownCloud, NAS boxes, Box…) as drives, over http or https.

use super::*;

/// Connects it as a drive. (Not written yet.)
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn webdav_connect(
    _state: State<'_, AppState>,
    server: String,
    user: Option<String>,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let _ = (server, user, secret);
    Err("Not written yet.".into())
}
