//! The file browser UI: connect screen, folder listing, and a text editor.

use crate::api::{call, Devices, Entry, Info, TextFile, WriteResult};
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "json", "xml", "html", "htm", "xhtml", "css", "js", "csv", "tsv", "log", "ini",
    "conf", "cfg", "yaml", "yml", "toml", "properties", "opf", "ncx", "svg", "srt", "vtt", "sh", "py", "kt",
    "java", "gradle", "env",
];
const TEXT_MAX: u64 = 5 * 1024 * 1024;

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Folder,
    Epub,
    Text,
    Other,
}

fn kind(e: &Entry) -> Kind {
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

fn human_size(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 {
            return if unit == "B" { format!("{v:.0} B") } else { format!("{v:.1} {unit}") };
        }
        v /= 1024.0;
    }
    format!("{v:.1} TB")
}

fn when(mtime: i64) -> String {
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

fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

#[derive(Clone, PartialEq)]
enum Notice {
    None,
    Error(String),
    Ok(String),
    ConfirmDelete(Entry),
    /// Asking for a name; `folder` says whether it's a new folder or a new text file.
    NewItem { folder: bool },
}

#[derive(Clone, PartialEq)]
struct Editor {
    path: String,
    /// Version of the device copy we started from; saving refuses if the device copy differs.
    version: String,
    saved_text: String,
}

#[derive(Clone, Copy)]
struct Ctx {
    info: RwSignal<Option<Info>>,
    cwd: RwSignal<String>,
    entries: RwSignal<Vec<Entry>>,
    loading: RwSignal<bool>,
    selected: RwSignal<Option<String>>,
    notice: RwSignal<Notice>,
    editor: RwSignal<Option<Editor>>,
    text: RwSignal<String>,
    status: RwSignal<(String, &'static str)>,
    confirm_discard: RwSignal<bool>,
    busy: RwSignal<bool>,
    ep: crate::epub::Ep,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            info: RwSignal::new(None),
            cwd: RwSignal::new(String::new()),
            entries: RwSignal::new(vec![]),
            loading: RwSignal::new(false),
            selected: RwSignal::new(None),
            notice: RwSignal::new(Notice::None),
            editor: RwSignal::new(None),
            text: RwSignal::new(String::new()),
            status: RwSignal::new((String::new(), "")),
            confirm_discard: RwSignal::new(false),
            busy: RwSignal::new(false),
            ep: crate::epub::Ep::new(),
        }
    }

    fn dirty(&self) -> bool {
        self.editor.with(|e| e.as_ref().is_some_and(|e| e.saved_text != self.text.get()))
    }

    fn error(&self, msg: impl Into<String>) {
        self.notice.set(Notice::Error(msg.into()));
    }

    /// Lists `path` and makes it the current folder. Keeps the old listing if it fails.
    fn load(self, path: String) {
        self.loading.set(true);
        spawn_local(async move {
            match call::<Vec<Entry>>("list", json!({ "path": path })).await {
                Ok(entries) => {
                    self.cwd.set(path);
                    self.entries.set(entries);
                    self.selected.set(None);
                    if matches!(self.notice.get_untracked(), Notice::Error(_)) {
                        self.notice.set(Notice::None);
                    }
                }
                Err(e) => self.error(e),
            }
            self.loading.set(false);
        });
    }

    fn reload(self) {
        self.load(self.cwd.get_untracked());
    }

    /// Navigation that would leave the editor asks first when there are unsaved changes.
    fn navigate(self, path: String) {
        if self.editor.get_untracked().is_some() {
            if self.dirty() {
                self.confirm_discard.set(true);
                return;
            }
            self.editor.set(None);
        }
        self.load(path);
    }

    fn open(self, e: Entry) {
        match kind(&e) {
            Kind::Folder => self.navigate(e.path),
            Kind::Epub => {
                self.loading.set(true);
                let ep = self.ep;
                spawn_local(async move {
                    if let Err(err) = ep.open(e.path).await {
                        self.error(err);
                    }
                    self.loading.set(false);
                });
            }
            Kind::Text => self.open_text(e.path),
            Kind::Other => self.download(e.path),
        }
    }

    fn open_text(self, path: String) {
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "path": path })).await {
                Ok(f) => {
                    self.text.set(f.text.clone());
                    self.editor.set(Some(Editor { path, version: f.version, saved_text: f.text }));
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                }
                Err(e) => self.error(e),
            }
            self.busy.set(false);
        });
    }

    fn save(self) {
        let Some(ed) = self.editor.get_untracked() else { return };
        if self.busy.get_untracked() {
            return;
        }
        let text = self.text.get_untracked();
        self.busy.set(true);
        self.status.set(("Saving…".into(), ""));
        spawn_local(async move {
            let args = json!({ "path": ed.path, "text": text, "base_version": ed.version, "create": false });
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    self.editor.set(Some(Editor { version: r.version, saved_text: text, ..ed }));
                    self.confirm_discard.set(false);
                    let note = if r.backup.is_some() { " The old version is backed up on the Mac." } else { "" };
                    self.status.set((format!("Saved to the device.{note}"), "ok"));
                }
                Err(e) => self.status.set((e, "error")),
            }
            self.busy.set(false);
        });
    }

    fn close_editor(self, force: bool) {
        if !force && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        self.editor.set(None);
        self.confirm_discard.set(false);
        self.reload();
    }

    fn download(self, path: String) {
        spawn_local(async move {
            match call::<Option<String>>("download", json!({ "path": path })).await {
                Ok(Some(dest)) => self.notice.set(Notice::Ok(format!("Saved to {dest}"))),
                Ok(None) => {}
                Err(e) => self.error(e),
            }
        });
    }

    fn upload(self) {
        let dir = self.cwd.get_untracked();
        self.busy.set(true);
        spawn_local(async move {
            match call::<Vec<String>>("upload", json!({ "dir": dir })).await {
                Ok(lines) if lines.is_empty() => {}
                Ok(lines) => {
                    self.notice.set(Notice::Ok(lines.join("\n")));
                    self.reload();
                }
                Err(e) => self.error(e),
            }
            self.busy.set(false);
        });
    }

    fn delete(self, e: Entry) {
        spawn_local(async move {
            match call::<Option<String>>("remove", json!({ "path": e.path })).await {
                Ok(backup) => {
                    let note = backup.map(|b| format!(" Backup: {b}")).unwrap_or_default();
                    self.notice.set(Notice::Ok(format!("Deleted {}.{note}", e.name)));
                    self.reload();
                }
                Err(err) => self.error(err),
            }
        });
    }

    fn create(self, name: String, folder: bool) {
        let name = name.trim().to_string();
        if name.is_empty() || name.contains('/') {
            self.error("Enter a name without /.");
            return;
        }
        let path = join(&self.cwd.get_untracked(), &name);
        spawn_local(async move {
            let result = if folder {
                call::<()>("mkdir", json!({ "path": path })).await
            } else {
                call::<WriteResult>("write_text", json!({ "path": path, "text": "", "base_version": null, "create": true }))
                    .await
                    .map(|_| ())
            };
            match result {
                Ok(()) => {
                    self.notice.set(Notice::None);
                    if folder { self.reload() } else { self.open_text(path) }
                }
                Err(e) => self.error(e),
            }
        });
    }

    fn connect(self, transport: &'static str, conn_error: RwSignal<Option<String>>) {
        self.busy.set(true);
        conn_error.set(None);
        spawn_local(async move {
            match call::<Info>("connect", json!({ "transport": transport, "serial": null })).await {
                Ok(info) => {
                    let start = info.default_dir.clone();
                    self.info.set(Some(info));
                    self.load(start);
                }
                Err(e) => conn_error.set(Some(e)),
            }
            self.busy.set(false);
        });
    }

    fn disconnect(self) {
        if self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        spawn_local(async move {
            let _ = call::<()>("disconnect", json!({})).await;
            self.info.set(None);
            self.editor.set(None);
            self.entries.set(vec![]);
            self.notice.set(Notice::None);
        });
    }
}

