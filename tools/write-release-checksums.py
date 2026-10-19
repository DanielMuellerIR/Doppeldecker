#!/usr/bin/env python3
"""Schreibt reproduzierbare SHA-256-Zeilen für fertige Release-Dateien."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import tomllib


ROOT = Path(__file__).resolve().parents[1]
BUNDLE_ROOT = ROOT / "target/release/bundle"
OUTPUT = BUNDLE_ROOT / "SHA256SUMS"
PACKAGE_SUFFIXES = {".dmg", ".deb", ".AppImage"}
with (ROOT / "Cargo.toml").open("rb") as handle:
    VERSION = tomllib.load(handle)["workspace"]["package"]["version"]


def is_release_artifact(path: Path) -> bool:
    if path.name == "latest.json":
        try:
            return json.loads(path.read_text()).get("version") == VERSION
        except (OSError, json.JSONDecodeError):
            return False
    if path.name.endswith((".app.tar.gz", ".app.tar.gz.sig")):
        return True
    if path.name.endswith((".AppImage.sig", ".deb.sig")):
        return f"_{VERSION}_" in path.name
    return path.suffix in PACKAGE_SUFFIXES and f"_{VERSION}_" in path.name


artifacts = sorted(
    path for path in BUNDLE_ROOT.rglob("*")
    if path.is_file() and is_release_artifact(path)
)
if not artifacts:
    raise SystemExit(
        f"Keine Release-Artefakte für Version {VERSION} gefunden."
    )

lines = []
for artifact in artifacts:
    digest = hashlib.sha256()
    with artifact.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    lines.append(f"{digest.hexdigest()}  {artifact.relative_to(BUNDLE_ROOT)}")

temporary = OUTPUT.with_suffix(".tmp")
temporary.write_text("\n".join(lines) + "\n")
os.replace(temporary, OUTPUT)
print(f"{len(lines)} Prüfsumme(n) geschrieben: {OUTPUT.relative_to(ROOT)}")
