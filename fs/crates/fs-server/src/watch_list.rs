//! Adding and removing watch paths while the daemon runs, for the IPC, REST
//! and gRPC APIs. A change goes to the config file, so it outlasts a restart,
//! and to the shared config, which `daemon::follow_watch_paths` follows:
//! it crawls an added path and starts watching it.
//!
//! The file is edited in place, keeping its comments and layout, and written
//! over rather than replaced: the config watcher follows the file itself, and
//! a rename would leave it watching the old one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::config::{without_verbatim, Config};

#[derive(Debug, thiserror::Error)]
pub enum WatchListError {
    #[error("{0}")]
    Invalid(String),
    #[error("not a watch path: {0}")]
    NotFound(String),
    #[error("cannot update the config: {0}")]
    Config(String),
}

/// What a change did: the watch paths after it, and whether they changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub watch_paths: Vec<PathBuf>,
    pub changed: bool,
}

/// One change at a time: each reads the file the last one wrote.
static CHANGING: Mutex<()> = Mutex::const_new(());

/// Adds `path`, which must be an absolute path to a folder. A path that is
/// already watched, or lies inside one that is, changes nothing.
pub async fn add(config: &Arc<RwLock<Config>>, path: &str) -> Result<Outcome, WatchListError> {
    let _one = CHANGING.lock().await;
    let path = folder(path)?;
    let (mut paths, source) = current(config).await;
    if let Some(outer) = paths.iter().find(|p| path.starts_with(p)) {
        if *outer != path {
            return Err(WatchListError::Invalid(format!(
                "{} is already watched as part of {}",
                path.display(),
                outer.display()
            )));
        }
        return Ok(Outcome {
            watch_paths: paths,
            changed: false,
        });
    }
    paths.push(path.clone());
    if let Some(file) = source {
        edit_file(file, move |entries| {
            entries.push(path.to_string_lossy().into_owned());
        })
        .await?;
    }
    Ok(store(config, paths).await)
}

/// Removes `path`, as given or as it resolves. Its entries stay in the index.
pub async fn remove(config: &Arc<RwLock<Config>>, path: &str) -> Result<Outcome, WatchListError> {
    let _one = CHANGING.lock().await;
    let target = resolved(Path::new(path));
    let (mut paths, source) = current(config).await;
    let before = paths.len();
    paths.retain(|p| resolved(p) != target);
    if paths.len() == before {
        return Err(WatchListError::NotFound(path.to_owned()));
    }
    if let Some(file) = source {
        edit_file(file, move |entries| {
            entries.retain(|e| resolved(Path::new(e)) != target);
        })
        .await?;
    }
    Ok(store(config, paths).await)
}

/// The path as `config::load` keeps a watch path: resolved where it exists.
fn resolved(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) if cfg!(windows) => without_verbatim(c),
        Ok(c) => c,
        Err(_) => p.to_path_buf(),
    }
}

fn folder(path: &str) -> Result<PathBuf, WatchListError> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(WatchListError::Invalid(format!(
            "watch path must be absolute: {path}"
        )));
    }
    if !p.is_dir() {
        return Err(WatchListError::Invalid(format!("not a folder: {path}")));
    }
    Ok(resolved(p))
}

async fn current(config: &Arc<RwLock<Config>>) -> (Vec<PathBuf>, Option<PathBuf>) {
    let c = config.read().await;
    (c.watch_paths.clone(), c.source_path.clone())
}

async fn store(config: &Arc<RwLock<Config>>, paths: Vec<PathBuf>) -> Outcome {
    config.write().await.watch_paths = paths.clone();
    Outcome {
        watch_paths: paths,
        changed: true,
    }
}

/// Rewrites `watch_paths` in `file` with `change`, leaving the rest as it is.
async fn edit_file(
    file: PathBuf,
    change: impl FnOnce(&mut Vec<String>) + Send + 'static,
) -> Result<(), WatchListError> {
    tokio::task::spawn_blocking(move || {
        let text = std::fs::read_to_string(&file).map_err(|e| err(&file, e))?;
        let edited = edit_text(&text, change).map_err(|e| err(&file, e))?;
        std::fs::write(&file, edited).map_err(|e| err(&file, e))
    })
    .await
    .map_err(|e| WatchListError::Config(e.to_string()))?
}

fn err(file: &Path, e: impl std::fmt::Display) -> WatchListError {
    WatchListError::Config(format!("{}: {e}", file.display()))
}

