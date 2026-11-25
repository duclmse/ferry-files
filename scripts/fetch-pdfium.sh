#!/bin/bash
# Puts a prebuilt PDFium (bblanchon/pdfium-binaries) in src-tauri/libs/pdfium, for the PDF preview
# to render pages with. The app bundle carries it (tauri.<os>.conf.json); a debug build run from
# the source tree finds it there too. The version matches pdfium-render's `pdfium_8125` bindings.
# PDFium is BSD-3-Clause (with Apache and other third-party licenses): they go in the bundle too.
set -euo pipefail
version="${PDFIUM_VERSION:-8125}"
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/src-tauri/libs/pdfium"
case "$(uname -s)" in
  Darwin) asset=pdfium-mac-univ.tgz; lib=lib/libpdfium.dylib ;;
  Linux)
    case "$(uname -m)" in
      aarch64 | arm64) asset=pdfium-linux-arm64.tgz ;;
      *) asset=pdfium-linux-x64.tgz ;;
    esac
    lib=lib/libpdfium.so
    ;;
  MINGW* | MSYS* | CYGWIN*)
    case "${PROCESSOR_ARCHITECTURE:-AMD64}" in
      ARM64) asset=pdfium-win-arm64.tgz ;;
      *) asset=pdfium-win-x64.tgz ;;
    esac
    lib=bin/pdfium.dll
    ;;
  *) echo "No prebuilt PDFium for $(uname -s)." >&2; exit 1 ;;
esac
if [ -f "$out/$lib" ] && [ "$(cat "$out/VERSION.txt" 2>/dev/null)" = "$version $asset" ]; then
  echo "PDFium $version already in $out."
  exit 0
fi
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 -o "$tmp/pdfium.tgz" "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$version/$asset"
mkdir "$tmp/x"
tar -xzf "$tmp/pdfium.tgz" -C "$tmp/x"
rm -rf "$out"
mkdir -p "$out"
cp "$tmp/x/$lib" "$out/"
cp "$tmp/x/LICENSE" "$out/PDFium-LICENSE.txt"
# The third-party licenses PDFium is built with, in one file for the bundle.
if [ -d "$tmp/x/licenses" ]; then
  for f in "$tmp/x/licenses"/*; do
    printf '==== %s ====\n\n' "$(basename "$f")"
    cat "$f"
    printf '\n\n'
  done > "$out/PDFium-third-party-LICENSES.txt"
else
  : > "$out/PDFium-third-party-LICENSES.txt"
fi
if [ "$(uname -s)" = Darwin ]; then
  chmod u+w "$out/libpdfium.dylib"
  install_name_tool -id @rpath/libpdfium.dylib "$out/libpdfium.dylib"
  codesign --force --sign - "$out/libpdfium.dylib"
fi
echo "$version $asset" > "$out/VERSION.txt"
echo "PDFium $version ready in $out."
