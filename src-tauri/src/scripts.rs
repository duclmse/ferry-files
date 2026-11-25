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

/// Runs the script at `script` (an app path of a .lua file on this computer) on `items` (paths on
/// `src`) in folder `dir`.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn run_script(
    state: State<'_, AppState>,
    src: String,
    dir: String,
    items: Vec<String>,
    script: String,
) -> Result<ScriptReport, String> {
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
        let r = run(dev, &dir, &items, &name, &code, &STOP);
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

/// Runs `code` with `ferry` set up for `dev`, until it ends or `stop` is set.
fn run(
    dev: Arc<Device>,
    dir: &str,
    items: &[String],
    name: &str,
    code: &str,
    stop: &'static AtomicBool,
) -> ScriptReport {
    let output = Arc::new(Mutex::new(Vec::<String>::new()));
    let changed = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let result = (|| -> mlua::Result<()> {
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
        let ferry = api(&lua, dev, dir, items, &changed, stop)?;
        let g = lua.globals();
        g.set("ferry", ferry)?;
        let out = output.clone();
        g.set(
            "print",
            lua.create_function(move |_, args: mlua::Variadic<Value>| {
                let line = args.iter().map(show).collect::<Vec<_>>().join("\t");
                let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
                o.push(line);
                if o.len() > OUTPUT_MAX {
                    o.remove(0);
                }
                Ok(())
            })?,
        )?;
        // "@": named as a file, so errors say "name.lua:3:".
        lua.load(code).set_name(format!("@{name}.lua")).exec()
    })();
    let output = std::mem::take(&mut *output.lock().unwrap_or_else(|e| e.into_inner()));
    ScriptReport {
        output,
        error: result.err().map(|e| plain_error(&e)),
        changed: changed.load(Ordering::Relaxed),
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
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("dir", dir)?;
    t.set(
        "items",
        lua.create_sequence_from(items.iter().map(String::as_str))?,
    )?;
    t.set("device", dev.label())?;
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
    let d = dev.clone();
    t.set(
        "stat",
        func(lua, move |lua, path: String| {
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
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "write",
        func(lua, move |_, (path, data): (String, mlua::LuaString)| {
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
    t.set(
        "exists",
        func(lua, move |_, path: String| {
            Ok(d.stat(&path).map_err(fail)?.is_some())
        })?,
    )?;
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "create",
        func(
            lua,
            move |_, (path, data): (String, Option<mlua::LuaString>)| {
                let bytes = data.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                d.write(&path, &bytes, None, true).map_err(fail)?;
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )?,
    )?;
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "append",
        func(lua, move |_, (path, data): (String, mlua::LuaString)| {
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
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "mkdir",
        func(lua, move |_, (path, parents): (String, Option<bool>)| {
            make_dirs(&d, &path, parents.unwrap_or(false), &c).map_err(fail)
        })?,
    )?;
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "copy",
        func(lua, move |_, (from, to): (String, String)| {
            copy_tree(&d, &from, &to, stop, &c).map_err(fail)
        })?,
    )?;
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "move",
        func(lua, move |_, (from, to): (String, String)| {
            move_tree(&d, &from, &to, stop, &c).map_err(fail)
        })?,
    )?;
    let (d, c) = (dev.clone(), changed.clone());
    t.set(
        "rename",
        func(lua, move |_, (path, new_name): (String, String)| {
            let to = d.rename(&path, &new_name).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(to)
        })?,
    )?;
    let (d, c) = (dev, changed.clone());
    t.set(
        "remove",
        func(lua, move |_, (path, all): (String, Option<bool>)| {
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
