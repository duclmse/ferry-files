//! fsd as a service, from Settings > Search: finding the fsd program, the settings Ferry gives
//! it, and fsd's own `install-service` and `uninstall-service`, which register it with launchd
//! (a LaunchAgent), systemd (a user unit) or Windows' service manager (for the whole PC).
//!
//! Ferry only changes a service it installed, one started with its own config file. An fsd the
//! user set up themselves is left as it is, and searched when it's running.

use super::*;
use crate::platform::{app_path, fsd_dir};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
pub(super) struct FsdStatus {
    /// The fsd program found, an app path.
    exe: Option<String>,
    /// Where fsd is registered as a service, when it is.
    service: Option<String>,
    /// Whether that service is Ferry's: started with `config`.
    ours: bool,
    /// Ferry's settings for fsd, an app path.
    config: String,
    /// "off" (not answering), "indexing", "ready", or "unknown" in a build without fsd searches.
    state: &'static str,
    /// The folders fsd indexes (app paths), and how many files and folders it has.
    roots: Vec<String>,
    entries: u64,
    /// Whether fsd can look inside the folders the system keeps it out of (`fsd::Index`).
    sees_protected: Option<bool>,
    /// Why fsd's answer couldn't be read.
    problem: Option<String>,
}

/// What fsd is called on this system.
fn exe_name() -> String {
    format!("fsd{}", std::env::consts::EXE_SUFFIX)
}

/// What the fsd the app carries is called. The bundle step builds it into src-tauri/libs/fsd
/// (`beforeBundleCommand` in tauri.<os>.conf.json) and puts it next to the app as ferry-fsd on
/// macOS (Contents/MacOS) and Linux (/usr/bin), and as ferry-fsd\fsd.exe on Windows.
fn bundled_name() -> String {
    format!("ferry-fsd{}", std::env::consts::EXE_SUFFIX)
}