fn edit_text(text: &str, change: impl FnOnce(&mut Vec<String>)) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let mut entries: Vec<String> = match doc.get("watch_paths") {
        None => Vec::new(),
        Some(item) => item
            .as_array()
            .ok_or("watch_paths is not a list")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or("watch_paths holds a non-string")
            })
            .collect::<Result<_, _>>()?,
    };
    change(&mut entries);
    let kept = doc.get("watch_paths").and_then(|i| i.as_array());
    let mut array = toml_edit::Array::new();
    for e in &entries {
        // Entries already there keep their own spelling and quoting.
        let same = kept.and_then(|a| a.iter().find(|v| v.as_str() == Some(e.as_str())));
        array.push_formatted(match same {
            Some(v) => v.clone(),
            None => literal(e),
        });
    }
    if entries.len() > 1 {
        for v in array.iter_mut() {
            v.decor_mut().set_prefix("\n    ");
        }
        array.set_trailing("\n");
        array.set_trailing_comma(true);
    } else if let Some(v) = array.iter_mut().next() {
        v.decor_mut().set_prefix("");
    }
    match doc.get_mut("watch_paths").and_then(|i| i.as_value_mut()) {
        Some(value) => {
            let decor = value.decor().clone();
            *value = toml_edit::Value::Array(array);
            *value.decor_mut() = decor;
        }
        None => {
            doc.insert("watch_paths", toml_edit::value(array));
        }
    }
    Ok(doc.to_string())
}

/// A TOML string for `s`: a literal one, so Windows backslashes stay as
/// they are, unless `s` holds a quote.
fn literal(s: &str) -> toml_edit::Value {
    if !s.contains(['\'', '\n', '\r']) {
        if let Ok(v) = format!("'{s}'").parse() {
            return v;
        }
    }
    toml_edit::Value::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(dir: &Path, text: &str) -> (Arc<RwLock<Config>>, PathBuf) {
        let file = dir.join("config.toml");
        std::fs::write(&file, text).unwrap();
        let cfg = crate::config::load(&file).unwrap();
        (Arc::new(RwLock::new(cfg)), file)
    }

    #[test]
    fn the_edit_keeps_the_rest_of_the_file() {
        let text = "# top\nwatch_paths = ['/a'] # mine\ndata_dir = \"d\"\n\n[api]\n# port\nrest_port = 1\n";
        let out = edit_text(text, |e| e.push(r"C:\x y".into())).unwrap();
        assert_eq!(
            out,
            "# top\nwatch_paths = [\n    '/a',\n    'C:\\x y',\n] # mine\ndata_dir = \"d\"\n\n[api]\n# port\nrest_port = 1\n"
        );
        let back = edit_text(&out, |e| e.retain(|p| p == "/a")).unwrap();
        assert_eq!(
            back,
            "# top\nwatch_paths = ['/a'] # mine\ndata_dir = \"d\"\n\n[api]\n# port\nrest_port = 1\n"
        );
        let parsed: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(parsed["watch_paths"][1].as_str(), Some(r"C:\x y"));
    }

    #[test]
    fn a_file_without_watch_paths_gets_them() {
        let out = edit_text("data_dir = \"d\"\n[api]\nrest_port = 1\n", |e| {
            e.push("/w".into())
        })
        .unwrap();
        let parsed: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(parsed["watch_paths"][0].as_str(), Some("/w"));
        assert_eq!(parsed["api"]["rest_port"].as_integer(), Some(1));
    }

    #[tokio::test]
    async fn add_and_remove_reach_the_config_and_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(first.join("inner")).unwrap();
        std::fs::create_dir(&second).unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let (cfg, file) = shared(
            dir.path(),
            &format!(
                "watch_paths = [\"{}\"]\ndata_dir = \"{}\"\nlog_dir = \"{}\"\n",
                fwd(&first),
                fwd(&dir.path().join("d")),
                fwd(&dir.path().join("l"))
            ),
        );
        let second_s = second.to_string_lossy().into_owned();

        let added = add(&cfg, &second_s).await.unwrap();
        assert!(added.changed);
        assert_eq!(added.watch_paths.len(), 2);
        assert_eq!(cfg.read().await.watch_paths, added.watch_paths);
        assert_eq!(
            crate::config::load(&file).unwrap().watch_paths,
            added.watch_paths
        );

        assert!(!add(&cfg, &second_s).await.unwrap().changed);
        let inner = first.join("inner").to_string_lossy().into_owned();
        assert!(matches!(
            add(&cfg, &inner).await,
            Err(WatchListError::Invalid(_))
        ));
        assert!(matches!(
            add(&cfg, "relative").await,
            Err(WatchListError::Invalid(_))
        ));

        let removed = remove(&cfg, &first.to_string_lossy()).await.unwrap();
        assert_eq!(removed.watch_paths, [resolved(&second)]);
        assert_eq!(
            crate::config::load(&file).unwrap().watch_paths,
            removed.watch_paths
        );
        assert!(matches!(
            remove(&cfg, &first.to_string_lossy()).await,
            Err(WatchListError::NotFound(_))
        ));
    }
}
