"""Echte Prozessgrenzen ohne Browserfenster oder Zugriff auf Nutzerdaten."""

from pathlib import Path
import json
import os
import select
import shutil
import signal
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
NODE = shutil.which("node")


@unittest.skipUnless(NODE, "Node fehlt")
class BrowserCleanupTests(unittest.TestCase):
    def run_fixture(self, mode: str, signum: int | None = None) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = Path(raw)
            server = directory / "server.mjs"
            server.write_text(
                'process.on("SIGTERM", () => {});\n'
                'console.log("ready");\nsetInterval(() => {}, 1000);\n'
            )
            runner = directory / "runner.mjs"
            runner.write_text(
                f'import {{ createBrowserCleanup }} from {json.dumps((ROOT / "tools/browser-cleanup.mjs").as_uri())};\n'
                'import { spawn } from "node:child_process";\n'
                'import { once } from "node:events";\n'
                f'const server = spawn(process.execPath, [{json.dumps(str(server))}]);\n'
                'const cleanup = createBrowserCleanup(server);\n'
                'await once(server.stdout, "data");\n'
                f'const mode = {json.dumps(mode)};\n'
                'const browserType = { launch: () => mode === "pending"\n'
                '  ? new Promise(() => {})\n'
                '  : Promise.resolve({ close: async () => {\n'
                '    if (mode === "close-error") throw new Error("expected-close-error");\n'
                '  } }) };\n'
                'void cleanup.launch(browserType, {});\n'
                'console.log(JSON.stringify({ pid: server.pid }));\n'
                'if (["normal", "close-error"].includes(mode)) {\n'
                '  let failure;\n'
                '  try { await cleanup.close(); } catch (error) { failure = error; }\n'
                '  if (mode === "close-error" ? failure?.message !== "expected-close-error" : failure) {\n'
                '    throw failure || new Error("Browserfehler wurde verschluckt");\n'
                '  }\n'
                '} else setInterval(() => {}, 1000);\n'
            )
            with (directory / "runner.log").open("wb") as log:
                parent = subprocess.Popen(
                    [NODE, str(runner)], start_new_session=True,
                    stdout=subprocess.PIPE, stderr=log,
                )
                try:
                    readable, _, _ = select.select([parent.stdout], [], [], 10)
                    self.assertTrue(readable, "Fixture wurde nicht bereit")
                    line = parent.stdout.readline()
                    self.assertTrue(line, "Fixture endete ohne Bereitschaft")
                    info = json.loads(line)
                    self.assertEqual(os.getpgid(info["pid"]), parent.pid)
                    if signum is not None:
                        os.kill(parent.pid, signum)
                    self.assertEqual(parent.wait(timeout=12), 128 + signum if signum else 0)
                    status = subprocess.run(
                        ["ps", "-p", str(info["pid"]), "-o", "stat="],
                        capture_output=True, text=True, check=False,
                    ).stdout.strip()
                    self.assertTrue(not status or status.startswith("Z"),
                                    f"Eigener Mock-Server läuft weiter: {status}")
                finally:
                    try:
                        os.killpg(parent.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    parent.wait(timeout=5)
                    parent.stdout.close()

    def test_normaler_abschluss_beendet_signalignorierenden_server(self) -> None:
        self.run_fixture("normal")

    def test_browser_schliessfehler_beendet_server_und_bleibt_sichtbar(self) -> None:
        self.run_fixture("close-error")

    def test_abbruch_beendet_browser_und_signalignorierenden_server(self) -> None:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=signum):
                self.run_fixture("signal", signum)

    def test_abbruch_raeumt_server_auch_bei_wartendem_browserstart_auf(self) -> None:
        self.run_fixture("pending", signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()