/// The fsd the app carries first, then one installed on the computer. Apps started from the
/// Finder (or a desktop menu) don't get the shell's PATH, so look where cargo and package
/// managers put programs too.
fn fsd_exe() -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let here = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let mut found = Vec::new();
    if let Some(d) = &here {
        found.push(d.join(bundled_name()));
        found.push(d.join("ferry-fsd").join(exe_name()));
    }
    // Where `lasting_exe` copies it out of an AppImage.
    found.push(fsd_dir().join(bundled_name()));
    let mut dirs: Vec<PathBuf> = Vec::new();
    // target/<profile> when run from the workspace: `cargo tauri dev` builds fsd there first.
    // On macOS it runs the app from target/<profile>/Ferry Files.app/Contents/MacOS, so look
    // beside that .app too (in a development build only: an installed app's folder is
    // Applications).
    let dev = here
        .as_deref()
        .filter(|d| cfg!(debug_assertions) && d.ends_with("Contents/MacOS"))
        .and_then(|d| d.ancestors().nth(3))
        .map(Path::to_path_buf);
    dirs.extend(here);
    dirs.extend(dev);
    dirs.push(home.join(".cargo").join("bin"));
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    } else if !cfg!(windows) {
        dirs.push(home.join(".local").join("bin"));
        dirs.extend(["/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    let exe = exe_name();
    found.extend(dirs.into_iter().map(|d| d.join(&exe)));
    found.into_iter().find(|p| p.is_file())
}

fn config_path() -> PathBuf {
    fsd_dir().join("config.toml")
}

/// fsd as the service manager has it: the text of its registration, and where that is.
struct Registered {
    text: String,
    place: String,
    /// For this user only (a LaunchAgent or a user unit), the kind Ferry installs. On Windows
    /// every service is the whole PC's, and Ferry installs that kind.
    user: bool,
}

/// Where fsd's `install-service` puts its registration: for this user first, then for the
/// whole computer (`--system`).
#[cfg(not(windows))]
fn service_files() -> Vec<(PathBuf, bool)> {
    let home = dirs::home_dir().unwrap_or_default();
    if cfg!(target_os = "macos") {
        vec![
            (home.join("Library/LaunchAgents/fs.fsd.plist"), true),
            ("/Library/LaunchDaemons/fs.fsd.plist".into(), false),
        ]
    } else {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        vec![
            (config.join("systemd/user/fsd.service"), true),
            ("/etc/systemd/system/fsd.service".into(), false),
        ]
    }
}

#[cfg(not(windows))]
fn registered() -> Option<Registered> {
    service_files().into_iter().find_map(|(file, user)| {
        let text = std::fs::read_to_string(&file).ok()?;
        Some(Registered {
            text,
            place: app_path(&file),
            user,
        })
    })
}

#[cfg(windows)]
fn registered() -> Option<Registered> {
    let out = crate::platform::command("sc.exe")
        .args(["qc", "FsDaemon"])
        .output()
        .ok()?;
    out.status.success().then(|| Registered {
        text: String::from_utf8_lossy(&out.stdout).into_owned(),
        place: "the FsDaemon service".into(),
        user: true,
    })
}

/// Whether the service is Ferry's: started with Ferry's config file, in the way Ferry installs.
fn ours(r: &Registered) -> bool {
    let config = config_path();
    // fsd registers the config's full path, so compare with that.
    let config = config.canonicalize().unwrap_or(config);
    let config = config.to_string_lossy();
    let config = config.strip_prefix(r"\\?\").unwrap_or(&config);
    let found = if cfg!(windows) {
        r.text.to_lowercase().contains(&config.to_lowercase())
    } else {
        r.text.contains(config)
    };
    r.user && found
}

/// A TOML string: in quotes, with what TOML escapes escaped.
fn toml_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What fsd leaves out unless told otherwise: version control, the system's caches and the
/// Trash, and what builds and package managers of common languages make, which is many files
/// nobody looks for by name.
const USUAL_SKIP: &[&str] = &[
    "**/.git/**",
    "**/.hg/**",
    "**/.svn/**",
    "**/.DS_Store",
    "**/.Trash/**",
    "**/Library/Caches/**",
    "**/AppData/Local/Temp/**",
    // Rust, Maven, sbt
    "**/target/**",
    // JavaScript and TypeScript
    "**/node_modules/**",
    "**/.next/**",
    "**/.nuxt/**",
    "**/.svelte-kit/**",
    "**/.parcel-cache/**",
    "**/.turbo/**",
    // Python
    "**/__pycache__/**",
    "**/.venv/**",
    "**/.tox/**",
    "**/.mypy_cache/**",
    "**/.pytest_cache/**",
    "**/.ruff_cache/**",
    // Gradle, Android, Flutter, CMake and others
    "**/build/**",
    "**/.gradle/**",
    "**/.dart_tool/**",
    "**/CMakeFiles/**",
    // .NET
    "**/obj/**",
    // Swift and Xcode
    "**/.build/**",
    "**/DerivedData/**",
    // Elixir, Haskell, Zig
    "**/_build/**",
    "**/.stack-work/**",
    "**/dist-newstyle/**",
    "**/.zig-cache/**",
    "**/zig-out/**",
    // Terraform
    "**/.terraform/**",
];

/// Ferry's settings for fsd: `roots` (`usual_roots`), by name only (no file contents until Settings >
/// Search turns on reading text, nothing inside zips), over the local socket or pipe alone, without the web and gRPC APIs.
fn config_text(roots: &[PathBuf], dir: &Path) -> String {
    let path = |p: &Path| toml_str(&p.to_string_lossy());
    format!(
        "# fsd's settings, written by Ferry Files for its fsd service (Settings > Search).\n\
         # After changing them, install the service again there to restart fsd with them.\n\
         \n\
         watch_paths = [{roots}]\n\
         data_dir = {data}\n\
         log_dir = {logs}\n\
         \n\
         [exclusions]\n\
         path_patterns = [\n{skip}]\n\
         \n\
         # Ferry Files searches by name, so fsd doesn't look inside zips.\n\
         [archive_index]\n\
         enabled = false\n\
         \n\
         # Ferry Files asks over the local socket (a named pipe on Windows) only.\n\
         [api]\n\
         rest_enabled = false\n\
         grpc_enabled = false\n",
        roots = roots.iter().map(|r| path(r)).collect::<Vec<_>>().join(", "),
        skip = USUAL_SKIP
            .iter()
            .map(|p| format!("    {},\n", toml_str(p)))
            .collect::<String>(),
        data = path(&dir.join("index")),
        logs = path(&dir.join("logs")),
    )
}

/// What fsd indexes until the user picks: on Windows every drive inside the PC (the service
/// runs for the whole PC), elsewhere the home folder.
fn usual_roots() -> Result<Vec<PathBuf>, String> {
    let drives = crate::platform::fixed_drives();
    if !drives.is_empty() {
        return Ok(drives);
    }
    let home = dirs::home_dir().ok_or("Can't tell where your home folder is.")?;
    Ok(vec![home])
}

/// Writes Ferry's settings for fsd, unless they're there already: the user may have changed
/// them.
fn write_config(config: &Path) -> Result<(), String> {
    if config.exists() {
        return Ok(());
    }
    let roots = usual_roots()?;
    let dir = fsd_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    std::fs::write(config, config_text(&roots, &dir))
        .map_err(|e| format!("Can't write {}: {e}", config.display()))
}

/// The last thing fsd said, without the name of its command in front.
fn plain(said: String) -> String {
    let last = said.lines().last().unwrap_or_default().trim();
    last.split_once(": ")
        .filter(|(cmd, _)| cmd.ends_with("-service"))
        .map_or(last, |(_, rest)| rest)
        .to_string()
}

/// The settings of fsd's that Settings > Search changes. The rest of the file stays as it is.
#[derive(Serialize, serde::Deserialize, Debug, PartialEq)]
pub(super) struct FsdSettings {
    /// The folders it indexes (`watch_paths`), app paths.
    folders: Vec<String>,
    /// Patterns of paths it leaves out (`exclusions.path_patterns`).
    skip: Vec<String>,
    /// Whether it leaves out hidden files and folders (`exclusions.exclude_hidden`).
    hidden: bool,
    /// Whether it reads the text of text files, for finding the words in them
    /// (`content.extension_limits` not empty).
    #[serde(default)]
    text: bool,
}

/// The files whose text fsd reads when told to (Find the words inside text files), and how much
/// of each: text and code people write, by extension. Others aren't read, so a home folder of
/// photos and builds costs little.
const TEXT_LIMITS: &[(&str, i64)] = &[
    ("txt", 131072),
    ("md", 131072),
    ("markdown", 131072),
    ("org", 131072),
    ("rst", 131072),
    ("tex", 131072),
    ("csv", 65536),
    ("log", 65536),
    ("rs", 524288),
    ("py", 524288),
    ("go", 524288),
    ("swift", 524288),
    ("c", 524288),
    ("h", 524288),
    ("cpp", 524288),
    ("hpp", 524288),
    ("java", 524288),
    ("kt", 524288),
    ("cs", 524288),
    ("rb", 262144),
    ("php", 262144),
    ("lua", 262144),
    ("sh", 65536),
    ("js", 262144),
    ("ts", 262144),
    ("tsx", 262144),
    ("jsx", 262144),
    ("css", 131072),
    ("html", 131072),
    ("xml", 65536),
    ("json", 65536),
    ("toml", 65536),
    ("yaml", 65536),
    ("yml", 65536),
    ("sql", 131072),
];

#[derive(Serialize)]
pub(super) struct FsdShown {
    settings: FsdSettings,
    /// The patterns fsd's settings start with, for putting back ones taken out.
    usual: Vec<&'static str>,
}

#[derive(Serialize)]
pub(super) struct FsdSaved {
    settings: FsdSettings,
    /// What happened to them: whether fsd follows them now.
    note: String,
}

/// Ferry's settings file for fsd as it is, or as Install would write it.
fn config_now() -> Result<String, String> {
    let config = config_path();
    match std::fs::read_to_string(&config) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(config_text(&usual_roots()?, &fsd_dir()))
        }
        Err(e) => Err(format!("Can't read {}: {e}", config.display())),
    }
}

fn parse(text: &str) -> Result<toml_edit::DocumentMut, String> {
    text.parse()
        .map_err(|e| format!("fsd's settings file has a mistake: {e}"))
}

fn settings_in(text: &str) -> Result<FsdSettings, String> {
    use toml_edit::Item;
    let doc = parse(text)?;
    let strings = |item: Option<&Item>| -> Vec<String> {
        item.and_then(Item::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let ex = doc.get("exclusions");
    Ok(FsdSettings {
        folders: strings(doc.get("watch_paths"))
            .iter()
            .map(|p| app_path(Path::new(p)))
            .collect(),
        skip: strings(ex.and_then(|e| e.get("path_patterns"))),
        hidden: ex
            .and_then(|e| e.get("exclude_hidden"))
            .and_then(Item::as_bool)
            .unwrap_or(false),
        text: doc
            .get("content")
            .and_then(|c| c.get("extension_limits"))
            .and_then(Item::as_table_like)
            .is_some_and(|t| t.iter().any(|(_, v)| v.as_integer().is_some_and(|n| n > 0))),
    })
}

/// A TOML list with one item a line, as people write them.
fn listed(items: &[String]) -> toml_edit::Array {
    let mut a: toml_edit::Array = items.iter().map(String::as_str).collect();
    if !a.is_empty() {
        a.iter_mut()
            .for_each(|v| v.decor_mut().set_prefix("\n    "));
        a.set_trailing_comma(true);
        a.set_trailing("\n");
    }
    a
}

/// Checks `s` and gives it tidied: folders as the computer's paths, no blank or repeated lines.
fn tidy(s: &FsdSettings) -> Result<(Vec<String>, Vec<String>), String> {
    let mut folders: Vec<String> = Vec::new();
    for f in &s.folders {
        let native = crate::platform::native(f);
        if !native.is_absolute() {
            return Err(format!(
                "{f} isn't a full path. Pick the folder with Add a folder."
            ));
        }
        let native = native.to_string_lossy().into_owned();
        if !folders.contains(&native) {
            folders.push(native);
        }
    }
    if folders.is_empty() {
        return Err("Add a folder for fsd to index.".into());
    }
    let mut skip: Vec<String> = Vec::new();
    for p in s.skip.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if !skip.iter().any(|q| q == p) {
            skip.push(p.to_string());
        }
    }
    Ok((folders, skip))
}

/// `text` with `s` in it, keeping the rest of the file, its comments and layout.
fn with_settings(text: &str, s: &FsdSettings) -> Result<String, String> {
    use toml_edit::value;
    let (folders, skip) = tidy(s)?;
    let mut doc = parse(text)?;
    doc["watch_paths"] = value(listed(&folders));
    let ex = &mut doc["exclusions"];
    if ex.is_none() {
        *ex = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if !ex.is_table_like() {
        return Err("fsd's settings file has exclusions as something other than a table. Fix it in the file.".into());
    }
    ex["path_patterns"] = value(listed(&skip));
    ex["exclude_hidden"] = value(s.hidden);
    // The text setting: the usual limits when turned on (a table of the user's own stays), none
    // when turned off.
    let was = settings_in(&doc.to_string())?.text;
    if s.text != was {
        let content = &mut doc["content"];
        if content.is_none() {
            *content = toml_edit::Item::Table(toml_edit::Table::new());
        }
        if !content.is_table_like() {
            return Err("fsd's settings file has content as something other than a table. Fix it in the file.".into());
        }
        let mut limits = toml_edit::Table::new();
        if s.text {
            for (ext, n) in TEXT_LIMITS {
                limits[*ext] = value(*n);
            }
        }
        content["extension_limits"] = toml_edit::Item::Table(limits);
    }
    Ok(doc.to_string())
}

/// What fsd says is wrong with the settings in `file`, when it finds something.
fn fsd_check(exe: &Path, file: &Path) -> Result<(), String> {
    let out = crate::platform::command(exe)
        .arg("--config")
        .arg(file)
        .arg("validate-config")
        .output()
        .map_err(|e| format!("Can't run fsd to check the settings: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "fsd won't take these settings: {}",
        objection(&String::from_utf8_lossy(&out.stderr))
    ))
}

/// What `validate-config` said is wrong, in a line. A file it can't read gets the place first,
/// then a picture of the line, then the reason; a file it reads gets an ERROR line for each
/// mistake.
fn objection(said: &str) -> String {
    if let Some((_, rest)) = said.split_once("Failed to load config: ") {
        let mut lines = rest.lines().map(str::trim).filter(|l| !l.is_empty());
        let place = lines.next().unwrap_or_default();
        let place = place.strip_prefix("Config error: ").unwrap_or(place);
        return match lines.filter(|l| !l.starts_with(['|', '^'])).last() {
            Some(why) if !why.contains('|') => format!("{why} ({place})"),
            _ => place.to_string(),
        };
    }
    let errors: Vec<&str> = said
        .lines()
        .filter_map(|l| l.strip_prefix("ERROR: "))
        .collect();
    if errors.is_empty() {
        said.trim().to_string()
    } else {
        errors.join("; ")
    }
}

/// Writes the settings file, after fsd has checked it when it can.
fn store(text: &str) -> Result<(), String> {
    let dir = fsd_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    let config = config_path();
    let new = dir.join("config.toml.new");
    std::fs::write(&new, text).map_err(|e| format!("Can't write {}: {e}", new.display()))?;
    let checked = match fsd_exe() {
        Some(exe) => fsd_check(&exe, &new),
        None => Ok(()),
    };
    if let Err(e) = checked {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    std::fs::rename(&new, &config).map_err(|e| {
        let _ = std::fs::remove_file(&new);
        format!("Can't write {}: {e}", config.display())
    })
}

/// Has a running fsd of Ferry's follow the file, and says what came of it.
fn apply() -> String {
    let ours = registered().is_some_and(|r| ours(&r));
    if !ours {
        return "Saved. fsd starts with them when you install it.".into();
    }
    #[cfg(feature = "fsd")]
    return match crate::fsd::reload() {
        Ok(None) => "Saved. fsd starts with them next time it runs.".into(),
        Ok(Some(later)) if later.is_empty() => "Saved. fsd follows them now.".into(),
        Ok(Some(later)) => format!("Saved. Install fsd again to apply {}.", later.join(", ")),
        Err(e) => format!("Saved, but fsd couldn't apply them ({e}). Install it again."),
    };
    #[cfg(not(feature = "fsd"))]
    "Saved. Install fsd again to restart it with them.".into()
}

/// fsd's settings from Ferry's file for it, or the ones Install would write.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn fsd_settings() -> Result<FsdShown, String> {
    blocking(|| {
        Ok(FsdShown {
            settings: settings_in(&config_now()?)?,
            usual: USUAL_SKIP.to_vec(),
        })
    })
    .await
}

/// Changes fsd's folders and what it leaves out in Ferry's file for it, and has Ferry's fsd
/// service follow them.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn fsd_set_settings(settings: FsdSettings) -> Result<FsdSaved, String> {
    blocking(move || {
        if let Some(r) = registered().filter(|r| !ours(r)) {
            return Err(format!(
                "fsd runs as a service with its own settings ({}). Change them there.",
                r.place
            ));
        }
        let text = with_settings(&config_now()?, &settings)?;
        store(&text)?;
        Ok(FsdSaved {
            settings: settings_in(&text)?,
            note: apply(),
        })
    })
    .await
}

fn status() -> FsdStatus {
    let reg = registered();
    #[cfg_attr(not(feature = "fsd"), allow(unused_mut))]
    let mut s = FsdStatus {
        exe: fsd_exe().map(|e| app_path(&e)),
        service: reg.as_ref().map(|r| r.place.clone()),
        ours: reg.as_ref().is_some_and(ours),
        config: app_path(&config_path()),
        state: "unknown",
        roots: vec![],
        entries: 0,
        sees_protected: None,
        problem: None,
    };
    #[cfg(feature = "fsd")]
    match crate::fsd::index() {
        Ok(None) => s.state = "off",
        Ok(Some(i)) => {
            s.state = if i.crawled { "ready" } else { "indexing" };
            (s.roots, s.entries) = (i.roots, i.entries);
            s.sees_protected = i.sees_protected;
        }
        Err(e) => {
            s.state = "off";
            s.problem = Some(e);
        }
    }
    s
}

/// Whether fsd is there, set up as a service, and how far its index is.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn fsd_status() -> Result<FsdStatus, String> {
    blocking(|| Ok(status())).await
}

/// Installs fsd as a service with Ferry's settings and starts it: for this user while they're
/// signed in, or on Windows for the whole PC from startup. Installing again restarts it.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn fsd_install() -> Result<FsdStatus, String> {
    blocking(|| {
        let exe = fsd_exe().ok_or_else(|| {
            format!(
                "Can't find {} on the {}. Put it where it can be found, then try again.",
                exe_name(),
                crate::platform::COMPUTER
            )
        })?;
        if let Some(r) = registered().filter(|r| !ours(r)) {
            return Err(format!(
                "fsd already runs as a service with its own settings ({}). Ferry Files leaves it alone and searches with it.",
                r.place
            ));
        }
        let exe = crate::platform::lasting_exe(&exe)?;
        let config = config_path();
        write_config(&config)?;
        let args = [
            "--config".to_string(),
            config.to_string_lossy().into_owned(),
            "install-service".to_string(),
        ];
        crate::platform::run_for_service(&exe, &args)
            .map_err(|e| format!("Couldn't install fsd: {}", plain(e)))?;
        Ok(status())
    })
    .await
}

