# rar2zip

An `fsd` extension that watches a folder for `.rar`, `.7z`, and `.tar.*` archives,
converts them to `.zip`, and transfers the result to one or more SFTP destinations
with bandwidth throttling and retry logic.

```
    ┌─────────────────┐
    │  Watch Folder   │
    │  (RAR/7Z/TAR)   │
    └─────────────────┘
          |
          | created, moved in, or present at start
          ▼
    ┌──────────────┐
    │   Watcher    │
    │  (notify)    │
    └──────────────┘
          |
          | Enqueue
          ▼
    ┌──────────┐
    │  Queue   │
    │ (SQLite) │
    └──────────┘
          │
          │
          ▼
  ┌──────────────────────────┐
  │   process_job            │
  │  ┌────────────────────┐  │
  │  │ Disk strategy:     │  │
  │  │  convert → zip     │  │
  │  │  SFTP upload × N   │  │
  │  ├────────────────────┤  │
  │  │ Stream strategy:   │  │
  │  │  decompress → pipe │  │
  │  │  SFTP upload × 1   │  │
  │  └────────────────────┘  │
  └──────────────────────────┘
              │
┌─────────────┼─────────────┐
▼             ▼             ▼
/metrics      /health       /ui
Prometheus    JSON          Dashboard
```

> **Platform:** Windows, Linux and macOS. Building it requires a C++ compiler.

---

## 1. Prerequisites

| Requirement | Notes |
|-------------|-------|
| Rust ≥ 1.78 and a C++ compiler | For building; the RARLAB UnRAR source is compiled in, so no `unrar.dll` is needed |
| SSH key pair | Private key for each SFTP destination; public-key login only |
| The server's host key | In `known_hosts` or pinned in `host_keys` (see [Host keys](#5-host-keys)) |
| `sha256sum` and `rm` on the remote | Run over SSH exec to check and clean up uploads. Standard on Linux; the `sftpd` extension provides both |

---

## 2. Build

```sh
cargo build --release -p fs-ext-rar2zip
```

Binary: `target/release/fs-ext-rar2zip`. Building needs a C++ compiler.

---

## 3. Configuration

Add an `[extensions.rar2zip]` section to the daemon's `config.toml`. Its presence
enables the extension (see `crates/fs-ext-rar2zip/config.toml` for a fully-annotated
example):

```toml
[extensions.rar2zip]
watch_folder  = 'C:\Incoming\Archives'
failed_folder = 'C:\Incoming\Failed'

include_extensions = ["rar", "7z", "tar.gz"]
exclude_patterns   = ["*.tmp", "*.part"]
min_file_age_secs  = 5

[[extensions.rar2zip.destinations]]
host         = "sftp.example.com"
port         = 22
user         = "deploy"
remote_path  = "/srv/uploads"
ssh_key_path = 'C:\Users\me\.ssh\id_ed25519'
# host_keys  = ["SHA256:..."]   # pin instead of using known_hosts

[extensions.rar2zip.conversion]
compression_level               = 6
stream_to_transfer_threshold_mb = 500
multipart_timeout_hours         = 24

[extensions.rar2zip.transfer]
max_concurrent       = 3
max_retries          = 3
retry_delay_secs     = 10
bandwidth_limit_kbps = 0   # 0 = unlimited
host_key_check       = "strict"

[extensions.rar2zip.observability]
metrics_port = 9090
dashboard_port = 8080
webhook_url  = "https://hooks.slack.com/services/..."
```

