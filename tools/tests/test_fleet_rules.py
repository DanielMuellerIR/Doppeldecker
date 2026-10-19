"""Hält die beiden fleetweiten Regeln vom 2026-08-03 an der Quelle fest.

Regel 1: Nach ``/Applications`` gelangt ausschließlich ein Bundle mit angeheftetem
Notary-Ticket; ad hoc gebaut wird nur im Projektordner.

Regel 2: Kein absoluter Pfad des Build-Rechners darf im ausgelieferten Bundle landen;
ruft die App ein Programm auf, muss es im eigenen Bundle liegen und bundle-relativ
aufgelöst werden.

Beide Regeln sind im Code bereits umgesetzt. Dieser Test verhindert, dass sie beim
nächsten Umbau still verschwinden.

**Der Test liest nur Quelltext.** Er baut nicht, signiert nicht, notarisiert nicht und
fasst ``/Applications`` nicht an. Das ist keine Bequemlichkeit, sondern der Kern der
Sache: Ein Test, der die Sperre dadurch belegen wollte, dass er den echten
Installationsweg startet, würde dabei die installierte App löschen, neu bauen und eine
echte Notarisierung einreichen — genau der Schaden, vor dem die Regel schützt.
"""

from __future__ import annotations

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


def effective_lines(text: str) -> "list[tuple[int, str]]":
    """Alle Zeilen ohne Kommentar- und Leerzeilen, mit ihrer Nummer."""
    lines = []
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        lines.append((number, line))
    return lines


def line_of(text: str, needle: str) -> int:
    """Zeilennummer des ersten **wirksamen** Vorkommens; -1, wenn es keins gibt.

    Kommentarzeilen zählen bewusst nicht. Der Kopfkommentar von ``install``
    beschreibt die Prüfreihenfolge bereits in Prosa; würde er als Beleg gelten,
    könnte die echte Prüfung verschwinden oder hinter den ersten Schreibzugriff
    rutschen, ohne dass dieser Test es merkt.
    """
    for number, line in effective_lines(text):
        if needle in line:
            return number
    return -1


def defangs_failure(line: str) -> bool:
    """Entschärft die Zeile ihren eigenen Fehlschlag?

    ``xcrun stapler validate … || true`` steht an der richtigen Stelle und sperrt
    trotzdem nichts. Eine Reihenfolgeprüfung allein würde das durchgehen lassen.
    """
    return re.search(r"\|\|\s*(true|:|echo\b)", line) is not None


class InstallRequiresNotaryTicketTests(unittest.TestCase):
    """Regel 1 — ./install prüft das Ticket, bevor es irgendetwas schreibt."""

    def setUp(self) -> None:
        self.install = read("install")

    def test_ticket_is_checked_before_the_first_write(self) -> None:
        ticket = line_of(self.install, "xcrun stapler validate Doppeldecker.app")
        # Erster Schreibzugriff im Installationsverzeichnis: der Staging-Pfad.
        staged = line_of(self.install, 'transaction=$(mktemp -d "${install_dir}/')
        self.assertGreater(ticket, 0, "Ticket-Prüfung fehlt in ./install")
        self.assertGreater(staged, 0, "Staging-Schritt in ./install nicht gefunden")
        self.assertLess(
            ticket,
            staged,
            "./install schreibt ins Installationsverzeichnis, bevor das Ticket geprüft ist",
        )

    def test_signature_and_gatekeeper_are_checked_before_the_first_write(self) -> None:
        staged = line_of(self.install, 'transaction=$(mktemp -d "${install_dir}/')
        for needle, name in (
            ("codesign --verify --deep --strict Doppeldecker.app", "Codesignatur"),
            ("spctl -a -t exec -vv Doppeldecker.app", "Gatekeeper-Bewertung"),
        ):
            with self.subTest(pruefung=name):
                found = line_of(self.install, needle)
                self.assertGreater(found, 0, f"{name} fehlt in ./install")
                self.assertLess(found, staged, f"{name} läuft erst nach dem ersten Schreiben")

    def test_checks_do_not_defang_their_own_failure(self) -> None:
        for needle, name in (
            ("xcrun stapler validate Doppeldecker.app", "Ticket-Prüfung"),
            ("codesign --verify --deep --strict Doppeldecker.app", "Codesignatur"),
            ("spctl -a -t exec -vv Doppeldecker.app", "Gatekeeper-Bewertung"),
        ):
            with self.subTest(pruefung=name):
                treffer = [
                    line for _, line in effective_lines(self.install) if needle in line
                ]
                self.assertTrue(treffer, f"{name} fehlt in ./install")
                for line in treffer:
                    self.assertFalse(
                        defangs_failure(line),
                        f"{name} steht an der richtigen Stelle, sperrt aber nichts: {line.strip()}",
                    )

    def test_build_app_uses_the_notarizing_build_mode(self) -> None:
        # `build-release.py macos` baut OHNE Notarisierung. Nur `macos-install`
        # reicht die App bei Apple ein und heftet das Ticket an; genau dieser
        # Modus muss vor einer Installation laufen.
        build_app = read("build-app")
        self.assertIn("tools/build-release.py macos-install", build_app)
        self.assertNotIn("tools/build-release.py macos\n", build_app)

    def test_macos_install_mode_notarizes_and_staples(self) -> None:
        build_release = read("tools/build-release.py")
        self.assertIn("if is_install:\n        notarize_and_staple_app(profile)", build_release)
        self.assertIn('subprocess.run(("xcrun", "stapler", "staple", str(app)), check=True)', build_release)


class NoBuildMachinePathsInBundleTests(unittest.TestCase):
    """Regel 2 — keine absoluten Build-Mac-Pfade, App ruft nur eigenes Bundle auf."""

    def test_build_and_release_verify_local_paths(self) -> None:
        for entry in ("build-app", "release"):
            with self.subTest(einstieg=entry):
                # Nicht `assertIn`: Eine auskommentierte oder in Prosa erwähnte Zeile
                # wäre sonst ein gültiger Beleg für eine Prüfung, die gar nicht läuft.
                self.assertGreater(
                    line_of(read(entry), "python3 tools/verify-release-privacy.py"),
                    0,
                    f"./{entry} prüft die ausgelieferten Binaries nicht auf lokale Pfade",
                )

    def test_release_build_neutralises_local_prefixes(self) -> None:
        build_release = read("tools/build-release.py")
        self.assertIn("--remap-path-prefix", build_release)
        for label in ("CARGO_HOME", "RUSTUP_HOME"):
            self.assertIn(label, build_release)

    def test_privacy_check_covers_the_bundled_cli(self) -> None:
        # Das CLI wird als Sidecar mitgeliefert und muss genauso sauber sein wie
        # das GUI-Binary — es wurde im selben Lauf gebaut.
        privacy = read("tools/verify-release-privacy.py")
        self.assertIn("macos/Doppeldecker.app/Contents/MacOS/doppeldecker-cli", privacy)

    def test_bundled_cli_is_resolved_relative_to_the_running_binary(self) -> None:
        # Der Terminalbefehl zeigt per Symlink auf das CLI IM Bundle. Der Pfad
        # dorthin muss aus `current_exe()` entstehen; eine feste Zeichenkette wäre
        # der Pfad des Build-Rechners und auf fremden Macs tot.
        cli_setup = read("src-tauri/src/cli_setup.rs")
        self.assertIn("std::env::current_exe()", cli_setup)
        self.assertIn("bundle_cli: macos.join(CLI_NAME)", cli_setup)


if __name__ == "__main__":
    unittest.main()
