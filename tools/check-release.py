#!/usr/bin/env python3
"""Prüft reproduzierbare Release-Invarianten ohne Artefakte oder Daten zu verändern."""

from __future__ import annotations

import base64
import json
import plistlib
import struct
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
REQUIRED_CSP = {
    "default-src 'self'",
    "script-src 'self'",
    "style-src 'self'",
    "connect-src ipc: http://ipc.localhost",
    "object-src 'none'",
}
UPDATER_ENDPOINT = (
    "https://github.com/DanielMuellerIR/Doppeldecker/"
    "releases/latest/download/latest.json"
)


def load_toml(path: str) -> dict:
    with (ROOT / path).open("rb") as handle:
        return tomllib.load(handle)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"Release-Prüfung fehlgeschlagen: {message}")


workspace = load_toml("Cargo.toml")
lock = load_toml("Cargo.lock")
tauri = json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())
package = json.loads((ROOT / "package.json").read_text())

version = workspace["workspace"]["package"]["version"]
require(tauri["version"] == version, "Tauri- und Workspace-Version unterscheiden sich")
require(package["version"] == version, "Node-Test-/Workspace-Version unterscheiden sich")
require(package["devDependencies"].get("@tauri-apps/cli") == "2.11.4", "Tauri CLI ist nicht exakt auf 2.11.4 gebunden")
require(package["devDependencies"].get("playwright") == "1.61.1", "Playwright ist nicht exakt gebunden")
macos_bundle = package["scripts"].get("bundle:macos", "")
public_macos_bundle = package["scripts"].get("bundle:macos:public", "")
linux_bundle = package["scripts"].get("bundle:linux", "")
for platform, command in (("macOS", macos_bundle), ("Linux", linux_bundle)):
    require("tools/build-release.py" in command, f"{platform}-Build umgeht die Pfadneutralisierung")
    require("tools/verify-release-privacy.py" in command, f"{platform}-Build prüft lokale Pfade nicht")
    require("tools/write-release-checksums.py" in command, f"{platform}-Build erzeugt keine Prüfsummen")
require("build-release.py macos-public" in public_macos_bundle, "öffentlicher macOS-Build nutzt nicht den sicheren Signierpfad")
require("verify-macos-release.sh" in public_macos_bundle, "öffentlicher macOS-Build prüft Signatur und Notarisierung nicht")
require("--allow-adhoc" not in public_macos_bundle, "öffentlicher macOS-Build erlaubt eine Ad-hoc-Signatur")
require("verify-release-privacy.py" in public_macos_bundle, "öffentlicher macOS-Build prüft lokale Pfade nicht")
require("write-release-checksums.py" in public_macos_bundle, "öffentlicher macOS-Build erzeugt keine Prüfsummen")
require("verify-linux-release.py" in linux_bundle, "Linux-Build prüft die Paketstruktur nicht")

workspace_packages = {
    entry["name"]: entry["version"]
    for entry in lock["package"]
    if entry["name"] in {"doppeldecker", "doppeldecker-core", "doppeldecker-cli"}
}
require(len(workspace_packages) == 3, "Cargo.lock enthält nicht alle drei Workspace-Pakete")
require(set(workspace_packages.values()) == {version}, "Cargo.lock enthält abweichende Paketversionen")

csp = tauri["app"]["security"]["csp"]
for directive in REQUIRED_CSP:
    require(directive in csp, f"CSP-Direktive fehlt: {directive}")
require("'unsafe-inline'" not in csp, "CSP erlaubt Inline-Code")
require(tauri["bundle"]["active"] is True, "Tauri-Bundling ist deaktiviert")
require(tauri["identifier"] == "com.doppeldecker.desktop", "Bundle-Identifier ist unerwartet")
# macOS 14 ist seit 2026-08-03 die einzige gültige Mindestversion: Der Release ist an
# aarch64 gebunden, dort setzt die Rust-Toolchain ohnehin bei 11.0 auf (die frühere
# 10.15 wurde still hochgeklammert), und getestet ist ausschließlich 14+.
require(tauri["bundle"]["macOS"]["minimumSystemVersion"] == "14.0", "macOS-Mindestversion ist nicht gebunden")
require(tauri["bundle"]["macOS"]["signingIdentity"] == "-", "lokale Ad-hoc-Signatur ist nicht konfiguriert")

