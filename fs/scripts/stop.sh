#!/usr/bin/env bash
# Stop fsd, cleanly, wherever it runs.
#
#   - Installed as a service (systemd unit, launchd job): stops it.
#   - A background fsd (scripts/start.sh): sends it SIGTERM, which lets it
#     save its index first, as `fsd stop` does.
#
# FS_WAIT sets how many seconds to wait (default 60). FS_FORCE=1 kills an
# fsd still running after that; a killed memory store loses what changed
# since its last snapshot and catches up on the next start. FS_PIDS (space
# separated) stops only those processes and leaves services alone.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
WAIT="${FS_WAIT:-60}"

as_scope() {
    if [ "$1" = --system ] && [ "$(id -u)" -ne 0 ]; then
        sudo "${@:2}"
    else
        "${@:2}"
    fi
}

# `fsd install-service` names the unit fsd and the launchd job fs.fsd;
# scripts/install.sh names them fs and com.fs.daemon.
if [ -z "${FS_PIDS:-}" ] && command -v systemctl >/dev/null 2>&1; then
    for unit in fsd fs; do
        for scope in --user --system; do
            systemctl "$scope" is-active --quiet "$unit.service" 2>/dev/null || continue
            echo "==> Stopping $unit.service ($scope)"
            as_scope "$scope" systemctl "$scope" stop "$unit.service"
        done
    done
fi
if [ -z "${FS_PIDS:-}" ] && [ "$(uname -s)" = Darwin ]; then
    for label in fs.fsd com.fs.daemon; do
        for domain in "gui/$(id -u)" system; do
            info="$(launchctl print "$domain/$label" 2>/dev/null)" || continue
            grep -q 'state = running' <<<"$info" || continue
            echo "==> Stopping the launchd job $domain/$label"
            # A clean exit is not restarted (KeepAlive SuccessfulExit false).
            if [ "$domain" = system ]; then scope=--system; else scope=--user; fi
            as_scope "$scope" launchctl kill SIGTERM "$domain/$label"
        done
    done
fi

pids="${FS_PIDS:-$(pgrep -x fsd || true)}"
if [ -z "$pids" ]; then
    echo "==> fsd is not running"
    exit 0
fi
for pid in $pids; do
    echo "==> Stopping fsd (PID $pid)"
    kill -TERM "$pid" 2>/dev/null || true
done

failed=0
for pid in $pids; do
    for _ in $(seq $((WAIT * 5))); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        if [ "${FS_FORCE:-0}" = 1 ]; then
            echo "warning: killing fsd (PID $pid)" >&2
            kill -KILL "$pid" 2>/dev/null || true
        else
            echo "warning: fsd (PID $pid) is still running; FS_FORCE=1 kills it" >&2
            failed=1
        fi
    fi
done
[ "$failed" = 0 ] || exit 1
echo "==> fsd is stopped"
