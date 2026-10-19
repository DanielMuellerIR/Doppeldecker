#!/bin/sh
# Startet einen kurzlebigen SMB-3.1.1-Server ausschließlich für den opt-in E2E-Test.
# Der Share liegt unter /tmp, erzwingt Signing und Verschlüsselung und verwendet nur
# ein festes wertloses Testpasswort. Es wird keine globale Samba-Datei verändert.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(dirname "$script_dir")
smbd_bin=${DOPPELDECKER_SMBD:-}
if [ -z "$smbd_bin" ]; then
  smbd_bin=$(command -v smbd || true)
fi
if [ -z "$smbd_bin" ] || [ ! -x "$smbd_bin" ]; then
  printf '%s\n' 'smbd fehlt; DOPPELDECKER_SMBD auf ein ausführbares Testbinary setzen.' >&2
  exit 1
fi

# Rustup legt Cargo oft nur in `$HOME/.cargo/env` ab und nicht in den PATH eines
# nicht-interaktiven Starts. Der Import steht deshalb vor der ersten Cargo-Nutzung:
# Stand er erst kurz vor dem Testlauf, scheiterte die Exaktnamen-Prüfung unten in
# einer sauberen Umgebung und meldete null gefundene Tests, obwohl Cargo nach dem
# Import verfügbar gewesen wäre (Review-Fund 2026-09-04).
if [ -f "${HOME:-}/.cargo/env" ]; then
  # shellcheck disable=SC1090
  . "$HOME/.cargo/env"
fi
if ! command -v cargo >/dev/null 2>&1; then
  printf '%s\n' 'cargo fehlt; Rust-Toolchain installieren oder PATH auf sie setzen.' >&2
  exit 1
fi

# `cargo test <filter>` endet auch dann mit 0, wenn der Filter null Tests trifft.
# Ohne diese Zählung würde ein umbenannter, aufgeteilter oder nicht mehr ignorierter
# Test als grüner SMB-E2E-Lauf gelten, obwohl gar nichts gegen Samba lief. Die Prüfung
# steht vor jeder Serverarbeit: Fehlt der Test, entsteht weder Wegwerfbaum noch smbd.
test_name=smb3_guid_paging_stream_quarantaene_reconnect_und_queue_bleiben_gebunden
exact_matches=$(cd "$project_dir" && cargo test -p doppeldecker-core --test smb_e2e -- --list \
  | awk -F ': ' -v name="$test_name" '
      $1 == name && $2 == "test" { count += 1 }
      END { print count + 0 }
    ')
if [ "$exact_matches" -ne 1 ]; then
  printf 'Erwarteter SMB-E2E-Test „%s“ wurde %s-mal exakt gefunden.\n' \
    "$test_name" "$exact_matches" >&2
  exit 1
fi

# Haltepunkt für den Selbsttest dieses Launchers: Er soll die Zählung prüfen,
# ohne dafür einen Wegwerfbaum anzulegen oder smbd zu starten. Ohne ihn lief der
# Test auf Linux bis in die Portwarteschleife und ließ /tmp/doppeldecker-smb-e2e.*
# samt Hashdatei liegen — auf macOS fiel das nicht auf, weil das Skript vorher an
# LibreSSL scheitert. Dasselbe Muster benutzt der SFTP-Launcher. Der Haltepunkt
# ist absichtlich kein erfolgreicher E2E-Lauf und beendet den Prozess daher rot.
if [ "${DOPPELDECKER_SMB_E2E_TEST_HOLD:-}" = "count-verified" ]; then
  printf 'Testzählung bestätigt (%s Treffer); Haltepunkt vor jeder Serverarbeit.\n' \
    "$exact_matches"
  exit 1
fi

test_root=$(mktemp -d /tmp/doppeldecker-smb-e2e.XXXXXX)
# Bis der vollständige Cleanup unten steht, darf ein Abbruch — fehlendes Werkzeug,
# scheiternde Schlüsselableitung, kaputte Konfiguration — den Wegwerfbaum nicht
# unbemerkt liegen lassen.
early_finish() {
  status=$?
  trap - EXIT INT TERM
  case "$test_root" in
    /tmp/doppeldecker-smb-e2e.*) rm -rf -- "$test_root" ;;
    *) printf 'Unsicherer SMB-E2E-Cleanuppfad wurde nicht entfernt: %s\n' "$test_root" >&2 ;;
  esac
  exit "$status"
}
trap early_finish EXIT INT TERM
private_dir="$test_root/private"
state_dir="$test_root/state"
lock_dir="$test_root/lock"
cache_dir="$test_root/cache"
pid_dir="$test_root/pid"
ncalrpc_dir="$test_root/ncalrpc"
share_root="$test_root/share"
mkdir -p "$private_dir" "$state_dir" "$lock_dir" "$cache_dir" "$pid_dir" "$ncalrpc_dir" "$share_root"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
config="$test_root/smb.conf"
log="$test_root/smbd.log"
user=$(id -un)
test_password='Doppeldecker-SMB-E2E-wertlos-2026'

