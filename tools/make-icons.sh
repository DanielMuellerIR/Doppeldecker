#!/bin/sh
# Erzeugt die App-Icons aus der gewählten PNG-Quelle neu (nur bei Icon-Änderungen nötig):
#   - src-tauri/icons/icon.png  — 1024×1024, u. a. für das Linux-Bundle
#   - src-tauri/icons/icon.icns — macOS-Icon mit allen Größen inkl. Retina;
#     der Tauri-Bundler kann aus einem einzelnen 1024er-PNG kein icns bauen
#     (1024 existiert im icns-Format nur als „512@2x“), deshalb bauen wir es
#     hier selbst über iconutil.
# Die Quadratquellen enthalten rundherum echten Himmel. Der Renderer schneidet
# daraus abgerundete Icons ohne Unterlage oder farbigen Rand.
# Benötigt rsvg-convert (brew install librsvg) und macOS-iconutil.
set -eu

cd "$(dirname "$0")/../src-tauri/icons"

source="icon-source.png"
renderer="icon-render.svg"
test -f "$source"
test -f "$renderer"
tmp_root="$(mktemp -d)"
trap 'rm -r "$tmp_root"' EXIT

rsvg-convert -w 1024 -h 1024 "$renderer" -o "$tmp_root/icon.png"
mv "$tmp_root/icon.png" icon.png

# iconutil erwartet ein .iconset-Verzeichnis mit fest benannten Größen.
tmp="$tmp_root/icon.iconset"
mkdir -p "$tmp"
for s in 16 32 128 256 512; do
  d=$((s * 2))
  rsvg-convert -w "$s" -h "$s" "$renderer" -o "$tmp/icon_${s}x${s}.png"
  rsvg-convert -w "$d" -h "$d" "$renderer" -o "$tmp/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$tmp" -o "$tmp_root/icon.icns"
mv "$tmp_root/icon.icns" icon.icns

# Dieselbe Maske gilt für alle zur Laufzeit wählbaren Dock-/Fenster-Icons.
for name in blue sunset aurora turquoise rose storm; do
  # librsvg erlaubt Bildreferenzen nur innerhalb des SVG-Verzeichnisses.
  # Quelle und Renderer deshalb gemeinsam im eigenen Temp-Verzeichnis halten.
  cp "../../design/icons/$name.png" "$tmp_root/icon-source.png"
  cp "$renderer" "$tmp_root/render.svg"
  rsvg-convert -w 512 -h 512 "$tmp_root/render.svg" -o "variants/$name.png"
done

echo "OK: icon.png und icon.icns neu erzeugt."
