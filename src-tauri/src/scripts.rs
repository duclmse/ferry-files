//! Lua scripts run on the files picked in a pane (Run script ▸ in the context menu), through mlua
//! with Lua 5.4 built in: those in the app's scripts folder, and the files and folders added in
//! Settings > Scripts.
//!
//! A script sees only what `ferry` gives it: no `io`, `os` or `require`, so every change it makes
//! goes through `Device::write`, `Device::remove` and the rest, and is checked and backed up like
//! any other. It runs on a thread of its own, and Stop ends it at its next few instructions.

use super::*;
use mlua::{HookTriggers, IntoLuaMulti, Lua, LuaOptions, StdLib, Table, Value, VmState};
use std::sync::atomic::{AtomicBool, Ordering};

/// Set to stop the script running.
static STOP: AtomicBool = AtomicBool::new(false);
/// Whether a script is running: one at a time.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Most memory a script may take.
const MEMORY: usize = 512 * 1024 * 1024;
/// Biggest file `ferry.read` reads.
const READ_MAX: u64 = 64 * 1024 * 1024;
/// Lines of output kept: the last ones.
const OUTPUT_MAX: usize = 2000;

/// Written to the scripts folder when it's made, to start from.
const EXAMPLE: &str = r#"-- Lists the files picked (or the folder's), with their sizes. Changes nothing.
--
-- Scripts are Lua 5.4, run from Run script in the right-click menu. The first comment line
-- is what Settings > Scripts says about the script. "Scripting guide.md", next to this file,
-- says everything `ferry` gives a script: reading, making, writing, copying, moving and
-- deleting files and folders, each change checked and backed up like any other.

local items = ferry.items
if #items == 0 then
  for _, e in ipairs(ferry.list(ferry.dir)) do
    if not e.dir then items[#items + 1] = e.path end
  end
end
for _, path in ipairs(items) do
  local e = ferry.stat(path)
  if e and not e.dir then
    print(e.name .. "  " .. e.size .. " bytes")
  end
end
"#;

/// The guide to scripting (docs/scripting.md), kept in the scripts folder as GUIDE_NAME.
const GUIDE: &str = include_str!("../../docs/scripting.md");
const GUIDE_NAME: &str = "Scripting guide.md";

/// The scripts folder: `folder` (an app path) when Settings > Scripts has another, else the app's.
fn folder_or_default(folder: Option<&str>) -> Result<std::path::PathBuf, String> {
    match folder.filter(|f| !f.is_empty()) {
        Some(f) => {
            let at = crate::platform::native(f);
            if at.is_dir() {
                Ok(at)
            } else {
                Err(format!(
                    "The scripts folder {} isn't there. Pick another in Settings > Scripts.",
                    at.display()
                ))
            }
        }
        None => dir(),
    }
}

/// The app's scripts folder, made (with the example in it) the first time it's asked for. The
/// guide in it is the app's: written again when it isn't this version's. (A folder picked in
/// Settings instead gets neither: it's the user's.)
fn dir() -> Result<std::path::PathBuf, String> {
    let dir = crate::platform::scripts_dir();
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Can't make the scripts folder {}: {e}", dir.display()))?;
        let _ = std::fs::write(dir.join("list sizes.lua"), EXAMPLE);
    }
    let guide = dir.join(GUIDE_NAME);
    if std::fs::read_to_string(&guide).ok().as_deref() != Some(GUIDE) {
        let _ = std::fs::write(&guide, GUIDE);
    }
    Ok(dir)
}

/// The scripting guide in the scripts folder, as an app path, to open in the editor.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn script_guide() -> Result<String, String> {
    blocking(|| Ok(crate::platform::app_path(&dir()?.join(GUIDE_NAME)))).await
}

#[derive(Serialize)]
pub(super) struct ScriptInfo {
    /// The file's name without ".lua".
    name: String,
    /// The file, as an app path.
    path: String,
    /// Its first comment line.
    about: String,
}

/// The first `--` comment of a script, if it starts with one.
fn about(src: &str) -> String {
    src.lines()
        .next()
        .and_then(|l| l.trim().strip_prefix("--"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

/// The script at `path`, when it's a .lua file.
fn info(path: &std::path::Path) -> Option<ScriptInfo> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let stem = name.strip_suffix(".lua")?.to_string();
    if !path.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    Some(ScriptInfo {
        name: stem,
        path: crate::platform::app_path(path),
        about: about(&text),
    })
}

/// The scripts in the scripts folder (`folder`, or the app's), then in `places` (app paths of .lua files, and of folders
/// whose .lua files are taken), each once, by name.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn scripts(
    places: Vec<String>,
    folder: Option<String>,
) -> Result<Vec<ScriptInfo>, String> {
    blocking(move || {
        let mut found: Vec<ScriptInfo> = vec![];
        let folders = std::iter::once(folder_or_default(folder.as_deref())?)
            .chain(places.iter().map(|p| crate::platform::native(p)));
        for at in folders {
            if at.is_dir() {
                found.extend(
                    std::fs::read_dir(&at)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|e| info(&e.path())),
                );
            } else {
                found.extend(info(&at));
            }
        }
        let mut seen = HashSet::new();
        found.retain(|s| seen.insert(s.path.clone()));
        found.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(found)
    })
    .await
}

/// Makes script `name` (".lua" added) in the scripts folder (`folder`, or the app's), to start writing; returns its app
/// path. Refuses a name that's taken.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn new_script(name: String, folder: Option<String>) -> Result<String, String> {
    blocking(move || {
        let name = name.trim().trim_end_matches(".lua").trim().to_string();
        if name.is_empty() || name.contains(['/', '\\', ':']) || name.starts_with('.') {
            return Err("Give the script a name, without / \\ or : in it.".into());
        }
        let path = folder_or_default(folder.as_deref())?.join(format!("{name}.lua"));
        if path.exists() {
            return Err(format!("There's already a script called {name}."));
        }
        let start = "-- What this script does, in a line (shown in Settings > Scripts).\n\n\
                     -- ferry.items: the paths picked; ferry.dir: the folder. See \"list sizes.lua\" for the rest.\n\
                     for _, path in ipairs(ferry.items) do\n  print(path)\nend\n";
        std::fs::write(&path, start).map_err(|e| format!("Can't make {}: {e}", path.display()))?;
        Ok(crate::platform::app_path(&path))
    })
    .await
}

