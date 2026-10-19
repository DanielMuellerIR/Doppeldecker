#!/bin/sh
# Prüft App und DMG nach dem Tauri-Bundle-Schritt. Ohne --allow-adhoc gilt nur ein
# Developer-ID-signiertes und notariell gestapeltes öffentliches Artefakt als Erfolg.
set -eu

allow_adhoc=false
case "${1:-}" in
  --allow-adhoc) allow_adhoc=true ;;
  "") ;;
  *) printf 'Verwendung: %s [--allow-adhoc]\n' "$0" >&2; exit 2 ;;
esac

if [ "$(uname -s)" != "Darwin" ]; then
  printf 'macOS-Artefakte können nur auf macOS verifiziert werden.\n' >&2
  exit 2
fi

version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
app="target/release/bundle/macos/Doppeldecker.app"
updater_archive=
if [ "$allow_adhoc" != true ]; then
  set -- target/release/bundle/macos/*.app.tar.gz
  if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
    printf 'Genau ein macOS-Updaterarchiv wurde erwartet.\n' >&2
    exit 1
  fi
  updater_archive=$1
  if [ ! -s "$updater_archive.sig" ]; then
    printf 'Signatur des macOS-Updaterarchivs fehlt oder ist leer.\n' >&2
    exit 1
  fi
fi
set -- target/release/bundle/dmg/Doppeldecker_"$version"_*.dmg
if [ "$#" -ne 1 ] || [ ! -f "$1" ]; then
  printf 'Genau ein DMG für Version %s wurde erwartet.\n' "$version" >&2
  exit 1
fi
dmg=$1

codesign --verify --deep --strict --verbose=2 "$app"
if [ ! -x "$app/Contents/MacOS/doppeldecker-cli" ]; then
  printf 'Mitgeliefertes doppeldecker-cli fehlt im App-Bundle.\n' >&2
  exit 1
fi
signature=$(codesign -dv --verbose=4 "$app" 2>&1)
printf '%s\n' "$signature" | grep -q 'Identifier=com.doppeldecker.desktop'

# Nicht nur die Mergequelle, sondern das tatsächlich ausgelieferte Bundle muss den
# erklärten LAN-Zweck und genau die drei vom Core gesuchten Bonjour-Typen tragen.
info="$app/Contents/Info.plist"
lan_description=$(/usr/libexec/PlistBuddy -c 'Print :NSLocalNetworkUsageDescription' "$info")
[ -n "$lan_description" ]
[ "$(/usr/libexec/PlistBuddy -c 'Print :NSBonjourServices:0' "$info")" = '_smb._tcp' ]
[ "$(/usr/libexec/PlistBuddy -c 'Print :NSBonjourServices:1' "$info")" = '_sftp-ssh._tcp' ]
[ "$(/usr/libexec/PlistBuddy -c 'Print :NSBonjourServices:2' "$info")" = '_webdav._tcp' ]
if /usr/libexec/PlistBuddy -c 'Print :NSBonjourServices:3' "$info" >/dev/null 2>&1; then
  printf 'Das App-Bundle deklariert unerwartete zusätzliche Bonjour-Dienste.\n' >&2
  exit 1
fi
if printf '%s\n' "$signature" | grep -q '^Signature=adhoc$'; then
  if [ "$allow_adhoc" != true ]; then
    printf 'Öffentliche Verifikation lehnt eine Ad-hoc-Signatur ab.\n' >&2
    exit 1
  fi
  signature_label='Ad-hoc (nur Entwicklung)'
else
  # `grep -qv` war hier wirkungslos: Es ist schon wahr, sobald IRGENDEINE Zeile der
  # rund zwanzigzeiligen codesign-Ausgabe die Zeichenkette nicht enthält. Gemeint war
  # „die Ausgabe enthält sie nirgends".
  if printf '%s\n' "$signature" | grep -q 'TeamIdentifier=not set'; then
    printf 'Signatur ohne Team-Identifier ist keine Developer-ID-Signatur.\n' >&2
    exit 1
  fi
  signature_label='Developer ID'
fi

hdiutil verify "$dmg" >/dev/null
mountpoint=$(mktemp -d /tmp/doppeldecker-dmg-verify.XXXXXX)
updater_temp=
device=
attach_plist=
# Reiner Not-Cleanup für Abbruch und Fehlerpfad. Der reguläre Weg hängt weiter unten
# GEPRÜFT das eigene Device aus, bevor die Erfolgsmeldung erscheint.
cleanup() {
  if [ -n "$device" ]; then
    hdiutil detach "$device" -force >/dev/null 2>&1 || true
  elif [ -n "$attach_plist" ]; then
    # Auch eine fehlgeschlagene Mountpoint-Zuordnung berechtigt nur zum
    # Aushängen der Devices aus genau unserer eigenen Attach-Antwort.
    printf '%s' "$attach_plist" | python3 -c '
import plistlib, subprocess, sys
try:
    entities = plistlib.loads(sys.stdin.buffer.read()).get("system-entities", [])
except Exception:
    sys.exit(0)
for entity in entities:
    device = entity.get("dev-entry")
    if device:
        subprocess.run(["hdiutil", "detach", device, "-force"], capture_output=True)
' || true
  fi
  rmdir "$mountpoint" >/dev/null 2>&1 || true
  case "$updater_temp" in
    /tmp/doppeldecker-updater-verify.*) rm -rf -- "$updater_temp" ;;
  esac
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$allow_adhoc" != true ]; then
  updater_temp=$(mktemp -d /tmp/doppeldecker-updater-verify.XXXXXX)
  # Das Updaterarchiv wird erst nach dem Stapeln neu gepackt. Geprüft wird nicht
  # nur die TAR-Struktur, sondern die tatsächlich extrahierte App samt
  # vollständigem CLI-Sidecar. Symlinks sind für beide Vertragsdateien unzulässig.
  tar -xzf "$updater_archive" -C "$updater_temp"
  updater_app="$updater_temp/Doppeldecker.app"
  updater_gui="$updater_app/Contents/MacOS/doppeldecker"
  updater_cli="$updater_app/Contents/MacOS/doppeldecker-cli"
  for binary in "$updater_gui" "$updater_cli"; do
    if [ ! -f "$binary" ] || [ -L "$binary" ]; then
      printf 'Updaterarchiv enthält keine reguläre Mach-O-Datei: %s\n' "$binary" >&2
      exit 1
    fi
    archs=$(lipo -archs "$binary")
    case " $archs " in
      *" arm64 "*) ;;
      *) printf 'Updaterarchiv-Binary ist nicht arm64: %s (%s)\n' "$binary" "$archs" >&2; exit 1 ;;
    esac
  done
  codesign --verify --deep --strict --verbose=2 "$updater_app"
  updater_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$updater_app/Contents/Info.plist")
  if [ "$updater_version" != "$version" ]; then
    printf 'Updaterarchiv-Version %s weicht von %s ab.\n' "$updater_version" "$version" >&2
    exit 1
  fi
  xcrun stapler validate "$updater_app"
  spctl -a -t exec -vv "$updater_app"
  if ! diff -qr "$app" "$updater_app" >/dev/null; then
    printf 'App-Bundle und Updaterarchiv-Inhalt unterscheiden sich.\n' >&2
    exit 1
  fi
fi
# `-plist` nennt das konkrete Device dieses Attach-Vorgangs. Nur dieses Device wird
# später ausgehängt — nie ein Pfad, hinter dem inzwischen etwas anderes hängen könnte.
attach_plist=$(hdiutil attach -readonly -nobrowse -mountpoint "$mountpoint" "$dmg" -plist)
device=$(printf '%s' "$attach_plist" | python3 -c '
import os, plistlib, sys
wanted = os.path.realpath(sys.argv[1])
for entity in plistlib.loads(sys.stdin.buffer.read()).get("system-entities", []):
    point = entity.get("mount-point")
    if point and os.path.realpath(point) == wanted and entity.get("dev-entry"):
        print(entity["dev-entry"])
        break
else:
    sys.exit(1)
' "$mountpoint")

# Hängt genau das eigene Device aus: erst regulär, dann — falls Finder oder Indexer
# den Mount kurz offen halten — einmal erzwungen.
detach_own_device() {
  hdiutil detach "$device" >/dev/null 2>&1 && return 0
  sleep 2
  hdiutil detach "$device" -force >/dev/null 2>&1
}
embedded="$mountpoint/Doppeldecker.app"
codesign --verify --deep --strict --verbose=2 "$embedded"
[ -x "$embedded/Contents/MacOS/doppeldecker-cli" ]
embedded_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$embedded/Contents/Info.plist")
embedded_version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$embedded/Contents/Info.plist")
[ "$embedded_id" = 'com.doppeldecker.desktop' ]
[ "$embedded_version" = "$version" ]
# Der echte Sandbox-Smoke-Test startet das erzeugte App-Bundle. Nur ein vollständig
# bytegleiches DMG-Bundle darf diesen Beleg für das ausgelieferte Artefakt übernehmen.
if ! diff -qr "$app" "$embedded" >/dev/null; then
  printf 'App-Bundle und DMG-Inhalt unterscheiden sich.\n' >&2
  exit 1
fi

if [ "$allow_adhoc" != true ]; then
  xcrun stapler validate "$app"
  xcrun stapler validate "$dmg"
  # Zusätzlich die im DMG montierte App direkt belegen: Ticket und
  # Gatekeeper-Bewertung des Bundles, das Nutzer tatsächlich herausziehen.
  xcrun stapler validate "$embedded"
  spctl -a -t exec -vv "$embedded"
fi

# Erst aushängen, dann Erfolg melden — und ein gescheitertes Aushängen ist ein
# Fehlschlag. Vorher verwarf allein der EXIT-Trap den Fehler mit `|| true`, sodass die
# Erfolgsmeldung auch über einem hängengebliebenen Mount stand.
if ! detach_own_device; then
  printf 'Eigenes DMG-Device %s konnte nicht ausgehängt werden.\n' "$device" >&2
  exit 1
fi
device=
attach_plist=
rmdir "$mountpoint" >/dev/null 2>&1 || true

printf 'macOS-Bundle grün: %s · %s · %s\n' "$version" "$signature_label" "$dmg"
