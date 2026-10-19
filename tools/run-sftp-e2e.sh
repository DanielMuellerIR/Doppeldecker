#!/bin/sh
# Startet einen kurzlebigen OpenSSH-SFTP-Server ausschließlich für den opt-in E2E.
# Hostkey, Clientkey, Agent und Serverroot entstehen unter einem eigenen /tmp-Root.
# Persönliche SSH-Schlüssel, globale sshd-Dateien und bestehende Daten bleiben außen.
set -eu

# Die Signalgrenze steht vor der ersten Ressource. Der Handler merkt ein Signal nur
# vor; er räumt nicht mitten in der Rootanlage, Agent- oder `setsid`-Registrierung
# auf. Direkt nach jeder atomaren Besitzmarke übernimmt derselbe EXIT-Cleanup.
pending_signal=0
cleanup_errors=""
stop_log_records=""
reap_log_records=""
test_root=""
test_root_owned=0
test_root_device=""
test_root_inode=""
test_root_uid=""
test_root_mode=""
test_root_mount_id=""
test_root_handle=""
server_root_device=""
server_root_inode=""
server_root_uid=""
server_root_mode=""
server_root_mount_id=""
server_root_handle=""
own_ssh_agent=0
ssh_agent_pid=""
ssh_agent_group_ready=0
ssh_agent_ready_file=""
ssh_agent_ack_file=""
ssh_agent_status_file=""
sshd_pid=""
sshd_group_ready=0
sshd_ready_file=""
sshd_ack_file=""
sshd_status_file=""
cargo_pid=""
cargo_group_ready=0
cargo_ready_file=""
cargo_ack_file=""
cargo_status_file=""
inherited_ssh_agent_pid=${SSH_AGENT_PID-}
inherited_ssh_auth_sock=${SSH_AUTH_SOCK-}

add_cleanup_error() {
  cleanup_errors="${cleanup_errors}${cleanup_errors:+; }$1"
}

record_signal() {
  if [ "$pending_signal" -eq 0 ]; then
    pending_signal=$1
  fi
}

exit_if_signaled() {
  if [ "$pending_signal" -ne 0 ]; then
    exit "$pending_signal"
  fi
}

ready_file_matches_pid() {
  managed_pid=$1
  ready_file=$2
  [ -n "$ready_file" ] || return 1
  [ -r "$ready_file" ] || return 1
  IFS= read -r ready_pid < "$ready_file" || return 1
  [ "$ready_pid" = "$managed_pid" ]
}

flush_diagnostic_log() {
  diagnostic_path=$1
  diagnostic_records=$2
  diagnostic_label=$3
  [ -n "$diagnostic_path" ] || return 0

  # Diagnose kommt erst nach dem Sicherheitscleanup. Der Helfer folgt keinen
  # Symlinks, öffnet nichtblockierend und schreibt höchstens 64 KiB ausschließlich
  # in eine reguläre Datei. FIFO, Device und Verzeichnis werden ohne Warten
  # abgelehnt; ihr Fehler wird anschließend nur zur Cleanupbilanz hinzugefügt.
  if ! python3 - "$diagnostic_path" "$diagnostic_records" "$diagnostic_label" <<'PY'
import os
from pathlib import Path
import stat
import sys
import time

path, records, label = sys.argv[1:]
payload = records.encode("utf-8")
if len(payload) > 64 * 1024:
    print(f"{label} überschreitet die Diagnosegrenze", file=sys.stderr)
    raise SystemExit(1)

flags = os.O_WRONLY | os.O_APPEND | os.O_NONBLOCK
flags |= getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
try:
    before = os.lstat(path)
except FileNotFoundError:
    before = None

if before is not None and not stat.S_ISREG(before.st_mode):
    print(f"{label}-Ziel ist keine reguläre Datei", file=sys.stderr)
    raise SystemExit(1)

marker = os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_DIAGNOSTIC_MARKER")
acknowledgement = os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_DIAGNOSTIC_ACK")
if marker and acknowledgement:
    marker_path = Path(marker)
    temporary = marker_path.with_name(f"{marker_path.name}.{os.getpid()}.tmp")
    temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
    os.replace(temporary, marker_path)
    while not Path(acknowledgement).exists():
        time.sleep(0.01)

if before is None:
    flags |= os.O_CREAT | os.O_EXCL

try:
    descriptor = os.open(path, flags, 0o600)
except OSError as error:
    print(f"{label}-Ziel konnte nicht sicher geöffnet werden: {error}", file=sys.stderr)
    raise SystemExit(1)

try:
    opened = os.fstat(descriptor)
    if not stat.S_ISREG(opened.st_mode):
        print(f"{label}-Ziel wechselte auf einen nicht regulären Typ", file=sys.stderr)
        raise SystemExit(1)
    if before is not None and (opened.st_dev, opened.st_ino) != (
        before.st_dev,
        before.st_ino,
    ):
        print(f"{label}-Ziel wechselte vor dem Schreiben seine Identität", file=sys.stderr)
        raise SystemExit(1)
    remaining = memoryview(payload)
    while remaining:
        written = os.write(descriptor, remaining)
        if written <= 0:
            print(f"{label} konnte nicht vollständig geschrieben werden", file=sys.stderr)
            raise SystemExit(1)
        remaining = remaining[written:]
finally:
    os.close(descriptor)
PY
  then
    add_cleanup_error "$diagnostic_label konnte nicht sicher geschrieben werden"
  fi
}

stop_managed_process() {
  managed_pid=$1
  group_ready=$2
  [ -n "$managed_pid" ] || return 0
  target=$managed_pid
  if [ "$group_ready" -eq 1 ]; then
    target=-$managed_pid
  fi

  # Diagnosezeilen bleiben bis nach TERM, KILL, Wait und Rootentfernung ausschließlich
  # im Speicher. Selbst ein bösartiges FIFO-Ziel kann den Sicherheitscleanup so nicht
  # vor dem ersten Signal blockieren.
  if [ -n "${DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG:-}" ]; then
    stop_log_records="${stop_log_records}${target}
"
  fi

  kill -TERM "$target" 2>/dev/null || true
  attempts=0
  while kill -0 "$target" 2>/dev/null && [ "$attempts" -lt 50 ]; do
    sleep 0.02
    attempts=$((attempts + 1))
  done
  kill -KILL "$target" 2>/dev/null || true
  # Nur der Gruppenleiter ist ein direktes Kind dieser Shell und kann hier geerntet
  # werden. Sein Gruppen-Signal beendet zusätzlich alle Sitzungs-Kinder.
  wait "$managed_pid" 2>/dev/null || true
}

