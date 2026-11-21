//! S3 buckets (AWS, Cloudflare R2, Backblaze B2, MinIO…) as drives.

use super::*;

/// Connects it as a drive. (Not written yet.)
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn s3_connect(
    _state: State<'_, AppState>,
    endpoint: String,
    region: String,
    bucket: String,
    access_key: String,
    secret: Option<String>,
) -> std::result::Result<connect::Info, String> {
    let _ = (endpoint, region, bucket, access_key, secret);
    Err("Not written yet.".into())
}
