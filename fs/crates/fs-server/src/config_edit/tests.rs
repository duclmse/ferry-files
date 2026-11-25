use std::sync::atomic::{AtomicUsize, Ordering};

use common::extension::{Extension, ExtensionFuture};
use tokio_util::sync::CancellationToken;

use super::*;

fn set(key: &str, value: &str) -> ConfigChange {
    ConfigChange::Set {
        key: key.into(),
        value: value.into(),
    }
}

fn unset(key: &str) -> ConfigChange {
    ConfigChange::Unset { key: key.into() }
}

struct Ext {
    reloads: AtomicUsize,
}

impl Extension for Ext {
    fn name(&self) -> &str {
        "demo"
    }
    fn validate(&self) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _shutdown: CancellationToken) -> ExtensionFuture {
        Box::pin(async { Ok(()) })
    }
    fn command(&self, _command: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn reload(&self) -> anyhow::Result<()> {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Daemon {
    _dir: tempfile::TempDir,
    file: PathBuf,
    config: Arc<RwLock<Config>>,
    editor: Arc<ConfigEditor>,
    ext: Arc<Ext>,
}

fn daemon(extra: &str) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("d").to_string_lossy().replace('\\', "/");
    let file = dir.path().join("config.toml");
    let text = format!(
        "# mine\ndata_dir = \"{d}\"\nlog_dir = \"{d}\"\n\n[search]\ndefault_limit = 50 # keep\n{extra}"
    );
    std::fs::write(&file, text).unwrap();
    let config = Arc::new(RwLock::new(config::load(&file).unwrap()));
    let ext = Arc::new(Ext {
        reloads: AtomicUsize::new(0),
    });
    let extensions: Extensions = vec![ext.clone() as Arc<dyn Extension>];
    let editor = ConfigEditor::new(Arc::clone(&config), Arc::new(extensions));
    Daemon {
        _dir: dir,
        file,
        config,
        editor,
        ext,
    }
}

#[tokio::test]
async fn a_change_goes_to_the_file_and_the_running_daemon() {
    let d = daemon("");
    let applied = d
        .editor
        .change(set("search.default_limit", "200"))
        .await
        .unwrap();
    assert!(applied.restart.is_empty(), "{applied:?}");
    assert_eq!(d.config.read().await.search.default_limit, 200);
    let text = std::fs::read_to_string(&d.file).unwrap();
    assert!(text.starts_with("# mine\n"), "{text}");
    assert!(text.contains("default_limit = 200 # keep"), "{text}");

    let got = d.editor.get(Some("search.default_limit")).await.unwrap();
    assert_eq!((got.text.as_str(), got.from_file), ("200", true));
    let got = d.editor.get(Some("search.max_limit")).await.unwrap();
    assert_eq!((got.text.as_str(), got.from_file), ("10000", false));
    let got = d.editor.get(None).await.unwrap();
    assert_eq!(got.text, text);
    assert!(matches!(
        d.editor.get(Some("search.nope")).await,
        Err(ConfigError::NotFound(_))
    ));

    d.editor
        .change(unset("search.default_limit"))
        .await
        .unwrap();
    assert_eq!(d.config.read().await.search.default_limit, 100);
    assert!(matches!(
        d.editor.change(unset("search.default_limit")).await,
        Err(ConfigError::NotFound(_))
    ));
    assert!(matches!(
        d.editor.change(unset("indexer.commit_interval_secs")).await,
        Err(ConfigError::NotFound(_))
    ));
}

#[tokio::test]
async fn settings_read_at_startup_are_reported_until_they_match_again() {
    let d = daemon("");
    let applied = d.editor.change(set("api.rest_port", "9999")).await.unwrap();
    assert_eq!(applied.restart, ["api.rest_port"]);
    let applied = d
        .editor
        .change(set("search.default_limit", "1"))
        .await
        .unwrap();
    assert_eq!(applied.restart, ["api.rest_port"], "still pending");
    let applied = d.editor.change(unset("api.rest_port")).await.unwrap();
    assert!(applied.restart.is_empty(), "{applied:?}");
}

#[tokio::test]
async fn a_change_that_does_not_validate_changes_nothing() {
    let d = daemon("");
    let before = std::fs::read_to_string(&d.file).unwrap();
    for change in [
        set("api.auth_mode", "all"),
        set("search.default_limit", "lots"),
        set("search.default_limit.x", "1"),
        ConfigChange::Replace {
            text: "data_dir = [".into(),
        },
    ] {
        let e = d.editor.change(change.clone()).await;
        assert!(
            matches!(e, Err(ConfigError::Invalid(_))),
            "{change:?}: {e:?}"
        );
    }
    assert_eq!(std::fs::read_to_string(&d.file).unwrap(), before);
    assert_eq!(d.config.read().await.search.default_limit, 50);
}

#[tokio::test]
async fn extensions_dir_is_not_changed_through_the_api() {
    let d = daemon("");
    let e = d.editor.change(set("extensions_dir", "/tmp/x")).await;
    assert!(matches!(e, Err(ConfigError::Refused(_))), "{e:?}");
}

#[tokio::test]
async fn an_extension_whose_section_changed_reloads() {
    let d = daemon("\n[extensions.demo]\nlevel = 1\n");
    let applied = d
        .editor
        .change(set("search.default_limit", "2"))
        .await
        .unwrap();
    assert!(applied.reloaded.is_empty());
    let applied = d
        .editor
        .change(set("extensions.demo.level", "2"))
        .await
        .unwrap();
    assert_eq!(applied.reloaded, ["demo"]);
    assert_eq!(d.ext.reloads.load(Ordering::SeqCst), 1);
    let applied = d
        .editor
        .change(set("extensions.other.x", "1"))
        .await
        .unwrap();
    assert_eq!(applied.problems.len(), 1, "{applied:?}");
    assert!(applied.problems[0].starts_with("other:"), "{applied:?}");
}

#[tokio::test]
async fn a_reload_applies_a_hand_edit_and_reloads_every_extension() {
    let d = daemon("");
    let text = std::fs::read_to_string(&d.file).unwrap();
    std::fs::write(
        &d.file,
        text.replace("default_limit = 50", "default_limit = 7"),
    )
    .unwrap();
    assert_eq!(
        d.config.read().await.search.default_limit,
        50,
        "not followed"
    );
    let applied = d.editor.change(ConfigChange::Reload).await.unwrap();
    assert_eq!(d.config.read().await.search.default_limit, 7);
    assert_eq!(applied.reloaded, ["demo"]);
}

#[test]
fn values_are_toml_or_else_strings() {
    let out = edited("", set("store.mode", "memory")).unwrap();
    assert_eq!(out, "[store]\nmode = \"memory\"\n");
    let out = edited("", set("data_dir", r"C:\fs data")).unwrap();
    let back: toml::Table = toml::from_str(&out).unwrap();
    assert_eq!(back["data_dir"].as_str(), Some(r"C:\fs data"), "{out}");
    let out = edited("", set("archive_index.formats", r#"["zip"]"#)).unwrap();
    assert_eq!(out, "[archive_index]\nformats = [\"zip\"]\n");
    let out = edited("", set(r#"content.extension_limits."*""#, "1024")).unwrap();
    assert_eq!(out, "[content.extension_limits]\n\"*\" = 1024\n");
}

#[test]
fn only_changed_extension_sections_are_named() {
    let a = "[extensions.a]\nx = 1\n[extensions.b]\ny = 1\n";
    let b = "[extensions.a]\nx = 1\n[extensions.b]\ny = 2\n[extensions.c]\n";
    assert_eq!(changed_sections(a, b), ["b", "c"]);
    assert!(changed_sections(a, a).is_empty());
}
