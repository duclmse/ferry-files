# Watch paths: next steps

> **Status:** proposed (Nov 2026). Builds on `ec9bfe8`: watch paths may be
> empty, change while the daemon runs (`daemon::follow_watch_paths`,
> `crawler::crawl_roots`), and change over IPC, REST and gRPC (`watch_list.rs`).

Eight follow-ups. Each section says what changes, where, what it costs at run
time, and how to test it. Measured numbers come from `docs/BENCHMARKS.md`
(C:\Windows, 348,615 entries, 16 threads); numbers marked _estimate_ are not
measured yet and should be checked with `scripts/bench-windows.ps1` or
`scripts/bench-live.py` once implemented.

## Summary

| #   | Change                                                 | Steady-state cost                                               | One-off cost                                     | Net effect                                                    |
| --- | ------------------------------------------------------ | --------------------------------------------------------------- | ------------------------------------------------ | ------------------------------------------------------------- |
| 1   | Forget a removed root's entries                        | none                                                            | one delete pass over the root's rows             | frees memory and disk; searches stop returning stale rows     |
| 2   | Fold child roots into an added parent                  | none                                                            | crawl of the parent only                         | avoids a double crawl and duplicate events                    |
| 3   | Never index `data_dir` / `log_dir`                     | one prefix test per path, already paid by exclusions            | none                                             | removes a self-feeding event loop (large win when it applies) |
| 4   | Crawl progress per root                                | one atomic add per batch                                        | none                                             | negligible                                                    |
| 5   | Per-root settings                                      | per-root exclusion engines; poll mode costs a scan per interval | none                                             | lets slow or remote roots stop costing the rest               |
| 6   | Roots that go offline and come back                    | one `stat` per root per second                                  | catch-up of the returning root, not a full crawl | avoids full crawls after unplugging a drive                   |
| 7   | Change notifications                                   | nothing without subscribers; one broadcast per batch with       | none                                             | removes client polling                                        |
| 8   | Administrators change a service's watch paths over IPC | a token lookup per mutating request                             | none                                             | negligible                                                    |

Suggested order: 3 (a correctness and load problem that exists today), 1, 2, 4,
6, 7, 5, 8. Items 1–4 are small; 5 and 6 change the config and the change-log
position format.

## 1. Forget a removed root's entries

**Today.** `watch_list::remove` drops the root from the config and the watcher
stops, but its rows stay. Nothing removes them later: a full crawl sweeps stale
rows per root (`remove_stale_for_root`), and a root that is no longer listed is
never swept.

**Change.**

- `WatchRemove { path, forget: bool }`, REST
  `DELETE /watch_paths?path=&forget=true`, gRPC `RemoveWatchPathRequest.forget`,
  `fs watch remove --forget <dir>`. Default `false`, so a mistaken removal costs
  nothing to undo.
- `follow_watch_paths` gets the forget list with the change (a
  `watch::Sender<Vec<PathBuf>>` next to the config, or a field in an
  `Arc<Mutex<_>>` the daemon owns) and, after the old watcher stops, runs on a
  blocking thread:
  - `FileStore::remove_directory(root)` plus the row for the root itself. SQLite
    uses `UNDER_DIR_1`, an index range; do **not** reuse
    `remove_stale_for_root`, whose `LIKE` scans the whole table.
  - Tantivy: one `delete_query` over a path-prefix query (or per-path
    `delete_term` only when the root has few content rows), then let the commit
    loop commit. No `commit()` here.
  - `ArchiveRegistry` entries and `.zidx` / `.dzx` sidecars under the root.
  - The per-root entry count meta key (`entries_key(root)`).
- Skip rows that another watch path still covers (a removed root inside a kept
  one, which `watch_list::add` refuses now but old configs may hold) and rows
  from `[file_lists]` (`data_dir/file_lists/` records them).
- Exclude the job from running alongside a crawl: take `crawl_active` the way
  `follow_watch_paths` does for added roots.

**Performance.**

