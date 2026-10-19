from __future__ import annotations

from pathlib import Path
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from module_loader import load_tool_module


ROOT = Path(__file__).resolve().parents[2]
CHECK_GATES = load_tool_module("check_gates", "check-gates.py")


class DirectoryMeasurementTests(unittest.TestCase):
    def test_missing_directory_uses_no_space(self) -> None:
        missing = ROOT / "target" / "definitely-missing-cache-test"
        self.assertEqual(CHECK_GATES.measure_directory_kib(missing), 0)

    def test_du_result_is_parsed_as_kib(self) -> None:
        completed = subprocess.CompletedProcess(
            args=("du", "-sk"),
            returncode=0,
            stdout="12345\tcache\n",
            stderr="",
        )
        with (
            mock.patch.object(Path, "exists", return_value=True),
            mock.patch.object(CHECK_GATES.subprocess, "run", return_value=completed),
        ):
            self.assertEqual(
                CHECK_GATES.measure_directory_kib(Path("cache")),
                12345,
            )


class CacheLimitTests(unittest.TestCase):
    def setUp(self) -> None:
        human_patch = mock.patch.object(CHECK_GATES, "human")
        human_patch.start()
        self.addCleanup(human_patch.stop)

    def test_cache_below_limit_is_kept(self) -> None:
        clean = mock.Mock(return_value=0)
        result = CHECK_GATES.enforce_debug_cache_limit(
            False,
            measure_fn=lambda _path: 5 * CHECK_GATES.KIB_PER_GIB,
            clean_fn=clean,
        )
        self.assertEqual(result["status"], "within-limit")
        clean.assert_not_called()

    def test_cache_above_limit_cleans_only_once(self) -> None:
        sizes = iter((7 * CHECK_GATES.KIB_PER_GIB, 0))
        clean = mock.Mock(return_value=0)
        result = CHECK_GATES.enforce_debug_cache_limit(
            False,
            measure_fn=lambda _path: next(sizes),
            clean_fn=clean,
        )
        self.assertEqual(result["status"], "cleaned")
        self.assertEqual(result["after_gib"], 0)
        clean.assert_called_once_with()

    def test_failed_cleanup_is_reported(self) -> None:
        sizes = iter((7 * CHECK_GATES.KIB_PER_GIB, 7 * CHECK_GATES.KIB_PER_GIB))
        result = CHECK_GATES.enforce_debug_cache_limit(
            False,
            measure_fn=lambda _path: next(sizes),
            clean_fn=lambda: 101,
        )
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["rc"], 1)


class GateSelectionTests(unittest.TestCase):
    def test_tool_teilgate_installiert_zuerst_node_abhaengigkeiten(self) -> None:
        selected = [name for name, _command in CHECK_GATES.select_gates(["test-tools"], [])]
        self.assertEqual(selected, ["pnpm-install", "test-tools"])

    def test_erforderliche_installation_kann_nicht_ausgelassen_werden(self) -> None:
        with self.assertRaisesRegex(ValueError, "test-tools.*pnpm-install"):
            CHECK_GATES.select_gates(["test-tools"], ["pnpm-install"])

    def test_ausgelassene_abhaengige_gates_ziehen_keine_installation_nach(self) -> None:
        selected = [
            name
            for name, _command in CHECK_GATES.select_gates(
                [], ["test-tools", "test-ui", "pnpm-install"]
            )
        ]

        self.assertEqual(
            selected,
            ["fmt", "test-core", "test-cli", "test-tauri", "clippy", "fuzz-check"],
        )


class GateCoverageTests(unittest.TestCase):
    """Ein Gate muss das prüfen, was sein Name behauptet."""

    def gate(self, name: str) -> list[str]:
        for gate_name, command in CHECK_GATES.GATES:
            if gate_name == name:
                return list(command)
        self.fail(f"Gate {name} fehlt")

    def test_fuzz_gate_uebersetzt_auch_die_tests_der_fuzz_kiste(self) -> None:
        # Ohne `--all-targets` waehlt `cargo check` nur Lib und Bins. `fuzz/` liegt
        # ausserhalb des Workspace, also faellt `fuzz/tests/seed_corpus.rs` sonst
        # durch jedes Gate und die versionierten Seeds veralten unbemerkt.
        self.assertIn("--all-targets", self.gate("fuzz-check"))
        self.assertIn("--locked", self.gate("fuzz-check"))

    def test_seed_regressionstest_existiert_ueberhaupt(self) -> None:
        seeds = ROOT / "fuzz" / "tests" / "seed_corpus.rs"
        self.assertTrue(seeds.is_file(), "Der Seed-Regressionstest der Fuzz-Kiste fehlt")


