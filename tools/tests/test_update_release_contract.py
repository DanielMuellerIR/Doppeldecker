"""Hält Signier- und Manifestgrenzen des manuellen Updaters fest."""

import base64
import hashlib
import io
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import unittest.mock

from module_loader import load_tool_module


ROOT = Path(__file__).resolve().parents[2]


TEST_KEY_ID = bytes.fromhex("0102030405060708")
# RFC 8032, Testvektor 1. Der Schlüssel ist ausschließlich öffentliches
# Testmaterial und kann kein echtes Doppeldecker-Artefakt signieren.
TEST_PRIVATE_SEED = bytes.fromhex(
    "9d61b19deffd5a60ba844af492ec2cc4"
    "4449c5697b326919703bac031cae7f60"
)
TEST_PUBLIC_RAW = bytes.fromhex(
    "d75a980182b10ab7d54bfed3c964073a"
    "0ee172f3daa62325af021a68f707511a"
)
PKCS8_ED25519_PREFIX = bytes.fromhex("302e020100300506032b657004220420")


def test_public_key() -> str:
    encoded = base64.b64encode(b"Ed" + TEST_KEY_ID + TEST_PUBLIC_RAW).decode()
    return f"untrusted comment: test key\n{encoded}\n"


def ed25519_sign(message: bytes) -> bytes:
    """Signiert mit Nodes Standardbibliothek, unabhängig vom Manifestcode."""
    script = r"""
const { createPrivateKey, sign } = require("node:crypto");
const key = createPrivateKey({
  key: Buffer.from(process.argv[1], "base64"),
  format: "der",
  type: "pkcs8",
});
process.stdout.write(sign(null, Buffer.from(process.argv[2], "base64"), key).toString("base64"));
"""
    key = base64.b64encode(PKCS8_ED25519_PREFIX + TEST_PRIVATE_SEED).decode()
    payload = base64.b64encode(message).decode()
    result = subprocess.run(
        ("node", "-e", script, key, payload), check=True, capture_output=True, text=True
    )
    return base64.b64decode(result.stdout)


def write_signature(path: Path, payload: Path) -> None:
    digest = hashlib.blake2b(payload.read_bytes(), digest_size=64).digest()
    signature = ed25519_sign(digest)
    trusted = "test fixture"
    global_signature = ed25519_sign(signature + trusted.encode())
    first = base64.b64encode(b"ED" + TEST_KEY_ID + signature).decode()
    second = base64.b64encode(global_signature).decode()
    document = (
        "untrusted comment: test signature\n"
        f"{first}\ntrusted comment: {trusted}\n{second}\n"
    )
    path.write_text(base64.b64encode(document.encode()).decode() + "\n")


def macho(cpu_type: int) -> bytes:
    return b"\xcf\xfa\xed\xfe" + cpu_type.to_bytes(4, "little") + b"\x00" * 16


def write_macos_archive(
    path: Path,
    version: str,
    cpu_type: int = 0x0100000C,
    *,
    cli_cpu_type: int | None = None,
    gui_symlink: bool = False,
) -> None:
    """Baut ein minimales `.app.tar.gz` mit genau den geprüften Kennzeichen.

    Enthalten sind nur `Info.plist` und der Mach-O-Kopf des Hauptbinaries — mehr
    liest das Manifestwerkzeug nicht, und mehr braucht der Vertragstest nicht.
    """
    plist = plistlib.dumps({"CFBundleShortVersionString": version})
    cli_cpu_type = cpu_type if cli_cpu_type is None else cli_cpu_type
    with tarfile.open(path, "w:gz") as bundle:
        payloads = (
            ("Doppeldecker.app/Contents/Info.plist", plist),
            ("Doppeldecker.app/Contents/MacOS/doppeldecker", macho(cpu_type)),
            (
                "Doppeldecker.app/Contents/MacOS/doppeldecker-cli",
                macho(cli_cpu_type),
            ),
        )
        for name, payload in payloads:
            info = tarfile.TarInfo(name)
            if gui_symlink and name.endswith("/doppeldecker"):
                info.type = tarfile.SYMTYPE
                info.linkname = "doppeldecker-cli"
                bundle.addfile(info)
                continue
            info.size = len(payload)
            bundle.addfile(info, io.BytesIO(payload))


