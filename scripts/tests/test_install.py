"""Exercise real PowerShell packaging/install/rollback/removal in isolated directories.

The tiny compiled executable replaces only the native command interface. Native hook
merge correctness is covered by Rust tests, not by this Windows distribution fixture.
Run from the repository root: python -m unittest discover -s scripts/tests -v
"""

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
POWERSHELL = shutil.which("powershell.exe")

FIXTURE_SOURCE = r"""
using System;
using System.Diagnostics;
using System.IO;
class Fixture {
    static int Main(string[] args) {
        if (args[0] == "--version") {
            Console.WriteLine("CodexProfiler VERSION");
            return 0;
        }
        string config = args[Array.IndexOf(args, "--codex-home") + 1];
        Directory.CreateDirectory(config);
        string marker = Path.Combine(config, "imported-executable.txt");
        if (args[0] == "--install-hooks") {
            if (File.Exists(Path.Combine(config, "fail-import.txt"))) {
                Console.Error.WriteLine("fixture import rejected");
                return 3;
            }
            File.WriteAllText(marker, Process.GetCurrentProcess().MainModule.FileName);
        } else if (args[0] == "--uninstall-hooks") {
            File.Delete(marker);
        } else {
            return 4;
        }
        Console.WriteLine("{\"ok\":true}");
        return 0;
    }
}
"""


def run_ps(script, *arguments):
    """Use Windows PowerShell 5.1, the oldest supported installer environment."""
    environment = os.environ.copy()
    # Codex's bundled PowerShell 7 module path is incompatible with Windows PowerShell 5.1.
    environment["PSModulePath"] = str(
        Path(environment["SYSTEMROOT"]) / "System32/WindowsPowerShell/v1.0/Modules"
    )
    return subprocess.run(
        [
            POWERSHELL,
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            str(script),
            *map(str, arguments),
        ],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=environment,
        timeout=60,
    )


