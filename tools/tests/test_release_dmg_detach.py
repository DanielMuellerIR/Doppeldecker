"""Hält fest, dass ein selbst angehängtes DMG-Device wirklich wieder verschwindet.

Beide Releasewerkzeuge hängen ein Disk-Image an. Wird es danach nicht sicher wieder
ausgehängt, entsteht kein sichtbarer Fehler, sondern ein stiller Schaden:

* ``tools/build-release.py`` komprimiert das noch gemountete Image mit
  ``hdiutil convert``. Die Schreibpuffer des Mounts sind dann nicht garantiert auf der
  Platte — das ausgelieferte DMG kann einen unvollständigen Stand einfrieren.
* ``tools/verify-macos-release.sh`` meldete „macOS-Bundle grün", während der EXIT-Trap
  den Fehler des ``hdiutil detach`` mit ``|| true`` verwarf.

Die Tests laufen ausschließlich mit erfundenen Antworten und einer ausgetauschten
``subprocess.run``: Es wird nichts gebaut, nichts angehängt und nichts signiert.
"""

from __future__ import annotations

from pathlib import Path
from contextlib import ExitStack
import json
import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from module_loader import load_tool_module


ROOT = Path(__file__).resolve().parents[2]


def load_build_release():
    """Lädt ``tools/build-release.py`` als Modul (der Bindestrich verbietet `import`)."""
    return load_tool_module("doppeldecker_build_release", "build-release.py")


def attach_plist(mount: str, device: str) -> str:
    """Baut eine ``hdiutil attach -plist``-Antwort mit genau einem Mountpoint."""
    document = {
        "system-entities": [
            {"dev-entry": f"{device}s1"},
            {"dev-entry": device, "mount-point": mount},
        ]
    }
    return plistlib.dumps(document).decode()


class DetachOwnDeviceTests(unittest.TestCase):
    """`hdiutil convert` darf erst nach einem BELEGTEN Aushängen starten."""

    def setUp(self) -> None:
        self.module = load_build_release()
        self.calls: list[tuple[str, ...]] = []

    def fake_run(self, returncodes: list[int]):
        """Antwortet der Reihe nach mit den vorgegebenen Exit-Codes."""

        def run(command, *args, **kwargs):
            self.calls.append(tuple(command))
            return mock.Mock(returncode=returncodes.pop(0), stderr=b"Resource busy")

        return run

    def test_regulaeres_aushaengen_genuegt(self) -> None:
        with mock.patch.object(self.module.subprocess, "run", self.fake_run([0])):
            self.module.detach_own_device("/dev/disk9")
        self.assertEqual(self.calls, [("hdiutil", "detach", "/dev/disk9")])

    def test_erzwungener_zweiter_versuch_wird_akzeptiert(self) -> None:
        with mock.patch.object(self.module.subprocess, "run", self.fake_run([1, 0])), \
                mock.patch.object(self.module.time, "sleep"):
            self.module.detach_own_device("/dev/disk9")
        self.assertEqual(self.calls[-1], ("hdiutil", "detach", "/dev/disk9", "-force"))

    def test_gescheitertes_aushaengen_wirft_und_stoppt_den_release(self) -> None:
        # Regression: Der Rückgabewert des `-force`-Laufs wurde nicht geprüft; der
        # Build lief anschließend in `hdiutil convert` weiter.
        with mock.patch.object(self.module.subprocess, "run", self.fake_run([1, 1])), \
                mock.patch.object(self.module.time, "sleep"):
            with self.assertRaises(SystemExit) as raised:
                self.module.detach_own_device("/dev/disk9")
        self.assertIn("/dev/disk9", str(raised.exception))


