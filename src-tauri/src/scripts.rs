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

mod api;
mod config;
mod files;
#[cfg(test)]
mod tests;
mod trial;

use api::*;
use files::*;
use trial::*;

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
