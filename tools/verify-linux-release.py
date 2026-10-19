#!/usr/bin/env python3
"""Prüft Linux-Pakete und attestiert das geprüfte AppImage hashgebunden."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
BUNDLE_ROOT = ROOT / "target/release/bundle"
ATTESTATION_SCHEMA = "doppeldecker-linux-attestation-v1"
ELF_MACHINE_X86_64 = 62
DEBIAN_PACKAGE_NAME = re.compile(r"^([a-z0-9][a-z0-9+.-]*)(?::[a-z0-9-]+)?(?:\s|\(|\[|$)")


def fail(message: str) -> None:
    raise SystemExit(f"Linux-Paketprüfung fehlgeschlagen: {message}")


def single_artifact(directory: str, suffix: str, version: str) -> Path:
    artifacts = list((BUNDLE_ROOT / directory).glob(f"Doppeldecker_{version}_*{suffix}"))
    if len(artifacts) != 1:
        fail(f"genau ein {suffix}-Artefakt für Version {version} wurde erwartet")
    return artifacts[0]


def output(*command: os.PathLike[str] | str) -> str:
    return subprocess.run(
        command,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ).stdout.strip()


def require_file(root: Path, relative: str, executable: bool = False) -> Path:
    path = root / relative
    if not path.is_file() or path.is_symlink():
        fail(f"reguläre Paketdatei fehlt: {relative}")
    if executable and not os.access(path, os.X_OK):
        fail(f"Paketdatei ist nicht ausführbar: {relative}")
    return path


def require_appimage_desktop(root: Path) -> Path:
    """AppDir erlaubt den internen Desktoplink, nicht beliebige Paket-Symlinks."""
    desktop = root / "Doppeldecker.desktop"
    if root.is_symlink():
        fail("AppDir-Wurzel darf kein Symlink sein")
    if not desktop.is_symlink():
        return require_file(root, "Doppeldecker.desktop")
    expected = Path("usr/share/applications/Doppeldecker.desktop")
    if desktop.readlink() != expected:
        fail("AppImage-Desktoplink zeigt nicht auf die eigene installierte Desktopdatei")
    current = root
    for index, part in enumerate(expected.parts):
        current /= part
        try:
            mode = current.lstat().st_mode
        except OSError:
            fail("Ziel des AppImage-Desktoplinks ist nicht vollständig belegt")
        valid = stat.S_ISREG(mode) if index == len(expected.parts) - 1 else stat.S_ISDIR(mode)
        if not valid:
            fail("Ziel des AppImage-Desktoplinks enthält verlinkte oder ungültige Komponenten")
    return current


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def require_artifact_digest(path: Path, expected: str, label: str) -> None:
    if sha256_file(path) != expected:
        fail(f"{label}-Artefakt änderte sich nach seiner gebundenen Prüfung")


def elf_architecture(path: Path) -> str:
    """Liest Klasse, Bytefolge und Maschine direkt aus dem ELF-Kopf."""
    header = path.read_bytes()[:20]
    if len(header) < 20 or header[:4] != b"\x7fELF":
        fail(f"{path.name} ist keine ELF-Datei")
    if header[4] != 2 or header[5] != 1:
        fail(f"{path.name} ist kein little-endian ELF64")
    machine = int.from_bytes(header[18:20], "little")
    if machine != ELF_MACHINE_X86_64:
        fail(f"{path.name} ist nicht x86_64 (ELF-Maschine {machine})")
    return "x86_64"


def verify_layout(root: Path, label: str) -> None:
    binary = require_file(root, "usr/bin/doppeldecker", executable=True)
    cli = require_file(root, "usr/bin/doppeldecker-cli", executable=True)
    require_file(root, "usr/share/applications/Doppeldecker.desktop")
    require_file(root, "usr/share/icons/hicolor/1024x1024/apps/doppeldecker.png")
    elf_architecture(binary)
    elf_architecture(cli)
    linked = output("ldd", binary)
    if "not found" in linked:
        fail(f"{label}-Binary besitzt nicht auflösbare Bibliotheken")
    cli_linked = output("ldd", cli)
    if "not found" in cli_linked:
        fail(f"{label}-CLI besitzt nicht auflösbare Bibliotheken")


def verify_cli_payload_version(cli: Path, version: str, label: str) -> None:
    """Prüft die Version des wirklich aus dem Paket entnommenen CLI-Binarys."""
    try:
        result = subprocess.run(
            (cli, "--version"),
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        fail(f"{label}-CLI konnte für die Versionsprüfung nicht gestartet werden")
    if result.returncode != 0:
        fail(f"{label}-CLI meldet seine Version nicht erfolgreich")
    expected = f"doppeldecker-cli {version}\n"
    if result.stdout != expected:
        fail(f"{label}-CLI-Version weicht von der Workspace-Version ab")


def dependency_package_names(value: str) -> set[str]:
    """Liest Paketnamen aus Debian-Beziehungen und deren Alternativen exakt.

    Versionsbedingungen, Architekturqualifizierer und Alternativen werden dabei
    getrennt. Ein längerer Name wie ``libgtk-3-0-fake`` kann deshalb nicht mehr
    durch einen bloßen Teilstringtreffer als ``libgtk-3-0`` gelten.
    """
    names: set[str] = set()
    for relationship in value.split(","):
        for alternative in relationship.split("|"):
            match = DEBIAN_PACKAGE_NAME.match(alternative.strip())
            if match:
                names.add(match.group(1))
    return names


def sign_update_artifact(artifact: Path, environment: dict[str, str]) -> Path:
    """Signiert ohne Schlüsselargument und verwirft jede alte Signatur zuerst."""
    signature = artifact.with_name(artifact.name + ".sig")
    signature.unlink(missing_ok=True)
    subprocess.run(
        ("pnpm", "exec", "tauri", "signer", "sign", str(artifact)),
        cwd=ROOT,
        env=environment,
        check=True,
    )
    if not signature.is_file() or not signature.read_text().strip():
        fail(f"Signatur für {artifact.name} fehlt nach dem Tauri-Aufruf")
    return signature


def write_appimage_attestation(
    appimage: Path,
    version: str,
    environment: dict[str, str],
    *,
    expected_digest: str | None = None,
    sign=sign_update_artifact,
) -> Path:
    """Bindet die auf Linux geprüfte Version und Architektur an den SHA-256-Hash."""
    digest = expected_digest or sha256_file(appimage)
    require_artifact_digest(appimage, digest, "AppImage")
    architecture = elf_architecture(appimage)
    require_artifact_digest(appimage, digest, "AppImage")
    attestation = appimage.with_name(appimage.name + ".attestation.json")
    signature = attestation.with_name(attestation.name + ".sig")
    signature.unlink(missing_ok=True)
    document = {
        "schema": ATTESTATION_SCHEMA,
        "artifact": appimage.name,
        "sha256": digest,
        "version": version,
        "elf_architecture": architecture,
    }
    temporary = attestation.with_name(f".{attestation.name}.tmp")
    temporary.write_text(json.dumps(document, sort_keys=True, separators=(",", ":")) + "\n")
    os.replace(temporary, attestation)
    sign(attestation, environment)
    require_artifact_digest(appimage, digest, "AppImage")
    return attestation


def finalize_linux_release(
    deb: Path,
    appimage: Path,
    version: str,
    environment: dict[str, str],
    *,
    cli_payloads: tuple[tuple[str, Path], ...],
    expected_digests: dict[str, str] | None = None,
    sign=sign_update_artifact,
) -> Path:
    """Prüft die Payload-Versionen und signiert erst danach die Artefakte."""
    attestation = appimage.with_name(appimage.name + ".attestation.json")
    for stale in (
        appimage.with_name(appimage.name + ".sig"),
        deb.with_name(deb.name + ".sig"),
        attestation,
        attestation.with_name(attestation.name + ".sig"),
    ):
        stale.unlink(missing_ok=True)
    labels = [label for label, _cli in cli_payloads]
    if len(labels) != 2 or set(labels) != {"DEB", "AppImage"}:
        fail("Versionsbelege für genau DEB- und AppImage-CLI wurden erwartet")
    digests = expected_digests or {
        "DEB": sha256_file(deb),
        "AppImage": sha256_file(appimage),
    }
    if set(digests) != {"DEB", "AppImage"}:
        fail("Hashbelege für genau DEB und AppImage wurden erwartet")
    for label, cli in cli_payloads:
        verify_cli_payload_version(cli, version, label)
    require_artifact_digest(appimage, digests["AppImage"], "AppImage")
    require_artifact_digest(deb, digests["DEB"], "DEB")
    sign(appimage, environment)
    require_artifact_digest(appimage, digests["AppImage"], "AppImage")
    require_artifact_digest(deb, digests["DEB"], "DEB")
    sign(deb, environment)
    require_artifact_digest(appimage, digests["AppImage"], "AppImage")
    require_artifact_digest(deb, digests["DEB"], "DEB")
    attestation = write_appimage_attestation(
        appimage,
        version,
        environment,
        expected_digest=digests["AppImage"],
        sign=sign,
    )
    # `write_appimage_attestation()` prüft das AppImage selbst nach seiner Signatur.
    # Hier bleibt deshalb nur der unabhängige DEB-Gegencheck: Änderte sich das DEB
    # währenddessen, gehörte `.deb.sig` zum früheren Inhalt, während der Release-
    # Checksummschritt gleich danach den späteren Inhalt veröffentlichen würde.
    require_artifact_digest(deb, digests["DEB"], "DEB")
    return attestation


def verify_release(environment: dict[str, str]) -> tuple[str, Path, Path]:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        version = tomllib.load(handle)["workspace"]["package"]["version"]

    deb = single_artifact("deb", ".deb", version)
    appimage = single_artifact("appimage", ".AppImage", version)
    if not deb.is_file() or deb.is_symlink():
        fail("DEB ist keine reguläre Datei")
    if not appimage.is_file() or appimage.is_symlink() or not os.access(appimage, os.X_OK):
        fail("AppImage ist keine reguläre ausführbare Datei")
    expected_digests = {
        "DEB": sha256_file(deb),
        "AppImage": sha256_file(appimage),
    }

    with tempfile.TemporaryDirectory(prefix="doppeldecker-linux-release-") as temporary:
        temporary_root = Path(temporary)
        bound_deb = temporary_root / "bound.deb"
        bound_appimage = temporary_root / "bound.AppImage"
        shutil.copyfile(deb, bound_deb)
        shutil.copyfile(appimage, bound_appimage)
        bound_appimage.chmod(0o700)
        require_artifact_digest(bound_deb, expected_digests["DEB"], "gebundenes DEB")
        require_artifact_digest(
            bound_appimage, expected_digests["AppImage"], "gebundenes AppImage"
        )

        fields = {
            field: output("dpkg-deb", "--field", bound_deb, field)
            for field in ("Package", "Version", "Architecture", "Depends")
        }
        if fields["Package"] != "doppeldecker":
            fail("unerwarteter DEB-Paketname")
        if fields["Version"] != version:
            fail("DEB-Version weicht von der Workspace-Version ab")
        if fields["Architecture"] != "amd64":
            fail("DEB-Architektur ist nicht amd64")
        dependency_names = dependency_package_names(fields["Depends"])
        for dependency in ("libwebkit2gtk-4.1-0", "libgtk-3-0"):
            if dependency not in dependency_names:
                fail(f"DEB-Abhängigkeit fehlt: {dependency}")
        elf_architecture(bound_appimage)

        deb_root = temporary_root / "deb"
        subprocess.run(
            ("dpkg-deb", "-x", bound_deb, deb_root),
            check=True,
            stdout=subprocess.DEVNULL,
        )
        verify_layout(deb_root, "DEB")
        deb_cli = deb_root / "usr/bin/doppeldecker-cli"

        subprocess.run(
            (bound_appimage, "--appimage-extract"),
            cwd=temporary_root,
            check=True,
            stdout=subprocess.DEVNULL,
        )
        appimage_root = temporary_root / "squashfs-root"
        require_appimage_desktop(appimage_root)
        require_file(appimage_root, "Doppeldecker.png")
        verify_layout(appimage_root, "AppImage")
        appimage_cli = appimage_root / "usr/bin/doppeldecker-cli"

        # Die attestierte Workspace-Version darf erst nach einem erfolgreichen
        # Lauf beider tatsächlich ausgelieferten CLI-Binarys signiert werden.
        attestation = finalize_linux_release(
            deb,
            appimage,
            version,
            environment,
            cli_payloads=(("DEB", deb_cli), ("AppImage", appimage_cli)),
            expected_digests=expected_digests,
        )
    return version, appimage, attestation


def main() -> None:
    if platform.system() != "Linux":
        fail("dieser Prüfer darf nur unter Linux laufen")
    version, _appimage, _attestation = verify_release(os.environ.copy())
    print(
        f"Linux-Paketprüfung grün: Doppeldecker {version} · "
        "DEB amd64 · AppImage x86_64 · Attestation signiert"
    )


if __name__ == "__main__":
    main()
