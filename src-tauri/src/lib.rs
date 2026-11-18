pub mod adb;
pub mod archive;
pub mod books;
pub mod device;
pub mod epub;
pub mod local;
pub mod media;
pub mod mtp;
mod protocol;
pub mod thumbs;
pub mod usb;
pub mod xml;
mod connect;
mod editor;
mod files;
mod library;
mod safety;
mod settings;
mod transfer;

use device::{DevError, Device, Entry, WriteResult};
use serde::Serialize;
use epub::Book;
use std::collections::HashMap;
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
    /// The Mac's own files, always available.
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
        }
    }
}

impl AppState {
    /// The files to work on: "mac" for the Mac's own, or a connected device's id.
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

    pub fn is_viewing(&self, src: &str, dir: &str) -> bool {
        self.viewing.lock().unwrap_or_else(|e| e.into_inner()).values().any(|(s, d)| s == src && d == dir)
    }
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
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .register_asynchronous_uri_scheme_protocol("yomi", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || responder.respond(protocol::handle(&app, request)));
        })
        .invoke_handler(tauri::generate_handler![
            connect::devices,
            connect::connect,
            connect::disconnect,
            connect::connections,
            connect::info,
            files::list,
            files::refresh,
            files::read_text,
            files::peek_text,
            files::media_info,
            files::peek_bytes,
            files::zip_list,
            files::copy_text,
            files::write_text,
            files::mkdir,
            files::remove,
            files::download,
            files::upload,
            safety::backups,
            safety::restore,
            safety::eject,
            files::quick_look,
            files::find,
            transfer::transfer,
            transfer::cancel_transfer,
            transfer::compare,
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
        .run(tauri::generate_context!())
        .expect("error while running yomi-files");
}

pub fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}
