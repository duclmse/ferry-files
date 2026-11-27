-- Packs each folder picked into a ZIP next to it, then deletes the folder once its ZIP checks out.
--
-- Pick one or more folders, then Run script ▸ zip folders. It asks once before deleting
-- anything: say No to keep the folders and only make the ZIPs. A ZIP already there isn't
-- replaced: it asks for another name, and Cancel skips that folder. A folder is deleted only
-- when its ZIP has as many files in it as were packed; its files are backed up first, as with
-- any delete, so they can be put back from Backups.

local folders = {}
for _, path in ipairs(ferry.items) do
  local e = ferry.stat(path)
  if e and e.dir then folders[#folders + 1] = e end
end
if #folders == 0 then
  print("Pick the folders to pack first: nothing picked is a folder.")
  return
end

local what = #folders == 1 and folders[1].name or (#folders .. " folders")
local delete = ferry.confirm("Delete " .. what .. " once packed? Files are backed up first. No keeps them and only makes the ZIPs.")

-- The ZIP's path for folder `e`: name.zip, or what the user types when that's taken.
local function zip_for(e)
  local to = ferry.join(ferry.dirname(e.path), e.name .. ".zip")
  while ferry.exists(to) do
    local name = ferry.ask(ferry.basename(to) .. " is already there. Name the ZIP for " .. e.name .. " (Cancel skips it):",
      e.name .. " (2).zip")
    if not name or name == "" then return nil end
    if not name:lower():match("%.zip$") then name = name .. ".zip" end
    to = ferry.join(ferry.dirname(e.path), name)
  end
  return to
end

-- How many files (not folders) are in the ZIP at `path`.
local function files_in(path)
  local items, total = ferry.zip(path)
  local n = 0
  for _, i in ipairs(items) do
    if not i.dir then n = n + 1 end
  end
  -- Only the first 5000 are listed: count the folders in the rest as files.
  return n + (total - #items)
end

local packed, deleted, kept = 0, 0, 0
for _, e in ipairs(folders) do
  local to = zip_for(e)
  if not to then
    print("skipped " .. e.name)
    kept = kept + 1
  else
    local ok, n = pcall(ferry.pack, e.path, to)
    if not ok then
      print("couldn't pack " .. e.name .. ": " .. n)
      kept = kept + 1
    elseif ferry.trial then
      -- Nothing was made, so there's no ZIP to check.
      packed = packed + 1
      if delete then ferry.remove(e.path, true) end
    else
      packed = packed + 1
      local there = files_in(to)
      print("packed " .. e.name .. " into " .. ferry.basename(to) .. " (" .. n .. " files)")
      if not delete then
        kept = kept + 1
      elseif there ~= n then
        print("kept " .. e.name .. ": the ZIP has " .. there .. " files, not " .. n)
        kept = kept + 1
      else
        local ok, err = pcall(ferry.remove, e.path, true)
        if ok then
          deleted = deleted + 1
          print("deleted " .. e.name)
        else
          print("couldn't delete " .. e.name .. ": " .. err)
          kept = kept + 1
        end
      end
    end
  end
end

if ferry.trial then
  print("Would pack " .. packed .. " of " .. #folders .. (delete and ", then delete them." or "."))
else
  print("Packed " .. packed .. " of " .. #folders .. ", deleted " .. deleted .. ", kept " .. kept .. ".")
end
