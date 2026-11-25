#!/usr/bin/env bash
# Start fsd unless it is already running.
#
#   - Installed as a service (systemd unit, launchd job): starts it if stopped.
#   - Otherwise: starts `fsd daemon` in the background and waits until it
#     answers over IPC. Stop it with scripts/stop.sh (a clean stop).
#
# Override discovery with FS_BIN / FS_CONFIG, as for run.sh; FS_WAIT sets how
# many seconds to wait (default 30).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
WAIT="${FS_WAIT:-30}"

# `fsd install-service` names the unit fsd and the launchd job fs.fsd;
# scripts/install.sh names them fs and com.fs.daemon.
if command -v systemctl >/dev/null 2>&1; then
    for unit in fsd fs; do
        for scope in --user --system; do
            systemctl "$scope" cat "$unit.service" >/dev/null 2>&1 || continue
            if systemctl "$scope" is-active --quiet "$unit.service"; then
                echo "==> fsd is running as $unit.service ($scope)"
                exit 0
            fi
            echo "==> Starting $unit.service ($scope)"
            if [ "$scope" = --system ] && [ "$(id -u)" -ne 0 ]; then
                sudo systemctl start "$unit.service"
            else
                systemctl "$scope" start "$unit.service"
            fi
            exit 0
        done
    done
fi
if [ "$(uname -s)" = Darwin ]; then
    for label in fs.fsd com.fs.daemon; do
        for domain in "gui/$(id -u)" system; do
            info="$(launchctl print "$domain/$label" 2>/dev/null)" || continue
            if grep -q 'state = running' <<<"$info"; then
                echo "==> fsd is running as the launchd job $domain/$label"
                exit 0
            fi
            echo "==> Starting the launchd job $domain/$label"
            if [ "$domain" = system ] && [ "$(id -u)" -ne 0 ]; then
                sudo launchctl kickstart "$domain/$label"
            else
                launchctl kickstart "$domain/$label"
            fi
            exit 0
        done
    done
fi

if pids="$(pgrep -x fsd)"; then
    echo "==> fsd is already running (PID $(echo $pids | tr ' ' ','))"
    exit 0
fi

BIN="${FS_BIN:-}"
if [ -z "$BIN" ]; then
    if [ -x "target/release/fsd" ]; then
        BIN="target/release/fsd"
    elif command -v fsd >/dev/null 2>&1; then
        BIN="$(command -v fsd)"
    else
        echo "fsd not found. Run scripts/build.sh or scripts/install.sh first." >&2
        exit 1
    fi
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

CONFIG="${FS_CONFIG:-}"
if [ -z "$CONFIG" ]; then
    for candidate in "./config.toml" "$HOME/.config/fs/config.toml" "/etc/fs/config.toml"; do
        if [ -f "$candidate" ]; then
            CONFIG="$candidate"
            break
        fi
    done
fi
if [ -z "$CONFIG" ]; then
    echo "No config.toml found. Run scripts/install.sh, or set FS_CONFIG=<path>." >&2
    exit 1
fi

if ! "$BIN" --config "$CONFIG" validate-config >/dev/null; then
    echo "$CONFIG is not valid; see: $BIN --config $CONFIG validate-config" >&2
    exit 1
fi

# fsd logs to the config's log_dir; this catches only what it prints before
# its logger is up, such as a config it cannot read.
OUT="${TMPDIR:-/tmp}/fsd.out"
echo "==> Starting $BIN with $CONFIG"
nohup "$BIN" --config "$CONFIG" daemon >"$OUT" 2>&1 </dev/null &
PID=$!

FS="$(dirname "$BIN")/fs"
for _ in $(seq $((WAIT * 2))); do
    sleep 0.5
    if ! kill -0 "$PID" 2>/dev/null; then
        echo "fsd exited. See $OUT and the config's log_dir." >&2
        exit 1
    fi
    [ -x "$FS" ] || break
    if "$FS" --config "$CONFIG" stats >/dev/null 2>&1; then
        echo "==> fsd is running (PID $PID) and answering"
        exit 0
    fi
done
if [ -x "$FS" ]; then
    # IPC may be off in the config (api.ipc_enabled = false).
    echo "warning: fsd is running (PID $PID) but did not answer over IPC within $WAIT s" >&2
else
    echo "==> fsd is running (PID $PID)"
fi
