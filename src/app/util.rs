//! Small helpers: file kinds, sizes and dates, paths, and settings kept in localStorage.

use super::*;

/// Folds a bar's parts away, a level at a time, while it can't hold them: `fold-1` on it, then
/// `fold-2` too, up to `levels`, which its CSS hides parts by. Checked whenever its width
/// changes, with every part laid out in one line (`measuring`), before the paint.
pub(crate) fn fold_bar(bar: NodeRef<leptos::html::Div>, levels: u32) {
    let fit = move || {
        let Some(el) = bar.try_get_untracked().flatten() else { return };
        let classes = el.class_list();
        for k in 1..=levels {
            let _ = classes.remove_1(&format!("fold-{k}"));
        }
        let _ = classes.add_1("measuring");
        let mut k = 0;
        while k < levels && el.scroll_width() > el.client_width() + 1 {
            k += 1;
            let _ = classes.add_1(&format!("fold-{k}"));
        }
        let _ = classes.remove_1("measuring");
    };
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, wasm_bindgen::closure::Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(node) = bar.get() else { return };
        let cb = wasm_bindgen::closure::Closure::<dyn Fn()>::new(fit);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&node);
            if let Some((old, _)) = watcher.try_update_value(|w| w.replace((ro, cb))).flatten() {
                old.disconnect();
            }
        }
        request_animation_frame(fit);
    });
    on_cleanup(move || {
        if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
            ro.disconnect();
        }
    });
}

pub(super) const TEXT_EXTS: &[&str] = &[
    "txt",
    "md",
    "markdown",
    "json",
    "xml",
    "html",
    "htm",
    "xhtml",
    "css",
    "js",
    "csv",
    "tsv",
    "log",
    "ini",
    "conf",
    "cfg",
    "yaml",
    "yml",
    "toml",
    "properties",
    "opf",
    "ncx",
    "svg",
    "srt",
    "vtt",
    "sh",
    "py",
    "kt",
    "java",
    "gradle",
    "env",
];
pub(super) const TEXT_MAX: u64 = 5 * 1024 * 1024;

/// Source code by extension, with the language the Kind column names.
const CODE_EXTS: &[(&[&str], &str)] = &[
    (&["rs"], "Rust"),
    (&["c", "h"], "C"),
    (&["cpp", "cc", "cxx", "hpp", "hh", "hxx", "ino"], "C++"),
    (&["m", "mm"], "Objective-C"),
    (&["cs"], "C#"),
    (&["go"], "Go"),
    (&["swift"], "Swift"),
    (&["java"], "Java"),
    (&["kt", "kts"], "Kotlin"),
    (&["scala", "sc"], "Scala"),
    (&["groovy", "gradle"], "Groovy"),
    (&["dart"], "Dart"),
    (&["js", "mjs", "cjs", "jsx"], "JavaScript"),
    (&["ts", "mts", "cts", "tsx"], "TypeScript"),
    (&["vue", "svelte"], "Component"),
    (&["py", "pyw", "pyi"], "Python"),
    (&["rb", "rake", "gemspec"], "Ruby"),
    (&["php"], "PHP"),
    (&["pl", "pm"], "Perl"),
    (&["lua"], "Lua"),
    (&["r"], "R"),
    (&["jl"], "Julia"),
    (&["hs"], "Haskell"),
    (&["ml", "mli"], "OCaml"),
    (&["fs", "fsx"], "F#"),
    (&["ex", "exs"], "Elixir"),
    (&["erl", "hrl"], "Erlang"),
    (&["clj", "cljs", "edn"], "Clojure"),
    (&["lisp", "el", "scm"], "Lisp"),
    (&["zig"], "Zig"),
    (&["nim"], "Nim"),
    (&["d"], "D"),
    (&["pas"], "Pascal"),
    (&["sh", "bash", "zsh", "fish", "command"], "Shell"),
    (&["ps1", "psm1"], "PowerShell"),
    (&["bat", "cmd"], "Batch"),
    (&["sql"], "SQL"),
    (&["html", "htm", "xhtml"], "HTML"),
    (&["css", "scss", "sass", "less"], "CSS"),
    (&["json", "jsonc", "json5"], "JSON"),
    (&["xml", "plist", "xsd", "xsl"], "XML"),
    (&["yaml", "yml"], "YAML"),
    (&["toml"], "TOML"),
    (&["tex", "sty", "bib"], "LaTeX"),
    (&["diff", "patch"], "Diff"),
    (&["proto"], "Protobuf"),
    (&["graphql", "gql"], "GraphQL"),
    (&["tf"], "Terraform"),
    (&["cmake"], "CMake"),
    (&["mk"], "Makefile"),
];

