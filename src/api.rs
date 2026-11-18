//! Calls into the Tauri backend (src-tauri/src/lib.rs). Argument names match the commands' snake_case names.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = listen)]
    fn tauri_listen(event: &str, handler: &js_sys::Function) -> js_sys::Promise;
}

/// Calls `f` with the payload of each backend event named `event`, for as long as the app runs.
pub fn listen<T: DeserializeOwned + 'static>(event: &str, f: impl Fn(T) + 'static) {
    let cb = Closure::<dyn Fn(JsValue)>::new(move |e: JsValue| {
        let payload = js_sys::Reflect::get(&e, &"payload".into()).unwrap_or(JsValue::NULL);
        if let Ok(v) = serde_wasm_bindgen::from_value(payload) {
            f(v);
        }
    });
    let _ = tauri_listen(event, cb.as_ref().unchecked_ref());
    cb.forget();
}

/// A copy or move under way: sent by the backend after each file.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Progress {
    pub job: u32,
    pub files_done: u32,
    pub files_total: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TransferReport {
    pub copied: u32,
    pub skipped: Vec<(String, String)>,
    pub cancelled: bool,
    pub backups: Vec<String>,
}

/// A book's info, for the library.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct BookInfo {
    pub title: String,
    pub creators: Vec<String>,
    pub language: String,
    pub publisher: String,
    pub subjects: Vec<String>,
    pub problem: Option<String>,
}

/// Something wrong with a book.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Issue {
    pub serious: bool,
    pub text: String,
}

/// What syncing one folder into another would copy.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CompareReport {
    pub files: u32,
    pub folders: u32,
    pub bytes: u64,
    /// Relative paths of files that are on both sides but differ in size.
    pub differ: Vec<String>,
    /// Relative paths only in the target.
    pub extra: Vec<String>,
    pub same: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Space {
    pub name: String,
    pub free: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct BackupNote {
    pub file: String,
    /// "mac", or a device's transport and name ("adb:Pixel 7").
    pub place: String,
    pub path: String,
    /// "replaced" or "deleted".
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Backup {
    pub file: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub note: Option<BackupNote>,
}

/// A file dropped on the window from Finder ("tauri://drag-drop"). The position is in physical pixels.
#[derive(Clone, Debug, Deserialize)]
pub struct FileDrop {
    pub paths: Vec<String>,
    pub position: Pos,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

pub async fn call<T: DeserializeOwned>(cmd: &str, args: serde_json::Value) -> Result<T, String> {
    let args = args
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|e| e.to_string())?;
    match invoke(cmd, args).await {
        Ok(v) => serde_wasm_bindgen::from_value(v).map_err(|e| e.to_string()),
        Err(e) => Err(e.as_string().unwrap_or_else(|| format!("{e:?}"))),
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Info {
    /// "mac", or the connected device's id ("d1", ...).
    pub id: String,
    pub usb: Option<String>,
    pub serial: Option<String>,
    pub label: String,
    pub transport: String,
    pub default_dir: String,
    pub quick_dirs: Vec<(String, String)>,
    pub backup_dir: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Devices {
    /// Devices on USB that aren't connected yet.
    pub found: Vec<UsbDevice>,
    pub adb_found: bool,
    pub mtp_error: Option<String>,
    /// A phone is on USB with adb but hasn't allowed this Mac yet.
    pub adb_waiting: bool,
    /// Disks mounted on the Mac, as (name, path).
    pub drives: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct UsbDevice {
    pub usb: String,
    pub transport: String,
    pub which: String,
    pub name: String,
    /// The USB serial number when known: the same phone over adb and MTP has the same one.
    pub serial: Option<String>,
}

impl UsbDevice {
    /// Tells phones apart: the serial, or where it is on USB.
    pub fn key(&self) -> String {
        self.serial.clone().unwrap_or_else(|| self.usb.clone())
    }
}

/// A piece of a text file (`peek_text`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Peek {
    pub text: String,
    /// Where the next piece starts, when the file goes on.
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TextFile {
    pub text: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WriteResult {
    pub version: String,
    pub backup: Option<String>,
}

// ----- EPUB editor (src-tauri/src/epub.rs) -----

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub title: String,
    pub creators: Vec<String>,
    pub language: String,
    pub publisher: String,
    pub date: String,
    pub identifier: String,
    pub subjects: Vec<String>,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ChapterRef {
    pub path: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Stats {
    pub version: String,
    pub chapters: usize,
    pub files: usize,
    pub bytes: u64,
    pub images: usize,
    pub styles: usize,
    pub fonts: usize,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Book {
    pub id: u32,
    /// "device" or "mac": where the book is saved back to.
    pub src: String,
    pub path: String,
    pub name: String,
    pub dirty: bool,
    pub meta: Meta,
    pub chapters: Vec<ChapterRef>,
    pub cover: Option<String>,
    pub stats: Stats,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ChapterDoc {
    pub source: String,
    pub html: Option<String>,
    pub problem: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Query {
    pub find: String,
    pub replace: String,
    pub match_case: bool,
    pub whole_word: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Snippet {
    pub before: String,
    pub text: String,
    pub after: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Hit {
    pub path: String,
    pub label: String,
    pub count: usize,
    pub snippets: Vec<Snippet>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Found {
    pub total: usize,
    pub hits: Vec<Hit>,
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Replaced {
    pub count: usize,
    pub book: Book,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Saved {
    pub book: Book,
    pub backup: bool,
}

/// Width, height and length of a picture, video or sound (`media_info`), each when known.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MediaInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Seconds.
    pub duration: Option<f64>,
}

/// Raw bytes of a file, a piece at a time.
#[derive(Clone, Debug, Deserialize)]
pub struct Bytes {
    pub bytes: Vec<u8>,
    /// Where the next piece starts, when the file goes on.
    pub next: Option<u64>,
}

/// An item in a ZIP file.
#[derive(Clone, Debug, Deserialize)]
pub struct Packed {
    pub name: String,
    pub size: u64,
    pub dir: bool,
}

/// What's in a ZIP file: the first items, and how many there are in all.
#[derive(Clone, Debug, Deserialize)]
pub struct Listing {
    pub items: Vec<Packed>,
    pub total: u64,
}
