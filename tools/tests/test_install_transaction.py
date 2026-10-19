"""Installationsverhalten in /tmp: echte Swift-Atomik, künstliche Prüfprogramme.

Die Kopie des Installers baut nichts und startet weder Notarisierung noch eine App.
Alle Prüfprogramme außer Swift und der rein lesenden Versionsauslese sind Ersatzprogramme.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

FAKE = r'''#!/usr/bin/env python3
import json, os, pathlib, shutil, signal, sys
command = pathlib.Path(sys.argv[0]).name
with open(os.environ['EVENT_LOG'], 'a') as log:
    log.write(json.dumps([command, sys.argv[1:]]) + '\n')
if command == 'pgrep': sys.exit(1)
if command == 'ditto':
    shutil.copytree(sys.argv[1], sys.argv[2], symlinks=True)
    sys.exit(0)
path = pathlib.Path(sys.argv[-1])
phase = 'source' if str(path) == 'Doppeldecker.app' else ('staged' if '.Doppeldecker.install.' in str(path) else 'target')
if phase == 'target' and command == 'spctl' and os.environ.get('SWAP_TRANSACTION_BEFORE_OPEN'):
    transaction = next(path.parent.glob('.Doppeldecker.install.*'))
    transaction.rename(transaction.with_name(transaction.name + '.saved'))
    transaction.mkdir(mode=0o700)
    (transaction / 'foreign').write_text('do not remove')
    (transaction / 'transaction.swift').write_text('import Foundation\ntry! "foreign code".write(toFile: "' + str(path.parent / 'foreign-code-ran') + '", atomically: true, encoding: .utf8)\n')
if phase == 'target' and command == 'spctl' and os.environ.get('SIGNAL_TARGET'):
    os.kill(os.getppid(), signal.SIGTERM)
if phase == 'target' and command == 'spctl' and os.environ.get('BREAK_VERSION'):
    (path / 'Contents/Info.plist').write_text('invalid plist')
if phase == 'target' and command == 'xcrun' and os.environ.get('REPLACE_TARGET'):
    path.rename(path.with_name('saved-new.app'))
    path.mkdir()
    (path / 'foreign').write_text('do not remove')
    sys.exit(9)
if os.environ.get('FAIL_CHECK') == phase + '-' + command: sys.exit(8)
print('accepted')
'''


def executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(0o755)


@unittest.skipUnless(sys.platform == 'darwin', 'Installer verwendet macOS-Swift-Atomik')
class InstallTransactionTests(unittest.TestCase):
    def run_install(self, *, previous: bool = True, **faults: str):
        temporary = tempfile.TemporaryDirectory(prefix='doppeldecker-install-test-', dir='/tmp')
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        script = root / 'install'
        shutil.copy2(ROOT / 'install', script)
        executable(root / 'build-app', '#!/bin/sh\nexit 0\n')
        source = root / 'Doppeldecker.app'
        (source / 'Contents').mkdir(parents=True)
        with (source / 'Contents/Info.plist').open('wb') as stream:
            plistlib.dump({'CFBundleShortVersionString': '7.8.9'}, stream)
        (source / 'new').write_text('new version')
        install_root = root / 'installation'
        install_root.mkdir()
        if faults.get('FOREIGN_REMAINDER'):
            remainder = install_root / '.Doppeldecker.install.foreign'
            remainder.mkdir()
            (remainder / 'keep').write_text('foreign remainder')
        target = install_root / 'Doppeldecker.app'
        if previous:
            target.mkdir()
            (target / 'old').write_bytes(b'old version\0unchanged')
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        for command in ['xcrun', 'codesign', 'spctl', 'pgrep', 'ditto']:
            executable(bin_dir / command, FAKE)
        if faults.get('SIGNAL_SWAP'):
            # TERM trifft die wartende Shell, während der Helper noch läuft.
            # Danach führt er denselben echten Swift-Commit weiter aus; Rollback
            # muss auf dessen Ende warten und darf nicht mit ihm konkurrieren.
            wrapper = root / 'swift-wrapper'
            executable(wrapper, """#!/usr/bin/env python3
