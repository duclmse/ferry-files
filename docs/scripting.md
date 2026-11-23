# Scripting Ferry Files

Ferry Files runs [Lua 5.4](https://www.lua.org/manual/5.4/) scripts on the files
you pick: rename a batch of photos, sort downloads into folders, write a list of
what's on a phone. A script works the same on the computer, a phone or a server,
through the app, so every file it replaces or deletes is backed up first, as
with anything else you do in the app.

This guide is the app's own copy, written to the scripts folder as
`Scripting guide.md`. The app replaces it when it's updated, so keep your notes
elsewhere.

## Running a script

1. Pick files or folders in a pane, or pick nothing to run on the folder itself.
2. Right-click, then **Run script ▸** and the script's name.
3. A notice at the bottom of the pane says the script is running, with **Stop**.
   When it ends, the notice shows what the script printed, any error, and how
   many items it changed. ▶ runs it again on the same items.

The pane lists its folder again when the script changed something.

A script can also have a shortcut: in **Settings > Scripts**, click **Add shortcut** beside it and
press the keys (Esc stops, ⌫ takes the shortcut away). Pressing it runs the script on what's
picked in the active pane, or on its folder. A shortcut one of the app's commands already has is
refused; change that one under **Settings > Keyboard** first. The menu shows each script's
shortcut beside its name.

## Where scripts are

**Settings > Scripts** lists every script the app found, and where it looks:

- **The scripts folder**: the app's own, which it makes the first time with an
  example, `list sizes.lua`, and this guide. **Change…** picks another folder to
  use instead (new scripts go there), and **Use the app's** goes back. The
  folder button shows it in your file manager. A folder you pick is left as it
  is: the example and this guide stay in the app's folder.
- **Files and folders you add** with **Add scripts…** and **Add a folder…**. A
  folder's `.lua` files are taken, not those in folders inside it. × takes an
  entry off the list; the files stay.

A script is any `.lua` file. Its name is the file's name without `.lua`, and its
first line, when it's a `--` comment, is what Settings says about it.

In Settings, **New script** makes a script in the scripts folder and opens it in
the editor, and the edit button next to each script opens that one. **Reload**
looks for scripts again; the menu also looks each time it opens.

## A first script

```lua
-- Prints the name and size of each file picked.
for _, path in ipairs(ferry.items) do
  local e = ferry.stat(path)
  if e and not e.dir then
    print(e.name, e.size)
  end
end
```

## What a script gets

Everything goes through the global table `ferry`. Paths are the app's:
`/sdcard/DCIM/a.jpg` on a phone, `/C:/Users/me/a.jpg` for the computer on
Windows, `/Users/me/a.jpg` on a Mac. Use `ferry.join` to build them rather than
adding `/` by hand.

### Where the script runs

| Name           | What it is                                                             |
| -------------- | ---------------------------------------------------------------------- |
| `ferry.dir`    | The pane's folder.                                                     |
| `ferry.items`  | The paths picked, in the pane's order. Empty when nothing was picked.  |
| `ferry.device` | Where the files are: the computer's name, the phone's or the server's. |

### Looking

| Function             | What it does                                                                                                                                                                                                                                                                              |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ferry.list(dir)`    | What's in folder `dir`: a list of items (below).                                                                                                                                                                                                                                          |
| `ferry.stat(path)`   | The item at `path`, or `nil` when there's nothing there.                                                                                                                                                                                                                                  |
| `ferry.exists(path)` | `true` when there's a file or folder at `path`.                                                                                                                                                                                                                                           |
| `ferry.media(path)`  | For a picture, video or sound (by its extension), a table: `kind` (`"picture"`, `"video"` or `"sound"`), and `width` and `height` in pixels and `duration` in seconds where the file's header tells them (`nil` otherwise). `nil` for any other file. Only the start of the file is read. |

An item is a table:

| Field   | What it is                                             |
| ------- | ------------------------------------------------------ |
| `name`  | Its name, `a.jpg`.                                     |
| `path`  | Its full path.                                         |
| `dir`   | `true` for a folder.                                   |
| `size`  | Bytes (0 for a folder).                                |
| `mtime` | When it was last changed, in seconds since 1970 (UTC). |

### Reading and writing files

| Function                   | What it does                                                                                                    |
| -------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `ferry.read(path)`         | The file's contents, as a Lua string (any bytes, not only text). Up to 64 MB.                                   |
| `ferry.create(path, text)` | Makes a new file; `text` can be left out for an empty one. Fails when something is already at `path`.           |
| `ferry.write(path, text)`  | Writes the file, making it or replacing it. A file it replaces is backed up first.                              |
| `ferry.append(path, text)` | Adds `text` to the end of the file (made when it isn't there). The old version is backed up. Files up to 64 MB. |

### Folders

| Function                  | What it does                                                                                     |
| ------------------------- | ------------------------------------------------------------------------------------------------ |
| `ferry.mkdir(path)`       | Makes a folder. Fails when something is already at `path`, or the folder above isn't there.      |
| `ferry.mkdir(path, true)` | Makes the folder and any folders above it that aren't there. A folder already at `path` is fine. |

### Copying, moving, renaming and deleting

These work on files and folders alike, on the device the pane shows.

| Function                       | What it does                                                                                                                                                                                                                  |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ferry.copy(from, to)`         | Copies a file, or a folder with everything in it, to the path `to`. A folder already at `to` gets the contents added; a file already there is never replaced: the copy stops with an error, and what was copied so far stays. |
| `ferry.move(from, to)`         | Moves a file or folder to the path `to`, which must be free. On one of the computer's disks it's a single quick step. Elsewhere it's copied, then the original is deleted with backups.                                       |
| `ferry.rename(path, new_name)` | Renames in the same folder. Returns the new path.                                                                                                                                                                             |
| `ferry.remove(path)`           | Deletes a file (backed up first) or an empty folder. Returns where the backup is.                                                                                                                                             |
| `ferry.remove(path, true)`     | Deletes a folder with everything in it: each file backed up, each folder deleted once it's empty.                                                                                                                             |

Big files are fine for `copy` and `move`: they go through a file on the
computer, not memory.

### Paths

| Function                | What it does                           |
| ----------------------- | -------------------------------------- |
| `ferry.join(dir, name)` | `dir` and `name` with one `/` between. |
| `ferry.basename(path)`  | The last part: `a.jpg`.                |
| `ferry.dirname(path)`   | The folder it's in: `/sdcard/DCIM`.    |

### Output

`print(...)` adds a line to what the notice shows when the script ends, its
values separated by tabs. The last 2,000 lines are kept. Nothing shows while the
script runs.

## When something goes wrong

A function that can't do what it's asked raises an error that says why, in plain
words ("…/a.txt is already there. Not replaced."). Uncaught, it stops the
script, and the notice shows it in red. Catch it with `pcall` to carry on:

```lua
local ok, err = pcall(ferry.create, ferry.join(ferry.dir, "notes.txt"), "hello")
if not ok then
  print("skipped: " .. err)
end
```

Changes made before an error stay made. Files a script replaced or deleted are
in **Backups** (in the sidebar), where they can be put back.

## What a script can't do

So that every change goes through the app and is backed up, scripts don't get
Lua's `io`, `os`, `package` (`require`) or `debug` libraries. They have
`string`, `table`, `math`, `utf8` and `coroutine`, and the basics (`pairs`,
`ipairs`, `tostring`, `pcall` and so on).

- A script works on the device the pane shows. To copy between devices, use the
  app's own copy.
- One script runs at a time.
- A script may use up to 512 MB of memory.
- **Stop** ends a script within a moment. A copy under way finishes its current
  file first.

## Examples

### Number the photos picked

```lua
-- Renames the files picked to 001.jpg, 002.jpg… in the order they're shown.
for i, path in ipairs(ferry.items) do
  local ext = ferry.basename(path):match("%.[^.]+$") or ""
  local new = string.format("%03d%s", i, ext:lower())
  print(ferry.basename(path) .. " → " .. new)
  ferry.rename(path, new)
end
```

### Sort a folder by type

```lua
-- Moves each file in the folder into a folder named after its extension (JPG, PDF…).
for _, e in ipairs(ferry.list(ferry.dir)) do
  local ext = e.name:match("%.([^.]+)$")
  if not e.dir and ext then
    local to = ferry.join(ferry.dir, ext:upper())
    ferry.mkdir(to, true)
    local ok, err = pcall(ferry.move, e.path, ferry.join(to, e.name))
    if not ok then print(err) end
  end
end
```

### Back up the folder picked

```lua
-- Copies each folder picked next to itself, as "name (copy)".
for _, path in ipairs(ferry.items) do
  local to = path .. " (copy)"
  if ferry.exists(to) then
    print(ferry.basename(to) .. " is already there")
  else
    ferry.copy(path, to)
    print("copied " .. ferry.basename(path))
  end
end
```

### Write a list of what's here

```lua
-- Writes "contents.txt" in the folder: each item with its size, folders first.
local lines = {}
for _, e in ipairs(ferry.list(ferry.dir)) do
  if e.name ~= "contents.txt" then
    lines[#lines + 1] = (e.dir and "[folder] " or "") .. e.name .. (e.dir and "" or ("  " .. e.size))
  end
end
table.sort(lines)
ferry.write(ferry.join(ferry.dir, "contents.txt"), table.concat(lines, "\n") .. "\n")
print(#lines .. " items listed")
```

### Delete what isn't media, and small pictures

The full version of this one, with settings at its top (the smallest size kept,
whether sounds count, whether to go into folders inside, and a list-only mode),
is in the app's source as `docs/scripts/delete non-media and small media.lua`.

```lua
-- Deletes files that aren't media, and pictures and videos under 500×500.
for _, e in ipairs(ferry.list(ferry.dir)) do
  if not e.dir then
    local m = ferry.media(e.path)
    local small = m and m.width and (m.width < 500 or m.height < 500)
    if not m or small then
      ferry.remove(e.path)
      print("deleted " .. e.name)
    end
  end
end
```

### Clear out empty folders

```lua
-- Deletes the empty folders inside the folder, however deep.
local function sweep(dir)
  local left = 0
  for _, e in ipairs(ferry.list(dir)) do
    if e.dir and sweep(e.path) == 0 then
      ferry.remove(e.path)
      print("removed " .. e.path)
    else
      left = left + 1
    end
  end
  return left
end
sweep(ferry.dir)
```
