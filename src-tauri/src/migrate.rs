//! One-time moves from the app's old name, yomi-files, to Ferry Files. Runs at startup, before
//! the window (and so its web view) exists. Nothing is deleted: settings are copied, and the
//! cache folder (backups included) is moved, merging into the new one if both exist.

// Only macOS builds were ever called yomi-files.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::Path;

/// The web view's data folders in `~/Library/WebKit`, old and new: the bundled app's (named by
/// its identifier) and a development build's (named by its binary).
#[cfg(target_os = "macos")]
const WEBKIT: [(&str, &str); 2] = [("dev.yomi.files", "dev.ferryfiles.desktop"), ("yomi-files-app", "ferry-files")];

pub fn run() {
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir() {
        let webkit = home.join("Library/WebKit");
        for (old, new) in WEBKIT {
            if let Err(e) = copy_once(&webkit.join(old), &webkit.join(new)) {
                eprintln!("Couldn't copy settings from {old} to {new}: {e}");
            }
        }
        let caches = home.join("Library/Caches");
        if let Err(e) = move_or_merge(&caches.join("yomi-files"), &caches.join("ferry-files")) {
            eprintln!("Couldn't move ~/Library/Caches/yomi-files to ferry-files: {e}");
        }
    }
}

/// Copies folder `old` to `new` unless `new` is already there. Copies into a temporary name
/// first, so a copy cut short is tried again next time instead of being taken as done.
fn copy_once(old: &Path, new: &Path) -> std::io::Result<()> {
    if !old.is_dir() || new.exists() {
        return Ok(());
    }
    let part = new.with_file_name(format!(".{}.partial", new.file_name().unwrap_or_default().to_string_lossy()));
    let _ = std::fs::remove_dir_all(&part);
    copy_dir(old, &part)?;
    std::fs::rename(&part, new)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for item in std::fs::read_dir(from)? {
        let item = item?;
        let target = to.join(item.file_name());
        let kind = item.file_type()?;
        if kind.is_dir() {
            copy_dir(&item.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(item.path(), &target)?;
        }
    }
    Ok(())
}

/// Moves folder `old` to `new`. If `new` exists too, moves in what it doesn't have yet, folder by
/// folder; `index.tsv` (the backups' notes) gets the old lines it's missing. Anything that would
/// overwrite stays in `old`.
fn move_or_merge(old: &Path, new: &Path) -> std::io::Result<()> {
    if !old.is_dir() {
        return Ok(());
    }
    if !new.exists() {
        return std::fs::rename(old, new);
    }
    for item in std::fs::read_dir(old)? {
        let item = item?;
        let (from, to) = (item.path(), new.join(item.file_name()));
        if !to.exists() {
            std::fs::rename(&from, &to)?;
        } else if item.file_type()?.is_dir() && to.is_dir() {
            move_or_merge(&from, &to)?;
        } else if item.file_name() == "index.tsv" {
            let have = std::fs::read_to_string(&to)?;
            let known: std::collections::HashSet<&str> = have.lines().collect();
            let missing: String = std::fs::read_to_string(&from)?.lines().filter(|l| !l.is_empty() && !known.contains(l)).map(|l| format!("{l}\n")).collect();
            if !missing.is_empty() {
                let sep = if have.is_empty() || have.ends_with('\n') { "" } else { "\n" };
                std::fs::write(&to, format!("{have}{sep}{missing}"))?;
            }
            std::fs::remove_file(&from)?;
        }
    }
    // Only goes if it's empty now.
    let _ = std::fs::remove_dir(old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_once_and_merges_backups() {
        let root = std::env::temp_dir().join(format!("ferry-migrate-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::create_dir_all(old.join("backups")).unwrap();
        std::fs::write(old.join("backups/a.txt"), "a").unwrap();
        std::fs::write(old.join("backups/index.tsv"), "a.txt\tmac\t/a\tdelete\nsame\n").unwrap();
        std::fs::create_dir_all(old.join("thumbs")).unwrap();

        // Copying leaves the old folder, and a second copy changes nothing.
        let copy = root.join("copy");
        copy_once(&old, &copy).unwrap();
        assert!(copy.join("backups/a.txt").is_file() && old.join("backups/a.txt").is_file());
        std::fs::write(copy.join("backups/a.txt"), "changed").unwrap();
        copy_once(&old, &copy).unwrap();
        assert_eq!(std::fs::read_to_string(copy.join("backups/a.txt")).unwrap(), "changed");

        // Merging: new files move in, notes are combined, clashes stay behind.
        std::fs::create_dir_all(new.join("backups")).unwrap();
        std::fs::write(new.join("backups/index.tsv"), "same\n").unwrap();
        std::fs::write(new.join("backups/clash"), "new").unwrap();
        std::fs::write(old.join("backups/clash"), "old").unwrap();
        move_or_merge(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("backups/a.txt")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(new.join("backups/index.tsv")).unwrap(), "same\na.txt\tmac\t/a\tdelete\n");
        assert_eq!(std::fs::read_to_string(new.join("backups/clash")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old.join("backups/clash")).unwrap(), "old");
        assert!(new.join("thumbs").is_dir());

        // With no new folder, the whole thing is renamed.
        let moved = root.join("moved");
        move_or_merge(&copy, &moved).unwrap();
        assert!(!copy.exists() && moved.join("backups/a.txt").is_file());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
