//! What differs between macOS, Windows and Linux on the page: the modifier key, the names of
//! things (the Mac, Finder), how keys are written, and the address the backend serves files at.

use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq)]
pub enum Os {
    Mac,
    Windows,
    Linux,
}

pub fn os() -> Os {
    thread_local!(static OS: Os = {
        let agent = web_sys::window().and_then(|w| w.navigator().user_agent().ok()).unwrap_or_default();
        if agent.contains("Mac OS") || agent.contains("Macintosh") {
            Os::Mac
        } else if agent.contains("Windows") {
            Os::Windows
        } else {
            Os::Linux
        }
    });
    OS.with(|o| *o)
}

pub fn is_mac() -> bool {
    os() == Os::Mac
}

/// The command key: ⌘ on the Mac, Ctrl elsewhere.
pub fn cmd(meta: bool, ctrl: bool) -> bool {
    if is_mac() {
        meta
    } else {
        ctrl
    }
}

/// "Mac", "PC" or "computer".
pub fn computer() -> &'static str {
    match os() {
        Os::Mac => "Mac",
        Os::Windows => "PC",
        Os::Linux => "computer",
    }
}

/// For sentences: "the Mac", "this PC" or "this computer".
pub fn the_computer() -> &'static str {
    the_computer_on(os())
}

/// `the_computer` for a given system.
pub fn the_computer_on(os: Os) -> &'static str {
    match os {
        Os::Mac => "the Mac",
        Os::Windows => "this PC",
        Os::Linux => "this computer",
    }
}

/// "this Mac", "this PC" or "this computer".
pub fn this_computer() -> &'static str {
    match os() {
        Os::Mac => "this Mac",
        Os::Windows => "this PC",
        Os::Linux => "this computer",
    }
}

/// Where deleted files go: "Recycle Bin" on Windows, "Trash" elsewhere.
pub fn trash() -> &'static str {
    if os() == Os::Windows {
        "Recycle Bin"
    } else {
        "Trash"
    }
}

/// "sidebar" on the Mac, "navigation pane" elsewhere.
pub fn sidebar() -> &'static str {
    if is_mac() {
        "sidebar"
    } else {
        "navigation pane"
    }
}

/// How to install adb, as a sentence.
pub fn adb_install() -> &'static str {
    match os() {
        Os::Mac => "Install it with: brew install --cask android-platform-tools",
        Os::Windows => "Install it with: winget install Google.PlatformTools",
        Os::Linux => "Install it with your package manager (adb on Debian and Ubuntu, android-tools on Fedora and Arch).",
    }
}

/// "Finder", "File Explorer" or "Files".
pub fn file_manager() -> &'static str {
    match os() {
        Os::Mac => "Finder",
        Os::Windows => "File Explorer",
        Os::Linux => "Files",
    }
}

/// The menu item that opens the system's window about a file or folder.
pub fn properties() -> &'static str {
    if is_mac() {
        "Get Info"
    } else {
        "Properties"
    }
}

/// Whether the computer's `path` has a Get Info (Properties) window: not Windows' list of drives.
pub fn has_properties(path: &str) -> bool {
    !(os() == Os::Windows && path == "/")
}

/// What Space does on a file: Quick Look on the Mac; elsewhere it opens in its app.
pub fn quick_look() -> &'static str {
    if is_mac() {
        "Quick Look"
    } else {
        "Open in default app"
    }
}

/// Keys as written on the Mac ("⌘⌫", "⌘\\"), as written here: "Ctrl+Backspace" elsewhere.
pub fn keys(mac: &'static str) -> &'static str {
    if is_mac() {
        return mac;
    }
    thread_local!(static DONE: RefCell<HashMap<&'static str, &'static str>> = RefCell::new(HashMap::new()));
    DONE.with(|done| {
        *done.borrow_mut().entry(mac).or_insert_with(|| {
            let mut out = String::new();
            for c in mac.chars() {
                match c {
                    '⌘' => out.push_str("Ctrl+"),
                    '⌥' => out.push_str("Alt+"),
                    '⇧' => out.push_str("Shift+"),
                    '⌫' => out.push_str("Backspace"),
                    c => out.push(c),
                }
            }
            // A handful of fixed labels, each made once.
            Box::leak(out.into_boxed_str())
        })
    })
}

/// Where the backend serves files: its own scheme, which Windows' web view reaches over http.
pub fn served(rest: &str) -> String {
    if os() == Os::Windows {
        format!("http://ferry.localhost/{rest}")
    } else {
        format!("ferry://localhost/{rest}")
    }
}

/// A path on the computer as the app writes it ("/C:/Users" on Windows), from the system's own.
pub fn app_path(native: &str) -> String {
    if os() != Os::Windows {
        return native.to_string();
    }
    // A network share, \\nas\Photos, is "//nas/Photos" (as `platform::app_path` has it).
    if let Some(unc) = native.strip_prefix(r"\\?\UNC\").or_else(|| {
        native
            .strip_prefix(r"\\")
            .filter(|r| !r.starts_with(['?', '.']))
    }) {
        return format!("//{}", unc.replace('\\', "/").trim_end_matches('/'));
    }
    let p = native
        .strip_prefix(r"\\?\")
        .unwrap_or(native)
        .replace('\\', "/");
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        "/".into()
    } else {
        format!("/{p}")
    }
}

/// Whether `text` is a whole path as Windows writes it: "C:\Users", "C:/Users", "C:" or
/// "\\nas\Photos". Never elsewhere, where the app's paths are the system's.
pub fn is_native_path(text: &str) -> bool {
    if os() != Os::Windows {
        return false;
    }
    let b = text.as_bytes();
    let drive = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/');
    drive || text.starts_with(r"\\")
}

/// A path on the computer as the system writes it ("C:\Users" on Windows).
pub fn native_path(app: &str) -> String {
    if os() != Os::Windows {
        return app.to_string();
    }
    if let Some(unc) = app
        .strip_prefix("//")
        .filter(|p| !p.is_empty() && !p.starts_with('/'))
    {
        return format!(r"\\{}", unc.replace('/', "\\"));
    }
    let p = app.trim_start_matches('/').replace('/', "\\");
    if p.len() == 2 && p.ends_with(':') {
        format!("{p}\\")
    } else {
        p
    }
}

/// The line that takes the computer's own terminal to folder `dir` (an app path): PowerShell's
/// Set-Location on Windows, a POSIX shell's cd elsewhere.
pub fn cd_line(dir: &str) -> String {
    if os() == Os::Windows {
        format!(
            "Set-Location -LiteralPath '{}'\r",
            native_path(dir).replace('\'', "''")
        )
    } else {
        posix_cd(dir)
    }
}

/// cd to `dir` in a POSIX shell (a Mac's or Linux's, a phone's over adb, an SFTP server's).
pub fn posix_cd(dir: &str) -> String {
    format!("cd '{}'\r", dir.replace('\'', r"'\''"))
}
