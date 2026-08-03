#!/usr/bin/env bash
# Run filesearch in the foreground (direct mode, not via systemd/launchd).
# Override discovery with FILESEARCH_BIN / FILESEARCH_CONFIG.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

BIN="${FILESEARCH_BIN:-}"
if [ -z "$BIN" ]; then
    if [ -x "target/release/filesearch" ]; then
        BIN="target/release/filesearch"
    elif command -v filesearch >/dev/null 2>&1; then
        BIN="$(command -v filesearch)"
    else
        echo "filesearch binary not found. Run scripts/build.sh or scripts/install.sh first." >&2
        exit 1
    fi
fi

CONFIG="${FILESEARCH_CONFIG:-}"
if [ -z "$CONFIG" ]; then
    for candidate in "./config.toml" "$HOME/.config/filesearch/config.toml" "/etc/filesearch/config.toml"; do
        if [ -f "$candidate" ]; then
            CONFIG="$candidate"
            break
        fi
    done
fi
if [ -z "$CONFIG" ]; then
    echo "No config.toml found. Run scripts/install.sh, or set FILESEARCH_CONFIG=<path>." >&2
    exit 1
fi

echo "==> Binary: $BIN"
echo "==> Config: $CONFIG"
exec "$BIN" --config "$CONFIG" daemon
