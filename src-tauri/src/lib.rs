pub mod adb;
pub mod archive;
pub mod books;
pub mod device;
pub mod epub;
pub mod local;
pub mod media;
mod migrate;
/// MTP over libmtp on macOS and Linux; on Windows over Windows' own Portable Devices API, since
/// libmtp would need the phone's USB driver replaced there.
#[cfg(not(windows))]
pub mod mtp;
#[cfg(windows)]
#[path = "wpd.rs"]
pub mod mtp;
pub mod platform;
mod protocol;
pub mod thumbs;
pub mod usb;
pub mod xml;
mod connect;
mod dupes;
mod everything;
mod import;
mod editor;
mod files;
mod highlight;
mod library;
mod player;
mod safety;
mod settings;
mod transfer;
mod watch;
mod wifi;

use device::{DevError, Device, Entry, WriteResult};
use serde::Serialize;
use epub::Book;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// Text files bigger than this open as downloads instead of in the editor.
const TEXT_MAX: u64 = 5 * 1024 * 1024;

/// A connected device.
struct Conn {
    dev: Arc<Device>,
    /// What it is on USB ("adb:SERIAL" or "mtp:BUS-DEV"), so it isn't offered or connected twice.
    usb: Option<String>,
    /// The USB serial number, the same over adb and MTP: one phone isn't connected both ways.
    serial: Option<String>,
}

pub struct AppState {
    /// Connected devices by id ("d1", "d2", ...). Several can be connected at once.
    devs: Mutex<HashMap<String, Conn>>,
    next_dev: AtomicU32,
    /// The computer's own files, always available.
    mac: Arc<Device>,
    /// Open EPUBs by id. protocol.rs serves their files to the chapter view.
    pub books: Mutex<HashMap<u32, Book>>,
    next_book: AtomicU32,
    /// Where each open book came from ("device" or "mac"), so it's saved back there.
    book_src: Mutex<HashMap<u32, String>>,
    /// The device and folder each pane is showing. Thumbnails for other device folders aren't made.
    viewing: Mutex<HashMap<u32, (String, String)>>,
    /// Held while a thumbnail is being made, so they're made one at a time.
    pub thumb_lock: Mutex<()>,
    /// Cancel flags of running transfers, by job number (chosen by the frontend).
    jobs: Mutex<HashMap<u32, Arc<std::sync::atomic::AtomicBool>>>,
    /// Connected devices not on USB at the last look, by id: gone at the second (see `devices`).
    missing: Mutex<HashSet<String>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            devs: Mutex::default(),
            next_dev: AtomicU32::new(1),
            mac: Arc::new(Device::new(Box::new(local::Local))),
            books: Mutex::default(),
            next_book: AtomicU32::default(),
            book_src: Mutex::default(),
            viewing: Mutex::default(),
            thumb_lock: Mutex::default(),
            jobs: Mutex::default(),
            missing: Mutex::default(),
        }
    }
}

impl AppState {
    /// The files to work on: "mac" for the computer's own, or a connected device's id.
    pub fn source(&self, src: Option<&str>) -> Result<Arc<Device>, String> {
        match src {
            Some("mac") => Ok(self.mac.clone()),
            Some(id) => self
                .devs()
                .get(id)
                .map(|c| c.dev.clone())
                .ok_or_else(|| "That device isn't connected any more.".to_string()),
            None => Err("No device given.".into()),
        }
    }

    fn devs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Conn>> {
        self.devs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lets go of every connected device, for quitting: an MTP phone's session is closed only
    /// when its device is dropped, and quitting doesn't drop the app's state. Transfers are
    /// cancelled too, and what still uses a device gets up to 10 seconds to let go of it (a phone
    /// that stopped answering may never let go).
    fn close_all(&self) {
        mtp::stop_copies();
        for cancel in self.jobs.lock().unwrap_or_else(|e| e.into_inner()).values() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let devs: Vec<Arc<Device>> = self.devs().drain().map(|(_, c)| c.dev).collect();
        let_go(devs, std::time::Duration::from_secs(10));
    }

    /// Disconnects the devices `ids`, and returns once they're let go of (an MTP phone's session
    /// closed, so it can be opened again at once), or after 5 seconds of something still using
    /// one. Their file copies stop; a transfer using one fails at its next file.
    fn close(&self, ids: &[String]) {
        let devs: Vec<Arc<Device>> = {
            let mut all = self.devs();
            ids.iter().filter_map(|id| all.remove(id)).map(|c| c.dev).collect()
        };
        self.viewing.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (s, _)| !ids.contains(s));
        self.missing.lock().unwrap_or_else(|e| e.into_inner()).retain(|id| !ids.contains(id));
        let_go(devs, std::time::Duration::from_secs(5));
    }

