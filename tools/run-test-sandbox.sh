#!/bin/sh
# Startet Doppeldecker ausschließlich in einem frisch erzeugten Wegwerfbereich.
# Der Ordner wird nach dem App-Ende absichtlich NICHT automatisch gelöscht: So können
# Tester Quarantäne und Resultate prüfen und ihn anschließend über den Papierkorb entfernen.
set -eu

release_bundle=false
installed_bundle=false
reuse_sandbox=
prebuilt_binary=
start_via_launchservices=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --release)
      release_bundle=true
      shift
      ;;
    --installed)
      installed_bundle=true
      shift
      ;;
    --open)
      start_via_launchservices=true
      shift
      ;;
    --binary)
      if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        printf 'Für --binary fehlt der Pfad des fertigen Programms.\n' >&2
        exit 2
      fi
      prebuilt_binary=$2
      shift 2
      ;;
    --reuse)
      if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        printf 'Für --reuse fehlt der markierte Testbereich.\n' >&2
        exit 2
      fi
      reuse_sandbox=$2
      shift 2
      ;;
    --help|-h)
      printf 'Verwendung: %s [--release|--installed|--binary <Programm>] [--open] [--reuse <Testbereich>]\n' "$0"
      printf '  --release  gebündeltes macOS-.app beziehungsweise Linux-AppImage starten\n'
      printf '  --installed unter Linux das installierte DEB-Programm starten\n'
      printf '  --binary   fertiges Programm; vorhandenes signiertes .app unverändert starten\n'
      printf '  --open     neue App-Instanz über LaunchServices mit Test-Sandbox starten\n'
      printf '             (nötig beim Start über SSH; nur macOS, auch mit --release)\n'
      printf '  --reuse    vorhandenen markierten Testbereich für Recovery erneut öffnen\n'
      exit 0
      ;;
    *)
      printf 'Unbekannte Option: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

if [ "$release_bundle" = true ] && [ "$installed_bundle" = true ]; then
  printf '%s\n' '--release und --installed schließen einander aus.' >&2
  exit 2
fi
# `--binary` startet ein anderswo gebautes Programm — etwa eines, das für den
# Linux-Testhost in einem Container entstanden ist, weil dort die Dev-Bibliotheken
# der GUI fehlen. Es bleibt dieselbe markierte Sandbox und dieselbe Testbremse; nur
# der Übersetzungsschritt entfällt. Deshalb schließt es die beiden anderen
# Programmquellen aus.
if [ -n "$prebuilt_binary" ] &&
   { [ "$release_bundle" = true ] || [ "$installed_bundle" = true ]; }; then
  printf '%s\n' '--binary schließt --release und --installed aus.' >&2
  exit 2
fi
if [ -n "$prebuilt_binary" ]; then
  if [ ! -f "$prebuilt_binary" ] || [ ! -x "$prebuilt_binary" ]; then
    printf 'Angegebenes Programm fehlt oder ist nicht ausführbar: %s\n' \
      "$prebuilt_binary" >&2
    exit 2
  fi
  # Der spätere Start läuft aus dem Wegwerfbereich heraus; ein relativer Pfad zeigte
  # danach ins Leere.
  prebuilt_binary=$(CDPATH= cd -- "$(dirname -- "$prebuilt_binary")" && pwd -P)/$(basename -- "$prebuilt_binary")
fi
platform=$(uname -s)
if [ "$installed_bundle" = true ] && [ "$platform" != "Linux" ]; then
  printf '%s\n' '--installed wird nur unter Linux unterstützt.' >&2
  exit 2
fi
if [ "$start_via_launchservices" = true ] && [ "$platform" != "Darwin" ]; then
  printf '%s\n' '--open wird nur unter macOS unterstützt.' >&2
  exit 2
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(dirname "$script_dir")
temp_base=${TMPDIR:-/tmp}
if [ -n "$reuse_sandbox" ]; then
  if [ ! -d "$reuse_sandbox" ]; then
    printf 'Testbereich existiert nicht oder ist kein Ordner.\n' >&2
    exit 2
  fi
  sandbox=$(CDPATH= cd -- "$reuse_sandbox" && pwd -P)
  if [ ! -f "$sandbox/.doppeldecker-test-sandbox" ] ||
     [ ! -d "$sandbox/quelle" ] || [ ! -d "$sandbox/ziel" ]; then
    printf 'Wiederanlauf verlangt Marker sowie quelle/ und ziel/.\n' >&2
    exit 2
  fi