remove_bound_test_root() {
  bound_root=$1
  bound_device=$2
  bound_inode=$3
  bound_uid=$4
  bound_mode=$5
  bound_mount_id=$6
  python3 - "$bound_root" "$bound_device" "$bound_inode" "$bound_uid" "$bound_mode" \
    "$bound_mount_id" <<'PY'
import ctypes
import errno
import os
from pathlib import Path
import secrets
import stat
import sys
import time

raw, expected_device, expected_inode, expected_uid, expected_mode, expected_mount_id = sys.argv[1:]

def mount_identity(descriptor):
    """Liefert die Mount-Identität des bereits geöffneten Verzeichnisses."""
    if sys.platform.startswith("linux"):
        with open(f"/proc/self/fdinfo/{descriptor}", encoding="ascii") as source:
            for line in source:
                if line.startswith("mnt_id:"):
                    return f"linux:{line.split(':', 1)[1].strip()}"
        raise SystemExit("Linux lieferte keine Mount-ID für den Rootdeskriptor")
    if sys.platform == "darwin":
        class Fsid(ctypes.Structure):
            _fields_ = [("value", ctypes.c_int32 * 2)]

        class StatFs(ctypes.Structure):
            _fields_ = [
                ("block_size", ctypes.c_uint32),
                ("io_size", ctypes.c_int32),
                ("blocks", ctypes.c_uint64),
                ("blocks_free", ctypes.c_uint64),
                ("blocks_available", ctypes.c_uint64),
                ("files", ctypes.c_uint64),
                ("files_free", ctypes.c_uint64),
                ("fsid", Fsid),
                ("owner", ctypes.c_uint32),
                ("fs_type", ctypes.c_uint32),
                ("flags", ctypes.c_uint32),
                ("subtype", ctypes.c_uint32),
                ("type_name", ctypes.c_char * 16),
                ("mount_point", ctypes.c_char * 1024),
                ("mounted_from", ctypes.c_char * 1024),
                ("reserved", ctypes.c_uint32 * 8),
            ]

        information = StatFs()
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.fstatfs(descriptor, ctypes.byref(information)) != 0:
            raise SystemExit(f"fstatfs scheiterte: {os.strerror(ctypes.get_errno())}")
        mount_point = bytes(information.mount_point).split(b"\0", 1)[0].decode(
            "utf-8", "strict"
        )
        return f"darwin:{information.fsid.value[0]}:{information.fsid.value[1]}:{mount_point}"
    raise SystemExit("Diese Plattform bietet keine sichere Mount-ID-Prüfung")

def rename_noreplace(old_name, new_name, parent_descriptor):
    """Verschiebt den gebundenen Root nur auf einen noch freien Claimnamen."""
    libc = ctypes.CDLL(None, use_errno=True)
    old_bytes = os.fsencode(old_name)
    new_bytes = os.fsencode(new_name)
    if sys.platform.startswith("linux"):
        renameat2 = getattr(libc, "renameat2", None)
        if renameat2 is None:
            raise OSError(errno.ENOTSUP, "renameat2 ist nicht verfügbar")
        renameat2.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        renameat2.restype = ctypes.c_int
        result = renameat2(
            parent_descriptor,
            old_bytes,
            parent_descriptor,
            new_bytes,
            1,  # RENAME_NOREPLACE
        )
    elif sys.platform == "darwin":
        renameatx_np = getattr(libc, "renameatx_np", None)
        if renameatx_np is None:
            raise OSError(errno.ENOTSUP, "renameatx_np ist nicht verfügbar")
        renameatx_np.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        renameatx_np.restype = ctypes.c_int
        result = renameatx_np(
            parent_descriptor,
            old_bytes,
            parent_descriptor,
            new_bytes,
            0x00000004,  # RENAME_EXCL
        )
    else:
        raise OSError(errno.ENOTSUP, "Kein exklusives Rename auf dieser Plattform")
    if result == 0:
        return
    number = ctypes.get_errno()
    if number == errno.EEXIST:
        raise FileExistsError(number, os.strerror(number), new_name)
    raise OSError(number, os.strerror(number), old_name)

prefix = "doppeldecker-sftp-e2e."
if not raw or "\n" in raw or "\r" in raw or "\0" in raw:
    raise SystemExit("SFTP-E2E-Root ist leer oder mehrzeilig")
path = Path(raw)
if not path.is_absolute() or os.path.normpath(raw) != raw:
    raise SystemExit("SFTP-E2E-Root ist nicht absolut und normalisiert")
allowed_parents = {Path("/tmp").resolve(), Path("/private/tmp").resolve()}
parent = path.parent.resolve()
if not path.name.startswith(prefix):
    raise SystemExit("SFTP-E2E-Root besitzt nicht den festen Namen")
suffix = path.name[len(prefix):]
if parent not in allowed_parents or not suffix:
    raise SystemExit("SFTP-E2E-Root liegt nicht als fester direkter /tmp-Unterordner vor")

# Der Root selbst bleibt ab jetzt mit O_NOFOLLOW geöffnet. Alle Inhalte werden nur
# relativ zu diesem Deskriptor geleert. Ein später ausgetauschter Rootname kann damit
# weder als neuer Baum betreten noch rekursiv gelöscht werden.
directory_flags = (
    os.O_RDONLY
    | getattr(os, "O_DIRECTORY", 0)
    | getattr(os, "O_CLOEXEC", 0)
    | getattr(os, "O_NOFOLLOW", 0)
)
parent_fd = os.open(parent, directory_flags)
root_fd = None
claim_fd = None
claim_name = None
try:
    root_fd = os.open(path.name, directory_flags, dir_fd=parent_fd)
    opened = os.fstat(root_fd)
    if not stat.S_ISDIR(opened.st_mode):
        raise SystemExit("SFTP-E2E-Root ist kein echtes Verzeichnis mehr")
    if (opened.st_dev, opened.st_ino) != (int(expected_device), int(expected_inode)):
        raise SystemExit("SFTP-E2E-Root wechselte vor dem Cleanup seine Identität")
    if opened.st_uid != int(expected_uid) or stat.S_IMODE(opened.st_mode) != int(expected_mode):
        raise SystemExit("SFTP-E2E-Root wechselte Besitzer oder Modus")
    root_mount_id = mount_identity(root_fd)
    if root_mount_id != expected_mount_id:
        raise SystemExit("SFTP-E2E-Root wechselte vor dem Cleanup seine Mount-ID")

    # Der öffentliche Originalname ist nach dem Schließen des Rootdeskriptors keine
    # sichere Löschbindung. Solange der Root noch offen ist, ziehen wir ihn deshalb
    # atomar auf einen zufälligen, exklusiven Namen im bereits gehaltenen Elternordner.
    for _ in range(128):
        proposed = f"{prefix}cleanup-{secrets.token_hex(12)}"
        try:
            rename_noreplace(path.name, proposed, parent_fd)
        except FileExistsError:
            continue
        claim_name = proposed
        break
    if claim_name is None:
        raise SystemExit("Kein exklusiver SFTP-E2E-Cleanup-Claimname verfügbar")

    claim_fd = os.open(claim_name, directory_flags, dir_fd=parent_fd)
    claim = os.fstat(claim_fd)
    claim_at_name = os.stat(claim_name, dir_fd=parent_fd, follow_symlinks=False)
    for metadata in (claim, claim_at_name):
        if (
            (metadata.st_dev, metadata.st_ino) != (int(expected_device), int(expected_inode))
            or metadata.st_uid != int(expected_uid)
            or stat.S_IMODE(metadata.st_mode) != int(expected_mode)
            or not stat.S_ISDIR(metadata.st_mode)
        ):
            raise SystemExit("SFTP-E2E-Cleanup-Claim besitzt nicht die gebundene Rootidentität")
    if mount_identity(claim_fd) != expected_mount_id:
        raise SystemExit("SFTP-E2E-Cleanup-Claim wechselte die gebundene Mount-ID")

    marker = os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_MARKER")
    acknowledgement = os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_ACK")
    if marker and acknowledgement:
        marker_path = Path(marker)
        temporary = marker_path.with_name(f"{marker_path.name}.{os.getpid()}.tmp")
        temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
        os.replace(temporary, marker_path)
        while not Path(acknowledgement).exists():
            time.sleep(0.01)

    def empty_directory(directory_fd):
        for name in os.listdir(directory_fd):
            before = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if before.st_dev != opened.st_dev:
                raise SystemExit(
                    f"SFTP-E2E-Cleanup betritt keine Mountgrenze: {name}"
                )
            if stat.S_ISDIR(before.st_mode):
                child_fd = os.open(name, directory_flags, dir_fd=directory_fd)
                try:
                    child = os.fstat(child_fd)
                    if child.st_dev != opened.st_dev:
                        raise SystemExit(
                            f"SFTP-E2E-Unterordner liegt auf einem anderen Device: {name}"
                        )
                    if (child.st_dev, child.st_ino) != (before.st_dev, before.st_ino):
                        raise SystemExit(f"SFTP-E2E-Unterordner wechselte seine Identität: {name}")
                    child_mount_id = mount_identity(child_fd)
                    if name == os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_MOUNT_ID_NAME"):
                        child_mount_id = f"simuliert:{child_mount_id}"
                    if child_mount_id != root_mount_id:
                        raise SystemExit(
                            f"SFTP-E2E-Cleanup betritt keine fremde Mount-ID: {name}"
                        )
                    empty_directory(child_fd)
                finally:
                    os.close(child_fd)
                current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
                if (current.st_dev, current.st_ino) != (before.st_dev, before.st_ino):
                    raise SystemExit(f"SFTP-E2E-Unterordner wurde ausgetauscht: {name}")
                os.rmdir(name, dir_fd=directory_fd)
            else:
                current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
                if (current.st_dev, current.st_ino) != (before.st_dev, before.st_ino):
                    raise SystemExit(f"SFTP-E2E-Eintrag wurde ausgetauscht: {name}")
                os.unlink(name, dir_fd=directory_fd)

    empty_directory(root_fd)

    marker = os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_CLOSE_MARKER")
    acknowledgement = os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_CLOSE_ACK")
    if marker and acknowledgement:
        marker_path = Path(marker)
        temporary = marker_path.with_name(f"{marker_path.name}.{os.getpid()}.tmp")
        temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
        os.replace(temporary, marker_path)
        while not Path(acknowledgement).exists():
            time.sleep(0.01)

    final_claim = os.stat(claim_name, dir_fd=parent_fd, follow_symlinks=False)
    if (
        (final_claim.st_dev, final_claim.st_ino)
        != (int(expected_device), int(expected_inode))
        or final_claim.st_uid != int(expected_uid)
        or stat.S_IMODE(final_claim.st_mode) != int(expected_mode)
        or not stat.S_ISDIR(final_claim.st_mode)
        or mount_identity(claim_fd) != expected_mount_id
    ):
        raise SystemExit("SFTP-E2E-Cleanup-Claim wechselte vor dem abschließenden rmdir")
    # Nur der zufällige, weiterhin offen gehaltene Claim ist ein Löschziel. Ein am
    # ursprünglichen Namen auftauchender fremder Ordner bleibt vollständig unberührt.
    os.rmdir(claim_name, dir_fd=parent_fd)
except BaseException:
    recovery_path = parent / claim_name if claim_name is not None else path
    print(
        f"SFTP-E2E-Cleanup-Claim-Recoverypfad: {recovery_path}",
        file=sys.stderr,
    )
    raise
finally:
    if claim_fd is not None:
        os.close(claim_fd)
    if root_fd is not None:
        os.close(root_fd)
    os.close(parent_fd)
PY
}

