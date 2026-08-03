#!/usr/bin/env bash
# Build filesearch (and rar2zip on Windows) in release mode.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

os="$(uname -s)"

echo "==> Building filesearch (release)"
cargo build --release -p filesearch

case "$os" in
    Linux|Darwin)
        echo "==> Skipping rar2zip: it uses the Windows Service API and only builds on Windows"
        ;;
    *)
        echo "==> Unrecognized platform '$os'; skipping rar2zip"
        ;;
esac

echo "==> Done: target/release/filesearch"
