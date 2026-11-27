//! The `ferry` table a script gets: what it can read, and the changes it can ask for.

use super::*;

/// An entry as a table: { name, path, dir, size, mtime }.
pub(super) fn entry(lua: &Lua, e: &Entry) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", e.name.as_str())?;
    t.set("path", e.path.as_str())?;
    t.set("dir", e.dir)?;
    t.set("size", e.size)?;
    t.set("mtime", e.mtime)?;
    Ok(t)
}

/// What kind of media a file is, by its extension: "picture", "video" or "sound".
pub(super) fn media_kind(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase())?;
    const PICTURES: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "heif", "avif", "tif", "tiff", "svg",
    ];
    const VIDEOS: &[&str] = &[
        "mp4", "m4v", "mov", "3gp", "mkv", "webm", "avi", "wmv", "mpg", "mpeg", "ts",
    ];
    const SOUNDS: &[&str] = &[
        "mp3", "m4a", "aac", "wav", "flac", "ogg", "oga", "opus", "wma",
    ];
    [("picture", PICTURES), ("video", VIDEOS), ("sound", SOUNDS)]
        .into_iter()
        .find(|(_, xs)| xs.contains(&ext.as_str()))
        .map(|(k, _)| k)
}

/// The file at `path`, for reading as a ZIP: an error for the script when it's a folder or isn't
/// there.
pub(super) fn zip_file(d: &Device, path: &str) -> mlua::Result<Entry> {
    let e = d
        .stat(path)
        .map_err(fail)?
        .ok_or_else(|| fail(format!("No such file: {path}")))?;
    if e.dir {
        return Err(fail(format!("{path} is a folder, not a ZIP.")));
    }
    Ok(e)
}

