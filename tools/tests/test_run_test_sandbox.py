"""Prüft den `--binary`-Modus des Sandbox-Launchers.

Der Linux-Testhost hat die Dev-Bibliotheken der GUI nicht und kann deshalb nicht
selbst übersetzen; das Programm entsteht dort in einem Container. Ohne diesen Modus
müsste ein solcher Test am Launcher vorbei gestartet werden — und liefe damit ohne
die markierte Sandbox und ohne die Testbremse, die das Backend nur zusammen mit ihr
akzeptiert.
"""

from __future__ import annotations

import json
import os
import plistlib
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "run-test-sandbox.sh"


def write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(body).lstrip(), encoding="utf-8")
    path.chmod(0o755)


class SandboxLauncherBinaerModus(unittest.TestCase):
    def _starte(self, *argumente: str, tmpdir: str) -> subprocess.CompletedProcess[str]:
        umgebung = dict(os.environ)
        # Der Wegwerfbereich entsteht unter TMPDIR; so bleibt er im Testordner.
        umgebung["TMPDIR"] = tmpdir
        return subprocess.run(
            [str(SCRIPT), *argumente],
            cwd=ROOT,
            env=umgebung,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=60,
            check=False,
        )

    def test_uebergebenes_programm_bekommt_die_markierte_sandbox(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = Path(raw)
            bericht = temp / "bericht.json"
            # Das Ersatzprogramm hält fest, womit der Launcher es gestartet hat, und
            # endet sofort — es öffnet kein Fenster.
            write_executable(
                temp / "programm" / "doppeldecker",
                f"""
                #!/usr/bin/env python3
                import json, os, sys
                with open("{bericht}", "w", encoding="utf-8") as ziel:
                    json.dump({{
                        "sandbox": os.environ.get("DOPPELDECKER_SANDBOX"),
                        "bremse": os.environ.get("DOPPELDECKER_TEST_COPY_DELAY_MS"),
                        "programm": sys.argv[0],
                    }}, ziel)
                """,
            )
            prozess = self._starte(
                "--binary", str(temp / "programm" / "doppeldecker"), tmpdir=raw
            )
            self.assertEqual(prozess.returncode, 0, f"stderr:\n{prozess.stderr}")
            gemeldet = json.loads(bericht.read_text(encoding="utf-8"))

            # Innerhalb des Blocks prüfen: Der Wegwerfbereich liegt unter TMPDIR und
            # verschwindet mit ihm.
            sandbox = Path(gemeldet["sandbox"])
            self.assertTrue(sandbox.name.startswith("doppeldecker-test."), sandbox)
            self.assertEqual(gemeldet["bremse"], "5")
            # Marker und beide Panes müssen stehen, sonst lehnt das Backend die
            # Testbremse ab.
            self.assertTrue((sandbox / ".doppeldecker-test-sandbox").is_file())
            self.assertTrue((sandbox / "quelle").is_dir())
            self.assertTrue((sandbox / "ziel").is_dir())

    @unittest.skipUnless(sys.platform == "darwin", "Bundle entsteht nur unter macOS")
    def test_bundle_traegt_sandbox_und_bremse_als_lsenvironment(self) -> None:
        """`--open` startet über LaunchServices, und `open` reicht keine Umgebung
        weiter. Sandbox und Testbremse müssen deshalb im Bundle selbst stehen,
        sonst lehnt das Backend die Bremse ab."""
        with tempfile.TemporaryDirectory() as raw:
            temp = Path(raw)
            bericht = temp / "bericht.json"
            write_executable(
                temp / "programm" / "doppeldecker",
                f"""
                #!/usr/bin/env python3
                import json, os
                with open("{bericht}", "w", encoding="utf-8") as ziel:
                    json.dump({{"sandbox": os.environ.get("DOPPELDECKER_SANDBOX")}}, ziel)
                """,
            )
            prozess = self._starte(
                "--binary", str(temp / "programm" / "doppeldecker"), tmpdir=raw
            )
            self.assertEqual(prozess.returncode, 0, f"stderr:\n{prozess.stderr}")
            sandbox = Path(json.loads(bericht.read_text(encoding="utf-8"))["sandbox"])
            plist_path = next(sandbox.glob(".doppeldecker-test-bundle.*/Doppeldecker-Test.app/Contents/Info.plist"))
            plist = plist_path.read_text(encoding="utf-8")

            self.assertIn("LSEnvironment", plist)
            self.assertIn(str(sandbox), plist)
            self.assertIn("DOPPELDECKER_TEST_COPY_DELAY_MS", plist)

    @unittest.skipUnless(sys.platform == "darwin", "Bundle entsteht nur unter macOS")
    def test_bundle_plist_erhaelt_sandboxpfad_mit_xml_sonderzeichen(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = Path(raw)
            basis = temp / "Test & <Recovery>"
            basis.mkdir()
            programm = temp / "doppeldecker"
            write_executable(programm, "#!/bin/sh\nexit 0\n")
            prozess = self._starte("--binary", str(programm), tmpdir=str(basis))
            self.assertEqual(prozess.returncode, 0, f"stderr:\n{prozess.stderr}")
            sandboxes = list(basis.glob("doppeldecker-test.*"))
            self.assertEqual(len(sandboxes), 1)
            sandbox = sandboxes[0]
            plist_path = next(sandbox.glob("**/Doppeldecker-Test.app/Contents/Info.plist"))
            with plist_path.open("rb") as source:
                document = plistlib.load(source)
            self.assertEqual(document["LSEnvironment"], {
                "DOPPELDECKER_SANDBOX": str(sandbox),
                "DOPPELDECKER_TEST_COPY_DELAY_MS": "5",
            })

    @unittest.skipUnless(sys.platform == "darwin", "Bundle entsteht nur unter macOS")
    def test_recovery_folgt_keinem_vorhandenen_bundle_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            temp = Path(raw)
            sandbox = temp / "sandbox"
            sandbox.mkdir()
            (sandbox / ".doppeldecker-test-sandbox").write_text("DOPPELDECKER TEST SANDBOX\n")
            (sandbox / "quelle").mkdir()
            (sandbox / "ziel").mkdir()
            fremdes_bundle = temp / "anderes-bundle"
            fremdes_binary = fremdes_bundle / "Contents/MacOS/doppeldecker"
            write_executable(fremdes_binary, "#!/bin/sh\n# Fremder Stand\nexit 0\n")
            vorher = fremdes_binary.read_bytes()
            (sandbox / "Doppeldecker-Test.app").symlink_to(fremdes_bundle, target_is_directory=True)
            programm = temp / "doppeldecker"
            write_executable(programm, "#!/bin/sh\n# Neuer Teststand\nexit 0\n")
            prozess = self._starte("--reuse", str(sandbox), "--binary", str(programm), tmpdir=raw)
            self.assertEqual(prozess.returncode, 0, f"stderr:\n{prozess.stderr}")
            self.assertEqual(fremdes_binary.read_bytes(), vorher)
            self.assertFalse((fremdes_bundle / "Contents/Info.plist").exists())
            private_binaries = list(sandbox.glob(".doppeldecker-test-bundle.*/Doppeldecker-Test.app/Contents/MacOS/doppeldecker"))
            self.assertEqual(len(private_binaries), 1)
            self.assertEqual(private_binaries[0].read_bytes(), programm.read_bytes())

    def test_fehlendes_programm_wird_vor_jeder_sandbox_abgelehnt(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            prozess = self._starte("--binary", f"{raw}/gibt-es-nicht", tmpdir=raw)
            uebrig = list(Path(raw).glob("doppeldecker-test.*"))

        self.assertEqual(prozess.returncode, 2, f"stderr:\n{prozess.stderr}")
        self.assertIn("nicht ausführbar", prozess.stderr)
        self.assertEqual(uebrig, [], "abgelehnter Start darf keinen Testbereich anlegen")

    def test_binary_schliesst_die_anderen_programmquellen_aus(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            prozess = self._starte("--binary", "/bin/sh", "--release", tmpdir=raw)

        self.assertEqual(prozess.returncode, 2, f"stderr:\n{prozess.stderr}")
        self.assertIn("schließt --release und --installed aus", prozess.stderr)

    @unittest.skipUnless(sys.platform == "darwin", "--open gibt es nur unter macOS")
    def test_release_bundle_darf_nicht_unmarkiert_ueber_launchservices_starten(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            prozess = self._starte("--release", "--open", tmpdir=raw)
            uebrig = list(Path(raw).glob("doppeldecker-test.*"))

        self.assertEqual(prozess.returncode, 2, f"stderr:\n{prozess.stderr}")
        self.assertIn("Release-Bundle enthält keine Test-Sandbox", prozess.stderr)
        self.assertEqual(uebrig, [], "abgelehnter Start darf keinen Testbereich anlegen")


if __name__ == "__main__":
    unittest.main()
