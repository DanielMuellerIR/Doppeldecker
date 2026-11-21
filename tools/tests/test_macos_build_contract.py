"""Quelltextprüfungen für den macOS-Build und dessen Bundlepfade."""

from __future__ import annotations

from pathlib import Path
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


class NotarizedBuildTests(unittest.TestCase):
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
    """Die App verwendet ihr eigenes Bundle und prüft ausgelieferte Pfade."""

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