/// Asks for .lua files (or, with `folder`, a folder of them) to add in Settings > Scripts, as app
/// paths; none when cancelled.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn pick_scripts(app: AppHandle, folder: bool) -> Result<Vec<String>, String> {
    blocking(move || {
        let d = app.dialog().file();
        let picked = if folder {
            d.blocking_pick_folder().into_iter().collect()
        } else {
            d.add_filter("Lua scripts", &["lua"])
                .blocking_pick_files()
                .unwrap_or_default()
        };
        Ok(picked
            .into_iter()
            .filter_map(|f| f.into_path().ok())
            .map(|p| crate::platform::app_path(&p))
            .collect())
    })
    .await
}

#[derive(Serialize)]
pub(super) struct ScriptReport {
    /// What it printed.
    output: Vec<String>,
    /// Why it stopped early: its error, or Stop.
    error: Option<String>,
    /// How many files and folders it wrote, made, renamed or removed.
    changed: u32,
}

/// What a script printed since the last of these, and how much it has changed so far: sent as
/// the "script" event while it runs.
#[derive(Serialize, Clone)]
pub(super) struct ScriptProgress {
    lines: Vec<String>,
    changed: u32,
}

/// How often a running script's new lines are sent.
const TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Runs the script at `script` (an app path of a .lua file on this computer) on `items` (paths on
/// `src`) in folder `dir`. With `trial`, nothing is changed: each change it would make is a line
/// of its output instead. What it prints comes as "script" events while it runs.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn run_script(
    app: AppHandle,
    state: State<'_, AppState>,
    src: String,
    dir: String,
    items: Vec<String>,
    script: String,
    trial: Option<bool>,
) -> Result<ScriptReport, String> {
    use tauri::Emitter;
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let path = crate::platform::native(&script);
        let Some(s) = info(&path) else {
            return Err(format!(
                "{} isn't a Lua script (a .lua file) any more.",
                path.display()
            ));
        };
        let name = s.name;
        let code = std::fs::read_to_string(&path)
            .map_err(|e| format!("Can't read the script {}: {e}", path.display()))?;
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err("A script is already running. Stop it first.".into());
        }
        STOP.store(false, Ordering::SeqCst);
        let tick = move |p: ScriptProgress| {
            let _ = app.emit("script", p);
        };
        let r = run(
            dev,
            &dir,
            &items,
            &name,
            &code,
            &STOP,
            trial.unwrap_or(false),
            &tick,
        );
        RUNNING.store(false, Ordering::SeqCst);
        Ok(r)
    })
    .await
}

/// Stops the script running, if any.
#[tauri::command(rename_all = "snake_case")]
pub(super) fn stop_script() {
    STOP.store(true, Ordering::SeqCst);
}

/// What a script prints: the last OUTPUT_MAX lines, and how many it has printed in all.
#[derive(Default)]
struct Out {
    lines: Mutex<std::collections::VecDeque<String>>,
    printed: std::sync::atomic::AtomicU64,
}

impl Out {
    fn push(&self, line: String) {
        let mut o = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        o.push_back(line);
        if o.len() > OUTPUT_MAX {
            o.pop_front();
        }
        self.printed.fetch_add(1, Ordering::Relaxed);
    }

    /// The lines printed after the first `seen`, of those still kept.
    fn since(&self, seen: u64) -> (Vec<String>, u64) {
        let o = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let printed = self.printed.load(Ordering::Relaxed);
        let new = ((printed - seen) as usize).min(o.len());
        (o.iter().skip(o.len() - new).cloned().collect(), printed)
    }
}

/// Runs `code` with `ferry` set up for `dev`, until it ends or `stop` is set; with `trial`,
/// without changing anything. `tick` gets what it prints as it goes, every TICK.
#[allow(clippy::too_many_arguments)]
fn run(
    dev: Arc<Device>,
    dir: &str,
    items: &[String],
    name: &str,
    code: &str,
    stop: &'static AtomicBool,
    trial: bool,
    tick: &(dyn Fn(ScriptProgress) + Sync),
) -> ScriptReport {
    let output = Arc::new(Out::default());
    let changed = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let done = AtomicBool::new(false);
    let result = std::thread::scope(|s| {
        s.spawn(|| {
            let (mut seen, mut was) = (0, 0);
            while !done.load(Ordering::Relaxed) {
                // In short sleeps, so a script that ends quickly isn't kept waiting.
                let start = std::time::Instant::now();
                while start.elapsed() < TICK && !done.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let (lines, printed) = output.since(seen);
                let n = changed.load(Ordering::Relaxed);
                if printed != seen || n != was {
                    (seen, was) = (printed, n);
                    tick(ScriptProgress { lines, changed: n });
                }
            }
        });
        let r = lua_run(dev, dir, items, name, code, stop, trial, &output, &changed);
        done.store(true, Ordering::Relaxed);
        r
    });
    let output = std::mem::take(&mut *output.lines.lock().unwrap_or_else(|e| e.into_inner()));
    ScriptReport {
        output: output.into(),
        error: result.err().map(|e| plain_error(&e)),
        changed: changed.load(Ordering::Relaxed),
    }
}

