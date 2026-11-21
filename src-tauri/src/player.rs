//! A video popped out of the preview into a small window of its own that stays on top of other
//! windows. It loads the app's page, which plays the video alone when it finds `__FERRY_PLAYER`.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use tauri::{AppHandle, WebviewUrl, WebviewWindowBuilder};

/// What the frontend's player hands over (video.rs, `Popped`): passed back to it as is.
#[derive(Serialize, Deserialize)]
pub(super) struct Popped {
    url: String,
    key: String,
    title: String,
    time: f64,
    turn: i32,
    playing: bool,
    width: f64,
    height: f64,
    /// The subtitles loaded, and which one shows.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    tracks: serde_json::Value,
    #[serde(default)]
    shown: Option<usize>,
}

static NEXT: AtomicU32 = AtomicU32::new(1);

#[tauri::command(rename_all = "snake_case")]
pub(super) async fn pop_out(app: AppHandle, video: Popped) -> Result<(), String> {
    let label = format!("player-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let (w, h) = (video.width.clamp(240.0, 1600.0), video.height.clamp(180.0, 1200.0));
    let title = video.title.clone();
    // A JSON string holding the JSON, so the page reads it as a string.
    let data = serde_json::to_string(&serde_json::to_string(&video).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    WebviewWindowBuilder::new(&app, label, WebviewUrl::App("index.html".into()))
        .title(title)
        .inner_size(w, h)
        .min_inner_size(240.0, 180.0)
        .always_on_top(true)
        .initialization_script(format!("window.__FERRY_PLAYER = {data};"))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popped_round_trips_as_the_frontend_writes_it() {
        let v: Popped = serde_json::from_str(
            r#"{"url":"ferry://x/a.mp4","key":"/a.mp4|1|2","title":"a \"b\".mp4","time":12.5,"turn":1,"playing":true,"width":480,"height":334}"#,
        )
        .unwrap();
        let back: serde_json::Value = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(back["title"], "a \"b\".mp4");
        assert_eq!(back["turn"], 1);
    }
}