#[component]
pub fn App() -> impl IntoView {
    let ctx = Ctx::new();
    provide_context(ctx);
    let ep = ctx.ep;
    provide_context(ep);
    // Closing a book goes back to the listing, which may have changed (a save writes the file and its backup).
    Effect::new(move |was_open: Option<bool>| {
        let open = ep.book.with(|b| b.is_some());
        if was_open == Some(true) && !open {
            ctx.reload();
        }
        open
    });

    // Pick up an existing connection after a webview reload.
    spawn_local(async move {
        if let Ok(Some(info)) = call::<Option<Info>>("info", json!({})).await {
            let start = info.default_dir.clone();
            ctx.info.set(Some(info));
            ctx.load(start);
        }
    });

    // Keyboard: ⌘S saves; in the listing ↑/↓ move, Enter or ⌘↓ opens, ⌘↑ or Backspace goes up.
    let _keys = window_event_listener(ev::keydown, move |e| {
        let meta = e.meta_key() || e.ctrl_key();
        if ep.book.with_untracked(|b| b.is_some()) {
            if meta && e.key() == "s" {
                e.prevent_default();
                ep.save();
            }
            return;
        }
        if ctx.editor.get_untracked().is_some() {
            if meta && e.key() == "s" {
                e.prevent_default();
                ctx.save();
            }
            return;
        }
        if ctx.info.get_untracked().is_none() || !matches!(ctx.notice.get_untracked(), Notice::None | Notice::Ok(_) | Notice::Error(_)) {
            return;
        }
        let entries = ctx.entries.get_untracked();
        let idx = ctx.selected.get_untracked().and_then(|p| entries.iter().position(|x| x.path == p));
        match (e.key().as_str(), meta) {
            ("ArrowDown", false) | ("ArrowUp", false) => {
                e.prevent_default();
                let next = match (idx, e.key() == "ArrowDown") {
                    (None, _) => 0,
                    (Some(i), true) => (i + 1).min(entries.len().saturating_sub(1)),
                    (Some(i), false) => i.saturating_sub(1),
                };
                if let Some(x) = entries.get(next) {
                    ctx.selected.set(Some(x.path.clone()));
                }
            }
            ("Enter", _) | ("ArrowDown", true) => {
                if let Some(x) = idx.and_then(|i| entries.get(i)) {
                    e.prevent_default();
                    ctx.open(x.clone());
                }
            }
            ("Backspace", _) | ("ArrowUp", true) => {
                let cwd = ctx.cwd.get_untracked();
                if cwd != "/" {
                    e.prevent_default();
                    ctx.navigate(parent(&cwd));
                }
            }
            _ => {}
        }
    });

    view! {
        {move || if ctx.info.get().is_some() { view! { <Browser/> }.into_any() } else { view! { <Connect/> }.into_any() }}
    }
}

