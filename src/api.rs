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

/// A file in a group of duplicates; `at` indexes the folders searched.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Dupe {
    pub at: usize,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Dupes {
    pub groups: Vec<Vec<Dupe>>,
    pub files: u32,
    pub complete: bool,
    /// The files came from fsd's index: `files` counts those of a shared size only.
    #[serde(default)]
    pub from_index: bool,
}

/// How far a duplicate search is, sent by the backend as it goes.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct DupeProgress {
    pub job: u32,
    pub text: String,
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

/// One of the user's Lua scripts (Run script…): its name, and what its first comment says.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ScriptInfo {
    pub name: String,
    /// The file, as an app path.
    pub path: String,
    pub about: String,
}

/// What a script did: what it printed, why it stopped early, how many items it changed.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ScriptReport {
    pub output: Vec<String>,
    pub error: Option<String>,
    pub changed: u32,
}

/// What a running script printed since the last of these, and how many items it has changed.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ScriptProgress {
    pub lines: Vec<String>,
    pub changed: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TransferReport {
    pub copied: u32,
    /// With resume: files found already copied (same size) and left as they were.
    #[serde(default)]
    pub already: u32,
    pub skipped: Vec<(String, String)>,
    pub cancelled: bool,
    pub backups: Vec<String>,
    /// Syncing as a mirror: items removed from the target.
    #[serde(default)]
    pub removed: u32,
    /// What the copy made at the target's top level that wasn't there before.
    #[serde(default)]
    pub made: Vec<String>,
    /// Items moved whole in one step on one disk (not counted in `copied`).
    #[serde(default)]
    pub moved: u32,
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

/// How each item of two compared folders stands (by name): "only", "same", "newer", "older"
/// or "differs".
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Marks {
    pub a: std::collections::HashMap<String, String>,
    pub b: std::collections::HashMap<String, String>,
}

/// What syncing one folder into another would copy.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CompareReport {
    pub files: u32,
    pub folders: u32,
    pub bytes: u64,
    /// Relative paths of files that are on both sides but differ in size.
    pub differ: Vec<String>,
    /// Of `differ`, the files an update would replace, and their size.
    #[serde(default)]
    pub changed: u32,
    #[serde(default)]
    pub changed_bytes: u64,
    /// Relative paths only in the target.
    pub extra: Vec<String>,
    pub same: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Space {
    pub name: String,
    pub free: u64,
    pub total: u64,
    /// The folder the storage starts at; "" when unknown.
    #[serde(default)]
    pub path: String,
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

/// fsd, the fs project's search service, as Settings > Search shows it (`fsd_service.rs`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FsdStatus {
    /// The fsd program found, an app path.
    pub exe: Option<String>,
    /// Where fsd is registered as a service, when it is.
    pub service: Option<String>,
    /// Whether that service is Ferry's own, started with `config`.
    pub ours: bool,
    /// Ferry's settings for fsd, an app path.
    pub config: String,
    /// "off" (not answering), "indexing", "ready", or "unknown" in a build without fsd searches.
    pub state: String,
    /// The folders fsd indexes, and how many files and folders it has.
    pub roots: Vec<String>,
    pub entries: u64,
    /// Whether fsd can look inside the folders the system keeps background programs out of
    /// (Desktop, Documents and Downloads on macOS): None when it can't tell yet.
    #[serde(default)]
    pub sees_protected: Option<bool>,
    /// Why fsd's answer couldn't be read.
    pub problem: Option<String>,
}

impl FsdStatus {
    /// Whether it's on its way somewhere, so worth asking again soon: indexing, or Ferry's
    /// service started but not answering yet.
    pub fn changing(&self) -> bool {
        self.state == "indexing" || (self.ours && self.state == "off")
    }

    pub fn running(&self) -> bool {
        matches!(self.state.as_str(), "indexing" | "ready")
    }
}

/// The settings of fsd's that Settings > Search changes (`fsd_service.rs`).
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct FsdSettings {
    /// The folders it indexes, app paths.
    pub folders: Vec<String>,
    /// Patterns of paths it leaves out.
    pub skip: Vec<String>,
    /// Whether it leaves out hidden files and folders.
    pub hidden: bool,
    /// Whether it reads the text of text files, for finding the words in them.
    #[serde(default)]
    pub text: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FsdShown {
    pub settings: FsdSettings,
    /// The patterns fsd's settings start with, for putting back ones taken out.
    pub usual: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct FsdSaved {
    pub settings: FsdSettings,
    /// What happened to them: whether fsd follows them now.
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// Marked hidden by the system (macOS, Windows), apart from a leading dot.
    #[serde(default)]
    pub hidden: bool,
}

impl Entry {
    /// Whether it's hidden: a dot name, or marked hidden by the system.
    pub fn is_hidden(&self) -> bool {
        self.hidden || self.name.starts_with('.')
    }
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

/// An SMB share mounted on the computer.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SmbMount {
    /// "smb://[user@]host/share".
    pub share: String,
    /// Where it's mounted (a path on the computer).
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Devices {
    /// Devices on USB that aren't connected yet.
    pub found: Vec<UsbDevice>,
    pub adb_found: bool,
    pub mtp_error: Option<String>,
    /// Why iPhones can't be looked for (Apple's device service is missing).
    #[serde(default)]
    pub iphone_error: Option<String>,
    /// A phone is on USB with adb but hasn't allowed this Mac yet.
    pub adb_waiting: bool,
    /// Disks mounted on the Mac, as (name, path).
    pub drives: Vec<(String, String)>,
    /// The paths of those that can be ejected (not the disks inside the computer).
    #[serde(default)]
    pub ejectable: Vec<String>,
    /// Connected devices no longer on USB, which were disconnected, as (id, name).
    #[serde(default)]
    pub gone: Vec<(String, String)>,
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

/// A phone with Wireless debugging open, found on the network (`adb_discover`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct WifiService {
    pub name: String,
    /// "pair" (waiting for a pairing code) or "connect" (paired).
    pub kind: String,
    pub address: String,
}

/// Whether an adb serial is a phone reached over Wi-Fi: "host:port", or the name adb gives one
/// it connected to by itself ("adb-…._adb-tls-connect._tcp").
pub fn wireless(serial: &str) -> bool {
    serial.contains(':') || serial.contains("._adb-tls-connect.")
}

impl Info {
    /// A phone (over adb or MTP, or an iPhone), not a server.
    pub fn is_phone(&self) -> bool {
        matches!(self.transport.as_str(), "adb" | "mtp" | "iphone")
    }

    pub fn wireless(&self) -> bool {
        self.transport == "adb" && self.serial.as_deref().is_some_and(wireless)
    }
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
    /// Colours for code: (kind, bytes) runs over `text`; None when it's plain.
    #[serde(default)]
    pub runs: Option<Vec<(u8, u32)>>,
    /// Colours stopped in this piece: the rest of the file is plain.
    #[serde(default)]
    pub colours_stopped: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TextFile {
    pub text: String,
    pub version: String,
}

/// Colours for text being edited (`colour_text`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TextColours {
    /// (kind, bytes) runs over the text; None when it's plain.
    #[serde(default)]
    pub runs: Option<Vec<(u8, u32)>>,
    /// Colours stopped partway: the rest is plain.
    #[serde(default)]
    pub stopped: bool,
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

/// An EPUB's title and chapters, read without opening it (`epub_contents`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct EpubContents {
    pub title: String,
    pub chapters: Vec<ChapterRef>,
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
    /// The table of contents, its entries in order.
    #[serde(default)]
    pub toc: Vec<TocEntry>,
    pub cover: Option<String>,
    pub stats: Stats,
}

/// An entry of a book's table of contents: the chapter it opens (and the id there, if it names
/// a place in it), and its depth, 1 for the outer list.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct TocEntry {
    pub label: String,
    pub path: String,
    pub fragment: Option<String>,
    pub depth: usize,
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
    pub regex: bool,
    pub preserve_case: bool,
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

/// One match replaced (`epub_replace_one`): where it ends in the chapter, if one was left.
#[derive(Clone, Debug, Deserialize)]
pub struct ReplacedOne {
    pub at: Option<usize>,
    pub book: Book,
}

/// What moving part of a chapter did (`epub_move`).
#[derive(Clone, Debug, Deserialize)]
pub struct Moved {
    /// The chapter it went to.
    pub to: String,
    pub blocks: usize,
    /// Links changed to follow the part moved.
    pub links: usize,
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
    /// Packed without compression: read in place, so size doesn't matter.
    #[serde(default)]
    pub stored: bool,
}

/// A PDF's page count and outline (empty when it has none).
/// What a PDF says about itself (the backend's `preview::pdf::Info`).
#[derive(Deserialize, Clone, Debug, PartialEq, Default)]
pub struct PdfInfo {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub pages: usize,
    pub page_size: Option<[f32; 2]>,
    pub version: Option<String>,
    pub encrypted: bool,
    pub attachments: Vec<(String, u64)>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PdfOutline {
    pub pages: usize,
    pub headings: Vec<Heading>,
}

/// Text along one line of a PDF page, to lay over the page drawn: `t` stretched across the box
/// `b` (fractions of the page: left, top, width, height), then `e` ("", a space or a line end).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PdfRun {
    pub t: String,
    pub b: [f64; 4],
    pub e: String,
}

/// A link on a PDF page: its box (as `PdfRun`'s), and the page it goes to (from 1) or the web or
/// mail address it opens.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PdfLink {
    pub b: [f64; 4],
    pub page: Option<usize>,
    pub uri: Option<String>,
}

/// What's laid over a PDF page drawn: its text and its links.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct PdfLayer {
    pub runs: Vec<PdfRun>,
    pub links: Vec<PdfLink>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Heading {
    pub title: String,
    /// 1 for the top level.
    pub level: usize,
    /// From 1.
    pub page: usize,
}

/// What's in a ZIP file: the first items, and how many there are in all.
#[derive(Clone, Debug, Deserialize)]
pub struct Listing {
    pub items: Vec<Packed>,
    pub total: u64,
}
