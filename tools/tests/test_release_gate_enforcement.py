"""Belegt, dass drei Release-Gates ihren Fehlerfall wirklich sperren.

Alle drei Prüfungen standen an der richtigen Stelle, konnten aber nicht fehlschlagen:
``verify-macos-release.sh`` prüfte die Team-ID mit ``grep -qv``, der SMB-E2E-Runner
verliess sich auf den Exitcode von ``cargo test <filter>`` (der auch bei null Treffern
0 ist), und der npm-Schritt ``release:update-manifest`` konnte seine Pflichtoption gar
nicht bekommen.

Die beiden ausführbaren Fälle laufen ohne Netz, ohne Apple-Werkzeuge und ohne Samba:
Der SMB-Runner bekommt ein ``cargo``-Stub im PATH und bricht ab, bevor er einen
Wegwerfbaum oder einen Server anlegt.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


class SmbRunnerRequiresExactlyOneTestTests(unittest.TestCase):
    """Der Runner darf einen leeren Filter nie als grünen Lauf melden."""

    def run_with_cargo_stub(self, listing: str) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory(prefix="doppeldecker-smb-runner-") as tmp:
            bin_dir = Path(tmp) / "bin"
            bin_dir.mkdir()
            cargo = bin_dir / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                # Nur das Listing beantworten; jeder echte Lauf wäre ein Testfehler.
                'case "$*" in\n'
                "  *--list*) printf '%s' \"$STUB_LISTING\" ;;\n"
                "  *) printf 'Der Stub darf nie einen echten Testlauf starten.\\n' >&2; exit 97 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            cargo.chmod(0o755)
            smbd = bin_dir / "smbd-stub"
            smbd_started = Path(tmp) / "smbd-started"
            smbd.write_text(
                "#!/bin/sh\n"
                f'printf \'%s\\n\' "$*" > "{smbd_started}"\n'
                "sleep 5\n",
                encoding="utf-8",
            )
            smbd.chmod(0o755)

            umgebung = dict(os.environ)
            umgebung["PATH"] = f"{bin_dir}:{umgebung['PATH']}"
            umgebung["STUB_LISTING"] = listing
            umgebung["DOPPELDECKER_SMBD"] = str(smbd)
            umgebung.pop("DOPPELDECKER_SMB_E2E_TEST_HOLD", None)
            ergebnis = subprocess.run(
                ["sh", str(ROOT / "tools" / "run-smb-e2e.sh")],
                capture_output=True,
                text=True,
                env=umgebung,
                timeout=120,
            )
            self.assertFalse(
                smbd_started.exists(),
                "Die Testzählung muss vor jedem SMB-Serverstart abbrechen",
            )
            return ergebnis

    def test_fehlender_test_bricht_vor_jeder_serverarbeit_ab(self) -> None:
        vorher = set(Path("/tmp").glob("doppeldecker-smb-e2e.*"))
        ergebnis = self.run_with_cargo_stub("irgendein_anderer_test: test\n\n1 test\n")
        self.assertNotEqual(ergebnis.returncode, 0, ergebnis.stdout + ergebnis.stderr)
        self.assertIn("0-mal exakt gefunden", ergebnis.stderr)
        self.assertEqual(
            vorher,
            set(Path("/tmp").glob("doppeldecker-smb-e2e.*")),
            "Ein fehlender Test darf keinen Wegwerfbaum hinterlassen",
        )

    def test_doppelter_treffer_bricht_ebenfalls_ab(self) -> None:
        name = "smb3_guid_paging_stream_quarantaene_reconnect_und_queue_bleiben_gebunden"
        ergebnis = self.run_with_cargo_stub(f"{name}: test\n{name}: test\n\n2 tests\n")
        self.assertNotEqual(ergebnis.returncode, 0)
        self.assertIn("2-mal exakt gefunden", ergebnis.stderr)

    def test_runner_faehrt_den_test_exakt(self) -> None:
        runner = read("tools/run-smb-e2e.sh")
        self.assertIn('--test smb_e2e "$test_name" -- --exact --ignored', runner)


class UpdateManifestIsReachableFromNpmTests(unittest.TestCase):
    """Die dokumentierte gemeinsame Finalisierung muss aufrufbar sein."""

    def test_npm_schritt_schreibt_erst_das_manifest_dann_die_pruefsummen(self) -> None:
        scripts = json.loads(read("package.json"))["scripts"]
        schritt = scripts["release:update-manifest"]
        manifest = schritt.index("tools/write-update-manifest.py")
        checksums = schritt.index("tools/write-release-checksums.py")
        self.assertLess(
            manifest,
            checksums,
            "Nur in dieser Reihenfolge enthält SHA256SUMS das veröffentlichte latest.json",
        )

    def test_veroeffentlichungszeit_kommt_auch_aus_der_umgebung(self) -> None:
        # npm und pnpm hängen zusätzliche Argumente ans Ende der Kommandozeichenkette,
        # also an den zweiten Befehl. Ohne den Umgebungsweg wäre der Schritt tot.
        skript = str(ROOT / "tools" / "write-update-manifest.py")
        umgebung = dict(os.environ)
        umgebung["DOPPELDECKER_PUB_DATE"] = "kein-datum"
        mit_variable = subprocess.run(
            [sys.executable, skript], capture_output=True, text=True, env=umgebung, cwd=ROOT
        )
        self.assertNotIn("Veröffentlichungszeit fehlt", mit_variable.stderr)

        umgebung.pop("DOPPELDECKER_PUB_DATE")
        ohne_variable = subprocess.run(
            [sys.executable, skript], capture_output=True, text=True, env=umgebung, cwd=ROOT
        )
        self.assertNotEqual(ohne_variable.returncode, 0)
        self.assertIn("Veröffentlichungszeit fehlt", ohne_variable.stderr)

    def test_zeit_wird_nie_geraten(self) -> None:
        quelle = read("tools/write-update-manifest.py")
        self.assertNotIn("datetime.now", quelle)
        self.assertNotIn("utcnow", quelle)


class TeamIdentifierCheckCanFailTests(unittest.TestCase):
    """Quelltextprüfung mit Absicht.

    Der Zweig läuft erst nach ``codesign -dv`` auf einem signierten Bundle und nach
    ``hdiutil verify`` auf einem echten DMG. Ihn auszuführen hiesse, eine Signatur und
    ein Image zu bauen — genau die schwere Kette, die dieses Gate absichern soll.
    Geprüft wird deshalb, dass der Zweig überhaupt einen Fehlerausgang besitzt.
    """

    def test_signatur_ohne_team_wird_abgelehnt(self) -> None:
        quelle = read("tools/verify-macos-release.sh")
        self.assertNotIn(
            "grep -qv 'TeamIdentifier=not set'",
            quelle,
            "`grep -qv` ist hier immer wahr und kann den Fehlerfall nie melden",
        )
        zweig = re.search(
            r"if printf '%s\\n' \"\$signature\" \| grep -q 'TeamIdentifier=not set'; then"
            r"(.*?)\n  fi",
            quelle,
            re.S,
        )
        self.assertIsNotNone(zweig, "Team-Identifier-Prüfung nicht gefunden")
        self.assertIn("exit 1", zweig.group(1))


if __name__ == "__main__":
    unittest.main()