class SelfCollectionTests(unittest.TestCase):
    """Der Direktaufruf dieser Datei muss alle ihre Tests einsammeln.

    Stand ``unittest.main()`` in der Mitte, beendete sich der Direktaufruf dort und
    die danach definierten Klassen liefen nie — beim Gate ueber ``unittest discover``
    dagegen schon. Ein solcher Unterschied verdeckt genau die Tests, die jemand beim
    Debuggen einzeln laufen laesst.
    """

    def test_unittest_main_steht_am_ende(self) -> None:
        zeilen = [
            zeile.rstrip()
            for zeile in (ROOT / "tools" / "tests" / "test_check_gates.py")
            .read_text(encoding="utf-8")
            .splitlines()
            if zeile.strip()
        ]
        self.assertEqual(zeilen[-2:], ['if __name__ == "__main__":', "    unittest.main()"])


class SignalNoteTests(unittest.TestCase):
    """Ein per Signal beendetes Gate ist kein fehlgeschlagener Test.

    Ohne diese Unterscheidung liest sich ein von außen abgebrochener Lauf wie
    eine unzuverlässige Testsuite, und die Suche beginnt an der falschen Stelle.
    """

    def test_normal_exit_codes_have_no_signal_note(self) -> None:
        self.assertIsNone(CHECK_GATES.signal_note(0))
        self.assertIsNone(CHECK_GATES.signal_note(1))
        self.assertIsNone(CHECK_GATES.signal_note(127))

    def test_negative_return_code_names_the_signal(self) -> None:
        self.assertEqual(CHECK_GATES.signal_note(-15), "SIGTERM")
        self.assertEqual(CHECK_GATES.signal_note(-9), "SIGKILL")

    def test_unknown_signal_number_stays_readable(self) -> None:
        self.assertEqual(CHECK_GATES.signal_note(-9999), "Signal 9999")


class GateProcessTests(unittest.TestCase):
    def test_abbruch_beendet_auch_signalignorierende_nachkommen(self) -> None:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=signum), tempfile.TemporaryDirectory() as raw:
                directory = Path(raw)
                ready = directory / "ready.json"
                grandchild_ready = directory / "grandchild"
                grandchild = (
                    "import os, signal, time; from pathlib import Path; "
                    "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                    "signal.signal(signal.SIGINT, signal.SIG_IGN); "
                    "signal.signal(signal.SIGHUP, signal.SIG_IGN); "
                    f"Path({str(grandchild_ready)!r}).write_text(str(os.getpid())); "
                    "time.sleep(30)"
                )
                child = (
                    "import json, os, subprocess, sys, time; from pathlib import Path; "
                    f"p=subprocess.Popen([sys.executable, '-c', {grandchild!r}]); "
                    f"ready=Path({str(grandchild_ready)!r})\n"
                    "while not ready.exists(): time.sleep(.01)\n"
                    f"Path({str(ready.with_suffix('.tmp'))!r}).write_text(json.dumps("
                    "{'pid':os.getpid(),'grandchild':p.pid,'pgid':os.getpgrp()})); "
                    f"os.replace({str(ready.with_suffix('.tmp'))!r}, {str(ready)!r}); "
                    "p.wait()"
                )
                runner = (
                    "import importlib.util; from pathlib import Path; "
                    f"s=importlib.util.spec_from_file_location('gates', {str(ROOT / 'tools/check-gates.py')!r}); "
                    "m=importlib.util.module_from_spec(s); s.loader.exec_module(m); "
                    f"m.ROOT=Path({raw!r}); m.LOG_DIR=m.ROOT/'logs'; "
                    f"m.run_gate('signal-test', {[sys.executable, '-c', child]!r}, True)"
                )
                with (directory / "runner.log").open("wb") as log:
                    parent = subprocess.Popen(
                        [sys.executable, "-c", runner], start_new_session=True,
                        stdout=log, stderr=subprocess.STDOUT,
                    )
                    info = None
                    try:
                        deadline = time.monotonic() + 10
                        while not ready.exists():
                            self.assertIsNone(parent.poll(), "Runner endete vor Testbereitschaft")
                            self.assertLess(time.monotonic(), deadline)
                            time.sleep(.01)
                        info = json.loads(ready.read_text())
                        os.kill(parent.pid, signum)
                        self.assertEqual(parent.wait(timeout=10), 128 + signum)
                        for pid in (info["pid"], info["grandchild"]):
                            status = subprocess.run(
                                ["ps", "-p", str(pid), "-o", "stat="],
                                capture_output=True, text=True, check=False,
                            ).stdout.strip()
                            self.assertTrue(not status or status.startswith("Z"),
                                            f"Nachkomme {pid} läuft weiter: {status}")
                    finally:
                        # Altcode scheitert vor der neuen Sitzung: Beide möglichen
                        # Gruppen sind ausschließlich eigene künstliche Prozesse.
                        groups = {parent.pid}
                        if info:
                            self.assertIn(info["pgid"], (parent.pid, info["pid"]))
                            groups.add(info["pgid"])
                        for group in groups:
                            try:
                                os.killpg(group, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                        parent.wait(timeout=5)

    def test_normaler_befehl_behaelt_seinen_exitcode(self) -> None:
        with tempfile.TemporaryFile() as log:
            for code in (0, 7):
                self.assertEqual(CHECK_GATES.run_command(
                    [sys.executable, "-c", f"raise SystemExit({code})"], log,
                ), code)


if __name__ == "__main__":
    unittest.main()
