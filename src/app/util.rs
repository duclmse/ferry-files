//! Small helpers: file kinds, sizes and dates, paths, and settings kept in localStorage.

use super::*;

pub(super) const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "json", "xml", "html", "htm", "xhtml", "css", "js", "csv", "tsv", "log", "ini",
    "conf", "cfg", "yaml", "yml", "toml", "properties", "opf", "ncx", "svg", "srt", "vtt", "sh", "py", "kt",
    "java", "gradle", "env",
];
pub(super) const TEXT_MAX: u64 = 5 * 1024 * 1024;

pub(super) fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Folder,
    Epub,
    Text,
    Other,
}

pub(super) fn kind(e: &Entry) -> Kind {
    let x = ext(&e.name);
    if e.dir {
        Kind::Folder
    } else if x == "epub" {
        Kind::Epub
    } else if TEXT_EXTS.contains(&x.as_str()) && e.size <= TEXT_MAX {
        Kind::Text
    } else {
        Kind::Other
    }
}

pub(super) fn human_size(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if unit == "B" { format!("{v:.0} B") } else { format!("{v:.1} {unit}") };
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

pub(super) fn when(mtime: i64) -> String {
    if mtime <= 0 {
        return String::new();
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(mtime as f64 * 1000.0));
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date(),
        d.get_hours(),
        d.get_minutes()
    )
}

pub(super) fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

pub(super) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

pub(super) fn base(path: &str) -> String {
    match path.trim_end_matches('/').rsplit('/').next() {
        Some("") | None => "/".into(),
        Some(b) => b.into(),
    }
}

pub(super) fn icon(k: Kind) -> &'static str {
    match k {
        Kind::Folder => "📁",
        Kind::Epub => "📘",
        Kind::Text => "📝",
        Kind::Other => "📄",
    }
}

pub(super) const THUMB_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "epub"];
/// Same limit as the backend (src-tauri/src/thumbs.rs): bigger files aren't read for a thumbnail.
pub(super) const THUMB_MAX: u64 = 60 * 1024 * 1024;

/// Thumbnail URL for images and EPUBs (their cover), served by the backend's `yomi://` scheme.
pub(super) fn thumb_url(src: Src, e: &Entry) -> Option<String> {
    (!e.dir && e.size <= THUMB_MAX && THUMB_EXTS.contains(&ext(&e.name).as_str())).then(|| {
        let p = String::from(js_sys::encode_uri_component(&e.path));
        format!("yomi://localhost/thumb?src={}&p={p}&s={}&m={}", src.key(), e.size, e.mtime)
    })
}

/// Whether `path` is `dir` or inside it.
pub(super) fn within(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    path == dir || dir.is_empty() || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// Every folder from `root` down to `path`, e.g. ("/a", "/a/b/c") gives ["/a", "/a/b", "/a/b/c"].
/// Just `path` when it isn't inside `root`.
pub(super) fn chain_from(root: &str, path: &str) -> Vec<String> {
    if root.is_empty() || !within(path, root) {
        return vec![path.to_string()];
    }
    let mut out = vec![root.to_string()];
    let mut acc = root.trim_end_matches('/').to_string();
    for part in path[acc.len()..].split('/').filter(|p| !p.is_empty()) {
        acc = format!("{acc}/{part}");
        out.push(acc.clone());
    }
    out
}

pub(super) fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

pub(super) fn stored(key: &str) -> Option<String> {
    storage().and_then(|s| s.get_item(key).ok().flatten())
}

pub(super) fn store(key: &str, value: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, value);
    }
}

pub(super) const SPLIT_KEY: &str = "yomi-files.split";
pub(super) const HIDDEN_KEY: &str = "yomi-files.hidden";
pub(super) const PHONE_THUMBS_KEY: &str = "yomi-files.phone-thumbs";
pub(super) const AUTO_CONNECT_KEY: &str = "yomi-files.auto-connect";

/// A setting kept in localStorage as "1" or "0".
pub(super) fn stored_flag(key: &str, default: bool) -> bool {
    stored(key).map_or(default, |v| v == "1")
}

/// "Folder", "EPUB book", "Text", or the file's extension ("PDF").
pub(super) fn kind_label(e: &Entry) -> String {
    match kind(e) {
        Kind::Folder => "Folder".into(),
        Kind::Epub => "EPUB book".into(),
        Kind::Text => "Text".into(),
        Kind::Other => match ext(&e.name) {
            x if x.is_empty() => "File".into(),
            x => x.to_uppercase(),
        },
    }
}