/// Source files known by their whole name.
const CODE_NAMES: &[(&str, &str)] = &[
    ("makefile", "Makefile"),
    ("gnumakefile", "Makefile"),
    ("dockerfile", "Dockerfile"),
    ("containerfile", "Dockerfile"),
    ("cmakelists.txt", "CMake"),
    ("gemfile", "Ruby"),
    ("rakefile", "Ruby"),
    ("podfile", "Ruby"),
    ("vagrantfile", "Ruby"),
    ("justfile", "Makefile"),
];

/// Text without an extension worth naming: dotfiles (".gitignore") and the usual project files.
const TEXT_NAMES: &[&str] = &[
    "readme",
    "license",
    "licence",
    "copying",
    "authors",
    "changelog",
    "notice",
    "todo",
    "procfile",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".editorconfig",
    ".npmrc",
    ".nvmrc",
    ".prettierrc",
    ".eslintrc",
    ".babelrc",
    ".dockerignore",
    ".bashrc",
    ".zshrc",
    ".profile",
    ".bash_profile",
    ".vimrc",
    ".env",
    ".flake8",
    ".clang-format",
];

/// The language of a source file, by its name or extension.
pub(super) fn code_language(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if let Some((_, l)) = CODE_NAMES.iter().find(|(n, _)| *n == lower) {
        return Some(l);
    }
    // "Dockerfile.dev", "app.Dockerfile"
    if lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile") {
        return Some("Dockerfile");
    }
    let x = ext(name);
    CODE_EXTS.iter().find(|(xs, _)| xs.contains(&x.as_str())).map(|(_, l)| *l)
}

/// Whether a file of this name is text the preview and editor can show.
pub(super) fn is_text_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    TEXT_EXTS.contains(&ext(name).as_str()) || code_language(name).is_some() || TEXT_NAMES.contains(&lower.as_str()) || lower.starts_with(".env.")
}

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
    } else if is_text_name(&e.name) && e.size <= TEXT_MAX {
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

/// `s` made safe for a part of a URL, as JavaScript's encodeURIComponent does: everything but
/// letters, digits and -_.!~*'() as %XX bytes of its UTF-8.
pub(super) fn uri_part(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(super) fn when(mtime: i64) -> String {
    if mtime <= 0 {
        return String::new();
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(mtime as f64 * 1000.0));
    format!("{}-{:02}-{:02} {:02}:{:02}", d.get_full_year(), d.get_month() + 1, d.get_date(), d.get_hours(), d.get_minutes())
}

pub(super) fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

pub(super) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

pub(super) fn base(path: &str) -> String {
    match path.trim_end_matches('/').rsplit('/').next() {
        Some("") | None => "/".into(),
        Some(b) => b.into(),
    }
}

pub(super) const THUMB_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "epub"];
/// Same limit as the backend (src-tauri/src/thumbs.rs): bigger files aren't read for a thumbnail.
pub(super) const THUMB_MAX: u64 = 60 * 1024 * 1024;

/// Files the preview can show or play itself, streamed from the device (see `stream_url`).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Media {
    /// Images, shown whole in the preview (sized to it) rather than as a thumbnail.
    Image,
    Video,
    Audio,
}

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "avif", "svg"];
const VIDEO_EXTS: &[&str] = &["mp4", "m4v", "mov", "webm", "3gp"];
const AUDIO_EXTS: &[&str] = &["mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus"];
/// Same as the backend's whole-file limit (protocol.rs): an `<img>` asks for the whole file.
const IMAGE_MAX: u64 = 64 * 1024 * 1024;

pub(super) fn media(e: &Entry) -> Option<Media> {
    let x = ext(&e.name);
    let x = x.as_str();
    if e.dir {
        None
    } else if IMAGE_EXTS.contains(&x) && e.size <= IMAGE_MAX {
        Some(Media::Image)
    } else if VIDEO_EXTS.contains(&x) {
        Some(Media::Video)
    } else if AUDIO_EXTS.contains(&x) {
        Some(Media::Audio)
    } else {
        None
    }
}

/// URL of the file itself, read in parts as the player asks for them, without copying it first.
pub(super) fn stream_url(src: Src, e: &Entry) -> String {
    let p = uri_part(&e.path);
    crate::os::served(&format!("file?src={}&p={p}&s={}&m={}", src.key(), e.size, e.mtime))
}

