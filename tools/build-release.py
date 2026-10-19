#!/usr/bin/env python3
"""Baut Release-Artefakte mit neutralisierten lokalen Quellpfaden."""

from __future__ import annotations

import argparse
import ctypes
import errno
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tomllib


ROOT = Path(__file__).resolve().parents[1]
FLAG_SEPARATOR = "\x1f"
DEVELOPER_ID_PREFIX = "Developer ID Application:"
# Nur ein neutraler Vorschlagswert für die interaktive Erstabfrage. Der tatsächlich
# genutzte Profilname ist kein Geheimnis, aber intern und gehört NICHT ins Repo —
# er wird über NOTARY_PROFILE oder clone-lokal in `.git/config` aufgelöst (siehe
# resolve_notary_profile). Der frühere feste Default zeigte auf ein Profil, das nur
# auf einem Mac existierte, und ließ jeden anderen Mac scheitern.
NOTARY_PROFILE_PROMPT_DEFAULT = "notary"
# Clone-lokaler git-config-Schlüssel: `.git/config` ist pro Arbeitskopie und kann
# weder committet noch zu GitHub gepusht werden — ideal für den internen Namen.
NOTARY_PROFILE_GIT_KEY = "doppeldecker.notaryProfile"
SIDECAR_NAME = "doppeldecker-cli"
# Der selbstgepackte DMG-Name deklariert diese Architektur; der Release ist
# fail-closed daran gebunden (siehe require_macos_release_arch).
MACOS_RELEASE_TARGET = "aarch64-apple-darwin"
# Darwin sys/stdio.h: Erstexport exklusiv anlegen, vorhandene App atomar tauschen.
MACOS_RENAME_SWAP = 0x00000002
MACOS_RENAME_EXCL = 0x00000004


def existing_rustflags(environment: dict[str, str]) -> list[str]:
    """Übernimmt vorhandene Flags, ohne Pfade durch Shell-Quoting zu beschädigen."""
    encoded = environment.get("CARGO_ENCODED_RUSTFLAGS")
    plain = environment.pop("RUSTFLAGS", "")
    if encoded:
        return [flag for flag in encoded.split(FLAG_SEPARATOR) if flag]
    return shlex.split(plain) if plain else []


def release_environment() -> dict[str, str]:
    environment = os.environ.copy()
    flags = existing_rustflags(environment)
    home = Path.home().resolve()
    candidates = {
        ROOT.resolve(): "/build/src/doppeldecker",
        Path(environment.get("CARGO_HOME", home / ".cargo")).resolve(): "/build/cargo",
        Path(environment.get("RUSTUP_HOME", home / ".rustup")).resolve(): "/build/rustup",
        home: "/build/home",
    }
    # Längere Präfixe kommen zuerst, damit etwa der Workspace einen aussagekräftigen
    # neutralen Pfad statt nur des allgemeinen Build-Benutzerordners erhält.
    for source, replacement in sorted(candidates.items(), key=lambda item: -len(str(item[0]))):
        flags.extend(("--remap-path-prefix", f"{source}={replacement}"))
    environment["CARGO_ENCODED_RUSTFLAGS"] = FLAG_SEPARATOR.join(flags)
    return environment


def rust_target_triple(environment: dict[str, str]) -> str:
    """Ermittelt das native Cargo-Ziel für den mitgelieferten CLI-Sidecar."""
    configured = environment.get("CARGO_BUILD_TARGET", "").strip()
    if configured:
        return configured
    result = subprocess.run(
        ("rustc", "-vV"),
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    )
    for line in result.stdout.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise SystemExit("Rust-Zielplattform konnte nicht ermittelt werden.")


def prepare_cli_sidecar(environment: dict[str, str]) -> str:
    """Baut das CLI und legt Tauri die erwartete Triple-Datei im Buildbaum bereit.

    Der Pfad liegt absichtlich unter `target/`: Plattformbinaries werden nie
    eingecheckt. Der Rückgabewert ist der suffixfreie, aus `src-tauri` auflösbare
    Pfad für Tauri `bundle.externalBin`.
    """
    target = rust_target_triple(environment)
    command = (
        "cargo",
        "build",
        "-p",
        SIDECAR_NAME,
        "--release",
        "--target",
        target,
    )
    subprocess.run(command, cwd=ROOT, env=environment, check=True)
    extension = ".exe" if "windows" in target else ""
    built = ROOT / "target" / target / "release" / f"{SIDECAR_NAME}{extension}"
    if not built.is_file():
        raise SystemExit("Gebautes doppeldecker-cli wurde nicht gefunden.")
    sidecars = ROOT / "target" / "sidecars"
    sidecars.mkdir(parents=True, exist_ok=True)
    staged = sidecars / f"{SIDECAR_NAME}-{target}{extension}"
    shutil.copy2(built, staged)
    staged.chmod(staged.stat().st_mode | 0o111)
    return f"../target/sidecars/{SIDECAR_NAME}"