def ar_member(name: str, payload: bytes) -> bytes:
    header = (
        f"{name + '/':<16}{0:<12}{0:<6}{0:<6}{0o100644:<8o}{len(payload):<10}`\n"
    ).encode("ascii")
    return header + payload + (b"\n" if len(payload) % 2 else b"")


def write_deb(path: Path, version: str, architecture: str = "amd64") -> None:
    control = (
        "Package: doppeldecker\n"
        f"Version: {version}\n"
        f"Architecture: {architecture}\n"
        "Description: test\n"
    ).encode()
    control_buffer = io.BytesIO()
    with tarfile.open(fileobj=control_buffer, mode="w:gz") as archive:
        info = tarfile.TarInfo("./control")
        info.size = len(control)
        archive.addfile(info, io.BytesIO(control))
    data_buffer = io.BytesIO()
    with tarfile.open(fileobj=data_buffer, mode="w:gz"):
        pass
    path.write_bytes(
        b"!<arch>\n"
        + ar_member("debian-binary", b"2.0\n")
        + ar_member("control.tar.gz", control_buffer.getvalue())
        + ar_member("data.tar.gz", data_buffer.getvalue())
    )


def write_appimage(path: Path, machine: int = 62) -> None:
    # ELF64 little-endian; e_machine liegt an Byte 18. Der Rest ist für die
    # Architekturprüfung absichtlich minimiert.
    header = bytearray(64)
    header[:16] = b"\x7fELF\x02\x01\x01" + b"\x00" * 9
    header[16:18] = (2).to_bytes(2, "little")
    header[18:20] = machine.to_bytes(2, "little")
    path.write_bytes(header + b"test-appimage")


def write_attestation(path: Path, appimage: Path, version: str) -> None:
    document = {
        "schema": "doppeldecker-linux-attestation-v1",
        "artifact": appimage.name,
        "sha256": hashlib.sha256(appimage.read_bytes()).hexdigest(),
        "version": version,
        "elf_architecture": "x86_64",
    }
    path.write_text(json.dumps(document, sort_keys=True, separators=(",", ":")) + "\n")


# Genau die Dateien, die der Preflight liest. Ein Abbild daraus erlaubt es, den
# echten Prüflauf gegen gezielt veränderte Eingaben zu starten, statt seinen
# Quelltext nach Zeichenketten zu durchsuchen.
PREFLIGHT_INPUTS = (
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "LICENSE",
    "CHANGELOG.md",
    "README.md",
    "docs/release-plan.md",
    "src-tauri/tauri.conf.json",
    "src-tauri/Info.plist",
    "src-tauri/icons/icon.png",
    "tools/check-release.py",
    "tools/build-release.py",
)


def stage_release_preflight_repo(directory: Path) -> Path:
    """Kopiert den aktuellen Projektstand, soweit der Preflight ihn liest."""
    for relative in PREFLIGHT_INPUTS:
        target = directory / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / relative, target)
    sources = directory / "src-tauri/src"
    sources.mkdir(parents=True, exist_ok=True)
    for module in (ROOT / "src-tauri/src").glob("*.rs"):
        shutil.copy2(module, sources / module.name)
    return directory


def run_release_preflight(repository: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        (sys.executable, str(repository / "tools/check-release.py")),
        capture_output=True,
        text=True,
        check=False,
    )


def load_build_release_module():
    """Lädt den Buildtreiber als Modul, damit einzelne Schritte injizierbar sind."""
    return load_tool_module("doppeldecker_build_release", "build-release.py")


def load_manifest_module():
    return load_tool_module(
        "doppeldecker_update_manifest", "write-update-manifest.py"
    )


