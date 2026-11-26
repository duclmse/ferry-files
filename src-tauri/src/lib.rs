pub mod books;
pub mod device;
pub mod devices;
mod dupes;
mod files;
#[cfg(feature = "fsd")]
mod fsd;
mod fsd_service;
mod idle;
mod import;
pub mod local;
mod migrate;
pub mod platform;
pub mod preview;
mod protocol;
mod safety;
mod scripts;
mod servers;
mod settings;
mod terminal;
mod transfer;
pub use servers::secrets::askpass;

use books::epub::Book;
use device::{DevError, Device, Entry, WriteResult};
use serde::Serialize;
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
        devices::mtp::stop_copies();
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
            ids.iter()
                .filter_map(|id| all.remove(id))
                .map(|c| c.dev)
                .collect()
        };
        self.viewing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (s, _)| !ids.contains(s));
        self.missing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id| !ids.contains(id));
        let_go(devs, std::time::Duration::from_secs(5));
    }

    pub fn is_viewing(&self, src: &str, dir: &str) -> bool {
        self.viewing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|(s, d)| s == src && d == dir)
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
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    migrate::run();
    std::thread::spawn(preview::thumbs::trim_caches);
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::default())
        .register_asynchronous_uri_scheme_protocol("ferry", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || responder.respond(protocol::handle(&app, request)));
        })
        .invoke_handler(tauri::generate_handler![
            preview::player::pop_out,
            devices::connect::devices,
            devices::connect::connect,
            devices::connect::disconnect,
            devices::connect::connections,
            devices::connect::info,
            devices::connect::watch_devices,
            devices::wifi::adb_pair,
            devices::wifi::adb_connect,
            devices::wifi::adb_discover,
            files::list,
            files::list_page,
            files::refresh,
            files::read_text,
            files::peek_text,
            files::preview::media_info,
            files::preview::peek_bytes,
            files::preview::subtitles,
            files::preview::subtitle,
            files::preview::zip_list,
            files::preview::pdf_outline,
            files::preview::pdf_find,
            files::preview::pdf_pages,
            files::preview::pdf_marks,
            files::preview::pdf_text,
            files::preview::open_link,
            files::preview::epub_contents,
            files::preview::epub_peek,
            files::preview::epub_peek_find,
            files::copy_text,
            files::clip_files,
            files::clip_set,
            files::write_text,
            files::colour_text,
            files::mkdir,
            files::remove,
            files::move_to_trash,
            files::remove_forever,
            files::remove_tree,
            files::download,
            files::upload,
            safety::backups,
            safety::restore,
            safety::clear_backups,
            safety::eject,
            files::find::quick_look,
            transfer::transfer,
            transfer::cancel_transfer,
            transfer::compare,
            transfer::compare_marks,
            dupes::duplicates,
            import::import_photos,
            transfer::sync,
            books::library::library,
            books::library::book_info,
            books::library::book_check,
            books::library::book_change,
            books::library::book_shrink,
            files::rename,
            safety::space,
            files::open_file,
            files::find::reveal,
            files::find::find_files,
            settings::app_folders,
            settings::open_folder,
            settings::clear_cache,
            fsd_service::fsd_status,
            fsd_service::fsd_install,
            fsd_service::fsd_uninstall,
            fsd_service::fsd_settings,
            fsd_service::fsd_set_settings,
            scripts::scripts,
            scripts::run_script,
            scripts::stop_script,
            scripts::pick_scripts,
            scripts::new_script,
            scripts::script_guide,
            servers::everything::everything_connect,
            servers::everything::everything_find,
            servers::sftp::sftp_connect,
            servers::webdav::webdav_connect,
            servers::ftp::ftp_connect,
            servers::s3::s3_connect,
            servers::smb::smb_mount,
            servers::smb::smb_mounts,
            servers::smb::smb_unmount,
            servers::secrets::secret_set,
            servers::secrets::secret_clear,
            terminal::term_open,
            terminal::term_write,
            terminal::term_resize,
            terminal::term_close,
            books::editor::epub_open,
            books::editor::epub_chapter,
            books::editor::epub_set_source,
            books::editor::epub_set_body,
            books::editor::epub_set_meta,
            books::editor::epub_cover,
            books::editor::epub_find,
            books::editor::epub_replace,
            books::editor::epub_move,
            books::editor::epub_rename_chapter,
            books::editor::epub_replace_one,
            books::editor::epub_toc_add,
            books::editor::epub_toc_remove,
            books::editor::epub_save,
            books::editor::epub_export,
            books::editor::epub_close,
        ])
        .build(tauri::generate_context!())
        .expect("error while running Ferry Files");
    #[cfg(unix)]
    quit_on_signals(app.handle().clone());
    app.run(|app, event| match event {
        // The window is made now, once the app runs (not when it's built).
        tauri::RunEvent::Ready => platform::webview_ready(app),
        tauri::RunEvent::Exit => state(app).close_all(),
        _ => {}
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
            eprintln!(
                "Ferry Files: can't catch Ctrl+C ({e}); it will quit without letting go of phones."
            );
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
