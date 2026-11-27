-- Renames each CBZ to "[author] title.cbz", from the info.yaml packed in it.
--
-- It works on the CBZs picked, or every CBZ in the pane's folder when none are. The info.yaml
-- can be anywhere in the CBZ; its keys are matched whatever their case. The author is "author"
-- (or "authors", "artist", "writer"), and several are joined with ", "; with none, the name is
-- just the title. Characters a file name can't hold become "_", and ":" becomes " -".
--
-- A CBZ with no info.yaml, or no title in it, stays as it is, and so does one whose new name is
-- already taken. Each is listed. Trying it (the eye button at the end of its row in
-- Run script ▸) lists the new names, and renames nothing.

local AUTHOR_KEYS = { "author", "authors", "artist", "artists", "writer", "writers" }
local TITLE_KEYS = { "title", "name", "series" }

-- The value of the first of `keys` in table `t`, whatever the case of its keys.
local function field(t, keys)
  if type(t) ~= "table" then return nil end
  local lower = {}
  for k, v in pairs(t) do
    if type(k) == "string" then lower[k:lower()] = v end
  end
  for _, k in ipairs(keys) do
    local v = lower[k]
    if v ~= nil and v ~= ferry.null then return v end
  end
  return nil
end

-- A value as text: a list's items joined with ", ", a map's names (as some tools write
-- authors: {name: …}) the same way.
local function text(v)
  if v == nil or v == ferry.null then return nil end
  if type(v) == "table" then
    local parts = {}
    for _, x in ipairs(v) do
      local s = text(type(x) == "table" and field(x, { "name" }) or x)
      if s and s ~= "" then parts[#parts + 1] = s end
    end
    if #parts == 0 then
      local s = field(v, { "name" })
      return s and text(s) or nil
    end
    return table.concat(parts, ", ")
  end
  local s = tostring(v):gsub("%s+", " "):match("^%s*(.-)%s*$")
  return s ~= "" and s or nil
end

-- `s` as a file name: no / \ : * ? " < > |, no spaces or dots at the ends.
local function clean(s)
  s = s:gsub(":", " -"):gsub('[/\\%*%?"<>|%c]', "_"):gsub("%s+", " ")
  return (s:match("^%s*(.-)[%s%.]*$"))
end

-- The info.yaml in the CBZ at `path`, read; or nil and why not.
local function info(path)
  local items = ferry.zip(path)
  local name
  for _, e in ipairs(items) do
    local base = e.name:match("([^/]+)$")
    if not e.dir and base and base:lower() == "info.yaml" then
      -- The one nearest the top.
      if not name or select(2, e.name:gsub("/", "")) < select(2, name:gsub("/", "")) then
        name = e.name
      end
    end
  end
  if not name then return nil, "no info.yaml in it" end
  local ok, doc = pcall(ferry.yaml, (ferry.unzip(path, name)))
  if not ok then return nil, tostring(doc) end
  return doc
end

local cbzs = {}
if #ferry.items == 0 then
  for _, e in ipairs(ferry.list(ferry.dir)) do
    if not e.dir and e.name:lower():match("%.cbz$") then cbzs[#cbzs + 1] = e.path end
  end
else
  for _, path in ipairs(ferry.items) do
    if path:lower():match("%.cbz$") then cbzs[#cbzs + 1] = path end
  end
end

local renamed, kept = 0, 0
for _, path in ipairs(cbzs) do
  local name = ferry.basename(path)
  local ok, doc, why = pcall(info, path)
  if not ok then doc, why = nil, tostring(doc) end
  local title = doc and text(field(doc, TITLE_KEYS))
  if not title then
    print(name .. ": kept, " .. (why or "no title in its info.yaml"))
    kept = kept + 1
  else
    local author = text(field(doc, AUTHOR_KEYS))
    local new = clean(author and ("[" .. author .. "] " .. title) or title) .. ".cbz"
    if new == name then
      -- Already named so.
    elseif ferry.exists(ferry.join(ferry.dirname(path), new)) then
      print(name .. ": kept, " .. new .. " is already there")
      kept = kept + 1
    else
      local done, err = pcall(ferry.rename, path, new)
      if done then
        print(name .. " → " .. new)
        renamed = renamed + 1
      else
        print(name .. ": kept, " .. tostring(err))
        kept = kept + 1
      end
    end
  end
end

print(string.format("%s %d of %d %s%s.", ferry.trial and "Would rename" or "Renamed", renamed, #cbzs,
  #cbzs == 1 and "CBZ" or "CBZs", kept > 0 and string.format(", %d kept as they are", kept) or ""))