/// Stops Ferry's fsd service and removes it. Its settings and index stay in their folder.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn fsd_uninstall() -> Result<FsdStatus, String> {
    blocking(|| {
        let Some(r) = registered() else {
            return Ok(status());
        };
        if !ours(&r) {
            return Err(format!(
                "This fsd service ({}) wasn't installed by Ferry Files, so it's left as it is. Remove it with: fsd uninstall-service",
                r.place
            ));
        }
        let exe = fsd_exe().ok_or_else(|| {
            format!(
                "Can't find {}, which removes its service. Put it back, then try again.",
                exe_name()
            )
        })?;
        crate::platform::run_for_service(&exe, &["uninstall-service".to_string()])
            .map_err(|e| format!("Couldn't remove the fsd service: {}", plain(e)))?;
        Ok(status())
    })
    .await
}

/// Where to let Ferry's fsd into Desktop, Documents and Downloads, on macOS: System Settings at
/// Full Disk Access, and the program the service runs, shown in Finder to drag into its list.
/// fsd stays out of those folders until then instead of having macOS ask.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn fsd_allow() -> Result<(), String> {
    blocking(|| {
        crate::platform::open_full_disk_access()?;
        let exe = registered()
            .filter(ours)
            .and_then(|r| service_program(&r.text))
            .or_else(fsd_exe);
        if let Some(exe) = exe {
            tauri_plugin_opener::reveal_item_in_dir(&exe)
                .map_err(|e| format!("Can't show {} in Finder: {e}", exe.display()))?;
        }
        Ok(())
    })
    .await
}

