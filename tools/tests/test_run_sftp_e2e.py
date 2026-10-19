from __future__ import annotations

import ctypes
from dataclasses import dataclass
import errno
import os
from pathlib import Path
import secrets
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "run-sftp-e2e.sh"
TEST_ROOT_PREFIX = "doppeldecker-sftp-e2e."


@dataclass(frozen=True)
class OwnedTestRoot:
    """Unveränderliche Besitzbindung aus zwei stabilen descriptor-relativen Reads."""

    path: Path
    device: int
    inode: int
    uid: int
    mode: int
    mount_identity: str

    def __str__(self) -> str:
        return str(self.path)

    def __truediv__(self, child: str) -> Path:
        return self.path / child

    @property
    def name(self) -> str:
        return self.path.name

    def exists(self) -> bool:
        return self.path.exists()

    def lstat(self) -> os.stat_result:
        return self.path.lstat()

    def with_name(self, name: str) -> Path:
        return self.path.with_name(name)

    def rename(self, target: Path) -> Path:
        return self.path.rename(target)


def write_executable(path: Path, body: str) -> None:
    path.write_text(textwrap.dedent(body).lstrip(), encoding="utf-8")
    path.chmod(0o755)


def wait_for_path(path: Path, process: subprocess.Popen[str]) -> None:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if path.exists():
            return
        if process.poll() is not None:
            raise AssertionError(
                f"SFTP-Testskript endete vor dem Testhaltepunkt ({process.returncode})."
            )
        time.sleep(0.02)
    raise AssertionError(f"Testhaltepunkt wurde nicht erreicht: {path}")


def _validate_test_artifact_lexically(value: str | Path) -> Path:
    """Bindet nur einen unverändert normalisierten direkten temporären Namen."""
    raw = str(value)
    if not raw or "\n" in raw or "\r" in raw or "\0" in raw:
        raise AssertionError("Der künstliche SFTP-Root ist leer oder mehrzeilig")
    candidate = Path(raw)
    if not candidate.is_absolute() or os.path.normpath(raw) != raw:
        raise AssertionError("Der künstliche SFTP-Root ist nicht absolut und normalisiert")
    if candidate.parent not in {Path("/tmp"), Path("/private/tmp")}:
        raise AssertionError("Der künstliche SFTP-Root liegt nicht direkt unter /tmp")
    if not candidate.name.startswith(TEST_ROOT_PREFIX) or not candidate.name.removeprefix(
        TEST_ROOT_PREFIX
    ):
        raise AssertionError("Der künstliche SFTP-Root besitzt nicht den festen Präfix")

    # Diese ausdrückliche zweite Grenze macht die Absicht auch für Anfänger klar:
    # Repo, Arbeitsordner, Home und ihre Eltern oder Kinder sind niemals Testroots.
    protected = {ROOT.resolve(), Path.cwd().resolve(), Path.home().resolve()}
    canonical_candidate = candidate.parent.resolve() / candidate.name
    for path in protected:
        if (
            canonical_candidate == path
            or canonical_candidate in path.parents
            or path in canonical_candidate.parents
        ):
            raise AssertionError("Der künstliche SFTP-Root überlappt einen Schutzpfad")
    return candidate


def validate_owned_test_root(value: str | Path) -> Path:
    candidate = _validate_test_artifact_lexically(value)
    try:
        metadata = candidate.lstat()
    except FileNotFoundError:
        return candidate
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise AssertionError("Der künstliche SFTP-Root ist kein echtes Verzeichnis")
    resolved = candidate.resolve(strict=True)
    resolved_metadata = resolved.lstat()
    if (resolved_metadata.st_dev, resolved_metadata.st_ino) != (
        metadata.st_dev,
        metadata.st_ino,
    ):
        raise AssertionError("Der künstliche SFTP-Root wechselte seine Identität")
    return resolved


def _directory_open_flags() -> int:
    return (
        os.O_RDONLY
        | getattr(os, "O_DIRECTORY", 0)
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
    )


def _descriptor_mount_identity(descriptor: int) -> str:
    if sys.platform.startswith("linux"):
        with open(f"/proc/self/fdinfo/{descriptor}", encoding="ascii") as source:
            for line in source:
                if line.startswith("mnt_id:"):
                    return f"linux:{line.split(':', 1)[1].strip()}"
        raise AssertionError("Linux lieferte keine Mount-ID für den Testdeskriptor")
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
            raise AssertionError(
                f"fstatfs scheiterte: {os.strerror(ctypes.get_errno())}"
            )
        mount_point = bytes(information.mount_point).split(b"\0", 1)[0].decode(
            "utf-8", "strict"
        )
        return (
            f"darwin:{information.fsid.value[0]}:{information.fsid.value[1]}:"
            f"{mount_point}"
        )
    raise AssertionError("Diese Plattform bietet keine sichere Mount-ID-Prüfung")


def _empty_directory_descriptor(
    directory_fd: int, root_mount_identity: str | None = None
) -> None:
    """Leert ausschließlich den bereits geöffneten Testordner, nie einen Pfadbaum."""
    root_device = os.fstat(directory_fd).st_dev
    current_mount_identity = _descriptor_mount_identity(directory_fd)
    if root_mount_identity is None:
        root_mount_identity = current_mount_identity
    if current_mount_identity != root_mount_identity:
        raise AssertionError("Test-Cleanup betritt keine fremde Mount-ID")
    for name in os.listdir(directory_fd):
        before = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if before.st_dev != root_device:
            raise AssertionError("Test-Cleanup betritt keine Mountgrenze")
        if stat.S_ISDIR(before.st_mode):
            child_fd = os.open(name, _directory_open_flags(), dir_fd=directory_fd)
            try:
                opened = os.fstat(child_fd)
                if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                    raise AssertionError("Test-Unterordner wechselte seine Identität")
                if opened.st_dev != root_device:
                    raise AssertionError("Test-Unterordner liegt auf einem anderen Device")
                if _descriptor_mount_identity(child_fd) != root_mount_identity:
                    raise AssertionError("Test-Unterordner liegt auf einer anderen Mount-ID")
                _empty_directory_descriptor(child_fd, root_mount_identity)
            finally:
                os.close(child_fd)
            current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (before.st_dev, before.st_ino):
                raise AssertionError("Test-Unterordner wurde vor rmdir ausgetauscht")
            os.rmdir(name, dir_fd=directory_fd)
        else:
            current = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if (current.st_dev, current.st_ino) != (before.st_dev, before.st_ino):
                raise AssertionError("Testeintrag wurde vor unlink ausgetauscht")
            os.unlink(name, dir_fd=directory_fd)


def _rename_noreplace(old_name: str, new_name: str, parent_fd: int) -> None:
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
        result = renameat2(parent_fd, old_bytes, parent_fd, new_bytes, 1)
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
        result = renameatx_np(parent_fd, old_bytes, parent_fd, new_bytes, 0x00000004)
    else:
        raise OSError(errno.ENOTSUP, "Kein exklusives Rename auf dieser Plattform")
    if result == 0:
        return
    number = ctypes.get_errno()
    if number == errno.EEXIST:
        raise FileExistsError(number, os.strerror(number), new_name)
    raise OSError(number, os.strerror(number), old_name)


def _metadata_matches_binding(metadata: os.stat_result, root: OwnedTestRoot) -> bool:
    return (
        (metadata.st_dev, metadata.st_ino) == (root.device, root.inode)
        and metadata.st_uid == root.uid
        and stat.S_IMODE(metadata.st_mode) == root.mode
        and stat.S_ISDIR(metadata.st_mode)
    )


def bind_owned_test_root(value: str | Path) -> OwnedTestRoot:
    candidate = _validate_test_artifact_lexically(value)
    parent = candidate.parent.resolve(strict=True)
    parent_fd = os.open(parent, _directory_open_flags())
    descriptor = None
    try:
        before = os.stat(candidate.name, dir_fd=parent_fd, follow_symlinks=False)
        descriptor = os.open(candidate.name, _directory_open_flags(), dir_fd=parent_fd)
        opened = os.fstat(descriptor)
        after = os.stat(candidate.name, dir_fd=parent_fd, follow_symlinks=False)
        if not stat.S_ISDIR(opened.st_mode) or stat.S_ISLNK(before.st_mode):
            raise AssertionError("Der künstliche SFTP-Root ist kein echtes Verzeichnis")
        if (before.st_dev, before.st_ino) != (opened.st_dev, opened.st_ino) or (
            after.st_dev,
            after.st_ino,
        ) != (opened.st_dev, opened.st_ino):
            raise AssertionError("Der künstliche SFTP-Root wechselte seine Identität")
        if (
            before.st_uid != opened.st_uid
            or after.st_uid != opened.st_uid
            or stat.S_IMODE(before.st_mode) != stat.S_IMODE(opened.st_mode)
            or stat.S_IMODE(after.st_mode) != stat.S_IMODE(opened.st_mode)
        ):
            raise AssertionError("Der künstliche SFTP-Root wechselte Besitzer oder Modus")
        canonical = parent / candidate.name
        return OwnedTestRoot(
            path=canonical,
            device=opened.st_dev,
            inode=opened.st_ino,
            uid=opened.st_uid,
            mode=stat.S_IMODE(opened.st_mode),
            mount_identity=_descriptor_mount_identity(descriptor),
        )
    finally:
        if descriptor is not None:
            os.close(descriptor)
        os.close(parent_fd)


