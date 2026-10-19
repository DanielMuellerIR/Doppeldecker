"""Prüft den CI-Vertrag als YAML-Struktur statt mit Texttreffern."""

from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]


class CiYamlStructureTests(unittest.TestCase):
    def test_workflow_ist_parsebar_und_beide_matrixziele_besitzen_das_gate(self) -> None:
        script = r"""
import fs from "node:fs";
import { parseDocument } from "yaml";

const text = fs.readFileSync(process.argv[1], "utf8");
const document = parseDocument(text);
if (document.errors.length) {
  throw document.errors[0];
}
const workflow = document.toJS();
if (workflow.permissions?.contents !== "read") process.exit(9);
const gate = workflow.jobs?.gates;
if (!gate || gate["runs-on"] !== "${{ matrix.runner }}") process.exit(10);
if (gate.strategy?.["fail-fast"] !== false) process.exit(15);
const rows = gate.strategy?.matrix?.include;
if (!Array.isArray(rows)) process.exit(11);
const runners = new Set(rows.map((row) => row.runner));
if (!runners.has("macos-15") || !runners.has("ubuntu-24.04")) process.exit(12);
const steps = gate.steps;
if (!Array.isArray(steps)) process.exit(13);
const sharedGate = steps.filter((step) => step.run === "python3 tools/check-gates.py --json");
if (sharedGate.length !== 1) process.exit(14);
const linuxPackages = steps.find((step) => step.name === "Tauri-Systemabhängigkeiten installieren");
if (linuxPackages?.if !== "runner.os == 'Linux'") process.exit(16);
if (!linuxPackages.run.includes("libwebkit2gtk-4.1-dev")) process.exit(17);
const linuxBrowser = steps.find((step) => step.run === "pnpm exec playwright install --with-deps chromium");
if (linuxBrowser?.if !== "runner.os == 'Linux'") process.exit(18);
const macBrowser = steps.find((step) => step.run === "pnpm exec playwright install chromium");
if (macBrowser?.if !== "runner.os == 'macOS'") process.exit(19);
"""
        result = subprocess.run(
            ("node", "--input-type=module", "-e", script, ROOT / ".github/workflows/ci.yml"),
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr or result.stdout)


if __name__ == "__main__":
    unittest.main()