cat > "$config" <<EOF
[global]
server role = standalone server
workgroup = WORKGROUP
security = user
map to guest = Never
passdb backend = smbpasswd:$private_dir/smbpasswd
private dir = $private_dir
state directory = $state_dir
lock directory = $lock_dir
cache directory = $cache_dir
pid directory = $pid_dir
ncalrpc dir = $ncalrpc_dir
log file = $test_root/log.%m
max log size = 1000
interfaces = 127.0.0.1
bind interfaces only = yes
smb ports = $port
server min protocol = SMB3_11
server max protocol = SMB3_11
server signing = mandatory
smb encrypt = required
load printers = no
disable spoolss = yes
printing = bsd
printcap name = /dev/null

[DDTEST]
path = $share_root
read only = no
browseable = no
valid users = $user
follow symlinks = no
wide links = no
EOF

# Samba verweigert `smbpasswd -L` ohne root auch bei einer privaten Datei. Deshalb
# erzeugt der Runner genau einen klassischen Testdatensatz selbst. Der NT-Hash stammt
# aus dem festen wertlosen Testwert; LM bleibt deaktiviert. Nichts davon gelangt ins
# Repository, Queue-Journal oder Terminal.
nt_hash=$(printf '%s' "$test_password" \
  | iconv -f UTF-8 -t UTF-16LE \
  | openssl dgst -provider legacy -md4 2>/dev/null \
  | sed 's/.*= //' \
  | tr '[:lower:]' '[:upper:]')
if [ "${#nt_hash}" -ne 32 ]; then
  printf '%s\n' 'NT-Hash für den wertlosen Samba-Testnutzer konnte nicht erzeugt werden.' >&2
  exit 1
fi
lct=$(printf '%08X' "$(date +%s)")
printf '%s:%s:XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX:%s:[U          ]:LCT-%s:\n' \
  "$user" "$(id -u)" "$nt_hash" "$lct" > "$private_dir/smbpasswd"
chmod 600 "$private_dir/smbpasswd"

server_pid_file="$test_root/server.pid"
manifest="$test_root/manifest.json"
owner_marker=$(basename "$test_root")
python3 - "$manifest" "$share_root" "$owner_marker" "$port" "$user" <<'PY'
import json
import sys

manifest, root, owner, port, user = sys.argv[1:]
document = {
    "provider": {
        "kind": "smb",
        "host": "127.0.0.1",
        "port": int(port),
        "share": "DDTEST",
        "user": user,
        "dialect": "3.1.1",
        "signing": "required",
        "encryption": "required",
    },
    "virtual_root": root,
    "ownership_marker": owner,
    "planned_entries": [
        "paging-*",
        "upload-target",
        "download-source",
        "bidirektional",
        "queue",
        ".doppeldecker-quarantine",
    ],
}
with open(manifest, "x", encoding="utf-8") as target:
    json.dump(document, target, sort_keys=True)
    target.write("\n")
PY

python3 - "$server_pid_file" "$log" "$smbd_bin" "$config" <<'PY'
import subprocess
import sys

pid_file, log_file, smbd, config = sys.argv[1:]
with open(log_file, "ab", buffering=0) as output:
    process = subprocess.Popen(
        [smbd, "-F", "--no-process-group", "-s", config, "-d", "1"],
        stdin=subprocess.DEVNULL,
        stdout=output,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
with open(pid_file, "w", encoding="ascii") as target:
    target.write(f"{process.pid}\n")
PY
smbd_pid=$(cat "$server_pid_file")
finish() {
  status=$?
  trap - EXIT INT TERM
  # `smbd` läuft in einer eigenen Session; seine geforkten Verbindungskinder
  # überleben das `kill` des Leiters und arbeiteten sonst weiter, während `rm -rf`
  # den Share unter ihnen entfernt. Deshalb die ganze Prozessgruppe beenden und
  # kurz auf ihr Ende warten — `wait` greift nicht, der Prozess ist kein Kind
  # dieser Shell.
  kill -TERM -- "-$smbd_pid" 2>/dev/null || kill "$smbd_pid" 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$smbd_pid" 2>/dev/null || break
    sleep 0.2
  done
  if [ "$status" -eq 0 ]; then
    case "$test_root" in
      /tmp/doppeldecker-smb-e2e.*)
        if ! rm -rf -- "$test_root" || [ -e "$test_root" ]; then
          printf 'SMB-E2E-Cleanup scheiterte; Recoverypfad: %s\n' "$test_root" >&2
          status=1
        fi
        ;;
      *)
        printf 'Unsicherer SMB-E2E-Cleanuppfad wurde nicht entfernt: %s\n' "$test_root" >&2
        status=1
        ;;
    esac
  else
    printf '\nSMB-E2E-Testbereich bleibt zur Fehleranalyse erhalten:\n%s\n' "$test_root" >&2
  fi
  exit "$status"
}
trap finish EXIT INT TERM

python3 - "$port" <<'PY'
import socket
import sys
import time

port = int(sys.argv[1])
for _ in range(100):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.2):
            break
    except OSError:
        time.sleep(0.05)
else:
    raise SystemExit("SMB-Testserver startete nicht")
PY

cd "$project_dir"
DOPPELDECKER_SMB_TEST_HOST=127.0.0.1 \
DOPPELDECKER_SMB_TEST_PORT="$port" \
DOPPELDECKER_SMB_TEST_SHARE=DDTEST \
DOPPELDECKER_SMB_TEST_USER="$user" \
DOPPELDECKER_SMB_TEST_ROOT="$share_root" \
  cargo test -p doppeldecker-core --test smb_e2e "$test_name" -- --exact --ignored --nocapture
