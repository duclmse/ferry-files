-- Deletes files that aren't pictures, videos or sounds, and pictures and videos smaller than 500×500.
--
-- Runs on the files picked, and on what's in the folders picked; with nothing picked, on the
-- files in the pane's folder. Each file deleted is backed up first (see Backups in the sidebar),
-- so it can be put back.
--
-- "Smaller than 500×500" means narrower or shorter than 500 pixels: a 1200×300 panorama goes.
-- A picture or video whose size can't be read from its file (some formats, a damaged file) is
-- kept, and listed as such. Change the settings below to suit.

local MIN_WIDTH = 500
local MIN_HEIGHT = 500
-- Sounds count as media, so they're kept. Set to false to delete them too.
local KEEP_SOUNDS = true
-- Go into the folders inside the folders as well.
local INTO_SUBFOLDERS = false
-- true: only list what would be deleted, and delete nothing.
local LIST_ONLY = false

local deleted, kept, unknown, failed = 0, 0, 0, 0

-- What to do with one file: why it goes, or nil to keep it.
local function why(path)
  local m = ferry.media(path)
  if not m then
    return "not a picture, video or sound"
  end
  if m.kind == "sound" then
    return (not KEEP_SOUNDS) and "a sound" or nil
  end
  if not m.width or not m.height then
    unknown = unknown + 1
    print("kept (size not known): " .. ferry.basename(path))
    return nil
  end
  if m.width < MIN_WIDTH or m.height < MIN_HEIGHT then
    return string.format("%d×%d", m.width, m.height)
  end
  return nil
end

local function file(path)
  local reason = why(path)
  if not reason then
    kept = kept + 1
    return
  end
  if LIST_ONLY then
    print("would delete " .. ferry.basename(path) .. " (" .. reason .. ")")
    deleted = deleted + 1
    return
  end
  local ok, err = pcall(ferry.remove, path)
  if ok then
    print("deleted " .. ferry.basename(path) .. " (" .. reason .. ")")
    deleted = deleted + 1
  else
    print("couldn't delete " .. ferry.basename(path) .. ": " .. tostring(err))
    failed = failed + 1
  end
end

local function folder(dir, deep)
  for _, e in ipairs(ferry.list(dir)) do
    if not e.dir then
      file(e.path)
    elseif deep then
      folder(e.path, true)
    end
  end
end

if #ferry.items == 0 then
  folder(ferry.dir, INTO_SUBFOLDERS)
else
  for _, path in ipairs(ferry.items) do
    local e = ferry.stat(path)
    if e and e.dir then
      folder(path, INTO_SUBFOLDERS)
    elseif e then
      file(path)
    end
  end
end

print(string.format("%s %d, kept %d%s%s.",
  LIST_ONLY and "Would delete" or "Deleted", deleted, kept,
  unknown > 0 and string.format(" (%d of unknown size)", unknown) or "",
  failed > 0 and string.format(", couldn't delete %d", failed) or ""))