#[component]
fn Connect() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let devices = RwSignal::new(None::<Devices>);
    let conn_error = RwSignal::new(None::<String>);
    let scan = move || {
        devices.set(None);
        spawn_local(async move {
            match call::<Devices>("devices", json!({})).await {
                Ok(d) => devices.set(Some(d)),
                Err(e) => conn_error.set(Some(e)),
            }
        });
    };
    scan();

    view! {
        <div class="connect">
            <h1>"yomi-files"</h1>
            <p>"Browse, read and edit files on an Android phone connected by USB."</p>
            <dl>
                <dt>"adb"</dt>
                <dd>{move || match devices.get() {
                    None => "Looking…".to_string(),
                    Some(d) if !d.adb_found => "adb isn't installed (brew install --cask android-platform-tools).".into(),
                    Some(d) if d.adb.is_empty() => "No device. Needs USB debugging, authorised on the phone.".into(),
                    Some(d) => d.adb.join(", "),
                }}</dd>
                <dt>"MTP"</dt>
                <dd>{move || match devices.get() {
                    None => "Looking…".to_string(),
                    Some(Devices { mtp_error: Some(e), .. }) => e,
                    Some(d) if d.mtp.is_empty() => "No device. Set the phone's USB mode to \"File transfer\".".into(),
                    Some(d) => d.mtp.join(", "),
                }}</dd>
            </dl>
            <div class="row">
                <button class="btn primary" disabled=move || ctx.busy.get() on:click=move |_| ctx.connect("auto", conn_error)>
                    {move || if ctx.busy.get() { view! { <span class="spin"></span>" Connecting…" }.into_any() } else { "Connect".into_any() }}
                </button>
                <button class="btn" disabled=move || ctx.busy.get() on:click=move |_| ctx.connect("adb", conn_error)>"Use adb"</button>
                <button class="btn" disabled=move || ctx.busy.get() on:click=move |_| ctx.connect("mtp", conn_error)>"Use MTP"</button>
                <span class="spacer"></span>
                <button class="btn" on:click=move |_| scan()>"Scan again"</button>
            </div>
            {move || conn_error.get().map(|e| view! { <p class="error">{e}</p> })}
            <p>"Connect uses adb when a device is ready, otherwise MTP. Over MTP, close Android File Transfer and yomi-files.py first: only one app can use the phone at a time."</p>
        </div>
    }
}

