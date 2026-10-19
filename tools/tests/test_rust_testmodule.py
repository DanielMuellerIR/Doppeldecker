"""Verbietet zwei gleichnamige Module nebeneinander in derselben Rust-Datei.

`#[cfg(all(test, target_os = "linux"))] mod tests` neben `#[cfg(test)] mod tests`
sieht auf macOS harmlos aus: Dort ist nur eine der beiden Bedingungen wahr. Unter
Linux sind beide wahr, und der Build bricht mit E0428 ab, bevor ein einziger Test
läuft (Review-Fund 2026-09-04). Ein plattformabhängiger Test gehört deshalb als
einzelner Test mit `#[cfg(target_os = …)]` in das gemeinsame Modul.

Erlaubt bleibt der entgegengesetzte, seit jeher übliche Fall: gleichnamige Module,
deren `target_os`-Bedingungen sich nachweislich ausschließen — so trägt
`core/src/acl.rs` je ein `mod platform` für Linux und für macOS.

Die Prüfung läuft auf beiden Plattformen und braucht keinen Linux-Build.
"""

from __future__ import annotations

from collections import defaultdict
from itertools import combinations
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
# Nur Module ohne Einrückung: Geschwister auf oberster Dateiebene. Verschachtelte
# Module liegen in unterschiedlichen Elternmodulen und kollidieren nicht.
TOP_LEVEL_MODULE = re.compile(r"^mod ([A-Za-z_][A-Za-z0-9_]*) \{")
NEGIERTES_TARGET_OS = re.compile(r'not\s*\(\s*target_os\s*=\s*"([^"]+)"\s*\)')
TARGET_OS = re.compile(r'target_os\s*=\s*"([^"]+)"')


def versionierte_rust_dateien() -> list[Path]:
    ausgabe = subprocess.run(
        ["git", "ls-files", "*.rs"],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        text=True,
        check=True,
    ).stdout
    return [ROOT / zeile for zeile in ausgabe.splitlines() if zeile]


def plattformbedingung(attribute: str) -> str | None:
    """Der Plattformzweig eines Moduls, oder None ohne `target_os`-Bedingung.

    Ein negiertes Prädikat bekommt einen eigenen Schlüssel: `linux` und
    `not(linux)` schließen sich ebenso aus wie `linux` und `macos`.
    """
    # Komplexe Alternativen oder äußere Negation erlauben keinen Schluss aus
    # einem einzelnen gefundenen target_os. Unbekannt bleibt deshalb konservativ.
    if len(TARGET_OS.findall(attribute)) != 1 or re.search(r"\bany\s*\(", attribute):
        return None
    remaining = NEGIERTES_TARGET_OS.sub("", attribute)
    if re.search(r"\bnot\s*\(", remaining):
        return None
    negiert = NEGIERTES_TARGET_OS.search(attribute)
    if negiert:
        return f"!{negiert.group(1)}"
    einfach = TARGET_OS.search(attribute)
    return einfach.group(1) if einfach else None


def plattformen_schliessen_sich_aus(zweige: list[str | None]) -> bool:
    for left, right in combinations(zweige, 2):
        if left is None or right is None:
            return False
        if left.startswith("!") and right.startswith("!"):
            return False
        if left.startswith("!"):
            if left[1:] != right:
                return False
        elif right.startswith("!"):
            if right[1:] != left:
                return False
        elif left == right:
            return False
    return True


def modulzweige(quelle: str) -> dict[str, list[str | None]]:
    """Je Top-Level-Modulname die Plattformzweige seiner Definitionen."""
    zweige: dict[str, list[str | None]] = defaultdict(list)
    attribute: list[str] = []
    for zeile in quelle.splitlines():
        treffer = TOP_LEVEL_MODULE.match(zeile)
        if treffer:
            zweige[treffer.group(1)].append(plattformbedingung(" ".join(attribute)))
            attribute = []
        elif zeile.startswith("#["):
            # Zusammenhängender Attributblock direkt über dem Modul.
            attribute.append(zeile)
        elif zeile.strip():
            attribute = []
    return zweige


class RustModulnamenSindEindeutig(unittest.TestCase):
    def test_gleichnamige_module_brauchen_sich_ausschliessende_plattformen(self) -> None:
        verstoesse: list[str] = []
        for datei in versionierte_rust_dateien():
            for name, zweige in sorted(modulzweige(datei.read_text(encoding="utf-8")).items()):
                if len(zweige) < 2:
                    continue
                if plattformen_schliessen_sich_aus(zweige):
                    continue
                verstoesse.append(
                    f"{datei.relative_to(ROOT)}: mod {name} "
                    f"({len(zweige)}-mal, Plattformzweige {zweige})"
                )

        self.assertEqual(
            verstoesse,
            [],
            "Gleichnamige Geschwistermodule brechen den Build, sobald beide "
            "cfg-Bedingungen zugleich gelten:\n" + "\n".join(verstoesse),
        )

    def test_negierte_plattformzweige_duerfen_sich_nicht_ueberlappen(self) -> None:
        for left, right, exclusive in [
            ('target_os = "linux"', 'target_os = "macos"', True),
            ('not(target_os = "linux")', 'target_os = "linux"', True),
            ('not(target_os = "linux")', 'target_os = "macos"', False),
            ('not(target_os = "linux")', 'not(target_os = "macos")', False),
            ('any(test, target_os = "linux")', 'target_os = "macos"', False),
            ('not(all(test, target_os = "linux"))', 'target_os = "macos"', False),
        ]:
            with self.subTest(left=left, right=right):
                source = f"#[cfg({left})]\nmod duplicate {{\n}}\n#[cfg({right})]\nmod duplicate {{\n}}\n"
                self.assertEqual(
                    plattformen_schliessen_sich_aus(modulzweige(source)["duplicate"]),
                    exclusive,
                )


if __name__ == "__main__":
    unittest.main()
