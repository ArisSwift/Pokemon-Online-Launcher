#!/usr/bin/env bash
# Put the launcher where the desktop can find it: a binary in ~/.local/bin, an icon and a
# menu entry. Nothing outside the user's home is touched, and no root is needed.
set -eu

root="$(cd "$(dirname "$0")/../.." && pwd)"
image="${1:-$root/dist/pokem-online-x86_64.AppImage}"

if [ ! -f "$image" ]; then
  echo "No AppImage at $image" >&2
  echo "Download one from the release page, or build it with:" >&2
  echo "  APPIMAGETOOL=/path/to/appimagetool $root/packaging/appimage/build.sh" >&2
  exit 1
fi

# The icon is taken out from inside a temp folder, so the path has to survive a cd.
image="$(cd "$(dirname "$image")" && pwd)/$(basename "$image")"

bin="$HOME/.local/bin"
applications="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons"
target="$bin/pokem-online.AppImage"

mkdir -p "$bin" "$applications" "$icons"
install -m 755 "$image" "$target"
echo "launcher:  $target"

# The icon already lives inside the image, so take it out instead of keeping a second copy
# in the repo. Without it the menu entry still works, with a generic icon.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
(cd "$tmp" && "$image" --appimage-extract 'pokem-online.png' >/dev/null 2>&1) || true
# An absolute path is allowed in Icon=, and it avoids guessing which size folder a
# 1254px image belongs in.
icon="application-x-executable"
if [ -f "$tmp/squashfs-root/pokem-online.png" ]; then
  install -m 644 "$tmp/squashfs-root/pokem-online.png" "$icons/pokem-online.png"
  icon="$icons/pokem-online.png"
fi

cat > "$applications/pokem-online.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Pokem Online
Comment=Play pokem.onl with Discord presence
Exec=$target %U
Icon=$icon
Terminal=false
Categories=Game;
StartupWMClass=pokem-online
EOF
echo "menu entry: $applications/pokem-online.desktop"

# Both are optional: a desktop that has neither still shows the entry, just later.
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo
echo "Start it from your menu as Pokem Online, or run: $target"
echo "To undo all of it:"
echo "  rm '$target' '$applications/pokem-online.desktop' '$icons/pokem-online.png'"