else
  sandbox=$(mktemp -d "$temp_base/doppeldecker-test.XXXXXX")
  printf 'DOPPELDECKER TEST SANDBOX\n' > "$sandbox/.doppeldecker-test-sandbox"
  mkdir -p "$sandbox/quelle" "$sandbox/ziel"
fi

finish() {
  printf '\nTestbereich bleibt zur Kontrolle erhalten:\n%s\n' "$sandbox"
  printf 'Nach der Kontrolle bitte über den Papierkorb entfernen.\n'
}
trap finish EXIT

printf 'Doppeldecker-Testbereich:\n%s\n\n' "$sandbox"
cd "$project_dir"
if [ "$platform" = "Darwin" ]; then
  existing_bundle=
  if [ -n "$prebuilt_binary" ]; then
    executable_dir=$(dirname -- "$prebuilt_binary")
    contents_dir=$(dirname -- "$executable_dir")
    candidate=$(dirname -- "$contents_dir")
    case "$candidate" in
      *.app)
        if [ "$(basename -- "$executable_dir")" = MacOS ] &&
           [ "$(basename -- "$contents_dir")" = Contents ]; then
          # Ein signiertes Bundle darf weder neu verpackt noch über seine Plist
          # verändert werden. Auch ein unpassender Executable-Eintrag ist ein Fehler,
          # kein Anlass, die Signatur durch einen Debugwrapper zu umgehen.
          python3 - "$candidate/Contents/Info.plist" "$(basename -- "$prebuilt_binary")" <<'PYEXEC'
import plistlib
import sys

with open(sys.argv[1], "rb") as source:
    document = plistlib.load(source)
if document.get("CFBundleExecutable") != sys.argv[2]:
    sys.exit("CFBundleExecutable stimmt nicht mit --binary überein.")
PYEXEC
          codesign --verify --deep --strict "$candidate"
          existing_bundle=$candidate
        fi
        ;;
    esac
  fi
  if [ -n "$existing_bundle" ]; then
    bundle=$existing_bundle
  elif [ "$release_bundle" = true ]; then
    # Der Release-Smoke-Test verwendet exakt das von Tauri erzeugte und signierte
    # Bundle. Der Aufrufer baut es vorher über `pnpm bundle:macos`.
    bundle="$project_dir/target/release/bundle/macos/Doppeldecker.app"
    if [ ! -x "$bundle/Contents/MacOS/doppeldecker" ]; then
      printf 'Release-Bundle fehlt. Zuerst `pnpm bundle:macos` ausführen.\n' >&2
      exit 1
    fi
    codesign --verify --deep --strict "$bundle"
  else
    # Ein nacktes Cargo-Binary besitzt unter macOS keine vollständige App-Präsenz und
    # lässt sich deshalb nicht zuverlässig über Bedienungshilfen testen. Das kleine,
    # nur im Wegwerfbereich erzeugte Bundle verwendet exakt denselben Debug-Build --
    # beziehungsweise mit `--binary` genau das übergebene Programm.
    if [ -n "$prebuilt_binary" ]; then
      program=$prebuilt_binary
    else
      cargo build -p doppeldecker
      program="$project_dir/target/debug/doppeldecker"
    fi
    # Recovery darf ein vorhandenes Test-Bundle nicht erneut beschreiben:
    # Symlinks oder Hardlinks darin könnten auf fremde Dateien zeigen. Ein frisch
    # exklusiv angelegter Elternordner bindet alle folgenden Bundle-Schreibpfade.
    bundle_parent=$(mktemp -d "$sandbox/.doppeldecker-test-bundle.XXXXXX")
    bundle="$bundle_parent/Doppeldecker-Test.app"
    mkdir -p "$bundle/Contents/MacOS"
    cp "$program" "$bundle/Contents/MacOS/doppeldecker"
    # Nur der rohe Debugbuild erhält einen eigenen Wegwerfwrapper. Die Umgebung
    # bleibt auch beim direkten Start darin nachvollziehbar; LaunchServices erhält
    # dieselben eng benannten Werte ausdrücklich über --env.
    python3 - "$bundle/Contents/Info.plist" "$sandbox" "$project_dir/src-tauri/Info.plist" <<'PYPLIST'