finish() {
  status=$?
  # Ab jetzt besitzt ausschließlich dieser eine Cleanup-Pfad den Ablauf. Weitere
  # HUP/INT/TERM dürfen ihn weder abbrechen noch den zuerst gemerkten Status ändern,
  # während TERM/KILL/Wait und das Entfernen des Testroots noch laufen.
  trap '' HUP INT TERM
  trap - EXIT
  if [ "$pending_signal" -ne 0 ]; then
    status=$pending_signal
  fi

  # Das Bereitschaftsdokument wird vom Kind erst nach erfolgreichem `setsid`
  # atomar veröffentlicht. So schließt auch ein Signal genau zwischen Datei und
  # Shell-Zuweisung die richtige eigene Gruppe ein, nie eine bloß vermutete PGID.
  if [ -n "$cargo_pid" ] && ready_file_matches_pid "$cargo_pid" "$cargo_ready_file"; then
    cargo_group_ready=1
  fi
  stop_managed_process "$cargo_pid" "$cargo_group_ready"

  if [ -n "$sshd_pid" ] && ready_file_matches_pid "$sshd_pid" "$sshd_ready_file"; then
    sshd_group_ready=1
  fi
  stop_managed_process "$sshd_pid" "$sshd_group_ready"

  if [ "$own_ssh_agent" -eq 1 ]; then
    if ready_file_matches_pid "$ssh_agent_pid" "$ssh_agent_ready_file"; then
      ssh_agent_group_ready=1
    fi
    stop_managed_process "$ssh_agent_pid" "$ssh_agent_group_ready"
  fi

  # Alle Artefaktschreiber liefen relativ zum descriptorverifizierten cwd. Vor
  # dem abschließenden Namensclaim verlassen wir ihn; FD 7/8 bleiben weiterhin
  # offen, bis der Cleanup entweder exakt diesen Root entfernt oder fail-closed
  # einen Recoverypfad meldet.
  if ! cd /; then
    add_cleanup_error "descriptorgebundener Arbeitsordner konnte nicht verlassen werden"
  fi
  if [ "$test_root_owned" -eq 1 ]; then
    if ! remove_bound_test_root "$test_root" "$test_root_device" "$test_root_inode" \
      "$test_root_uid" "$test_root_mode" "$test_root_mount_id"; then
      printf 'SFTP-E2E-Cleanup scheiterte; Recoverypfad: %s\n' "$test_root" >&2
      if [ "$status" -eq 0 ]; then
        status=1
      fi
    fi
  fi
  # Die beiden Deskriptoren bleiben bis hinter Prozess- und Rootcleanup offen.
  # Damit kann kein späterer Namensaustausch die von diesem Lauf verwendeten
  # Artefaktpfade auf einen fremden Baum umlenken.
  exec 8<&- 2>/dev/null || true
  exec 7<&- 2>/dev/null || true
  flush_diagnostic_log "${DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG:-}" \
    "$stop_log_records" "Stop-Logger"
  flush_diagnostic_log "${DOPPELDECKER_SFTP_E2E_TEST_REAP_LOG:-}" \
    "$reap_log_records" "Reap-Logger"
  if [ -n "$cleanup_errors" ]; then
    printf 'SFTP-E2E-Cleanup meldete zusätzliche Fehler: %s\n' "$cleanup_errors" >&2
    if [ "$status" -eq 0 ]; then
      status=1
    fi
  fi
  exit "$status"
}

trap finish EXIT
trap 'record_signal 129' HUP
trap 'record_signal 130' INT
trap 'record_signal 143' TERM

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(dirname "$script_dir")
sshd_bin=${DOPPELDECKER_SSHD:-}
if [ -z "$sshd_bin" ]; then
  sshd_bin=$(command -v sshd || true)
fi
if [ -z "$sshd_bin" ] || [ ! -x "$sshd_bin" ]; then
  printf '%s\n' 'sshd fehlt; DOPPELDECKER_SSHD auf ein ausführbares Testbinary setzen.' >&2
  exit 1
fi
for tool in ssh-agent ssh-add ssh-keygen python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf '%s\n' "$tool fehlt; SFTP-E2E kann nicht sicher gestartet werden." >&2
    exit 1
  fi
done

disable_fsync=${DOPPELDECKER_SFTP_DISABLE_FSYNC:-0}
case "$disable_fsync" in
  0)
    sftp_subsystem='internal-sftp'
    test_name='echter_sftp_root_bindet_identitaet_probe_reconnect_und_streams'
    ;;
  1)
    # OpenSSH kündigt die Erweiterung weiterhin an, verweigert aber den konkreten
    # Request. So belegt derselbe echte Server, dass nur das Verhalten zählt — ohne
    # globale sshd-Konfiguration oder Nutzerdaten anzufassen.
    sftp_subsystem='internal-sftp -P fsync'
    test_name='sftp_ohne_fsync_erlaubt_sichtbaren_best_effort_upload'
    ;;
  *)
    printf '%s\n' 'DOPPELDECKER_SFTP_DISABLE_FSYNC muss 0 oder 1 sein.' >&2
    exit 2
    ;;
esac

