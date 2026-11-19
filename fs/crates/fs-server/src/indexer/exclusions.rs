use std::path::Path;

use glob::Pattern;

use crate::config::ExclusionConfig;
use crate::error::{AppError, Result};

#[derive(Clone)]
pub struct ExclusionEngine {
    patterns: Vec<Pattern>,
    exclude_hidden: bool,
}

impl ExclusionEngine {
    pub fn new(cfg: &ExclusionConfig) -> Result<Self> {
        let patterns = cfg
            .path_patterns
            .iter()
            .map(|p| Pattern::new(p).map_err(|e| AppError::Config(e.to_string())))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            patterns,
            exclude_hidden: cfg.exclude_hidden,
        })
    }

    /// Returns true if this path should be excluded from indexing.
    pub fn should_exclude(&self, path: &Path, is_hidden: bool) -> bool {
        if self.exclude_hidden && is_hidden {
            return true;
        }
        let path_str = path.to_string_lossy();
        self.patterns.iter().any(|p| p.matches(&path_str))
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