/// Photos at least this big are shrunk by the backend for the preview (see `fit_url`).
const FIT_FROM: u64 = 2 * 1024 * 1024;

/// URL of a big photo shrunk to about what the preview can show, and upright: its full-size
/// pixels would take far more memory than it needs. `None` for other files.
pub(super) fn fit_url(src: Src, e: &Entry) -> Option<String> {
    (e.size >= FIT_FROM && e.size <= THUMB_MAX && ["jpg", "jpeg", "png", "webp"].contains(&ext(&e.name).as_str())).then(|| {
        let p = uri_part(&e.path);
        crate::os::served(&format!("fit?src={}&p={p}&s={}&m={}", src.key(), e.size, e.mtime))
    })
}

/// Thumbnail URL for images and EPUBs (their cover), served by the backend's `ferry://` scheme.
/// On Windows, any of the computer's own files: Windows makes those (videos, PDFs and so on).
pub(super) fn thumb_url(src: Src, e: &Entry) -> Option<String> {
    let system = src == Src::Mac && crate::os::os() == crate::os::Os::Windows;
    (!e.dir && (system || (e.size <= THUMB_MAX && THUMB_EXTS.contains(&ext(&e.name).as_str())))).then(|| {
        let p = uri_part(&e.path);
        crate::os::served(&format!("thumb?src={}&p={p}&s={}&m={}", src.key(), e.size, e.mtime))
    })
}

/// Whether `path` is `dir` or inside it.
/// A middle press: kept from scrolling or pasting, so its release can close a tab. WebKit has no
/// `auxclick`, so tabs close on the middle button's `mouseup`.
pub(super) fn middle_down(ev: web_sys::MouseEvent) {
    if ev.button() == 1 {
        ev.prevent_default();
    }
}

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

pub(crate) fn stored(key: &str) -> Option<String> {
    storage().and_then(|s| s.get_item(key).ok().flatten())
}

/// Settings saved under the app's old name ("yomi-files.…") move to "ferry-files.…", unless the
/// new name already has one. Runs before anything reads them.
pub(super) fn rename_stored_keys() {
    let Some(s) = storage() else { return };
    let keys: Vec<String> = (0..s.length().unwrap_or(0)).filter_map(|i| s.key(i).ok().flatten()).collect();
    for old in keys {
        let Some(rest) = old.strip_prefix("yomi-files.") else { continue };
        let new = format!("ferry-files.{rest}");
        if let (Ok(Some(v)), Ok(None)) = (s.get_item(&old), s.get_item(&new)) {
            if s.set_item(&new, &v).is_err() {
                continue;
            }
        }
        let _ = s.remove_item(&old);
    }
}

pub(crate) fn store(key: &str, value: &str) {
    if let Some(s) = storage() {
        let _ = s.set_item(key, value);
    }
}

pub(super) const SPLIT_KEY: &str = "ferry-files.split";
pub(super) const SIDE_KEY: &str = "ferry-files.sidebar";
/// Phones connected over Wi-Fi before: [(name, address)].
pub(super) const WIFI_KEY: &str = "ferry-files.wifi";
/// Transfers not finished when the app quit, to resume.
pub(super) const JOBS_KEY: &str = "ferry-files.jobs";
pub(super) const SEARCHES_KEY: &str = "ferry-files.searches";
pub(super) const THEME_KEY: &str = "ferry-files.theme";