import plistlib
import sys

# Gleiche Datenschutzdeklarationen; fehlende Usage-Texte lassen TCC-Zugriffe abbrechen.
plist_path, sandbox_path, app_plist_path = sys.argv[1:]
with open(app_plist_path, "rb") as source:
    document = plistlib.load(source)
with open(plist_path, "wb") as target:
    document.update({
        "CFBundleExecutable": "doppeldecker",
        "CFBundleIdentifier": "com.doppeldecker.test",
        "CFBundleName": "Doppeldecker Test",
        "CFBundlePackageType": "APPL",
        "LSEnvironment": {
            "DOPPELDECKER_SANDBOX": sandbox_path,
            "DOPPELDECKER_TEST_COPY_DELAY_MS": "5",
        },
    })
    plistlib.dump(document, target)
PYPLIST
  fi
  launch_program="$bundle/Contents/MacOS/doppeldecker"
  if [ -n "$existing_bundle" ]; then
    launch_program=$prebuilt_binary
  fi
  # Fünf Millisekunden pro Fortschrittsblock machen den Abbruch einer großen
  # Testkopie zuverlässig bedienbar. Das Backend akzeptiert die Bremse nur zusammen
  # mit einer gültigen markierten Sandbox.
  if [ "$start_via_launchservices" = true ]; then
    # Ein über SSH gestarteter Prozess hängt an keiner GUI-Sitzung: Er läuft, sein
    # Fenster erscheint aber auf keinem Bildschirm. LaunchServices startet die App
    # dagegen in der angemeldeten Sitzung des Benutzers. `-W` wartet auf ihr Ende,
    # damit der Testbereich wie sonst erst danach gemeldet wird.
    set -- -n -W "$bundle" --env "DOPPELDECKER_SANDBOX=$sandbox" \
      --env DOPPELDECKER_TEST_COPY_DELAY_MS=5
    if [ "${SSH_AUTH_SOCK+x}" = x ]; then
      set -- "$@" --env "SSH_AUTH_SOCK=$SSH_AUTH_SOCK"
    fi
    open "$@"
  else
    DOPPELDECKER_SANDBOX="$sandbox" DOPPELDECKER_TEST_COPY_DELAY_MS=5 \
      "$launch_program"
  fi
else
  if [ "$installed_bundle" = true ]; then
    installed=/usr/bin/doppeldecker
    if [ ! -x "$installed" ]; then
      printf 'Installiertes Doppeldecker-Programm wurde nicht gefunden.\n' >&2
      exit 1
    fi
    DOPPELDECKER_SANDBOX="$sandbox" DOPPELDECKER_TEST_COPY_DELAY_MS=5 "$installed"
  elif [ "$release_bundle" = true ]; then
    version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
    set -- "$project_dir/target/release/bundle/appimage/Doppeldecker_${version}_"*.AppImage
    if [ "$#" -ne 1 ] || [ ! -x "$1" ]; then
      printf 'Genau ein ausführbares Release-AppImage für Version %s wurde erwartet.\n' "$version" >&2
      exit 1
    fi
    DOPPELDECKER_SANDBOX="$sandbox" DOPPELDECKER_TEST_COPY_DELAY_MS=5 "$1"
  elif [ -n "$prebuilt_binary" ]; then
    DOPPELDECKER_SANDBOX="$sandbox" DOPPELDECKER_TEST_COPY_DELAY_MS=5 \
      "$prebuilt_binary"
  else
    DOPPELDECKER_SANDBOX="$sandbox" DOPPELDECKER_TEST_COPY_DELAY_MS=5 \
      cargo run -p doppeldecker
  fi
fi
