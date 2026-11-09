//! Discovers extension executables under `extensions_dir`.
//!
//! Layout: `<extensions_dir>/<name>/extension.toml` beside its executable.
//! A broken extension is reported and skipped; search must still start.

mod process;
mod trust;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use common::extension::Extension;
use common::extension_stdio::EXTENSION_PROTOCOL_VERSION;
use serde::Deserialize;

use crate::error::AppError;
use process::{ProcessExtension, Spec};

pub type Extensions = Vec<Arc<dyn Extension>>;

const MANIFEST: &str = "extension.toml";

#[derive(Default)]
pub struct Loaded {
    pub extensions: Extensions,
    pub problems: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: String,
    version: String,
    protocol: u32,
    executable: String,
}

pub fn load(config_path: &Path) -> Result<Loaded, AppError> {
    let text = std::fs::read_to_string(config_path).map_err(AppError::Io)?;
    let root: toml::Table =
        toml::from_str(&text).map_err(|e| AppError::Config(format!("config parse: {e}")))?;

    let sections: Vec<String> = root
        .get("extensions")
        .and_then(toml::Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();
    let dir = root.get("extensions_dir").and_then(toml::Value::as_str);

    let mut loaded = Loaded::default();
    match dir {
        Some(dir) => discover(Path::new(dir), config_path, &mut loaded),
        None if !sections.is_empty() => loaded.problems.push(format!(
            "[extensions.{}] is configured but extensions_dir is not set",
            sections.join(", ")
        )),
        None => {}
    }

    for section in sections {
        if dir.is_some() && !loaded.extensions.iter().any(|e| e.name() == section) {
            loaded
                .problems
                .push(format!("[extensions.{section}]: extension not loaded"));
        }
    }
    Ok(loaded)
}

fn discover(dir: &Path, config_path: &Path, loaded: &mut Loaded) {
    let root = match trust::check_root(dir) {
        Ok(root) => root,
        Err(e) => {
            loaded.problems.push(format!("extensions_dir: {e}"));
            return;
        }
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) => {
            loaded.problems.push(format!("extensions_dir: {e}"));
            return;
        }
    };

    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_file() {
            continue;
        }
        match load_one(&path, config_path) {
            Ok(ext) => loaded.extensions.push(Arc::new(ext)),
            Err(e) => loaded.problems.push(format!("{}: {e}", path.display())),
        }
    }
}

fn load_one(dir: &Path, config_path: &Path) -> Result<ProcessExtension, String> {
    trust::check_entry(dir)?;
    let manifest_path = dir.join(MANIFEST);
    trust::check_entry(&manifest_path)?;

    let text = std::fs::read_to_string(&manifest_path).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let manifest: Manifest = toml::from_str(&text).map_err(|e| format!("{MANIFEST}: {e}"))?;

    let folder = dir.file_name().and_then(|n| n.to_str());
    if folder != Some(manifest.name.as_str()) {
        return Err(format!(
            "manifest name '{}' must match its folder name",
            manifest.name
        ));
    }
    if !valid_name(&manifest.name) {
        return Err(format!("invalid extension name '{}'", manifest.name));
    }
    if manifest.protocol != EXTENSION_PROTOCOL_VERSION {
        return Err(format!(
            "protocol {} is not supported (this fsd speaks {EXTENSION_PROTOCOL_VERSION})",
            manifest.protocol
        ));
    }

    let executable = dir.join(executable_name(&manifest.executable)?);
    trust::check_entry(&executable)?;
    trust::check_executable(&executable)?;

    Ok(ProcessExtension::new(Spec {
        name: manifest.name,
        version: manifest.version,
        executable,
        config_path: config_path.to_path_buf(),
    }))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// A bare file name keeps the manifest from pointing outside the vetted folder.
fn executable_name(raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err(format!("executable '{raw}' must be a bare file name")),
    }
    let mut name = path.to_path_buf();
    if cfg!(windows) && name.extension().is_none() {
        name.set_extension("exe");
    }
    Ok(name)
}

#[cfg(all(test, unix))]
mod tests;