class AttachedDeviceTests(unittest.TestCase):
    """Nur das eigene Device darf gebunden werden — und der Fehlerpfad räumt auf."""

    def setUp(self) -> None:
        self.module = load_build_release()

    def test_findet_das_device_des_eigenen_mountpoints(self) -> None:
        plist = attach_plist("/Volumes/Doppeldecker", "/dev/disk9")
        self.assertEqual(
            self.module.attached_device(plist, Path("/Volumes/Doppeldecker")),
            "/dev/disk9",
        )

    def test_fremder_mountpoint_liefert_kein_device(self) -> None:
        plist = attach_plist("/Volumes/Fremd", "/dev/disk9")
        with self.assertRaises(SystemExit):
            self.module.attached_device(plist, Path("/Volumes/Doppeldecker"))

    def test_notaushaengen_trifft_nur_gemeldete_devices(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(command, *args, **kwargs):
            calls.append(tuple(command))
            return mock.Mock(returncode=0, stderr=b"")

        plist = attach_plist("/Volumes/Doppeldecker", "/dev/disk9")
        with mock.patch.object(self.module.subprocess, "run", run):
            self.module.detach_all_reported_devices(plist)
        self.assertEqual(
            calls,
            [
                ("hdiutil", "detach", "/dev/disk9s1", "-force"),
                ("hdiutil", "detach", "/dev/disk9", "-force"),
            ],
        )

    def test_unlesbare_antwort_bleibt_folgenlos(self) -> None:
        with mock.patch.object(self.module.subprocess, "run", mock.Mock()) as run:
            self.module.detach_all_reported_devices("kein plist")
        run.assert_not_called()


class AppExportPreservationTests(unittest.TestCase):
    def prepare(self, root: Path, previous: bool = True):
        source = root / "target/release/bundle/macos/Doppeldecker.app"
        source.mkdir(parents=True)
        (source / "payload").write_bytes(b"neue fertige App")
        destination = root / "Doppeldecker.app"
        if previous:
            destination.mkdir()
            (destination / "payload").write_bytes(b"vorherige fertige App")
        return source, destination

    def test_kopierfehler_erhaelt_den_bisherigen_app_export(self):
        module = load_build_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, destination = self.prepare(root)
            def fail(command, **kwargs):
                candidate = Path(command[2])
                candidate.mkdir()
                (candidate / "payload").write_bytes(b"unvollstaendige Kopie")
                raise subprocess.CalledProcessError(1, command)
            with mock.patch.object(module, "ROOT", root), mock.patch.object(module.subprocess, "run", fail):
                with self.assertRaises(subprocess.CalledProcessError):
                    module.export_app_to_root()
            self.assertEqual((destination / "payload").read_bytes(), b"vorherige fertige App")
            self.assertEqual(list(root.glob(".doppeldecker-app-export-*")), [])

    def test_uebergabefehler_erhaelt_den_bisherigen_export(self):
        module = load_build_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, destination = self.prepare(root)
            def copy(command, **kwargs):
                shutil.copytree(command[1], command[2])
            with mock.patch.object(module, "ROOT", root), mock.patch.object(module.subprocess, "run", copy), \
                    mock.patch.object(module, "replace_exported_app", side_effect=OSError("gezielter Übergabefehler")):
                with self.assertRaises(OSError):
                    module.export_app_to_root()
            self.assertEqual((destination / "payload").read_bytes(), b"vorherige fertige App")
            self.assertEqual(list(root.glob(".doppeldecker-app-export-*")), [])

    @unittest.skipUnless(sys.platform == "darwin", "macOS-App-Export verwendet Darwin-Atomik")
    def test_native_uebergabe_fuer_erstexport_und_vorhandene_app(self):
        for previous in (False, True):
            with self.subTest(previous=previous), tempfile.TemporaryDirectory() as directory:
                module = load_build_release()
                root = Path(directory)
                _, destination = self.prepare(root, previous)
                def copy(command, **kwargs):
                    shutil.copytree(command[1], command[2])
                with mock.patch.object(module, "ROOT", root), mock.patch.object(module.subprocess, "run", copy):
                    module.export_app_to_root()
                self.assertEqual((destination / "payload").read_bytes(), b"neue fertige App")
                self.assertEqual(list(root.glob(".doppeldecker-app-export-*")), [])

    @unittest.skipUnless(sys.platform == "darwin", "macOS-App-Export verwendet Darwin-Atomik")
    def test_native_uebergabe_erhaelt_fremde_datei_und_symlinks(self):
        for kind in ("file", "symlink", "dangling"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                module = load_build_release()
                root = Path(directory)
                _, destination = self.prepare(root, previous=False)
                target = root / "alias-target"
                if kind == "file":
                    destination.write_bytes(b"fremde Datei")
                else:
                    if kind == "symlink":
                        target.mkdir()
                        (target / "payload").write_bytes(b"fremde verknuepfte Daten")
                    destination.symlink_to(target, target_is_directory=True)
                def copy(command, **kwargs):
                    shutil.copytree(command[1], command[2])
                with mock.patch.object(module, "ROOT", root), mock.patch.object(module.subprocess, "run", copy):
                    with self.assertRaises(FileExistsError):
                        module.export_app_to_root()
                if kind == "file":
                    self.assertEqual(destination.read_bytes(), b"fremde Datei")
                else:
                    self.assertTrue(destination.is_symlink())
                    self.assertEqual(destination.readlink(), target)
                    if kind == "symlink":
                        self.assertEqual((target / "payload").read_bytes(), b"fremde verknuepfte Daten")
                    else:
                        self.assertFalse(target.exists())
                self.assertEqual(list(root.glob(".doppeldecker-app-export-*")), [])

    @unittest.skipUnless(sys.platform == "darwin", "macOS-App-Export verwendet Darwin-Atomik")
    def test_native_uebergabe_ohne_kandidaten_erhaelt_altstand(self):
        module = load_build_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, destination = self.prepare(root)
            with mock.patch.object(module, "ROOT", root), mock.patch.object(module.subprocess, "run"):
                with self.assertRaises(FileNotFoundError):
                    module.export_app_to_root()
            self.assertEqual((destination / "payload").read_bytes(), b"vorherige fertige App")
            self.assertEqual(list(root.glob(".doppeldecker-app-export-*")), [])


class DmgArtifactPreservationTests(unittest.TestCase):
    def test_erst_ein_vollstaendig_notarisiertes_image_ersetzt_das_alte(self) -> None:
        for failure in ("submit", "staple", "validate", None):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                module = load_build_release()
                root = Path(directory)
                bundle = root / "target/release/bundle/dmg"
                bundle.mkdir(parents=True)
                image = bundle / "Doppeldecker_0.1.123_aarch64.dmg"
                image.write_bytes(b"vorheriges Image")
                original_exists = Path.exists
                verified = []

                def absent_mount(candidate):
                    return False if candidate.parent == Path("/Volumes") else original_exists(candidate)

                def run(command, **_kwargs):
                    if command[0] == "du":
                        return subprocess.CompletedProcess(command, 0, stdout="1\tApp.app\n")
                    if command[:2] == ("hdiutil", "attach"):
                        mount = command[command.index("-mountpoint") + 1]
                        return subprocess.CompletedProcess(command, 0, stdout=attach_plist(mount, "/dev/disk-test"))
                    if command[:2] == ("hdiutil", "convert"):
                        Path(command[-1]).write_bytes(b"neues Image")
                    if command[0] == "xcrun":
                        action = command[2]
                        verified.append(action)
                        # Das alte Paket muss selbst während der letzten Prüfung
                        # noch am endgültigen Platz liegen.
                        self.assertEqual(image.read_bytes(), b"vorheriges Image")
                        if action == failure:
                            raise subprocess.CalledProcessError(1, command)
                    return subprocess.CompletedProcess(command, 0)

                with ExitStack() as patches:
                    patches.enter_context(mock.patch.object(module, "ROOT", root))
                    patches.enter_context(mock.patch.object(module, "workspace_version", return_value="0.1.123"))
                    patches.enter_context(mock.patch.object(Path, "exists", absent_mount))
                    patches.enter_context(mock.patch.object(Path, "symlink_to"))
                    patches.enter_context(mock.patch.object(module.subprocess, "run", run))
                    for name, value in {
                        "release_environment": {}, "developer_id_identity": "Testidentität",
                        "require_notary_profile": "Testprofil", "run_tauri_build": None,
                        "notarize_and_staple_app": None, "require_macos_release_arch": None,
                        "repack_and_sign_macos_updater": root / "updater.tar.gz",
                        "verify_macos_updater_archive": None, "export_app_to_root": None,
                    }.items():
                        patches.enter_context(mock.patch.object(module, name, return_value=value))
                    if failure:
                        with self.assertRaises(subprocess.CalledProcessError):
                            module.release_macos(finder_layout=False)
                        self.assertEqual(image.read_bytes(), b"vorheriges Image")
                    else:
                        module.release_macos(finder_layout=False)
                        self.assertEqual(image.read_bytes(), b"neues Image")
                        self.assertEqual(verified, ["submit", "staple", "validate"])
                self.assertEqual(list(bundle.iterdir()), [image])

    def test_belegter_mount_erhaelt_vorhandene_images(self) -> None:
        module = load_build_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "target/release/bundle/dmg"
            bundle.mkdir(parents=True)
            images = [bundle / "Doppeldecker_0.1.123_aarch64.dmg",
                      bundle / "Doppeldecker_0.1.123_aarch64-rw.dmg"]
            for image in images:
                image.write_bytes(b"vorheriges Image")
            original_exists = Path.exists
            def occupied(candidate):
                return candidate.parent == Path("/Volumes") or original_exists(candidate)
            with mock.patch.object(module, "ROOT", root), \
                    mock.patch.object(module, "workspace_version", return_value="0.1.123"), \
                    mock.patch.object(Path, "exists", occupied), \
                    mock.patch.object(module.subprocess, "run") as run:
                with self.assertRaises(SystemExit):
                    module.build_dmg_from_app(root / "App.app", "Testidentität", False, "Testprofil")
                run.assert_not_called()
            for image in images:
                self.assertEqual(image.read_bytes(), b"vorheriges Image")

    def test_gescheiterter_neubau_erhaelt_fertiges_image(self) -> None:
        module = load_build_release()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bundle = root / "target/release/bundle/dmg"
            bundle.mkdir(parents=True)
            image = bundle / "Doppeldecker_0.1.123_aarch64.dmg"
            image.write_bytes(b"vorheriges Image")
            original_exists = Path.exists
            def absent_mount(candidate):
                return False if candidate.parent == Path("/Volumes") else original_exists(candidate)
            def run(command, **_kwargs):
                if command[0] == "du":
                    return subprocess.CompletedProcess(command, 0, stdout="1\tApp.app\n")
                raise subprocess.CalledProcessError(1, command)
            with mock.patch.object(module, "ROOT", root), \
                    mock.patch.object(module, "workspace_version", return_value="0.1.123"), \
                    mock.patch.object(Path, "exists", absent_mount), \
                    mock.patch.object(module.subprocess, "run", run):
                with self.assertRaises(subprocess.CalledProcessError):
                    module.build_dmg_from_app(root / "App.app", "Testidentität", False, "Testprofil")
            self.assertEqual(image.read_bytes(), b"vorheriges Image")


def source_lines(relative: str) -> list[str]:
    return (ROOT / relative).read_text(encoding="utf-8").splitlines()


def line_of(lines: list[str], needle: str) -> int:
    """Zeilennummer des ersten Vorkommens; -1, wenn es den Text nicht gibt."""
    for number, line in enumerate(lines, 1):
        if needle in line:
            return number
    return -1


class BuildReleaseSourceOrderTests(unittest.TestCase):
    """Die Device-Bindung selbst muss im geschützten Bereich stehen.

    `attached_device` steigt bei einer unerwarteten Antwort mit `SystemExit` aus.
    Stand der Aufruf vor dem `try`, blieb das Image bei genau diesem Fehler angehängt.
    """

    def test_device_bindung_liegt_in_einem_try_mit_notaushaengen(self) -> None:
        lines = source_lines("tools/build-release.py")
        binding = line_of(lines, "device = attached_device(attach.stdout, mount)")
        self.assertGreater(binding, 0, "Device-Bindung nicht gefunden")
        self.assertEqual(lines[binding - 2].strip(), "try:")
        following = "\n".join(lines[binding : binding + 3])
        self.assertIn("except BaseException:", following)
        self.assertIn("detach_all_reported_devices(attach.stdout)", following)


class VerifyMacosReleaseSourceTests(unittest.TestCase):
    """Das Prüfskript hängt sein eigenes Device geprüft aus, bevor es Erfolg meldet."""

    def setUp(self) -> None:
        self.lines = source_lines("tools/verify-macos-release.sh")

    def test_attach_bindet_ein_eigenes_device(self) -> None:
        attach = line_of(self.lines, "hdiutil attach -readonly")
        self.assertGreater(attach, 0)
        self.assertIn("-plist", self.lines[attach - 1])
        self.assertGreater(line_of(self.lines, "device=$("), 0)

    def test_geprueftes_aushaengen_steht_vor_der_erfolgsmeldung(self) -> None:
        # Regression: Ausgehängt wurde nur im EXIT-Trap, also NACH der Meldung — und
        # dort verwarf `|| true` jeden Fehler.
        detach = line_of(self.lines, "if ! detach_own_device; then")
        success = line_of(self.lines, "macOS-Bundle grün")
        self.assertGreater(detach, 0, "Geprüftes Aushängen fehlt")
        self.assertGreater(success, 0)
        self.assertLess(detach, success)
        self.assertIn("exit 1", "\n".join(self.lines[detach : detach + 3]))

    def test_exit_trap_bleibt_als_notfallweg(self) -> None:
        self.assertGreater(line_of(self.lines, "trap cleanup EXIT"), 0)


class VerifyMacosReleaseBehaviorTests(unittest.TestCase):
    """Führt das echte Shellskript mit künstlichen Apple-Werkzeugen aus."""

    def run_verifier(self, failure: str) -> tuple[subprocess.CompletedProcess, list[list[str]]]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = root / "bin"
            binaries.mkdir()
            app = root / "target/release/bundle/macos/Doppeldecker.app"
            (app / "Contents/MacOS").mkdir(parents=True)
            for name in ["doppeldecker", "doppeldecker-cli"]:
                executable = app / "Contents/MacOS" / name
                executable.write_text("Testbinary")
                executable.chmod(0o755)
            (root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.1.123"\n')
            dmg = root / "target/release/bundle/dmg/Doppeldecker_0.1.123_aarch64.dmg"
            dmg.parent.mkdir(parents=True)
            dmg.write_bytes(b"Testimage")
            stub = binaries / "apple-tool"
            stub.write_text(f"#!{sys.executable}\n" + r'''
import json, os, pathlib, plistlib, shutil, sys
tool, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
root = pathlib.Path(os.environ["REVIEW_FIXTURE"])
if tool == "uname":
    print("Darwin")
elif tool == "PlistBuddy":
    key = args[1].removeprefix("Print :")
    values = {"NSLocalNetworkUsageDescription": "Test-LAN", "NSBonjourServices:0": "_smb._tcp",
        "NSBonjourServices:1": "_sftp-ssh._tcp", "NSBonjourServices:2": "_webdav._tcp",
        "CFBundleIdentifier": "com.doppeldecker.desktop", "CFBundleShortVersionString": "0.1.123"}
    if key not in values: sys.exit(1)
    print(values[key])
elif tool == "codesign":
    if "-dv" in args:
        print("Identifier=com.doppeldecker.desktop\nSignature=adhoc")
    elif os.environ["REVIEW_FAILURE"] == "embedded" and args[-1].startswith("/tmp/"):
        sys.exit(42)
elif tool == "hdiutil":
    with (root / "calls.jsonl").open("a") as log:
        log.write(json.dumps(args) + "\n")
    if args[0] == "attach":
        mount = pathlib.Path(args[args.index("-mountpoint") + 1])
        (root / "mount.json").write_text(json.dumps(str(mount)))
        shutil.copytree(root / "target/release/bundle/macos/Doppeldecker.app", mount / "Doppeldecker.app")
        reported = "/unrelated" if os.environ["REVIEW_FAILURE"] == "mapping" else str(mount)
        print(plistlib.dumps({"system-entities": [{"dev-entry": "/dev/disk-test-parent"},
            {"dev-entry": "/dev/disk-test", "mount-point": reported}]}).decode())
    elif args[0] == "detach":
        mount = pathlib.Path(json.loads((root / "mount.json").read_text()))
        if mount.name.startswith("doppeldecker-dmg-verify."):
            shutil.rmtree(mount / "Doppeldecker.app", ignore_errors=True)
''')
            stub.chmod(0o755)
            for name in ["uname", "PlistBuddy", "codesign", "hdiutil"]:
                (binaries / name).symlink_to(stub)
            (binaries / "python3").symlink_to(sys.executable)
            source = (ROOT / "tools/verify-macos-release.sh").read_text()
            self.assertIn("/usr/libexec/PlistBuddy", source)
            script = root / "verify.sh"
            script.write_text(source.replace("/usr/libexec/PlistBuddy", str(binaries / "PlistBuddy")))
            environment = {**os.environ, "PATH": f"{binaries}:{os.environ['PATH']}",
                "REVIEW_FIXTURE": str(root), "REVIEW_FAILURE": failure}
            try:
                result = subprocess.run(["/bin/sh", str(script), "--allow-adhoc"], cwd=root,
                    env=environment, text=True, capture_output=True, timeout=15)
                calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
                return result, calls
            finally:
                if (root / "mount.json").exists():
                    mount = Path(json.loads((root / "mount.json").read_text()))
                    if mount.parent == Path("/tmp") and mount.name.startswith("doppeldecker-dmg-verify."):
                        shutil.rmtree(mount, ignore_errors=True)

    def test_fehler_cleanup_bindet_device_statt_mountpfad(self) -> None:
        result, calls = self.run_verifier("embedded")
        self.assertNotEqual(result.returncode, 0)
        detached = [call for call in calls if call[0] == "detach"]
        self.assertEqual(detached, [["detach", "/dev/disk-test", "-force"]])

    def test_fehlende_mountzuordnung_raeumt_nur_gemeldete_devices_auf(self) -> None:
        result, calls = self.run_verifier("mapping")
        self.assertNotEqual(result.returncode, 0)
        detached = [call for call in calls if call[0] == "detach"]
        self.assertEqual(detached, [["detach", "/dev/disk-test-parent", "-force"],
                                   ["detach", "/dev/disk-test", "-force"]])

    def test_erfolg_haengt_device_genau_einmal_aus(self) -> None:
        result, calls = self.run_verifier("")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("macOS-Bundle grün", result.stdout)
        self.assertEqual([call for call in calls if call[0] == "detach"],
                         [["detach", "/dev/disk-test"]])


if __name__ == "__main__":
    unittest.main()