### Full config reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `watch_folder` | `string` | *(required)* | Folder to watch for new archives |
| `failed_folder` | `string` | *(required)* | Failed jobs are moved here |
| `include_extensions` | `[string]` | `["rar","7z","tar.gz"]` | Extensions to process |
| `exclude_patterns` | `[string]` | `[]` | Glob patterns to ignore |
| `watch_subdirs` | `[string]` | *(all)* | Only watch these subdirectory names |
| `min_file_age_secs` | `int` | `5` | Minimum file age before processing |
| `destinations[].host` | `string` | *(required)* | SFTP hostname |
| `destinations[].port` | `int` | `22` | SFTP port |
| `destinations[].user` | `string` | *(required)* | SSH username |
| `destinations[].remote_path` | `string` | *(required)* | Remote upload directory |
| `destinations[].ssh_key_path` | `string` | *(required)* | Private key file path |
| `destinations[].host_key_check` | `string` | *(transfer's)* | Overrides `transfer.host_key_check` |
| `destinations[].host_keys` | `[string]` | `[]` | Pinned `SHA256:` fingerprints; when set, `known_hosts` is not used |
| `conversion.compression_level` | `int` | `6` | ZIP deflate level (0–9) |
| `conversion.archive_password` | `string` | *(none)* | Password for encrypted archives |
| `conversion.stream_to_transfer_threshold_mb` | `int` | `500` | Files above this use Stream strategy |
| `conversion.multipart_timeout_hours` | `int` | `24` | Expire incomplete multi-part sets after N hours |
| `transfer.max_concurrent` | `int` | `3` | Max concurrent SFTP uploads |
| `transfer.max_retries` | `int` | `3` | Retry attempts per destination |
| `transfer.retry_delay_secs` | `int` | `10` | Base delay for exponential backoff |
| `transfer.bandwidth_limit_kbps` | `int` | `0` | Upload rate limit (0 = unlimited) |
| `transfer.host_key_check` | `string` | `strict` | `strict`, `accept-new` or `off` |
| `transfer.known_hosts` | `string` | `~/.ssh/known_hosts` | OpenSSH known_hosts file |
| `observability.bind` | `ip` | `127.0.0.1` | Address for the metrics and dashboard servers |
| `observability.metrics_port` | `int` | `9090` | Prometheus metrics HTTP port |
| `observability.dashboard_port` | `int` | `8080` | HTML dashboard HTTP port |
| `observability.webhook_url` | `string` | *(none)* | Slack/Teams webhook for alerts |
| `observability.alert_email` | `string` | *(none)* | Alert recipient email address |
| `observability.alert_queue_depth_threshold` | `int` | `50` | Alert when queue depth exceeds this |
| `smtp.host` | `string` | *(required for email)* | SMTP server hostname |
| `smtp.port` | `int` | *(required for email)* | SMTP port |
| `smtp.user` / `smtp.password` | `string` | *(required for email)* | SMTP credentials |
| `smtp.from` | `string` | *(required for email)* | Sender address |

---

## 4. Running

rar2zip is started, stopped and restarted by `fsd`. To install it, copy the
executable and `extension.toml` to `<extensions_dir>/rar2zip/` and set
`extensions_dir` in the daemon config.

```sh
# Validate the daemon config, including this section (also pings each destination)
fsd --config config.toml validate-config

# Run the daemon, and the extension with it
fsd --config config.toml daemon

# Requires a running daemon
fs --config config.toml ext rar2zip status
fs --config config.toml ext rar2zip retry-failed
```

Log output goes to the daemon's log (`fs.log` in `log_dir`).

Archives are picked up when they are created in `watch_folder` or a subfolder,
moved or renamed into it (downloaders that finish by renaming `x.part` work),
or already there when rar2zip starts. `failed_folder` is skipped even when it
is inside `watch_folder`. A file whose content is already queued, running or
done is skipped; copying a failed one back in retries it.

---

## 5. Host keys

Before it authenticates, rar2zip checks the server's host key:

| `host_key_check` | Unknown host | Changed key |
|------------------|--------------|-------------|
| `strict` (default) | refused | refused |
| `accept-new` | recorded in `known_hosts` and used | refused |
| `off` | used, with a warning on every connection | used |

A destination with `host_keys` is checked against those fingerprints only.
To trust a server under `strict`, add it to `known_hosts`:

```sh
ssh-keyscan -p 22 sftp.example.com >> ~/.ssh/known_hosts
ssh-keyscan -p 22 sftp.example.com | ssh-keygen -lf -   # fingerprint for host_keys
```

`fs-ext-rar2zip --config config.toml validate` lists destinations that
`strict` would refuse. Use the known_hosts file of the account `fsd` runs as,
or set `transfer.known_hosts`.

---

## 6. Transfer Strategies

### Disk strategy (default)

1. Convert source archive → `.zip` on local disk.
2. Upload `.zip` to each destination via SFTP.
3. Verify remote SHA-256 checksum.
4. Delete source archive and local `.zip`.

Used when: `file_size < stream_to_transfer_threshold_mb` OR
`destination_count > 1`.

### Stream strategy

1. Decompress source archive entirely into RAM as ZIP bytes.
2. Upload directly to SFTP without writing any ZIP to disk.
3. Verify remote SHA-256 checksum.
4. Delete source archive.

Used when: `file_size > stream_to_transfer_threshold_mb` AND
`destination_count == 1`.

> **Note:** The full ZIP must fit in RAM. For multi-destination setups the
> Disk strategy is always used (zip is converted once, sent N times).

### Multi-part RAR

Both new-style (`movie.part1.rar`, `movie.part2.rar`, …) and old-style
(`movie.rar`, `movie.r00`, `movie.r01`, …) multi-part archives are supported.
Processing starts only when all parts are present on disk. Incomplete sets are
expired after `multipart_timeout_hours`.

---

## 7. Monitoring

### Prometheus

Scrape `http://<host>:9090/metrics`:

```yaml
scrape_configs:
  - job_name: rar2zip
    static_configs:
      - targets: ['localhost:9090']
```

Available metrics:
- `rar2zip_files_processed_total` - counter
- `rar2zip_bytes_transferred_total` - counter
- `rar2zip_conversion_failures_total` - counter
- `rar2zip_transfer_failures_total` - counter
- `rar2zip_queue_depth` - gauge

### Dashboard

Open `http://localhost:8080/ui` in a browser. The dashboard auto-refreshes
every 5 seconds and shows:

- Queue depth by status (pending / converting / transferring / done / failed)
- Last 20 jobs with status badges
- Transfer statistics

### Health endpoint

`GET http://localhost:9090/health` returns:
```json
{"status": "ok", "queue_depth": 3}
```

---

## 8. Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `host key SHA256:... is not in <file>` | Server not in known_hosts under `strict` | Add it with `ssh-keyscan`, pin `host_keys`, or use `accept-new` once |
| `HOST KEY CHANGED` | Server key differs from known_hosts | Confirm the change with the server's owner, then remove the old line from known_hosts |
| `matches none of the pinned host_keys` | Wrong or outdated pin | Recheck the fingerprint (`ssh-keyscan ... \| ssh-keygen -lf -`) |
| SSH auth failure | Wrong key or user | Verify `ssh_key_path`, `user`, and that the public key is in `~/.ssh/authorized_keys` on the remote |
| `sha256sum: command not found` | Remote is not Linux | Install `coreutils` on the remote, or use a compatible `sha256sum` binary |
| Port already in use | Metrics/dashboard port conflict | Change `observability.metrics_port` or `observability.dashboard_port` |
| Jobs stuck in `converting` | Process crashed mid-job | Jobs in non-terminal states are automatically reset to `pending` on startup |
| Multi-part set never processed | Parts arrive slowly | Increase `multipart_timeout_hours`; check `watch_folder` for all parts |