/// Shows the app light, dark, or as the Mac is ("auto").
pub(super) fn apply_theme(theme: &str) {
    if let Some(root) = document().document_element() {
        let _ = match theme {
            "light" | "dark" => root.set_attribute("data-theme", theme),
            _ => root.remove_attribute("data-theme"),
        };
    }
}
pub(super) const HIDDEN_KEY: &str = "ferry-files.hidden";
pub(super) const COLOUR_KEY: &str = "ferry-files.colour-code";
pub(super) const WRAP_KEY: &str = "ferry-files.wrap-text";
pub(super) const LINE_NUMBERS_KEY: &str = "ferry-files.line-numbers";
/// The sidebar's sections that are hidden (a JSON list of their keys).
pub(super) const HIDDEN_SECTIONS_KEY: &str = "ferry-files.hidden-sections";
/// The sidebar's sections in the order chosen (a JSON list of `SECTIONS` keys).
pub(super) const SECTION_ORDER_KEY: &str = "ferry-files.section-order";
/// The connections (a JSON list of `servers::Conn`).
pub(super) const CONNS_KEY: &str = "ferry-files.connections";
/// Kept by older versions, read once to fill `CONNS_KEY`: the Everything servers (a JSON list of addresses).
pub(super) const EVERYTHING_KEY: &str = "ferry-files.everything-servers";
/// The SFTP servers added in the sidebar (a JSON list of addresses, like "me@box:2222").
pub(super) const SFTP_KEY: &str = "ferry-files.sftp-servers";
/// The SMB shares added in the sidebar (a JSON list, like "smb://nas/Photos").
pub(super) const SMB_KEY: &str = "ferry-files.smb-shares";
pub(super) const PHONE_THUMBS_KEY: &str = "ferry-files.phone-thumbs";
/// The sidebar opens its folders down to the one the active pane shows.
pub(super) const FOLLOW_FOLDER_KEY: &str = "ferry-files.sidebar-follow";
pub(super) const AUTO_CONNECT_KEY: &str = "ferry-files.auto-connect";
pub(super) const VERIFY_KEY: &str = "ferry-files.verify";

/// The search's kind filter: (label, what the backend calls it).
pub(super) const SEARCH_KINDS: &[(&str, &str)] = &[
    ("Any kind", ""),
    ("Folders", "folder"),
    ("Pictures", "picture"),
    ("Videos", "video"),
    ("Audio", "audio"),
    ("Documents", "document"),
    ("Books", "book"),
    ("Archives", "archive"),
];
const MB: u64 = 1024 * 1024;
/// The search's size filter: (label, at least, under).
pub(super) const SEARCH_SIZES: &[(&str, Option<u64>, Option<u64>)] = &[
    ("Any size", None, None),
    ("Under 1 MB", None, Some(MB)),
    ("1 to 100 MB", Some(MB), Some(100 * MB)),
    ("Over 100 MB", Some(100 * MB), None),
    ("Over 1 GB", Some(1024 * MB), None),
];
/// The search's modified filter: (label, days back; negative: older than that).
pub(super) const SEARCH_TIMES: &[(&str, i64)] =
    &[("Any time", 0), ("Today", 1), ("Past week", 7), ("Past month", 30), ("Past year", 365), ("Over a year ago", -365)];
pub(super) const REGEX_KEY: &str = "ferry-files.filter-regex";
/// Whether a pane's filter pane shows its kind, size and time choices.
pub(super) const OPTIONS_KEY: &str = "ferry-files.search-options";
pub(super) const SIDE_W_KEY: &str = "ferry-files.side-w";
/// The narrowest the sidebar may be dragged; a narrower saved width is widened to it.
pub(super) const SIDE_MIN: f64 = 250.0;
pub(super) const PANE_W_KEY: &str = "ferry-files.pane-w";
/// The narrowest the right pane may be; also `.pane[data-pane="1"]` in views.css.
pub(super) const RIGHT_PANE_MIN: f64 = 360.0;
/// The narrowest the preview may be dragged.
pub(super) const PREVIEW_MIN: f64 = 200.0;

/// A regular expression, or why `source` isn't one.
pub(super) fn regex(source: &str, any_case: bool) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(source).case_insensitive(any_case).build().map_err(|e| format!("That pattern doesn't work: {}", regex_problem(&e)))
}

/// The line of a regex error that says what's wrong, without the pattern drawn above it.
pub(super) fn regex_problem(e: &regex::Error) -> String {
    let m = e.to_string();
    m.lines().find_map(|l| l.strip_prefix("error: ")).map_or(m.clone(), str::to_string)
}

