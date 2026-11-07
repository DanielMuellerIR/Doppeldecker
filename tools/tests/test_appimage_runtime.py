"""Paketregressionen: GIO-Pfade, vollständige Runtime und atomarer Neuaufbau."""

from pathlib import Path
import json
import subprocess
import tempfile
import unittest
from unittest import mock

from module_loader import load_tool_module


def write_elf(path: Path, machine: int = 62) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    header = bytearray(20)
    header[:6] = b"\x7fELF\x02\x01"
    header[18:20] = machine.to_bytes(2, "little")
    path.write_bytes(header)
    path.chmod(0o755)


class AppImageRuntimeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_tool_module("appimage_runtime_build", "build-release.py")
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.module.ROOT = self.root
        self.module.workspace_version = lambda: "1.2.3"
        config = self.root / "src-tauri/tauri.conf.json"
        config.parent.mkdir()
        config.write_text(json.dumps({"productName": "Doppeldecker"}))
        self.directory = self.root / "target/release/bundle/appimage"
        self.app = self.directory / "Doppeldecker.AppDir"
        write_elf(self.app / "usr/bin/doppeldecker")
        self.hook = self.app / "apprun-hooks/linuxdeploy-plugin-gtk.sh"
        self.hook.parent.mkdir()
        self.hook.write_text(
            '#!/bin/sh\nexport GTK_DATA_PREFIX="$APPDIR/usr"\n'
            'export GIO_EXTRA_MODULES="$APPDIR/usr/lib/x86_64-linux-gnu/gio/modules\n'
            '/private/build/AppDir/usr/lib/i386-linux-gnu/gio/modules"\n'
        )
        self.module64 = self.app / "usr/lib/x86_64-linux-gnu/gio/modules/libgiognutls.so"
        write_elf(self.module64)
        write_elf(self.app / "usr/lib/i386-linux-gnu/gio/modules/libgiognutls.so", 3)
        for relative in (
            "usr/lib/gstreamer-1.0/libgstapp.so",
            "usr/lib/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner",
        ):
            write_elf(self.app / relative)
        self.artifact = self.directory / "Doppeldecker_1.2.3_amd64.AppImage"
        self.artifact.write_bytes(b"original-package")
        self.tool = self.root / "cache/tauri/linuxdeploy-plugin-appimage.AppImage"
        write_elf(self.tool)
        self.environment = {"XDG_CACHE_HOME": str(self.root / "cache")}

    def test_hook_enthaelt_nur_relative_pfade_passender_architektur(self) -> None:
        write_elf(self.app / "usr/lib64/gio/modules/libgiognutls.so")
        self.assertTrue(self.module.repair_appimage_gtk_hook(self.app))
        contents = self.hook.read_text()
        self.assertNotIn("/private/build", contents)
        self.assertNotIn("i386", contents)
        self.assertIn('export GTK_DATA_PREFIX="$APPDIR/usr"', contents)
        expanded = subprocess.check_output(
            ["sh", "-c", '. "$1"; printf "%s" "$GIO_EXTRA_MODULES"', "sh", str(self.hook)],
            env={"APPDIR": "/mounted package"},
            text=True,
        )
        self.assertEqual(
            expanded,
            "/mounted package/usr/lib/x86_64-linux-gnu/gio/modules:"
            "/mounted package/usr/lib64/gio/modules",
        )
        self.assertFalse(self.module.repair_appimage_gtk_hook(self.app))

    def test_externes_modul_und_unbekannter_hook_werden_abgelehnt(self) -> None:
        self.module64.unlink()
        external = self.root / "outside.so"
        write_elf(external)
        self.module64.symlink_to(external)
        with self.assertRaisesRegex(SystemExit, "außerhalb"):
            self.module.repair_appimage_gtk_hook(self.app)
        self.module64.unlink()
        write_elf(self.module64)
        self.hook.write_text("export SOMETHING_ELSE=1\n")
        with self.assertRaisesRegex(SystemExit, "unbekanntes Format"):
            self.module.repair_appimage_gtk_hook(self.app)

    def test_packfehler_erhaelt_altes_paket_wiederholung_packt_korrigierten_hook(self) -> None:
        with mock.patch.object(self.module.subprocess, "run", side_effect=subprocess.CalledProcessError(1, ["packer"])):
            with self.assertRaises(subprocess.CalledProcessError):
                self.module.finalize_appimage_runtime(self.environment)
        self.assertEqual(self.artifact.read_bytes(), b"original-package")
        self.assertFalse(self.module.repair_appimage_gtk_hook(self.app))

        def pack(command, *, env, check):
            self.assertEqual(command[1:], ("--appimage-extract-and-run", "--appdir", str(self.app)))
            self.assertEqual(env["ARCH"], "x86_64")
            self.assertTrue(check)
            write_elf(Path(env["LDAI_OUTPUT"]))

        with mock.patch.object(self.module.subprocess, "run", side_effect=pack) as run:
            self.module.finalize_appimage_runtime(self.environment)
        run.assert_called_once()
        self.assertEqual(self.module.elf_machine(self.artifact), 62)
        self.assertFalse(list(self.directory.glob(".appimage-runtime-*")))

    def test_paket_nutzt_host_wayland_und_erhaelt_andere_bibliotheken(self) -> None:
        library = self.app / "usr/lib/libwayland-client.so.0.22.0"
        write_elf(library)
        library.with_name("libwayland-client.so.0").symlink_to(library.name)
        write_elf(self.app / "usr/lib64/libwayland-client.so.0")
        neighbor = library.with_name("libwayland-cursor.so.0")
        write_elf(neighbor)
        outside = self.root / "external-library.so"
        write_elf(outside)
        library.with_name("libwayland-client.so").symlink_to(outside)

        def pack(_command, *, env, check):
            self.assertFalse(list(self.app.rglob("libwayland-client.so*")))
            self.assertTrue(neighbor.is_file())
            self.assertTrue(outside.is_file())
            write_elf(Path(env["LDAI_OUTPUT"]))

        with mock.patch.object(self.module.subprocess, "run", side_effect=pack):
            self.module.finalize_appimage_runtime(self.environment)

    def test_pruefer_lehnt_gebuendelte_wayland_clients_auch_als_toten_link_ab(self) -> None:
        verifier = load_tool_module("appimage_host_libraries", "verify-linux-release.py")
        verifier.verify_appimage_host_libraries(self.app)
        library = self.app / "usr/lib/libwayland-client.so.0"
        library.parent.mkdir(parents=True, exist_ok=True)
        library.symlink_to("missing-version.so")
        with self.assertRaisesRegex(SystemExit, "Wayland-Client"):
            verifier.verify_appimage_host_libraries(self.app)

    def test_fehlende_runtime_und_bestehende_signatur_stoppen_vor_packer(self) -> None:
        with mock.patch.object(self.module.subprocess, "run") as run:
            (self.app / "usr/lib/gstreamer-1.0/libgstapp.so").unlink()
            with self.assertRaisesRegex(SystemExit, "GStreamer-Runtime fehlt"):
                self.module.finalize_appimage_runtime(self.environment)
            self.artifact.with_name(self.artifact.name + ".sig").write_text("signature")
            with self.assertRaisesRegex(SystemExit, "erst nach"):
                self.module.finalize_appimage_runtime(self.environment)
        run.assert_not_called()
        self.assertEqual(self.artifact.read_bytes(), b"original-package")

    def test_voraussetzungen_stoppen_vor_abhaengigkeiten_und_signierung(self) -> None:
        with (
            mock.patch.object(self.module.shutil, "which", return_value=None),
            mock.patch.object(self.module, "install_node_dependencies") as install,
        ):
            with self.assertRaisesRegex(SystemExit, "patchelf"):
                self.module.run_tauri_build("deb,appimage", {})
            with self.assertRaisesRegex(SystemExit, "separat signieren"):
                self.module.run_tauri_build("appimage", {}, create_updater_artifacts=True)
        install.assert_not_called()

    def test_fehlendes_appsink_stoppt_vor_abhaengigkeitsinstallation(self) -> None:
        with (
            mock.patch.object(self.module.shutil, "which", side_effect=lambda name, **_kwargs: name),
            mock.patch.object(self.module.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)),
            mock.patch.object(self.module, "install_node_dependencies") as install,
        ):
            with self.assertRaisesRegex(SystemExit, "appsink"):
                self.module.run_tauri_build("appimage", {})
        install.assert_not_called()


class PackagePrivacyTests(unittest.TestCase):
    def test_hook_und_symlink_werden_neben_sauberem_binary_geprueft(self) -> None:
        module = load_tool_module("appimage_runtime_privacy", "verify-release-privacy.py")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            write_elf(root / "usr/bin/app")
            hook = root / "gtk-hook.sh"
            hook.write_text('export GIO_EXTRA_MODULES="/private/build/AppDir/usr/lib"\n')
            prefixes = {"Workspace": Path("/private/build")}
            with self.assertRaisesRegex(SystemExit, "gtk-hook.sh"):
                module.scan_entries(module.runtime_entries(root, "AppImage"), prefixes)
            hook.write_text('export GIO_EXTRA_MODULES="${APPDIR}/usr/lib"\n')
            link = root / "outside-link"
            link.symlink_to("/private/build/missing.so")
            with self.assertRaisesRegex(SystemExit, "outside-link"):
                module.scan_entries(module.runtime_entries(root, "AppImage"), prefixes)
            link.unlink()
            link.symlink_to("usr/bin/app")
            self.assertEqual(module.scan_entries(module.runtime_entries(root, "AppImage"), prefixes), 3)


if __name__ == "__main__":
    unittest.main()
