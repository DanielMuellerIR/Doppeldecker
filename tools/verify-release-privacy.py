#!/usr/bin/env python3
"""Stoppt, wenn ein Release-Binary lokale Build-Pfade offenlegt."""

from __future__ import annotations

from contextlib import contextmanager
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/doppeldecker"
BUNDLE_ROOT = ROOT / "target/release/bundle"
with (ROOT / "Cargo.toml").open("rb") as handle:
    VERSION = tomllib.load(handle)["workspace"]["package"]["version"]


def local_prefixes() -> dict[str, Path]:
    home = Path.home().resolve()
    return {
        "Workspace": ROOT.resolve(),
        "Benutzerordner": home,
        "Cargo-Ordner": Path(os.environ.get("CARGO_HOME", home / ".cargo")).resolve(),
        "Rustup-Ordner": Path(os.environ.get("RUSTUP_HOME", home / ".rustup")).resolve(),
    }


def single_artifact(pattern: str, label: str) -> Path:
    artifacts = list(BUNDLE_ROOT.glob(pattern))
    if len(artifacts) != 1:
        raise SystemExit(f"Release-Datenschutzprüfung: genau ein {label} wurde erwartet.")
    return artifacts[0]


def linux_bundle_binaries(root: Path) -> list[tuple[str, Path]]:
    """Extrahiert beide Linux-Pakete getrennt und liefert ihre echten Binaries."""
    deb = single_artifact(f"deb/Doppeldecker_{VERSION}_*.deb", "DEB")
    appimage = single_artifact(
        f"appimage/Doppeldecker_{VERSION}_*.AppImage", "AppImage"
    )
    deb_root = root / "deb"
    subprocess.run(
        ("dpkg-deb", "-x", deb, deb_root),
        check=True,
        stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        (appimage, "--appimage-extract"),
        cwd=root,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    return [
        ("DEB", deb_root / "usr/bin/doppeldecker"),
        ("DEB-CLI", deb_root / "usr/bin/doppeldecker-cli"),
        ("AppImage", root / "squashfs-root/usr/bin/doppeldecker"),
        ("AppImage-CLI", root / "squashfs-root/usr/bin/doppeldecker-cli"),
    ]


@contextmanager
def release_binaries():
    """Hält temporär extrahierte Paketbäume bis zum Scan zuverlässig am Leben."""
    binaries = [("Release", BINARY)]
    if platform.system() == "Linux":
        with tempfile.TemporaryDirectory(prefix="doppeldecker-release-privacy-") as temporary:
            binaries.extend(linux_bundle_binaries(Path(temporary)))
            yield binaries
    elif platform.system() == "Darwin":
        binaries.append(
            (
                "macOS-App",
                BUNDLE_ROOT / "macos/Doppeldecker.app/Contents/MacOS/doppeldecker",
            )
        )
        binaries.append(
            (
                "macOS-App-CLI",
                BUNDLE_ROOT
                / "macos/Doppeldecker.app/Contents/MacOS/doppeldecker-cli",
            )
        )
        yield binaries
    else:
        yield binaries

with release_binaries() as binaries:
    for bundle_label, binary in binaries:
        if not binary.is_file():
            raise SystemExit(f"Release-Datenschutzprüfung: {bundle_label}-Binary fehlt.")
        contents = binary.read_bytes()
        for prefix_label, prefix in local_prefixes().items():
            if os.fsencode(prefix) in contents:
                raise SystemExit(
                    "Release-Datenschutzprüfung fehlgeschlagen: lokaler "
                    f"{prefix_label} ist im {bundle_label}-Binary enthalten."
                )

    labels = ", ".join(label for label, _ in binaries)
    print(f"Release-Datenschutzprüfung grün: keine lokalen Build-Pfade in {labels}")