#[component]
fn Browser() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let ep = ctx.ep;
    move || if ep.book.with(|b| b.is_some()) { view! { <crate::epub::EpubEditor/> }.into_any() } else { view! {
        <div class="app">
            <Sidebar/>
            <div class="main">
                {move || if ctx.editor.get().is_some() { view! { <TextEditor/> }.into_any() } else { view! { <Listing/> }.into_any() }}
            </div>
        </div>
    }.into_any() }
}

#[component]
fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let info = move || ctx.info.get().unwrap_or_else(|| Info {
        label: String::new(), transport: String::new(), default_dir: "/".into(), quick_dirs: vec![], backup_dir: String::new(),
    });
    view! {
        <nav class="side">
            <div class="device">
                <b>{move || info().label}</b>
                <span class="kind">{move || info().transport}</span>
            </div>
            <h2>"Places"</h2>
            {move || info().quick_dirs.into_iter().map(|(name, path)| {
                let p = path.clone();
                let active = move || ctx.cwd.get() == p;
                view! {
                    <button class="item" class:active=active on:click=move |_| ctx.navigate(path.clone())>{name}</button>
                }
            }).collect_view()}
            <button class="item" class:active=move || ctx.cwd.get() == "/" on:click=move |_| ctx.navigate("/".into())>"/ (all storage)"</button>
            <span class="grow"></span>
            <button class="btn" on:click=move |_| ctx.disconnect()>"Disconnect"</button>
            <div class="foot">"Backups of replaced and deleted files: " {move || info().backup_dir}</div>
        </nav>
    }
}

#[component]
fn Crumbs() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="crumbs">
            {move || {
                let cwd = ctx.cwd.get();
                let mut acc = String::new();
                let mut out = vec![view! { <button on:click=move |_| ctx.navigate("/".into())>"/"</button> }.into_any()];
                for part in cwd.split('/').filter(|p| !p.is_empty()) {
                    acc = format!("{acc}/{part}");
                    let target = acc.clone();
                    out.push(view! {
                        <button on:click=move |_| ctx.navigate(target.clone())>{part.to_string()}</button>
                    }.into_any());
                    out.push(view! { <span class="sep">"/"</span> }.into_any());
                }
                out.pop_if(|_| cwd != "/");
                out
            }}
        </div>
    }
}

#[component]
fn Notices() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let name = RwSignal::new(String::new());
    view! {
        {move || match ctx.notice.get() {
            Notice::None => ().into_any(),
            Notice::Error(m) => view! {
                <div class="notice error"><span class="text">{m}</span>
                    <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Dismiss"</button></div>
            }.into_any(),
            Notice::Ok(m) => view! {
                <div class="notice ok"><span class="text">{m}</span>
                    <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Dismiss"</button></div>
            }.into_any(),
            Notice::ConfirmDelete(e) => {
                let what = if e.dir { "the empty folder" } else { "" };
                let label = format!("Delete {what} {} from the device?{}", e.name,
                    if e.dir { "" } else { " A backup is kept on the Mac." });
                view! {
                    <div class="notice"><span class="text">{label}</span>
                        <button class="btn danger solid" on:click=move |_| ctx.delete(e.clone())>"Delete"</button>
                        <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Cancel"</button></div>
                }.into_any()
            }
            Notice::NewItem { folder } => {
                name.set(String::new());
                let submit = move || ctx.create(name.get_untracked(), folder);
                view! {
                    <div class="notice">
                        <span>{if folder { "New folder:" } else { "New text file:" }}</span>
                        <input autofocus placeholder=if folder { "Folder name" } else { "notes.txt" }
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => submit(),
                                "Escape" => ctx.notice.set(Notice::None),
                                _ => {}
                            }/>
                        <button class="btn primary" on:click=move |_| submit()>"Create"</button>
                        <button class="btn" on:click=move |_| ctx.notice.set(Notice::None)>"Cancel"</button>
                    </div>
                }.into_any()
            }
        }}
    }
}

