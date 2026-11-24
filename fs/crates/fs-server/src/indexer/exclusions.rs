use std::path::{Path, PathBuf};

use std::sync::{Arc, Mutex};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{Config, ExclusionConfig};
use crate::error::{AppError, Result};

/// One automaton for all patterns: matching five `glob::Pattern`s one by
/// one cost 0.83 us per path, the set 0.13 us (1M paths, 5 default rules).
#[derive(Clone)]
pub struct ExclusionEngine {
    set: GlobSet,
    exclude_hidden: bool,
    /// fsd's own folders (`own_dirs`), resolved.
    own: Vec<PathBuf>,
}

/// The folders fsd writes to all the time: its index, logs and archive
/// sidecars. Indexed, each write would be an event whose indexing writes
/// again, so an idle daemon would never go idle. Not resolved: see
/// `resolve_own`.
pub fn own_dirs(cfg: &Config) -> Vec<PathBuf> {
    let svc = &cfg.archive_service;
    [
        Some(&cfg.data_dir),
        Some(&cfg.log_dir),
        svc.zidx_dir.as_ref(),
        svc.dzx_dir.as_ref(),
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect()
}

/// `dirs` as the crawler and the watchers spell paths: absolute, links
/// resolved, and without `\\?\` on Windows, as `config::load` keeps watch
/// paths.
fn resolve_own(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .filter_map(|d| {
            let d = std::path::absolute(d).ok()?;
            Some(match d.canonicalize() {
                Ok(c) if cfg!(windows) => crate::config::without_verbatim(c),
                Ok(c) => c,
                Err(_) => d,
            })
        })
        .collect()
}

impl ExclusionEngine {
    pub fn new(cfg: &ExclusionConfig) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &cfg.path_patterns {
            // globset compares Windows paths with `/` separators and reads
            // `\` there as a literal; the glob crate let `\` match either.
            let pattern = if cfg!(windows) {
                pattern.replace('\\', "/")
            } else {
                pattern.clone()
            };
            let glob = Glob::new(&pattern).map_err(|e| AppError::Config(e.to_string()))?;
            builder.add(glob);
        }
        Ok(Self {
            set: builder
                .build()
                .map_err(|e| AppError::Config(e.to_string()))?,
            exclude_hidden: cfg.exclude_hidden,
            own: Vec::new(),
        })
    }

    /// The engine for the daemon's config: its patterns, and its own folders
    /// (`own_dirs`) kept out.
    pub fn for_config(cfg: &Config) -> Result<Self> {
        let mut engine = Self::new(&cfg.exclusions)?;
        engine.own = resolve_own(&own_dirs(cfg));
        Ok(engine)
    }

    /// `for_config`, built once and reused until the patterns or folders
    /// change; watcher events ask for it per batch.
    pub fn shared(cfg: &Config) -> Result<Arc<Self>> {
        type Key = (Vec<String>, bool, Vec<PathBuf>);
        static CACHE: Mutex<Option<(Key, Arc<ExclusionEngine>)>> = Mutex::new(None);
        let key = (
            cfg.exclusions.path_patterns.clone(),
            cfg.exclusions.exclude_hidden,
            own_dirs(cfg),
        );
        let mut cache = CACHE
            .lock()
            .map_err(|_| AppError::Config("exclusion cache poisoned".into()))?;
        if let Some((cached, engine)) = cache.as_ref() {
            if *cached == key {
                return Ok(Arc::clone(engine));
            }
        }
        let engine = Arc::new(Self::for_config(cfg)?);
        *cache = Some((key, Arc::clone(&engine)));
        Ok(engine)
    }

    /// Returns true if this path should be excluded from indexing.
    pub fn should_exclude(&self, path: &Path, is_hidden: bool) -> bool {
        (self.exclude_hidden && is_hidden)
            || self.own.iter().any(|d| path.starts_with(d))
            || self.set.is_match(path)
    }
}

/// Logs each of fsd's own folders that lies under a watch path, once per
/// start: it is left out of the index.
pub fn log_own_dirs_under(cfg: &Config) {
    for dir in resolve_own(&own_dirs(cfg)) {
        if let Some(root) = cfg.watch_paths.iter().find(|r| dir.starts_with(r)) {
            tracing::info!(
                "{} is under watch path {}; it is not indexed",
                dir.display(),
                root.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Network mount detection - platform-specific
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub fn is_network_mount(path: &Path) -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    network_by_mount_table(&mounts, path)
}

/// The deepest mount point holding `path` decides, so a local disk mounted
/// inside a network share is local.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn network_by_mount_table(mounts: &str, path: &Path) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let _device = parts.next()?;
            // /proc/mounts escapes the space in a mount point as \040.
            let point = parts.next()?.replace("\\040", " ");
            let fstype = parts.next()?;
            path.starts_with(&point)
                .then(|| (point.len(), fstype.to_owned()))
        })
        .max_by_key(|(len, _)| *len)
        .is_some_and(|(_, fstype)| {
            matches!(
                fstype.as_str(),
                "nfs" | "nfs4" | "cifs" | "smbfs" | "smb3" | "fuse.sshfs"
            )
        })
}

