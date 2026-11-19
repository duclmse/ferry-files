//! The few commands clients may run over an exec channel.
//!
//! There is no shell. rar2zip checks each upload with `sha256sum <path>` and
//! removes a partial one with `rm -f <path>`, so those two are answered here,
//! inside the jail, with output shaped like the coreutils tools.

use std::io::Read;

use sha2::{Digest, Sha256};

use crate::jail::Jail;

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: u32,
}

impl Output {
    fn ok(stdout: String) -> Self {
        Self {
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
            status: 0,
        }
    }

    fn fail(status: u32, message: String) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: format!("{message}\n").into_bytes(),
            status,
        }
    }
}

/// Runs `command` against `jail`. Blocks on file I/O.
pub fn run(command: &str, jail: &Jail, read_only: bool) -> Output {
    let words = match split_words(command) {
        Ok(w) => w,
        Err(e) => return Output::fail(2, e),
    };
    match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["sha256sum", paths @ ..] if !paths.is_empty() => sha256sum(paths, jail),
        ["rm", "-f", paths @ ..] if !paths.is_empty() => {
            if read_only {
                return Output::fail(1, "rm: read-only server".into());
            }
            rm_f(paths, jail)
        }
        _ => Output::fail(
            127,
            format!("{command}: not supported (only sha256sum and rm -f)"),
        ),
    }
}

fn sha256sum(paths: &[&str], jail: &Jail) -> Output {
    let mut out = String::new();
    let mut errors = Vec::new();
    for path in paths {
        match jail
            .resolve(path)
            .map_err(|_| "permission denied".to_owned())
            .and_then(|real| hash_file(&real).map_err(|e| e.to_string()))
        {
            Ok(hex) => out.push_str(&format!("{hex}  {path}\n")),
            Err(e) => errors.push(format!("sha256sum: {path}: {e}")),
        }
    }
    finish(out, errors)
}

fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buf[..n]);
    }
}

fn rm_f(paths: &[&str], jail: &Jail) -> Output {
    let mut errors = Vec::new();
    for path in paths {
        let result = jail
            .resolve(path)
            .map_err(|_| "permission denied".to_owned())
            .and_then(|real| match std::fs::remove_file(&real) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            });
        if let Err(e) = result {
            errors.push(format!("rm: {path}: {e}"));
        }
    }
    finish(String::new(), errors)
}

fn finish(stdout: String, errors: Vec<String>) -> Output {
    if errors.is_empty() {
        Output::ok(stdout)
    } else {
        Output {
            stdout: stdout.into_bytes(),
            stderr: format!("{}\n", errors.join("\n")).into_bytes(),
            status: 1,
        }
    }
}

/// POSIX shell word splitting for quotes and backslashes only. Anything a
/// shell would expand or redirect is refused rather than passed on literally,
/// so a command never means something different here than under a shell.
pub fn split_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated '".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated \"".into()),
                        },
                        Some(c @ ('$' | '`')) => return Err(format!("{c} is not supported")),
                        Some(c) => word.push(c),
                        None => return Err("unterminated \"".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("trailing \\".into()),
                }
            }
            c if "$`;|&<>(){}*?[~#".contains(c) => {
                return Err(format!("{c} is not supported"));
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_follow_shell_quoting() {
        assert_eq!(split_words("sha256sum /a").unwrap(), ["sha256sum", "/a"]);
        assert_eq!(
            split_words(r"rm -f 'it'\''s $HOME.zip'").unwrap(),
            ["rm", "-f", "it's $HOME.zip"]
        );
        assert_eq!(
            split_words(r#"x "a \"b\" \n" c\ d ''"#).unwrap(),
            ["x", r#"a "b" \n"#, "c d", ""]
        );
        for bad in [
            "a; rm -rf /",
            "a $(id)",
            "a `id`",
            "a > f",
            "a *",
            "a 'open",
            "a \"$x\"",
        ] {
            assert!(split_words(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sha256sum_and_rm_work_inside_the_jail_only() {
        let dir = tempfile::tempdir().unwrap();
        let jail = Jail::new(dir.path()).unwrap();
        std::fs::write(jail.root().join("a b.zip"), b"abc").unwrap();

        let out = run("sha256sum '/a b.zip'", &jail, false);
        assert_eq!(out.status, 0);
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  /a b.zip\n"
        );

        let missing = run("sha256sum /nope", &jail, false);
        assert_eq!(missing.status, 1);
        assert!(String::from_utf8(missing.stderr).unwrap().contains("/nope"));

        assert_eq!(run("rm -f '/a b.zip'", &jail, true).status, 1, "read-only");
        assert!(jail.root().join("a b.zip").exists());
        assert_eq!(
            run("rm -f '/a b.zip' /never-existed", &jail, false).status,
            0
        );
        assert!(!jail.root().join("a b.zip").exists());

        assert_eq!(run("cat /etc/passwd", &jail, false).status, 127);
        assert_eq!(run("rm /x", &jail, false).status, 127, "only rm -f");
        assert_eq!(run("sha256sum", &jail, false).status, 127);
    }
}