# Erst nach allen rein syntaktischen Prüfungen entsteht der künstliche Root. Der
# Signal-Handler ist dennoch bereits aktiv und verlässt das Skript erst, nachdem die
# Zuweisung und die enge Pfadprüfung den Root eindeutig als unseren Besitz markieren.
exit_if_signaled
test_root_binding=$(
  # Ein Signal an die gesamte Vordergrundgruppe darf den Root-Helfer nicht nach
  # dem Anlegen, aber vor der Ausgabe seiner Besitzdaten töten. Nur der Elternprozess
  # merkt das Signal; das Kind liefert die Bindung noch vollständig.
  trap '' HUP INT TERM
  python3 - <<'PY'
import ctypes
import errno
import os
from pathlib import Path
import secrets
import stat
import sys
import time

def mount_identity(descriptor):
    if sys.platform.startswith("linux"):
        with open(f"/proc/self/fdinfo/{descriptor}", encoding="ascii") as source:
            for line in source:
                if line.startswith("mnt_id:"):
                    return f"linux:{line.split(':', 1)[1].strip()}"
        raise SystemExit("Linux lieferte keine Mount-ID für den Rootdeskriptor")
    if sys.platform == "darwin":
        class Fsid(ctypes.Structure):
            _fields_ = [("value", ctypes.c_int32 * 2)]

        class StatFs(ctypes.Structure):
            _fields_ = [
                ("block_size", ctypes.c_uint32), ("io_size", ctypes.c_int32),
                ("blocks", ctypes.c_uint64), ("blocks_free", ctypes.c_uint64),
                ("blocks_available", ctypes.c_uint64), ("files", ctypes.c_uint64),
                ("files_free", ctypes.c_uint64), ("fsid", Fsid),
                ("owner", ctypes.c_uint32), ("fs_type", ctypes.c_uint32),
                ("flags", ctypes.c_uint32), ("subtype", ctypes.c_uint32),
                ("type_name", ctypes.c_char * 16),
                ("mount_point", ctypes.c_char * 1024),
                ("mounted_from", ctypes.c_char * 1024),
                ("reserved", ctypes.c_uint32 * 8),
            ]
        information = StatFs()
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.fstatfs(descriptor, ctypes.byref(information)) != 0:
            raise SystemExit(f"fstatfs scheiterte: {os.strerror(ctypes.get_errno())}")
        mount_point = bytes(information.mount_point).split(b"\0", 1)[0].decode("utf-8", "strict")
        return f"darwin:{information.fsid.value[0]}:{information.fsid.value[1]}:{mount_point}"
    raise SystemExit("Diese Plattform bietet keine sichere Mount-ID-Prüfung")

def rename_noreplace(old_name, new_name, parent_descriptor):
    """Verschiebt einen Namen nur, wenn der zufällige Claim noch wirklich frei ist."""
    libc = ctypes.CDLL(None, use_errno=True)
    old_bytes = os.fsencode(old_name)
    new_bytes = os.fsencode(new_name)
    if sys.platform.startswith("linux"):
        renameat2 = getattr(libc, "renameat2", None)
        if renameat2 is None:
            raise OSError(errno.ENOTSUP, "renameat2 ist nicht verfügbar")
        renameat2.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        renameat2.restype = ctypes.c_int
        result = renameat2(
            parent_descriptor,
            old_bytes,
            parent_descriptor,
            new_bytes,
            1,  # RENAME_NOREPLACE
        )
    elif sys.platform == "darwin":
        renameatx_np = getattr(libc, "renameatx_np", None)
        if renameatx_np is None:
            raise OSError(errno.ENOTSUP, "renameatx_np ist nicht verfügbar")
        renameatx_np.argtypes = [
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_char_p,
            ctypes.c_uint,
        ]
        renameatx_np.restype = ctypes.c_int
        result = renameatx_np(
            parent_descriptor,
            old_bytes,
            parent_descriptor,
            new_bytes,
            0x00000004,  # RENAME_EXCL
        )
    else:
        raise OSError(errno.ENOTSUP, "Kein exklusives Rename auf dieser Plattform")
    if result == 0:
        return
    number = ctypes.get_errno()
    if number == errno.EEXIST:
        raise FileExistsError(number, os.strerror(number), new_name)
    raise OSError(number, os.strerror(number), old_name)

prefix = "doppeldecker-sftp-e2e."
parent_text = "/tmp"
parent = Path(parent_text)
allowed_parents = {Path("/tmp").resolve(), Path("/private/tmp").resolve()}
canonical_parent = parent.resolve()
if canonical_parent not in allowed_parents:
    raise SystemExit("Temporärer Elternordner ist nicht /tmp oder /private/tmp")
directory_flags = (
    os.O_RDONLY
    | getattr(os, "O_DIRECTORY", 0)
    | getattr(os, "O_CLOEXEC", 0)
    | getattr(os, "O_NOFOLLOW", 0)
)
parent_fd = os.open(canonical_parent, directory_flags)
for _ in range(128):
    candidate_name = f"{prefix}{secrets.token_hex(12)}"
    try:
        os.mkdir(candidate_name, 0o700, dir_fd=parent_fd)
    except FileExistsError:
        continue
    candidate = parent / candidate_name
    break
else:
    raise SystemExit("Kein exklusiver SFTP-E2E-Rootname verfügbar")

root_fd = None
owned_identity = None
owned_uid = None
owned_mode = None
owned_mount_id = None
handed_off = False
try:
    before = os.stat(candidate.name, dir_fd=parent_fd, follow_symlinks=False)
    root_fd = os.open(candidate.name, directory_flags, dir_fd=parent_fd)
    metadata = os.fstat(root_fd)
    owned_identity = (metadata.st_dev, metadata.st_ino)
    owned_uid = metadata.st_uid
    owned_mode = stat.S_IMODE(metadata.st_mode)
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(before.st_mode):
        raise SystemExit("Root-Helfer erzeugte kein echtes Verzeichnis")
    if owned_identity != (before.st_dev, before.st_ino):
        raise SystemExit("Root-Helfer sah unmittelbar nach der Anlage einen Austausch")
    if metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
        raise SystemExit("Root-Helfer band Besitzer oder Modus nicht eindeutig")
    if os.listdir(root_fd):
        raise SystemExit("Exklusiv erzeugter SFTP-E2E-Root ist nicht leer")
    canonical = candidate.resolve(strict=True)
    if canonical.parent not in allowed_parents:
        raise SystemExit("SFTP-E2E-Root verließ bei der Normalisierung /tmp")
    canonical_metadata = canonical.lstat()
    if (canonical_metadata.st_dev, canonical_metadata.st_ino) != owned_identity:
        raise SystemExit("SFTP-E2E-Root wechselte bei der Normalisierung seine Identität")

    # Die Testveröffentlichung geschieht absichtlich vor der Fehlerinjektion. Der
    # Unterprozesstest kann dadurch den exakten Namen prüfen, ohne je einen breiten
    # /tmp-Such- oder Löschpfad zu bilden.
    state = os.environ.get("FAKE_SFTP_STATE")
    if state:
        root_file = Path(state) / "root.txt"
        temporary = root_file.with_name(f"{root_file.name}.{os.getpid()}.tmp")
        temporary.write_text(str(candidate), encoding="utf-8")
        os.replace(temporary, root_file)
    root_mount_id = mount_identity(root_fd)
    owned_mount_id = root_mount_id
    if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_FAIL") == "mount-id":
        raise RuntimeError("simulierter Fehler nach gebundener Mount-ID")

    if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == "mktemp-assignment":
        marker = Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
        temporary = marker.with_name(f"{marker.name}.{os.getpid()}.tmp")
        temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
        os.replace(temporary, marker)
        time.sleep(0.3)

    if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_FAIL") == "late":
        raise RuntimeError("simulierter später Root-Helferfehler vor Übergabe")

    print(
        f"{candidate}\t{metadata.st_dev}\t{metadata.st_ino}"
        f"\t{metadata.st_uid}\t{stat.S_IMODE(metadata.st_mode)}\t{root_mount_id}",
        flush=True,
    )
    handed_off = True
finally:
    if not handed_off and owned_identity is not None:
        claim_name = None
        try:
            current = os.stat(candidate.name, dir_fd=parent_fd, follow_symlinks=False)
            if (
                (current.st_dev, current.st_ino) != owned_identity
                or current.st_uid != owned_uid
                or stat.S_IMODE(current.st_mode) != owned_mode
                or not stat.S_ISDIR(current.st_mode)
            ):
                raise RuntimeError(
                    "Originalname besitzt vor dem Claim nicht mehr die gebundene Identität"
                )

            # Der ursprüngliche, öffentlich bekannte Name wird nie nach dem Schließen
            # eines Deskriptors gelöscht. Zuerst ziehen wir unseren Root atomar auf
            # einen neuen exklusiven Claim im bereits gehaltenen /tmp-Elternordner.
            for _ in range(128):
                proposed = f"{prefix}cleanup-{secrets.token_hex(12)}"
                try:
                    rename_noreplace(candidate.name, proposed, parent_fd)
                except FileExistsError:
                    continue
                claim_name = proposed
                break
            if claim_name is None:
                raise RuntimeError("Kein exklusiver Cleanup-Claimname verfügbar")

            cleanup_fd = os.open(claim_name, directory_flags, dir_fd=parent_fd)
            try:
                cleanup = os.fstat(cleanup_fd)
                claim_at_name = os.stat(
                    claim_name, dir_fd=parent_fd, follow_symlinks=False
                )
                if (
                    (cleanup.st_dev, cleanup.st_ino) != owned_identity
                    or (claim_at_name.st_dev, claim_at_name.st_ino) != owned_identity
                    or cleanup.st_uid != owned_uid
                    or claim_at_name.st_uid != owned_uid
                    or stat.S_IMODE(cleanup.st_mode) != owned_mode
                    or stat.S_IMODE(claim_at_name.st_mode) != owned_mode
                    or not stat.S_ISDIR(cleanup.st_mode)
                    or not stat.S_ISDIR(claim_at_name.st_mode)
                ):
                    raise RuntimeError("Cleanup-Claim besitzt nicht die gebundene Rootidentität")
                if mount_identity(cleanup_fd) != owned_mount_id:
                    raise RuntimeError("Cleanup-Claim wechselte die gebundene Mount-ID")
                if os.listdir(cleanup_fd):
                    raise RuntimeError("Cleanup-Claim ist vor internem Fehlercleanup nicht leer")

                marker = os.environ.get(
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_CLOSE_MARKER"
                )
                acknowledgement = os.environ.get(
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_CLOSE_ACK"
                )
                if marker and acknowledgement:
                    marker_path = Path(marker)
                    temporary = marker_path.with_name(
                        f"{marker_path.name}.{os.getpid()}.tmp"
                    )
                    temporary.write_text(
                        str(parent / claim_name), encoding="utf-8"
                    )
                    os.replace(temporary, marker_path)
                    deadline = time.monotonic() + 10
                    while not Path(acknowledgement).exists():
                        if time.monotonic() >= deadline:
                            raise RuntimeError(
                                "Root-Helfer-Testfreigabe kam nicht rechtzeitig"
                            )
                        time.sleep(0.01)

                final_claim = os.stat(
                    claim_name, dir_fd=parent_fd, follow_symlinks=False
                )
                final_opened = os.fstat(cleanup_fd)
                if (
                    (final_claim.st_dev, final_claim.st_ino) != owned_identity
                    or (final_opened.st_dev, final_opened.st_ino) != owned_identity
                    or final_claim.st_uid != owned_uid
                    or final_opened.st_uid != owned_uid
                    or stat.S_IMODE(final_claim.st_mode) != owned_mode
                    or stat.S_IMODE(final_opened.st_mode) != owned_mode
                    or not stat.S_ISDIR(final_claim.st_mode)
                    or not stat.S_ISDIR(final_opened.st_mode)
                    or mount_identity(cleanup_fd) != owned_mount_id
                ):
                    raise RuntimeError(
                        "Cleanup-Claim wechselte vor dem rmdir seine vollständige Bindung"
                    )
                # Der geprüfte Claim bleibt bis nach dem nichtrekursiven rmdir offen.
                os.rmdir(claim_name, dir_fd=parent_fd)
            finally:
                os.close(cleanup_fd)
        except (OSError, RuntimeError) as error:
            claim_path = (
                str(parent / claim_name)
                if claim_name is not None
                else "noch kein Claim angelegt"
            )
            print(
                "Root-Helfer-Cleanup scheiterte; "
                f"Original-Recoverypfad {candidate}; Claim-Recoverypfad {claim_path}: {error}",
                file=sys.stderr,
            )
    if root_fd is not None:
        os.close(root_fd)
    os.close(parent_fd)
PY
)
old_ifs=$IFS
IFS='	'
set -- $test_root_binding
IFS=$old_ifs
if [ "$#" -ne 6 ]; then
  printf '%s\n' 'Testroot ließ sich nicht eindeutig an Pfad, Device, Inode, UID, Modus und Mount-ID binden.' >&2
  exit 1
