#!/bin/bash
# Cargo's runner on macOS (.cargo/config.toml). A bare binary shows in the Dock, ⌘Tab and
# Activity Monitor under its file name, ferry-files; run from inside an app bundle it shows as
# Ferry Files, like the built app. So the app binary is linked into target/<profile>/Ferry Files.app
# and run from there. Anything else cargo runs (tests, examples) is run as it is.
#
# The bundle has no CFBundleIdentifier on purpose: the web view then keeps keying its data
# (~/Library/WebKit/ferry-files) by the binary's name, as before, instead of sharing the
# built app's.
set -euo pipefail
if [ $# -eq 0 ]; then
  echo "usage: $0 <binary> [args...]" >&2
  echo "Cargo runs this for you on macOS: use cargo tauri dev (or cargo run in src-tauri)." >&2
  exit 2
fi
bin="$1"
shift
if [ "$(basename "$bin")" != ferry-files ]; then
  exec "$bin" "$@"
fi
app="$(dirname "$bin")/Ferry Files.app/Contents"
mkdir -p "$app/MacOS"
# A hard link, not a symlink: the app finds its bundle from the path it was started by.
ln -f "$bin" "$app/MacOS/ferry-files"
cat > "$app/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Ferry Files</string>
  <key>CFBundleDisplayName</key><string>Ferry Files</string>
  <key>CFBundleExecutable</key><string>ferry-files</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
exec "$app/MacOS/ferry-files" "$@"
