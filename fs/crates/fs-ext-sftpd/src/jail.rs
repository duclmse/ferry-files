//! Maps client paths onto the served folder without letting them leave it.
//!
//! Clients see the folder as `/`. A path is first normalized as text, so
//! `..` can never climb above `/`. Symlinks are the other way out, so the
//! deepest existing part of the resolved path is canonicalized and must still
//! lie under the root. A link swapped in between that check and the file
//! operation is not caught; the root should not be writable by anyone who
//! could plant one outside SFTP.

use std::io;
use std::path::{Path, PathBuf};

use russh_sftp::protocol::StatusCode;

#[derive(Debug)]
pub struct Jail {
    root: PathBuf,
}

impl Jail {
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a folder", root.display()),
            ));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The client-visible absolute form of `path`: `/`-separated, without
    /// `.`, `..` or empty parts. Relative paths start at `/`.
    pub fn normalize(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split(is_separator) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                p => parts.push(p),
            }
        }
        format!("/{}", parts.join("/"))
    }

    /// The real path for client path `path`, refused if it would lead
    /// outside the root.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let virtual_path = Self::normalize(path);
        let mut real = self.root.clone();
        for part in virtual_path.split('/').filter(|p| !p.is_empty()) {
            if !allowed_part(part) {
                return Err(StatusCode::PermissionDenied);
            }
            real.push(part);
        }
        self.check_inside(&real)?;
        Ok(real)
    }

    fn check_inside(&self, real: &Path) -> Result<(), StatusCode> {
        let mut existing = real;
        loop {
            match existing.canonicalize() {
                Ok(c) if c.starts_with(&self.root) => return Ok(()),
                Ok(_) => return Err(StatusCode::PermissionDenied),
                // A dangling symlink canonicalizes to NotFound like a missing
                // file, so tell them apart before walking up.
                Err(_) if existing.symlink_metadata().is_ok() => {
                    return Err(StatusCode::PermissionDenied)
                }
                Err(_) => match existing.parent() {
                    Some(parent) if parent.starts_with(&self.root) => existing = parent,
                    _ => return Err(StatusCode::PermissionDenied),
                },
            }
        }
    }

    /// The client-visible path of a real path under the root.
    pub fn to_virtual(&self, real: &Path) -> Option<String> {
        let rest = real.strip_prefix(&self.root).ok()?;
        let parts: Vec<String> = rest
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some(format!("/{}", parts.join("/")))
    }
}

/// Windows also splits paths on `\`, so it must separate parts here too.
fn is_separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// On Windows `C:x` and `name:stream` reach other drives and NTFS streams.
fn allowed_part(part: &str) -> bool {
    let drive_or_stream = cfg!(windows) && part.contains(':');
    !drive_or_stream && !part.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_never_climbs_above_the_root() {
        assert_eq!(Jail::normalize(""), "/");
        assert_eq!(Jail::normalize("."), "/");
        assert_eq!(Jail::normalize("/a/./b//c/"), "/a/b/c");
        assert_eq!(Jail::normalize("a/../../../etc/passwd"), "/etc/passwd");
        assert_eq!(Jail::normalize("/.."), "/");
    }

    #[test]
    fn resolve_stays_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(dir.path()).unwrap();
        let root = jail.root().to_path_buf();
        assert_eq!(jail.resolve("/").unwrap(), root);
        assert_eq!(jail.resolve("../../x").unwrap(), root.join("x"));
        assert_eq!(
            jail.resolve("new/deeper/file").unwrap(),
            root.join("new/deeper/file")
        );
        assert_eq!(jail.to_virtual(&root.join("a").join("b")).unwrap(), "/a/b");
        assert_eq!(jail.to_virtual(&root).unwrap(), "/");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_root_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(dir.path()).unwrap();
        let root = jail.root();
        std::os::unix::fs::symlink(outside.path(), root.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("nope"), root.join("dangling")).unwrap();
        std::fs::create_dir(root.join("in")).unwrap();
        std::os::unix::fs::symlink(root.join("in"), root.join("alias")).unwrap();

        for path in [
            "/out",
            "/out/secret",
            "/out/new-file",
            "/out/a/b",
            "/dangling",
        ] {
            assert!(
                matches!(jail.resolve(path), Err(StatusCode::PermissionDenied)),
                "{path} must be refused"
            );
        }
        assert!(
            jail.resolve("/alias/new").is_ok(),
            "links inside the root are fine"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_separators_and_drive_parts_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(dir.path()).unwrap();
        assert_eq!(Jail::normalize(r"a\..\..\x"), "/x");
        assert!(jail.resolve("C:x").is_err());
        assert!(jail.resolve("file:stream").is_err());
    }
}