fi
test_root=$1
test_root_device=$2
test_root_inode=$3
test_root_uid=$4
test_root_mode=$5
test_root_mount_id=$6
test_root_owned=1
exit_if_signaled

server_root="$test_root/root"
# Der Rootname ist ab jetzt nur noch für den eng gebundenen Abschlussclaim und
# für die explizite E2E-Identitätsprüfung sichtbar. Alle Runner-Artefakte werden
# über den bis zum EXIT-Cleanup gehaltenen Verzeichnisdeskriptor angesprochen.
cd "$test_root"
exec 7< .
test_root_handle=.
python3 - "$test_root_device" "$test_root_inode" \
  "$test_root_uid" "$test_root_mode" <<'PY'
import os
import stat
import sys

device, inode, uid, mode = sys.argv[1:]
opened = os.fstat(7)
named = os.stat(".", follow_symlinks=False)
if (
    not stat.S_ISDIR(opened.st_mode)
    or (opened.st_dev, opened.st_ino) != (int(device), int(inode))
    or (named.st_dev, named.st_ino) != (opened.st_dev, opened.st_ino)
    or opened.st_uid != int(uid)
    or stat.S_IMODE(opened.st_mode) != int(mode)
):
    raise SystemExit("Gehaltenes Root-Handle besitzt nicht die gebundene Identität")
PY
if [ "${DOPPELDECKER_SFTP_E2E_TEST_HOLD:-}" = "root-handle-claimed" ]; then
  root_handle_marker=${DOPPELDECKER_SFTP_E2E_TEST_MARKER:?}
  root_handle_ack=${DOPPELDECKER_SFTP_E2E_TEST_ROOT_HANDLE_ACK:?}
  root_handle_temporary="$root_handle_marker.$$.tmp"
  printf '%s\n' "$$" >"$root_handle_temporary"
  mv -f -- "$root_handle_temporary" "$root_handle_marker"
  while [ ! -e "$root_handle_ack" ]; do
    exit_if_signaled
    sleep 0.02
  done
fi
host_key=./ssh_host_ed25519_key
client_key=./client_ed25519
authorized_keys=./authorized_keys
config=./sshd_config
log=./sshd.log
pid_file=./sshd.pid
manifest=./manifest.json
session_launcher=./start-session.py
owner_marker=$(basename "$test_root")
server_root_binding=$(python3 - "$test_root_handle" "$test_root_device" "$test_root_inode" \
  "$test_root_uid" "$test_root_mode" "$test_root_mount_id" <<'PY'
import ctypes
import os
import stat
import sys

owner_path, owner_device, owner_inode, owner_uid, owner_mode, owner_mount_id = sys.argv[1:]
directory_flags = (
    os.O_RDONLY
    | getattr(os, "O_DIRECTORY", 0)
    | getattr(os, "O_CLOEXEC", 0)
    | getattr(os, "O_NOFOLLOW", 0)
)

def mount_identity(descriptor):
    if sys.platform.startswith("linux"):
        with open(f"/proc/self/fdinfo/{descriptor}", encoding="ascii") as source:
            for line in source:
                if line.startswith("mnt_id:"):
                    return f"linux:{line.split(':', 1)[1].strip()}"
        raise SystemExit("Linux lieferte keine Mount-ID für den Serverroot")
    if sys.platform == "darwin":
        class Fsid(ctypes.Structure):
            _fields_ = [("value", ctypes.c_int32 * 2)]

        class StatFs(ctypes.Structure):
            _fields_ = [
                ("block_size", ctypes.c_uint32), ("io_size", ctypes.c_int32),
                ("blocks", ctypes.c_uint64), ("blocks_free", ctypes.c_uint64),
                ("blocks_available", ctypes.c_uint64), ("files", ctypes.c_uint64),
                ("files_free", ctypes.c_uint64), ("fsid", Fsid),
                ("owner", ctypes.c_uint32), ("fs_type", ctypes.c_uint32),
                ("flags", ctypes.c_uint32), ("subtype", ctypes.c_uint32),
                ("type_name", ctypes.c_char * 16),
                ("mount_point", ctypes.c_char * 1024),
                ("mounted_from", ctypes.c_char * 1024),
                ("reserved", ctypes.c_uint32 * 8),
            ]
        information = StatFs()
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.fstatfs(descriptor, ctypes.byref(information)) != 0:
            raise SystemExit(f"fstatfs scheiterte: {os.strerror(ctypes.get_errno())}")
        mount_point = bytes(information.mount_point).split(b"\0", 1)[0].decode("utf-8", "strict")
        return f"darwin:{information.fsid.value[0]}:{information.fsid.value[1]}:{mount_point}"
    raise SystemExit("Diese Plattform bietet keine sichere Mount-ID-Prüfung")

owner_fd = os.open(owner_path, directory_flags)
try:
    owner = os.fstat(owner_fd)
    if (owner.st_dev, owner.st_ino) != (int(owner_device), int(owner_inode)):
        raise SystemExit("Eigentümerroot wechselte vor der Serverroot-Anlage")
    if owner.st_uid != int(owner_uid) or stat.S_IMODE(owner.st_mode) != int(owner_mode):
        raise SystemExit("Eigentümerroot wechselte Besitzer oder Modus")
    if mount_identity(owner_fd) != owner_mount_id:
        raise SystemExit("Eigentümerroot wechselte vor der Serverroot-Anlage seine Mount-ID")

    # Die Anlage relativ zum gehaltenen Eigentümerroot ist exklusiv. Direkt danach
    # werden die Identitätswerte aus demselben Deskriptor gebunden; ein vorhandener
    # passend benannter Fremdordner wird niemals übernommen.
    os.mkdir("root", 0o700, dir_fd=owner_fd)
    server_before = os.stat("root", dir_fd=owner_fd, follow_symlinks=False)
    server_fd = os.open("root", directory_flags, dir_fd=owner_fd)
    try:
        server = os.fstat(server_fd)
        if (server.st_dev, server.st_ino) != (server_before.st_dev, server_before.st_ino):
            raise SystemExit("Serverroot wechselte unmittelbar nach seiner Anlage")
        if server.st_uid != os.geteuid() or stat.S_IMODE(server.st_mode) != 0o700:
            raise SystemExit("Serverroot besitzt nicht UID und Modus des Testprozesses")
        if os.listdir(server_fd):
            raise SystemExit("Exklusiv erzeugter Serverroot ist nicht leer")
        server_mount_id = mount_identity(server_fd)
        if server_mount_id != owner_mount_id:
            raise SystemExit("Serverroot liegt auf einer anderen Mount-ID")
        print(
            f"{server.st_dev}\t{server.st_ino}\t{server.st_uid}"
            f"\t{stat.S_IMODE(server.st_mode)}\t{server_mount_id}"
        )
    finally:
        os.close(server_fd)
finally:
    os.close(owner_fd)
PY
)
old_ifs=$IFS
IFS='	'
set -- $server_root_binding
IFS=$old_ifs
if [ "$#" -ne 5 ]; then
  printf '%s\n' 'Serverroot ließ sich nicht an Device, Inode, UID, Modus und Mount-ID binden.' >&2
  exit 1
