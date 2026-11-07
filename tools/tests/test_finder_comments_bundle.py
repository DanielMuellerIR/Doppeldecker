"""Finder-Kommentare brauchen dieselben Datenschutzangaben im Produkt und Testbundle."""
from pathlib import Path
import json
import plistlib
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class FinderCommentBundleTests(unittest.TestCase):
    def test_macos_bundle_declares_only_the_required_automation_entitlement(self):
        config = json.loads((ROOT / 'src-tauri/tauri.conf.json').read_text())
        entitlements = ROOT / 'src-tauri' / config['bundle']['macOS']['entitlements']
        with entitlements.open('rb') as source:
            self.assertEqual(plistlib.load(source), {'com.apple.security.automation.apple-events': True})
        with (ROOT / 'src-tauri/Info.plist').open('rb') as source:
            self.assertIn('Finder', plistlib.load(source)['NSAppleEventsUsageDescription'])

    def test_disposable_debug_bundle_inherits_the_product_privacy_declarations(self):
        launcher = (ROOT / 'tools/run-test-sandbox.sh').read_text()
        block = launcher.split("<<'PYPLIST'\n", 1)[1].split('\nPYPLIST', 1)[0]
        with tempfile.TemporaryDirectory() as area:
            output = Path(area) / 'Info.plist'
            subprocess.run([sys.executable, '-', str(output), area, str(ROOT / 'src-tauri/Info.plist')],
                           input=block, text=True, check=True, capture_output=True)
            with output.open('rb') as source:
                actual = plistlib.load(source)
            with (ROOT / 'src-tauri/Info.plist').open('rb') as source:
                declared = plistlib.load(source)
            for key, value in declared.items():
                self.assertEqual(actual[key], value)
            self.assertEqual(actual['CFBundleIdentifier'], 'com.doppeldecker.test')
            self.assertEqual(actual['LSEnvironment']['DOPPELDECKER_SANDBOX'], area)


if __name__ == '__main__':
    unittest.main()