/// Whether `name` contains `lower` (already lowercase) in any case. Most names are ASCII, and
/// are compared as they are rather than copied lowercase.
pub(super) fn contains_any_case(name: &str, lower: &str) -> bool {
    if !name.is_ascii() {
        return name.to_lowercase().contains(lower);
    }
    let (n, t) = (name.as_bytes(), lower.as_bytes());
    t.is_empty() || n.windows(t.len()).any(|w| w.iter().zip(t).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// A pattern with * and ? as the source of a regular expression that matches whole names.
pub(super) fn glob_source(pattern: &str) -> String {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c => out.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
    }
    out.push('$');
    out
}

/// A setting kept in localStorage as "1" or "0".
pub(crate) fn stored_flag(key: &str, default: bool) -> bool {
    stored(key).map_or(default, |v| v == "1")
}

/// "Folder", "EPUB book", the language of source code ("Rust"), "Text", or the file's extension
/// ("PDF").
pub(super) fn kind_label(e: &Entry) -> String {
    match kind(e) {
        Kind::Folder => "Folder".into(),
        Kind::Epub => "EPUB book".into(),
        Kind::Text => code_language(&e.name).unwrap_or("Text").into(),
        Kind::Other => match ext(&e.name) {
            x if x.is_empty() => "File".into(),
            x => x.to_uppercase(),
        },
    }
}

/// Whether some text on the page is selected (⌘C then copies it).
pub(super) fn text_selected() -> bool {
    window().get_selection().ok().flatten().is_some_and(|s| !s.is_collapsed())
}

/// When some of a text preview (or a PDF's pages) is selected, selects all of it (as far as it's
/// loaded or drawn) and says so; ⌘A then picks text, not files.
pub(super) fn select_all_peek() -> bool {
    let Some(sel) = window().get_selection().ok().flatten() else { return false };
    if sel.is_collapsed() {
        return false;
    }
    let Some(pre) = sel
        .anchor_node()
        .and_then(|n| n.dyn_ref::<web_sys::Element>().cloned().or_else(|| n.parent_element()))
        .and_then(|e| e.closest(".peek pre, .pdf-col").ok().flatten())
    else {
        return false;
    };
    sel.select_all_children(&pre).is_ok()
}

/// How long an error stays up while the pointer isn't over it.
const DISMISS_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

/// Closes a notice `DISMISS_AFTER` after it's shown, or after the pointer leaves it: never while
/// the pointer is over it. Each new notice starts the time again. Made once per place notices show.
#[derive(Clone, Copy)]
pub(super) struct AutoDismiss {
    shown: Signal<bool>,
    close: Callback<()>,
    timer: StoredValue<Option<TimeoutHandle>, LocalStorage>,
    hovered: StoredValue<bool>,
}

impl AutoDismiss {
    /// `shown` reads the notice (so every new one is seen); `close` takes it down.
    pub(super) fn new(shown: impl Fn() -> bool + Send + Sync + 'static, close: impl Fn() + Send + Sync + 'static) -> Self {
        let d = AutoDismiss {
            shown: Signal::derive(shown),
            close: Callback::new(move |()| close()),
            timer: StoredValue::new_local(None),
            hovered: StoredValue::new(false),
        };
        Effect::new(move |_| {
            // A new notice is a new element: the pointer isn't over it until it moves there (one
            // dismissed under the pointer never got its mouseleave).
            d.hovered.set_value(false);
            if d.shown.get() {
                d.start();
            } else {
                d.stop();
            }
        });
        on_cleanup(move || d.stop());
        d
    }

    fn start(self) {
        self.stop();
        if self.hovered.get_value() {
            return;
        }
        if let Ok(h) = set_timeout_with_handle(
            move || {
                let _ = self.timer.try_set_value(None);
                if !self.hovered.try_get_value().unwrap_or(true) {
                    self.close.run(());
                }
            },
            DISMISS_AFTER,
        ) {
            self.timer.set_value(Some(h));
        }
    }

    fn stop(self) {
        if let Some(h) = self.timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    }

    /// For the notice's `mouseover` (not only `mouseenter`: one that appears under a still pointer
    /// gets it on the first move).
    pub(super) fn hover(self) {
        if !self.hovered.get_value() {
            self.hovered.set_value(true);
            self.stop();
        }
    }

    /// For the notice's `mouseleave`.
    pub(super) fn leave(self) {
        self.hovered.set_value(false);
        if self.shown.get_untracked() {
            self.start();
        }
    }
}

/// `n` with its thousands grouped: "48,213".
pub(super) fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    out
}

#[cfg(test)]
mod count_tests {
    #[test]
    fn uri_parts() {
        assert_eq!(super::uri_part("/sdcard/My Photos/é#1?.jpg"), "%2Fsdcard%2FMy%20Photos%2F%C3%A9%231%3F.jpg");
        assert_eq!(super::uri_part("a-b_c.d!~*'()"), "a-b_c.d!~*'()");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(super::count(0), "0");
        assert_eq!(super::count(999), "999");
        assert_eq!(super::count(1000), "1,000");
        assert_eq!(super::count(48213), "48,213");
        assert_eq!(super::count(1234567), "1,234,567");
    }
}
