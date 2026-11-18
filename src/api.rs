//! Calls into the Tauri backend (src-tauri/src/lib.rs). Argument names match the commands' snake_case names.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
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
    pub label: String,
    pub transport: String,
    pub default_dir: String,
    pub quick_dirs: Vec<(String, String)>,
    pub backup_dir: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Devices {
    pub adb: Vec<String>,
    pub adb_found: bool,
    pub mtp: Vec<String>,
    pub mtp_error: Option<String>,
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
