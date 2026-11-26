-- Moves the grayscale pictures in the pane's folder, and in the folders inside it down to three
-- levels below, into a new folder "Grayscale" in the pane's folder.
--
-- With folders picked, it looks in those instead (and the new folder is made in the pane's
-- folder all the same). Files picked are looked at themselves.
--
-- Inside "Grayscale" each picture keeps the folders it was in, so "Ch 2/003.jpg" goes to
-- "Grayscale/Ch 2/003.jpg" and two pictures with the same name don't meet. A picture whose new
-- path is already taken stays where it is, and is listed.
--
-- A picture is grayscale when no part of it is in colour (ferry.colour's peak): a gray manga
-- page with one colour panel stays. Pictures ferry.colour can't read (HEIC, AVIF, JPEG XL,
-- camera raw) stay, and are counted.

-- The new folder's name, in the pane's folder.
local TO = "Grayscale"
-- How many levels of folders below to look in: 0 is the folder alone.
local DEPTH = 3
-- The most of any part of a picture (1/16 of it) that may be in colour for it still to count
-- as grayscale. Raise it a little if scans with a coloured speck are kept.
local MAX_PEAK = 0.02
-- true: only list what would be moved, and move nothing.
local LIST_ONLY = false

local to = ferry.join(ferry.dir, TO)
local found = {}
local unreadable = 0

local function file(path, rel)
  local c = ferry.colour(path)
  if c == nil then
    local m = ferry.media(path)
    if m and m.kind == "picture" then
      unreadable = unreadable + 1
    end
  elseif c.peak <= MAX_PEAK then
    found[#found + 1] = { path = path, rel = rel }
  end
end

local function folder(dir, rel, depth)
  for _, e in ipairs(ferry.list(dir)) do
    local r = rel == "" and e.name or (rel .. "/" .. e.name)
    if not e.dir then
      file(e.path, r)
    elseif depth > 0 and e.path ~= to then
      folder(e.path, r, depth - 1)
    end
  end
end

-- Everything is looked at first, so what's moved isn't come across again.
if #ferry.items == 0 then
  folder(ferry.dir, "", DEPTH)
else
  for _, path in ipairs(ferry.items) do
    local e = ferry.stat(path)
    if e and e.dir and path ~= to then
      folder(path, e.name, DEPTH)
    elseif e and not e.dir then
      file(path, e.name)
    end
  end
end

local moved, failed = 0, 0
for _, f in ipairs(found) do
  local dest = ferry.join(to, f.rel)
  if LIST_ONLY then
    print("would move " .. f.rel)
    moved = moved + 1
  else
    local ok, err = pcall(function()
      ferry.mkdir(ferry.dirname(dest), true)
      ferry.move(f.path, dest)
    end)
    if ok then
      moved = moved + 1
    else
      print("couldn't move " .. f.rel .. ": " .. tostring(err))
      failed = failed + 1
    end
  end
end

print(string.format("%s %d grayscale %s to %s%s%s.",
  LIST_ONLY and "Would move" or "Moved", moved, moved == 1 and "picture" or "pictures", TO,
  failed > 0 and string.format(", couldn't move %d", failed) or "",
  unreadable > 0 and string.format(", %d pictures can't be read and stayed", unreadable) or ""))