- Memory store: `remove_where` walks every entry under the write lock and then
  compacts. That is one pass over the whole index, not just the root: for 350k
  entries it is a few ms; for 5M entries _estimate_ 50–150 ms during which
  writes and searches wait. If that matters, batch the removal (remove a block
  of matches, release the lock, repeat) the way the crawl writer batches
  upserts.
- SQLite: an index range delete, about the cost of writing those rows; roughly a
  third of the crawl time for the root (_estimate_). The WAL grows by the
  deleted pages until the next checkpoint.
- Tantivy: deletes are tombstones until segments merge. Search cost is
  unchanged; disk is reclaimed by the next merge.
- Afterwards: memory and disk drop by the root's share (18.7 B/entry
  `index.bin`, 794 B/entry `index.db`) and name scans get faster in proportion.

**Tests.** Integration: add a root, wait for its file, remove with forget, the
file is gone from search and `/stats` counts drop; without forget it stays.
Unit: memory and SQLite arms both remove exactly the subtree.

## 2. Fold child roots into an added parent

**Today.** Adding `C:\` while `C:\Users` is watched keeps both. The full crawl
walks `C:\Users` twice (once per root), and both watchers report each change in
it, so every event there is applied twice.

**Change.** In `watch_list::add`, when the new path contains existing roots:
remove those roots from the list in the same edit, and report them in the
outcome (`Outcome::absorbed`), so the client can say "C:\Users is now part of
C:\". `crawl_roots` then crawls the parent once; its sweep
(`remove_stale_for_root(parent, generation)`) covers the children's rows, which
the crawl has just re-tagged.

To avoid re-crawling the children, a later refinement can walk the parent but
skip each absorbed child's subtree (`WalkState::Skip`) and re-tag its rows with
the parent's `watch_root` and the new generation in one pass, instead of
re-reading the disk.

**Performance.**

- One crawl of the parent instead of parent plus children: the saving is the
  children's crawl time (73k entries/s memory, 51k entries/s SQLite).
- Live: each change under a child is applied once instead of twice, halving
  watcher work there.
- The skip-and-retag refinement turns the children's part of the crawl into a
  row update: memory store _estimate_ 10× faster than walking them.

**Tests.** Unit in `watch_list`: absorbed children leave the list and the file.
Integration: add a child, then its parent; the child's file is found once, and
an edit in the child produces one event (`events_processed` +1).

## 3. Never index `data_dir` or `log_dir`

> **Done.** `exclusions::own_dirs`, `ExclusionEngine::for_config`; tests
> `the_daemons_own_folders_are_left_out`,
> `the_data_and_log_folders_are_not_crawled` and
> `test_own_folders_under_a_watch_path_are_ignored` (57 events in 4 s idle
> before, none after).

**Today.** Nothing keeps the daemon's own folders out of the crawl. Watching a
folder that contains `data_dir` (the default config's `"data"` next to a watched
`"."`, or `C:\` with data under `C:\ProgramData`) means:

- the Tantivy commit loop changes segment files every `commit_interval_secs` (5
  s), each change is an event, each event is applied, which marks the index
  changed, and so on;
- the memory store rewrites `index.bin` every `snapshot_interval_secs`;
- the JSON log grows with every line, and each applied event logs.

So an idle daemon never goes idle.

**Change.** Exclude, rather than refuse: watching `C:\` is legitimate.

- `ExclusionEngine::new` takes the resolved `data_dir`, `log_dir` and every
  configured `zidx_dir` / `dzx_dir`, and `should_exclude` tests them as path
  prefixes before the user patterns.
- Live events need nothing more: every backend's events reach `process_events`,
  which drops excluded paths (`without_excluded`). `ExclusionEngine::shared`
  caches engines by config, so the new prefixes must be part of its key.
- Log once at startup: "data_dir is under watch path X; it is not indexed".

**Performance.** One extra prefix comparison per path in the walk, against 2–4
prefixes: noise next to the `stat` each entry costs. The win, where it applies,
is the whole feedback loop: from a constant trickle of events and writes back to
0 % CPU when idle (BENCHMARKS.md idle: 0 % memory store).

**Tests.** Crawler unit test with `data_dir` inside the watch root: no row under
it. Integration: a watched root containing `data_dir`, idle for 3 ×
`commit_interval_secs`, `events_processed` does not grow.

## 4. Crawl progress per root

**Today.** `RootsResponse.crawled` and `/watch_paths` say only whether the first
full crawl has finished. A root added later shows `crawled: true` while it is
still being crawled.

**Change.**

- `IndexMetrics` gains `roots: RwLock<HashMap<String, RootProgress>>` with
  `state` (`pending`, `crawling`, `ready`, `offline` from item 6), `entries` (an
  `Arc<AtomicU64>` the crawl writer thread bumps per batch), `started` and
  `finished`.
- `crawl_roots` sets `crawling` per root as it starts it and `ready` after its
  sweep; `follow_watch_paths` sets `pending` for added roots.
- Protocol: `RootsResponse` gets `Vec<RootStatus>` alongside `roots` (bump
  `PROTOCOL_VERSION`, update Ferry's `wire.rs` copy); REST and gRPC return the
  same fields. `fs watch` prints `C:\Users  ready  1,204,331 entries`.

**Performance.** The writer thread already counts each committed row
(`total_files.fetch_add` per row). Count per batch instead (one
`fetch_add(batch.len())`), which is cheaper than today's per-row adds. The map
is read by API calls and written a few times per crawl. Negligible.

**Tests.** Unit: states move pending → crawling → ready. Integration: right
after `WatchAdd`, the root is not `ready`; it becomes `ready` with a non-zero
count.

## 5. Per-root settings

**Today.** Exclusions, content limits and the watcher backend are global. A
network share, a USB drive and a source tree all get the same treatment.

**Change.** Allow a table form beside the plain list, both accepted:

```toml
watch_paths = ["C:/Projects"]