#[allow(clippy::too_many_arguments)]
fn lua_run(
    dev: Arc<Device>,
    dir: &str,
    items: &[String],
    name: &str,
    code: &str,
    stop: &'static AtomicBool,
    trial: bool,
    output: &Arc<Out>,
    changed: &Changed,
) -> mlua::Result<()> {
    {
        let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        lua.set_memory_limit(MEMORY)?;
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(10_000),
            move |_, _| {
                if stop.load(Ordering::Relaxed) {
                    Err(mlua::Error::runtime("Stopped."))
                } else {
                    Ok(VmState::Continue)
                }
            },
        )?;
        let pretend = trial.then(|| {
            Arc::new(Pretend {
                dev: dev.clone(),
                dir: dir.to_string(),
                t: Mutex::default(),
                out: output.clone(),
                changed: changed.clone(),
            })
        });
        let ferry = api(&lua, dev, dir, items, changed, stop, pretend)?;
        let g = lua.globals();
        g.set("ferry", ferry)?;
        let out = output.clone();
        g.set(
            "print",
            lua.create_function(move |_, args: mlua::Variadic<Value>| {
                out.push(args.iter().map(show).collect::<Vec<_>>().join("\t"));
                Ok(())
            })?,
        )?;
        // "@": named as a file, so errors say "name.lua:3:".
        lua.load(code).set_name(format!("@{name}.lua")).exec()
    }
}

/// A change a script asks for in a trial, where it's only told.
enum Would {
    Write(String),
    Create(String),
    Append(String),
    Mkdir(String, bool),
    Copy(String, String),
    Move(String, String),
    Rename(String, String),
    Remove(String, bool),
}

/// A trial's changes, none of them made: what would be at a path (true: a folder), and what
/// would be gone.
#[derive(Default)]
struct Trial {
    made: HashMap<String, bool>,
    gone: HashSet<String>,
}

/// What a trial run tells instead of changing anything.
struct Pretend {
    dev: Arc<Device>,
    /// The pane's folder: paths in it are told from there.
    dir: String,
    t: Mutex<Trial>,
    out: Arc<Out>,
    changed: Changed,
}

/// Whether `path` is `base` or inside it.
fn within(path: &str, base: &str) -> bool {
    path == base || path.starts_with(&format!("{}/", base.trim_end_matches('/')))
}

impl Pretend {
    /// What would be at `path` after the trial's changes so far: None, or whether a folder.
    fn there(&self, t: &Trial, path: &str) -> Result<Option<bool>, String> {
        if let Some(&dir) = t.made.get(path) {
            return Ok(Some(dir));
        }
        if t.gone.iter().any(|g| within(path, g)) {
            return Ok(None);
        }
        Ok(self
            .dev
            .stat(path)
            .map_err(|e| e.to_string())?
            .map(|e| e.dir))
    }

    /// `path` as the output shows it: from the pane's folder when it's in it.
    fn shown<'a>(&self, path: &'a str) -> &'a str {
        let base = format!("{}/", self.dir.trim_end_matches('/'));
        path.strip_prefix(&base).unwrap_or(path)
    }

    fn take(t: &mut Trial, path: &str) {
        t.made.retain(|m, _| !within(m, path));
        t.gone.insert(path.to_string());
    }

    /// Checks `w` as the real change would, then tells it in the output instead of making it.
    fn act(&self, w: Would) -> mlua::Result<()> {
        let mut t = self.t.lock().unwrap_or_else(|e| e.into_inner());
        let line = self.check(&mut t, w).map_err(fail)?;
        if let Some(line) = line {
            self.out.push(line);
            self.changed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn check(&self, t: &mut Trial, w: Would) -> Result<Option<String>, String> {
        let missing = |p: &str| format!("No such file or folder: {p}");
        Ok(Some(match w {
            Would::Write(p) => {
                let was = self.there(t, &p)?;
                if was == Some(true) {
                    return Err(format!("{p} is a folder."));
                }
                t.made.insert(p.clone(), false);
                let verb = if was.is_some() { "replace" } else { "write" };
                format!("Would {verb} {}", self.shown(&p))
            }
            Would::Create(p) => {
                if self.there(t, &p)?.is_some() {
                    return Err(format!("{p} is already there. Not replaced."));
                }
                t.made.insert(p.clone(), false);
                format!("Would make {}", self.shown(&p))
            }
            Would::Append(p) => {
                if self.there(t, &p)? == Some(true) {
                    return Err(format!("{p} is a folder."));
                }
                t.made.insert(p.clone(), false);
                format!("Would add to {}", self.shown(&p))
            }
            Would::Mkdir(p, parents) => {
                match self.there(t, &p)? {
                    Some(true) if parents => return Ok(None),
                    Some(_) => return Err(format!("{p} is already there.")),
                    None => {}
                }
                let mut up = device::dirname(&p);
                if !parents && up != p && self.there(t, &up)?.is_none() {
                    return Err(format!("The folder {up} isn't there."));
                }
                while parents && up != p && self.there(t, &up)?.is_none() {
                    t.made.insert(up.clone(), true);
                    let next = device::dirname(&up);
                    if next == up {
                        break;
                    }
                    up = next;
                }
                t.made.insert(p.clone(), true);
                format!("Would make folder {}", self.shown(&p))
            }
            Would::Copy(a, b) => {
                let dir = self.there(t, &a)?.ok_or_else(|| missing(&a))?;
                match self.there(t, &b)? {
                    Some(false) if dir => {
                        return Err(format!(
                            "A file is already at {b}, where the folder would go."
                        ))
                    }
                    Some(_) if !dir => return Err(format!("{b} is already there. Not replaced.")),
                    _ => {}
                }
                t.made.insert(b.clone(), dir);
                format!("Would copy {} to {}", self.shown(&a), self.shown(&b))
            }
            Would::Move(a, b) => {
                if self.there(t, &b)?.is_some() {
                    return Err(format!("{b} is already there. Nothing was moved."));
                }
                let dir = self.there(t, &a)?.ok_or_else(|| missing(&a))?;
                Self::take(t, &a);
                t.made.insert(b.clone(), dir);
                format!("Would move {} to {}", self.shown(&a), self.shown(&b))
            }
            Would::Rename(p, to) => {
                let dir = self.there(t, &p)?.ok_or_else(|| missing(&p))?;
                if self.there(t, &to)?.is_some() {
                    return Err(format!("{to} is already there."));
                }
                Self::take(t, &p);
                t.made.insert(to.clone(), dir);
                format!(
                    "Would rename {} to {}",
                    self.shown(&p),
                    device::basename(&to)
                )
            }
            Would::Remove(p, all) => {
                let dir = self.there(t, &p)?.ok_or_else(|| missing(&p))?;
                let full = dir
                    && !t.made.contains_key(&p)
                    && !self.dev.listdir(&p).map_err(|e| e.to_string())?.is_empty();
                if full && !all {
                    return Err(format!("{p} isn't empty."));
                }
                Self::take(t, &p);
                if full {
                    format!("Would delete {} and everything in it", self.shown(&p))
                } else {
                    format!("Would delete {}", self.shown(&p))
                }
            }
        }))
    }
}

/// A value as `print` shows it.
fn show(v: &Value) -> String {
    match v {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.to_string_lossy().to_string(),
        Value::Error(e) => plain_error(e),
        other => format!("<{}>", other.type_name()),
    }
}

/// A script's error in a line or two: its message and where (no Rust or Lua internals).
fn plain_error(e: &mlua::Error) -> String {
    match e {
        mlua::Error::CallbackError { cause, .. } => plain_error(cause),
        mlua::Error::RuntimeError(m) => m.lines().next().unwrap_or_default().to_string(),
        mlua::Error::MemoryError(_) => format!(
            "The script used more than {} MB of memory.",
            MEMORY / 1024 / 1024
        ),
        other => other
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

/// An entry as a table: { name, path, dir, size, mtime }.
fn entry(lua: &Lua, e: &Entry) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", e.name.as_str())?;
    t.set("path", e.path.as_str())?;
    t.set("dir", e.dir)?;
    t.set("size", e.size)?;
    t.set("mtime", e.mtime)?;
    Ok(t)
}

/// What kind of media a file is, by its extension: "picture", "video" or "sound".
fn media_kind(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase())?;
    const PICTURES: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "avif", "tif", "tiff", "svg",
    ];
    const VIDEOS: &[&str] = &[
        "mp4", "m4v", "mov", "3gp", "mkv", "webm", "avi", "wmv", "mpg", "mpeg", "ts",
    ];
    const SOUNDS: &[&str] = &[
        "mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma",
    ];
    [("picture", PICTURES), ("video", VIDEOS), ("sound", SOUNDS)]
        .into_iter()
        .find(|(_, xs)| xs.contains(&ext.as_str()))
        .map(|(k, _)| k)
}

