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
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open("/proc/mounts") else {
        return false;
    };
    let path_str = path.to_string_lossy();
    BufReader::new(file)
        .lines()
        .map_while(std::io::Result::ok)
        .any(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                path_str.starts_with(parts[1])
                    && matches!(parts[2], "nfs" | "nfs4" | "cifs" | "smbfs")
            } else {
                false
            }
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