fi
server_root_device=$1
server_root_inode=$2
server_root_uid=$3
server_root_mode=$4
server_root_mount_id=$5
exec 8< root
server_root_handle=root
python3 - "$server_root_device" "$server_root_inode" \
  "$server_root_uid" "$server_root_mode" <<'PY'
import os
import stat
import sys

device, inode, uid, mode = sys.argv[1:]
opened = os.fstat(8)
named = os.stat("root", follow_symlinks=False)
if (
    not stat.S_ISDIR(opened.st_mode)
    or (opened.st_dev, opened.st_ino) != (int(device), int(inode))
    or (named.st_dev, named.st_ino) != (opened.st_dev, opened.st_ino)
    or opened.st_uid != int(uid)
    or stat.S_IMODE(opened.st_mode) != int(mode)
):
    raise SystemExit("Gehaltenes Serverroot-Handle besitzt nicht die gebundene Identität")
PY
exit_if_signaled

# Der kleine Starter bestätigt die neue Sitzung atomar, bevor er das eigentliche
# Programm ausführt. Bis zu dieser Bestätigung darf das Shell-Cleanup nur die
# bekannte Kind-PID treffen; danach darf es sicher die eigene Prozessgruppe treffen.
cat > "$session_launcher" <<'PY'
import os
from pathlib import Path
import subprocess
import sys
import time

ready_path = Path(sys.argv[1])
ack_path = Path(sys.argv[2])
label = sys.argv[3]
status_path = Path(sys.argv[4])
command = sys.argv[5:]

if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == f"{label}-ready-timeout":
    marker = Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
    temporary = marker.with_name(f"{marker.name}.{os.getpid()}.tmp")
    temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
    os.replace(temporary, marker)
    # Der Starter bleibt länger als die normale Ready-Frist am Leben, erzeugt aber
    # kein Dienstkind. So ist der Timeout-Cleanup ohne fremde Prozesse messbar.
    time.sleep(8)
    raise SystemExit(0)

if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == f"{label}-before-setsid":
    marker = Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
    temporary = marker.with_name(f"{marker.name}.{os.getpid()}.tmp")
    temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
    os.replace(temporary, marker)
    while True:
        time.sleep(1)

if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == f"{label}-exit-before-ready":
    marker = Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
    temporary = marker.with_name(f"{marker.name}.{os.getpid()}.tmp")
    temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
    os.replace(temporary, marker)
    raise SystemExit(23)

os.setsid()
temporary = ready_path.with_name(f"{ready_path.name}.{os.getpid()}.tmp")
temporary.write_text(f"{os.getpid()}\n", encoding="utf-8")
os.replace(temporary, ready_path)
# Ready bestätigt nur die neue eigene Sitzung. Das Kind darf erst entstehen, wenn
# die Elternshell diese PGID in ihrem Cleanupzustand registriert und das ACK atomar
# zurückgeschrieben hat. So gibt es kein Fenster mit ungebundenem Dienstkind.
while True:
    try:
        if ack_path.read_text(encoding="utf-8").strip() == str(os.getpid()):
            break
    except FileNotFoundError:
        pass
    time.sleep(0.01)
# Der Dienst läuft als Kind eines Supervisors, der Gruppenleiter bleibt. Dadurch gehört
# die PGID bis zum gemeinsamen Cleanup nachweislich weiterhin zu diesem Start und
# kann nicht nach einem vorzeitigen Wait an einen fremden Prozess recycelt werden.
try:
    # Root- und Serverroot-Deskriptor bleiben auch im beaufsichtigten Kind offen.
    # Mehrere über /dev/fd gebundene Artefaktpfade (Agent-Socket, Schlüssel,
    # Statusdateien) wären sonst nach Pythons standardmäßigem close_fds wieder
    # bloß über den austauschbaren öffentlichen Rootnamen erreichbar.
    child = subprocess.Popen(command, pass_fds=(7, 8))
    return_code = child.wait()
    child_status = return_code if return_code >= 0 else 128 - return_code
except OSError as error:
    print(f"{label} konnte nicht gestartet werden: {error}", file=sys.stderr)
    child_status = 127

status_temporary = status_path.with_name(f"{status_path.name}.{os.getpid()}.tmp")
status_temporary.write_text(
    f"{os.getpid()} {child_status}\n", encoding="utf-8"
)
os.replace(status_temporary, status_path)
while True:
    time.sleep(1)
PY

acknowledge_group_start() {
  managed_pid=$1
  ack_file=$2
  label=$3
  if [ "${DOPPELDECKER_SFTP_E2E_TEST_HOLD:-}" = "$label-ready-before-ack" ]; then
    ack_marker=${DOPPELDECKER_SFTP_E2E_TEST_MARKER:?}
    ack_temporary="$ack_marker.$$.tmp"
    printf '%s\n' "$managed_pid" >"$ack_temporary"
    mv -f -- "$ack_temporary" "$ack_marker"
    while [ "$pending_signal" -eq 0 ]; do
      sleep 0.02 || true
    done
    exit_if_signaled
  fi
  ack_temporary="$ack_file.$$.tmp"
  printf '%s\n' "$managed_pid" >"$ack_temporary"
  mv -f -- "$ack_temporary" "$ack_file"
}

wait_for_group_ready() {
  managed_pid=$1
  ready_file=$2
  label=$3
  ready_attempt_limit=200
  # Im Timeout-Selbsttest muss die Frist vor dem zeitlich begrenzten Starter enden,
  # auch wenn macOS die kurzen Sleeps bündelt. Der normale Serverstart behält 200.
  if [ "${DOPPELDECKER_SFTP_E2E_TEST_HOLD:-}" = "$label-ready-timeout" ]; then
    ready_attempt_limit=20
  fi
  attempts=0
  while [ "$attempts" -lt "$ready_attempt_limit" ]; do
    if ready_file_matches_pid "$managed_pid" "$ready_file"; then
      return 0
    fi
    if ! kill -0 "$managed_pid" 2>/dev/null; then
      if wait "$managed_pid"; then
        child_status=0
      else
        child_status=$?
      fi
      printf '%s-Starter endete vor der bestätigten Prozessgruppe (Status %s).\n' \
        "$label" "$child_status" >&2
      if [ "$child_status" -eq 0 ]; then
        return 1
      fi
      return "$child_status"
    fi
    exit_if_signaled
    sleep 0.02
    attempts=$((attempts + 1))
  done
  printf '%s-Starter bestätigte seine Prozessgruppe nicht rechtzeitig.\n' "$label" >&2
  # Bis zur Ready-Datei ist ausschließlich die direkte Kind-PID gebunden. Der
  # Starter hat noch kein ACK und darf daher kein Dienstkind erzeugt haben. Wir
  # beenden und ernten genau dieses Kind vollständig, bevor der Aufrufer seine
  # Registrierung löscht; eine Prozessgruppe wird hier niemals vermutet.
  stop_managed_process "$managed_pid" 0
  return 1
}