    pub fn is_viewing(&self, src: &str, dir: &str) -> bool {
        self.viewing.lock().unwrap_or_else(|e| e.into_inner()).values().any(|(s, d)| s == src && d == dir)
    }
}

/// Stops `devs`' copies and waits up to `within` for whatever else uses them to finish, so
/// they're dropped (and MTP sessions closed) here. One still in use after that is dropped by its
/// last user.
fn let_go(mut devs: Vec<Arc<Device>>, within: std::time::Duration) {
    for d in &devs {
        d.stop();
    }
    let until = std::time::Instant::now() + within;
    while devs.iter().any(|d| Arc::strong_count(d) > 1) && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    devs.clear();
}

fn msg(e: DevError) -> String {
    e.to_string()
}

/// Runs device work off the main thread so the window stays responsive (MTP can be slow).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    migrate::run();
    std::thread::spawn(thumbs::trim_caches);
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::default())
        .register_asynchronous_uri_scheme_protocol("ferry", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || responder.respond(protocol::handle(&app, request)));
        })
        .invoke_handler(tauri::generate_handler![
            player::pop_out,
            connect::devices,
            connect::connect,
            connect::disconnect,
            connect::connections,
            connect::info,
            connect::watch_devices,
            wifi::adb_pair,
            wifi::adb_connect,
            wifi::adb_discover,
            files::list,
            files::refresh,
            files::read_text,
            files::peek_text,
            files::media_info,
            files::peek_bytes,
            files::zip_list,
            files::copy_text,
            files::clip_files,
            files::clip_set,
            files::write_text,
            files::mkdir,
            files::remove,
            files::remove_tree,
            files::download,
            files::upload,
            safety::backups,
            safety::restore,
            safety::clear_backups,
            safety::eject,
            files::quick_look,
            files::find,
            transfer::transfer,
            transfer::cancel_transfer,
            transfer::compare,
            transfer::compare_marks,
            dupes::duplicates,
            import::import_photos,
            transfer::sync,
            library::library,
            library::book_info,
            library::book_check,
            library::book_change,
            library::book_shrink,
            files::rename,
            safety::space,
            files::open_file,
            settings::app_folders,
            settings::open_folder,
            settings::clear_cache,
            everything::everything_connect,
            editor::epub_open,
            editor::epub_chapter,
            editor::epub_set_source,
            editor::epub_set_body,
            editor::epub_set_meta,
            editor::epub_cover,
            editor::epub_find,
            editor::epub_replace,
            editor::epub_save,
            editor::epub_export,
            editor::epub_close,
        ])
        .build(tauri::generate_context!())
        .expect("error while running Ferry Files");
    #[cfg(unix)]
    quit_on_signals(app.handle().clone());
    app.run(|app, event| {
        if let tauri::RunEvent::Exit = event {
            state(app).close_all();
        }
    });
}

/// Ctrl+C in the terminal (as with `cargo tauri dev`), `kill`, or the terminal closing quits the
/// way the app's own Quit does, so connected phones are let go of: a process just ended by the
/// signal leaves the phone's MTP session open, and the next connect fails until it's replugged.
/// A second signal ends the app at once, for a phone that won't let go.
#[cfg(unix)]
fn quit_on_signals(app: AppHandle) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Ferry Files: can't catch Ctrl+C ({e}); it will quit without letting go of phones.");
            return;
        }
    };
    std::thread::spawn(move || {
        let mut seen = signals.forever();
        if seen.next().is_some() {
            app.exit(0);
        }
        if let Some(sig) = seen.next() {
            std::process::exit(128 + sig);
        }
    });
}

pub fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}