def install_node_dependencies(environment: dict[str, str]) -> None:
    """Stellt die gesperrten Tauri-Werkzeuge auch in einem frischen Clone bereit.

    Der Aufruf bleibt absichtlich Teil jedes Bundle-Laufs. ``pnpm`` prüft einen
    bereits aktuellen ``node_modules``-Baum schnell, während nur ein solcher
    vollständiger Abgleich garantiert, dass ein vorhandener, aber veralteter Baum
    nicht mit einer anderen Tauri-Version weitergebaut wird.
    """
    try:
        subprocess.run(
            ("pnpm", "install", "--frozen-lockfile"),
            cwd=ROOT,
            env=environment,
            check=True,
        )
    except FileNotFoundError as error:
        raise SystemExit(
            "pnpm wurde nicht gefunden. Für Release-Builds wird pnpm mit Node.js 20 oder neuer benötigt."
        ) from error


def require_updater_signing_key(environment: dict[str, str]) -> None:
    """Verlangt den passwortgeschützten privaten Schlüssel nur im Prozess-Env.

    Der Wert wird bewusst weder ausgegeben noch aus einer Projektdatei gelesen.
    """
    if not environment.get("TAURI_SIGNING_PRIVATE_KEY", "").strip():
        raise SystemExit(
            "Signierte Update-Artefakte brauchen TAURI_SIGNING_PRIVATE_KEY in der Umgebung."
        )
    if not environment.get("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", ""):
        raise SystemExit(
            "Der Updater-Signierschlüssel muss mit TAURI_SIGNING_PRIVATE_KEY_PASSWORD geschützt sein."
        )


def run_tauri_build(
    bundles: str,
    environment: dict[str, str],
    *,
    create_updater_artifacts: bool = False,
) -> None:
    """Baut GUI und CLI als ein gemeinsam signiertes/gepacktes Artefakt."""
    if create_updater_artifacts:
        require_updater_signing_key(environment)
    install_node_dependencies(environment)
    sidecar = prepare_cli_sidecar(environment)
    bundle = {"externalBin": [sidecar]}
    if create_updater_artifacts:
        bundle["createUpdaterArtifacts"] = True
    bundle_override = json.dumps({"bundle": bundle})
    subprocess.run(
        (
            "pnpm",
            "exec",
            "tauri",
            "build",
            "--bundles",
            bundles,
            "--config",
            bundle_override,
        ),
        cwd=ROOT,
        env=environment,
        check=True,
    )


def developer_id_identity(environment: dict[str, str]) -> str:
    """Ermittelt eindeutig eine Developer-ID, ohne sie projektspezifisch zu speichern."""
    configured = environment.get("APPLE_SIGNING_IDENTITY", "").strip()
    if configured:
        if configured == "-":
            raise SystemExit("Öffentlicher macOS-Build lehnt eine Ad-hoc-Signatur ab.")
        return configured

    result = subprocess.run(
        ("security", "find-identity", "-v", "-p", "codesigning"),
        check=True,
        capture_output=True,
        text=True,
    )
    identities = []
    for line in result.stdout.splitlines():
        match = re.match(r'^\s*\d+\)\s+[0-9A-F]+\s+"(.+)"$', line)
        if match and match.group(1).startswith(DEVELOPER_ID_PREFIX):
            identities.append(match.group(1))
    if len(identities) != 1:
        raise SystemExit(
            "Öffentlicher macOS-Build braucht genau eine Developer-ID-Application-"
            "Identität oder APPLE_SIGNING_IDENTITY."
        )
    return identities[0]


def notary_profile_works(profile: str) -> bool:
    """`history` ist der verlässliche Test eines notarytool-Profils. Ein bloßer
    Schlüsselbund-Eintrag beweist noch keine gültige Anmeldung."""
    return (
        subprocess.run(
            ("xcrun", "notarytool", "history", "--keychain-profile", profile),
            capture_output=True,
            text=True,
        ).returncode
        == 0
    )