import os, signal, sys, time
if sys.argv[2] == 'commit':
    os.kill(os.getppid(), signal.SIGTERM)
    time.sleep(0.1)
os.execv('/usr/bin/swift', ['swift', *sys.argv[1:]])
""")
            script.write_text(script.read_text().replace('/usr/bin/swift', str(wrapper)))
        if faults.get('SWAP_TRANSACTION_AFTER_CHECK'):
            # Der Root-FD und die Abschlussidentitäten sind bereits gebunden.
            # Dann tauscht ein anderer Akteur nur den sichtbaren Wrappernamen aus.
            source_text = script.read_text()
            hook = '\n'.join([
                '    if phase == "finalize" {',
                '        try FileManager.default.moveItem(atPath: transaction, toPath: transaction + ".saved")',
                '        try FileManager.default.createDirectory(atPath: transaction, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])',
                '        try "do not remove".write(toFile: transaction + "/foreign", atomically: true, encoding: .utf8)',
                '    }',
            ])
            needle = '    if phase != "commit" {'
            assert source_text.count(needle) == 1
            script.write_text(source_text.replace(needle, hook + '\n' + needle))
        if faults.get('OLD_SYMLINK'):
            outside = root / 'outside'
            outside.mkdir()
            (outside / 'keep').write_text('outside contents')
            (target / 'external-link').symlink_to(outside, target_is_directory=True)
        log = root / 'events.jsonl'
        process = subprocess.run(
            [str(script)], cwd=root,
            env={**os.environ, 'PATH': str(bin_dir) + ':' + os.environ['PATH'],
                 'DOPPELDECKER_INSTALL_DIR': str(install_root), 'EVENT_LOG': str(log), **faults},
            capture_output=True, text=True, timeout=90,
        )
        return process, install_root, target, log

    def assert_no_transactions(self, install_root: Path):
        self.assertEqual(list(install_root.glob('.Doppeldecker.install.*')), [])

    def test_update_und_erstinstallation_werden_nach_vollstaendiger_pruefung_committed(self):
        for previous in [True, False]:
            with self.subTest(previous=previous):
                result, folder, target, log = self.run_install(previous=previous)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn('manuelle Recovery', result.stderr)
                self.assertNotIn('transaction.swift', result.stderr)
                self.assertEqual((target / 'new').read_text(), 'new version')
                self.assertIn('(7.8.9)', result.stdout)
                self.assert_no_transactions(folder)
                checks = [event for event in map(json.loads, log.read_text().splitlines()) if event[0] in ['xcrun', 'codesign', 'spctl']]
                self.assertEqual(len(checks), 9)
                self.assertTrue(all('Doppeldecker.app' == event[1][-1] for event in checks[:3]))
                self.assertTrue(all('.Doppeldecker.install.' in event[1][-1] for event in checks[3:6]))

    def test_fremde_installationsreste_bleiben_bei_erfolg_erhalten(self):
        result, folder, _, _ = self.run_install(FOREIGN_REMAINDER='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((folder / '.Doppeldecker.install.foreign/keep').read_text(), 'foreign remainder')

    def test_quellpruefung_stoppt_vor_erstem_installationsschreiben(self):
        result, folder, target, _ = self.run_install(FAIL_CHECK='source-xcrun')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((target / 'old').read_bytes(), b'old version\0unchanged')
        self.assert_no_transactions(folder)

    def test_defekter_stagingstand_ersetzt_keine_vorhandene_app(self):
        result, folder, target, _ = self.run_install(FAIL_CHECK='staged-codesign')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((target / 'old').read_bytes(), b'old version\0unchanged')
        self.assert_no_transactions(folder)

    def test_post_swap_fehler_stellt_altstand_wieder_her_oder_entfernt_erstinstallation(self):
        for previous in [True, False]:
            with self.subTest(previous=previous):
                result, folder, target, _ = self.run_install(previous=previous, FAIL_CHECK='target-xcrun')
                self.assertNotEqual(result.returncode, 0, result.stdout)
                if previous:
                    self.assertEqual((target / 'old').read_bytes(), b'old version\0unchanged')
                    self.assertFalse((target / 'new').exists())
                else:
                    self.assertFalse(target.exists())
                self.assert_no_transactions(folder)

    def test_fehlgeschlagene_versionsauslese_nimmt_austausch_zurueck(self):
        result, folder, target, _ = self.run_install(BREAK_VERSION='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((target / 'old').read_bytes(), b'old version\0unchanged')
        self.assert_no_transactions(folder)

    def test_term_nimmt_austausch_zurueck(self):
        result, folder, target, _ = self.run_install(SIGNAL_TARGET='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((target / 'old').read_bytes(), b'old version\0unchanged')
        self.assert_no_transactions(folder)

    def test_term_waehrend_swift_commit_wartet_vor_rollback(self):
        result, folder, target, _ = self.run_install(SIGNAL_SWAP='1')
        self.assertEqual(result.returncode, 143, result.stderr)
        self.assertEqual((target / 'old').read_bytes(), b'old version\0unchanged')
        self.assert_no_transactions(folder)

    def test_fremder_zielwechsel_bleibt_unberuehrt_und_backup_sichtbar(self):
        result, folder, target, _ = self.run_install(REPLACE_TARGET='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((target / 'foreign').read_text(), 'do not remove')
        transactions = list(folder.glob('.Doppeldecker.install.*'))
        self.assertEqual(len(transactions), 1)
        self.assertEqual((transactions[0] / 'Doppeldecker.app/old').read_bytes(), b'old version\0unchanged')
        self.assertIn('manuelle Recovery', result.stderr)
        self.assertIn(str(transactions[0]), result.stderr)


    def test_wrapperaustausch_vor_oeffnen_bleibt_unberuehrt_und_fuehrt_keinen_fremden_code_aus(self):
        result, folder, target, _ = self.run_install(SWAP_TRANSACTION_BEFORE_OPEN='1')
        self.assertNotEqual(result.returncode, 0)
        wrappers = list(folder.glob('.Doppeldecker.install.*'))
        original = next(path for path in wrappers if path.name.endswith('.saved'))
        foreign = next(path for path in wrappers if not path.name.endswith('.saved'))
        self.assertEqual((foreign / 'foreign').read_text(), 'do not remove')
        self.assertEqual((original / 'Doppeldecker.app/old').read_bytes(), b'old version\0unchanged')
        self.assertFalse((folder / 'foreign-code-ran').exists())
        self.assertEqual((target / 'new').read_text(), 'new version')
        self.assertIn('manuelle Recovery', result.stderr)

    def test_wrapperaustausch_nach_abschlusspruefung_lenkt_fd_cleanup_nicht_um(self):
        result, folder, target, _ = self.run_install(SWAP_TRANSACTION_AFTER_CHECK='1')
        self.assertNotEqual(result.returncode, 0)
        wrappers = list(folder.glob('.Doppeldecker.install.*'))
        original = next(path for path in wrappers if path.name.endswith('.saved'))
        foreign = next(path for path in wrappers if not path.name.endswith('.saved'))
        self.assertEqual((foreign / 'foreign').read_text(), 'do not remove')
        self.assertEqual(list(original.iterdir()), [])
        self.assertEqual((target / 'new').read_text(), 'new version')
        self.assertIn('manuelle Recovery', result.stderr)

    def test_cleanup_folgt_keinem_symlink_im_altbundle(self):
        result, folder, target, _ = self.run_install(OLD_SYMLINK='1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((folder.parent / 'outside/keep').read_text(), 'outside contents')
        self.assertEqual((target / 'new').read_text(), 'new version')
        self.assert_no_transactions(folder)


if __name__ == '__main__': unittest.main()
