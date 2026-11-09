use std::os::unix::fs::PermissionsExt;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;

// A real child process speaking the protocol. `status` answers and `crash`
// exits, which is enough to exercise the supervisor end to end.
const SCRIPT: &str = r#"#!/bin/sh
if [ "$3" = "validate" ]; then
    echo "bad setting"
    exit 1
fi
while read -r line; do
    case "$line" in
        *shutdown*) exit 0 ;;
        *crash*) exit 3 ;;
        *) id=$(echo "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
           echo "{\"id\":$id,\"ok\":true,\"output\":\"pong\"}" ;;
    esac
done
"#;

struct Fixture {
    dir: TempDir,
    config: PathBuf,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let extensions = dir.path().join("exts");
    std::fs::create_dir(&extensions).unwrap();
    set_mode(&extensions, 0o755);
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!("extensions_dir = \"{}\"\n", extensions.display()),
    )
    .unwrap();
    Fixture { dir, config }
}

impl Fixture {
    fn extensions(&self) -> PathBuf {
        self.dir.path().join("exts")
    }

    fn install(&self, name: &str, manifest: &str) -> PathBuf {
        let folder = self.extensions().join(name);
        std::fs::create_dir(&folder).unwrap();
        set_mode(&folder, 0o755);
        std::fs::write(folder.join("extension.toml"), manifest).unwrap();
        set_mode(&folder.join("extension.toml"), 0o644);
        let exe = folder.join("run.sh");
        std::fs::write(&exe, SCRIPT).unwrap();
        set_mode(&exe, 0o755);
        folder
    }

    fn install_good(&self, name: &str) -> PathBuf {
        self.install(name, &manifest(name, 1, "run.sh"))
    }

    fn load(&self) -> Loaded {
        load(&self.config).unwrap()
    }
}

fn manifest(name: &str, protocol: u32, executable: &str) -> String {
    format!(
        "name = \"{name}\"\nversion = \"1.0.0\"\nprotocol = {protocol}\nexecutable = \"{executable}\"\n"
    )
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn assert_problem(loaded: &Loaded, needle: &str) {
    assert!(loaded.extensions.is_empty());
    assert!(
        loaded.problems.iter().any(|p| p.contains(needle)),
        "{:?}",
        loaded.problems
    );
}

#[test]
fn no_extensions_dir_loads_nothing() {
    let f = fixture();
    std::fs::write(&f.config, "watch_paths = []\n").unwrap();
    let loaded = f.load();
    assert!(loaded.extensions.is_empty() && loaded.problems.is_empty());
}

#[test]
fn section_without_extensions_dir_is_reported() {
    let f = fixture();
    std::fs::write(&f.config, "[extensions.nope]\n").unwrap();
    assert_problem(&f.load(), "extensions_dir is not set");
}

#[test]
fn dropped_in_folder_is_discovered() {
    let f = fixture();
    f.install_good("alpha");
    let loaded = f.load();
    assert_eq!(loaded.extensions.len(), 1, "{:?}", loaded.problems);
    assert_eq!(loaded.extensions[0].name(), "alpha");
}

#[test]
fn one_broken_extension_does_not_block_others() {
    let f = fixture();
    f.install_good("alpha");
    f.install("beta", "not toml =");
    let loaded = f.load();
    assert_eq!(loaded.extensions.len(), 1);
    assert_eq!(loaded.problems.len(), 1);
}

#[test]
fn section_for_missing_extension_is_reported() {
    let f = fixture();
    let mut text = std::fs::read_to_string(&f.config).unwrap();
    text.push_str("[extensions.ghost]\n");
    std::fs::write(&f.config, text).unwrap();
    assert_problem(&f.load(), "[extensions.ghost]");
}

#[test]
fn group_writable_extensions_dir_is_refused() {
    let f = fixture();
    f.install_good("alpha");
    set_mode(&f.extensions(), 0o775);
    assert_problem(&f.load(), "must not be writable");
}

#[test]
fn world_writable_executable_is_refused() {
    let f = fixture();
    let folder = f.install_good("alpha");
    set_mode(&folder.join("run.sh"), 0o777);
    assert_problem(&f.load(), "must not be writable");
}

#[test]
fn symlinked_executable_is_refused() {
    let f = fixture();
    let folder = f.install_good("alpha");
    std::fs::remove_file(folder.join("run.sh")).unwrap();
    std::os::unix::fs::symlink("/bin/sh", folder.join("run.sh")).unwrap();
    assert_problem(&f.load(), "must not be a symlink");
}

#[test]
fn executable_outside_the_folder_is_refused() {
    let f = fixture();
    f.install("alpha", &manifest("alpha", 1, "../../evil"));
    assert_problem(&f.load(), "bare file name");
}

#[test]
fn manifest_name_must_match_folder() {
    let f = fixture();
    f.install("alpha", &manifest("beta", 1, "run.sh"));
    assert_problem(&f.load(), "must match its folder");
}

#[test]
fn unsupported_protocol_is_refused() {
    let f = fixture();
    f.install("alpha", &manifest("alpha", 99, "run.sh"));
    assert_problem(&f.load(), "protocol 99");
}

#[test]
fn validate_reports_the_executables_errors() {
    let f = fixture();
    f.install_good("alpha");
    let loaded = f.load();
    assert_eq!(loaded.extensions[0].validate(), vec!["bad setting"]);
}

async fn command(ext: &Arc<dyn Extension>, name: &'static str) -> anyhow::Result<String> {
    let ext = Arc::clone(ext);
    tokio::task::spawn_blocking(move || ext.command(name))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_commands_and_stops_cleanly() {
    let f = fixture();
    f.install_good("alpha");
    let ext = f.load().extensions.remove(0);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(ext.run(shutdown.clone()));

    assert_eq!(command(&ext, "status").await.unwrap(), "pong");

    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn crashed_extension_is_restarted() {
    let f = fixture();
    f.install_good("alpha");
    let ext = f.load().extensions.remove(0);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(ext.run(shutdown.clone()));

    assert_eq!(command(&ext, "status").await.unwrap(), "pong");
    assert!(command(&ext, "crash").await.is_err());

    // The first restart waits one second.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if command(&ext, "status").await.is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never came back");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    shutdown.cancel();
    task.await.unwrap().unwrap();
}
