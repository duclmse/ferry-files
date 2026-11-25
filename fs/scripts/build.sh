#!/usr/bin/env bash
# Build fsd and fs in release mode. Extra arguments go to cargo, e.g.
#   scripts/build.sh -p fs-ext-rar2zip   # also build the rar2zip extension
#   scripts/build.sh -p fs-ext-sftpd     # also build the sftpd extension
#
# An fsd running from ../target/release is stopped first (scripts/stop.sh) and
# started again after with the same config (scripts/start.sh), so it runs
# the new build. FS_NO_RESTART=1 leaves it stopped.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

release="$(cd .. && pwd)/target/release/fsd"
running=""
config=""
for pid in $(pgrep -x fsd || true); do
    cmd="$(ps -o command= -p "$pid" 2>/dev/null || true)"
    case "$cmd" in
        "$release "* | "../target/release/fsd "*)
            running="$pid"
            config="$(sed -nE 's/.*(--config|-c) +([^ ]+).*/\2/p' <<<"$cmd")"
            ;;
    esac
done
if [ -n "$running" ]; then
    echo "==> fsd from ../target/release is running; stopping it for the build"
    FS_FORCE=1 FS_PIDS="$running" scripts/stop.sh
fi

echo "==> Building fsd and fs (release)"
status=0
cargo build --release -p fs-server -p fs-cli "$@" || status=$?

if [ -n "$running" ] && [ "${FS_NO_RESTART:-0}" != 1 ]; then
    echo "==> Starting fsd again"
    FS_BIN="$release" FS_CONFIG="${config:-${FS_CONFIG:-}}" scripts/start.sh
fi

[ "$status" = 0 ] || exit "$status"
echo "==> Done: ../target/release/fsd, ../target/release/fs"