def _remove_bound_test_directory(root: OwnedTestRoot) -> None:
    lexical = _validate_test_artifact_lexically(root.path)
    parent = lexical.parent.resolve(strict=True)
    parent_fd = os.open(parent, _directory_open_flags())
    root_fd = None
    claim_fd = None
    claim_name = None
    try:
        try:
            before = os.stat(lexical.name, dir_fd=parent_fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        if not _metadata_matches_binding(before, root):
            raise AssertionError("Testroot wechselte vor dem Cleanup seine Besitzbindung")
        root_fd = os.open(lexical.name, _directory_open_flags(), dir_fd=parent_fd)
        opened = os.fstat(root_fd)
        if not _metadata_matches_binding(opened, root):
            raise AssertionError("Testroot wechselte vor dem Cleanup seine Besitzbindung")
        if _descriptor_mount_identity(root_fd) != root.mount_identity:
            raise AssertionError("Testroot wechselte vor dem Cleanup seine Mount-ID")

        for _ in range(128):
            proposed = f"{TEST_ROOT_PREFIX}cleanup-{secrets.token_hex(12)}"
            try:
                _rename_noreplace(lexical.name, proposed, parent_fd)
            except FileExistsError:
                continue
            claim_name = proposed
            break
        if claim_name is None:
            raise AssertionError("Kein exklusiver Test-Cleanup-Claim verfügbar")

        claim_fd = os.open(claim_name, _directory_open_flags(), dir_fd=parent_fd)
        claim = os.fstat(claim_fd)
        claim_at_name = os.stat(claim_name, dir_fd=parent_fd, follow_symlinks=False)
        if not _metadata_matches_binding(claim, root) or not _metadata_matches_binding(
            claim_at_name, root
        ):
            raise AssertionError("Test-Cleanup-Claim besitzt nicht die ursprüngliche Bindung")
        if _descriptor_mount_identity(claim_fd) != root.mount_identity:
            raise AssertionError("Test-Cleanup-Claim wechselte die ursprüngliche Mount-ID")

        _empty_directory_descriptor(root_fd, root.mount_identity)
        final_claim = os.stat(claim_name, dir_fd=parent_fd, follow_symlinks=False)
        if not _metadata_matches_binding(final_claim, root):
            raise AssertionError("Test-Cleanup-Claim driftete vor rmdir")
        if _descriptor_mount_identity(claim_fd) != root.mount_identity:
            raise AssertionError("Test-Cleanup-Claim driftete vor rmdir auf eine andere Mount-ID")
        os.rmdir(claim_name, dir_fd=parent_fd)
    finally:
        if claim_fd is not None:
            os.close(claim_fd)
        if root_fd is not None:
            os.close(root_fd)
        os.close(parent_fd)


def read_owned_test_root(
    root_file: Path,
    process: subprocess.Popen[str] | None = None,
    *,
    require_exists: bool = False,
) -> OwnedTestRoot:
    deadline = time.monotonic() + (15 if process is not None else 1)
    previous: OwnedTestRoot | None = None
    while time.monotonic() < deadline:
        try:
            first_value = root_file.read_text(encoding="utf-8")
            candidate = bind_owned_test_root(first_value)
            second_value = root_file.read_text(encoding="utf-8")
            if second_value != first_value:
                raise AssertionError("Die Rootveröffentlichung wechselte während der Bindung")
        except (AssertionError, OSError, UnicodeError):
            candidate = None
        if candidate is not None:
            if candidate == previous:
                return candidate
            previous = candidate
        else:
            previous = None
        if process is not None and process.poll() is not None:
            raise AssertionError(
                f"SFTP-Testskript endete vor stabiler Root-Veröffentlichung ({process.returncode})."
            )
        time.sleep(0.02)
    raise AssertionError("Kein nichtleerer, stabiler und sicherer SFTP-Testroot veröffentlicht")


def read_published_test_root_path(root_file: Path) -> Path:
    first = root_file.read_text(encoding="utf-8")
    second = root_file.read_text(encoding="utf-8")
    if first != second:
        raise AssertionError("Die Rootveröffentlichung wechselte zwischen zwei Reads")
    return _validate_test_artifact_lexically(first).parent.resolve() / Path(first).name


def remove_owned_test_root(root: OwnedTestRoot) -> None:
    if not isinstance(root, OwnedTestRoot):
        raise AssertionError("Testcleanup verlangt die ursprüngliche vollständige Rootbindung")
    _remove_bound_test_directory(root)


def remove_exact_test_artifact(path: Path, device: int, inode: int) -> None:
    """Entfernt nur ein im Test selbst mit Identität gebundenes /tmp-Artefakt."""
    lexical = _validate_test_artifact_lexically(path)
    try:
        current = lexical.lstat()
    except FileNotFoundError:
        return
    if (current.st_dev, current.st_ino) != (device, inode):
        raise AssertionError("Testartefakt wechselte vor dem Test-Cleanup seine Identität")
    if stat.S_ISDIR(current.st_mode):
        binding = bind_owned_test_root(lexical)
        if (binding.device, binding.inode) != (device, inode):
            raise AssertionError("Testartefakt wechselte vor dem Test-Cleanup seine Identität")
        _remove_bound_test_directory(binding)
        return

    parent_fd = os.open(lexical.parent.resolve(strict=True), _directory_open_flags())
    try:
        rebound = os.stat(lexical.name, dir_fd=parent_fd, follow_symlinks=False)
        if (rebound.st_dev, rebound.st_ino) != (device, inode):
            raise AssertionError("Testartefakt wurde vor unlink ausgetauscht")
        os.unlink(lexical.name, dir_fd=parent_fd)
    finally:
        os.close(parent_fd)


def process_is_running(pid: int) -> bool:
    proc_stat = Path(f"/proc/{pid}/stat")
    if proc_stat.exists():
        try:
            # Ein bereits beendeter, aber vom Container-PID-1 noch nicht geernteter
            # Zombie hält `kill(pid, 0)` erfolgreich. Für den Unterprozessvertrag
            # zählt er trotzdem als beendet.
            if proc_stat.read_text(encoding="utf-8").split()[2] == "Z":
                return False
        except (IndexError, OSError):
            pass
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def stop_owned_test_process(process: subprocess.Popen[str]) -> None:
    # `Popen` bleibt bis zum Wait ein unreaptes direktes Kind; seine PID kann daher
    # nicht recycelt werden. Aus Statusdateien gelesene Supervisor-/Agent-PIDs haben
    # diesen Besitzbeleg nicht und werden im `finally` niemals signalisiert.
    if process.poll() is not None:
        process.communicate(timeout=1)
        return
    process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
    process.communicate(timeout=1)


def run_owned_runner(environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
    command = [str(SCRIPT)]
    child = subprocess.Popen(command, cwd=ROOT, env=environment,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    failure: subprocess.TimeoutExpired | None = None
    try:
        stdout, stderr = child.communicate(timeout=15)
        return subprocess.CompletedProcess(command, child.returncode, stdout, stderr)
    except subprocess.TimeoutExpired as error:
        failure = error
        diagnostics: dict[str, object] = {"runner_pid": child.pid}
        try:
            state = Path(environment["FAKE_SFTP_STATE"])
            root = bind_owned_test_root(read_published_test_root_path(state / "root.txt"))
            for directory, names in [
                (root.path, ["ssh-agent-session.ready", "ssh-agent-session.ack",
                    "ssh-agent-status", "ssh-agent.log", "agent.sock"]),
                (state, ["agent.pid"]),
            ]:
                directory_fd = os.open(directory, _directory_open_flags())
                try:
                    if directory == root.path and not _metadata_matches_binding(os.fstat(directory_fd), root):
                        raise AssertionError("Diagnoseroot wechselte seine Bindung")
                    for name in names:
                        try:
                            info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
                            diagnostics[name] = {"mode": stat.S_IFMT(info.st_mode), "size": info.st_size}
                            if name.endswith((".ready", ".ack", "-status", ".pid")):
                                descriptor = os.open(name, os.O_RDONLY | os.O_NONBLOCK
                                    | getattr(os, "O_NOFOLLOW", 0), dir_fd=directory_fd)
                                try:
                                    opened = os.fstat(descriptor)
                                    if not stat.S_ISREG(opened.st_mode):
                                        raise AssertionError("Keine reguläre Diagnosedatei")
                                    diagnostics[name] = os.read(descriptor, 128).decode("ascii", errors="replace")
                                finally:
                                    os.close(descriptor)
                        except (OSError, AssertionError) as diagnostic_error:
                            diagnostics[name] = repr(diagnostic_error)
                finally:
                    os.close(directory_fd)
        except (OSError, AssertionError) as diagnostic_error:
            diagnostics["snapshot_error"] = repr(diagnostic_error)
        error.add_note(f"SFTP-Timeoutdiagnose vor TERM: {diagnostics!r}")
        raise
    finally:
        try:
            stop_owned_test_process(child)
        except Exception as cleanup_error:
            if failure is None:
                raise
            failure.add_note(f"Runner-Cleanup zusätzlich fehlgeschlagen: {cleanup_error!r}")


class SftpE2eSignalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        fixture = tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-commands-")
        cls.addClassCleanup(fixture.cleanup)
        cls._fixture_bin = Path(fixture.name)
        cls.write_fake_commands(cls._fixture_bin)
        # macOS kann neue ausführbare Skripte noch vor der ersten Python-Zeile
        # prüfen. Diese begrenzte Fixture-Vorbereitung gehört vor die getakteten
        # Signalfälle; deren Ready-/Cleanupfristen bleiben unverändert.
        for command in sorted(cls._fixture_bin.iterdir()):
            command.chmod(0o555)
            subprocess.run(
                [str(command), "--fixture-ready"], stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                timeout=30, check=True,
            )

    def test_runner_timeout_laesst_dem_eigenen_kind_den_term_cleanup(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-runner-timeout-") as directory:
            root = Path(directory)
            sentinel = root / "cleanup"
            runner = self._fixture_bin / "runner"
            with mock.patch(f"{__name__}.SCRIPT", runner):
                with self.assertRaises(subprocess.TimeoutExpired):
                    run_owned_runner({**os.environ, "FAKE_SFTP_STATE": str(root)})
            self.assertEqual(sentinel.read_text(), "TERM")

    def test_root_helferfehler_entfernt_nur_seinen_exakt_gebundenen_leerroot(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-root-helper-fail-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()

            for failure in ("mount-id", "late"):
                with self.subTest(failure=failure):
                    environment["DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_FAIL"] = failure
                    process = subprocess.run(
                        [str(SCRIPT)],
                        cwd=ROOT,
                        env=environment,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        timeout=15,
                        check=False,
                    )
                    self.assertNotEqual(process.returncode, 0)
                    created = read_published_test_root_path(state / "root.txt")
                    self.assertFalse(
                        created.exists(),
                        f"Der vor Übergabe fehlgeschlagene Root blieb zurück: {created}",
                    )
                    self.assertFalse(
                        list(state.glob("*.pid")),
                        "Ein Vor-Übergabe-Fehler darf noch keinen Agent-, sshd- oder Cargo-Prozess starten",
                    )

    def test_root_helfer_cleanup_loescht_nach_close_keinen_ersatz(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-root-helper-swap-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            marker = state / "root-close.ready"
            acknowledgement = state / "root-close.continue"
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_FAIL": "late",
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_CLOSE_MARKER": str(marker),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_CLOSE_ACK": str(
                        acknowledgement
                    ),
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            cleanup_records: list[tuple[Path, int, int]] = []
            try:
                wait_for_path(marker, process)
                root = read_published_test_root_path(state / "root.txt")

                # Der alte Ablauf hatte den eigenen Deskriptor hier schon geschlossen.
                # Wir verschieben deshalb nur die exakt gelesene Testidentität und
                # stellen am alten Namen einen leeren Fremdordner bereit.
                displaced = root.with_name(f"{root.name}.test-displaced-{os.urandom(8).hex()}")
                if root.exists():
                    root.rename(displaced)
                    displaced_stat = displaced.lstat()
                    cleanup_records.append(
                        (displaced, displaced_stat.st_dev, displaced_stat.st_ino)
                    )
                replacement = root
                replacement.mkdir(mode=0o700)
                replacement_stat = replacement.lstat()
                cleanup_records.append(
                    (replacement, replacement_stat.st_dev, replacement_stat.st_ino)
                )
                acknowledgement.write_text("weiter\n", encoding="utf-8")

                stdout, stderr = process.communicate(timeout=8)
                self.assertNotEqual(process.returncode, 0)
                self.assertEqual(
                    (replacement.lstat().st_dev, replacement.lstat().st_ino),
                    (replacement_stat.st_dev, replacement_stat.st_ino),
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                if displaced.exists():
                    self.assertEqual(
                        (displaced.lstat().st_dev, displaced.lstat().st_ino),
                        (displaced_stat.st_dev, displaced_stat.st_ino),
                    )
                self.assertFalse(list(state.glob("*.pid")))
            finally:
                stop_owned_test_process(process)
                for path, device, inode in cleanup_records:
                    remove_exact_test_artifact(path, device, inode)

    def test_root_helfer_laesst_claim_bei_close_drift_als_recovery_stehen(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-root-helper-drift-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            marker = state / "root-close.ready"
            acknowledgement = state / "root-close.continue"
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_FAIL": "late",
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_CLOSE_MARKER": str(marker),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HELPER_CLOSE_ACK": str(
                        acknowledgement
                    ),
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            claim: Path | None = None
            claim_identity: tuple[int, int] | None = None
            try:
                wait_for_path(marker, process)
                claim = validate_owned_test_root(marker.read_text(encoding="utf-8"))
                metadata = claim.lstat()
                claim_identity = (metadata.st_dev, metadata.st_ino)
                claim.chmod(0o710)
                acknowledgement.write_text("weiter\n", encoding="utf-8")

                stdout, stderr = process.communicate(timeout=8)
                self.assertNotEqual(process.returncode, 0)
                rebound = claim.lstat()
                self.assertEqual(
                    (rebound.st_dev, rebound.st_ino),
                    claim_identity,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertEqual(stat.S_IMODE(rebound.st_mode), 0o710)
                self.assertIn(claim.name, stderr)
            finally:
                stop_owned_test_process(process)
                if claim is not None and claim.exists():
                    assert claim_identity is not None
                    remove_exact_test_artifact(claim, *claim_identity)

    def test_testcleanup_uebernimmt_keinen_ersatz_fuer_veroeffentlichten_root(
        self,
    ) -> None:
        original = Path(tempfile.mkdtemp(prefix=TEST_ROOT_PREFIX, dir="/tmp")).resolve()
        root_file = original.parent / f"{original.name}.published"
        root_file.write_text(str(original), encoding="utf-8")
        owned = read_owned_test_root(root_file, require_exists=True)
        displaced = original.with_name(
            f"{original.name}.test-displaced-{os.urandom(8).hex()}"
        )
        replacement = original
        cleanup_records: list[tuple[Path, int, int]] = []
        try:
            original.rename(displaced)
            displaced_stat = displaced.lstat()
            cleanup_records.append(
                (displaced, displaced_stat.st_dev, displaced_stat.st_ino)
            )
            replacement.mkdir(mode=0o700)
            replacement_stat = replacement.lstat()
            cleanup_records.append(
                (replacement, replacement_stat.st_dev, replacement_stat.st_ino)
            )

            with self.assertRaises(AssertionError):
                remove_owned_test_root(owned)
            self.assertEqual(
                (replacement.lstat().st_dev, replacement.lstat().st_ino),
                (replacement_stat.st_dev, replacement_stat.st_ino),
            )
        finally:
            root_file.unlink(missing_ok=True)
            for path, device, inode in cleanup_records:
                remove_exact_test_artifact(path, device, inode)

    def test_laufcleanup_loescht_nach_close_keinen_fremden_leerroot(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-run-cleanup-swap-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            marker = state / "root-cleanup-close.ready"
            acknowledgement = state / "root-cleanup-close.continue"
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_CLOSE_MARKER": str(
                        marker
                    ),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_CLOSE_ACK": str(
                        acknowledgement
                    ),
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            cleanup_records: list[tuple[Path, int, int]] = []
            try:
                root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                wait_for_path(marker, process)

                # Im alten Ablauf ist dies exakt das Fenster zwischen der letzten
                # Prüfung des öffentlichen Rootnamens und dessen `rmdir`. Nach dem
                # sicheren Claim ist der Originalname hier bereits frei.
                if root.exists():
                    displaced = root.with_name(
                        f"{root.name}.test-displaced-{os.urandom(8).hex()}"
                    )
                    root.rename(displaced)
                    displaced_stat = displaced.lstat()
                    cleanup_records.append(
                        (displaced, displaced_stat.st_dev, displaced_stat.st_ino)
                    )
                replacement = root.path
                replacement.mkdir(mode=0o700)
                replacement_stat = replacement.lstat()
                cleanup_records.append(
                    (replacement, replacement_stat.st_dev, replacement_stat.st_ino)
                )
                acknowledgement.write_text("weiter\n", encoding="utf-8")

                stdout, stderr = process.communicate(timeout=8)
                self.assertEqual(
                    process.returncode,
                    0,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertEqual(
                    (replacement.lstat().st_dev, replacement.lstat().st_ino),
                    (replacement_stat.st_dev, replacement_stat.st_ino),
                    "Der Laufcleanup löschte den fremden Ersatz am Originalnamen",
                )
            finally:
                stop_owned_test_process(process)
                for path, device, inode in cleanup_records:
                    remove_exact_test_artifact(path, device, inode)

    def test_path_mktemp_fremdroot_wird_nie_uebernommen(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-foreign-root-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            foreign = Path(tempfile.mkdtemp(prefix=TEST_ROOT_PREFIX, dir="/tmp"))
            sentinel = foreign / "bleibt"
            sentinel.write_text("fremd\n", encoding="utf-8")
            metadata = foreign.lstat()
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_MKTEMP_FOREIGN_ROOT": str(foreign),
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.run(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=15,
                check=False,
            )
            try:
                self.assertEqual(
                    process.returncode,
                    0,
                    f"stdout:\n{process.stdout}\nstderr:\n{process.stderr}",
                )
                self.assertFalse(
                    (state / "mktemp.called").exists(),
                    "Der Runner darf einen PATH-mktemp-Helfer nicht mehr aufrufen",
                )
                self.assertEqual(sentinel.read_text(encoding="utf-8"), "fremd\n")
                self.assertEqual(
                    (foreign.lstat().st_dev, foreign.lstat().st_ino),
                    (metadata.st_dev, metadata.st_ino),
                )
                owned = read_published_test_root_path(state / "root.txt")
                self.assertFalse(owned.exists(), "Der selbst erzeugte Root blieb zurück")
            finally:
                remove_exact_test_artifact(foreign, metadata.st_dev, metadata.st_ino)

    def test_root_swap_nach_descriptorbindung_loescht_den_ersatz_nie(self) -> None:
        self.run_root_identity_swap_after_binding_case()

    def test_nach_root_claim_schreiben_alle_runner_artefakte_in_den_gehaltenen_root(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-root-writer-swap-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            acknowledgement = state / "root-handle.continue"
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "root-handle-claimed",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "root-handle.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_HANDLE_ACK": str(acknowledgement),
                    "FAKE_CARGO_EXIT": "17",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            cleanup_records: list[tuple[Path, int, int]] = []
            try:
                wait_for_path(state / "root-handle.ready", process)
                root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                displaced = Path(
                    tempfile.mktemp(prefix=f"{TEST_ROOT_PREFIX}writer-bound.", dir="/tmp")
                )
                root.rename(displaced)
                displaced_stat = displaced.lstat()
                cleanup_records.append(
                    (displaced, displaced_stat.st_dev, displaced_stat.st_ino)
                )
                replacement = root.path
                replacement.mkdir(mode=0o700)
                (replacement / "bleibt").write_text("fremd\n", encoding="utf-8")
                replacement_stat = replacement.lstat()
                cleanup_records.append(
                    (replacement, replacement_stat.st_dev, replacement_stat.st_ino)
                )
                acknowledgement.write_text("weiter\n", encoding="utf-8")

                stdout, stderr = process.communicate(timeout=15)
                self.assertEqual(
                    process.returncode,
                    1,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertEqual(
                    sorted(path.name for path in replacement.iterdir()),
                    ["bleibt"],
                    "Nach dem Claim schrieb der Runner in den ausgetauschten Rootnamen",
                )
                self.assertTrue((displaced / "manifest.json").is_file())
                self.assertTrue((displaced / "sshd_config").is_file())
                self.assertTrue((displaced / "root").is_dir())
                self.assertIn("Recoverypfad", stderr)
            finally:
                stop_owned_test_process(process)
                for path, device, inode in cleanup_records:
                    remove_exact_test_artifact(path, device, inode)

    def test_mountgrenze_bleibt_als_exakter_recoverypfad_stehen(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-xdev-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "running",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_MOUNT_ID_NAME": "kunst-mount",
                    "FAKE_CARGO_EXIT": "",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            root: Path | None = None
            recovery: Path | None = None
            try:
                wait_for_path(state / "cargo.pid", process)
                root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                mount = root / "kunst-mount"
                mount.mkdir()
                (mount / "bleibt").write_text("nicht betreten\n", encoding="utf-8")
                os.kill(process.pid, signal.SIGTERM)
                stdout, stderr = process.communicate(timeout=8)
                self.assertEqual(
                    process.returncode,
                    143,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                recovery_line = next(
                    line
                    for line in stderr.splitlines()
                    if line.startswith("SFTP-E2E-Cleanup-Claim-Recoverypfad: ")
                )
                recovery = bind_owned_test_root(
                    recovery_line.removeprefix(
                        "SFTP-E2E-Cleanup-Claim-Recoverypfad: "
                    )
                )
                mount = recovery / "kunst-mount"
                self.assertEqual(
                    (mount / "bleibt").read_text(encoding="utf-8"),
                    "nicht betreten\n",
                )
                self.assertIn("Recoverypfad:", stderr)
                self.assertIn(recovery.name, stderr)
            finally:
                stop_owned_test_process(process)
                if recovery is not None and recovery.exists():
                    remove_owned_test_root(recovery)
                elif root is not None and root.exists():
                    remove_owned_test_root(root)

    def test_diagnoselogger_beschreibt_keinen_regulaeren_ersatz(self) -> None:
        self.run_diagnostic_identity_swap_case()

    def test_root_veroeffentlichung_ignoriert_leere_und_teilgeschriebene_werte(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-root-guard-") as temp:
            guard = Path(temp)
            root_file = guard / "root.txt"
            sentinel = guard / "cwd-sentinel"
            root_file.write_text("", encoding="utf-8")
            sentinel.write_text("bleibt\n", encoding="utf-8")
            published = Path(
                tempfile.mkdtemp(prefix=TEST_ROOT_PREFIX, dir="/tmp")
            ).resolve()

            def publish_in_steps() -> None:
                time.sleep(0.05)
                root_file.write_text("/tmp/doppeldecker-sftp-e2e.", encoding="utf-8")
                time.sleep(0.05)
                temporary = root_file.with_name(f"{root_file.name}.tmp")
                temporary.write_text(str(published), encoding="utf-8")
                os.replace(temporary, root_file)

            publisher = threading.Thread(target=publish_in_steps)
            publisher.start()
            old_cwd = Path.cwd()
            owned = read_owned_test_root(root_file)
            try:
                self.assertEqual(owned.path, published)
                os.chdir(guard)
                with self.assertRaises(AssertionError):
                    remove_owned_test_root(Path(""))
                self.assertEqual(sentinel.read_text(encoding="utf-8"), "bleibt\n")
            finally:
                os.chdir(old_cwd)
                publisher.join(timeout=2)
                remove_owned_test_root(owned)

    def test_testhelper_normalisiert_traversal_und_symlink_nie_zum_loeschziel(
        self,
    ) -> None:
        anchor = Path(tempfile.mkdtemp(prefix=f"{TEST_ROOT_PREFIX}anchor.", dir="/tmp"))
        target = Path(tempfile.mkdtemp(prefix=f"{TEST_ROOT_PREFIX}target.", dir="/tmp"))
        link = Path(tempfile.mktemp(prefix=f"{TEST_ROOT_PREFIX}link.", dir="/tmp"))
        link.symlink_to(target, target_is_directory=True)
        anchor_stat = anchor.lstat()
        target_stat = target.lstat()
        link_stat = link.lstat()
        try:
            with self.assertRaises(AssertionError):
                validate_owned_test_root("/tmp/doppeldecker-sftp-e2e.")
            with self.assertRaises(AssertionError):
                validate_owned_test_root(anchor / ".." / target.name)
            with self.assertRaises(AssertionError):
                validate_owned_test_root(link)
        finally:
            remove_exact_test_artifact(link, link_stat.st_dev, link_stat.st_ino)
            remove_exact_test_artifact(target, target_stat.st_dev, target_stat.st_ino)
            remove_exact_test_artifact(anchor, anchor_stat.st_dev, anchor_stat.st_ino)

    def test_finally_verwendet_nur_ungeernteten_popen_besitz(self) -> None:
        owned = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(3600)"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        stop_owned_test_process(owned)
        self.assertIsNotNone(owned.returncode)

    def test_fehlernder_stop_logger_bricht_cleanup_nicht_ab(self) -> None:
        self.run_signal_case(
            signal.SIGTERM,
            143,
            "cargo-listing",
            (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "cargo-list-supervisor.pid",
                "cargo-list.pid",
                "cargo-list-child.pid",
            ),
            broken_stop_log=True,
        )

    def test_fifo_logger_ohne_leser_blockieren_signalcleanup_nicht(self) -> None:
        for caught, expected in [
            (signal.SIGHUP, 129),
            (signal.SIGINT, 130),
            (signal.SIGTERM, 143),
        ]:
            with self.subTest(signal=caught.name):
                self.run_signal_case(
                    caught,
                    expected,
                    "ssh-agent-ready-timeout",
                    ("hold.ready",),
                    fifo_logs=True,
                )

    def test_reap_logger_fehler_registriert_cargo_vor_exit_ab(self) -> None:
        self.run_reap_log_failure_case()

    def test_fehlerhafte_agentantwort_bindet_keinen_geerbten_agenten(self) -> None:
        for mode, expected in [
            ("start-failure", 23),
            ("missing-pid", 1),
            ("malformed-pid", 1),
            ("missing-socket", 1),
        ]:
            with self.subTest(mode=mode):
                self.run_agent_failure_case(mode, expected)

    def test_wiederverwendete_cargo_identitaeten_bleiben_fremd(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-reap-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            foreign = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(3600)"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG": str(
                        state / "stop-targets"
                    ),
                    "DOPPELDECKER_SFTP_E2E_TEST_REUSED_GROUP_FILE": str(
                        state / "reused-group"
                    ),
                    "DOPPELDECKER_SFTP_E2E_TEST_REAP_LOG": str(state / "reaped"),
                    "FAKE_AGENT_MODE": "",
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CARGO_NO_CHILD": "1",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            try:
                process = run_owned_runner(environment)
                self.assertEqual(
                    process.returncode,
                    0,
                    f"stdout:\n{process.stdout}\nstderr:\n{process.stderr}",
                )
                cargo_list_pid = int((state / "cargo-list.pid").read_text())
                cargo_list_supervisor = int(
                    (state / "cargo-list-supervisor.pid").read_text()
                )
                cargo_pid = int((state / "cargo.pid").read_text())
                cargo_supervisor = int(
                    (state / "cargo-supervisor.pid").read_text()
                )
                signaled_targets = [
                    int(line)
                    for line in (state / "stop-targets").read_text().splitlines()
                ]
                self.assertNotIn(-cargo_list_pid, signaled_targets)
                self.assertNotIn(-cargo_pid, signaled_targets)
                simulated_reuse = {
                    int(line)
                    for line in (state / "reused-group").read_text().splitlines()
                }
                self.assertEqual(simulated_reuse, {cargo_list_pid, cargo_pid})
                reaped_supervisors = {
                    int(line) for line in (state / "reaped").read_text().splitlines()
                }
                self.assertEqual(
                    reaped_supervisors, {cargo_list_supervisor, cargo_supervisor}
                )
                self.assertEqual(
                    sum(target == -cargo_list_supervisor for target in signaled_targets), 1
                )
                self.assertEqual(
                    sum(target == -cargo_supervisor for target in signaled_targets), 1
                )
                self.assertIn(-cargo_list_supervisor, signaled_targets)
                self.assertIn(-cargo_supervisor, signaled_targets)
                self.assertFalse(process_is_running(cargo_list_supervisor))
                self.assertFalse(process_is_running(cargo_supervisor))
                self.assertTrue(
                    process_is_running(foreign.pid),
                    "Eine wiederverwendete fremde Prozessgruppe muss unangetastet bleiben",
                )
                test_root = read_published_test_root_path(state / "root.txt")
                self.assertFalse(test_root.exists())
            finally:
                if foreign.poll() is None:
                    foreign.terminate()
                    foreign.wait(timeout=5)

    def test_cargo_fehlerstatus_bleibt_trotz_gemeinsamem_cleanup_erhalten(self) -> None:
        self.run_signal_case(
            None,
            17,
            "normal-exit",
            (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "cargo-supervisor.pid",
                "cargo.pid",
                "cargo-child.pid",
            ),
        )

    def test_signale_an_alle_startfenster_raeumen_prozesse_und_root(self) -> None:
        phases = {
            "mktemp-assignment": (),
            "agent-before-owned": ("agent.pid",),
            "ssh-agent-ready-before-ack": ("hold.ready",),
            "sshd-before-setsid": ("agent.pid", "hold.ready"),
            "sshd-ready-before-ack": ("agent.pid", "hold.ready"),
            "cargo-before-setsid": (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "hold.ready",
            ),
            "cargo-listing-ready-before-ack": (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "hold.ready",
            ),
            "cargo-listing": (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "cargo-list-supervisor.pid",
                "cargo-list.pid",
                "cargo-list-child.pid",
            ),
            "cargo-ready-before-ack": (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "hold.ready",
            ),
            "running": (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "cargo-supervisor.pid",
                "cargo.pid",
                "cargo-child.pid",
            ),
        }
        for phase, pid_files in phases.items():
            for caught, expected in [
                (signal.SIGHUP, 129),
                (signal.SIGINT, 130),
                (signal.SIGTERM, 143),
            ]:
                with self.subTest(phase=phase, signal=caught.name):
                    self.run_signal_case(caught, expected, phase, pid_files)

    def test_zweites_signal_unterbricht_laufenden_cleanup_nicht(self) -> None:
        self.run_signal_case(
            signal.SIGHUP,
            129,
            "running",
            (
                "agent.pid",
                "sshd.pid",
                "sshd-child.pid",
                "cargo-supervisor.pid",
                "cargo.pid",
                "cargo-child.pid",
            ),
            second_signal=signal.SIGTERM,
        )

    def test_signale_an_die_vordergrundgruppe_im_mktemp_fenster_raeumen_auf(
        self,
    ) -> None:
        for caught, expected in [
            (signal.SIGHUP, 129),
            (signal.SIGINT, 130),
            (signal.SIGTERM, 143),
        ]:
            with self.subTest(signal=caught.name):
                self.run_signal_case(
                    caught,
                    expected,
                    "mktemp-assignment",
                    (),
                    signal_group=True,
                )

    def test_vor_ready_gestorbene_starter_werden_nicht_erneut_signalisiert(self) -> None:
        for label in ["ssh-agent", "sshd", "cargo-listing"]:
            with self.subTest(label=label):
                self.run_starter_failure_case(label)

    def test_ready_timeout_beendet_lebenden_starter_vor_deregistrierung(self) -> None:
        self.run_starter_timeout_case("ssh-agent")

    def test_signale_nach_cargo_reap_treffen_die_alte_gruppe_nie(self) -> None:
        for caught, expected in [
            (signal.SIGHUP, 129),
            (signal.SIGINT, 130),
            (signal.SIGTERM, 143),
        ]:
            with self.subTest(signal=caught.name):
                self.run_signal_case(
                    caught,
                    expected,
                    "cargo-listing-after-reap",
                    ("agent.pid", "sshd.pid", "sshd-child.pid"),
                )

    def run_signal_case(
        self,
        caught: signal.Signals | None,
        expected: int,
        phase: str,
        pid_files: tuple[str, ...],
        second_signal: signal.Signals | None = None,
        signal_group: bool = False,
        broken_stop_log: bool = False,
        fifo_logs: bool = False,
    ) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-signal-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            stop_log = state / "stop-targets"
            reap_log = state / "reaped"
            if broken_stop_log:
                stop_log.mkdir()
            if fifo_logs:
                os.mkfifo(stop_log)
                os.mkfifo(reap_log)

            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    # Die Signaltests prüfen immer denselben E2E-Zweig. Eine vom
                    # aufrufenden Terminal geerbte Umschaltung darf Testname und
                    # Cargo-Attrappe nicht unbemerkt verändern.
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": phase,
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG": str(stop_log),
                    "DOPPELDECKER_SFTP_E2E_TEST_REAP_LOG": (
                        str(reap_log) if fifo_logs else ""
                    ),
                    "FAKE_CARGO_EXIT": "17" if caught is None else "",
                    "FAKE_CLEANUP_MARKER": (
                        str(state / "cleanup.started") if second_signal is not None else ""
                    ),
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            foreign = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(3600)"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            test_root: Path | None = None
            pids: list[int] = []
            try:
                checkpoint = state / (
                    "cargo.pid" if phase in {"running", "normal-exit"} else "hold.ready"
                )
                wait_for_path(checkpoint, process)
                test_root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                self.assertTrue(test_root.exists())
                for name in pid_files:
                    wait_for_path(state / name, process)
                    pid = int((state / name).read_text(encoding="utf-8"))
                    pids.append(pid)

                if caught is not None:
                    # Bewusst nie die Test-Prozessgruppe signalisieren: Selbst
                    # während einer noch nicht bestätigten Registrierung muss
                    # ausschließlich das Skript den richtigen Besitz erkennen.
                    if signal_group:
                        os.killpg(process.pid, caught)
                    else:
                        os.kill(process.pid, caught)
                    if second_signal is not None:
                        wait_for_path(state / "cleanup.started", process)
                        os.kill(process.pid, second_signal)
                stdout, stderr = process.communicate(timeout=2 if fifo_logs else 8)
                self.assertEqual(
                    process.returncode,
                    expected,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertFalse(test_root.exists(), "gemeinsamer EXIT-Cleanup lief nicht")

                deadline = time.monotonic() + 5
                while time.monotonic() < deadline and any(
                    process_is_running(pid) for pid in pids
                ):
                    time.sleep(0.02)
                self.assertEqual(
                    [pid for pid in pids if process_is_running(pid)],
                    [],
                    "SFTP-E2E hinterließ einen Unterprozess",
                )
                self.assertTrue(
                    process_is_running(foreign.pid),
                    "Der Cleanup darf keinen fremden Prozess treffen",
                )
                not_started = {
                    "ssh-agent-ready-before-ack": "agent.pid",
                    "sshd-ready-before-ack": "sshd.pid",
                    "cargo-listing-ready-before-ack": "cargo-list.pid",
                    "cargo-ready-before-ack": "cargo.pid",
                }.get(phase)
                if not_started is not None:
                    self.assertFalse(
                        (state / not_started).exists(),
                        "Vor dem Eltern-ACK wurde bereits ein Dienstkind gestartet",
                    )
                if phase == "cargo-listing-after-reap":
                    reaped_pid = int((state / "hold.ready").read_text())
                    stop_targets = [
                        int(line)
                        for line in (state / "stop-targets").read_text().splitlines()
                    ]
                    self.assertEqual(
                        stop_targets.count(-reaped_pid),
                        1,
                        "Die geerntete und als fremd wiederverwendet simulierte PGID wurde erneut signalisiert",
                    )
            finally:
                if fifo_logs and process.poll() is None:
                    # Beim roten Gegenbeleg hängt nur die direkt als Popen gebundene
                    # Skript-PID am FIFO. Das noch lebende Starterkind beendet sich
                    # durch den Testhaltepunkt selbst; keine bloße PID wird hier
                    # signalisiert.
                    process.kill()
                    process.communicate(timeout=5)
                else:
                    stop_owned_test_process(process)
                if fifo_logs:
                    deadline = time.monotonic() + 7
                    while time.monotonic() < deadline and any(
                        process_is_running(pid) for pid in pids
                    ):
                        time.sleep(0.02)
                if foreign.poll() is None:
                    foreign.terminate()
                    foreign.wait(timeout=5)
                # Auch ein roter Regressionstest darf keinen künstlichen Root
                # hinterlassen. Die Assertion oben prüft trotzdem zuerst das Skript.
                if test_root is not None and test_root.exists():
                    remove_owned_test_root(test_root)

    def run_reap_log_failure_case(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-reap-log-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            (state / "reaped").mkdir()
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG": str(
                        state / "stop-targets"
                    ),
                    "DOPPELDECKER_SFTP_E2E_TEST_REAP_LOG": str(state / "reaped"),
                    "FAKE_AGENT_MODE": "",
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CARGO_NO_CHILD": "1",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.run(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=15,
                check=False,
            )
            self.assertEqual(
                process.returncode,
                1,
                f"stdout:\n{process.stdout}\nstderr:\n{process.stderr}",
            )
            supervisor = int(
                (state / "cargo-list-supervisor.pid").read_text(encoding="utf-8")
            )
            stop_targets = [
                int(line)
                for line in (state / "stop-targets").read_text(encoding="utf-8").splitlines()
            ]
            self.assertEqual(
                stop_targets.count(-supervisor),
                1,
                "Die bereits geerntete Cargo-Gruppenzahl wurde erneut signalisiert",
            )
            root = read_published_test_root_path(state / "root.txt")
            self.assertFalse(root.exists())
            self.assertFalse(process_is_running(supervisor))

    def run_starter_failure_case(self, label: str) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-starter-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": f"{label}-exit-before-ready",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG": str(state / "stop-targets"),
                    "FAKE_AGENT_MODE": "",
                    "FAKE_CARGO_EXIT": "",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = run_owned_runner(environment)
            self.assertEqual(
                process.returncode,
                23,
                f"stdout:\n{process.stdout}\nstderr:\n{process.stderr}",
            )
            dead_pid = int((state / "hold.ready").read_text())
            stop_log = state / "stop-targets"
            targets = (
                [int(line) for line in stop_log.read_text().splitlines()]
                if stop_log.exists()
                else []
            )
            self.assertNotIn(dead_pid, targets)
            self.assertNotIn(-dead_pid, targets)
            root = read_published_test_root_path(state / "root.txt")
            self.assertFalse(root.exists())

    def run_starter_timeout_case(self, label: str) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-starter-timeout-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": f"{label}-ready-timeout",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG": str(state / "stop-targets"),
                    "FAKE_AGENT_MODE": "",
                    "FAKE_CARGO_EXIT": "",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            starter_pid: int | None = None
            root: Path | None = None
            try:
                wait_for_path(state / "hold.ready", process)
                starter_pid = int((state / "hold.ready").read_text())
                root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                stdout, stderr = process.communicate(timeout=10)
                self.assertEqual(
                    process.returncode,
                    1,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertFalse(
                    process_is_running(starter_pid),
                    "Der noch lebende Starter wurde vor dem Löschen seiner Registrierung nicht geerntet",
                )
                self.assertFalse(
                    (state / "agent.pid").exists(),
                    "Ohne Ready/ACK darf kein Agentkind gestartet werden",
                )
                self.assertFalse(root.exists(), "Root blieb nach dem Ready-Timeout zurück")
                stop_targets = [
                    int(line)
                    for line in (state / "stop-targets").read_text().splitlines()
                ]
                self.assertEqual(
                    stop_targets.count(starter_pid),
                    1,
                    "Der unbestätigte Starter muss genau über seine positive PID beendet werden",
                )
                self.assertNotIn(
                    -starter_pid,
                    stop_targets,
                    "Vor Ready darf keine Prozessgruppe vermutet werden",
                )
            finally:
                stop_owned_test_process(process)
                # Der rote Gegenbeleg lässt den Starter absichtlich noch kurz
                # leben. Er beendet sich selbst; eine inzwischen wiederverwendbare
                # bloße PID darf der Test-Cleanup nicht signalisieren.
                if starter_pid is not None:
                    deadline = time.monotonic() + 5
                    while process_is_running(starter_pid) and time.monotonic() < deadline:
                        time.sleep(0.02)
                if root is not None and root.exists():
                    remove_owned_test_root(root)

    def run_agent_failure_case(self, mode: str, expected: int) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-agent-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            inherited = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(3600)"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "normal-exit",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "FAKE_AGENT_MODE": mode,
                    "FAKE_CARGO_EXIT": "0",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                    "SSH_AGENT_PID": str(inherited.pid),
                    "SSH_AUTH_SOCK": str(state / "inherited-agent.sock"),
                }
            )
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            test_root: Path | None = None
            new_agent_pids: list[int] = []
            try:
                test_root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                if mode != "start-failure":
                    wait_for_path(state / "agent.pid", process)
                    wait_for_path(state / "agent-child.pid", process)
                    new_agent_pids.extend(
                        int((state / name).read_text(encoding="utf-8"))
                        for name in ("agent.pid", "agent-child.pid")
                    )
                stdout, stderr = process.communicate(timeout=8)
                self.assertEqual(
                    process.returncode,
                    expected,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertTrue(
                    process_is_running(inherited.pid),
                    "Ein geerbter Agent darf nie als neu gestarteter Besitz gelten",
                )
                self.assertEqual(
                    [pid for pid in new_agent_pids if process_is_running(pid)],
                    [],
                    f"Neu gestarteter Agent oder sein Kind blieb nach {mode} aktiv",
                )
                self.assertFalse(
                    (state / "agent-kill.called").exists(),
                    "Cleanup darf ssh-agent -k nicht gegen geerbte Variablen aufrufen",
                )
                self.assertFalse(
                    (state / "inherited-agent-visible").exists(),
                    "Der neue Agent darf geerbte Agentvariablen nicht sehen",
                )
                self.assertFalse(test_root.exists(), "Root blieb nach Agentfehler zurück")
            finally:
                stop_owned_test_process(process)
                if inherited.poll() is None:
                    inherited.terminate()
                    inherited.wait(timeout=5)
                if test_root is not None and test_root.exists():
                    remove_owned_test_root(test_root)

    def run_root_identity_swap_after_binding_case(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-root-bound-swap-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            marker = state / "root-cleanup-bound"
            acknowledgement = state / "root-cleanup-continue"
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "running",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_MARKER": str(marker),
                    "DOPPELDECKER_SFTP_E2E_TEST_ROOT_CLEANUP_ACK": str(acknowledgement),
                    "FAKE_CARGO_EXIT": "",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            cleanup_records: list[tuple[Path, int, int]] = []
            try:
                wait_for_path(state / "cargo.pid", process)
                root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                os.kill(process.pid, signal.SIGTERM)
                wait_for_path(marker, process)

                displaced = None
                if root.exists():
                    displaced = Path(
                        tempfile.mktemp(prefix=f"{TEST_ROOT_PREFIX}bound.", dir="/tmp")
                    )
                    root.rename(displaced)
                    displaced_stat = displaced.lstat()
                    cleanup_records.append(
                        (displaced, displaced_stat.st_dev, displaced_stat.st_ino)
                    )
                replacement = root.path
                replacement.mkdir(mode=0o700)
                (replacement / "bleibt").write_text("fremd\n", encoding="utf-8")
                replacement_stat = replacement.lstat()
                cleanup_records.append(
                    (replacement, replacement_stat.st_dev, replacement_stat.st_ino)
                )
                acknowledgement.write_text("weiter\n", encoding="utf-8")

                stdout, stderr = process.communicate(timeout=8)
                self.assertEqual(
                    process.returncode,
                    143,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertEqual(
                    (replacement.lstat().st_dev, replacement.lstat().st_ino),
                    (replacement_stat.st_dev, replacement_stat.st_ino),
                )
                self.assertEqual(
                    (replacement / "bleibt").read_text(encoding="utf-8"),
                    "fremd\n",
                )
            finally:
                stop_owned_test_process(process)
                for path, device, inode in cleanup_records:
                    remove_exact_test_artifact(path, device, inode)

    def run_diagnostic_identity_swap_case(self) -> None:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-sftp-log-swap-") as temp:
            test_dir = Path(temp)
            fake_bin = test_dir / "bin"
            state = test_dir / "state"
            fake_bin.mkdir()
            state.mkdir()
            self.install_fake_commands(fake_bin)
            stop_log = state / "stop-targets"
            old_log = state / "stop-targets-old"
            replacement = state / "stop-targets-replacement"
            stop_log.write_text("alt\n", encoding="utf-8")
            replacement.write_text("fremd\n", encoding="utf-8")
            marker = state / "diagnostic-before-open"
            acknowledgement = state / "diagnostic-continue"
            environment = os.environ.copy()
            environment.update(
                {
                    "DOPPELDECKER_SSHD": str(fake_bin / "sshd"),
                    "DOPPELDECKER_SFTP_DISABLE_FSYNC": "0",
                    "DOPPELDECKER_SFTP_E2E_TEST_HOLD": "running",
                    "DOPPELDECKER_SFTP_E2E_TEST_MARKER": str(state / "hold.ready"),
                    "DOPPELDECKER_SFTP_E2E_TEST_STOP_LOG": str(stop_log),
                    "DOPPELDECKER_SFTP_E2E_TEST_DIAGNOSTIC_MARKER": str(marker),
                    "DOPPELDECKER_SFTP_E2E_TEST_DIAGNOSTIC_ACK": str(acknowledgement),
                    "FAKE_CARGO_EXIT": "",
                    "FAKE_CLEANUP_MARKER": "",
                    "FAKE_SFTP_STATE": str(state),
                    "HOME": str(test_dir / "home"),
                    "PATH": f"{fake_bin}{os.pathsep}{environment['PATH']}",
                }
            )
            environment.pop("SSH_AGENT_PID", None)
            environment.pop("SSH_AUTH_SOCK", None)
            (test_dir / "home").mkdir()
            process = subprocess.Popen(
                [str(SCRIPT)],
                cwd=ROOT,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                start_new_session=True,
            )
            root: Path | None = None
            try:
                wait_for_path(state / "cargo.pid", process)
                root = read_owned_test_root(
                    state / "root.txt", process, require_exists=True
                )
                os.kill(process.pid, signal.SIGTERM)
                wait_for_path(marker, process)
                stop_log.rename(old_log)
                replacement.rename(stop_log)
                acknowledgement.write_text("weiter\n", encoding="utf-8")
                stdout, stderr = process.communicate(timeout=8)
                self.assertEqual(
                    process.returncode,
                    143,
                    f"stdout:\n{stdout}\nstderr:\n{stderr}",
                )
                self.assertEqual(stop_log.read_text(encoding="utf-8"), "fremd\n")
                self.assertEqual(old_log.read_text(encoding="utf-8"), "alt\n")
                self.assertFalse(root.exists())
            finally:
                stop_owned_test_process(process)
                if root is not None and root.exists():
                    remove_owned_test_root(root)

    def install_fake_commands(self, fake_bin: Path) -> None:
        # Nur die unveränderlichen Programme werden geteilt. Zustand, Socket,
        # Port, Schlüssel und Root liegen weiterhin im eigenen Testfall.
        for command in self._fixture_bin.iterdir():
            if command.name != "runner":
                (fake_bin / command.name).symlink_to(command)

    @staticmethod
    def write_fake_commands(fake_bin: Path) -> None:
        python = sys.executable
        write_executable(
            fake_bin / "mktemp",
            f"""
            #!{python}
            import os
            import json
            import pathlib
            import tempfile
            import time

            state = pathlib.Path(os.environ["FAKE_SFTP_STATE"])
            foreign_root = os.environ.get("FAKE_MKTEMP_FOREIGN_ROOT")
            if foreign_root:
                (state / "mktemp.called").write_text("1\\n", encoding="utf-8")
                print(foreign_root)
                raise SystemExit(0)
            mode = os.environ.get("FAKE_MKTEMP_MODE", "")
            artifacts = []
            if mode == "traversal":
                anchor = pathlib.Path(tempfile.mkdtemp(
                    prefix="doppeldecker-sftp-e2e.anchor.", dir="/tmp"
                ))
                foreign = pathlib.Path(tempfile.mkdtemp(
                    prefix="doppeldecker-sftp-e2e.foreign.", dir="/tmp"
                ))
                (anchor / "bleibt").write_text("eigen\\n", encoding="utf-8")
                (foreign / "bleibt").write_text("fremd\\n", encoding="utf-8")
                root = anchor / ".." / foreign.name
                artifacts = [anchor, foreign]
            elif mode == "symlink":
                target = pathlib.Path(tempfile.mkdtemp(
                    prefix="doppeldecker-sftp-e2e.target.", dir="/tmp"
                ))
                (target / "bleibt").write_text("fremd\\n", encoding="utf-8")
                link = pathlib.Path(tempfile.mktemp(
                    prefix="doppeldecker-sftp-e2e.link.", dir="/tmp"
                ))
                link.symlink_to(target, target_is_directory=True)
                root = link
                artifacts = [target, link]
            elif mode == "empty-suffix":
                root = pathlib.Path("/tmp/doppeldecker-sftp-e2e.")
                try:
                    root.mkdir(mode=0o700)
                except FileExistsError:
                    raise SystemExit(91)
                (root / "bleibt").write_text("fremd\\n", encoding="utf-8")
                artifacts = [root]
            else:
                root = pathlib.Path(tempfile.mkdtemp(
                    prefix="doppeldecker-sftp-e2e.", dir="/tmp"
                ))
            if artifacts:
                records = []
                for artifact in artifacts:
                    metadata = artifact.lstat()
                    records.append({{
                        "path": str(artifact),
                        "device": metadata.st_dev,
                        "inode": metadata.st_ino,
                    }})
                artifact_file = state / "mktemp-artifacts.json"
                artifact_temporary = artifact_file.with_name(
                    f"{{artifact_file.name}}.{{os.getpid()}}.tmp"
                )
                artifact_temporary.write_text(json.dumps(records), encoding="utf-8")
                os.replace(artifact_temporary, artifact_file)
            root_file = state / "root.txt"
            temporary = root_file.with_name(f"{{root_file.name}}.{{os.getpid()}}.tmp")
            temporary.write_text(str(root), encoding="utf-8")
            os.replace(temporary, root_file)
            if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == "mktemp-assignment":
                marker = pathlib.Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
                temporary = marker.with_name(marker.name + ".tmp")
                temporary.write_text(f"{{os.getpid()}}\\n", encoding="utf-8")
                os.replace(temporary, marker)
                time.sleep(0.3)
            print(root)
            """,
        )

        write_executable(
            fake_bin / "ssh-keygen",
            f"""
            #!{python}
            import pathlib
            import sys

            args = sys.argv[1:]
            if any(arg.startswith("-l") for arg in args):
                print("256 SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA test")
                raise SystemExit(0)
            target = pathlib.Path(args[args.index("-f") + 1])
            target.write_text("private-test-key\\n", encoding="utf-8")
            target.with_name(target.name + ".pub").write_text(
                "ssh-ed25519 AAAA test\\n", encoding="utf-8"
            )
            """,
        )
        write_executable(
            fake_bin / "ssh-add",
            f"""
            #!{python}
            raise SystemExit(0)
            """,
        )
        write_executable(
            fake_bin / "ssh-agent",
            f"""
            #!{python}
            import os
            import pathlib
            import signal
            import socket
            import subprocess
            import sys
            import time

            state = pathlib.Path(os.environ["FAKE_SFTP_STATE"])
            if "-D" in sys.argv[1:]:
                if os.environ.get("SSH_AGENT_PID") or os.environ.get("SSH_AUTH_SOCK"):
                    (state / "inherited-agent-visible").write_text("1\\n", encoding="utf-8")
                mode = os.environ.get("FAKE_AGENT_MODE", "")
                if mode == "start-failure":
                    raise SystemExit(23)
                (state / "agent.pid").write_text(str(os.getpid()), encoding="utf-8")
                if mode in {{"missing-pid", "malformed-pid", "missing-socket"}}:
                    child = subprocess.Popen(
                        [sys.executable, "-c", "import time; time.sleep(3600)"],
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    (state / "agent-child.pid").write_text(
                        str(child.pid), encoding="utf-8"
                    )
                    if mode == "missing-pid":
                        print("SSH_AUTH_SOCK=/unbrauchbar; export SSH_AUTH_SOCK;", flush=True)
                    elif mode == "malformed-pid":
                        print("SSH_AGENT_PID=keine-pid; export SSH_AGENT_PID;", flush=True)
                    else:
                        print(f"SSH_AGENT_PID={{os.getpid()}}; export SSH_AGENT_PID;", flush=True)
                    while True:
                        time.sleep(1)

                if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == "agent-before-owned":
                    marker = pathlib.Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
                    temporary = marker.with_name(marker.name + ".tmp")
                    temporary.write_text(f"{{os.getpid()}}\\n", encoding="utf-8")
                    os.replace(temporary, marker)
                    time.sleep(0.3)
                socket_path = pathlib.Path(sys.argv[sys.argv.index("-a") + 1])
                agent_socket = socket.socket(socket.AF_UNIX)
                agent_socket.bind(str(socket_path))
                agent_socket.listen()
                while True:
                    time.sleep(1)
            if "-s" in sys.argv[1:]:
                if os.environ.get("SSH_AGENT_PID") or os.environ.get("SSH_AUTH_SOCK"):
                    (state / "inherited-agent-visible").write_text("1\\n", encoding="utf-8")
                mode = os.environ.get("FAKE_AGENT_MODE", "")
                if mode == "start-failure":
                    raise SystemExit(23)
                if mode in {{"missing-pid", "malformed-pid", "missing-socket"}}:
                    # Der echte `ssh-agent -s` kann schon daemonisieren, bevor seine
                    # Ausgabe unvollständig ankommt. Dieser Unterprozess macht das
                    # frühere Besitzleck unabhängig vom Ausgabetext beobachtbar.
                    child = subprocess.Popen(
                        [sys.executable, "-c", "import time; time.sleep(3600)"],
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    (state / "agent.pid").write_text(
                        str(child.pid), encoding="utf-8"
                    )
                if mode == "missing-pid":
                    print(f"SSH_AUTH_SOCK={{state / 'new-agent.sock'}}; export SSH_AUTH_SOCK;")
                    raise SystemExit(0)
                if mode == "malformed-pid":
                    print(f"SSH_AUTH_SOCK={{state / 'new-agent.sock'}}; export SSH_AUTH_SOCK;")
                    print("SSH_AGENT_PID=keine-pid; export SSH_AGENT_PID;")
                    raise SystemExit(0)
                if mode == "missing-socket":
                    print(f"SSH_AGENT_PID={{child.pid}}; export SSH_AGENT_PID;")
                    raise SystemExit(0)
                child = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(3600)"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                agent_socket = socket.socket(socket.AF_UNIX)
                agent_socket.bind(str(state / "agent.sock"))
                agent_socket.close()
                (state / "agent.pid").write_text(str(child.pid), encoding="utf-8")
                if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == "agent-before-owned":
                    marker = pathlib.Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
                    temporary = marker.with_name(marker.name + ".tmp")
                    temporary.write_text(f"{{child.pid}}\\n", encoding="utf-8")
                    os.replace(temporary, marker)
                    time.sleep(0.3)
                print(f"SSH_AUTH_SOCK={{state / 'agent.sock'}}; export SSH_AUTH_SOCK;")
                print(f"SSH_AGENT_PID={{child.pid}}; export SSH_AGENT_PID;")
                raise SystemExit(0)
            if "-k" in sys.argv[1:]:
                (state / "agent-kill.called").write_text("1\\n", encoding="utf-8")
                pid_text = os.environ.get("SSH_AGENT_PID", "")
                if not pid_text.isdecimal():
                    raise SystemExit(2)
                pid = int(pid_text)
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                raise SystemExit(0)
            raise SystemExit(2)
            """,
        )
        write_executable(
            fake_bin / "sshd",
            f"""
            #!{python}
            import os
            import pathlib
            import signal
            import socket
            import subprocess
            import sys

            args = sys.argv[1:]
            config = pathlib.Path(args[args.index("-f") + 1])
            settings = {{}}
            for line in config.read_text(encoding="utf-8").splitlines():
                key, _, value = line.partition(" ")
                settings[key] = value
            state = pathlib.Path(os.environ["FAKE_SFTP_STATE"])
            (state / "sshd.pid").write_text(str(os.getpid()), encoding="utf-8")
            child = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(3600)"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            (state / "sshd-child.pid").write_text(str(child.pid), encoding="utf-8")
            listener = socket.socket()
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", int(settings["Port"])))
            listener.listen()
            while True:
                connection, _ = listener.accept()
                connection.close()
            """,
        )
        write_executable(
            fake_bin / "cargo",
            f"""
            #!{python}
            import os
            import pathlib
            import signal
            import subprocess
            import sys
            import time

            if "--list" in sys.argv:
                state = pathlib.Path(os.environ["FAKE_SFTP_STATE"])
                (state / "cargo-list-supervisor.pid").write_text(
                    str(os.getppid()), encoding="utf-8"
                )
                (state / "cargo-list.pid").write_text(
                    str(os.getpid()), encoding="utf-8"
                )
                if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_HOLD") == "cargo-listing":
                    child = subprocess.Popen(
                        [sys.executable, "-c", "import time; time.sleep(3600)"],
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    (state / "cargo-list-child.pid").write_text(
                        str(child.pid), encoding="utf-8"
                    )
                    marker = pathlib.Path(os.environ["DOPPELDECKER_SFTP_E2E_TEST_MARKER"])
                    temporary = marker.with_name(marker.name + ".tmp")
                    temporary.write_text(f"{{os.getpid()}}\\n", encoding="utf-8")
                    os.replace(temporary, marker)
                    while True:
                        time.sleep(1)
                print("echter_sftp_root_bindet_identitaet_probe_reconnect_und_streams: test")
                if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_REUSED_GROUP_FILE"):
                    with pathlib.Path(
                        os.environ["DOPPELDECKER_SFTP_E2E_TEST_REUSED_GROUP_FILE"]
                    ).open("a", encoding="utf-8") as reused:
                        reused.write(f"{{os.getpid()}}\\n")
                raise SystemExit(0)
            state = pathlib.Path(os.environ["FAKE_SFTP_STATE"])
            (state / "cargo-supervisor.pid").write_text(
                str(os.getppid()), encoding="utf-8"
            )
            if os.environ.get("FAKE_CLEANUP_MARKER"):
                def mark_cleanup(_signum, _frame):
                    pathlib.Path(os.environ["FAKE_CLEANUP_MARKER"]).write_text(
                        "1\\n", encoding="utf-8"
                    )

                signal.signal(signal.SIGTERM, mark_cleanup)
            # Die Shell soll `cargo_pid` gesetzt haben und in `wait` stehen, bevor
            # der Test sie gezielt signalisiert.
            time.sleep(0.1)
            (state / "cargo.pid").write_text(str(os.getpid()), encoding="utf-8")
            child_code = "import time; time.sleep(3600)"
            if os.environ.get("FAKE_CLEANUP_MARKER"):
                child_code = (
                    "import signal,time; "
                    "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                    "time.sleep(3600)"
                )
            if not os.environ.get("FAKE_CARGO_NO_CHILD"):
                child = subprocess.Popen(
                    [sys.executable, "-c", child_code],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                (state / "cargo-child.pid").write_text(
                    str(child.pid), encoding="utf-8"
                )
            if os.environ.get("FAKE_CARGO_EXIT"):
                # Der Gruppenleiter beendet sich zuerst. Der EXIT-Cleanup muss den
                # zurückbleibenden Cargo-Nachkommen dennoch über die bestätigte PGID
                # erreichen und zugleich den unveränderten Fehlerstatus liefern.
                time.sleep(0.2)
                if os.environ.get("DOPPELDECKER_SFTP_E2E_TEST_REUSED_GROUP_FILE"):
                    with pathlib.Path(
                        os.environ["DOPPELDECKER_SFTP_E2E_TEST_REUSED_GROUP_FILE"]
                    ).open("a", encoding="utf-8") as reused:
                        reused.write(f"{{os.getpid()}}\\n")
                raise SystemExit(int(os.environ["FAKE_CARGO_EXIT"]))
            while True:
                time.sleep(1)
            """,
        )

        write_executable(fake_bin / "runner", f"""
            #!{python}
            import os
            import signal
            import time
            from pathlib import Path

            def cleanup(*_args):
                (Path(os.environ["FAKE_SFTP_STATE"]) / "cleanup").write_text("TERM")
                raise SystemExit(0)

            signal.signal(signal.SIGTERM, cleanup)
            time.sleep(30)
        """)
        for command in fake_bin.iterdir():
            first, body = command.read_text(encoding="utf-8").split("\n", 1)
            command.write_text(
                first + '\nimport sys\n'
                'if sys.argv[1:] == ["--fixture-ready"]: raise SystemExit(0)\n'
                + body,
                encoding="utf-8",
            )


if __name__ == "__main__":
    unittest.main()
