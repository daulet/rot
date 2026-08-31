from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[3]
SCRIPT = ROOT / ".github/scripts/package-deb.sh"


class DebianPackageScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.tools = self.directory / "tools"
        self.tools.mkdir()
        self.binary = self.directory / "rot"
        self.binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.binary.chmod(0o755)

        fake_dpkg = self.tools / "dpkg-deb"
        fake_dpkg.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            'test "$1" = --root-owner-group\n'
            'test "$2" = --build\n'
            'test -f "$3/usr/share/doc/rot/copyright"\n'
            'grep -F "Copyright (c) 2026 Rot contributors" '
            '"$3/usr/share/doc/rot/copyright" >/dev/null\n'
            'grep -F "Source: https://github.com/daulet/rot" '
            '"$3/usr/share/doc/rot/copyright" >/dev/null\n'
            'grep -F "MIT License" "$3/usr/share/doc/rot/copyright" >/dev/null\n'
            'grep -F "Apache License" "$3/usr/share/doc/rot/copyright" >/dev/null\n'
            'cp "$3/DEBIAN/control" "$4"\n',
            encoding="utf-8",
        )
        fake_dpkg.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_script(self, *extra: str) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment["PATH"] = f"{self.tools}:{environment['PATH']}"
        return subprocess.run(
            [
                str(SCRIPT),
                "--binary",
                str(self.binary),
                "--version",
                "1.2.3",
                "--arch",
                "amd64",
                "--out-dir",
                str(self.directory / "dist"),
                *extra,
            ],
            cwd=ROOT,
            env=environment,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def test_control_metadata_and_output_name(self) -> None:
        process = self.run_script()
        self.assertEqual(process.returncode, 0, process.stderr)
        output = self.directory / "dist/rot_1.2.3_amd64.deb"
        control = output.read_text(encoding="utf-8")
        self.assertIn("Package: rot\n", control)
        self.assertIn("Version: 1.2.3\n", control)
        self.assertIn("Architecture: amd64\n", control)
        self.assertIn("Installed-Size: ", control)

    def test_invalid_architecture_fails_before_packaging(self) -> None:
        process = self.run_script("--arch", "riscv64")
        self.assertEqual(process.returncode, 2)
        self.assertIn("unsupported Debian architecture", process.stderr)


if __name__ == "__main__":
    unittest.main()
