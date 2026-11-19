#!/usr/bin/env bash
# Build fsd and fs in release mode. Extra arguments go to cargo, e.g.
#   scripts/build.sh -p fs-ext-rar2zip   # also build the rar2zip extension
#   scripts/build.sh -p fs-ext-sftpd     # also build the sftpd extension
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> Building fsd and fs (release)"
cargo build --release -p fs-server -p fs-cli "$@"

echo "==> Done: target/release/fsd, target/release/fs"