clear_ssh_agent_registration() {
  own_ssh_agent=0
  ssh_agent_pid=""
  ssh_agent_group_ready=0
  ssh_agent_ready_file=""
  ssh_agent_ack_file=""
  ssh_agent_status_file=""
}

record_reaped_supervisor() {
  reaped_pid=$1
  reaped_label=$2
  if [ -n "${DOPPELDECKER_SFTP_E2E_TEST_REAP_LOG:-}" ]; then
    reap_log_records="${reap_log_records}${reaped_pid}
"
  fi
  if [ "${DOPPELDECKER_SFTP_E2E_TEST_HOLD:-}" = "$reaped_label-after-reap" ]; then
    reaped_marker=${DOPPELDECKER_SFTP_E2E_TEST_MARKER:?}
    reaped_temporary="$reaped_marker.$$.tmp"
    printf '%s\n' "$reaped_pid" >"$reaped_temporary"
    mv -f -- "$reaped_temporary" "$reaped_marker"
    sleep 0.3
  fi
}

wait_for_managed_child_status() {
  managed_pid=$1
  status_file=$2
  label=$3
  while :; do
    if [ -r "$status_file" ]; then
      managed_status_pid=""
      managed_child_status=""
      managed_status_extra=""
      IFS=' ' read -r managed_status_pid managed_child_status managed_status_extra \
        < "$status_file" || true
      case "$managed_child_status" in
        '' | *[!0-9]*) ;;
        *)
          if [ "$managed_status_pid" = "$managed_pid" ] \
            && [ -z "$managed_status_extra" ] \
            && [ "$managed_child_status" -le 255 ]
          then
            return 0
          fi
          ;;
      esac
      printf '%s-Statusdatei war nicht eindeutig an den eigenen Supervisor gebunden.\n' \
        "$label" >&2
      return 1
    fi
    if ! kill -0 "$managed_pid" 2>/dev/null; then
      # Der Leiter wird hier absichtlich noch nicht geerntet. Solange seine PID als
      # Zombie gebunden bleibt, kann der EXIT-Cleanup die bestätigte Gruppe sicher
      # beenden und erst danach gemeinsam warten.
      printf '%s-Supervisor endete vor dem gebundenen Kindstatus.\n' "$label" >&2
      return 1
    fi
    exit_if_signaled
    sleep 0.02
  done
}

wait_for_ssh_agent_socket() {
  attempts=0
  while [ "$attempts" -lt 200 ]; do
    if [ -S "$SSH_AUTH_SOCK" ] && kill -0 "$ssh_agent_pid" 2>/dev/null; then
      return 0
    fi
    if [ -r "$ssh_agent_status_file" ]; then
      agent_status_pid=""
      agent_status=""
      agent_status_extra=""
      IFS=' ' read -r agent_status_pid agent_status agent_status_extra \
        < "$ssh_agent_status_file" || true
      case "$agent_status" in
        '' | *[!0-9]*) agent_status=1 ;;
      esac
      if [ "$agent_status_pid" != "$ssh_agent_pid" ] \
        || [ -n "$agent_status_extra" ] \
        || [ "$agent_status" -gt 255 ]
      then
        agent_status=1
      fi
      printf 'Eigener ssh-agent endete vor seinem Socket (Status %s).\n' \
        "$agent_status" >&2
      return "$agent_status"
    fi
    if ! kill -0 "$ssh_agent_pid" 2>/dev/null; then
      if wait "$ssh_agent_pid"; then
        agent_status=0
      else
        agent_status=$?
      fi
      # Nach dem Wait ist die Prozessidentität verbraucht. Eine bloße Existenzprobe
      # derselben Zahl dürfte eine inzwischen fremde PGID nicht wieder zu unserem
      # Besitz erklären; deshalb immer bedingungslos deregistrieren.
      clear_ssh_agent_registration
      printf 'Eigener ssh-agent endete vor seinem Socket (Status %s).\n' \
        "$agent_status" >&2
      if [ "$agent_status" -eq 0 ]; then
        return 1
      fi
      return "$agent_status"
    fi
    exit_if_signaled
    sleep 0.02
    attempts=$((attempts + 1))
  done
  printf '%s\n' 'Eigener ssh-agent stellte seinen Socket nicht rechtzeitig bereit.' >&2
  return 1
}

ssh-keygen -q -t ed25519 -N '' -f "$host_key"
ssh-keygen -q -t ed25519 -N '' -f "$client_key"
cp "$client_key.pub" "$authorized_keys"
chmod 600 "$authorized_keys" "$host_key" "$client_key"

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
user=$(id -un)
cat > "$config" <<EOF
Port $port
ListenAddress 127.0.0.1
HostKey $test_root/ssh_host_ed25519_key
PidFile ./sshd.pid
AuthorizedKeysFile $test_root/authorized_keys
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
PermitRootLogin no
AllowUsers $user
Subsystem sftp $sftp_subsystem
LogLevel VERBOSE
EOF

fingerprint_hex() {
  ssh-keygen -lf "$1" -E sha256 \
    | awk '{ sub(/^SHA256:/, "", $2); print $2 }' \
    | python3 -c 'import base64,sys; value=sys.stdin.read().strip(); value += "=" * (-len(value) % 4); print(base64.b64decode(value).hex())'
}
host_algorithm=$(awk '{ print $1 }' "$host_key.pub")
host_fingerprint=$(fingerprint_hex "$host_key.pub")
client_fingerprint=$(fingerprint_hex "$client_key.pub")

python3 - "$manifest" "$server_root_handle" "$server_root" "$owner_marker" "$port" \
  "$host_algorithm" \
  "$host_fingerprint" "$test_root_device" "$test_root_inode" "$test_root_uid" \
  "$test_root_mode" "$test_root_mount_id" "$server_root_device" \
  "$server_root_inode" "$server_root_uid" "$server_root_mode" \
  "$server_root_mount_id" <<'PY'
import json
from pathlib import Path
import sys

(
    manifest,
    bound_root,
    root,
    owner,
    port,
    algorithm,
    fingerprint,
    owner_device,
    owner_inode,
    owner_uid,
    owner_mode,
    owner_mount_id,
    server_device,
    server_inode,
    server_uid,
    server_mode,
    server_mount_id,
) = sys.argv[1:]
document = {
    "provider": {
        "kind": "sftp",
        "host": "127.0.0.1",
        "port": int(port),
        "host_key_algorithm": algorithm,
        "host_key_sha256": fingerprint,
    },
    "virtual_root": root,
    "canonical_virtual_root": str(Path(bound_root).resolve(strict=True)),
    "ownership_marker": owner,
    "owner_root": {
        "device": int(owner_device),
        "inode": int(owner_inode),
        "uid": int(owner_uid),
        "mode": int(owner_mode),
        "mount_id": owner_mount_id,
    },
    "server_root": {
        "device": int(server_device),
        "inode": int(server_inode),
        "uid": int(server_uid),
        "mode": int(server_mode),
        "mount_id": server_mount_id,
    },
    "planned_entries": [
        "mkdir-e2e",
        "ziel/quelle.txt",
        "f5-upload.txt",
        "f5-download.txt",
        "f6-move.txt",
        "rename-source",
        "delete-e2e",
        "stream-abort",
        "best-effort-upload.txt",
    ],
}
with open(manifest, "x", encoding="utf-8") as target:
    json.dump(document, target, sort_keys=True)
    target.write("\n")
PY

