"""Regressionstests für die hashgebundene AppImage-Attestation."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from module_loader import load_tool_module


ROOT = Path(__file__).resolve().parents[2]


def load_linux_verifier():
    return load_tool_module("doppeldecker_linux_release", "verify-linux-release.py")


def write_elf(path: Path, machine: int = 62) -> None:
    header = bytearray(64)
    header[:16] = b"\x7fELF\x02\x01\x01" + b"\x00" * 9
    header[18:20] = machine.to_bytes(2, "little")
    path.write_bytes(header + b"payload")


def write_cli_version(path: Path, version: str) -> None:
    path.write_text(f"#!/bin/sh\nprintf 'doppeldecker-cli {version}\\n'\n")
    path.chmod(0o755)


class LinuxReleaseAttestationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_linux_verifier()

    def test_appimage_desktop_erlaubt_regulaere_datei_und_internen_standardlink(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            installed = root / "usr/share/applications/Doppeldecker.desktop"
            installed.parent.mkdir(parents=True)
            installed.write_text("[Desktop Entry]\nName=Doppeldecker\n")
            desktop = root / "Doppeldecker.desktop"
            desktop.write_bytes(installed.read_bytes())
            self.assertEqual(self.module.require_appimage_desktop(root), desktop)
            desktop.unlink()
            desktop.symlink_to("usr/share/applications/Doppeldecker.desktop")
            self.assertEqual(self.module.require_appimage_desktop(root), installed)
            desktop.unlink()
            desktop.symlink_to(installed)
            with self.assertRaises(SystemExit):
                self.module.require_appimage_desktop(root)

    def test_appimage_desktop_verweigert_fremde_und_unbelegte_linkziele(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            outside = base / "outside.desktop"
            outside.write_text("foreign")
            for index, target in enumerate((str(outside), "../outside.desktop", "missing.desktop")):
                root = base / str(index)
                root.mkdir()
                (root / "Doppeldecker.desktop").symlink_to(target)
                with self.subTest(target=target), self.assertRaises(SystemExit):
                    self.module.require_appimage_desktop(root)
            self.assertEqual(outside.read_text(), "foreign")

    def test_appimage_desktop_verweigert_verlinkte_eltern_und_zieldatei(self) -> None:
        for parent_link in (False, True):
            with self.subTest(parent_link=parent_link), tempfile.TemporaryDirectory() as temporary:
                base = Path(temporary)
                root = base / "appdir"
                root.mkdir()
                outside = base / "foreign"
                installed = outside / "share/applications/Doppeldecker.desktop"
                installed.parent.mkdir(parents=True)
                installed.write_text("foreign")
                (root / "Doppeldecker.desktop").symlink_to(
                    "usr/share/applications/Doppeldecker.desktop"
                )
                if parent_link:
                    (root / "usr").symlink_to(outside, target_is_directory=True)
                else:
                    target = root / "usr/share/applications/Doppeldecker.desktop"
                    target.parent.mkdir(parents=True)
                    target.symlink_to(installed)
                with self.assertRaises(SystemExit):
                    self.module.require_appimage_desktop(root)
                self.assertEqual(installed.read_text(), "foreign")

    def test_elf_architektur_wird_aus_der_datei_gelesen(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "binary"
            write_elf(binary)
            self.assertEqual(self.module.elf_architecture(binary), "x86_64")
            write_elf(binary, machine=183)
            with self.assertRaisesRegex(SystemExit, "x86_64"):
                self.module.elf_architecture(binary)

    def test_attestation_bindet_version_architektur_und_sha256(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            appimage = Path(temporary) / "Doppeldecker_1.2.3_amd64.AppImage"
            write_elf(appimage)

            def sign(payload: Path, _environment: dict[str, str]) -> Path:
                signature = payload.with_name(payload.name + ".sig")
                signature.write_text("stub signature\n")
                return signature

            attestation = self.module.write_appimage_attestation(
                appimage,
                "1.2.3",
                {"TAURI_SIGNING_PRIVATE_KEY": "test"},
                sign=sign,
            )
            document = json.loads(attestation.read_text())
            expected_hash = hashlib.sha256(appimage.read_bytes()).hexdigest()
            signature_exists = attestation.with_name(attestation.name + ".sig").is_file()

        self.assertEqual(document["version"], "1.2.3")
        self.assertEqual(document["elf_architecture"], "x86_64")
        self.assertEqual(document["sha256"], expected_hash)
        self.assertTrue(signature_exists)

    def test_signierfehler_hinterlaesst_keine_veraltete_attestationssignatur(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            appimage = Path(temporary) / "Doppeldecker_1.2.3_amd64.AppImage"
            write_elf(appimage)
            attestation = appimage.with_name(appimage.name + ".attestation.json")
            stale_signature = attestation.with_name(attestation.name + ".sig")
            stale_signature.write_text("alt\n")

            def fail(_payload: Path, _environment: dict[str, str]) -> Path:
                raise subprocess.CalledProcessError(1, ("tauri", "signer"))

            with self.assertRaises(subprocess.CalledProcessError):
                self.module.write_appimage_attestation(
                    appimage, "1.2.3", {}, sign=fail
                )

            self.assertFalse(stale_signature.exists())

    def test_linux_finalisierung_signiert_deb_vor_der_appimage_attestation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            deb = root / "Doppeldecker_1.2.3_amd64.deb"
            deb.write_bytes(b"deb")
            appimage = root / "Doppeldecker_1.2.3_amd64.AppImage"
            write_elf(appimage)
            deb_cli = root / "deb-cli"
            appimage_cli = root / "appimage-cli"
            write_cli_version(deb_cli, "1.2.3")
            write_cli_version(appimage_cli, "1.2.3")
            calls: list[str] = []

            def sign(payload: Path, _environment: dict[str, str]) -> Path:
                calls.append(payload.name)
                signature = payload.with_name(payload.name + ".sig")
                signature.write_text("stub\n")
                return signature

            self.module.finalize_linux_release(
                deb,
                appimage,
                "1.2.3",
                {},
                cli_payloads=(("DEB", deb_cli), ("AppImage", appimage_cli)),
                expected_digests={
                    "DEB": hashlib.sha256(deb.read_bytes()).hexdigest(),
                    "AppImage": hashlib.sha256(appimage.read_bytes()).hexdigest(),
                },
                sign=sign,
            )

        self.assertEqual(
            calls,
            [
                "Doppeldecker_1.2.3_amd64.AppImage",
                "Doppeldecker_1.2.3_amd64.deb",
                "Doppeldecker_1.2.3_amd64.AppImage.attestation.json",
            ],
        )

    def test_deb_aenderung_waehrend_der_attestationssignatur_faellt_auf(self) -> None:
        """Die dritte Signatur prüfte bisher nur das AppImage erneut.

        Ändert sich das DEB während der Attestationssignatur, gehört ``.deb.sig``
        zum früheren Inhalt, während die anschließende Prüfsummendatei den späteren
        Inhalt veröffentlicht. Die Finalisierung muss deshalb am Ende beide
        Artefakte noch einmal gegen ihren gebundenen Hash prüfen.
        """
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            deb = root / "Doppeldecker_1.2.3_amd64.deb"
            deb.write_bytes(b"deb")
            appimage = root / "Doppeldecker_1.2.3_amd64.AppImage"
            write_elf(appimage)
            deb_cli = root / "deb-cli"
            appimage_cli = root / "appimage-cli"
            write_cli_version(deb_cli, "1.2.3")
            write_cli_version(appimage_cli, "1.2.3")

            def sign(payload: Path, _environment: dict[str, str]) -> Path:
                signature = payload.with_name(payload.name + ".sig")
                signature.write_text("stub\n")
                if payload.name.endswith(".attestation.json"):
                    # Genau hier lag die Lücke: Nach dieser Signatur wurde das DEB
                    # nicht mehr gegengeprüft.
                    with deb.open("ab") as handle:
                        handle.write(b"x")
                return signature

            with self.assertRaisesRegex(SystemExit, "DEB-Artefakt änderte sich"):
                self.module.finalize_linux_release(
                    deb,
                    appimage,
                    "1.2.3",
                    {},
                    cli_payloads=(("DEB", deb_cli), ("AppImage", appimage_cli)),
                    expected_digests={
                        "DEB": hashlib.sha256(b"deb").hexdigest(),
                        "AppImage": hashlib.sha256(appimage.read_bytes()).hexdigest(),
                    },
                    sign=sign,
                )

    def test_falsche_payload_version_bricht_vor_jeder_signatur_ab(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            deb = root / "Doppeldecker_1.2.3_amd64.deb"
            deb.write_bytes(b"deb")
            appimage = root / "Doppeldecker_1.2.3_amd64.AppImage"
            write_elf(appimage)
            deb_cli = root / "deb-cli"
            appimage_cli = root / "appimage-cli"
            write_cli_version(deb_cli, "1.2.3")
            write_cli_version(appimage_cli, "1.2.2")
            calls: list[str] = []
            attestation = appimage.with_name(appimage.name + ".attestation.json")
            stale = (
                appimage.with_name(appimage.name + ".sig"),
                deb.with_name(deb.name + ".sig"),
                attestation,
                attestation.with_name(attestation.name + ".sig"),
            )
            for path in stale:
                path.write_text("veraltet\n")

            def sign(payload: Path, _environment: dict[str, str]) -> Path:
                calls.append(payload.name)
                return payload.with_name(payload.name + ".sig")

            with self.assertRaisesRegex(SystemExit, "Workspace-Version"):
                self.module.finalize_linux_release(
                    deb,
                    appimage,
                    "1.2.3",
                    {},
                    cli_payloads=(("DEB", deb_cli), ("AppImage", appimage_cli)),
                    expected_digests={
                        "DEB": hashlib.sha256(deb.read_bytes()).hexdigest(),
                        "AppImage": hashlib.sha256(appimage.read_bytes()).hexdigest(),
                    },
                    sign=sign,
                )
            self.assertEqual(calls, [])
            self.assertTrue(all(not path.exists() for path in stale))

    def test_payload_lauf_darf_originalartefakt_nicht_unbemerkt_austauschen(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            deb = root / "Doppeldecker_1.2.3_amd64.deb"
            deb.write_bytes(b"deb")
            appimage = root / "Doppeldecker_1.2.3_amd64.AppImage"
            write_elf(appimage)
            expected_digests = {
                "DEB": hashlib.sha256(deb.read_bytes()).hexdigest(),
                "AppImage": hashlib.sha256(appimage.read_bytes()).hexdigest(),
            }
            deb_cli = root / "deb-cli"
            appimage_cli = root / "appimage-cli"
            write_cli_version(deb_cli, "1.2.3")
            appimage_cli.write_text(
                "#!/bin/sh\n"
                f"printf x >> '{appimage}'\n"
                "printf 'doppeldecker-cli 1.2.3\\n'\n"
            )
            appimage_cli.chmod(0o755)
            calls: list[str] = []

            def sign(payload: Path, _environment: dict[str, str]) -> Path:
                calls.append(payload.name)
                return payload.with_name(payload.name + ".sig")

            with self.assertRaisesRegex(SystemExit, "änderte sich"):
                self.module.finalize_linux_release(
                    deb,
                    appimage,
                    "1.2.3",
                    {},
                    cli_payloads=(("DEB", deb_cli), ("AppImage", appimage_cli)),
                    expected_digests=expected_digests,
                    sign=sign,
                )

            self.assertEqual(calls, [])

    def test_deb_signierfehler_entfernt_veraltete_signatur(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            deb = Path(temporary) / "Doppeldecker_1.2.3_amd64.deb"
            deb.write_bytes(b"deb")
            signature = deb.with_name(deb.name + ".sig")
            signature.write_text("alt\n")
            with mock.patch.object(
                self.module.subprocess,
                "run",
                side_effect=subprocess.CalledProcessError(1, ("tauri", "signer")),
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.module.sign_update_artifact(deb, {})

            self.assertFalse(signature.exists())

    def test_debian_abhaengigkeiten_werden_als_exakte_paketnamen_geprueft(self) -> None:
        names = self.module.dependency_package_names(
            "libwebkit2gtk-4.1-0 (>= 2.44) | webkit-fallback, "
            "libgtk-3-0:any (>= 3.24), libc6"
        )
        self.assertIn("libwebkit2gtk-4.1-0", names)
        self.assertIn("libgtk-3-0", names)
        self.assertNotIn("libgtk-3-0-fake", names)

        fake_names = self.module.dependency_package_names(
            "libwebkit2gtk-4.1-0-fake, libgtk-3-0-fake"
        )
        self.assertNotIn("libwebkit2gtk-4.1-0", fake_names)
        self.assertNotIn("libgtk-3-0", fake_names)


if __name__ == "__main__":
    unittest.main()
