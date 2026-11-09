//! Builds the extensions listed under `[extensions]` in the daemon config.

use std::path::Path;
use std::sync::Arc;

use common::extension::Extension;

use crate::error::AppError;

pub type Extensions = Vec<Arc<dyn Extension>>;

// Reads the file itself because each extension owns its section's schema; the
// daemon's `Config` only has to ignore the `[extensions]` table.
pub fn load(config_path: &Path) -> Result<Extensions, AppError> {
    let text = std::fs::read_to_string(config_path).map_err(AppError::Io)?;
    let root: toml::Table =
        toml::from_str(&text).map_err(|e| AppError::Config(format!("config parse: {e}")))?;

    let Some(section) = root.get("extensions") else {
        return Ok(Vec::new());
    };
    let table = section
        .as_table()
        .ok_or_else(|| AppError::Config("[extensions] must be a table".into()))?;

    table.keys().map(|name| build(name, config_path)).collect()
}

fn build(name: &str, config_path: &Path) -> Result<Arc<dyn Extension>, AppError> {
    match name {
        #[cfg(feature = "rar2zip")]
        "rar2zip" => {
            let ext = fs_ext_rar2zip::extension::Rar2zipExtension::load(config_path)
                .map_err(|e| AppError::Config(format!("extension rar2zip: {e}")))?;
            Ok(Arc::new(ext))
        }
        #[cfg(not(feature = "rar2zip"))]
        "rar2zip" => {
            let _ = config_path;
            Err(AppError::Config(
                "[extensions.rar2zip] is configured, but this fsd was built without it \
                 (rebuild with --features rar2zip)"
                    .into(),
            ))
        }
        other => Err(AppError::Config(format!("unknown extension: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn config(text: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(text.as_bytes()).unwrap();
        f
    }

    #[test]
    fn no_extensions_section_loads_nothing() {
        let f = config("watch_paths = []\n");
        assert!(load(f.path()).unwrap().is_empty());
    }

    #[test]
    fn unknown_extension_is_rejected() {
        let f = config("[extensions.nope]\n");
        let err = load(f.path()).err().unwrap().to_string();
        assert!(err.contains("unknown extension: nope"), "{err}");
    }

    #[cfg(not(feature = "rar2zip"))]
    #[test]
    fn rar2zip_without_feature_names_the_fix() {
        let f = config("[extensions.rar2zip]\n");
        let err = load(f.path()).err().unwrap().to_string();
        assert!(err.contains("--features rar2zip"), "{err}");
    }

    #[cfg(feature = "rar2zip")]
    #[test]
    fn rar2zip_section_builds_the_extension() {
        let f = config(
            r#"
[extensions.rar2zip]
watch_folder = "/tmp/in"
failed_folder = "/tmp/failed"
destinations = []

[extensions.rar2zip.conversion]
"#,
        );
        let exts = load(f.path()).unwrap();
        assert_eq!(exts.len(), 1);
        assert_eq!(exts[0].name(), "rar2zip");
    }
}
