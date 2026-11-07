#!/usr/bin/env python3
"""Stoppt, wenn ein Release-Paket lokale Build-Pfade offenlegt."""

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
    """Extrahiert beide Linux-Pakete und erfasst auch Hooks und Symlinks."""
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
    binaries = [
        ("DEB", deb_root / "usr/bin/doppeldecker"),
        ("DEB-CLI", deb_root / "usr/bin/doppeldecker-cli"),
        ("AppImage", root / "squashfs-root/usr/bin/doppeldecker"),
        ("AppImage-CLI", root / "squashfs-root/usr/bin/doppeldecker-cli"),
    ]
    return binaries + runtime_entries(deb_root, "DEB-Datei") + runtime_entries(
        root / "squashfs-root", "AppImage-Datei"
    )


def runtime_entries(root: Path, label: str) -> list[tuple[str, Path]]:
    return [
        (label, path)
        for path in sorted(root.rglob("*"))
        if path.is_symlink() or path.is_file()
    ]


def scan_entries(entries: list[tuple[str, Path]], prefixes: dict[str, Path]) -> int:
    checked = set()
    for label, path in entries:
        if path in checked:
            continue
        checked.add(path)
        if path.is_symlink():
            contents = os.fsencode(os.readlink(path))
        elif path.is_file():
            contents = path.read_bytes()
        else:
            raise SystemExit(f"Release-Datenschutzprüfung: {label} fehlt.")
        for prefix_label, prefix in prefixes.items():
            if os.fsencode(prefix) in contents:
                raise SystemExit(
                    "Release-Datenschutzprüfung fehlgeschlagen: lokaler "
                    f"{prefix_label} ist in {label} ({path.name}) enthalten."
                )
    return len(checked)


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
        binaries.append(
            (
                "macOS-Metadatenhelfer",
                BUNDLE_ROOT
                / "macos/Doppeldecker.app/Contents/Library/LaunchServices/doppeldecker-metadata-helper",
            )
        )
        binaries.extend(runtime_entries(BUNDLE_ROOT / "macos/Doppeldecker.app", "macOS-Datei"))
        yield binaries
    else:
        yield binaries


def main() -> None:
    with release_binaries() as binaries:
        count = scan_entries(binaries, local_prefixes())
    print(f"Release-Datenschutzprüfung grün: {count} Dateien/Symlinks ohne lokale Build-Pfade")


if __name__ == "__main__":
    main()