/// The program a launchd service runs: the first of its ProgramArguments.
fn service_program(plist: &str) -> Option<PathBuf> {
    let args = &plist[plist.find("<key>ProgramArguments</key>")?..];
    let start = args.find("<string>")? + "<string>".len();
    let end = start + args[start..].find("</string>")?;
    let exe = args[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    Some(PathBuf::from(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launchd_service_s_program_is_its_first_argument() {
        let plist = "<dict>\n\t<key>Label</key><string>fs.fsd</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/Applications/Ferry &amp; Co.app/Contents/MacOS/ferry-fsd</string>\n\t\t<string>--config</string>\n\t</array>\n</dict>";
        assert_eq!(
            service_program(plist),
            Some(PathBuf::from(
                "/Applications/Ferry & Co.app/Contents/MacOS/ferry-fsd"
            ))
        );
        assert_eq!(service_program("<dict></dict>"), None);
    }

    #[test]
    fn toml_strings_are_escaped() {
        assert_eq!(toml_str("/Users/me"), "\"/Users/me\"");
        assert_eq!(toml_str(r"C:\Users\a"), r#""C:\\Users\\a""#);
        assert_eq!(toml_str("a\"b\tc"), "\"a\\\"b\\u0009c\"");
    }

    #[test]
    fn config_has_the_home_folder_and_no_web_api() {
        let text = config_text(
            &["/Users/me".into()],
            Path::new("/Users/me/Library/Application Support/ferry-files/fsd"),
        );
        let parsed: toml::Value = toml::from_str(&text).expect("valid TOML");
        assert_eq!(parsed["watch_paths"][0].as_str(), Some("/Users/me"));
        assert_eq!(
            parsed["data_dir"].as_str(),
            Some("/Users/me/Library/Application Support/ferry-files/fsd/index")
        );
        assert_eq!(parsed["api"]["rest_enabled"].as_bool(), Some(false));
        assert_eq!(parsed["api"]["grpc_enabled"].as_bool(), Some(false));
        assert_eq!(parsed["archive_index"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn config_lists_each_drive() {
        let text = config_text(&[r"C:\".into(), r"D:\".into()], Path::new("/tmp/f"));
        let parsed: toml::Value = toml::from_str(&text).expect("valid TOML");
        let roots: Vec<_> = parsed["watch_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        assert_eq!(roots, [r"C:\", r"D:\"]);
    }

    #[test]
    fn fsd_errors_lose_the_command_name() {
        assert_eq!(
            plain("install-service: `launchctl bootstrap` failed: boom\n".into()),
            "`launchctl bootstrap` failed: boom"
        );
        assert_eq!(plain("a: b".into()), "a: b");
    }

    #[test]
    fn settings_change_and_the_rest_stays() {
        let text = config_text(&["/Users/me".into()], Path::new("/tmp/f"));
        let s = settings_in(&text).unwrap();
        assert_eq!(s.folders, ["/Users/me"]);
        assert_eq!(s.skip, USUAL_SKIP);
        assert!(!s.hidden);
        let changed = FsdSettings {
            folders: vec![
                "/Users/me".into(),
                "/Volumes/Photos".into(),
                "/Users/me".into(),
            ],
            skip: vec![
                "  **/build/**  ".into(),
                String::new(),
                "**/build/**".into(),
            ],
            hidden: true,
            text: false,
        };
        let out = with_settings(&text, &changed).unwrap();
        assert_eq!(
            settings_in(&out).unwrap(),
            FsdSettings {
                folders: vec!["/Users/me".into(), "/Volumes/Photos".into()],
                skip: vec!["**/build/**".into()],
                hidden: true,
                text: false,
            }
        );
        // Comments and the other settings keep their place.
        assert!(out.starts_with("# fsd's settings, written by Ferry Files"));
        assert!(out.contains("# Ferry Files searches by name"));
        assert!(out.contains("rest_enabled = false"));
        assert!(
            out.contains("watch_paths = [\n    \"/Users/me\",\n    \"/Volumes/Photos\",\n]"),
            "{out}"
        );
        // A file without exclusions gets them.
        let bare = with_settings("data_dir = \"/d\"\nlog_dir = \"/l\"\n", &changed).unwrap();
        assert!(settings_in(&bare).unwrap().hidden);
    }

    #[test]
    fn reading_text_turns_on_and_off() {
        let text = config_text(&["/Users/me".into()], Path::new("/tmp/f"));
        let mut s = settings_in(&text).unwrap();
        assert!(!s.text);
        s.text = true;
        let on = with_settings(&text, &s).unwrap();
        assert!(settings_in(&on).unwrap().text);
        let cfg: toml::Value = toml::from_str(&on).unwrap();
        assert_eq!(
            cfg["content"]["extension_limits"]["md"].as_integer(),
            Some(131072)
        );
        assert!(cfg["content"]["extension_limits"].get("*").is_none());
        // The user's own limits stay while it's on.
        let own = format!("{on}").replace("md = 131072", "md = 7");
        assert!(with_settings(&own, &s).unwrap().contains("md = 7"));
        s.text = false;
        let off = with_settings(&on, &s).unwrap();
        assert!(!settings_in(&off).unwrap().text);
        assert!(!off.contains("md = "), "{off}");
    }

    #[test]
    fn fsd_objections_fit_in_a_line() {
        let unread = "Failed to load config: Config error: TOML parse error at line 1, column 15\n  |\n1 | watch_paths = 3\n  |               ^\ninvalid type: integer `3`, expected a sequence\n\n";
        assert_eq!(
            objection(unread),
            "invalid type: integer `3`, expected a sequence (TOML parse error at line 1, column 15)"
        );
        assert_eq!(
            objection("WARNING: watch_path does not exist\nERROR: a\nERROR: b\n"),
            "a; b"
        );
        assert_eq!(objection(" odd \n"), "odd");
    }

    #[test]
    fn settings_need_a_full_folder_path() {
        let s = |folders: &[&str]| FsdSettings {
            folders: folders.iter().map(|f| f.to_string()).collect(),
            skip: vec![],
            hidden: false,
            text: false,
        };
        assert!(with_settings("", &s(&[]))
            .unwrap_err()
            .contains("Add a folder"));
        assert!(with_settings("", &s(&["me/docs"]))
            .unwrap_err()
            .contains("full path"));
        assert!(with_settings("x = ", &s(&["/a"]))
            .unwrap_err()
            .contains("mistake"));
    }

    #[test]
    fn only_a_user_service_with_ferrys_config_is_ours() {
        let config = config_path().to_string_lossy().into_owned();
        let with = |text: &str, user| Registered {
            text: text.into(),
            place: String::new(),
            user,
        };
        assert!(ours(&with(&format!("<string>{config}</string>"), true)));
        assert!(!ours(&with(&format!("<string>{config}</string>"), false)));
        assert!(!ours(&with("<string>/etc/fs/config.toml</string>", true)));
    }
}
