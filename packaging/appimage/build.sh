#!/usr/bin/env bash
# Package the release binary as an AppImage.
#
#   ./packaging/appimage/build.sh [output-dir]
#
# Expects `cargo build --release` to have run first. CEF is taken from the
# directory cef-dll-sys resolved, not from target/release.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

profile="${PROFILE:-release}"
name="pokem-online"
out="${1:-$root/dist}"
appdir="$out/Pokem Online.AppDir"
tool="${APPIMAGETOOL:-/mnt/arizen/Github/appimagetool}"

bin="$root/target/$profile/$name"
if [ ! -x "$bin" ]; then
  echo "missing $bin, run cargo build --$profile first" >&2
  exit 1
fi

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" \
         "$appdir/usr/share/applications" \
         "$appdir/usr/share/icons/hicolor/256x256/apps"

cp "$bin" "$appdir/usr/bin/"
cp -r assets "$appdir/usr/bin/assets"

cef_so="$root/target/$profile/libcef.so"
if [ ! -e "$cef_so" ]; then
  cef_so="$(find "target/$profile/build" -name libcef.so -print -quit)"
fi
if [ -z "$cef_so" ]; then
  echo "CEF runtime not found next to the binary or under target/$profile/build" >&2
  exit 1
fi
cef_dir="$(dirname "$cef_so")"
echo "CEF runtime: $cef_dir"

cp "$cef_dir"/*.so "$appdir/usr/bin/" 2>/dev/null || true
for pattern in '*.so.*' '*.pak' '*.dat' '*.bin' '*.json'; do
  for file in "$cef_dir"/$pattern; do
    if [ -e "$file" ]; then cp "$file" "$appdir/usr/bin/"; fi
  done
done
cp "$cef_dir/chrome-sandbox" "$appdir/usr/bin/"
cp -r "$cef_dir/locales" "$appdir/usr/bin/"

cp Icon/icon.png "$appdir/$name.png"
cp Icon/icon.png "$appdir/usr/share/icons/hicolor/256x256/apps/$name.png"
cp packaging/appimage/AppRun "$appdir/AppRun"
chmod 755 "$appdir/AppRun"
cp packaging/appimage/pokem-online.desktop "$appdir/"
cp packaging/appimage/pokem-online.desktop "$appdir/usr/share/applications/"

ARCH=x86_64 "$tool" --no-appstream "$appdir" "$out/$name-x86_64.AppImage"
echo "wrote $out/$name-x86_64.AppImage"
