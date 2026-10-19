"""Prüft die Testauswahl im opt-in SMB-E2E-Launcher.

Der Launcher zählt vor jeder Serverarbeit, ob es den erwarteten E2E-Test genau
einmal gibt (`cargo test … -- --list`). Diese Zählung stand vor dem Import von
`$HOME/.cargo/env`. In einer sauberen Umgebung — Rustup installiert, Cargo aber
nicht im PATH — schlug deshalb genau die Prüfung fehl, die den Lauf absichert:
Sie meldete null Treffer, obwohl Cargo nach dem später folgenden Import verfügbar
gewesen wäre (Review-Fund 2026-09-04).

Die Testläufe setzen `DOPPELDECKER_SMB_E2E_TEST_HOLD=count-verified`. Der
Haltepunkt prüft nur die Zählung, beendet den Launcher absichtlich mit einem
Fehlerstatus und läuft vor `mktemp` sowie vor jedem Serverstart. Ohne ihn lief der
erste Fall auf Linux bis in die Portwarteschleife des echten Serverstarts und ließ
dort einen Wegwerfbaum samt Hashdatei liegen; auf macOS fiel das nicht auf, weil
das Skript vorher an LibreSSL scheitert (Review-Fund 2026-09-10).
"""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "tools" / "run-smb-e2e.sh"
TEST_NAME = "smb3_guid_paging_stream_quarantaene_reconnect_und_queue_bleiben_gebunden"
# Nur Systemverzeichnisse: Rustup liegt unter `$HOME/.cargo/bin` und darf hier
# gerade nicht mitkommen, sonst prüft der Test seine eigene Umgebung statt das Skript.
SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
SMB_TEMP_GLOB = "doppeldecker-smb-e2e.*"


def write_executable(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(body).lstrip(), encoding="utf-8")
    path.chmod(0o755)


class SmbLauncherTestauswahl(unittest.TestCase):
    def _lauf(
        self,
        test_dir: Path,
        mit_cargo_env: bool,
        treffer: int = 1,
    ) -> subprocess.CompletedProcess[str]:
        fake_bin = test_dir / "fake-bin"
        home = test_dir / "home"
        cargo_bin = home / ".cargo" / "bin"
        aufruf_log = test_dir / "cargo-aufrufe"
        server_aufruf = test_dir / "smbd-aufruf"

        # smbd darf nie gestartet werden: Der Haltepunkt beendet den Launcher direkt
        # nach der Zählung, also vor `mktemp` und vor jedem Serverstart. Der Marker
        # macht einen versehentlichen Start unabhängig vom Exitcode sichtbar.
        write_executable(
            fake_bin / "smbd",
            f"#!/bin/sh\nprintf '%s\\n' \"$*\" > \"{server_aufruf}\"\nexit 1\n",
        )
        # Der Ersatz-Cargo protokolliert seinen Aufruf und liefert die Testliste im
        # Format von `--list`.
        zeilen = "".join(
            f"printf '%s: test\\n' '{TEST_NAME}'\n" for _ in range(treffer)
        )
        write_executable(
            cargo_bin / "cargo",
            f"""
            #!/bin/sh
            printf '%s\\n' "$*" >> "{aufruf_log}"
            {zeilen.strip() or "true"}
            """,
        )
        if mit_cargo_env:
            (home / ".cargo" / "env").write_text(
                f'export PATH="{cargo_bin}:$PATH"\n', encoding="utf-8"
            )

        environment = dict(os.environ)
        environment.update(
            {
                "HOME": str(home),
                "PATH": f"{fake_bin}{os.pathsep}{SYSTEM_PATH}",
                "DOPPELDECKER_SMBD": str(fake_bin / "smbd"),
                "DOPPELDECKER_SMB_E2E_TEST_HOLD": "count-verified",
            }
        )
        return subprocess.run(
            [str(SCRIPT)],
            cwd=ROOT,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=60,
            check=False,
        )

    def test_rustup_umgebung_gilt_schon_fuer_die_testzaehlung(self) -> None:
        vorher = set(Path("/tmp").glob(SMB_TEMP_GLOB))
        with tempfile.TemporaryDirectory() as raw:
            test_dir = Path(raw)
            process = self._lauf(test_dir, mit_cargo_env=True)
            log = test_dir / "cargo-aufrufe"
            aufrufe = log.read_text(encoding="utf-8") if log.exists() else ""
            self.assertEqual(process.returncode, 1, f"stderr:\n{process.stderr}")
            self.assertIn("Haltepunkt", process.stdout)
            self.assertFalse((test_dir / "smbd-aufruf").exists())
        self.assertEqual(
            vorher,
            set(Path("/tmp").glob(SMB_TEMP_GLOB)),
            "Der Zähl-Haltepunkt darf keinen SMB-Wegwerfbaum anlegen",
        )

        self.assertIn(
            "--list",
            aufrufe,
            f"cargo wurde nicht gefunden; stderr:\n{process.stderr}",
        )
        self.assertNotIn("0-mal exakt gefunden", process.stderr)

    def test_fehlendes_cargo_meldet_das_werkzeug_statt_null_treffer(self) -> None:
        if shutil.which("cargo", path=SYSTEM_PATH):
            self.skipTest("systemweites Cargo im PATH; der Launcher fände es hier zu Recht")
        vorher = set(Path("/tmp").glob(SMB_TEMP_GLOB))
        with tempfile.TemporaryDirectory() as raw:
            test_dir = Path(raw)
            process = self._lauf(test_dir, mit_cargo_env=False)
            self.assertFalse((test_dir / "smbd-aufruf").exists())

        self.assertEqual(process.returncode, 1, f"stderr:\n{process.stderr}")
        self.assertIn("cargo fehlt", process.stderr)
        self.assertNotIn("0-mal exakt gefunden", process.stderr)
        self.assertEqual(
            vorher,
            set(Path("/tmp").glob(SMB_TEMP_GLOB)),
            "Der Cargo-Fehler darf keinen SMB-Wegwerfbaum anlegen",
        )


    def test_abweichende_trefferzahl_bricht_vor_jeder_serverarbeit_ab(self) -> None:
        for treffer in (0, 2):
            vorher = set(Path("/tmp").glob(SMB_TEMP_GLOB))
            with self.subTest(treffer=treffer), tempfile.TemporaryDirectory() as raw:
                test_dir = Path(raw)
                process = self._lauf(test_dir, mit_cargo_env=True, treffer=treffer)
                self.assertEqual(process.returncode, 1, f"stderr:\n{process.stderr}")
                self.assertIn(f"{treffer}-mal exakt gefunden", process.stderr)
                self.assertNotIn("Haltepunkt", process.stdout)
                self.assertFalse((test_dir / "smbd-aufruf").exists())
            self.assertEqual(vorher, set(Path("/tmp").glob(SMB_TEMP_GLOB)))

    def test_der_eigentliche_lauf_bindet_den_test_exakt(self) -> None:
        quelle = SCRIPT.read_text(encoding="utf-8")
        self.assertIn('"$test_name" -- --exact --ignored', quelle)

if __name__ == "__main__":
    unittest.main()