@unittest.skipUnless(os.name == "nt" and POWERSHELL, "Windows PowerShell is required")
class InstallerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workspace = tempfile.TemporaryDirectory(
            prefix=".installer-tests-", dir=REPO
        )
        cls.fixture_dir = Path(cls.workspace.name)
        cls.packages = {}
        for version in ("0.2.0", "0.3.0"):
            source = cls.fixture_dir / f"fixture-{version}.cs"
            source.write_text(
                FIXTURE_SOURCE.replace("VERSION", version), encoding="utf-8"
            )
            binary = cls.fixture_dir / f"fixture-{version}.exe"
            compile_script = cls.fixture_dir / "compile.ps1"
            compile_script.write_text(
                "param($SourcePath, $OutputPath)\n"
                "$ErrorActionPreference = 'Stop'\n"
                "Add-Type -TypeDefinition (Get-Content -LiteralPath $SourcePath -Raw) "
                "-OutputAssembly $OutputPath -OutputType ConsoleApplication\n",
                encoding="utf-8",
            )
            result = run_ps(compile_script, source, binary)
            if result.returncode:
                raise AssertionError(result.stdout + result.stderr)
            output_dir = cls.fixture_dir / version
            result = run_ps(
                REPO / "scripts/package.ps1",
                "-BinaryPath",
                binary,
                "-Version",
                version,
                "-OutputDir",
                output_dir,
            )
            if result.returncode:
                raise AssertionError(result.stdout + result.stderr)
            cls.packages[version] = (
                output_dir / f"CodexProfiler-v{version}-windows-x64.zip"
            )

    @classmethod
    def tearDownClass(cls):
        cls.workspace.cleanup()

    def setUp(self):
        self.sandbox = tempfile.TemporaryDirectory(prefix="case-", dir=self.fixture_dir)
        self.base = Path(self.sandbox.name)
        self.install_dir = self.base / "Installed App space 中文"
        self.codex_home = self.base / "Codex config space 中文"

    def tearDown(self):
        self.sandbox.cleanup()

    def install(self, version="0.2.0", archive=None):
        return run_ps(
            REPO / "scripts/install.ps1",
            "-PackagePath",
            archive or self.packages[version],
            "-InstallDir",
            self.install_dir,
            "-CodexHome",
            self.codex_home,
            "-NoShortcut",
            "-NoLaunch",
        )

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def copy_archive(self, entries=None):
        """Write a separate checksum list so a modified fixture can test extraction itself."""
        archive = self.base / self.packages["0.2.0"].name
        if entries is None:
            shutil.copyfile(self.packages["0.2.0"], archive)
        else:
            with (
                zipfile.ZipFile(self.packages["0.2.0"]) as original,
                zipfile.ZipFile(archive, "w") as modified,
            ):
                for entry in original.infolist():
                    modified.writestr(entry, original.read(entry))
                for name, contents in entries.items():
                    modified.writestr(name, contents)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.base / "SHA256SUMS").write_text(
            f"{digest}  {archive.name}\n", encoding="ascii"
        )
        return archive

    # Install, repeat and upgrade must retain one stable executable path and all user data.
    def test_repeat_upgrade_and_uninstall_preserve_user_files(self):
        self.assert_success(self.install())
        executable = self.install_dir / "codex-profiler.exe"
        marker = self.codex_home / "imported-executable.txt"
        self.assertEqual(Path(marker.read_text(encoding="utf-8")), executable)
        database = self.install_dir / "profiler.sqlite3"
        foreign = self.install_dir / "notes.txt"
        database.write_bytes(b"retained profiling database")
        foreign.write_text("unrelated", encoding="utf-8")
        self.assert_success(self.install())
        self.assert_success(
            run_ps(
                self.install_dir / "scripts/import-hooks.ps1",
                "-CodexHome",
                self.codex_home,
            )
        )
        self.assert_success(
            run_ps(
                REPO / "scripts/install.ps1",
                "-PackagePath",
                self.packages["0.3.0"],
                "-InstallDir",
                self.install_dir,
                "-NoShortcut",
                "-NoLaunch",
            )
        )
        state = json.loads(
            (self.install_dir / "install-state.json").read_text(encoding="utf-8-sig")
        )
        self.assertEqual(state["version"], "0.3.0")
        self.assertEqual(Path(marker.read_text(encoding="utf-8")), executable)
        result = run_ps(self.install_dir / "scripts/uninstall.ps1")
        self.assert_success(result)
        self.assertFalse(executable.exists())
        self.assertFalse(marker.exists())
        self.assertFalse((self.install_dir / "install-state.json").exists())
        self.assertEqual(database.read_bytes(), b"retained profiling database")
        self.assertEqual(foreign.read_text(encoding="utf-8"), "unrelated")

    # Corrupt bytes must be rejected before any executable or config is installed.
    def test_corrupt_archive_is_rejected_before_installation(self):
        archive = self.copy_archive()
        with archive.open("ab") as handle:
            handle.write(b"corruption")
        result = self.install(archive=archive)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA256", result.stdout + result.stderr)
        self.assertFalse((self.install_dir / "codex-profiler.exe").exists())
        self.assertFalse(self.codex_home.exists())

    # A checksum-valid archive still cannot place files outside the approved package layout.
    def test_traversal_entry_is_rejected(self):
        archive = self.copy_archive({"../escaped.txt": b"must not extract"})
        result = self.install(archive=archive)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("archive entry", result.stdout + result.stderr)
        self.assertFalse((self.base / "escaped.txt").exists())
        self.assertFalse((self.install_dir / "codex-profiler.exe").exists())

    # A nonempty folder with no ownership manifest must remain untouched.
    def test_unmanaged_directory_is_rejected(self):
        self.install_dir.mkdir()
        existing = self.install_dir / "README.md"
        existing.write_text("source checkout", encoding="utf-8")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(existing.read_text(encoding="utf-8"), "source checkout")
        self.assertEqual(list(self.install_dir.iterdir()), [existing])

    # A failed hook import after publication restores the previous binary and install state.
    def test_failed_upgrade_rolls_back_owned_files(self):
        self.assert_success(self.install())
        executable = self.install_dir / "codex-profiler.exe"
        original_binary = executable.read_bytes()
        original_state = (self.install_dir / "install-state.json").read_bytes()
        (self.codex_home / "fail-import.txt").touch()
        result = self.install("0.3.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture import rejected", result.stdout + result.stderr)
        self.assertEqual(executable.read_bytes(), original_binary)
        self.assertEqual(
            (self.install_dir / "install-state.json").read_bytes(), original_state
        )
        marker = self.codex_home / "imported-executable.txt"
        self.assertEqual(Path(marker.read_text(encoding="utf-8")), executable)

    # Rejected initial import must not leave an executable or an ownership claim behind.
    def test_failed_initial_import_removes_owned_files(self):
        self.codex_home.mkdir()
        (self.codex_home / "fail-import.txt").touch()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.install_dir / "codex-profiler.exe").exists())
        self.assertFalse((self.install_dir / "install-state.json").exists())
        self.assertTrue((self.codex_home / "fail-import.txt").exists())


if __name__ == "__main__":
    unittest.main()
