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
3. A notice at the bottom of the pane says the script is running, with **Stop**,
   and shows what it prints as it goes and how many items it has changed. When
   it ends, the notice shows what the script printed, any error, and how many
   items it changed. ▶ runs it again on the same items.

The pane lists its folder again when the script changed something.

### Trying a script first

The eye button at the end of each script's row in **Run script ▸** tries it:
it runs the script without changing anything. Each change it asks for (writing, making, copying, moving, renaming or
deleting) is checked as it would be, then becomes a line of its output instead:
`Would move a.jpg to Grayscale/a.jpg`. A change that would fail fails the same
way. When it ends, ▶ runs it for real.

In a trial, `ferry.exists` and `ferry.stat` answer as if the changes so far had
been made: a folder the script would have made is there, and a file it would
have moved isn't. Other functions see the files as they are: `ferry.list` doesn't
show a file the trial would have made, and `ferry.read` reads a file as it was.
`ferry.trial` is `true` in a trial, so a script can say "would move" instead of
"moved".

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
| `ferry.trial`  | `true` when tried (the eye button in **Run script ▸**): nothing changes. |

### Looking

| Function             | What it does                                                                                                                                                                                                                                                                              |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ferry.list(dir)`    | What's in folder `dir`: a list of items (below).                                                                                                                                                                                                                                          |
| `ferry.stat(path)`   | The item at `path`, or `nil` when there's nothing there.                                                                                                                                                                                                                                  |
| `ferry.exists(path)` | `true` when there's a file or folder at `path`.                                                                                                                                                                                                                                           |
| `ferry.media(path)`  | For a picture, video or sound (by its extension), a table: `kind` (`"picture"`, `"video"` or `"sound"`), and `width` and `height` in pixels and `duration` in seconds where the file's header tells them (`nil` otherwise). `nil` for any other file. Only the start of the file is read. |
| `ferry.colour(path)` | How colourful a picture is: a table with `share`, how much of it is in colour (0 to 1), and `peak`, the same for the most colourful of its 16 parts (a 4 × 4 grid). `nil` for a file that isn't a picture, or one it can't read (only JPEG, PNG, GIF, WebP and BMP can be). The whole file is read, up to 64 MB. `ferry.color` is the same. |

`ferry.colour` doesn't count faint colour (the fringes a JPEG leaves around black lines) or a pale tint over the whole picture (yellowed paper). A gray manga page has `share` and `peak` near 0; one with a colour panel in a corner has a small `share` and a high `peak`; a colour page has both high:

```lua
for _, e in ipairs(ferry.list(ferry.dir)) do
  local c = ferry.colour(e.path)
  if c then
    local what = c.share > 0.3 and "colour" or c.peak > 0.3 and "colour patch" or "gray"
    print(e.name, what)
  end
end
```

`docs/scripts/move grayscale pictures.lua` in the app's source uses it to move
the grayscale pictures in a folder, and in the folders inside it down to three
levels, into a new folder.

An item is a table:

| Field   | What it is                                             |
| ------- | ------------------------------------------------------ |
| `name`  | Its name, `a.jpg`.                                     |
| `path`  | Its full path.                                         |
| `dir`   | `true` for a folder.                                   |
| `size`  | Bytes (0 for a folder).                                |
| `mtime` | When it was last changed, in seconds since 1970 (UTC). |

### Inside a ZIP

A ZIP (or CBZ, EPUB, APK, JAR…) can be read without unpacking it. Only the parts asked for are
read, never the whole ZIP, so these are quick on a phone or a server too. Nothing in the ZIP is
changed: to change a ZIP, `ferry.read` it and `ferry.write` a new one.

| Function                                | What it does                                                                                                                                                                                                                                                       |
| --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ferry.zip(path)`                       | What's packed in the ZIP at `path`: a list of tables with `name` (its full name in the ZIP, `ch1/003.jpg`; folders end in `/`), `size` (bytes, unpacked), `dir`, and `stored` (`true` when packed without compression). Then how many items it has in all: the list has the first 5000. |
| `ferry.unzip(path, name)`               | The file `name` packed in the ZIP, unpacked, as a Lua string, and its size. Up to 64 MB.                                                                                                                                                                            |
| `ferry.unzip(path, name, start, count)` | `count` bytes from byte `start` (0 is the first) of the file packed in the ZIP, and its size. `count` can be left out for the rest of it; less comes back near the end, and `""` past it. A file packed without compression is read in place; a compressed one is unpacked up to there. Up to 64 MB at a time. |

