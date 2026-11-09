//! Decides whether files on disk may be executed with the daemon's privileges.
//!
//! The daemon runs whatever is in the extensions folder, so anyone able to
//! change that folder must already be as privileged as the daemon.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(unix)]
const GROUP_OR_OTHER_WRITE: u32 = 0o022;
#[cfg(unix)]
const STICKY: u32 = 0o1000;

/// Resolves the extensions folder and vets it and every parent directory.
#[cfg(unix)]
pub(super) fn check_root(dir: &Path) -> Result<PathBuf, String> {
    let root = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    check_owner_and_mode(&root, false)?;
    for ancestor in root.ancestors().skip(1) {
        // A sticky parent such as /tmp cannot have our entry swapped out.
        check_owner_and_mode(ancestor, true)?;
    }
    Ok(root)
}

// Windows ACLs are not inspected. Program Files is writable only by
// administrators by default, so location stands in for an ACL check.
#[cfg(windows)]
pub(super) fn check_root(dir: &Path) -> Result<PathBuf, String> {
    let root = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let trusted = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .and_then(|p| p.canonicalize().ok())
        .ok_or("cannot resolve %ProgramFiles%")?;
    if !root.starts_with(&trusted) {
        return Err(format!(
            "{} must be inside {}",
            root.display(),
            trusted.display()
        ));
    }
    Ok(root)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn check_root(_dir: &Path) -> Result<PathBuf, String> {
    Err("extensions are not supported on this platform".into())
}

/// Vets one path inside the folder. Symlinks are refused because the checks
/// above say nothing about where they lead.
pub(super) fn check_entry(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} must not be a symlink", path.display()));
    }
    #[cfg(unix)]
    check_owner_and_mode(path, false)?;
    Ok(())
}

pub(super) fn check_executable(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    #[cfg(unix)]
    if meta.permissions().mode() & 0o100 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn check_owner_and_mode(path: &Path, allow_sticky: bool) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let euid = nix::unistd::geteuid().as_raw();
    if meta.uid() != 0 && meta.uid() != euid {
        return Err(format!(
            "{} must be owned by root or the daemon user",
            path.display()
        ));
    }
    let mode = meta.permissions().mode();
    if mode & GROUP_OR_OTHER_WRITE != 0 && !(allow_sticky && mode & STICKY != 0) {
        return Err(format!(
            "{} must not be writable by group or others (mode {:o})",
            path.display(),
            mode & 0o7777
        ));
    }
    Ok(())
}