def git_config_notary_profile() -> str:
    """Liest den clone-lokal gemerkten Profilnamen aus `.git/config` (falls gesetzt)."""
    result = subprocess.run(
        ("git", "config", "--local", "--get", NOTARY_PROFILE_GIT_KEY),
        capture_output=True,
        text=True,
        cwd=ROOT,
    )
    return result.stdout.strip() if result.returncode == 0 else ""


def remember_notary_profile(profile: str) -> None:
    """Merkt den Profilnamen clone-lokal, damit künftige Builds ihn nicht erneut
    erfragen. `.git/config` ist nicht committebar und damit public-safe."""
    subprocess.run(
        ("git", "config", "--local", NOTARY_PROFILE_GIT_KEY, profile),
        cwd=ROOT,
        check=False,
    )


SETUP_HINT = (
    "Kein verwendbares notarytool-Profil auf diesem Mac. So wird es dauerhaft gelöst:\n"
    "  • Projektübergreifend (empfohlen): NOTARY_PROFILE=<profil> in ~/.zshenv setzen —\n"
    "    gilt dann für alle eigenen Apps auf diesem Mac.\n"
    "  • Oder nur für diese Arbeitskopie: git config --local "
    f"{NOTARY_PROFILE_GIT_KEY} <profil>\n"
    "Das Profil selbst wird einmalig je Mac im Schlüsselbund angelegt (nur in einer\n"
    "lokalen GUI-Terminalsitzung, nicht über SSH):\n"
    "  xcrun notarytool store-credentials '<profil>' --apple-id '<apple-id>' "
    "--team-id '<team-id>'\n"
    "Das App-Passwort dabei nur an der verdeckten Abfrage eingeben, nie als Argument."
)


def resolve_notary_profile(environment: dict[str, str]) -> str:
    """Löst den notarytool-Profilnamen public-safe auf — ohne internen Namen im Repo.

    Reihenfolge nach Fastra-Vorbild: 1) die Umgebungsvariable `NOTARY_PROFILE`
    (z. B. global in `~/.zshenv` für alle Projekte), 2) der clone-lokal in
    `.git/config` gemerkte Name, 3) eine einmalige interaktive Abfrage an einem
    echten Terminal. Über SSH/CI ohne Vorkonfiguration bricht es mit klarer Anleitung
    ab, statt auf einen erfundenen Default zu zeigen.
    """
    profile = environment.get("NOTARY_PROFILE", "").strip()
    if not profile:
        profile = git_config_notary_profile()
    if not profile:
        if not sys.stdin.isatty():
            raise SystemExit(SETUP_HINT)
        entered = input(
            f"Notary-Profilname für diesen Mac [{NOTARY_PROFILE_PROMPT_DEFAULT}]: "
        ).strip()
        profile = entered or NOTARY_PROFILE_PROMPT_DEFAULT
    return profile


def setup_notary_profile_interactively(profile: str) -> None:
    """Bietet an einem echten Terminal an, das fehlende Profil einmalig einzurichten.

    Apple-ID und Team-ID werden abgefragt statt eingecheckt (im öffentlichen Repo
    steht so nichts Internes). Das App-Passwort erfragt `store-credentials` selbst
    verdeckt und legt es direkt im Schlüsselbund ab — es taucht nie als Argument auf.
    """
    print(
        f"✗ Notary-Profil {profile!r} ist auf diesem Mac nicht verwendbar.\n"
        "  Schlüsselbund-Profile werden von iCloud nicht zwischen Macs synchronisiert.",
        file=sys.stderr,
    )
    if not sys.stdin.isatty():
        raise SystemExit(SETUP_HINT)
    answer = input("Profil jetzt interaktiv im Schlüsselbund einrichten? [j/N] ").strip()
    if answer.lower() not in {"j", "ja", "y", "yes"}:
        raise SystemExit(SETUP_HINT)
    apple_id = input("Apple-ID: ").strip()
    team_id = input("Team-ID: ").strip()
    if not apple_id or not team_id:
        raise SystemExit("Apple-ID und Team-ID dürfen nicht leer sein.")
    # Kein --password: notarytool fragt das App-spezifische Passwort verdeckt ab.
    subprocess.run(
        ("xcrun", "notarytool", "store-credentials", profile,
         "--apple-id", apple_id, "--team-id", team_id),
        check=True,
    )
    if not notary_profile_works(profile):
        raise SystemExit(f"Profil {profile!r} funktioniert nach der Einrichtung noch nicht.")


