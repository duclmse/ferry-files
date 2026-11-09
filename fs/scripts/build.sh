#!/usr/bin/env bash
# Build fsd and fs in release mode. Extra arguments go to cargo, e.g.
#   scripts/build.sh --features fs-server/rar2zip
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> Building fsd and fs (release)"
cargo build --release -p fs-server -p fs-cli "$@"

echo "==> Done: target/release/fsd, target/release/fs"
