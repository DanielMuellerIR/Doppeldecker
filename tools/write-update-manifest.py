#!/usr/bin/env python3
"""Verbindet die getrennt gebauten macOS-/Linux-Signaturen zu ``latest.json``.

Das Skript läuft erst, nachdem beide Plattformartefakte in ``target/release/bundle``
gesammelt wurden. Es signiert nichts und liest keinen privaten Schlüssel; die
Tauri-Signaturen müssen bereits neben den exakt veröffentlichten Dateien liegen.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import re
import subprocess
import tarfile
import tomllib
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]
BUNDLE_ROOT = ROOT / "target/release/bundle"
REPOSITORY = "DanielMuellerIR/Doppeldecker"
# Die Linux-Pakete tragen Version und Architektur im Namen; genau diese Kennung
# ist der veröffentlichte Vertrag (siehe tools/verify-linux-release.py).
LINUX_ARCHITECTURE = "amd64"
# Das macOS-Updater-Archiv trägt keine Version im Namen. Seine Version und
# Architektur werden deshalb aus dem Archivinhalt gelesen.
MACOS_APP_NAME = "Doppeldecker.app"
MACOS_INFO_PLIST = f"{MACOS_APP_NAME}/Contents/Info.plist"
MACOS_MAIN_BINARY = f"{MACOS_APP_NAME}/Contents/MacOS/doppeldecker"
MACOS_CLI_BINARY = f"{MACOS_APP_NAME}/Contents/MacOS/doppeldecker-cli"
# Thin 64-Bit-Mach-O: Die vier Magic-Bytes stehen little-endian in der Datei.
MACHO_64_MAGIC = b"\xcf\xfa\xed\xfe"
# CPU_TYPE_ARM64 (0x0100000C), ebenfalls little-endian gelesen.
MACHO_CPU_ARM64 = 0x0100000C
ELF_MACHINE_X86_64 = 62
ATTESTATION_SCHEMA = "doppeldecker-linux-attestation-v1"
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")
NODE_ED25519_VERIFY = r"""
const { createPublicKey, verify } = require("node:crypto");
const key = createPublicKey({
  key: Buffer.from(process.argv[1], "base64"),
  format: "der",
  type: "spki",
});
const valid = verify(
  null,
  Buffer.from(process.argv[2], "base64"),
  key,
  Buffer.from(process.argv[3], "base64"),
);
process.exit(valid ? 0 : 1);
"""
# RFC 3339 mit ausdrücklicher Zeitzone. `fromisoformat` allein akzeptiert auch
# kompaktere ISO-8601-Schreibweisen, die der Tauri-Updater nicht liest.
RFC_3339 = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$"
)


def workspace_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        return tomllib.load(handle)["workspace"]["package"]["version"]


def exactly_one(pattern: str) -> Path:
    matches = sorted(BUNDLE_ROOT.glob(pattern))
    if len(matches) != 1:
        raise SystemExit(
            f"Update-Manifest erwartet genau ein Artefakt für {pattern}, gefunden: {len(matches)}"
        )
    return matches[0]


def canonical_pub_date(value: str) -> str:
    """Prüft die Veröffentlichungszeit vollständig und gibt sie kanonisch zurück.

    Der Tauri-Updater parst `pub_date` strikt als RFC-3339-Zeitpunkt. Eine nur
    oberflächlich geprüfte Zeichenkette (etwa mit einem ungültigen Kalendertag)
    ergäbe ein Manifest, das jeder Client schon beim Lesen verwirft.
    """
    if not RFC_3339.match(value):
        raise SystemExit(
            "--pub-date muss ein RFC-3339-Zeitpunkt mit Zeitzone sein, z. B. 2026-08-06T18:00:00Z"
        )
    try:
        parsed = datetime.datetime.fromisoformat(value)
    except ValueError as error:
        raise SystemExit(f"--pub-date ist kein gültiger Zeitpunkt: {error}") from error
    if parsed.utcoffset() != datetime.timedelta(0):
        raise SystemExit("--pub-date muss als UTC-Zeitpunkt mit abschließendem Z vorliegen")
    return parsed.astimezone(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def linux_artifact(pattern: str, suffix: str, version: str) -> Path:
    """Bindet ein Linux-Paket fail-closed an Version und Zielarchitektur.

    Ohne diese Prüfung veröffentlicht ein allein im Bundle-Verzeichnis
    verbliebenes Paket einer älteren Version seine alte Nutzlast unter der
    aktuellen Versionsnummer und dem aktuellen Release-Tag.
    """
    artifact = exactly_one(pattern)
    expected = f"Doppeldecker_{version}_{LINUX_ARCHITECTURE}{suffix}"
    if artifact.name != expected:
        raise SystemExit(
            f"Update-Manifest erwartet {expected}, gefunden: {artifact.name}"
        )
    return artifact


def require_arm64_macho(archive: Path, member: str, payload: bytes) -> None:
    """Verlangt eine reguläre thin-arm64-Mach-O-Datei für GUI und CLI."""
    if payload[:4] != MACHO_64_MAGIC:
        raise SystemExit(f"{archive.name}: {member} ist kein thin 64-Bit-Mach-O")
    cpu_type = int.from_bytes(payload[4:8], "little")
    if cpu_type != MACHO_CPU_ARM64:
        raise SystemExit(
            f"{archive.name}: {member} ist nicht arm64 (CPU-Typ 0x{cpu_type:08x})"
        )


def macos_archive(version: str) -> Path:
    """Bindet das macOS-Updater-Archiv an Version und Architektur seines Inhalts.

    Das Archiv heißt immer gleich; nur sein Inhalt verrät, ob es zum aktuellen
    Release gehört. Geprüft werden deshalb die Kurzversion aus `Info.plist` und
    der Architekturwert im Mach-O-Kopf des Hauptbinaries.
    """
    archive = exactly_one("macos/*.app.tar.gz")
    with tarfile.open(archive, "r:gz") as bundle:
        plist = extract_member(bundle, archive, MACOS_INFO_PLIST)
        gui_head = extract_member(bundle, archive, MACOS_MAIN_BINARY)[:8]
        cli_head = extract_member(bundle, archive, MACOS_CLI_BINARY)[:8]
    short_version = plistlib.loads(plist).get("CFBundleShortVersionString")
    if short_version != version:
        raise SystemExit(
            f"{archive.name} enthält Version {short_version!r}, erwartet wurde {version!r}"
        )
    require_arm64_macho(archive, MACOS_MAIN_BINARY, gui_head)
    require_arm64_macho(archive, MACOS_CLI_BINARY, cli_head)
    return archive


def extract_member(bundle: tarfile.TarFile, archive: Path, name: str) -> bytes:
    """Liest genau eine reguläre Datei aus dem Archiv, ohne irgendetwas auszupacken."""
    matches = [member for member in bundle.getmembers() if member.name == name]
    if len(matches) != 1:
        raise SystemExit(
            f"{archive.name} enthält {name} nicht genau einmal (gefunden: {len(matches)})"
        )
    member = matches[0]
    if not member.isfile():
        raise SystemExit(f"{archive.name}: {name} ist keine reguläre Datei")
    handle = bundle.extractfile(member)
    if handle is None:
        raise SystemExit(f"{archive.name}: {name} ist nicht lesbar")
    with handle:
        return handle.read()


def updater_public_key() -> str:
    """Liest exakt den in der ausgelieferten App konfigurierten Schlüssel."""
    config = json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())
    encoded = config.get("plugins", {}).get("updater", {}).get("pubkey", "")
    try:
        return base64.b64decode(encoded, validate=True).decode("ascii")
    except (ValueError, UnicodeDecodeError) as error:
        raise SystemExit("Updater-Public-Key ist nicht gültig base64-kodiert") from error


def decode_base64(value: str, label: str) -> bytes:
    try:
        return base64.b64decode(value, validate=True)
    except ValueError as error:
        raise SystemExit(f"{label} ist nicht gültig base64-kodiert") from error


def parse_public_key(public_key: str) -> tuple[bytes, bytes]:
    lines = public_key.splitlines()
    if len(lines) != 2 or not lines[0].startswith("untrusted comment: "):
        raise SystemExit("Updater-Public-Key besitzt kein gültiges Minisign-Format")
    binary = decode_base64(lines[1], "Updater-Public-Key")
    if len(binary) != 42 or binary[:2] not in (b"Ed", b"ED"):
        raise SystemExit("Updater-Public-Key besitzt keinen unterstützten Ed25519-Kopf")
    return binary[2:10], binary[10:42]


def parse_signature(signature: str, label: str) -> tuple[bytes, bytes, str, bytes]:
    lines = signature.splitlines()
    if (
        len(lines) != 4
        or not lines[0].startswith("untrusted comment: ")
        or not lines[2].startswith("trusted comment: ")
    ):
        raise SystemExit(f"{label} besitzt kein gültiges Minisign-Format")
    first = decode_base64(lines[1], label)
    global_signature = decode_base64(lines[3], label)
    if len(first) != 74 or len(global_signature) != 64:
        raise SystemExit(f"{label} besitzt unerwartete Minisign-Längen")
    if first[:2] != b"ED":
        raise SystemExit(f"{label} verwendet nicht den erwarteten Prehash-Modus")
    return first[2:10], first[10:74], lines[2][17:], global_signature


def node_verify_ed25519(
    public_key: bytes,
    message: bytes,
    signature: bytes,
    *,
    run_command=None,
) -> bool:
    """Prüft Ed25519 über die mit Node 20 gesperrte Standardbibliothek."""
    runner = run_command or subprocess.run
    result = runner(
        (
            "node",
            "-e",
            NODE_ED25519_VERIFY,
            base64.b64encode(ED25519_SPKI_PREFIX + public_key).decode(),
            base64.b64encode(message).decode(),
            base64.b64encode(signature).decode(),
        ),
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return result.returncode == 0


def verify_signature(
    artifact: Path,
    signature_path: Path,
    public_key_text: str,
    *,
    run_command=None,
) -> str:
    """Prüft Nutzlastsignatur, Schlüssel-ID und signierten Trusted Comment."""
    if not signature_path.is_file():
        raise SystemExit(f"Signatur fehlt: {signature_path.relative_to(ROOT)}")
    encoded_signature = signature_path.read_text().strip()
    if not encoded_signature:
        raise SystemExit(f"Signatur ist leer: {signature_path.relative_to(ROOT)}")
    try:
        signature_text = decode_base64(
            encoded_signature, signature_path.name
        ).decode("ascii")
    except UnicodeDecodeError as error:
        raise SystemExit(
            f"{signature_path.name} enthält keine ASCII-Minisign-Signatur"
        ) from error
    public_key_id, public_key = parse_public_key(public_key_text)
    signature_key_id, signature, trusted, global_signature = parse_signature(
        signature_text, signature_path.name
    )
    if signature_key_id != public_key_id:
        raise SystemExit(f"Signaturprüfung fehlgeschlagen für {artifact.name}: falscher Schlüssel")
    digest = hashlib.blake2b(digest_size=64)
    with artifact.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    if not node_verify_ed25519(
        public_key, digest.digest(), signature, run_command=run_command
    ):
        raise SystemExit(f"Signaturprüfung fehlgeschlagen für {artifact.name}")
    if not node_verify_ed25519(
        public_key,
        signature + trusted.encode(),
        global_signature,
        run_command=run_command,
    ):
        raise SystemExit(
            f"Signaturprüfung des Trusted Comment fehlgeschlagen für {artifact.name}"
        )
    # latest.json erwartet genau die von Tauri erzeugte äußere Base64-Zeichenkette,
    # nicht das dekodierte vierzeilige Minisign-Dokument.
    return encoded_signature


def signature_for(artifact: Path, public_key: str) -> str:
    signature_path = artifact.with_name(artifact.name + ".sig")
    return verify_signature(artifact, signature_path, public_key)


def ar_members(path: Path) -> dict[str, bytes]:
    """Liest das einfache ar-Containerformat eines DEB ohne Plattformwerkzeug."""
    data = path.read_bytes()
    if not data.startswith(b"!<arch>\n"):
        raise SystemExit(f"{path.name} ist kein DEB/ar-Archiv")
    members: dict[str, bytes] = {}
    offset = 8
    while offset < len(data):
        header = data[offset : offset + 60]
        if len(header) != 60 or header[58:60] != bytes((96, 10)):
            raise SystemExit(f"{path.name} enthält einen ungültigen ar-Kopf")
        raw_name = header[:16].decode("ascii", errors="strict").strip()
        name = raw_name.removesuffix("/")
        try:
            size = int(header[48:58].decode("ascii").strip())
        except ValueError as error:
            raise SystemExit(f"{path.name} enthält eine ungültige ar-Größe") from error
        start = offset + 60
        end = start + size
        if end > len(data) or name in members:
            raise SystemExit(f"{path.name} enthält ein abgeschnittenes oder doppeltes ar-Mitglied")
        members[name] = data[start:end]
        offset = end + (size % 2)
    return members


def deb_fields(artifact: Path) -> dict[str, str]:
    members = ar_members(artifact)
    controls = [
        payload for name, payload in members.items() if name.startswith("control.tar.")
    ]
    if len(controls) != 1:
        raise SystemExit(f"{artifact.name} enthält nicht genau ein control.tar-Archiv")
    try:
        with tarfile.open(fileobj=io.BytesIO(controls[0]), mode="r:*") as bundle:
            candidates = [
                member
                for member in bundle.getmembers()
                if member.name in ("control", "./control")
            ]
            if len(candidates) != 1 or not candidates[0].isfile():
                raise SystemExit(f"{artifact.name} enthält keine reguläre DEB-control-Datei")
            handle = bundle.extractfile(candidates[0])
            if handle is None:
                raise SystemExit(f"{artifact.name}: DEB-control ist nicht lesbar")
            control = handle.read().decode("utf-8")
    except (tarfile.TarError, UnicodeDecodeError) as error:
        raise SystemExit(f"{artifact.name}: DEB-control ist ungültig") from error
    fields: dict[str, str] = {}
    current = ""
    for line in control.splitlines():
        if line.startswith((" ", "\t")) and current:
            fields[current] += "\n" + line[1:]
            continue
        if ":" not in line:
            current = ""
            continue
        current, value = line.split(":", 1)
        fields[current] = value.strip()
    return fields


def verify_deb_metadata(artifact: Path, version: str) -> None:
    fields = deb_fields(artifact)
    if fields.get("Package") != "doppeldecker":
        raise SystemExit(f"{artifact.name}: unerwarteter DEB-Paketname")
    if fields.get("Version") != version:
        raise SystemExit(
            f"{artifact.name}: DEB-Version {fields.get('Version')!r} statt {version!r}"
        )
    if fields.get("Architecture") != LINUX_ARCHITECTURE:
        raise SystemExit(
            f"{artifact.name}: DEB-Architektur {fields.get('Architecture')!r} statt amd64"
        )


def verify_appimage(artifact: Path, version: str, public_key: str) -> Path:
    header = artifact.read_bytes()[:20]
    if (
        len(header) < 20
        or header[:4] != b"\x7fELF"
        or header[4] != 2
        or header[5] != 1
        or int.from_bytes(header[18:20], "little") != ELF_MACHINE_X86_64
    ):
        raise SystemExit(f"{artifact.name} ist kein x86_64-ELF-AppImage")
    attestation = artifact.with_name(artifact.name + ".attestation.json")
    if not attestation.is_file():
        raise SystemExit(f"AppImage-Attestation fehlt: {attestation.name}")
    verify_signature(
        attestation,
        attestation.with_name(attestation.name + ".sig"),
        public_key,
    )
    try:
        document = json.loads(attestation.read_text())
    except json.JSONDecodeError as error:
        raise SystemExit(f"{attestation.name} ist kein gültiges JSON") from error
    expected = {
        "schema": ATTESTATION_SCHEMA,
        "artifact": artifact.name,
        "sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        "version": version,
        "elf_architecture": "x86_64",
    }
    if document.get("version") != version:
        raise SystemExit(
            f"AppImage-Version {document.get('version')!r} statt {version!r}"
        )
    if document != expected:
        raise SystemExit(f"{attestation.name} passt nicht exakt zur AppImage-Nutzlast")
    return attestation


def platform_entry(artifact: Path, version: str, public_key: str) -> dict[str, str]:
    base = f"https://github.com/{REPOSITORY}/releases/download/v{version}/"
    return {
        "url": base + quote(artifact.name),
        "signature": signature_for(artifact, public_key),
    }


def build_manifest(
    version: str,
    pub_date: str,
    *,
    public_key: str | None = None,
) -> dict[str, object]:
    public_key = public_key or updater_public_key()
    macos = macos_archive(version)
    linux = linux_artifact("appimage/*.AppImage", ".AppImage", version)
    deb = linux_artifact("deb/*.deb", ".deb", version)
    attestation = verify_appimage(linux, version, public_key)
    verify_deb_metadata(deb, version)
    expected_signatures = {
        artifact.with_name(artifact.name + ".sig")
        for artifact in (macos, linux, deb, attestation)
    }
    actual_signatures = set(BUNDLE_ROOT.rglob("*.sig"))
    unexpected = sorted(actual_signatures - expected_signatures)
    if unexpected:
        names = ", ".join(str(path.relative_to(BUNDLE_ROOT)) for path in unexpected)
        raise SystemExit(f"Unerwartete, ungeprüfte Signaturdatei(en): {names}")
    return {
        "version": version,
        "notes": f"Doppeldecker {version}",
        "pub_date": pub_date,
        "platforms": {
            "darwin-aarch64-app": platform_entry(macos, version, public_key),
            "linux-x86_64-appimage": platform_entry(linux, version, public_key),
            "linux-x86_64-deb": platform_entry(deb, version, public_key),
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--pub-date",
        # Nicht `required`: npm und pnpm hängen zusätzliche Argumente ans **Ende** der
        # Kommandozeichenkette, also an den zweiten Befehl von `release:update-manifest`.
        # Ein `required=True` machte den dokumentierten Finalisierungsschritt damit
        # unaufrufbar. Fehlt der Wert ganz, bricht der Lauf weiter unten ab; geraten
        # wird die Veröffentlichungszeit nie.
        default=os.environ.get("DOPPELDECKER_PUB_DATE"),
        help=(
            "Veröffentlichungszeit als RFC-3339-Wert, z. B. 2026-08-06T18:00:00Z. "
            "Alternativ über die Umgebungsvariable DOPPELDECKER_PUB_DATE."
        ),
    )
    args = parser.parse_args()
    if not args.pub_date:
        parser.error(
            "Veröffentlichungszeit fehlt: --pub-date setzen oder DOPPELDECKER_PUB_DATE exportieren"
        )
    pub_date = canonical_pub_date(args.pub_date)
    version = workspace_version()
    output = BUNDLE_ROOT / "latest.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(".tmp")
    temporary.write_text(
        json.dumps(build_manifest(version, pub_date), ensure_ascii=False, indent=2) + "\n"
    )
    os.replace(temporary, output)
    print(f"Update-Manifest geschrieben: {output.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