#[component]
fn Listing() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let selected_entry = move || {
        let sel = ctx.selected.get()?;
        ctx.entries.with(|es| es.iter().find(|e| e.path == sel).cloned())
    };
    let in_storage = move || ctx.cwd.get() != "/" || ctx.info.get().is_some_and(|i| i.transport == "adb");
    view! {
        <div class="bar">
            <button class="btn" title="Up (⌘↑)" disabled=move || ctx.cwd.get() == "/"
                on:click=move |_| ctx.navigate(parent(&ctx.cwd.get_untracked()))>"↑"</button>
            <Crumbs/>
            {move || ctx.loading.get().then(|| view! { <span class="spin"></span> })}
            <button class="btn" title="Re-read the folder from the device" on:click=move |_| spawn_local(async move {
                let _ = call::<()>("refresh", json!({})).await;
                ctx.reload();
            })>"Refresh"</button>
            <button class="btn" disabled=move || !in_storage() on:click=move |_| ctx.notice.set(Notice::NewItem { folder: true })>"New folder"</button>
            <button class="btn" disabled=move || !in_storage() on:click=move |_| ctx.notice.set(Notice::NewItem { folder: false })>"New file"</button>
            <button class="btn" disabled=move || !in_storage() || ctx.busy.get() on:click=move |_| ctx.upload()>"Upload…"</button>
        </div>
        {move || selected_entry().map(|e| {
            let (e1, e2, e3) = (e.clone(), e.clone(), e.clone());
            let open_label = match kind(&e) {
                Kind::Folder => "Open", Kind::Epub => "Open in EPUB editor", Kind::Text => "Edit", Kind::Other => "Download…",
            };
            view! {
                <div class="notice">
                    <span class="text">{e.name.clone()}</span>
                    <button class="btn primary" on:click=move |_| ctx.open(e1.clone())>{open_label}</button>
                    {(!e.dir && kind(&e) != Kind::Other).then(|| view! {
                        <button class="btn" on:click=move |_| ctx.download(e2.path.clone())>"Download…"</button>
                    })}
                    <button class="btn danger" on:click=move |_| ctx.notice.set(Notice::ConfirmDelete(e3.clone()))>"Delete…"</button>
                </div>
            }
        })}
        <Notices/>
        <div class="list">
            <table>
                <thead><tr><th>"Name"</th><th class="num">"Modified"</th><th class="num">"Size"</th></tr></thead>
                <tbody>
                    <For each=move || ctx.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                        {
                            let k = kind(&e);
                            let (p1, p2) = (e.path.clone(), e.path.clone());
                            let e_open = e.clone();
                            let icon = match k { Kind::Folder => "📁", Kind::Epub => "📘", Kind::Text => "📝", Kind::Other => "📄" };
                            let tag = match k { Kind::Epub => "EPUB", Kind::Text => "text", _ => "" };
                            view! {
                                <tr class:selected=move || ctx.selected.get().as_deref() == Some(p1.as_str())
                                    on:click=move |_| ctx.selected.set(Some(p2.clone()))
                                    on:dblclick=move |_| ctx.open(e_open.clone())>
                                    <td class="name"><span class="icon">{icon}</span>{e.name.clone()}
                                        {(!tag.is_empty()).then(|| view! { <span class="kind">{tag}</span> })}</td>
                                    <td class="num">{when(e.mtime)}</td>
                                    <td class="num">{if e.dir { String::new() } else { human_size(e.size) }}</td>
                                </tr>
                            }
                        }
                    </For>
                </tbody>
            </table>
            {move || (ctx.entries.with(|e| e.is_empty()) && !ctx.loading.get()).then(|| view! {
                <div class="empty">"This folder is empty."</div>
            })}
        </div>
    }
}

#[component]
fn TextEditor() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let path = move || ctx.editor.get().map(|e| e.path).unwrap_or_default();
    let dirty = Memo::new(move |_| ctx.dirty());
    view! {
        <div class="bar">
            <button class="btn" on:click=move |_| ctx.close_editor(false)>"← Files"</button>
            <span class="title">{path}{move || dirty.get().then_some(" •")}</span>
            <span class="spacer"></span>
            <span class=move || format!("status {}", ctx.status.get().1)>{move || ctx.status.get().0}</span>
            <button class="btn primary" title="⌘S" disabled=move || !dirty.get() || ctx.busy.get() on:click=move |_| ctx.save()>"Save"</button>
        </div>
        {move || ctx.confirm_discard.get().then(|| view! {
            <div class="notice">
                <span class="text">"You have unsaved changes."</span>
                <button class="btn primary" on:click=move |_| ctx.save()>"Save"</button>
                <button class="btn danger" on:click=move |_| ctx.close_editor(true)>"Discard changes"</button>
                <button class="btn" on:click=move |_| ctx.confirm_discard.set(false)>"Keep editing"</button>
            </div>
        })}
        <Notices/>
        <div class="editor">
            <textarea spellcheck="false" prop:value=move || ctx.text.get()
                on:input=move |ev| ctx.text.set(event_target_value(&ev))></textarea>
        </div>
    }
}
