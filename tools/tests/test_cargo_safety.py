"""Prüft die Cargo-Sperre mit einem isolierten Programm ohne App-Zugriff."""

from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


@unittest.skipUnless(shutil.which("cargo"), "Cargo ist nicht installiert")
class CargoUserPathSafetyTests(unittest.TestCase):
    def test_parent_environment_cannot_disable_test_user_path_guard(self):
        with tempfile.TemporaryDirectory(prefix="doppeldecker-cargo-safety-") as temp:
            root = Path(temp)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname = "cargo-safety-probe"\nversion = "0.0.0"\n'
                'edition = "2021"\n[workspace]\n', encoding="utf-8"
            )
            (root / "src/main.rs").write_text(
                'fn main() { println!("{}", std::env::var('
                '"DOPPELDECKER_NO_DEFAULT_USER_PATHS").unwrap()); }\n',
                encoding="utf-8",
            )
            for inherited in [None, "0", ""]:
                with self.subTest(inherited=inherited):
                    env = os.environ.copy()
                    env.pop("DOPPELDECKER_NO_DEFAULT_USER_PATHS", None)
                    if inherited is not None:
                        env["DOPPELDECKER_NO_DEFAULT_USER_PATHS"] = inherited
                    result = subprocess.run(
                        ["cargo", "run", "--offline", "--quiet", "--manifest-path",
                         str(root / "Cargo.toml"), "--target-dir", str(root / "target")],
                        cwd=ROOT, env=env, text=True, capture_output=True, timeout=60,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.strip(), "1")


if __name__ == "__main__":
    unittest.main()
