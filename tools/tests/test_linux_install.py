"""Installation und Rücknahme ausschließlich in künstlichen Benutzerverzeichnissen."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from module_loader import load_tool_module


class LinuxInstallTests(unittest.TestCase):
    def setUp(self):
        self.module = load_tool_module("linux_install_test", "install-linux.py")
        self.area = tempfile.TemporaryDirectory()
        self.addCleanup(self.area.cleanup)
        self.root = Path(self.area.name)
        self.source = self.root / "candidate.AppImage"
        self.prefix = self.root / "Benutzer mit Leerzeichen" / ".local"
        header = bytearray(20)
        header[:6] = b"\x7fELF\x02\x01"
        header[8:11] = b"AI\x02"
        header[18:20] = (62).to_bytes(2, "little")
        self.source.write_bytes(header + b"first candidate")
        self.source.chmod(0o755)
        self.desktop_update = mock.patch.object(self.module.shutil, "which", return_value=None)
        self.desktop_update.start()
        self.addCleanup(self.desktop_update.stop)

    def test_install_and_upgrade_keep_writable_update_path_and_user_configuration(self):
        configuration = self.prefix / "unrelated-settings.json"
        configuration.parent.mkdir(parents=True)
        configuration.write_text("user settings")
        first = self.module.install(self.source, self.prefix, self.module.digest(self.source))
        app = Path(first["appimage"])
        self.assertEqual(app.read_bytes(), self.source.read_bytes())
        self.assertEqual(app.stat().st_mode & 0o777, 0o755)
        # Eine echte Updaterinstallation ändert nur die Nutzlast, nicht die Starter.
        app.write_bytes(app.read_bytes() + b"updated by plugin")
        self.source.write_bytes(self.source.read_bytes() + b"new candidate")
        second = self.module.install(self.source, self.prefix)
        self.assertEqual(first["appimage"], second["appimage"])
        self.assertEqual(app.read_bytes(), self.source.read_bytes())
        self.assertEqual(configuration.read_text(), "user settings")
        desktop = self.prefix / "share/applications/com.doppeldecker.desktop.desktop"
        self.assertIn(self.module.exec_value(app), desktop.read_text())

    def test_failed_initial_install_removes_its_inserted_files(self):
        replace = self.module.os.replace
        calls = 0

        def fail_second(source, target):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("synthetic write failure")
            return replace(source, target)

        with mock.patch.object(self.module.os, "replace", side_effect=fail_second):
            with self.assertRaises(OSError):
                self.module.install(self.source, self.prefix)
        self.assertFalse((self.prefix / "lib/doppeldecker/Doppeldecker.AppImage").exists())
        self.assertFalse((self.prefix / "bin/doppeldecker").exists())

    def test_failed_upgrade_restores_previous_payload(self):
        first = self.module.install(self.source, self.prefix)
        app = Path(first["appimage"])
        old = app.read_bytes()
        self.source.write_bytes(old + b"next")
        replace = self.module.os.replace

        def fail_manifest(source, target):
            if Path(source).name == "installation.json":
                raise OSError("synthetic manifest failure")
            return replace(source, target)

        with mock.patch.object(self.module.os, "replace", side_effect=fail_manifest):
            with self.assertRaises(OSError):
                self.module.install(self.source, self.prefix)
        self.assertEqual(app.read_bytes(), old)
        self.assertEqual(json.loads((app.parent / "installation.json").read_text()), first)

    def test_checksum_and_foreign_launcher_are_rejected_before_replacement(self):
        with self.assertRaises(ValueError):
            self.module.install(self.source, self.prefix, "0" * 64)
        launcher = self.prefix / "bin/doppeldecker"
        launcher.parent.mkdir(parents=True)
        launcher.write_text("foreign program")
        with self.assertRaises(ValueError):
            self.module.install(self.source, self.prefix)
        self.assertEqual(launcher.read_text(), "foreign program")
