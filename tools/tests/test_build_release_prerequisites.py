"""Regressionstests für frühe, mutationslose Release-Voraussetzungen."""

from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from module_loader import load_tool_module


ROOT = Path(__file__).resolve().parents[2]


def load_build_release():
    return load_tool_module(
        "doppeldecker_build_release_prerequisites", "build-release.py"
    )


class BuildReleasePrerequisitesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_build_release()

    def test_tauri_build_installiert_gesperrte_node_abhaengigkeiten_zuerst(self) -> None:
        calls: list[tuple[str, ...]] = []

        def record(command, **_kwargs):
            calls.append(tuple(command))
            return subprocess.CompletedProcess(command, 0)

        with (
            mock.patch.object(self.module.subprocess, "run", side_effect=record),
            mock.patch.object(
                self.module,
                "prepare_cli_sidecar",
                return_value="../target/sidecars/doppeldecker-cli",
            ),
        ):
            self.module.run_tauri_build("app", {"PATH": "/usr/bin"})

        self.assertEqual(calls[0], ("pnpm", "install", "--frozen-lockfile"))
        self.assertEqual(calls[1][:4], ("pnpm", "exec", "tauri", "build"))

    def test_fehlendes_pnpm_meldet_die_voraussetzung_verstaendlich(self) -> None:
        with mock.patch.object(
            self.module.subprocess, "run", side_effect=FileNotFoundError("pnpm")
        ):
            with self.assertRaisesRegex(SystemExit, "pnpm wurde nicht gefunden"):
                self.module.install_node_dependencies({})

    def test_oeffentlicher_build_bricht_vor_werkzeugen_ohne_signierschluessel_ab(self) -> None:
        with mock.patch.object(self.module.subprocess, "run") as run:
            for incomplete in (
                {},
                {"TAURI_SIGNING_PRIVATE_KEY": "   "},
                {"TAURI_SIGNING_PRIVATE_KEY": "test"},
            ):
                with self.assertRaisesRegex(SystemExit, "TAURI_SIGNING_PRIVATE_KEY"):
                    self.module.run_tauri_build(
                        "app", incomplete, create_updater_artifacts=True
                    )

        run.assert_not_called()

    def test_notary_profil_ohne_tty_bricht_ohne_prompt_ab(self) -> None:
        with (
            mock.patch.object(self.module, "git_config_notary_profile", return_value=""),
            mock.patch.object(self.module.sys.stdin, "isatty", return_value=False),
            mock.patch("builtins.input") as prompt,
        ):
            with self.assertRaisesRegex(SystemExit, "Kein verwendbares notarytool-Profil"):
                self.module.resolve_notary_profile({})

        prompt.assert_not_called()

    def test_macos_public_alias_nutzt_den_kanonischen_schluesselbundprofil_weg(self) -> None:
        with (
            mock.patch.object(self.module.sys, "argv", ["build-release.py", "macos-public"]),
            mock.patch.object(self.module, "release_macos") as release,
        ):
            self.module.main()

        release.assert_called_once_with(finder_layout=False)

    def test_linux_baut_ohne_fruehe_tauri_signatur_nach_schluessel_preflight(self) -> None:
        environment = {
            "TAURI_SIGNING_PRIVATE_KEY": "test",
            "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "test",
        }
        with (
            mock.patch.object(self.module.sys, "argv", ["build-release.py", "linux"]),
            mock.patch.object(self.module, "release_environment", return_value=environment),
            mock.patch.object(self.module, "require_updater_signing_key") as require_key,
            mock.patch.object(self.module, "run_tauri_build") as build,
        ):
            self.module.main()

        require_key.assert_called_once_with(environment)
        build.assert_called_once_with(
            "deb,appimage", environment, create_updater_artifacts=False
        )

    def test_macos_release_packt_updater_erst_nach_app_ticket_neu(self) -> None:
        """Das von Tauri vorab erzeugte Archiv darf nie ausgeliefert werden."""
        calls: list[str] = []
        tauri_arguments: list[tuple[str, dict[str, str], bool]] = []

        def record(name, result=None):
            def function(*_args, **_kwargs):
                calls.append(name)
                return result

            return function

        def record_tauri(
            bundles: str,
            environment: dict[str, str],
            *,
            create_updater_artifacts: bool = False,
        ) -> None:
            calls.append("tauri")
            tauri_arguments.append((bundles, environment, create_updater_artifacts))

        archive = ROOT / "target/release/bundle/macos/Doppeldecker.app.tar.gz"
        dmg = ROOT / "target/release/bundle/dmg/Doppeldecker_test_aarch64.dmg"
        with (
            mock.patch.object(self.module, "release_environment", return_value={}),
            mock.patch.object(self.module, "developer_id_identity", return_value="Developer ID"),
            mock.patch.object(self.module, "require_notary_profile", return_value="notary"),
            mock.patch.object(self.module, "run_tauri_build", side_effect=record_tauri),
            mock.patch.object(
                self.module, "notarize_and_staple_app", side_effect=record("app-ticket")
            ),
            mock.patch.object(
                self.module, "require_macos_release_arch", side_effect=record("app-arch")
            ),
            mock.patch.object(
                self.module,
                "repack_and_sign_macos_updater",
                side_effect=record("updater-neu", archive),
            ),
            mock.patch.object(
                self.module,
                "verify_macos_updater_archive",
                side_effect=record("updater-pruefen"),
            ),
            mock.patch.object(self.module, "export_app_to_root", side_effect=record("export")),
            mock.patch.object(
                self.module, "build_dmg_from_app", side_effect=record("dmg", dmg)
            ) as build_dmg,
        ):
            self.module.release_macos(finder_layout=False)

        self.assertEqual(
            calls,
            [
                "tauri",
                "app-ticket",
                "app-arch",
                "updater-neu",
                "updater-pruefen",
                "export",
                "dmg",
            ],
        )
        self.assertEqual(len(tauri_arguments), 1)
        build_dmg.assert_called_once_with(ROOT / "Doppeldecker.app", "Developer ID", False, "notary")
        bundles, environment, creates_updater = tauri_arguments[0]
        self.assertEqual(bundles, "app")
        self.assertEqual(environment["APPLE_SIGNING_IDENTITY"], "Developer ID")
        self.assertIs(
            creates_updater,
            True,
            "macOS muss den Signierschlüssel vor dem ersten Buildschritt verlangen",
        )

    def test_fehlerhafte_updater_pruefung_stoppt_vor_dmg(self) -> None:
        archive = ROOT / "target/release/bundle/macos/Doppeldecker.app.tar.gz"
        with (
            mock.patch.object(self.module, "release_environment", return_value={}),
            mock.patch.object(self.module, "developer_id_identity", return_value="Developer ID"),
            mock.patch.object(self.module, "require_notary_profile", return_value="notary"),
            mock.patch.object(self.module, "run_tauri_build"),
            mock.patch.object(self.module, "notarize_and_staple_app"),
            mock.patch.object(self.module, "require_macos_release_arch"),
            mock.patch.object(
                self.module, "repack_and_sign_macos_updater", return_value=archive
            ),
            mock.patch.object(
                self.module,
                "verify_macos_updater_archive",
                side_effect=SystemExit("Archiv-App ohne Ticket"),
            ),
            mock.patch.object(self.module, "build_dmg_from_app") as build_dmg,
        ):
            with self.assertRaisesRegex(SystemExit, "ohne Ticket"):
                self.module.release_macos(finder_layout=False)

        build_dmg.assert_not_called()

    def test_updater_wird_atomar_neu_gepackt_und_danach_signiert(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app = root / "Doppeldecker.app"
            app.mkdir()
            archive = root / "Doppeldecker.app.tar.gz"
            signature = root / "Doppeldecker.app.tar.gz.sig"
            archive.write_bytes(b"alter-inhalt")
            signature.write_text("alte-signatur\n")
            calls: list[tuple[str, ...]] = []

            def run(command, **_kwargs):
                calls.append(tuple(command))
                if command[0] == "tar":
                    Path(command[2]).write_bytes(b"fertige-app-mit-ticket")
                else:
                    self.assertEqual(archive.read_bytes(), b"fertige-app-mit-ticket")
                    self.assertFalse(signature.exists(), "alte Signatur muss vorher weg sein")
                    signature.write_text("neue-signatur\n")
                return subprocess.CompletedProcess(command, 0)

            result = self.module.repack_and_sign_macos_updater(
                app,
                {
                    "TAURI_SIGNING_PRIVATE_KEY": "test",
                    "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "test",
                },
                run_command=run,
            )

            self.assertEqual(result, archive)
            self.assertEqual(archive.read_bytes(), b"fertige-app-mit-ticket")
            self.assertEqual(signature.read_text(), "neue-signatur\n")
            self.assertEqual(calls[0][0], "tar")
            self.assertEqual(calls[1][:6], ("pnpm", "exec", "tauri", "signer", "sign", str(archive)))

    def test_signierfehler_laesst_keine_alte_archivsignatur_zurueck(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app = root / "Doppeldecker.app"
            app.mkdir()
            signature = root / "Doppeldecker.app.tar.gz.sig"
            signature.write_text("alt\n")

            def run(command, **_kwargs):
                if command[0] == "tar":
                    Path(command[2]).write_bytes(b"neu")
                    return subprocess.CompletedProcess(command, 0)
                raise subprocess.CalledProcessError(1, command)

            with self.assertRaises(subprocess.CalledProcessError):
                self.module.repack_and_sign_macos_updater(
                    app,
                    {
                        "TAURI_SIGNING_PRIVATE_KEY": "test",
                        "TAURI_SIGNING_PRIVATE_KEY_PASSWORD": "test",
                    },
                    run_command=run,
                )

            self.assertFalse(signature.exists())


if __name__ == "__main__":
    unittest.main()