class UpdateReleaseContractTests(unittest.TestCase):
    def test_app_enthaelt_eine_startfaehige_updater_konfiguration(self) -> None:
        config = json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())
        updater = config.get("plugins", {}).get("updater")
        self.assertIsInstance(
            updater,
            dict,
            "Ohne plugins.updater bricht das Tauri-Plugin bereits beim App-Start ab",
        )
        public_key = updater.get("pubkey", "")
        decoded_key = base64.b64decode(public_key, validate=True).decode("ascii")
        self.assertIn("untrusted comment: minisign public key", decoded_key)
        self.assertIn("RW", decoded_key)
        self.assertEqual(
            updater.get("endpoints"),
            [
                "https://github.com/DanielMuellerIR/Doppeldecker/"
                "releases/latest/download/latest.json"
            ],
        )

    def test_nur_der_updaterlauf_erzeugt_signierte_updater_artefakte(self) -> None:
        """Beobachtet die tatsächlich an Tauri übergebene Bundlekonfiguration."""
        module = load_build_release_module()
        commands: list[tuple] = []
        module.install_node_dependencies = lambda environment: None
        module.prepare_cli_sidecar = lambda environment: "../target/sidecars/doppeldecker-cli"
        signing = {
            "TAURI_SIGNING_PRIVATE_KEY": "geheim",
            "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "auch-geheim",
        }

        with unittest.mock.patch.object(
            module.subprocess, "run", lambda command, **kwargs: commands.append(command)
        ):
            module.run_tauri_build("app", dict(signing), create_updater_artifacts=True)
            module.run_tauri_build(
                "deb,appimage", dict(signing), create_updater_artifacts=False
            )

        self.assertEqual(len(commands), 2)
        bundles = []
        for command in commands:
            self.assertNotIn("geheim", " ".join(command), "Ein Secret steht in argv")
            override = json.loads(command[command.index("--config") + 1])["bundle"]
            self.assertEqual(
                override["externalBin"], ["../target/sidecars/doppeldecker-cli"]
            )
            bundles.append(override)
        self.assertIs(bundles[0].get("createUpdaterArtifacts"), True)
        self.assertNotIn(
            "createUpdaterArtifacts",
            bundles[1],
            "Linux signiert erst nach dem Ausführen der entnommenen CLI-Binaries",
        )

    def test_install_preflight_lehnt_eine_startunfaehige_updater_konfiguration_ab(self) -> None:
        """Führt den echten Preflight gegen ein kopiertes, verändertes Abbild aus.

        Grundlage ist der aktuelle Projektstand; pro Fall wird genau eine Sache
        verändert. Beobachtet werden Exit-Code und Meldung des Skripts.
        """
        with tempfile.TemporaryDirectory() as temporary:
            repository = stage_release_preflight_repo(Path(temporary))
            config_path = repository / "src-tauri/tauri.conf.json"
            original = config_path.read_text()
            self.assertEqual(
                run_release_preflight(repository).returncode,
                0,
                "Das kopierte Abbild bildet den grünen Projektstand nicht ab",
            )

            def failure_for(change) -> str:
                config = json.loads(original)
                change(config)
                config_path.write_text(json.dumps(config))
                try:
                    result = run_release_preflight(repository)
                finally:
                    config_path.write_text(original)
                self.assertNotEqual(result.returncode, 0)
                return result.stdout + result.stderr

            def drop_updater(config: dict) -> None:
                config["plugins"].pop("updater")

            def break_public_key(config: dict) -> None:
                config["plugins"]["updater"]["pubkey"] = base64.b64encode(
                    b"untrusted comment: irgendwas\nAB\n"
                ).decode()

            def unbind_endpoint(config: dict) -> None:
                config["plugins"]["updater"]["endpoints"] = [
                    "http://updates.example.test/latest.json"
                ]

            self.assertIn("plugins.updater fehlt", failure_for(drop_updater))
            self.assertIn("Updater-Schlüssel", failure_for(break_public_key))
            self.assertIn("Updater-Endpunkt ist nicht exakt", failure_for(unbind_endpoint))

    def test_manifest_bindet_genau_die_drei_unterstuetzten_releaseziele(self) -> None:
        """Das gebaute Manifest enthält genau die drei ausgelieferten Ziele."""
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(Path(temporary), "0.1.124")
            manifest = module.build_manifest(
                "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
            )

        self.assertEqual(
            set(manifest["platforms"]),
            {"darwin-aarch64-app", "linux-x86_64-appimage", "linux-x86_64-deb"},
        )
        base = (
            "https://github.com/DanielMuellerIR/Doppeldecker/"
            "releases/download/v0.1.124/"
        )
        for platform, suffix in (
            ("darwin-aarch64-app", ".app.tar.gz"),
            ("linux-x86_64-appimage", ".AppImage"),
            ("linux-x86_64-deb", ".deb"),
        ):
            url = manifest["platforms"][platform]["url"]
            self.assertTrue(url.startswith(base), f"{platform}: {url}")
            self.assertTrue(url.endswith(suffix), f"{platform}: {url}")

    def test_manifest_findet_ein_artefakt_nur_an_seinem_gebundenen_ort(self) -> None:
        """Ein Paket im falschen Bundleordner wird nicht ersatzweise gefunden."""
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            bundle = self.stage_bundle(Path(temporary), "0.1.124")
            module.BUNDLE_ROOT = bundle
            appimage = next((bundle / "appimage").glob("*.AppImage"))
            for path in (appimage, appimage.with_name(appimage.name + ".sig")):
                shutil.move(path, bundle / "macos" / path.name)
            with self.assertRaises(SystemExit) as failure:
                module.build_manifest("0.1.124", "2026-08-06T18:00:00Z")
        self.assertIn("appimage/*.AppImage", str(failure.exception))

    @staticmethod
    def stage_bundle(directory: Path, version: str, **overrides) -> Path:
        """Legt einen vollständigen, signierten Bundlebaum für genau eine Version an."""
        bundle = Path(directory)
        (bundle / "macos").mkdir()
        (bundle / "appimage").mkdir()
        (bundle / "deb").mkdir()
        macos = bundle / "macos" / "Doppeldecker.app.tar.gz"
        appimage_version = overrides.get("appimage_version", version)
        linux = bundle / "appimage" / f"Doppeldecker_{appimage_version}_amd64.AppImage"
        deb = bundle / "deb" / f"Doppeldecker_{version}_amd64.deb"
        write_macos_archive(
            macos,
            overrides.get("macos_version", version),
            overrides.get("cpu_type", 0x0100000C),
            cli_cpu_type=overrides.get("cli_cpu_type"),
            gui_symlink=overrides.get("gui_symlink", False),
        )
        write_appimage(linux, overrides.get("appimage_machine", 62))
        write_deb(
            deb,
            overrides.get("deb_version", version),
            overrides.get("deb_architecture", "amd64"),
        )
        attestation = linux.with_name(linux.name + ".attestation.json")
        write_attestation(
            attestation,
            linux,
            overrides.get("attestation_version", version),
        )
        for payload in (macos, linux, deb, attestation):
            write_signature(payload.with_name(payload.name + ".sig"), payload)
        return bundle

    def test_manifest_liest_die_signaturen_der_exakten_nutzlasten(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(Path(temporary), "0.1.124")

            manifest = module.build_manifest(
                "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
            )

        self.assertEqual(manifest["version"], "0.1.124")
        self.assertEqual(
            base64.b64decode(
                manifest["platforms"]["darwin-aarch64-app"]["signature"]
            ).decode().splitlines()[0],
            "untrusted comment: test signature",
        )
        self.assertEqual(
            base64.b64decode(
                manifest["platforms"]["linux-x86_64-appimage"]["signature"]
            ).decode().splitlines()[0],
            "untrusted comment: test signature",
        )
        self.assertEqual(
            base64.b64decode(
                manifest["platforms"]["linux-x86_64-deb"]["signature"]
            ).decode().splitlines()[0],
            "untrusted comment: test signature",
        )
        self.assertTrue(
            manifest["platforms"]["linux-x86_64-appimage"]["url"].endswith(
                "/Doppeldecker_0.1.124_amd64.AppImage"
            )
        )

    def test_manifest_lehnt_ein_altes_linux_paket_ab(self) -> None:
        """Ein liegengebliebenes Paket darf nicht unter der neuen Version erscheinen."""
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", appimage_version="0.1.123"
            )
            with self.assertRaises(SystemExit) as failure:
                module.build_manifest("0.1.124", "2026-08-06T18:00:00Z")
        self.assertIn("Doppeldecker_0.1.124_amd64.AppImage", str(failure.exception))

    def test_manifest_lehnt_ein_altes_macos_archiv_ab(self) -> None:
        """Das macOS-Archiv heißt immer gleich; nur sein Inhalt belegt die Version."""
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", macos_version="0.1.123"
            )
            with self.assertRaises(SystemExit) as failure:
                module.build_manifest("0.1.124", "2026-08-06T18:00:00Z")
        self.assertIn("0.1.123", str(failure.exception))

    def test_manifest_lehnt_eine_fremde_architektur_ab(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            # CPU_TYPE_X86_64 statt arm64.
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", cpu_type=0x01000007
            )
            with self.assertRaises(SystemExit) as failure:
                module.build_manifest("0.1.124", "2026-08-06T18:00:00Z")
        self.assertIn("arm64", str(failure.exception))

    def test_manifest_verlangt_auch_einen_regulaeren_arm64_cli_sidecar(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", cli_cpu_type=0x01000007
            )
            with self.assertRaisesRegex(SystemExit, "doppeldecker-cli.*arm64"):
                module.build_manifest(
                    "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
                )

    def test_manifest_lehnt_einen_macho_symlink_ab(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", gui_symlink=True
            )
            with self.assertRaisesRegex(SystemExit, "reguläre Datei"):
                module.build_manifest(
                    "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
                )

    def test_manifest_prueft_deb_metadaten_aus_der_nutzlast(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", deb_version="0.1.123"
            )
            with self.assertRaisesRegex(SystemExit, "DEB-Version"):
                module.build_manifest(
                    "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
                )

    def test_manifest_bindet_appimage_version_an_signierte_hash_attestation(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(
                Path(temporary), "0.1.124", attestation_version="0.1.123"
            )
            with self.assertRaisesRegex(SystemExit, "AppImage-Version"):
                module.build_manifest(
                    "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
                )

    def test_veraltete_signatur_nach_nutzlastaenderung_scheitert(self) -> None:
        module = load_manifest_module()
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(Path(temporary), "0.1.124")
            archive = next((module.BUNDLE_ROOT / "macos").glob("*.app.tar.gz"))
            # Gzip/TAR ignoriert angehängte Bytes beim Lesen; die Inhaltsprüfung
            # bleibt daher grün, während nur die kryptografische Nutzlastbindung
            # den manipulierten Bytestrom erkennt.
            archive.write_bytes(archive.read_bytes() + b"manipuliert")
            with self.assertRaisesRegex(SystemExit, "Signaturprüfung"):
                module.build_manifest(
                    "0.1.124", "2026-08-06T18:00:00Z", public_key=test_public_key()
                )

    def test_falscher_oeffentlicher_schluessel_scheitert(self) -> None:
        module = load_manifest_module()
        wrong_raw = bytes([TEST_PUBLIC_RAW[0] ^ 1]) + TEST_PUBLIC_RAW[1:]
        wrong = base64.b64encode(b"Ed" + TEST_KEY_ID + wrong_raw).decode()
        wrong_key = f"untrusted comment: wrong\n{wrong}\n"
        with tempfile.TemporaryDirectory() as temporary:
            module.BUNDLE_ROOT = self.stage_bundle(Path(temporary), "0.1.124")
            with self.assertRaisesRegex(SystemExit, "Signaturprüfung"):
                module.build_manifest(
                    "0.1.124", "2026-08-06T18:00:00Z", public_key=wrong_key
                )

    def test_pub_date_wird_vollstaendig_als_rfc_3339_geprueft(self) -> None:
        module = load_manifest_module()
        self.assertEqual(
            module.canonical_pub_date("2026-08-06T18:00:00Z"), "2026-08-06T18:00:00Z"
        )
        # Bruchteilssekunden sind gültiges RFC 3339 und werden kanonisch gekürzt.
        self.assertEqual(
            module.canonical_pub_date("2026-08-06T18:00:00.250Z"), "2026-08-06T18:00:00Z"
        )
        for invalid in (
            "not-a-T-dateZ",
            # UTC bleibt Pflicht: Ein Offset wäre zwar RFC 3339, aber nicht der
            # im Manifest zugesicherte Wert.
            "2026-08-06T20:00:00+02:00",
            "2026-02-30T18:00:00Z",
            "2026-08-06T25:00:00Z",
            "2026-08-06 18:00:00Z",
            "2026-08-06T18:00:00",
        ):
            with self.subTest(invalid=invalid), self.assertRaises(SystemExit):
                module.canonical_pub_date(invalid)


if __name__ == "__main__":
    unittest.main()
