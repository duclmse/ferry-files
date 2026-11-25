//! Changing the config while the daemon runs, for the IPC, REST and gRPC
//! APIs. fsd reads its config file only at startup and does not follow it;
//! a change made here goes to the file, keeping its comments and layout,
//! and to the shared config the daemon runs with. Settings read as they are
//! used apply at once; those read at startup (`AT_STARTUP`) apply when fsd
//! restarts, and a change reports them. Extensions whose
//! `[extensions.<name>]` changed are asked to reload.
//!
//! The file is written over in place rather than replaced, so it keeps its
//! owner and permissions: a service's config may be readable by its account
//! alone.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use fs_protocol::ipc_protocol::{ConfigApplied, ConfigChange, ConfigValue};
use tokio::sync::{Mutex, MutexGuard, RwLock};

use crate::config::{self, Config};
use crate::extensions::Extensions;

/// Settings fsd reads once, at startup.
const AT_STARTUP: &[&str] = &[
    "data_dir",
    "log_dir",
    "store.mode",
    "store.indexes",
    "archive_service",
    "api.rest_enabled",
    "api.rest_bind",
    "api.rest_port",
    "api.rest_cors_origins",
    "api.grpc_enabled",
    "api.grpc_bind",
    "api.grpc_port",
    "api.ipc_enabled",
    "api.ipc_path",
    "watcher",
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("not set: {0}")]
    NotFound(String),
    #[error("{0}")]
    Refused(String),
    #[error("cannot update the config: {0}")]
    Io(String),
}

/// The daemon's config as the APIs read and change it. One per daemon;
/// `WatchList` changes the watch paths through it.
pub struct ConfigEditor {
    config: Arc<RwLock<Config>>,
    extensions: Arc<Extensions>,
    /// One change at a time: each reads the file the last one wrote.
    changing: Mutex<()>,
    /// The `AT_STARTUP` settings as the daemon started, taken at the first
    /// change: until then nothing else changes them.
    started: OnceLock<toml::Value>,
}

impl ConfigEditor {
    pub fn new(config: Arc<RwLock<Config>>, extensions: Arc<Extensions>) -> Arc<Self> {
        Arc::new(Self {
            config,
            extensions,
            changing: Mutex::new(()),
            started: OnceLock::new(),
        })
    }

    /// Held by every change to the file.
    pub(crate) async fn lock(&self) -> MutexGuard<'_, ()> {
        self.changing.lock().await
    }

    /// The whole file, or with `key` the value the daemon runs with: the
    /// file's when it sets the key, otherwise the default.
    pub async fn get(&self, key: Option<&str>) -> Result<ConfigValue, ConfigError> {
        let text = read(self.file().await?).await?;
        let Some(key) = key else {
            return Ok(ConfigValue {
                text,
                from_file: true,
            });
        };
        let keys = parse_key(key)?;
        let doc = parse_doc(&text)?;
        let mut item = doc.as_item();
        let mut found = true;
        for k in &keys {
            match item.get(k.as_str()) {
                Some(i) => item = i,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if found {
            let text = match item.as_value() {
                Some(v) => {
                    let mut v = v.clone();
                    v.decor_mut().clear();
                    v.to_string()
                }
                None => item.to_string().trim().to_owned(),
            };
            return Ok(ConfigValue {
                text,
                from_file: true,
            });
        }
        let running = as_value(&*self.config.read().await)?;
        let value = at(&running, &keys).ok_or_else(|| ConfigError::NotFound(key.to_owned()))?;
        Ok(ConfigValue {
            text: render(value),
            from_file: false,
        })
    }

    /// Applies `change` to the file and the running daemon. A change that
    /// does not parse or validate changes nothing.
    pub async fn change(&self, change: ConfigChange) -> Result<ConfigApplied, ConfigError> {
        let _one = self.lock().await;
        let file = self.file().await?;
        let before = read(file.clone()).await?;
        let reload = change == ConfigChange::Reload;
        let after = edited(&before, change)?;
        let next =
            config::from_text(&after, &file).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        let errors = config::validate(&next);
        if !errors.is_empty() {
            return Err(ConfigError::Invalid(errors.join("; ")));
        }
        let running = self.config.read().await.clone();
        if next.extensions_dir != running.extensions_dir {
            return Err(ConfigError::Refused(
                "extensions_dir names the programs fsd runs, so it changes only in the file, \
                 while fsd is stopped"
                    .into(),
            ));
        }
        let started = match self.started.get() {
            Some(s) => s.clone(),
            None => {
                let s = as_value(&running)?;
                self.started.get_or_init(|| s).clone()
            }
        };
        let next_value = as_value(&next)?;
        let restart = AT_STARTUP
            .iter()
            .filter(|k| {
                let keys: Vec<String> = k.split('.').map(str::to_owned).collect();
                at(&started, &keys) != at(&next_value, &keys)
            })
            .map(|k| (*k).to_owned())
            .collect();
        // A reload cannot tell what changed since the daemon last applied
        // the file, so every extension takes it again.
        let sections = if reload {
            self.extensions
                .iter()
                .map(|e| e.name().to_owned())
                .collect()
        } else {
            changed_sections(&before, &after)
        };

        if !reload {
            write(file, after).await?;
        }
        *self.config.write().await = next;

        let mut applied = ConfigApplied {
            restart,
            ..ConfigApplied::default()
        };
        for name in sections {
            let Some(ext) = self.extensions.iter().find(|e| e.name() == name).cloned() else {
                applied
                    .problems
                    .push(format!("{name}: no such extension is loaded"));
                continue;
            };
            match tokio::task::spawn_blocking(move || ext.reload()).await {
                Ok(Ok(())) => applied.reloaded.push(name),
                Ok(Err(e)) => applied.problems.push(format!("{name}: {e:#}")),
                Err(e) => applied.problems.push(format!("{name}: {e}")),
            }
        }
        tracing::info!(
            "config changed; restart for {:?}, reloaded {:?}",
            applied.restart,
            applied.reloaded
        );
        Ok(applied)
    }

    async fn file(&self) -> Result<PathBuf, ConfigError> {
        self.config
            .read()
            .await
            .source_path
            .clone()
            .ok_or_else(|| ConfigError::Io("the daemon was not started from a config file".into()))
    }
}

async fn read(file: PathBuf) -> Result<String, ConfigError> {
    tokio::task::spawn_blocking(move || std::fs::read_to_string(&file).map_err(|e| io(&file, e)))
        .await
        .map_err(|e| ConfigError::Io(e.to_string()))?
}

async fn write(file: PathBuf, text: String) -> Result<(), ConfigError> {
    tokio::task::spawn_blocking(move || std::fs::write(&file, text).map_err(|e| io(&file, e)))
        .await
        .map_err(|e| ConfigError::Io(e.to_string()))?
}

fn io(file: &Path, e: impl std::fmt::Display) -> ConfigError {
    ConfigError::Io(format!("{}: {e}", file.display()))
}

fn parse_doc(text: &str) -> Result<toml_edit::DocumentMut, ConfigError> {
    text.parse()
        .map_err(|e| ConfigError::Invalid(format!("the config file does not parse: {e}")))
}

fn parse_key(key: &str) -> Result<Vec<String>, ConfigError> {
    let keys = toml_edit::Key::parse(key)
        .map_err(|e| ConfigError::Invalid(format!("not a key: {key}: {e}")))?;
    if keys.is_empty() {
        return Err(ConfigError::Invalid("empty key".into()));
    }
    Ok(keys.iter().map(|k| k.get().to_owned()).collect())
}

fn as_value(cfg: &Config) -> Result<toml::Value, ConfigError> {
    toml::Value::try_from(cfg).map_err(|e| ConfigError::Io(format!("config: {e}")))
}

fn at<'a>(mut v: &'a toml::Value, keys: &[String]) -> Option<&'a toml::Value> {
    for k in keys {
        v = v.as_table()?.get(k)?;
    }
    Some(v)
}