#[cfg(target_os = "macos")]
pub fn is_network_mount(path: &Path) -> bool {
    use nix::sys::statfs::statfs;
    statfs(path)
        .map(|st| {
            matches!(
                st.filesystem_type_name(),
                "nfs" | "smbfs" | "afpfs" | "webdav"
            )
        })
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn is_network_mount(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_REMOTE_VAL: u32 = 4;
    let root: Vec<u16> = path
        .components()
        .next()
        .map(|c| {
            let mut v: Vec<u16> = c.as_os_str().encode_wide().collect();
            if !v.ends_with(&[b'\\' as u16]) {
                v.push(b'\\' as u16);
            }
            v.push(0);
            v
        })
        .unwrap_or_else(|| vec![0]);
    let drive_type = unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) };
    drive_type == DRIVE_REMOTE_VAL
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn is_network_mount(_path: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ExclusionConfig;

    fn make_engine(patterns: &[&str], exclude_hidden: bool) -> ExclusionEngine {
        ExclusionEngine::new(&ExclusionConfig {
            path_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            exclude_hidden,
            max_content_size_bytes: 0,
            skip_network_content: false,
        })
        .unwrap()
    }

    #[test]
    fn mount_table_uses_the_deepest_mount_point() {
        let table = "/dev/sda1 / ext4 rw 0 0\n\
                     server:/share /mnt/nfs nfs4 rw 0 0\n\
                     /dev/sdb1 /mnt/nfs/local ext4 rw 0 0\n\
                     //host/my\\040share /mnt/my\\040share cifs rw 0 0\n";
        assert!(network_by_mount_table(table, Path::new("/mnt/nfs/a")));
        assert!(!network_by_mount_table(table, Path::new("/mnt/nfsdata/a")));
        assert!(!network_by_mount_table(
            table,
            Path::new("/mnt/nfs/local/a")
        ));
        assert!(network_by_mount_table(table, Path::new("/mnt/my share/a")));
        assert!(!network_by_mount_table(table, Path::new("/home/a")));
    }

    #[test]
    fn star_crosses_separators_as_the_glob_crate_did() {
        let eng = make_engine(&["/w/*.pyc", "**/target/**"], false);
        assert!(eng.should_exclude(Path::new("/w/a/b/c.pyc"), false));
        assert!(eng.should_exclude(Path::new("/p/target/debug/x"), false));
        assert!(!eng.should_exclude(Path::new("/p/target"), false));
        assert!(!eng.should_exclude(Path::new("/p/targets/x"), false));
    }

    fn config(patterns: &[&str], data_dir: &str) -> Config {
        let mut cfg: Config = toml::from_str(&format!(
            "data_dir = '{data_dir}'
log_dir = '{data_dir}/logs'"
        ))
        .unwrap();
        cfg.exclusions.path_patterns = patterns.iter().map(|s| s.to_string()).collect();
        cfg
    }

    #[test]
    fn shared_engine_follows_pattern_changes() {
        let a = config(&["**/a/**"], "/d");
        let b = config(&["**/b/**"], "/d");
        assert!(ExclusionEngine::shared(&a)
            .unwrap()
            .should_exclude(Path::new("/x/a/f"), false));
        let eng = ExclusionEngine::shared(&b).unwrap();
        assert!(!eng.should_exclude(Path::new("/x/a/f"), false));
        assert!(eng.should_exclude(Path::new("/x/b/f"), false));
    }

    #[test]
    fn the_daemons_own_folders_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = resolve_own(&[dir.path().to_path_buf()]).remove(0);
        std::fs::create_dir_all(root.join("data").join("logs")).unwrap();
        let cfg = config(&[], &root.join("data").to_string_lossy());
        let eng = ExclusionEngine::for_config(&cfg).unwrap();
        assert!(eng.should_exclude(&root.join("data"), false));
        assert!(eng.should_exclude(&root.join("data").join("index.bin"), false));
        assert!(eng.should_exclude(&root.join("data").join("logs").join("x.log"), false));
        assert!(!eng.should_exclude(&root.join("database.txt"), false));
        assert!(!eng.should_exclude(&root.join("src").join("data"), false));
        // The pattern-only engine leaves them in.
        assert!(!ExclusionEngine::new(&cfg.exclusions)
            .unwrap()
            .should_exclude(&root.join("data").join("index.bin"), false));
    }

    #[test]
    fn glob_node_modules_excluded() {
        let eng = make_engine(&["**/node_modules/**"], false);
        assert!(eng.should_exclude(Path::new("/project/node_modules/lodash/index.js"), false));
    }

    #[test]
    fn non_matching_path_not_excluded() {
        let eng = make_engine(&["**/node_modules/**"], false);
        assert!(!eng.should_exclude(Path::new("/project/src/main.rs"), false));
    }

    #[test]
    fn hidden_excluded_when_flag_set() {
        let eng = make_engine(&[], true);
        assert!(eng.should_exclude(Path::new("/home/user/.bashrc"), true));
    }

    #[test]
    fn hidden_not_excluded_when_flag_unset() {
        let eng = make_engine(&[], false);
        assert!(!eng.should_exclude(Path::new("/home/user/.bashrc"), true));
    }
}
