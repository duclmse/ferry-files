# Using fs

How to install, configure and run every part of the workspace: the `fsd`
daemon, the `fs` client and its TUI, the APIs, archive indexing and the
archive service, Everything file lists, and the `rar2zip` and `sftpd`
extensions.

For a full description of each module, see [reference/](reference/). For
building and testing from source, see [DEVELOPMENT.md](DEVELOPMENT.md).

- [1. The pieces](#1-the-pieces)
- [2. Install and first start](#2-install-and-first-start)
- [3. Configuration](#3-configuration)
- [4. Watch paths](#4-watch-paths)
- [5. The `fs` client](#5-the-fs-client)
- [6. Searching](#6-searching)
- [7. `fsd` commands](#7-fsd-commands)
- [8. Running as a service](#8-running-as-a-service)
- [9. APIs](#9-apis)
- [10. Files inside ZIPs, and the archive service](#10-files-inside-zips-and-the-archive-service)
- [11. Everything file lists](#11-everything-file-lists)
- [12. The index: stores, restarts and catch-up](#12-the-index-stores-restarts-and-catch-up)
- [13. Extensions](#13-extensions)
- [14. Library crates](#14-library-crates)
- [15. Scripts](#15-scripts)
- [16. Troubleshooting](#16-troubleshooting)

## 1. The pieces

| Crate | Binary | What you use it for |
| ----- | ------ | ------------------- |
| `fs-server` | `fsd` | The daemon: crawls the watch paths, follows changes, answers searches over IPC, REST and gRPC, and runs extensions |
| `fs-cli` | `fs` | The command-line client and a terminal UI. It talks to `fsd` over IPC only |
| `fs-protocol` | – | The IPC wire format and client, for programs that talk to `fsd` |
| `fs-ext-rar2zip` | `fs-ext-rar2zip` | Extension: converts RAR / 7Z / TAR archives dropped into a folder to ZIP and uploads them over SFTP |
| `fs-ext-sftpd` | `fs-ext-sftpd` | Extension: serves one folder over SFTP, public-key login only |
| `fs-common` | – | Logging, error types and the extension protocol, shared by the others |

One config file drives everything. `fsd` reads all of it, `fs` reads only
`api.ipc_path` from it, and each extension reads its own
`[extensions.<name>]` section.

## 2. Install and first start

**From a release build:**

```sh
# Windows (PowerShell)
scripts\build.ps1                 # fsd, fs and both extensions, release mode
scripts\install.ps1               # copies fsd.exe and fs.exe, writes a config
scripts\install.ps1 -Service      # the same, and registers the Windows service (admin)

# Linux / macOS
scripts/build.sh                  # fsd and fs; add -p fs-ext-rar2zip / -p fs-ext-sftpd
scripts/install.sh                # user install (~/.local/bin), config, user service
sudo scripts/install.sh --system  # system-wide, with a system service
```

The extensions need a C++ compiler to build (`unrar` compiles RARLAB's
UnRAR source).

**Starting it by hand.** `scripts/start.ps1` and `scripts/start.sh` start
fsd only if it isn't already running. If fsd is installed as a service and
stopped, they start the service. Otherwise they start `fsd daemon` in the
background and wait for it to answer over IPC.

They look for a config in this order:
1. `$FS_CONFIG`;
2. `./config.toml`;
3. the installed config;
4. `scripts/config.default.toml`.

The last one watches nothing until you add a watch path (see section 4).

```sh
fsd --config config.toml validate-config     # check it first
fsd --config config.toml daemon              # run in the foreground (Ctrl+C stops it)
fs  --config config.toml stats               # is it answering?
```

A first start crawls every watch path. Later starts catch up on what
changed while fsd was stopped (section 12).

## 3. Configuration

[`scripts/config.default.toml`](../scripts/config.default.toml) lists every
field with its default and a line on what it does.
`config.windows.toml`, `config.macos.toml` and
`crates/fs-server/config.toml` (Linux) are commented examples for each
platform.

| Section | Holds |
| ------- | ----- |
| top level | `watch_paths`, `data_dir`, `log_dir`, `extensions_dir` |
| `[[watch]]` | one watch path with its own mode, poll interval, content and exclusions (section 4) |
| `[store]` | memory or SQLite store, SQLite indexes, snapshot interval |
| `[exclusions]` | path globs to skip, hidden files, content size cap, network content |
| `[content.extension_limits]` | which extensions get full-text indexing, and how many bytes |
| `[archive_index]` | indexing the files inside ZIPs |
| `[api]` | REST, gRPC and IPC switches, ports, binds, CORS, token, IPC path |
| `[search]` | default and maximum limits, fuzzy settings, candidate caps |
| `[indexer]` | crawl and content threads, commit interval, event batch, NTFS file table |
| `[watcher]` | live change backend, debounce, buffer |
| `[file_lists]` | Everything `.efu` lists to search |
| `[archive_service]` | the `/archive` routes, `.zidx` / `.dzx` sidecars, scheduler |
| `[macros]` | your own query shorthands |
| `[extensions.<name>]` | one per extension (section 13) |

**Paths.** Relative paths are read from the folder fsd starts in. A service
installed with `install-service` starts in its config file's folder.

**Changing it while fsd runs.** fsd reads the file only at startup; an
edit by hand does nothing until fsd restarts or is told to apply it. Change
the config through fsd instead:

| How | Show | Set a key | Remove a key | Apply a hand edit | Replace the file |
| --- | ---- | --------- | ------------ | ----------------- | ---------------- |
| `fs` (IPC) | `fs config [get [key]]` | `fs config set <key> <value>` | `fs config unset <key>` | `fs config reload` | `fs config replace <file>` |
| REST | `GET /config[?key=]` | `PATCH /config {"key", "value"}` | `DELETE /config?key=` | `POST /config/reload` | `PUT /config` (TOML body) |
| gRPC | `GetConfig` | `ChangeConfig` `set` | `ChangeConfig` `unset` | `ChangeConfig` `reload` | `ChangeConfig` `replace` |

- **Keys** are dotted: `search.default_limit`, `content.extension_limits."*"`.
  A value is TOML (`200`, `true`, `"text"`, `["zip"]`); other text is taken
  as a string, so `fs config set store.mode memory` works. `get` with a key
  the file doesn't set shows the default.
- **Checked first.** A change that doesn't parse or validate changes
  nothing. Otherwise fsd writes the file in place, keeping its comments,
  layout and permissions, and runs with the new config.
- **What applies when.** Most settings apply at once: an added watch path
  is crawled and then watched, a removed one is no longer watched. These
  apply when fsd restarts, and a change lists them under `restart` until
  they match what fsd started with again: `data_dir`, `log_dir`,
  `store.mode`, `store.indexes`, `[archive_service]`, `[watcher]`, and the
  API switches, ports, binds, CORS origins and IPC path.
- **Extensions.** An extension whose `[extensions.<name>]` changed is asked
  to reload (`reload` asks every one). rar2zip takes the new settings, or
  keeps its running ones and says why. sftpd takes them at restart.
- **Not through the API:** `extensions_dir`, which names the programs fsd
  runs. Change it in the file while fsd is stopped.
- **Who may.** REST and gRPC need the token in every `api.auth_mode`, even
  to read: the config may hold passwords. A system service takes these
  over IPC only from an administrator, as with watch paths (section 4).

**Minimal config:**

```toml
watch_paths = ["C:/Projects", "D:/Data"]
data_dir    = "C:/ProgramData/fs"
log_dir     = "C:/ProgramData/fs/logs"

[exclusions]
path_patterns = ["**/.git/**", "**/node_modules/**", "**/target/**"]

[content.extension_limits]
rs  = 524288
md  = 131072
"*" = 32768
```

## 4. Watch paths

Watch paths are the folders fsd indexes and keeps current. Leave them out,
or give an empty list, and fsd runs without indexing anything. You can add
them later in any of these ways:

| How | List | Add | Remove |
| --- | ---- | --- | ------ |
| Config file | edit `watch_paths` | edit, then `fs config reload` | edit, then `fs config reload` |
| `fs` (IPC) | `fs watch` | `fs watch add <dir>` | `fs watch remove [--forget] <dir>` |
| REST | `GET /watch_paths` | `POST /watch_paths {"path": "..."}` | `DELETE /watch_paths?path=...[&forget=true]` |
| gRPC | `ListWatchPaths` | `AddWatchPath` | `RemoveWatchPath` (`forget`) |

- **Saved to the config.** A change through `fs`, REST or gRPC is also
  written to the config file, so it outlasts a restart. Only the
  `watch_paths` list is rewritten; comments and layout stay.
- **Offline paths.** A watch path that doesn't exist, such as an unplugged
  drive or an unmounted share, is `offline`. fsd still starts. The path's
  entries stay searchable, and nothing else is crawled because of it. fsd
  checks once a second; when the path is back, fsd catches up what changed
  from its own place in the change log, or crawls it alone if it can't.
  `validate-config` warns about such paths.
- **The path.** To add one, it must be an existing folder. `fs watch add .` resolves `.`
  against your current folder; REST and gRPC need an absolute path.
- **fsd's own folders.** `data_dir`, `log_dir` and the archive sidecar
  folders are never indexed, even under a watch path, so watching `C:\` with
  data in `C:\ProgramData\fs` is fine.
- **Already covered.** Adding a folder that is already watched changes
  nothing, and adding one inside a watched folder is refused. Adding a parent
  of watch paths replaces them with it: REST and gRPC list them as
  `absorbed`.
- **Removing.** fsd stops watching the folder. Its entries stay in the
  index, unless you remove it with `forget`: then they leave the index too,
  with their full-text documents and archive sidecars. A removed folder inside
  another watch path keeps its entries, which that path still covers.
- **Who may change them.** REST and gRPC need the bearer token, whatever
  `api.auth_mode` is (section 9.1). Every local user can reach a system
  service's IPC endpoint, so it takes changes over IPC only from an
  administrator: on Windows, `fs` run from an elevated prompt; on Unix,
  root or a member of `api.admin_group`. Others can edit its config file,
  or use REST or gRPC with the token.
- **Settings per path.** A `[[watch]]` table gives one watch path settings
  of its own; its path joins `watch_paths`:

  ```toml
  [[watch]]
  path      = "//nas/media"
  mode      = "poll"            # "watch" (default) or "poll"
  poll_secs = 900               # poll: seconds between scans
  content   = false             # no full-text content under it
  exclude   = ["**/@eaDir/**"]  # globs skipped under it, on top of [exclusions]
  ```

  A polled path gets no live watcher: every `poll_secs` a timestamp scan
  finds what changed. Use it for network shares, whose change notifications
  are missing or unreliable. A scan lists every folder of the path (about
  3 s for 350k entries), so keep the interval in minutes. Removing the path
  removes its table.

  A table whose path is inside another watch path is not a watch path of
  its own: its `content` and `exclude` apply to that folder, and its `mode`
  is ignored. That is what happens to a watch path with settings when you
  add a parent of it: it keeps its settings.
- **Progress.** `fs watch` lists each watch path with its state and the
  entries its crawl wrote:

  ```text
  C:\Users     ready     1,204,331 entries
  D:\Photos    crawling  48,210 entries
  ```

  The states are `pending` (added, not crawled yet), `crawling`, `ready`
  and `offline`. `GET /watch_paths` and `ListWatchPaths` return the same in
  `roots`, with the Unix times the crawl started and finished.
- **Inside the daemon.** fsd compares the watch paths with the config once a
  second. On a change it restarts the watcher and crawls only the added
  folders.

## 5. The `fs` client

```text
fs [--config <file>] <command>       # --config defaults to ./config.toml
```

| Command | Does |
| ------- | ---- |
| `search [query] [flags]` | Search; prints one path per line |
| `stats` | Files, folders, content and archive entries indexed, events applied, last crawl |
| `reindex [--full]` | Crawls every watch path again |
| `open <path>` | Opens a file with its default app, through the daemon. For a file inside a ZIP, it extracts it to a temp folder first |
| `watch [list\|add <dir>\|remove [--forget] <dir>]` | Watch paths (section 4) |
| `config [get [key]\|set <key> <value>\|unset <key>\|reload\|replace <file>]` | The daemon's config (section 3) |
| `ext <name> <command>` | Sends a command to an extension, e.g. `fs ext rar2zip status` |
| `events [--kinds root,applied]` | Prints what changes in the daemon as it happens (section 9.5) |
| `tui` | The terminal UI |

**Search flags.** Each flag adds a condition to the query:

| Flag | Adds |
| ---- | ---- |
| `-l, --limit <n>` (100), `--offset <n>` | paging |
| `--json` | JSON output instead of paths |
| `-e, --ext rs,toml` | `ext:` |
| `--path <prefix>` | path prefix |
| `--parent <dir>` | `parent:` |
| `--size <expr>` | `size:` (`>1mb`, `<=500kb`, `large`) |
| `--modified <date>`, `--created <date>` | `dm:`, `dc:` |
| `--content <text>` | `content:` |
| `--fuzzy <text>` | `fuzzy:` |
| `--regex <re>` | `regex:` on the full path |
| `--files-only`, `--dirs-only`, `--archive-only` | `file:`, `folder:`, `archive:` |
| `--efu <file>` | writes the results as an Everything file list (section 11) |

```sh
fs search "*.rs size:>1kb"
fs search "fuzzy:reprot dm:lastweek"
fs search --ext rs,toml --path C:/Projects --json
fs search "path:D:/Photos" --limit 1000000 --efu photos.efu
```

**TUI** (`fs tui`). Results update as you type. Its keys:

| Key | Does |
| --- | ---- |
| typing, Backspace | edit the query |
| ↑ / ↓ | move through the results |
| Enter | open the selected file, as `fs open` does |
| Tab | cycle the mode: exact, fuzzy, regex, content, archive |
| F5 | ask for a reindex |
| Esc, Ctrl+C | quit |

## 6. Searching

The syntax is voidtools Everything's, with a few additions. The complete
reference is [reference/query-language.md](reference/query-language.md).

| Write | Means |
| ----- | ----- |
| `report pdf` | both terms (space is AND) |
| `*.jpg\|*.png` | either (`\|` is OR) |
| `!node_modules` | not |
| `<c:\|d:> *.mp3` | grouping |
| `"program files"` | a phrase with a space in it |
| `*.log`, `src/*.rs` | wildcards; a term with `/` or `\` matches the full path |
| `ext:jpg;png` | extensions |
| `size:>10mb`, `size:1kb..500kb`, `size:large` | sizes |
| `dm:today`, `dm:2024`, `dc:>=lastweek` | dates modified and created (local time) |
| `parent:C:\Windows` | directly in that folder, not in its subfolders |
| `path:src`, `case:Foo`, `wfn:`, `ww:` | modifiers |
| `file:`, `folder:` | files only, folders only |
| `audio:`, `video:`, `pic:`, `doc:`, `zip:`, `exe:` | type macros |
| `dupe:`, `dupe:size`, `sizedupe: *.mp4` | duplicates |
| `sort:size-desc`, `count:20` | result order and count |
| `content:"fix this"` | full text, for extensions listed in `[content.extension_limits]` |
| `fuzzy:reprot` | typo-tolerant name match (addition) |
| `archive:*.rs`, `noarchive:` | only files inside ZIPs, or only real files (addition) |
| `archiveof:D:/a.zip` | every indexed entry of one archive (addition) |
| `work: readme` | your own macro from `[macros]` |

`content:`, `fuzzy:` and `dupe:` decide how the whole query runs, so they
cannot appear inside `|` or after `!`.

## 7. `fsd` commands

```text
fsd [--config <file>] <command>      # --config defaults to ./config.toml
```

| Command | Does |
| ------- | ---- |
| `daemon [--service]` | Runs the daemon in the foreground. The service manager passes `--service` for a system service |
| `stop [--wait <secs>]` | Stops the daemon that uses this config's data folder, cleanly: it saves its index first. Exit code 3 when none is running |
| `validate-config` | Checks the config, including each extension's section, and prints every problem |
| `generate-token` | Prints a new bearer token, and the digest to put in `api.auth_token_hash` |
| `install-service [--user\|--system]` | Registers fsd with the service manager and starts it (section 8) |
| `uninstall-service [--user\|--system]` | Stops and removes it |
| `zidx build <zip> [-o <out>]` | Builds a `.zidx` index for a ZIP (section 10) |
| `zidx ls <zidx> [path] [-d <depth>]` | Lists a `.zidx` as a tree |
| `zidx stat <zidx> <path>` | Shows one entry's metadata |

fsd logs JSON lines to `log_dir`, one file per day.

## 8. Running as a service

```sh
# Windows (administrator): the FsDaemon service, as LocalSystem, from boot
fsd --config C:/ProgramData/fs/config.toml install-service

# Linux: a systemd user unit, or a system unit from boot
fsd --config ~/.config/fs/config.toml install-service
sudo fsd --config /etc/fs/config.toml install-service --system

# macOS: a LaunchAgent, or a LaunchDaemon from boot
fsd --config ~/.config/fs/config.toml install-service
sudo fsd --config /etc/fs/config.toml install-service --system

fsd uninstall-service              # --system for a system service
```

The config is validated before the service is registered.

**System and user services.** A system service (every Windows service, and
`--system` elsewhere) serves all local users. Any of them can search its
index over IPC, so watch only folders they may all see. It refuses requests
that would act with its own rights: `fs open` and extension commands, and
watch-path changes unless they come from an administrator (section 4). A
user service serves only its user.

On macOS, read the notes at the top of `config.macos.toml` (Full Disk
Access) first.

Stopping the service is a clean stop: fsd applies pending changes, commits
the full-text index and saves `index.bin`. A background fsd from
`start.ps1` / `start.sh` can only be killed, so it relies on the periodic
snapshot instead (`store.snapshot_interval_secs`).

## 9. APIs

Three APIs serve the same index:

| API | Default address | Turned on by |
| --- | --------------- | ------------ |
| IPC | `\\.\pipe\fs` (Windows), `/tmp/fs.sock` | `api.ipc_enabled` |
| REST | `127.0.0.1:7700` | `api.rest_enabled` |
| gRPC | `127.0.0.1:7701` | `api.grpc_enabled` |

Each also needs fsd built with its cargo feature (`ipc`, `rest`, `grpc`;
all on by default). A build without one logs that at startup.

### 9.1 Tokens

```sh
fsd generate-token     # prints the token, and the digest for api.auth_token_hash
```

Send it as `Authorization: Bearer <token>` (gRPC: `authorization` metadata).

| `api.auth_mode` | Needs the token |
| --------------- | --------------- |
| `"writes"` (default) | REST routes that change files (`/fs/write`, `copy`, `move`, `delete`, `mkdir`), and watch-path changes over REST and gRPC |
| `"all"` | Every REST route but `/health`, and every gRPC call |

Without `api.auth_token_hash`:
- the routes that need a token are refused (403 Forbidden, gRPC
  `PERMISSION_DENIED` for watch-path changes);
- `"all"` refuses to start.

Use `"all"` whenever REST or gRPC is bound to anything but loopback. IPC
uses no token; the socket or pipe is open only to its user (or to all users
for a system service).

### 9.2 REST

| Method and path | Does |
| --------------- | ---- |
| `GET /search?q=&limit=&offset=` | Search: `{result: [...], total, took_ms, mode}` |
| `GET /stats` | Index statistics |
| `POST /reindex` `{"full": bool}` | Asks for a crawl |
| `GET /file?path=` | One indexed entry's metadata |
| `POST /open` `{"path": "..."}` | Opens a file on the daemon's machine; for a file inside a ZIP, extracts it first |
| `GET /watch_paths` | Watch paths, each with its crawl state and entry count |
| `POST /watch_paths` `{"path": "..."}` | Adds a watch path (token) |
| `DELETE /watch_paths?path=[&forget=true]` | Removes one (token); `forget` drops its entries too |
| `GET /config[?key=]` | The config file, or a key's value: `{text, from_file}` (token) |
| `PATCH /config` `{"key", "value"}`, `DELETE /config?key=` | Sets or removes a key (token) |
| `PUT /config`, `POST /config/reload` | Replaces the file with the body, or applies it after a hand edit (token) |
| `GET /metrics` | Archive scheduler metrics, Prometheus text |
| `GET /events?kinds=` | What changes, as server-sent events (section 9.5) |
| `GET /health` | `{"status": "ok"}`, never needs a token |
| `HEAD`, `GET /fs/read?path=` | A file's metadata, or its bytes. `Range:` gets 206 Partial Content |
| `GET /fs/drives` | Drives and mount points |
| `PUT /fs/write?path=` | Writes the request body to a file (token) |
| `POST /fs/copy`, `POST /fs/move` `{"from", "to"}` | Copies or moves a file or folder (token) |
| `DELETE /fs/delete?path=` | Deletes a file or folder (token) |
| `POST /fs/mkdir?path=` | Creates a folder with its parents (token) |
| `/archive/...` | The archive service, when enabled (section 10) |

**Rules for `/fs/*` and progress.**
- `/fs/*` paths must be absolute; `..` is refused.
- Copy, move and delete take `?stream=true` and then report progress as
  server-sent events: `{processed, total, current, done, error?}`.

**Browsers.** A browser may read responses only from origins listed in
`api.rest_cors_origins`, which is empty by default. Don't use `["*"]`:
`/fs/read` needs no token in `"writes"` mode.

```sh
curl "localhost:7700/search?q=*.rs%20dm:today&limit=20"
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
     -d '{"path": "D:/Data"}' localhost:7700/watch_paths
```

### 9.3 gRPC

The service is `fs.FileSearch` in
[`crates/fs-server/proto/fs.proto`](../crates/fs-server/proto/fs.proto), with
reflection, so `grpcurl` needs no proto file.

| Call | Does |
| ---- | ---- |
| `Search`, `SearchStream` | Search, as one response or a stream of results |
| `GetStats` | Index statistics |
| `Reindex` | Asks for a crawl |
| `ListWatchPaths`, `AddWatchPath`, `RemoveWatchPath` | Watch paths; add and remove need the token |
| `Subscribe` | What changes, as a stream (section 9.5) |
| `GetConfig`, `ChangeConfig` | The config (section 3); both need the token |

```sh
grpcurl -plaintext 127.0.0.1:7701 list
grpcurl -plaintext -d '{"query": "*.rs", "limit": 50}' 127.0.0.1:7701 fs.FileSearch/Search
grpcurl -plaintext -H "authorization: Bearer $TOKEN" -d '{"path": "D:/Data"}' \
        127.0.0.1:7701 fs.FileSearch/AddWatchPath
```

### 9.4 IPC

`fs` and the TUI use IPC, and so can your own programs, through the
`fs-protocol` crate (section 14).

**Framing.** Each message is a little-endian `u32` length followed by a
postcard-encoded envelope: magic `ZFS1`, a protocol version and a request
id. fsd refuses any other protocol version, so a client and fsd must be
built from matching sources.

**Requests:** `Search`, `Stats`, `Reindex`, `Open`, `Extension`, `Roots`,
`WatchAdd`, `WatchRemove`, `Subscribe`, `ConfigGet` and `ConfigChange`.
A config change is answered with `ConfigApplied {restart, reloaded,
problems}`. Errors come back as
`Error {code, message}`.

### 9.5 Change notifications

Instead of polling `/stats` or the watch paths, a client can subscribe:
IPC `Subscribe { kinds }` (then `IpcClient::next_notice`), REST
`GET /events` (server-sent events) or gRPC `Subscribe`. `fs events` prints
them.

| Kind | Sent when | Carries |
| ---- | --------- | ------- |
| `watch_paths` | the watch paths change | the new list |
| `root` | a watch path's crawl moves on | path, state, entries |
| `applied` | a batch of file changes is applied | how many, and up to 32 folders they were in |
| `lagged` | the subscriber fell behind | how many notices it missed: refresh |

`kinds` picks some (`?kinds=root,applied`); `lagged` always comes. Sending
never waits for a slow subscriber: one that falls more than 256 notices
behind loses the oldest and gets `lagged`.

## 10. Files inside ZIPs, and the archive service

**Inner files.** With `[archive_index] enabled = true` (the default), the
files inside each ZIP are indexed as virtual paths:
`D:/a.zip!/docs/readme.md`.
- `archive:` and `noarchive:` scope a search to inner files or to real ones.
- `archiveof:D:/a.zip` lists one archive's entries.
- `fs open` and `POST /open` extract an inner file to a temp folder before
  opening it.
- `max_entries_per_archive` caps how many entries of one archive are listed.
- `max_nesting_depth` (at most 2) indexes ZIPs inside ZIPs.
- `passwords` are tried on encrypted entries.
- `[archive_index.inner_extension_limits]` turns on full-text indexing for
  inner files, as `[content.extension_limits]` does for real ones.

**Archive service.** `[archive_service] enabled = true` adds the `/archive`
REST routes. They let a client browse and stream a ZIP's contents without
extracting it:

| Route | Does |
| ----- | ---- |
| `GET /archive` | The registered archives and their ids |
| `GET /archive/:id/index` | The raw `.zidx` index |
| `GET /archive/:id/dir?node=` | One folder's listing |
| `GET /archive/:id/file?node=` | An entry's bytes; `Range:` gets 206 for stored entries, and for deflated ones with a `.dzx` sidecar |
| `GET /archive/:id/search?q=&limit=&offset=` | Search within one archive |

**The sidecar files:**
- `.zidx` is a compact binary index of an archive's tree, kept in `zidx_dir`.
- `.dzx` holds random-access points into a large deflated entry, so a range
  request need not inflate from the start. It is built for entries of at
  least `dzx_min_entry_bytes`, and kept in `dzx_dir`.

`fsd zidx build|ls|stat` makes and inspects `.zidx` files by hand.
`[archive_service.scheduler]` limits concurrent inflates per archive and,
optionally, rate-limits clients and queues them fairly.

## 11. Everything file lists

An `.efu` file is the CSV file list voidtools Everything exports. List them
and their entries are searchable next to crawled files:

```toml
[file_lists]
paths     = ["D:/catalogues/backup-disk.efu", "D:/catalogues/laptop.efu"]
poll_secs = 30
```

**When a list changes.**
- A list is read again when its size or modification time changes. Rows it
  no longer has are removed.
- A list taken out of the config takes its entries with it.
- A list that can't be read, for example on a drive that isn't mounted,
  keeps its entries.

**What its entries are.**
- Rows under a watch path are left out, since the crawl knows those files.
- Listed files aren't on this machine. Opening them fails like any missing
  file, and they have no content index.

`fs search <query> --limit <n> --efu out.efu` writes one. Everything can
open it (File > Open File List) or include it in its own index.

## 12. The index: stores, restarts and catch-up

**Stores** (`store.mode`):

| Store | On disk | Best at |
| ----- | ------- | ------- |
| memory (default via `"auto"`) | `data_dir/index.bin`, ~19 B per entry | substring, `parent:`, `dupe:` searches; less disk |
| `sqlite` | `data_dir/index.db`, ~800 B per entry | less process memory; cheapest indexed lookups (`ext:`) |

`"auto"` keeps an existing SQLite index rather than crawling again. Full
text is in a Tantivy index under `data_dir` with either store. Measured
numbers are in [BENCHMARKS.md](BENCHMARKS.md).

**Restarts.** fsd stores a position in the OS change log and replays it on
the next start:
- the USN journal on Windows (administrator rights);
- FSEvents history on macOS;
- a timestamp scan on Linux, and on Windows without the journal.

Each watch path keeps its own place in the log. Only the paths that need
it are crawled: paths added while fsd was stopped, paths on a different
volume than before, and paths whose history can't be trusted. A first start
crawls everything.

**Live changes** (`[watcher]`). `"native"` (the default) uses
ReadDirectoryChangesW on Windows and notify elsewhere. `"usn"` reads the NTFS
journal. Whenever a backend loses changes, a timestamp scan finds them.

**Large drives.** On Windows, `indexer.use_mft = "auto"` crawls a whole-drive
root from the NTFS file table, given administrator rights: 33 s against
650 s for all of `C:\` (1.43M entries).

`fs reindex` crawls every watch path again.

## 13. Extensions

### 13.1 Loading

Set `extensions_dir` to a folder with one subfolder per extension. Each
subfolder holds an `extension.toml` and its executable:

```text
extensions/rar2zip/extension.toml
extensions/rar2zip/fs-ext-rar2zip
extensions/sftpd/extension.toml
extensions/sftpd/fs-ext-sftpd
```

```toml
name       = "rar2zip"       # the folder's name
version    = "0.1.0"
protocol   = 1
executable = "fs-ext-rar2zip"
```

**Lifecycle.**
- fsd starts each extension when it starts, restarts it if it crashes, and
  stops it on shutdown.
- An extension reads its own `[extensions.<name>]` section of the daemon's
  config.
- `fsd validate-config` checks those sections too.
- `fs ext <name> <command>` sends it a command.
- The executables also run alone: `fs-ext-rar2zip --config <file> validate`
  checks the config without fsd.

**Extensions run with the daemon's rights.**
- On Unix, fsd refuses an extensions folder that anyone but root or its own
  user can write to, and refuses symlinks.
- On Windows the folder must be inside Program Files.
- The REST file API can't change the folder or the config file.

### 13.2 rar2zip

rar2zip watches `watch_folder` for RAR, 7Z, TAR, TAR.GZ and TAR.BZ2 archives.
It also picks up archives already there when it starts, and archives moved
or renamed into the folder. For each one it:

1. checks the archive's integrity;
2. converts it to ZIP, streaming entry by entry;
3. uploads the ZIP to every destination over SFTP and checks each upload's
   SHA-256 on the server.

Multi-part RARs (`.part1.rar`, `.r00`) are grouped. Failed archives go to
`failed_folder`.

```toml
[extensions.rar2zip]
watch_folder  = "/srv/incoming"
failed_folder = "/srv/incoming/failed"

[extensions.rar2zip.conversion]
stream_to_transfer_threshold_mb = 500   # larger archives stream without a ZIP on disk

[extensions.rar2zip.transfer]
max_concurrent = 3
host_key_check = "strict"               # "strict", "accept-new" or "off"

[[extensions.rar2zip.destinations]]
host         = "sftp.example.com"
port         = 22
user         = "deploy"
remote_path  = "/uploads"
ssh_key_path = "/etc/fs/rar2zip/id_ed25519"
# host_keys  = ["SHA256:..."]           # pin, instead of known_hosts

[extensions.rar2zip.observability]
metrics_port   = 9090
dashboard_port = 8080
```

**Strategies.** Below the threshold, rar2zip converts to a ZIP on disk,
transfers it, then deletes it. Above it, it streams the conversion straight
into SFTP, and the source archive serves as the retry point.

**Host keys.**
- `strict` refuses an unknown host and a changed key.
- `accept-new` records a new host in `known_hosts` and refuses a changed
  key.
- `off` accepts anything and logs a warning.

```sh
fs ext rar2zip status          # queue by state, recent jobs
fs ext rar2zip retry-failed    # queue failed jobs again
```

- **Dashboard:** `http://localhost:8080/ui`.
- **Prometheus metrics:** `http://localhost:9090/metrics`.
- **Health:** `http://localhost:9090/health`.

Every setting is in
[`crates/fs-ext-rar2zip/config.toml`](../crates/fs-ext-rar2zip/config.toml).
Operations and troubleshooting are in
[`crates/fs-ext-rar2zip/README.md`](../crates/fs-ext-rar2zip/README.md).

### 13.3 sftpd

sftpd serves one folder over SFTP. A rar2zip destination can point at it, on
the same machine or another.

```toml
[extensions.sftpd]
listen   = "127.0.0.1:2222"
root     = "/srv/fs/incoming"
host_key = "/var/lib/fs/sftpd/ssh_host_ed25519_key"   # generated if missing
# read_only = true

[[extensions.sftpd.users]]
name            = "rar2zip"
authorized_keys = "/etc/fs/sftpd/rar2zip.authorized_keys"
```

- **Login.** Public keys only, read from OpenSSH `authorized_keys` files.
  Key options such as `from=` are ignored, and logged.
- **The root.** Clients see `root` as `/`. `..` stops there, and paths that
  resolve through a symlink to outside it are refused.
- **No shell.** The exec channel answers only `sha256sum` and `rm -f`
  inside the root, which is what rar2zip runs.
- **Renames.** An SFTP rename doesn't replace an existing file.

```sh
fs ext sftpd status     # listen address, root, users, host key fingerprint
```

To use it as a rar2zip destination, pin its key with `host_keys`, using the
fingerprint `status` prints.

## 14. Library crates

**`fs-protocol`** is for Rust programs that talk to a running fsd:

```rust
use fs_protocol::ipc_client::IpcClient;
use fs_protocol::ipc_protocol::{IpcRequest, IpcResponse};

let mut client = IpcClient::connect(&fs_protocol::default_ipc_path()).await?;
if let IpcResponse::Search(r) = client
    .request(IpcRequest::Search { query: "*.rs".into(), limit: 20, offset: 0 })
    .await?
{
    for hit in r.hits {
        println!("{}", hit.path);
    }
}
```

The crate also holds `attrib`, the file attribute bits and their Windows
mapping, and `efu`, which reads and writes Everything file lists.

**`fs-common`** holds what the daemon and the extensions share:
- the tracing setup (JSON lines, daily files);
- the error types;
- the `Extension` trait;
- the JSON-lines protocol between fsd and extension executables
  (`extension_stdio.rs`).

A new extension implements that protocol on stdin and stdout, and logs to
stderr only.

## 15. Scripts

Each script has a PowerShell (`.ps1`) and a shell (`.sh`) version unless
noted.

| Script | Does |
| ------ | ---- |
| `build` | Release build of fsd and fs. The PowerShell version also builds both extensions; pass `-p <crate>` to the shell version. An fsd running from `target/release` is stopped first and started again with its config after (`-NoRestart` / `FS_NO_RESTART=1` leaves it stopped) |
| `install` | Installs the binaries and a config, optionally as a service; `-Uninstall` / `--uninstall` reverses it |
| `start` | Starts fsd if it isn't running (section 2) |
| `stop` | Stops fsd cleanly: the service, or each background fsd through `fsd stop` (SIGTERM on Unix). `-Force` / `FS_FORCE=1` kills one that doesn't stop |
| `run` | Runs fsd in the foreground |
| `config.default.toml` | Every config field at its default |
| `db` | Inspects an fs or rar2zip SQLite database through `sqlite3` |
| `bench`, `bench-query` | Crawl and query-latency benchmarks |
| `bench-windows.ps1`, `bench-everything.ps1` | The Windows end-to-end benchmark, and the same against voidtools Everything |
| `bench-live.py` | Live-change latency per watcher backend |
| `worktree` | Git worktree helpers for development |
| `clean` | Interactive disk cleanup for a dev machine (not specific to fs) |

## 16. Troubleshooting

| Symptom | Look at |
| ------- | ------- |
| `fs`: cannot connect | Is fsd running (`scripts/start`)? Does `fs` read the same config, so the same `api.ipc_path`? |
| `fs`: unsupported version | `fs` and `fsd` come from different builds; rebuild both |
| A config edit has no effect | fsd reads the file only at startup: run `fs config reload`, which says what is wrong with it, or restart fsd |
| Search finds nothing yet | `fs stats`: the first crawl may still be running (`last_crawl_secs` is 0 until it ends) |
| `NOT_ALLOWED` from `fs open`, `fs ext` or `fs watch add` | fsd runs as a system service (section 8); for `fs watch`, run it as an administrator |
| REST 403 "this route is disabled" | No `api.auth_token_hash` (section 9.1) |
| REST 401 | Missing or wrong bearer token |
| Changes made while stopped are missing | On Windows without administrator rights, catch-up is a timestamp scan; `fs reindex` crawls everything again |
| rar2zip host key errors | [rar2zip README, troubleshooting](../crates/fs-ext-rar2zip/README.md#8-troubleshooting) |