[[watch]]
path = '\\nas\media'
mode = "poll"            # "watch" (default) or "poll"
poll_secs = 900
content = false          # no full-text extraction
exclude = ["**/@eaDir/**"]
```

- `Config::roots()` merges both forms into `Vec<RootConfig>`; everything that
  reads `watch_paths` today reads `roots()`. `watch_list` edits whichever form
  holds the path.
- The crawler builds one `ExclusionEngine` per root: global patterns plus the
  root's. `SideWork` already decides content per root (network mounts), so
  `content = false` hooks in there.
- `mode = "poll"` roots get no watcher; `follow_watch_paths` runs
  `journal::changes_since(root, last_poll)` every `poll_secs` and sends the
  events. This is the timestamp scan the recovery path already uses.

**Performance.**

- Exclusion engines: a compiled glob set per root, kilobytes each.
- Poll mode costs one scan per interval, which lists every folder of the root:
  on C:\Windows the catch-up scan took 2.3–3.0 s at ~900 % CPU. So `poll_secs`
  should be minutes, and the scan should run on the blocking pool at low
  priority. It replaces a watcher that on network shares either misses changes
  (notify) or holds a 64 KiB buffer per root (native).
- `content = false` on media and archive roots removes most of the side work
  there (content extraction is the expensive part of a crawl with content limits
  set).

**Tests.** Config: both forms parse, merge, and round-trip through `watch_list`.
Crawler: a root's own exclusion applies only under it. Integration: a poll root
picks up a new file within `poll_secs`.

## 6. Roots that go offline and come back

**Today.** `validate` rejects a config whose watch path does not exist, so an
unplugged drive stops fsd from starting and makes every later reload fail. The
change-log position stores all roots together (`Position.roots`); any difference
means a full crawl of everything.

**Change.**

- `validate` warns instead of failing for a missing watch path (keep the error
  for a path that exists but is not a folder).
- `follow_watch_paths` checks each root's existence once per poll
  (`symlink_metadata`), starts the watcher only on present roots, and marks
  absent ones `offline` (item 4). Their rows stay and searches still return
  them; a client can grey them out.
- When a root comes back: catch it up rather than crawl it. Store a position per
  root (`journal_position:<root>` meta keys, or `Position.roots` matched per
  entry instead of as a whole), and run `catch_up` for that root alone. Fall
  back to `crawl_roots([root])` when its volume identity changed (a different
  disk mounted at the same letter).

**Performance.**

- One `stat` per root per second: microseconds locally. For network roots, do
  the check on the blocking pool with a timeout, since an unreachable share can
  block for tens of seconds.
- A returning root costs a catch-up (3.0 s for 348k entries by timestamp scan,
  less with the USN journal) instead of a full crawl of every root, and a daemon
  restart with a drive missing no longer crawls everything.
- Per-root positions remove today's "watch roots changed, full crawl" on restart
  after any watch-path edit: that is the largest saving in this plan for big
  indexes (a full crawl of C:\ took 33 s with the file table, 650 s walking).

**Tests.** Journal: per-root positions catch up one root and leave the others.
Integration: rename a watched folder away (offline), back again, files created
meanwhile appear without a full crawl (`last_crawl_secs` unchanged).

## 7. Change notifications

**Today.** Clients poll `/stats` or `Roots` to see crawls finish, and re-run
searches to see changes.

**Change.**

- A `tokio::sync::broadcast::Sender<Notice>` in the daemon, with notices for
  watch-path changes, root state changes (item 4) and applied event batches
  (count and the batch's parent folders, capped, not every path).
- IPC `Subscribe { kinds }`: the connection switches to a stream of
  `IpcResponse::Notice` frames until the client closes it. REST `GET /events` as
  server-sent events (the `/fs/*` routes already stream SSE). gRPC `Subscribe`
  as a server stream.
- A lagging subscriber gets `Notice::Lagged(n)` and should refresh, rather than
  slowing the daemon.

**Performance.** `broadcast::send` with no receivers is a lock and a counter;
with receivers, one clone of the notice per batch per subscriber. Batches are at
most `event_batch_size` (100) events, so notices are at most one per batch.
Never send per event, and never await a subscriber in `process_events`. Each
subscriber holds one task and a ring buffer slot per pending notice (size the
channel at a few hundred).

**Tests.** Unit: a lagging receiver gets `Lagged`, the sender never blocks.
Integration: subscribe, add a watch path, receive `RootState(ready)`.

## 8. Administrators change a service's watch paths over IPC

**Today.** A system service refuses `WatchAdd` / `WatchRemove` over IPC for
everyone, since any local user can connect.

**Change.** Identify the client and allow administrators:

- Windows: `GetNamedPipeClientProcessId` → `OpenProcess` → `OpenProcessToken` →
  `CheckTokenMembership` for the Administrators group (an elevated token only: a
  filtered UAC token does not count), or `ImpersonateNamedPipeClient` and check
  while impersonating.
- Unix: `SO_PEERCRED` (Linux) / `getpeereid` (macOS) and allow uid 0 or a
  configured group (`api.admin_group = "fs-admin"`).
- Decide per request, not per connection, and keep refusing `Open` and extension
  commands as today.

**Performance.** A few syscalls per mutating request (tens of microseconds),
none for searches. Negligible.

**Tests.** Unit: the decision function given an identity. The OS calls need a
service install, so cover them in the Windows and Linux service CI jobs rather
than unit tests.

## Invariants to keep

- No full file list in memory: removals and retags run as index ranges or in
  batches, never by collecting every path first.
- Tantivy commits stay in the commit loop.
- Watcher events stay in one ordered consumer (`process_events`); a poll root's
  events go through the same channel.
- Every `FileStore` method keeps its memory arm; anything that revives or
  renames entries calls `NameFilters::changed`.
- A new IPC request that acts as the daemon's account is refused for a shared
  service unless item 8's check passes.
