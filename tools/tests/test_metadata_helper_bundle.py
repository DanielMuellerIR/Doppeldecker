"""Paketprüfungen des Administratorhelfers ohne Systemregistrierung."""
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from module_loader import load_tool_module


class MetadataHelperBundleTests(unittest.TestCase):
    def setUp(self):
        self.module = load_tool_module("metadata_helper_build", "build-release.py")
        self.temp = tempfile.TemporaryDirectory(prefix="doppeldecker-helper-bundle-")
        self.addCleanup(self.temp.cleanup)
        self.app = Path(self.temp.name) / "Doppeldecker.app"
        self.helper = self.app / self.module.METADATA_HELPER_PROGRAM
        self.helper.parent.mkdir(parents=True)
        self.helper.write_bytes(b"synthetic Mach-O placeholder")
        self.helper.chmod(0o755)
        self.plist = self.app / f"Contents/Library/LaunchDaemons/{self.module.METADATA_HELPER_ID}.plist"
        self.plist.parent.mkdir(parents=True)
        self.source = Path(__file__).resolve().parents[2] / "native" / self.plist.name
        self.plist.write_bytes(self.source.read_bytes())
        self.calls = []
        self.helper_team = "A123456789"
        self.helper_identifier = self.module.METADATA_HELPER_ID

    def run_command(self, command, **kwargs):
        self.calls.append(command)
        if "-dv" in command:
            helper = Path(command[-1]) == self.helper
            identifier = self.helper_identifier if helper else "com.doppeldecker.desktop"
            team = self.helper_team if helper else "A123456789"
            return subprocess.CompletedProcess(command, 0, "", f"Identifier={identifier}\nTeamIdentifier={team}\n")
        return subprocess.CompletedProcess(command, 0, "", "")

    def test_passendes_paket_wird_ohne_registrierung_geprueft(self):
        self.module.verify_metadata_helper(self.app, run_command=self.run_command)
        self.assertEqual(len(self.calls), 4)
        self.assertTrue(all(command[0] == "codesign" for command in self.calls))

    def test_fehlende_oder_umgeleitete_nutzlast_stoppt_vor_signaturwerkzeugen(self):
        self.helper.unlink()
        with self.assertRaises(SystemExit):
            self.module.verify_metadata_helper(self.app, run_command=self.run_command)
        self.helper.symlink_to(self.source)
        with self.assertRaises(SystemExit):
            self.module.verify_metadata_helper(self.app, run_command=self.run_command)
        self.assertEqual(self.calls, [])

    def test_nicht_ausfuehrbarer_helfer_wird_vor_signaturwerkzeugen_abgelehnt(self):
        self.helper.chmod(0o644)
        with self.assertRaises(SystemExit):
            self.module.verify_metadata_helper(self.app, run_command=self.run_command)
        self.assertEqual(self.calls, [])

    def test_absoluter_programmpfad_und_zusaetzliche_dienste_werden_abgelehnt(self):
        original = plistlib.loads(self.source.read_bytes())
        for service in [dict(original, Program="/bin/sh"), dict(original, BundleProgram="/tmp/helper"), dict(original, UserName="nobody"), dict(original, MachServices={"arbitrary": True})]:
            self.plist.write_bytes(plistlib.dumps(service))
            with self.assertRaises(SystemExit):
                self.module.verify_metadata_helper(self.app, run_command=self.run_command)
        self.assertEqual(self.calls, [])

    def test_falsches_team_ad_hoc_und_falscher_identifier_werden_abgelehnt(self):
        for team, identifier in [("B123456789", self.module.METADATA_HELPER_ID), ("not set", self.module.METADATA_HELPER_ID), ("A123456789", "other.tool")]:
            self.helper_team, self.helper_identifier = team, identifier
            with self.assertRaises(SystemExit):
                self.module.verify_metadata_helper(self.app, run_command=self.run_command)


if __name__ == "__main__":
    unittest.main()