```lua
-- The CBZs with fewer than 10 pictures in them.
for _, e in ipairs(ferry.list(ferry.dir)) do
  if e.name:lower():match("%.cbz$") then
    local items, total = ferry.zip(e.path)
    local pics = 0
    for _, i in ipairs(items) do
      local ext = i.name:lower():match("%.(%w+)$")
      if ext == "jpg" or ext == "jpeg" or ext == "png" or ext == "webp" then pics = pics + 1 end
    end
    if pics < 10 then print(e.name, pics) end
  end
end

-- An EPUB's mimetype, and the first 4 bytes of a picture packed in a ZIP.
print(ferry.unzip(ferry.items[1], "mimetype"))
print(ferry.unzip(ferry.items[2], "pages/001.png", 0, 4) == "\137PNG")
```

`ferry.media` and `ferry.colour` take paths on the device, not names in a ZIP.

### Reading and writing files

| Function                   | What it does                                                                                                    |
| -------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `ferry.read(path)`         | The file's contents, as a Lua string (any bytes, not only text). Up to 64 MB.                                   |
| `ferry.create(path, text)` | Makes a new file; `text` can be left out for an empty one. Fails when something is already at `path`.           |
| `ferry.write(path, text)`  | Writes the file, making it or replacing it. A file it replaces is backed up first.                              |
| `ferry.append(path, text)` | Adds `text` to the end of the file (made when it isn't there). The old version is backed up. Files up to 64 MB. |

### Config files

These turn the text of a config file into Lua tables, to read settings from it. They only read:
to change a config, write its text with `ferry.write`.

| Function           | What it does                                                                                                                                                                         |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ferry.json(text)` | JSON: objects become tables with keys, arrays become lists (from 1), numbers become Lua integers when they're whole.                                                                 |
| `ferry.toml(text)` | TOML: tables and `[[arrays of tables]]` become tables and lists. Dates and times stay text, as written (`"1979-05-27T07:32:00Z"`).                                                      |
| `ferry.yaml(text)` | YAML, as YAML 1.2 reads it (see below). A file with several documents split by `---` gives each one: `local a, b = ferry.yaml(text)`.                                                  |
| `ferry.xml(text)`  | XML: the root element as a table, `{tag = "name", attr = {...}, ...}`, with what's inside it at 1, 2, …: elements as tables like it, and text as strings. Comments and spaces between tags are left out. |
| `ferry.null`       | What a `null` inside a list or map becomes, so the entry isn't lost: `if v == ferry.null then`. A file that's only `null` gives `nil`.                                                  |

```lua
local cfg = ferry.json(ferry.read(ferry.join(ferry.dir, "settings.json")))
print(cfg.theme, #cfg.recent)

local compose = ferry.yaml(ferry.read(ferry.join(ferry.dir, "docker-compose.yml")))
for name, s in pairs(compose.services) do print(name, s.image) end

-- The title in an EPUB, from its OPF.
local opf = ferry.xml(ferry.unzip(ferry.items[1], "OEBPS/content.opf"))
for _, el in ipairs(opf) do
  if el.tag == "metadata" then
    for _, m in ipairs(el) do
      if m.tag == "dc:title" then print(m[1]) end
    end
  end
end
```

YAML is read the way docker-compose files, CI workflows and app settings are written: maps and
lists by indentation and in brackets (`[a, b]`, `{x: 1}`), plain, `'single'` and `"double"`
quoted text, `|` and `>` text over several lines, and comments. Plain values that are numbers,
`true`/`false` or `null` (or `~`) become those, and the rest stay text, so `yes`, `no` and `on`
are text, as in YAML 1.2. Quoted values are always text, and keys are always text. Anchors and
aliases (`&a`, `*a`), tags (`!!str`) and `? ` keys aren't read: a file with them gives an error
naming the line, as does a mistake in a file.

An error says which line: `YAML line 4: the key "port" is there twice`.

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
values separated by tabs. The last 2,000 lines are kept. They show in the notice
as the script runs, a few times a second.

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
whether sounds count, whether to go into folders inside, and a list-only mode,
which trying it turns on),
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