/// A device error as the script's error, which it can catch with pcall.
fn fail(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

/// Counts what a script changed.
type Changed = Arc<std::sync::atomic::AtomicU32>;

fn stopped(stop: &AtomicBool) -> Result<(), String> {
    if stop.load(Ordering::Relaxed) {
        Err("Stopped.".into())
    } else {
        Ok(())
    }
}

/// A file copied on its device through a file on this computer, so it needn't fit in memory.
fn copy_file(d: &Device, from: &str, to: &str, stop: &AtomicBool) -> Result<(), String> {
    static N: AtomicU32 = AtomicU32::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "ferry-script-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let r = d
        .read_to(from, &tmp, &mut |_| {}, stop)
        .and_then(|_| d.write_from(to, &tmp, None, true));
    let _ = std::fs::remove_file(&tmp);
    r.map(|_| ()).map_err(|e| e.to_string())
}

/// Copies file or folder `from` to `to` on `d`. A folder's contents go into a folder already at
/// `to`; a file already there isn't replaced (the copy stops, saying so).
fn copy_tree(
    d: &Device,
    from: &str,
    to: &str,
    stop: &AtomicBool,
    changed: &Changed,
) -> Result<(), String> {
    stopped(stop)?;
    let e = d
        .stat(from)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No such file or folder: {from}"))?;
    if e.dir && (to == from || to.starts_with(&format!("{}/", from.trim_end_matches('/')))) {
        return Err(format!("{from} can't be copied into itself."));
    }
    let there = d.stat(to).map_err(|e| e.to_string())?;
    if !e.dir {
        if there.is_some() {
            return Err(format!("{to} is already there. Not replaced."));
        }
        copy_file(d, from, to, stop)?;
        changed.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    match there {
        Some(t) if t.dir => {}
        Some(_) => {
            return Err(format!(
                "A file is already at {to}, where the folder would go."
            ))
        }
        None => {
            d.mkdir(to).map_err(|e| e.to_string())?;
            changed.fetch_add(1, Ordering::Relaxed);
        }
    }
    for c in d.listdir(from).map_err(|e| e.to_string())? {
        copy_tree(d, &c.path, &device::join(to, &c.name), stop, changed)?;
    }
    Ok(())
}

/// Deletes file or folder `path` on `d`, a folder with everything in it: each file backed up,
/// each folder deleted once it's empty.
fn remove_tree(d: &Device, path: &str, stop: &AtomicBool, changed: &Changed) -> Result<(), String> {
    stopped(stop)?;
    let e = d
        .stat(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No such file or folder: {path}"))?;
    if e.dir {
        for c in d.listdir(path).map_err(|e| e.to_string())? {
            remove_tree(d, &c.path, stop, changed)?;
        }
    }
    d.remove(path).map_err(|e| e.to_string())?;
    changed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Moves file or folder `from` to `to` on `d`, which must be free: in one step where the device
/// can (on one of the computer's disks), otherwise copied, then the original deleted with
/// backups.
fn move_tree(
    d: &Device,
    from: &str,
    to: &str,
    stop: &AtomicBool,
    changed: &Changed,
) -> Result<(), String> {
    if d.stat(to).map_err(|e| e.to_string())?.is_some() {
        return Err(format!("{to} is already there. Nothing was moved."));
    }
    if d.move_to(from, to).map_err(|e| e.to_string())? {
        changed.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    copy_tree(d, from, to, stop, changed)?;
    remove_tree(d, from, stop, changed)
}

/// Makes folder `path`, and with `parents` the folders above it that aren't there; a folder
/// that's there already is fine then.
fn make_dirs(d: &Device, path: &str, parents: bool, changed: &Changed) -> Result<(), String> {
    match d.stat(path).map_err(|e| e.to_string())? {
        Some(e) if e.dir && parents => return Ok(()),
        Some(_) => return Err(format!("{path} is already there.")),
        None => {}
    }
    let up = device::dirname(path);
    if parents && up != path && d.stat(&up).map_err(|e| e.to_string())?.is_none() {
        make_dirs(d, &up, true, changed)?;
    }
    d.mkdir(path).map_err(|e| e.to_string())?;
    changed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// The `ferry` table.
fn api(
    lua: &Lua,
    dev: Arc<Device>,
    dir: &str,
    items: &[String],
    changed: &Changed,
    stop: &'static AtomicBool,
    pretend: Option<Arc<Pretend>>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("dir", dir)?;
    t.set(
        "items",
        lua.create_sequence_from(items.iter().map(String::as_str))?,
    )?;
    t.set("device", dev.label())?;
    t.set("trial", pretend.is_some())?;
    let d = dev.clone();
    t.set(
        "list",
        func(lua, move |lua, dir: String| {
            let es = d.listdir(&dir).map_err(fail)?;
            lua.create_sequence_from(
                es.iter()
                    .map(|e| entry(lua, e))
                    .collect::<mlua::Result<Vec<_>>>()?,
            )
        })?,
    )?;
    let (d, pt) = (dev.clone(), pretend.clone());
    t.set(
        "stat",
        func(lua, move |lua, path: String| {
            if let Some(p) = &pt {
                // Gone in the trial; one it made has no entry to give.
                let t = p.t.lock().unwrap_or_else(|e| e.into_inner());
                if p.there(&t, &path).map_err(fail)?.is_none() {
                    return Ok(Value::Nil);
                }
            }
            match d.stat(&path).map_err(fail)? {
                Some(e) => Ok(Value::Table(entry(lua, &e)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    let d = dev.clone();
    t.set(
        "read",
        func(lua, move |lua, path: String| {
            let e = d
                .stat(&path)
                .map_err(fail)?
                .ok_or_else(|| fail(format!("No such file: {path}")))?;
            if e.dir {
                return Err(fail(format!("{path} is a folder.")));
            }
            if e.size > READ_MAX {
                return Err(fail(format!(
                    "{path} is bigger than {} MB, which is as much as ferry.read reads.",
                    READ_MAX / 1024 / 1024
                )));
            }
            let (data, _) = d.read(&path).map_err(fail)?;
            lua.create_string(&data)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "write",
        func(lua, move |_, (path, data): (String, mlua::LuaString)| {
            if let Some(p) = &pt {
                return p.act(Would::Write(path));
            }
            d.write(&path, &data.as_bytes(), None, false)
                .map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    let d = dev.clone();
    t.set(
        "media",
        func(lua, move |lua, path: String| {
            let name = device::basename(&path).to_string();
            let Some(kind) = media_kind(&name) else {
                return Ok(Value::Nil);
            };
            let m = lua.create_table()?;
            m.set("kind", kind)?;
            // Only the header is read; a size that can't be told from it is left out.
            if let Ok(i) = crate::preview::media::info(&name, &mut |at, len| {
                d.read_range(&path, at, len).map_err(|e| e.to_string())
            }) {
                m.set("width", i.width)?;
                m.set("height", i.height)?;
                m.set("duration", i.duration)?;
            }
            Ok(Value::Table(m))
        })?,
    )?;
    let d = dev.clone();
    let colour = func(lua, move |lua, path: String| {
        if media_kind(device::basename(&path)) != Some("picture") {
            return Ok(Value::Nil);
        }
        let e = d
            .stat(&path)
            .map_err(fail)?
            .ok_or_else(|| fail(format!("No such file: {path}")))?;
        if e.size > READ_MAX {
            return Err(fail(format!(
                "{path} is bigger than {} MB, which is as much as ferry.colour reads.",
                READ_MAX / 1024 / 1024
            )));
        }
        let (data, _) = d.read(&path).map_err(fail)?;
        // Decoding and shrinking a big photo takes a moment; the script waits, as for read.
        let Some(c) = crate::preview::colour::colour(&data) else {
            return Ok(Value::Nil);
        };
        let m = lua.create_table()?;
        m.set("share", c.share)?;
        m.set("peak", c.peak)?;
        Ok(Value::Table(m))
    })?;
    t.set("colour", colour.clone())?;
    t.set("color", colour)?;
    let (d, pt) = (dev.clone(), pretend.clone());
    t.set(
        "exists",
        func(lua, move |_, path: String| {
            if let Some(p) = &pt {
                let t = p.t.lock().unwrap_or_else(|e| e.into_inner());
                return Ok(p.there(&t, &path).map_err(fail)?.is_some());
            }
            Ok(d.stat(&path).map_err(fail)?.is_some())
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "create",
        func(
            lua,
            move |_, (path, data): (String, Option<mlua::LuaString>)| {
                if let Some(p) = &pt {
                    return p.act(Would::Create(path));
                }
                let bytes = data.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                d.write(&path, &bytes, None, true).map_err(fail)?;
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "append",
        func(lua, move |_, (path, data): (String, mlua::LuaString)| {
            if let Some(p) = &pt {
                return p.act(Would::Append(path));
            }
            let mut now = match d.stat(&path).map_err(fail)? {
                Some(e) if e.dir => return Err(fail(format!("{path} is a folder."))),
                Some(e) if e.size > READ_MAX => {
                    return Err(fail(format!(
                        "{path} is bigger than {} MB, too big to add to.",
                        READ_MAX / 1024 / 1024
                    )))
                }
                Some(_) => d.read(&path).map_err(fail)?.0,
                None => vec![],
            };
            now.extend_from_slice(&data.as_bytes());
            d.write(&path, &now, None, false).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "mkdir",
        func(lua, move |_, (path, parents): (String, Option<bool>)| {
            if let Some(p) = &pt {
                return p.act(Would::Mkdir(path, parents.unwrap_or(false)));
            }
            make_dirs(&d, &path, parents.unwrap_or(false), &c).map_err(fail)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "copy",
        func(lua, move |_, (from, to): (String, String)| {
            if let Some(p) = &pt {
                return p.act(Would::Copy(from, to));
            }
            copy_tree(&d, &from, &to, stop, &c).map_err(fail)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "move",
        func(lua, move |_, (from, to): (String, String)| {
            if let Some(p) = &pt {
                return p.act(Would::Move(from, to));
            }
            move_tree(&d, &from, &to, stop, &c).map_err(fail)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "rename",
        func(lua, move |_, (path, new_name): (String, String)| {
            if let Some(p) = &pt {
                let to = device::join(&device::dirname(&path), &new_name);
                p.act(Would::Rename(path, to.clone()))?;
                return Ok(to);
            }
            let to = d.rename(&path, &new_name).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(to)
        })?,
    )?;
    let (d, c, pt) = (dev, changed.clone(), pretend);
    t.set(
        "remove",
        func(lua, move |_, (path, all): (String, Option<bool>)| {
            if let Some(p) = &pt {
                p.act(Would::Remove(path, all.unwrap_or(false)))?;
                return Ok(None);
            }
            if all.unwrap_or(false) {
                return remove_tree(&d, &path, stop, &c).map(|_| None).map_err(fail);
            }
            let backup = d.remove(&path).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(backup)
        })?,
    )?;
    t.set(
        "join",
        func(lua, |_, (dir, name): (String, String)| {
            Ok(device::join(&dir, &name))
        })?,
    )?;
    t.set(
        "basename",
        func(lua, |_, path: String| {
            Ok(device::basename(&path).to_string())
        })?,
    )?;
    t.set(
        "dirname",
        func(lua, |_, path: String| Ok(device::dirname(&path)))?,
    )?;
    // Failures come back as (false, message), which LIFT turns into errors that are plain
    // strings, raised where the script called: printable, and joinable with `..`.
    lua.load(LIFT).set_name("ferry").call(t)
}

/// Wraps each function of the table it's given so that (true, ...) returns `...` and
/// (false, message) raises `message` at the caller's line.
const LIFT: &str = r#"
local raw = ...
local t = {}
for k, v in pairs(raw) do
  if type(v) == "function" then
    t[k] = function(...)
      local r = table.pack(v(...))
      if r[1] then return table.unpack(r, 2, r.n) end
      error(r[2], 2)
    end
  else
    t[k] = v
  end
end
return t
"#;

/// A function for `ferry`: its result as (true, value), or a failure (a runtime error, from
/// `fail`) as (false, message), for LIFT.
fn func<A: mlua::FromLuaMulti, R: mlua::IntoLuaMulti>(
    lua: &Lua,
    f: impl Fn(&Lua, A) -> mlua::Result<R> + 'static,
) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, a: A| {
        let mut out = match f(lua, a) {
            Ok(r) => r.into_lua_multi(lua)?,
            Err(mlua::Error::RuntimeError(m)) => {
                return (false, lua.create_string(&m)?).into_lua_multi(lua)
            }
            Err(e) => return Err(e),
        };
        out.push_front(Value::Boolean(true));
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEVER: AtomicBool = AtomicBool::new(false);

    /// A script run for real, its progress dropped.
    fn run(
        dev: Arc<Device>,
        dir: &str,
        items: &[String],
        name: &str,
        code: &str,
        stop: &'static AtomicBool,
    ) -> ScriptReport {
        super::run(dev, dir, items, name, code, stop, false, &|_| {})
    }

    /// A scratch folder on this computer, removed afterwards.
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> (Scratch, String) {
        let dir =
            std::env::temp_dir().join(format!("ferry-script-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = crate::platform::app_path(&dir);
        (Scratch(dir), app)
    }

    #[test]
    fn scripts_list_read_and_write_through_the_device() {
        let (_s, dir) = scratch("rw");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let a = device::join(&dir, "a.txt");
        dev.write(&a, b"hello", None, true).unwrap();
        let code = r#"
            for _, e in ipairs(ferry.list(ferry.dir)) do print(e.name, e.size) end
            local s = ferry.read(ferry.items[1])
            ferry.write(ferry.join(ferry.dir, "b.txt"), s:upper())
            ferry.mkdir(ferry.join(ferry.dir, "sub"))
            print(ferry.basename(ferry.rename(ferry.join(ferry.dir, "b.txt"), "c.txt")))
        "#;
        let r = run(dev.clone(), &dir, &[a.clone()], "t", code, &NEVER);
        assert_eq!(r.error, None);
        assert_eq!(r.output, ["a.txt\t5", "c.txt"]);
        assert_eq!(r.changed, 3);
        assert_eq!(dev.read(&device::join(&dir, "c.txt")).unwrap().0, b"HELLO");
        assert!(dev
            .stat(&device::join(&dir, "sub"))
            .unwrap()
            .is_some_and(|e| e.dir));
    }

    #[test]
    fn scripts_get_no_io_or_os_and_say_what_went_wrong() {
        let (_s, dir) = scratch("sandbox");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        for code in ["io.open('x')", "os.remove('x')", "require('x')"] {
            let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
            assert!(r.error.is_some(), "{code}");
        }
        let r = run(
            dev.clone(),
            &dir,
            &[],
            "t",
            "ferry.read(ferry.join(ferry.dir, 'none.txt'))",
            &NEVER,
        );
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.starts_with("t.lua:1: No such file") && e.contains("none.txt")),
            "{:?}",
            r.error
        );
        // Caught, the error is a string, with the script's line.
        let r = run(
            dev.clone(),
            &dir,
            &[],
            "t",
            "local ok, e = pcall(function() local s = ferry.read('x') return s end)
print('skipped: ' .. e)",
            &NEVER,
        );
        assert_eq!(r.error, None);
        assert!(
            r.output[0].starts_with("skipped: t.lua:1: "),
            "{:?}",
            r.output
        );
        // A device error can be caught.
        let r = run(
            dev,
            &dir,
            &[],
            "t",
            "print(pcall(ferry.remove, ferry.join(ferry.dir, 'none')))",
            &NEVER,
        );
        assert_eq!(r.error, None);
        assert!(r.output[0].starts_with("false"), "{:?}", r.output);
    }

    #[test]
    fn scripts_make_copy_move_and_delete_files_and_folders() {
        let (_s, dir) = scratch("tree");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let code = r#"
            local d = ferry.dir
            ferry.mkdir(ferry.join(d, "a/b/c"), true)
            ferry.create(ferry.join(d, "a/b/c/one.txt"), "1")
            ferry.create(ferry.join(d, "a/two.txt"))
            ferry.append(ferry.join(d, "a/two.txt"), "2")
            ferry.append(ferry.join(d, "a/two.txt"), "2")
            print(pcall(ferry.create, ferry.join(d, "a/two.txt"), "again"))
            ferry.copy(ferry.join(d, "a"), ferry.join(d, "copy"))
            print(pcall(ferry.copy, ferry.join(d, "a"), ferry.join(d, "copy")))
            print(pcall(ferry.copy, ferry.join(d, "a"), ferry.join(d, "a/b/in")))
            ferry.move(ferry.join(d, "copy"), ferry.join(d, "moved"))
            print(ferry.exists(ferry.join(d, "copy")), ferry.read(ferry.join(d, "moved/b/c/one.txt")), ferry.read(ferry.join(d, "moved/two.txt")))
            print(pcall(ferry.remove, ferry.join(d, "moved")))
            ferry.remove(ferry.join(d, "moved"), true)
            print(ferry.exists(ferry.join(d, "moved")))
        "#;
        let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
        assert_eq!(r.error, None, "{:?}", r.output);
        assert!(
            r.output[0].starts_with("false\t") && r.output[0].contains("already exists"),
            "{:?}",
            r.output
        );
        assert!(
            r.output[1].starts_with("false\t") && r.output[1].contains("Not replaced"),
            "{:?}",
            r.output
        );
        assert!(r.output[2].contains("into itself"), "{:?}", r.output);
        assert_eq!(r.output[3], "false\t1\t22");
        assert!(
            r.output[4].starts_with("false\t"),
            "a folder with things in it isn't deleted without `true`: {:?}",
            r.output
        );
        assert_eq!(r.output[5], "false");
        // The original is as it was.
        assert_eq!(
            dev.read(&device::join(&dir, "a/b/c/one.txt")).unwrap().0,
            b"1"
        );
    }

    /// The start of a PNG `w` × `h`: enough for its size to be read.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    #[test]
    fn the_cleanup_script_keeps_only_big_media() {
        let (_s, dir) = scratch("clean");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let put = |name: &str, data: &[u8]| {
            dev.write(&device::join(&dir, name), data, None, true)
                .unwrap()
        };
        put("big.png", &png(800, 600));
        put("wide.png", &png(1200, 300));
        put("small.png", &png(120, 90));
        put("notes.txt", b"hello");
        put("song.mp3", b"ID3");
        dev.mkdir(&device::join(&dir, "sub")).unwrap();
        put("sub/tiny.png", &png(10, 10));
        let code = include_str!("../../docs/scripts/delete non-media and small media.lua");
        let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
        assert_eq!(r.error, None, "{:?}", r.output);
        let left: Vec<String> = dev
            .listdir(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        // Folders aren't gone into unless asked, and sounds are media.
        assert_eq!(left, ["big.png", "song.mp3", "sub"], "{:?}", r.output);
        assert!(dev
            .stat(&device::join(&dir, "sub/tiny.png"))
            .unwrap()
            .is_some());
        assert_eq!(r.changed, 3);
    }

    #[test]
    fn scripts_tell_gray_pages_from_colour_ones() {
        let (_s, dir) = scratch("colour");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let put = |name: &str, img: image::RgbImage| {
            let mut b = std::io::Cursor::new(Vec::new());
            img.write_to(&mut b, image::ImageFormat::Png).unwrap();
            dev.write(&device::join(&dir, name), b.get_ref(), None, true)
                .unwrap()
        };
        let gray = image::RgbImage::from_fn(200, 200, |x, _| {
            image::Rgb(if x % 10 < 2 { [0; 3] } else { [240; 3] })
        });
        let mut corner = gray.clone();
        for (x, y, p) in corner.enumerate_pixels_mut() {
            if x < 50 && y < 50 {
                *p = image::Rgb([200, 30, 30]);
            }
        }
        put("gray.png", gray);
        put("corner.png", corner);
        put(
            "photo.png",
            image::RgbImage::from_pixel(80, 60, image::Rgb([40, 120, 220])),
        );
        dev.write(&device::join(&dir, "notes.txt"), b"hi", None, true)
            .unwrap();
        let code = r#"
            for _, n in ipairs({"gray.png", "corner.png", "photo.png", "notes.txt"}) do
                local c = ferry.color(ferry.join(ferry.dir, n))
                if c then print(n, c.share > 0.5, c.peak > 0.5) else print(n, "nil") end
            end
        "#;
        let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
        assert_eq!(r.error, None, "{:?}", r.output);
        assert_eq!(
            r.output,
            [
                "gray.png\tfalse\tfalse",
                "corner.png\tfalse\ttrue",
                "photo.png\ttrue\ttrue",
                "notes.txt\tnil"
            ]
        );
        assert_eq!(r.changed, 0);
    }

    #[test]
    fn the_grayscale_script_moves_gray_pictures_three_levels_down() {
        let (s, dir) = scratch("gray");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let put = |name: &str, img: image::RgbImage| {
            let mut b = std::io::Cursor::new(Vec::new());
            img.write_to(&mut b, image::ImageFormat::Png).unwrap();
            let path = device::join(&dir, name);
            std::fs::create_dir_all(s.0.join(name).parent().unwrap()).unwrap();
            dev.write(&path, b.get_ref(), None, true).unwrap()
        };
        let gray = image::RgbImage::from_fn(120, 120, |x, _| {
            image::Rgb(if x % 10 < 2 { [0; 3] } else { [240; 3] })
        });
        let mut corner = gray.clone();
        for (x, y, p) in corner.enumerate_pixels_mut() {
            if x < 30 && y < 30 {
                *p = image::Rgb([200, 30, 30]);
            }
        }
        put("a.png", gray.clone());
        put("b.png", corner);
        put(
            "c.png",
            image::RgbImage::from_pixel(40, 40, image::Rgb([40, 120, 220])),
        );
        put("x/a.png", gray.clone());
        put("x/y/z/deep.png", gray.clone());
        put("x/y/z/w/too deep.png", gray);
        dev.write(&device::join(&dir, "notes.txt"), b"hi", None, true)
            .unwrap();
        let code = include_str!("../../docs/scripts/move grayscale pictures.lua");
        // Tried first: it says what it would move, and moves nothing.
        let r = super::run(dev.clone(), &dir, &[], "t", code, &NEVER, true, &|_| {});
        assert_eq!(r.error, None, "{:?}", r.output);
        assert!(r
            .output
            .contains(&"Would move x/a.png to Grayscale/x/a.png".to_string()));
        assert_eq!(
            r.output.last().unwrap(),
            "Would move 3 grayscale pictures to Grayscale."
        );
        assert!(dev
            .stat(&device::join(&dir, "Grayscale"))
            .unwrap()
            .is_none());
        let r = run(dev.clone(), &dir, &[], "t", code, &NEVER);
        assert_eq!(r.error, None, "{:?}", r.output);
        let is = |p: &str| dev.stat(&device::join(&dir, p)).unwrap().is_some();
        for p in [
            "Grayscale/a.png",
            "Grayscale/x/a.png",
            "Grayscale/x/y/z/deep.png",
        ] {
            assert!(is(p), "{p} {:?}", r.output);
        }
        for p in ["b.png", "c.png", "notes.txt", "x/y/z/w/too deep.png"] {
            assert!(is(p), "{p} {:?}", r.output);
        }
        assert!(!is("a.png") && !is("x/a.png"));
        assert_eq!(
            r.output.last().unwrap(),
            "Moved 3 grayscale pictures to Grayscale."
        );
    }

    #[test]
    fn a_trial_tells_its_changes_and_makes_none() {
        let (_s, dir) = scratch("trial");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let a = device::join(&dir, "a.txt");
        dev.write(&a, b"hello", None, true).unwrap();
        let code = r#"
            ferry.write(ferry.join(ferry.dir, "b.txt"), "new")
            ferry.mkdir(ferry.join(ferry.dir, "sub/deeper"), true)
            print(ferry.exists(ferry.join(ferry.dir, "sub")))
            ferry.move(ferry.items[1], ferry.join(ferry.dir, "sub/a.txt"))
            print(ferry.exists(ferry.items[1]), ferry.stat(ferry.items[1]))
            print(pcall(ferry.create, ferry.join(ferry.dir, "sub/a.txt")))
            ferry.remove(ferry.join(ferry.dir, "sub"), true)
        "#;
        let r = super::run(
            dev.clone(),
            &dir,
            &[a.clone()],
            "t",
            code,
            &NEVER,
            true,
            &|_| {},
        );
        assert_eq!(r.error, None, "{:?}", r.output);
        let sub_a = device::join(&dir, "sub/a.txt");
        assert_eq!(
            r.output,
            [
                "Would write b.txt".to_string(),
                "Would make folder sub/deeper".into(),
                "true".into(),
                "Would move a.txt to sub/a.txt".into(),
                "false\tnil".into(),
                format!("false\t{sub_a} is already there. Not replaced."),
                "Would delete sub".into(),
            ]
        );
        assert_eq!(r.changed, 4);
        let names: Vec<String> = dev
            .listdir(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.txt"]);
        assert_eq!(dev.read(&a).unwrap().0, b"hello");
    }

    #[test]
    fn what_a_script_prints_comes_while_it_runs() {
        let (s, dir) = scratch("live");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        let flag = s.0.join("seen");
        let got = Mutex::new(Vec::<ScriptProgress>::new());
        let tick = |p: ScriptProgress| {
            if p.lines.iter().any(|l| l == "first") {
                let _ = std::fs::write(&flag, b"");
            }
            got.lock().unwrap().push(p);
        };
        // Waits (for a while at most) until the first line has been sent.
        let code = r#"
            print("first")
            local seen = ferry.join(ferry.dir, "seen")
            for _ = 1, 1000000 do
              if ferry.exists(seen) then break end
            end
            print("second")
        "#;
        let r = super::run(dev, &dir, &[], "t", code, &NEVER, false, &tick);
        assert_eq!(r.error, None);
        assert_eq!(r.output, ["first", "second"]);
        let got = got.into_inner().unwrap();
        assert_eq!(
            got.first().map(|p| p.lines.clone()),
            Some(vec!["first".to_string()])
        );
    }

    #[test]
    fn new_lines_are_told_once_even_past_the_last_kept() {
        let out = Out::default();
        out.push("a".into());
        let (lines, seen) = out.since(0);
        assert_eq!((lines, seen), (vec!["a".to_string()], 1));
        for i in 0..OUTPUT_MAX + 5 {
            out.push(i.to_string());
        }
        let (lines, seen) = out.since(seen);
        assert_eq!(lines.len(), OUTPUT_MAX);
        assert_eq!(lines.last().map(String::as_str), Some("2004"));
        assert_eq!(out.since(seen).0, Vec::<String>::new());
    }

    #[test]
    fn stop_ends_a_script() {
        let (_s, dir) = scratch("stop");
        let dev = Arc::new(Device::new(Box::new(crate::local::Local)));
        static STOPPED: AtomicBool = AtomicBool::new(true);
        let r = run(dev, &dir, &[], "t", "while true do end", &STOPPED);
        assert_eq!(
            r.error.as_deref().map(|e| e.contains("Stopped.")),
            Some(true),
            "{:?}",
            r.error
        );
    }

    #[test]
    fn about_is_the_first_comment() {
        assert_eq!(about("-- Lists sizes.\nprint(1)"), "Lists sizes.");
        assert_eq!(about("print(1)"), "");
    }
}