/// A device error as the script's error, which it can catch with pcall.
pub(super) fn fail(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

/// The `ferry` table.
pub(super) fn api(
    lua: &Lua,
    dev: Arc<Device>,
    dir: &str,
    items: &[String],
    changed: &Changed,
    stop: &'static AtomicBool,
    pretend: Option<Arc<Pretend>>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("dir", dir)?;
    t.set(
        "items",
        lua.create_sequence_from(items.iter().map(String::as_str))?,
    )?;
    t.set("device", dev.label())?;
    t.set("trial", pretend.is_some())?;
    let d = dev.clone();
    t.set(
        "list",
        func(lua, move |lua, dir: String| {
            let es = d.listdir(&dir).map_err(fail)?;
            lua.create_sequence_from(
                es.iter()
                    .map(|e| entry(lua, e))
                    .collect::<mlua::Result<Vec<_>>>()?,
            )
        })?,
    )?;
    let (d, pt) = (dev.clone(), pretend.clone());
    t.set(
        "stat",
        func(lua, move |lua, path: String| {
            if let Some(p) = &pt {
                // Gone in the trial; one it made has no entry to give.
                let t = p.t.lock().unwrap_or_else(|e| e.into_inner());
                if p.there(&t, &path).map_err(fail)?.is_none() {
                    return Ok(Value::Nil);
                }
            }
            match d.stat(&path).map_err(fail)? {
                Some(e) => Ok(Value::Table(entry(lua, &e)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    let d = dev.clone();
    t.set(
        "read",
        func(lua, move |lua, path: String| {
            let e = d
                .stat(&path)
                .map_err(fail)?
                .ok_or_else(|| fail(format!("No such file: {path}")))?;
            if e.dir {
                return Err(fail(format!("{path} is a folder.")));
            }
            if e.size > READ_MAX {
                return Err(fail(format!(
                    "{path} is bigger than {} MB, which is as much as ferry.read reads.",
                    READ_MAX / 1024 / 1024
                )));
            }
            let (data, _) = d.read(&path).map_err(fail)?;
            lua.create_string(&data)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "write",
        func(lua, move |_, (path, data): (String, mlua::LuaString)| {
            if let Some(p) = &pt {
                return p.act(Would::Write(path));
            }
            d.write(&path, &data.as_bytes(), None, false)
                .map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    let d = dev.clone();
    t.set(
        "media",
        func(lua, move |lua, path: String| {
            let name = device::basename(&path).to_string();
            let Some(kind) = media_kind(&name) else {
                return Ok(Value::Nil);
            };
            let m = lua.create_table()?;
            m.set("kind", kind)?;
            // Only the header is read; a size that can't be told from it is left out.
            if let Ok(i) = crate::preview::media::info(&name, &mut |at, len| {
                d.read_range(&path, at, len).map_err(|e| e.to_string())
            }) {
                m.set("width", i.width)?;
                m.set("height", i.height)?;
                m.set("duration", i.duration)?;
            }
            Ok(Value::Table(m))
        })?,
    )?;
    let d = dev.clone();
    let colour = func(lua, move |lua, path: String| {
        if media_kind(device::basename(&path)) != Some("picture") {
            return Ok(Value::Nil);
        }
        let e = d
            .stat(&path)
            .map_err(fail)?
            .ok_or_else(|| fail(format!("No such file: {path}")))?;
        if e.size > READ_MAX {
            return Err(fail(format!(
                "{path} is bigger than {} MB, which is as much as ferry.colour reads.",
                READ_MAX / 1024 / 1024
            )));
        }
        let (data, _) = d.read(&path).map_err(fail)?;
        // Decoding and shrinking a big photo takes a moment; the script waits, as for read.
        let Some(c) = crate::preview::colour::colour(&data) else {
            return Ok(Value::Nil);
        };
        let m = lua.create_table()?;
        m.set("share", c.share)?;
        m.set("peak", c.peak)?;
        Ok(Value::Table(m))
    })?;
    t.set("colour", colour.clone())?;
    t.set("color", colour)?;
    // A ZIP's directory and the files packed in it, read a range at a time: never the whole ZIP.
    let d = dev.clone();
    t.set(
        "zip",
        func(lua, move |lua, path: String| {
            let e = zip_file(&d, &path)?;
            let l = crate::preview::archive::list(e.size, &mut |at, len| {
                stopped(stop)?;
                d.read_range(&path, at, len).map_err(|e| e.to_string())
            })
            .map_err(|e| fail(format!("{path}: {e}")))?;
            let items = lua.create_table()?;
            for (i, p) in l.items.iter().enumerate() {
                let r = lua.create_table()?;
                r.set("name", p.name.as_str())?;
                r.set("size", p.size)?;
                r.set("dir", p.dir)?;
                r.set("stored", p.stored)?;
                items.raw_set(i + 1, r)?;
            }
            Ok((items, l.total))
        })?,
    )?;
    let d = dev.clone();
    t.set(
        "unzip",
        func(
            lua,
            move |lua, (path, name, start, count): (String, String, Option<u64>, Option<u64>)| {
                let e = zip_file(&d, &path)?;
                let name = name.trim_start_matches('/');
                let (data, size) = crate::preview::archive::unpack_part(
                    e.size,
                    &mut |at, len| {
                        stopped(stop)?;
                        d.read_range(&path, at, len).map_err(|e| e.to_string())
                    },
                    name,
                    start.unwrap_or(0),
                    count.unwrap_or(u64::MAX),
                    READ_MAX,
                )
                .map_err(|e| fail(format!("{path}: {e}")))?;
                Ok((lua.create_string(&data)?, size))
            },
        )?,
    )?;
    let (d, pt) = (dev.clone(), pretend.clone());
    t.set(
        "exists",
        func(lua, move |_, path: String| {
            if let Some(p) = &pt {
                let t = p.t.lock().unwrap_or_else(|e| e.into_inner());
                return Ok(p.there(&t, &path).map_err(fail)?.is_some());
            }
            Ok(d.stat(&path).map_err(fail)?.is_some())
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "create",
        func(
            lua,
            move |_, (path, data): (String, Option<mlua::LuaString>)| {
                if let Some(p) = &pt {
                    return p.act(Would::Create(path));
                }
                let bytes = data.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                d.write(&path, &bytes, None, true).map_err(fail)?;
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "append",
        func(lua, move |_, (path, data): (String, mlua::LuaString)| {
            if let Some(p) = &pt {
                return p.act(Would::Append(path));
            }
            let mut now = match d.stat(&path).map_err(fail)? {
                Some(e) if e.dir => return Err(fail(format!("{path} is a folder."))),
                Some(e) if e.size > READ_MAX => {
                    return Err(fail(format!(
                        "{path} is bigger than {} MB, too big to add to.",
                        READ_MAX / 1024 / 1024
                    )))
                }
                Some(_) => d.read(&path).map_err(fail)?.0,
                None => vec![],
            };
            now.extend_from_slice(&data.as_bytes());
            d.write(&path, &now, None, false).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "mkdir",
        func(lua, move |_, (path, parents): (String, Option<bool>)| {
            if let Some(p) = &pt {
                return p.act(Would::Mkdir(path, parents.unwrap_or(false)));
            }
            make_dirs(&d, &path, parents.unwrap_or(false), &c).map_err(fail)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "copy",
        func(lua, move |_, (from, to): (String, String)| {
            if let Some(p) = &pt {
                return p.act(Would::Copy(from, to));
            }
            copy_tree(&d, &from, &to, stop, &c).map_err(fail)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "move",
        func(lua, move |_, (from, to): (String, String)| {
            if let Some(p) = &pt {
                return p.act(Would::Move(from, to));
            }
            move_tree(&d, &from, &to, stop, &c).map_err(fail)
        })?,
    )?;
    let (d, c, pt) = (dev.clone(), changed.clone(), pretend.clone());
    t.set(
        "rename",
        func(lua, move |_, (path, new_name): (String, String)| {
            if let Some(p) = &pt {
                let to = device::join(&device::dirname(&path), &new_name);
                p.act(Would::Rename(path, to.clone()))?;
                return Ok(to);
            }
            let to = d.rename(&path, &new_name).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(to)
        })?,
    )?;
    let (d, c, pt) = (dev, changed.clone(), pretend);
    t.set(
        "remove",
        func(lua, move |_, (path, all): (String, Option<bool>)| {
            if let Some(p) = &pt {
                p.act(Would::Remove(path, all.unwrap_or(false)))?;
                return Ok(None);
            }
            if all.unwrap_or(false) {
                return remove_tree(&d, &path, stop, &c).map(|_| None).map_err(fail);
            }
            let backup = d.remove(&path).map_err(fail)?;
            c.fetch_add(1, Ordering::Relaxed);
            Ok(backup)
        })?,
    )?;
    t.set(
        "join",
        func(lua, |_, (dir, name): (String, String)| {
            Ok(device::join(&dir, &name))
        })?,
    )?;
    t.set(
        "basename",
        func(lua, |_, path: String| {
            Ok(device::basename(&path).to_string())
        })?,
    )?;
    t.set(
        "dirname",
        func(lua, |_, path: String| Ok(device::dirname(&path)))?,
    )?;
    // Config files: text in, tables out. A null inside is ferry.null.
    t.set("null", config::null())?;
    t.set(
        "json",
        func(lua, |lua, text: mlua::LuaString| {
            let d = config::json(&text.to_str()?).map_err(|e| fail(format!("JSON {e}")))?;
            config::to_lua(lua, d)
        })?,
    )?;
    t.set(
        "toml",
        func(lua, |lua, text: mlua::LuaString| {
            let d = config::toml(&text.to_str()?).map_err(|e| fail(format!("TOML {e}")))?;
            config::to_lua(lua, d)
        })?,
    )?;
    t.set(
        "yaml",
        func(lua, |lua, text: mlua::LuaString| {
            let docs = config::yaml(&text.to_str()?).map_err(|e| fail(format!("YAML {e}")))?;
            docs.into_iter()
                .map(|d| config::to_lua(lua, d))
                .collect::<mlua::Result<mlua::MultiValue>>()
        })?,
    )?;
    t.set(
        "xml",
        func(lua, |lua, text: mlua::LuaString| {
            let d = config::xml(&text.to_str()?).map_err(|e| fail(format!("XML {e}")))?;
            config::xml_lua(lua, d)
        })?,
    )?;
    // Failures come back as (false, message), which LIFT turns into errors that are plain
    // strings, raised where the script called: printable, and joinable with `..`.
    lua.load(LIFT).set_name("ferry").call(t)
}

/// Wraps each function of the table it's given so that (true, ...) returns `...` and
/// (false, message) raises `message` at the caller's line.
pub(super) const LIFT: &str = r#"
local raw = ...
local t = {}
for k, v in pairs(raw) do
  if type(v) == "function" then
    t[k] = function(...)
      local r = table.pack(v(...))
      if r[1] then return table.unpack(r, 2, r.n) end
      error(r[2], 2)
    end
  else
    t[k] = v
  end
end
return t
"#;

/// A function for `ferry`: its result as (true, value), or a failure (a runtime error, from
/// `fail`) as (false, message), for LIFT.
pub(super) fn func<A: mlua::FromLuaMulti, R: mlua::IntoLuaMulti>(
    lua: &Lua,
    f: impl Fn(&Lua, A) -> mlua::Result<R> + 'static,
) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, a: A| {
        let mut out = match f(lua, a) {
            Ok(r) => r.into_lua_multi(lua)?,
            Err(mlua::Error::RuntimeError(m)) => {
                return (false, lua.create_string(&m)?).into_lua_multi(lua)
            }
            Err(e) => return Err(e),
        };
        out.push_front(Value::Boolean(true));
        Ok(out)
    })
}