fn render(v: &toml::Value) -> String {
    match v {
        toml::Value::Table(t) => toml::to_string(t).unwrap_or_default().trim().to_owned(),
        other => other.to_string(),
    }
}

/// `text` with `change` made, the rest kept as it is.
pub(crate) fn edited(text: &str, change: ConfigChange) -> Result<String, ConfigError> {
    let (key, value) = match change {
        ConfigChange::Replace { text } => return Ok(text),
        ConfigChange::Reload => return Ok(text.to_owned()),
        ConfigChange::Set { key, value } => (key, Some(value)),
        ConfigChange::Unset { key } => (key, None),
    };
    let keys = parse_key(&key)?;
    let mut doc = parse_doc(text)?;
    let Some((last, parents)) = keys.split_last() else {
        return Err(ConfigError::Invalid("empty key".into()));
    };
    let not_table = |k: &str| ConfigError::Invalid(format!("{key}: {k} is not a table"));
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    for k in parents {
        if value.is_none() && !table.contains_key(k) {
            return Err(ConfigError::NotFound(key.clone()));
        }
        let item = table.entry(k).or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        });
        table = item.as_table_like_mut().ok_or_else(|| not_table(k))?;
    }
    match value {
        None => {
            table
                .remove(last)
                .ok_or_else(|| ConfigError::NotFound(key.clone()))?;
        }
        Some(value) => {
            let mut value = toml_value(&value);
            match table.get_mut(last).and_then(|i| i.as_value_mut()) {
                // A value already there keeps its comments.
                Some(old) => {
                    *value.decor_mut() = old.decor().clone();
                    *old = value;
                }
                None => {
                    table.insert(last, toml_edit::Item::Value(value));
                }
            }
        }
    }
    Ok(doc.to_string())
}

/// `s` as a TOML value, or as a string when it is not one: `memory` and
/// `C:\data` need no quotes.
fn toml_value(s: &str) -> toml_edit::Value {
    match s.trim().parse::<toml_edit::Value>() {
        Ok(mut v) => {
            v.decor_mut().clear();
            v
        }
        Err(_) => toml_edit::Value::from(s),
    }
}

/// The names under `[extensions]` whose tables differ between the texts.
fn changed_sections(before: &str, after: &str) -> Vec<String> {
    let sections = |text: &str| -> toml::Table {
        toml::from_str::<toml::Table>(text)
            .ok()
            .and_then(|mut t| t.remove("extensions"))
            .and_then(|v| match v {
                toml::Value::Table(t) => Some(t),
                _ => None,
            })
            .unwrap_or_default()
    };
    let (a, b) = (sections(before), sections(after));
    let mut names: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
    names.sort();
    names.dedup();
    names.retain(|n| a.get(n) != b.get(n));
    names
}

#[cfg(test)]
mod tests;
