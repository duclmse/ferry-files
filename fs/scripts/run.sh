#!/usr/bin/env bash
# Run fs in the foreground (direct mode, not via systemd/launchd).
# Override discovery with FS_BIN / FS_CONFIG.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

BIN="${FS_BIN:-}"
if [ -z "$BIN" ]; then
    if [ -x "target/release/fsd" ]; then
        BIN="target/release/fsd"
    elif command -v fsd >/dev/null 2>&1; then
        BIN="$(command -v fsd)"
    else
        echo "fs binary not found. Run scripts/build.sh or scripts/install.sh first." >&2
        exit 1
    fi
fi

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

echo "==> Binary: $BIN"
echo "==> Config: $CONFIG"
exec "$BIN" --config "$CONFIG" daemon
