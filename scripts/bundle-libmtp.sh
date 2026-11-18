#!/bin/bash
# Puts Homebrew's libmtp and libusb in src-tauri/libs, for the app bundle to carry in
# Contents/Frameworks (tauri.macos.conf.json), so the app works without Homebrew. libmtp is
# pointed at the libusb next to it, and both are signed again (ad hoc) after the change.
# Both are LGPL-2.1: their licenses go in the bundle too, and they stay replaceable files.
set -euo pipefail
[ "$(uname)" = Darwin ] || exit 0
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/src-tauri/libs"
prefix="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
mtp="$prefix/opt/libmtp/lib/libmtp.9.dylib"
usb="$prefix/opt/libusb/lib/libusb-1.0.0.dylib"
if [ ! -f "$mtp" ] || [ ! -f "$usb" ]; then
  echo "libmtp isn't installed. Install it with: brew install libmtp" >&2
  exit 1
fi
rm -rf "$out"
mkdir -p "$out"
cp "$mtp" "$usb" "$out/"
chmod u+w "$out"/*.dylib
install_name_tool -id @rpath/libmtp.9.dylib "$out/libmtp.9.dylib"
install_name_tool -id @rpath/libusb-1.0.0.dylib "$out/libusb-1.0.0.dylib"
old="$(otool -L "$out/libmtp.9.dylib" | awk '/libusb-1\.0/ { print $1; exit }')"
install_name_tool -change "$old" @loader_path/libusb-1.0.0.dylib "$out/libmtp.9.dylib"
codesign --force --sign - "$out"/*.dylib
cp "$prefix/opt/libmtp/COPYING" "$out/libmtp-COPYING.txt"
cp "$prefix/opt/libusb/COPYING" "$out/libusb-COPYING.txt"
minos="$(vtool -show-build "$out/libmtp.9.dylib" 2>/dev/null | awk '/minos/ { print $2; exit }')"
echo "libmtp and libusb ready in $out (they need macOS ${minos:-?} or later)."
echo "Older Macs fall back to a Homebrew libmtp; for them, build libmtp with MACOSX_DEPLOYMENT_TARGET=11.0." 