exit_if_signaled
# Ein daemonisierender `ssh-agent -s` könnte schon laufen, bevor seine Ausgabe
# vollständig ankommt. Darum bleibt der Agent hier im Vordergrund, erhält einen
# expliziten Socket und wird unmittelbar über die Kind-PID als unser Besitz gebunden.
# Geerbte Agentwerte gehen weder an den Starter noch an den neuen Agentprozess.
# Der Agent legt den Socket relativ zum descriptorverifizierten cwd an. Cargo
# startet Testbinaries jedoch aus dem Workspace; für deren Clientzugriff braucht
# SSH_AUTH_SOCK deshalb den öffentlichen absoluten Namen. Ein Rootaustausch kann
# dort höchstens die Verbindung verweigern: Der SFTP-Test bindet zusätzlich den
# erwarteten öffentlichen Agent-Key-Fingerprint und übernimmt keinen Fremdagenten.
SSH_AUTH_SOCK="$test_root/agent.sock"
export SSH_AUTH_SOCK
ssh_agent_ready_file=./ssh-agent-session.ready
ssh_agent_ack_file=./ssh-agent-session.ack
ssh_agent_status_file=./ssh-agent-status
env -u SSH_AGENT_PID -u SSH_AUTH_SOCK \
  python3 "$session_launcher" "$ssh_agent_ready_file" "$ssh_agent_ack_file" \
    ssh-agent "$ssh_agent_status_file" \
    ssh-agent -D -a ./agent.sock >./ssh-agent.log 2>&1 &
ssh_agent_pid=$!
SSH_AGENT_PID=$ssh_agent_pid
export SSH_AGENT_PID
own_ssh_agent=1
if wait_for_group_ready "$ssh_agent_pid" "$ssh_agent_ready_file" ssh-agent; then
  ssh_agent_group_ready=1
  acknowledge_group_start "$ssh_agent_pid" "$ssh_agent_ack_file" ssh-agent
else
  agent_start_status=$?
  # `wait_for_group_ready` hat einen vor Ready beendeten Starter bereits geerntet.
  # Seine numerische PID/PGID darf ab hier nie wieder als Besitzbeleg dienen.
  clear_ssh_agent_registration
  exit "$agent_start_status"
fi
wait_for_ssh_agent_socket
exit_if_signaled
ssh-add "$client_key" >/dev/null 2>&1
exit_if_signaled
sshd_ready_file=./sshd-session.ready
sshd_ack_file=./sshd-session.ack
sshd_status_file=./sshd-status
python3 "$session_launcher" "$sshd_ready_file" "$sshd_ack_file" sshd "$sshd_status_file" \
  "$sshd_bin" -D -e -f "$config" >"$log" 2>&1 &
sshd_pid=$!
if wait_for_group_ready "$sshd_pid" "$sshd_ready_file" sshd; then
  sshd_group_ready=1
  acknowledge_group_start "$sshd_pid" "$sshd_ack_file" sshd
else
  sshd_start_status=$?
  sshd_pid=""
  sshd_group_ready=0
  sshd_ready_file=""
  sshd_ack_file=""
  sshd_status_file=""
  exit "$sshd_start_status"
fi
exit_if_signaled

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
    raise SystemExit("SFTP-Testserver startete nicht")
PY

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1090
  . "$HOME/.cargo/env"
fi
cargo_list_output=./cargo-list.out
cargo_ready_file=./cargo-list-session.ready
cargo_ack_file=./cargo-list-session.ack
cargo_status_file=./cargo-list-status
exit_if_signaled
python3 "$session_launcher" "$cargo_ready_file" "$cargo_ack_file" \
  cargo-listing "$cargo_status_file" \
  cargo test --manifest-path "$project_dir/Cargo.toml" -p doppeldecker-core \
    --test sftp_e2e -- --list >"$cargo_list_output" &
cargo_pid=$!
if wait_for_group_ready "$cargo_pid" "$cargo_ready_file" cargo-listing; then
  cargo_group_ready=1
  acknowledge_group_start "$cargo_pid" "$cargo_ack_file" cargo-listing
else
  cargo_start_status=$?
  cargo_pid=""
  cargo_group_ready=0
  cargo_ready_file=""
  cargo_ack_file=""
  cargo_status_file=""
  exit "$cargo_start_status"
fi
exit_if_signaled
wait_for_managed_child_status "$cargo_pid" "$cargo_status_file" cargo-listing
cargo_list_status=$managed_child_status
# Der Supervisor ist hier weiterhin der lebende Gruppenleiter. Erst der gemeinsame
# Gruppen-Cleanup beendet mögliche Nachkommen und erntet anschließend exakt ihn.
stop_managed_process "$cargo_pid" "$cargo_group_ready"
reaped_cargo_pid=$cargo_pid
cargo_pid=""
cargo_group_ready=0
cargo_ready_file=""
cargo_ack_file=""
cargo_status_file=""
record_reaped_supervisor "$reaped_cargo_pid" cargo-listing
exit_if_signaled
if [ "$cargo_list_status" -ne 0 ]; then
  exit "$cargo_list_status"
fi
listed_tests=$(cat "$cargo_list_output")
exact_matches=$(printf '%s\n' "$listed_tests" | awk -F ': ' -v name="$test_name" '
  $1 == name && $2 == "test" { count += 1 }
  END { print count + 0 }
')
if [ "$exact_matches" -ne 1 ]; then
  printf 'Erwarteter SFTP-E2E-Test „%s“ wurde %s-mal exakt gefunden.\n' \
    "$test_name" "$exact_matches" >&2
  exit 1
fi
exit_if_signaled
cargo_ready_file=./cargo-session.ready
cargo_ack_file=./cargo-session.ack
cargo_status_file=./cargo-status
DOPPELDECKER_SFTP_TEST_HOST=127.0.0.1 \
DOPPELDECKER_SFTP_TEST_PORT="$port" \
DOPPELDECKER_SFTP_TEST_USER="$user" \
DOPPELDECKER_SFTP_TEST_ROOT="$server_root" \
DOPPELDECKER_SFTP_TEST_OWNER_DEVICE="$test_root_device" \
DOPPELDECKER_SFTP_TEST_OWNER_INODE="$test_root_inode" \
DOPPELDECKER_SFTP_TEST_OWNER_UID="$test_root_uid" \
DOPPELDECKER_SFTP_TEST_OWNER_MODE="$test_root_mode" \
DOPPELDECKER_SFTP_TEST_OWNER_MOUNT_ID="$test_root_mount_id" \
DOPPELDECKER_SFTP_TEST_SERVER_DEVICE="$server_root_device" \
DOPPELDECKER_SFTP_TEST_SERVER_INODE="$server_root_inode" \
DOPPELDECKER_SFTP_TEST_SERVER_UID="$server_root_uid" \
DOPPELDECKER_SFTP_TEST_SERVER_MODE="$server_root_mode" \
DOPPELDECKER_SFTP_TEST_SERVER_MOUNT_ID="$server_root_mount_id" \
DOPPELDECKER_SFTP_TEST_HOST_KEY_ALGORITHM="$host_algorithm" \
DOPPELDECKER_SFTP_TEST_HOST_KEY_SHA256_HEX="$host_fingerprint" \
DOPPELDECKER_SFTP_TEST_AGENT_KEY_SHA256_HEX="$client_fingerprint" \
DOPPELDECKER_SFTP_EXPECT_NO_FSYNC="$disable_fsync" \
  python3 "$session_launcher" "$cargo_ready_file" "$cargo_ack_file" cargo "$cargo_status_file" \
    cargo test --manifest-path "$project_dir/Cargo.toml" -p doppeldecker-core \
      --test sftp_e2e "$test_name" -- --exact --ignored --nocapture &
cargo_pid=$!
if wait_for_group_ready "$cargo_pid" "$cargo_ready_file" cargo; then
  cargo_group_ready=1
  acknowledge_group_start "$cargo_pid" "$cargo_ack_file" cargo
else
  cargo_start_status=$?
  cargo_pid=""
  cargo_group_ready=0
  cargo_ready_file=""
  cargo_ack_file=""
  cargo_status_file=""
  exit "$cargo_start_status"
fi
exit_if_signaled
wait_for_managed_child_status "$cargo_pid" "$cargo_status_file" cargo
cargo_status=$managed_child_status
stop_managed_process "$cargo_pid" "$cargo_group_ready"
reaped_cargo_pid=$cargo_pid
cargo_pid=""
cargo_group_ready=0
cargo_ready_file=""
cargo_ack_file=""
cargo_status_file=""
record_reaped_supervisor "$reaped_cargo_pid" cargo
exit_if_signaled
exit "$cargo_status"