def require_notary_profile(environment: dict[str, str]) -> str:
    """Belegt vor dem langen Build, dass das DMG später eingereicht werden kann.

    Tauri notarisiert nur die `.app`. Das ausgelieferte DMG braucht ein eigenes
    Ticket und damit einen zweiten Weg zu Apple. Das Passwort darf dabei nicht als
    Kommandozeilenargument auftauchen, deshalb übernimmt ein notarytool-Profil im
    Schlüsselbund die Zugangsdaten. Der Preflight scheitert lieber sofort, als eine
    Viertelstunde Buildzeit zu verbrennen.
    """
    profile = resolve_notary_profile(environment)
    if not notary_profile_works(profile):
        setup_notary_profile_interactively(profile)
    # Für künftige Builds clone-lokal merken (public-safe, nicht committebar).
    remember_notary_profile(profile)
    return profile


def workspace_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        return tomllib.load(handle)["workspace"]["package"]["version"]


def notarize_and_staple_app(profile: str) -> None:
    """Notarisiert die gebaute `.app` direkt (ZIP-Einreichung) und heftet das Ticket an.

    Für den lokalen Gebrauch (Projekt-Root und /Applications) entsteht kein DMG.
    Wichtig für macOS-Berechtigungen: TCC bindet erteilte Rechte an eine stabile
    Codesignatur — Ad-hoc-Signaturen wechseln mit jedem Build und verlieren sie.
    Die Zugangsdaten liegen ausschließlich im notarytool-Schlüsselbundprofil;
    es werden keine Apple-Secrets über Argumente oder Projektdateien transportiert.
    """
    app = ROOT / "target/release/bundle/macos/Doppeldecker.app"
    if not app.is_dir():
        raise SystemExit("Erwartete App unter target/release/bundle/macos fehlt.")
    archive = app.parent / "Doppeldecker.zip"
    if archive.exists():
        archive.unlink()
    # ditto -c -k erzeugt das von Apple erwartete ZIP inklusive Resource Forks.
    subprocess.run(("ditto", "-c", "-k", "--keepParent", str(app), str(archive)), check=True)
    try:
        subprocess.run(
            ("xcrun", "notarytool", "submit", str(archive), "--keychain-profile", profile, "--wait"),
            check=True,
        )
    finally:
        archive.unlink(missing_ok=True)
    subprocess.run(("xcrun", "stapler", "staple", str(app)), check=True)
    subprocess.run(("xcrun", "stapler", "validate", str(app)), check=True)


def updater_signature_path(artifact: Path) -> Path:
    return artifact.with_name(artifact.name + ".sig")


def sign_update_artifact(
    artifact: Path,
    environment: dict[str, str],
    *,
    run_command=None,
) -> Path:
    """Signiert exakt die angegebene fertige Nutzlast mit Tauri Minisign.

    Eine alte Signatur wird vor dem Aufruf entfernt. Schlägt das Signieren fehl
    oder erzeugt das Werkzeug keine nichtleere ``.sig``-Datei, kann deshalb kein
    vorheriger Erfolg versehentlich neben einer neuen Nutzlast liegen bleiben.
    Der private Schlüssel bleibt ausschließlich in der Prozessumgebung.
    """
    require_updater_signing_key(environment)
    runner = run_command or subprocess.run
    signature = updater_signature_path(artifact)
    signature.unlink(missing_ok=True)
    runner(
        ("pnpm", "exec", "tauri", "signer", "sign", str(artifact)),
        cwd=ROOT,
        env=environment,
        check=True,
    )
    if not signature.is_file() or not signature.read_text().strip():
        raise SystemExit(f"Tauri hat keine Signatur für {artifact.name} erzeugt.")
    return signature


def repack_and_sign_macos_updater(
    app: Path,
    environment: dict[str, str],
    *,
    run_command=None,
) -> Path:
    """Packt das Updaterarchiv aus der bereits gestapelten App neu und signiert es.

    Tauri erzeugt das erste Archiv während des Builds und damit vor der direkten
    App-Notarisierung dieses Releasewegs. Dieses Archiv wird absichtlich ersetzt:
    Erst das fertige App-Bundle samt Ticket wird in eine temporäre TAR-Gzip-Datei
    gepackt, atomar an den Zielnamen gesetzt und danach neu signiert.
    """
    if not app.is_dir():
        raise SystemExit(f"Updater-Quelle fehlt: {app}")
    runner = run_command or subprocess.run
    archive = app.parent / f"{app.name}.tar.gz"
    temporary = archive.with_name(f".{archive.name}.tmp")
    updater_signature_path(archive).unlink(missing_ok=True)
    temporary.unlink(missing_ok=True)
    try:
        runner(
            ("tar", "-czf", str(temporary), "-C", str(app.parent), app.name),
            check=True,
        )
        if not temporary.is_file() or temporary.stat().st_size == 0:
            raise SystemExit("Das neu gepackte macOS-Updaterarchiv ist leer oder fehlt.")
        os.replace(temporary, archive)
        sign_update_artifact(archive, environment, run_command=runner)
    finally:
        temporary.unlink(missing_ok=True)
    return archive


