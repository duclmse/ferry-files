//! Try script: a run that changes nothing, and says what it would have changed.

use super::*;

/// A change a script asks for in a trial, where it's only told.
pub(super) enum Would {
    Write(String),
    Create(String),
    Append(String),
    Mkdir(String, bool),
    Copy(String, String),
    Move(String, String),
    Rename(String, String),
    Remove(String, bool),
    Pack(Vec<String>, String),
}

/// A trial's changes, none of them made: what would be at a path (true: a folder), and what
/// would be gone.
#[derive(Default)]
pub(super) struct Trial {
    pub(super) made: HashMap<String, bool>,
    pub(super) gone: HashSet<String>,
}

/// What a trial run tells instead of changing anything.
pub(super) struct Pretend {
    pub(super) dev: Arc<Device>,
    /// The pane's folder: paths in it are told from there.
    pub(super) dir: String,
    pub(super) t: Mutex<Trial>,
    pub(super) out: Arc<Out>,
    pub(super) changed: Changed,
}

/// Whether `path` is `base` or inside it.
pub(super) fn within(path: &str, base: &str) -> bool {
    path == base || path.starts_with(&format!("{}/", base.trim_end_matches('/')))
}

impl Pretend {
    /// What would be at `path` after the trial's changes so far: None, or whether a folder.
    pub(super) fn there(&self, t: &Trial, path: &str) -> Result<Option<bool>, String> {
        if let Some(&dir) = t.made.get(path) {
            return Ok(Some(dir));
        }
        if t.gone.iter().any(|g| within(path, g)) {
            return Ok(None);
        }
        Ok(self
            .dev
            .stat(path)
            .map_err(|e| e.to_string())?
            .map(|e| e.dir))
    }

    /// `path` as the output shows it: from the pane's folder when it's in it.
    pub(super) fn shown<'a>(&self, path: &'a str) -> &'a str {
        let base = format!("{}/", self.dir.trim_end_matches('/'));
        path.strip_prefix(&base).unwrap_or(path)
    }

    pub(super) fn take(t: &mut Trial, path: &str) {
        t.made.retain(|m, _| !within(m, path));
        t.gone.insert(path.to_string());
    }

    /// Checks `w` as the real change would, then tells it in the output instead of making it.
    pub(super) fn act(&self, w: Would) -> mlua::Result<()> {
        let mut t = self.t.lock().unwrap_or_else(|e| e.into_inner());
        let line = self.check(&mut t, w).map_err(fail)?;
        if let Some(line) = line {
            self.out.push(line);
            self.changed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    pub(super) fn check(&self, t: &mut Trial, w: Would) -> Result<Option<String>, String> {
        let missing = |p: &str| format!("No such file or folder: {p}");
        Ok(Some(match w {
            Would::Write(p) => {
                let was = self.there(t, &p)?;
                if was == Some(true) {
                    return Err(format!("{p} is a folder."));
                }
                t.made.insert(p.clone(), false);
                let verb = if was.is_some() { "replace" } else { "write" };
                format!("Would {verb} {}", self.shown(&p))
            }
            Would::Create(p) => {
                if self.there(t, &p)?.is_some() {
                    return Err(format!("{p} is already there. Not replaced."));
                }
                t.made.insert(p.clone(), false);
                format!("Would make {}", self.shown(&p))
            }
            Would::Append(p) => {
                if self.there(t, &p)? == Some(true) {
                    return Err(format!("{p} is a folder."));
                }
                t.made.insert(p.clone(), false);
                format!("Would add to {}", self.shown(&p))
            }
            Would::Mkdir(p, parents) => {
                match self.there(t, &p)? {
                    Some(true) if parents => return Ok(None),
                    Some(_) => return Err(format!("{p} is already there.")),
                    None => {}
                }
                let mut up = device::dirname(&p);
                if !parents && up != p && self.there(t, &up)?.is_none() {
                    return Err(format!("The folder {up} isn't there."));
                }
                while parents && up != p && self.there(t, &up)?.is_none() {
                    t.made.insert(up.clone(), true);
                    let next = device::dirname(&up);
                    if next == up {
                        break;
                    }
                    up = next;
                }
                t.made.insert(p.clone(), true);
                format!("Would make folder {}", self.shown(&p))
            }
            Would::Copy(a, b) => {
                let dir = self.there(t, &a)?.ok_or_else(|| missing(&a))?;
                match self.there(t, &b)? {
                    Some(false) if dir => {
                        return Err(format!(
                            "A file is already at {b}, where the folder would go."
                        ))
                    }
                    Some(_) if !dir => return Err(format!("{b} is already there. Not replaced.")),
                    _ => {}
                }
                t.made.insert(b.clone(), dir);
                format!("Would copy {} to {}", self.shown(&a), self.shown(&b))
            }
            Would::Move(a, b) => {
                if self.there(t, &b)?.is_some() {
                    return Err(format!("{b} is already there. Nothing was moved."));
                }
                let dir = self.there(t, &a)?.ok_or_else(|| missing(&a))?;
                Self::take(t, &a);
                t.made.insert(b.clone(), dir);
                format!("Would move {} to {}", self.shown(&a), self.shown(&b))
            }
            Would::Rename(p, to) => {
                let dir = self.there(t, &p)?.ok_or_else(|| missing(&p))?;
                if self.there(t, &to)?.is_some() {
                    return Err(format!("{to} is already there."));
                }
                Self::take(t, &p);
                t.made.insert(to.clone(), dir);
                format!(
                    "Would rename {} to {}",
                    self.shown(&p),
                    device::basename(&to)
                )
            }
            Would::Pack(from, to) => {
                for f in &from {
                    self.there(t, f)?.ok_or_else(|| missing(f))?;
                }
                if self.there(t, &to)?.is_some() {
                    return Err(format!("{to} is already there. Not replaced."));
                }
                t.made.insert(to.clone(), false);
                let what: Vec<&str> = from.iter().map(|f| self.shown(f)).collect();
                format!("Would pack {} into {}", what.join(", "), self.shown(&to))
            }
            Would::Remove(p, all) => {
                let dir = self.there(t, &p)?.ok_or_else(|| missing(&p))?;
                let full = dir
                    && !t.made.contains_key(&p)
                    && !self.dev.listdir(&p).map_err(|e| e.to_string())?.is_empty();
                if full && !all {
                    return Err(format!("{p} isn't empty."));
                }
                Self::take(t, &p);
                if full {
                    format!("Would delete {} and everything in it", self.shown(&p))
                } else {
                    format!("Would delete {}", self.shown(&p))
                }
            }
        }))
    }
}