# Der standardmäßig aktive Bonjour-Browser löst auf macOS eine lokale-
# Netzwerk-Freigabe aus. Zwecktext und exakt die drei tatsächlich gesuchten Dienste
# müssen deshalb schon in der Tauri-Mergequelle gebunden sein.
macos_info = plistlib.loads((ROOT / "src-tauri/Info.plist").read_bytes())
require(
    "Doppeldecker" in macos_info.get("NSLocalNetworkUsageDescription", ""),
    "macOS-LAN-Nutzungsbeschreibung fehlt",
)
require(
    macos_info.get("NSBonjourServices")
    == ["_smb._tcp", "_sftp-ssh._tcp", "_webdav._tcp"],
    "macOS-Bonjour-Dienste entsprechen nicht der aktiven Suche",
)

# Das Plugin des manuellen Updaters wird beim App-Start initialisiert. Fehlt dieser
# Block vollständig, ist der Build zwar erfolgreich, die fertige App beendet sich
# aber noch vor dem ersten Fenster. Darum gehört die exakte Laufzeitkonfiguration in
# den Release-Preflight, den auch ``./install`` vor Build und Notarisierung ausführt.
updater = tauri.get("plugins", {}).get("updater")
require(isinstance(updater, dict), "plugins.updater fehlt; die App könnte nicht starten")
try:
    updater_public_key = base64.b64decode(updater.get("pubkey", ""), validate=True).decode(
        "ascii"
    )
except (TypeError, ValueError, UnicodeDecodeError):
    updater_public_key = ""
require(
    "untrusted comment: minisign public key" in updater_public_key
    and "\nRW" in updater_public_key,
    "öffentlicher Updater-Schlüssel fehlt oder ist ungültig",
)
require(
    updater.get("endpoints") == [UPDATER_ENDPOINT],
    "Updater-Endpunkt ist nicht exakt an das HTTPS-Manifest gebunden",
)

build_release = (ROOT / "tools/build-release.py").read_text()
require(
    '"externalBin"' in build_release and "prepare_cli_sidecar" in build_release,
    "Release-Build bündelt doppeldecker-cli nicht als Tauri-Sidecar",
)

icon = ROOT / "src-tauri/icons/icon.png"
header = icon.read_bytes()[:24]
require(header[:8] == b"\x89PNG\r\n\x1a\n", "App-Icon ist kein PNG")
width, height = struct.unpack(">II", header[16:24])
require(
    width == 1024 and height == 1024,
    "App-Icon muss der gebündelten hicolor-Größe 1024×1024 entsprechen",
)

for required in ("LICENSE", "CHANGELOG.md", "README.md", "docs/release-plan.md"):
    require((ROOT / required).is_file(), f"öffentliche Datei fehlt: {required}")
changelog = (ROOT / "CHANGELOG.md").read_text()
require(f"## [{version}] - " in changelog, "aktuelle Version fehlt im Changelog")

# Nur der zentrale Helfer darf den nativen App-Konfigurationsordner auflösen. Er
# lenkt markierte GUI-Testläufe in deren Sandbox um und schützt so echte Profile,
# Queue-, Audit- und Recovery-Dateien vor Testmutationen.
config_offenders = [
    path.relative_to(ROOT)
    for path in (ROOT / "src-tauri/src").glob("*.rs")
    if path.name != "app_support.rs" and ".app_config_dir()" in path.read_text()
]
require(not config_offenders, f"direkter App-Konfigurationszugriff: {config_offenders}")

print(f"Release-Invarianten grün: Doppeldecker {version} · CSP strikt · Icon {width}×{height}")
