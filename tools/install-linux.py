#!/usr/bin/env python3
"""Installiert ein geprüftes AppImage benutzereigen an einem stabilen Updatepfad."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def desktop_value(value: str) -> str:
    # Desktop Entry und Exec verwenden getrennte Escape-Regeln.
    return value.replace("\\", "\\\\").replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")


def exec_value(path: Path) -> str:
    value = str(path).replace("%", "%%")
    for character in ("\\", '"', "`", "$"):
        value = value.replace(character, "\\" + character)
    return desktop_value('"' + value + '"')


def install(source: Path, prefix: Path, expected: str | None = None) -> dict:
    source = source.resolve(strict=True)
    if not source.is_file():
        raise ValueError("AppImage ist keine reguläre Datei")
    with source.open("rb") as stream:
        header = stream.read(20)
    if (len(header) != 20 or header[:6] != b"\x7fELF\x02\x01"
            or header[8:11] != b"AI\x02" or int.from_bytes(header[18:20], "little") != 62):
        raise ValueError("Ein x86_64-AppImage vom Typ 2 wird benötigt")
    source_digest = digest(source)
    if expected is not None and source_digest != expected.lower():
        raise ValueError("AppImage-Prüfsumme stimmt nicht überein")
    prefix = prefix.expanduser().resolve()
    app_dir = prefix / "lib/doppeldecker"
    app = app_dir / "Doppeldecker.AppImage"
    launcher = prefix / "bin/doppeldecker"
    desktop = prefix / "share/applications/com.doppeldecker.desktop.desktop"
    icon = app_dir / "doppeldecker.png"
    manifest = app_dir / "installation.json"
    paths = (app, launcher, desktop, icon, manifest)
    # Ein erster Aufruf darf keinen gleichnamigen fremden Befehl vereinnahmen.
    if not manifest.exists() and any(path.exists() or path.is_symlink() for path in paths):
        raise ValueError("Installationspfad ist bereits belegt; vorhandenen Bestand prüfen")
    for path in paths:
        if path.is_symlink() or path.exists() and not path.is_file():
            raise ValueError(f"Installationsdatei ist verlinkt oder ungültig: {path}")
        path.parent.mkdir(parents=True, exist_ok=True)
    if manifest.exists():
        previous = json.loads(manifest.read_text())
        if previous.get("schema") != 1 or previous.get("appimage") != str(app):
            raise ValueError("Vorhandene Installation ist nicht diesem Installer zugeordnet")
        for path in (launcher, desktop, icon):
            if not path.is_file() or digest(path) != previous.get("managed", {}).get(str(path)):
                raise ValueError(f"Installationsdatei wurde anderweitig geändert: {path}")
    stage = Path(tempfile.mkdtemp(prefix=".install-", dir=app_dir))
    cleanup = True
    try:
        staged_app = stage / "Doppeldecker.AppImage"
        shutil.copyfile(source, staged_app)
        staged_app.chmod(0o755)
        if digest(staged_app) != source_digest:
            raise ValueError("AppImage änderte sich während des Kopierens")
        staged_icon = stage / "doppeldecker.png"
        shutil.copyfile(ROOT / "src-tauri/icons/icon.png", staged_icon)
        staged_icon.chmod(0o644)
        staged_launcher = stage / "launcher"
        # Shell-Quoting gilt nur für diesen fest erzeugten Dateipfad, nie für argv.
        staged_launcher.write_text(f'#!/bin/sh\nexec {shlex.quote(str(app))} "$@"\n')
        staged_launcher.chmod(0o755)
        staged_desktop = stage / "desktop"
        staged_desktop.write_text(
            "[Desktop Entry]\nType=Application\nName=Doppeldecker\n"
            "Comment=Dateien in zwei Ansichten verwalten\n"
            f"Exec={exec_value(app)}\nIcon={desktop_value(str(icon))}\n"
            "Terminal=false\nCategories=System;FileTools;FileManager;\nStartupWMClass=Doppeldecker\n"
        )
        staged_desktop.chmod(0o644)
        replacements = [(app, staged_app), (launcher, staged_launcher), (desktop, staged_desktop), (icon, staged_icon)]
        state = {"schema": 1, "appimage": str(app), "sha256": source_digest,
                 "managed": {str(path): digest(staged) for path, staged in replacements if path != app}}
        staged_manifest = stage / "installation.json"
        staged_manifest.write_text(json.dumps(state, indent=2) + "\n")
        staged_manifest.chmod(0o644)
        replacements.append((manifest, staged_manifest))
        backups = {}
        installed = []
        try:
            for index, (path, staged) in enumerate(replacements):
                if path.exists():
                    backup = stage / f"backup-{index}"
                    shutil.copy2(path, backup)
                    backups[path] = backup
                new_digest = digest(staged)
                os.replace(staged, path)
                installed.append((path, new_digest))
                if digest(path) != new_digest:
                    raise OSError(f"Installationsprüfung fehlgeschlagen: {path}")
        except BaseException:
            cleanup = False
            for path, installed_digest in reversed(installed):
                if path.is_symlink() or not path.is_file() or digest(path) != installed_digest:
                    raise RuntimeError(f"Installationsbestand wurde verändert; Sicherung bleibt in {stage}: {path}")
                if path in backups:
                    os.replace(backups[path], path)
                else:
                    path.unlink()
            cleanup = True
            raise
    finally:
        if cleanup:
            shutil.rmtree(stage)
    updater = shutil.which("update-desktop-database")
    if updater:
        subprocess.run([updater, str(desktop.parent)], check=False)
    return state


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--appimage", type=Path, required=True)
    parser.add_argument("--prefix", type=Path, default=Path.home() / ".local")
    parser.add_argument("--sha256", help="Vorher unabhängig geprüfte SHA-256-Prüfsumme")
    args = parser.parse_args()
    try:
        result = install(args.appimage, args.prefix, args.sha256)
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"Installation fehlgeschlagen: {error}\n")
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