def verify_macos_updater_archive(
    archive: Path,
    environment: dict[str, str],
    *,
    run_command=None,
) -> None:
    """Prüft die tatsächlich extrahierte Archiv-App vor dem DMG-Schritt.

    Die Prüfung arbeitet ausschließlich in einem temporären Ordner. Sie bindet
    das Archiv an eine reguläre GUI- und CLI-Mach-O-Datei, Workspace-Version,
    arm64, Codesignatur und das angeheftete Notary-Ticket.
    """
    runner = run_command or subprocess.run
    if not archive.is_file():
        raise SystemExit(f"macOS-Updaterarchiv fehlt: {archive}")
    listing = runner(
        ("tar", "-tzf", str(archive)),
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    if not listing:
        raise SystemExit("macOS-Updaterarchiv ist leer.")
    for name in listing:
        parts = Path(name).parts
        if name.startswith("/") or ".." in parts:
            raise SystemExit(f"macOS-Updaterarchiv enthält einen unsicheren Pfad: {name}")

    with tempfile.TemporaryDirectory(prefix="doppeldecker-updater-verify-") as temporary:
        root = Path(temporary)
        runner(("tar", "-xzf", str(archive), "-C", str(root)), check=True)
        app = root / "Doppeldecker.app"
        info = app / "Contents/Info.plist"
        binaries = (
            app / "Contents/MacOS/doppeldecker",
            app / f"Contents/MacOS/{SIDECAR_NAME}",
        )
        if not info.is_file() or info.is_symlink():
            raise SystemExit("Updaterarchiv enthält keine reguläre Info.plist.")
        for binary in binaries:
            if not binary.is_file() or binary.is_symlink():
                raise SystemExit(
                    f"Updaterarchiv enthält keine reguläre Mach-O-Datei: {binary.name}"
                )
        with info.open("rb") as handle:
            archived_version = plistlib.load(handle).get("CFBundleShortVersionString")
        version = workspace_version()
        if archived_version != version:
            raise SystemExit(
                f"Updaterarchiv enthält Version {archived_version!r}, erwartet wurde {version!r}."
            )
        runner(("codesign", "--verify", "--deep", "--strict", str(app)), check=True)
        runner(("xcrun", "stapler", "validate", str(app)), check=True)
        require_macos_release_arch(app, environment)


def replace_exported_app(staged: Path, destination: Path) -> None:
    """Übergibt die fertige Kopie ohne Lücke im bisherigen macOS-App-Export."""
    if sys.platform != "darwin":
        raise OSError(errno.ENOTSUP, "Atomarer App-Export benötigt macOS")
    rename = ctypes.CDLL(None, use_errno=True).renameatx_np
    rename.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    source_fd = os.open(staged.parent, directory_flags)
    try:
        target_fd = os.open(destination.parent, directory_flags)
        try:
            arguments = (source_fd, os.fsencode(staged.name), target_fd, os.fsencode(destination.name))
            # Nur eine tatsächliche Kollision erlaubt den Tausch. Dabei wandert
            # der alte Export ins eigene Staging und bleibt bis zur Übergabe erhalten.
            if rename(*arguments, MACOS_RENAME_EXCL) == 0:
                return
            error = ctypes.get_errno()
            if error != errno.EEXIST:
                raise OSError(error, os.strerror(error), str(destination))
            previous = os.stat(destination.name, dir_fd=target_fd, follow_symlinks=False)
            if not stat.S_ISDIR(previous.st_mode):
                raise FileExistsError(errno.EEXIST, "Vorhandener App-Export ist kein Verzeichnis", str(destination))
            if rename(*arguments, MACOS_RENAME_SWAP) != 0:
                error = ctypes.get_errno()
                raise OSError(error, os.strerror(error), str(destination))
        finally:
            os.close(target_fd)
    finally:
        os.close(source_fd)


def export_app_to_root() -> None:
    """Legt die fertige App zusätzlich doppelklickbar in den Projekt-Root.

    Der Tauri-Bundle-Pfad liegt tief unter `target/`; für den Alltag soll die App
    direkt im Repo-Root auffindbar sein. `ditto` kopiert dabei Signatur, xattrs
    und Resource Forks verlustfrei — ein einfacher Python-Copytree könnte die
    Codesignatur beschädigen. Erst die vollständige eigene Kopie tauscht den alten
    Export atomar aus; bei Kopier- oder Übergabefehlern bleibt dieser erhalten.
    """
    source = ROOT / "target/release/bundle/macos/Doppeldecker.app"
    if not source.is_dir():
        raise SystemExit("Erwartete App unter target/release/bundle/macos fehlt.")
    destination = ROOT / "Doppeldecker.app"
    with tempfile.TemporaryDirectory(prefix=".doppeldecker-app-export-", dir=ROOT) as directory:
        staged = Path(directory) / destination.name
        subprocess.run(("ditto", str(source), str(staged)), check=True)
        replace_exported_app(staged, destination)


def require_macos_release_arch(app: Path, environment: dict[str, str]) -> None:
    """Bindet den Release fail-closed an die im DMG-Namen deklarierte Architektur.

    Auf einem Intel-Mac (oder mit fremdem CARGO_BUILD_TARGET) entstünde sonst ein
    x86_64-Bundle mit falscher aarch64-Kennzeichnung. Zusätzlich belegt
    `lipo -archs`, dass GUI-Binary und CLI-Sidecar wirklich arm64 enthalten.
    """
    target = rust_target_triple(environment)
    if target != MACOS_RELEASE_TARGET:
        raise SystemExit(
            f"macOS-Release ist an {MACOS_RELEASE_TARGET} gebunden (aktuelles Ziel: {target})."
        )
    for binary in (
        app / "Contents/MacOS/doppeldecker",
        app / f"Contents/MacOS/{SIDECAR_NAME}",
    ):
        if not binary.is_file():
            raise SystemExit(f"Erwartetes Binary fehlt für die Architekturprüfung: {binary}")
        archs = subprocess.run(
            ("lipo", "-archs", str(binary)),
            check=True,
            capture_output=True,
            text=True,
        ).stdout.split()
        if "arm64" not in archs:
            raise SystemExit(
                f"{binary} enthält kein arm64 (gefunden: {' '.join(archs) or 'nichts'})."
            )


def attached_device(plist_output: str, mount: Path) -> str:
    """Liest aus der `hdiutil attach -plist`-Antwort das Device des EIGENEN Mounts.

    Nur dieses Device darf später wieder ausgehängt werden — niemals ein fremdes
    Volume mit zufällig gleichem Namen.
    """
    entities = plistlib.loads(plist_output.encode()).get("system-entities", [])
    # hdiutil meldet den Mountpoint vollständig aufgelöst (z. B. /private/tmp/…);
    # der Vergleich muss deshalb über realpath laufen.
    expected = os.path.realpath(mount)
    for entity in entities:
        reported = entity.get("mount-point")
        if reported and os.path.realpath(reported) == expected:
            device = entity.get("dev-entry")
            if device:
                return device
    raise SystemExit("hdiutil attach lieferte kein Device für den eigenen Mountpoint.")


def detach_all_reported_devices(plist_output: str) -> bool:
    """Not-Aushängen, wenn der eigene Mountpoint keinem Device zugeordnet werden kann.

    Auch dann ist das Image angehängt. Ausgehängt wird deshalb genau das, was DIESER
    Attach-Lauf gemeldet hat — nie ein fremder Pfad und nie ein fremdes Volume.
    Der auslösende Fehler bleibt erhalten. Ohne belegtes Aushängen muss das
    Arbeitsimage erhalten bleiben, weil es noch die Grundlage eines Mounts sein kann.
    """
    try:
        entities = plistlib.loads(plist_output.encode()).get("system-entities", [])
    except Exception:
        return False
    detached = False
    failed = False
    for entity in entities:
        device = entity.get("dev-entry")
        if device:
            result = subprocess.run(("hdiutil", "detach", device, "-force"), capture_output=True)
            detached |= result.returncode == 0
            failed |= result.returncode != 0
    return detached and not failed


def detach_own_device(device: str) -> None:
    """Hängt ausschließlich das selbst angehängte Device aus.

    Erst regulär; hält Finder/Indexer den Mount kurz offen, nach kurzer Wartezeit
    einmal mit `-force`. Das trifft nie ein fremdes Volume, weil das Device aus
    dem eigenen `hdiutil attach` stammt.

    Scheitert auch der erzwungene Versuch, ist das ein harter Fehler: Ein noch
    gemountetes Image darf nicht nach `hdiutil convert` gehen — die Schreibpuffer
    des Mounts sind dann nicht garantiert auf der Platte, und das komprimierte DMG
    könnte einen unvollständigen Stand einfrieren.
    """
    result = subprocess.run(("hdiutil", "detach", device), capture_output=True)
    if result.returncode == 0:
        return
    time.sleep(2)
    forced = subprocess.run(("hdiutil", "detach", device, "-force"), capture_output=True)
    if forced.returncode != 0:
        raise SystemExit(
            f"{device} konnte nicht ausgehängt werden (auch nicht mit -force): "
            f"{forced.stderr.decode(errors='replace').strip() or 'ohne Meldung'}"
        )


def build_dmg_from_app(app: Path, identity: str, finder_layout: bool, notary_profile: str) -> Path:
    """Packt ein DMG aus einer FERTIGEN, bereits notarisierten App.

    Warum von Hand und nicht mit `tauri bundle --bundles dmg`: Der Tauri-Bundler
    signiert die App beim Packen neu („replacing existing signature") und löscht
    sie anschließend aus `target/release/bundle/macos/`. Das eben angeheftete
    Notary-Ticket wäre damit weg — belegt am 2026-07-26. hdiutil kopiert das
    Bundle dagegen unverändert, samt Ticket-Datei.

    Ein DMG-Layout ist nicht konfiguriert (`bundle.dmg` in tauri.conf.json ist
    leer), deshalb genügt hier App + Applications-Verknüpfung.
    """
    version = workspace_version()
    dmg_dir = ROOT / "target/release/bundle/dmg"
    dmg_dir.mkdir(parents=True, exist_ok=True)
    dmg = dmg_dir / f"Doppeldecker_{version}_aarch64.dmg"
    volume = "Doppeldecker"
    staging = Path(tempfile.mkdtemp(prefix=".doppeldecker-dmg-", dir=dmg_dir))
    rw_dmg = staging / "working.dmg"
    staged_dmg = staging / dmg.name
    mount = Path("/Volumes") / f"Doppeldecker-{staging.name.rsplit('-', 1)[-1]}"
    cleanup_allowed = True
    try:
        if mount.exists():
            raise SystemExit(f"{mount} ist bereits belegt; bitte manuell auswerfen.")
        megabytes = int(subprocess.run(("du", "-sm", str(app)), capture_output=True,
                                      text=True, check=True).stdout.split()[0]) + 60
        subprocess.run(
            ("hdiutil", "create", "-srcfolder", str(app), "-volname", volume,
             "-fs", "HFS+", "-fsargs", "-c c=64,a=16,e=16", "-format", "UDRW",
             "-size", f"{megabytes}m", str(rw_dmg)),
            check=True,
        )
        # `-plist` liefert das konkrete Device des eigenen Attach-Vorgangs.
        # Ein fehlgeschlagener Attach kann trotzdem einen Mount hinterlassen.
        # Sein Arbeitsimage darf deshalb ohne eigenen Detach-Beleg nicht weg.
        cleanup_allowed = False
        attach = subprocess.run(
            ("hdiutil", "attach", str(rw_dmg), "-mountpoint", str(mount),
             "-nobrowse", "-noverify", "-noautoopen", "-plist"),
            check=True,
            capture_output=True,
            text=True,
        )
        # Auch die Device-Zuordnung kann nach einem erfolgreichen Attach scheitern.
        try:
            device = attached_device(attach.stdout, mount)
        except BaseException:
            cleanup_allowed = detach_all_reported_devices(attach.stdout)
            raise
        try:
            (mount / "Applications").symlink_to("/Applications")
            if finder_layout:
                # Öffnet ein echtes Finder-Fenster und reißt den Fokus an sich —
                # deshalb für headless-Läufe abschaltbar.
                subprocess.run(("osascript", "-e", f'''
tell application "Finder"
  set imageDisk to disk of (POSIX file "{mount}" as alias)
  tell imageDisk
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {{200, 120, 800, 520}}
    set theViewOptions to the icon view options of container window
    set arrangement of theViewOptions to not arranged
    set icon size of theViewOptions to 128
    set position of item "Doppeldecker.app" of container window to {{150, 190}}
    set position of item "Applications" of container window to {{450, 190}}
    update without registering applications
    close
  end tell
end tell'''), check=True)
            subprocess.run(("sync",), check=True)
            time.sleep(2)   # Race: DS_Store-Schreibpuffer vs. detach
        finally:
            detach_own_device(device)
            cleanup_allowed = True

        subprocess.run(
            ("hdiutil", "convert", str(rw_dmg), "-format", "UDZO",
             "-imagekey", "zlib-level=9", "-o", str(staged_dmg)),
            check=True,
        )
        subprocess.run(
            ("codesign", "--force", "--timestamp", "--sign", identity, str(staged_dmg)),
            check=True,
        )
        # Auch ein später Notarisierungsfehler lässt das bisherige fertige Paket
        # bestehen. Erst der vollständig geprüfte Kandidat bekommt seinen Namen.
        subprocess.run(
            ("xcrun", "notarytool", "submit", str(staged_dmg),
             "--keychain-profile", notary_profile, "--wait"),
            check=True,
        )
        subprocess.run(("xcrun", "stapler", "staple", str(staged_dmg)), check=True)
        subprocess.run(("xcrun", "stapler", "validate", str(staged_dmg)), check=True)
        os.replace(staged_dmg, dmg)
        return dmg
    finally:
        if cleanup_allowed:
            shutil.rmtree(staging)
        else:
            print(f"Arbeitsimage wegen ungeklärtem Mount erhalten: {staging}", file=sys.stderr)


def release_macos(finder_layout: bool = True) -> None:
    """Baut App und DMG und notarisiert beide — nur über das Schlüsselbundprofil.

    Die Reihenfolge ist der Punkt: Erst wird die `.app` gebaut, notarisiert und
    gestapelt, und ERST DANN wird das DMG daraus gepackt. Andersherum trüge nur
    das Image ein Ticket; wer die App herauszieht, hätte ein Bundle ohne eigenes —
    das Ticket des DMG reist nicht mit.
    """
    environment = release_environment()
    identity = developer_id_identity(environment)
    environment["APPLE_SIGNING_IDENTITY"] = identity
    profile = require_notary_profile(environment)

    run_tauri_build("app", environment, create_updater_artifacts=True)
    notarize_and_staple_app(profile)
    bundled_app = ROOT / "target/release/bundle/macos/Doppeldecker.app"
    require_macos_release_arch(bundled_app, environment)
    updater_archive = repack_and_sign_macos_updater(bundled_app, environment)
    verify_macos_updater_archive(updater_archive, environment)

    # Erst nach der Archivprüfung in den Root exportieren. Der DMG-Schritt bekommt
    # damit denselben fertig gestapelten App-Stand wie das Updaterarchiv.
    export_app_to_root()
    build_dmg_from_app(ROOT / "Doppeldecker.app", identity, finder_layout, profile)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "platform",
        choices=("macos", "macos-public", "macos-install", "macos-release", "linux"),
    )
    parser.add_argument(
        "--no-finder-layout",
        action="store_true",
        help="DMG ohne AppleScript-Fensterlayout packen (headless; nur macos-release)",
    )
    args = parser.parse_args()
    is_macos = args.platform.startswith("macos")
    # Beide öffentlichen Namen benutzen denselben Schlüsselbundprofil-Weg. Der
    # npm-Alias bleibt kompatibel, öffnet aber kein Finder-Fenster.
    if args.platform in {"macos-release", "macos-public"}:
        # Der npm-Einstieg ist headless; nur ./release darf das optionale
        # Finder-Layout des DMG kurz konfigurieren.
        finder_layout = args.platform == "macos-release" and not args.no_finder_layout
        release_macos(finder_layout=finder_layout)
        return
    # „macos-install" baut einen lokalen Installationskandidaten: Developer-ID-
    # signiert und notarisiert (stabile Signatur => macOS-Berechtigungen bleiben
    # erhalten), aber ohne DMG und ohne Notarisierungs-Umgebungsvariablen — die
    # App selbst wird über das notarytool-Schlüsselbundprofil eingereicht.
    is_install = args.platform == "macos-install"
    bundles = "app" if is_install else ("app,dmg" if is_macos else "deb,appimage")
    environment = release_environment()
    if is_install:
        environment["APPLE_SIGNING_IDENTITY"] = developer_id_identity(environment)
    if args.platform == "linux":
        # Linux signiert erst nach dem Ausführen der aus beiden Paketen
        # entnommenen CLI-Binaries. Tauri darf hier keine frühe Signatur erzeugen.
        require_updater_signing_key(environment)
    profile = require_notary_profile(environment) if is_install else ""
    run_tauri_build(
        bundles,
        environment,
        create_updater_artifacts=False,
    )
    if is_install:
        notarize_and_staple_app(profile)
    if is_macos:
        # Erst nach der Notarisierung exportieren, damit im Root nie ein
        # Zwischenstand ohne fertiges Ticket liegt.
        export_app_to_root()


if __name__ == "__main__":
    main()
