"""Tests for the aplexer Python wrapper entry points."""

import os
import subprocess
import sys
import tempfile
import unittest
from io import StringIO
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import aplexer_cli
from aplexer_cli import _main


class TestVersion(unittest.TestCase):
    def test_version_exists(self):
        self.assertTrue(hasattr(aplexer_cli, "__version__"))

    def test_version_matches_pyproject(self):
        pyproject_path = os.path.join(
            os.path.dirname(os.path.dirname(__file__)), "pyproject.toml"
        )
        version = None
        with open(pyproject_path, "r") as f:
            for line in f:
                if line.startswith("version"):
                    version = line.split('"')[1]
                    break
        self.assertIsNotNone(version, "Could not find version in pyproject.toml")
        self.assertEqual(aplexer_cli.__version__, version)


class TestEntryPointsExist(unittest.TestCase):
    def test_main_a_is_callable(self):
        self.assertTrue(callable(_main.main_a))

    def test_main_aplexer_is_callable(self):
        self.assertTrue(callable(_main.main_aplexer))


class TestPlatformDetection(unittest.TestCase):
    """Platform detection resolves only architectures in the Linux wheel matrix."""

    def _assert_resolves(self, plat, machine, suffix):
        with mock.patch("sys.platform", plat), mock.patch(
            "platform.machine", return_value=machine
        ):
            for name in ("a", "aplexer"):
                path = _main._get_binary_path(name)
                self.assertIsNotNone(path)
                self.assertTrue(path.endswith(os.path.join("bin", name + suffix)))

    def test_linux_x86_64(self):
        self._assert_resolves("linux", "x86_64", "")

    def test_linux_aarch64(self):
        self._assert_resolves("linux", "aarch64", "")

    def test_linux_arm64_alias(self):
        self._assert_resolves("linux", "arm64", "")

    def test_unreleased_platforms_are_unsupported(self):
        for plat, machine in (
            ("darwin", "x86_64"),
            ("darwin", "arm64"),
            ("win32", "AMD64"),
            ("win32", "ARM64"),
            ("freebsd", "armv7l"),
        ):
            with self.subTest(platform=plat, machine=machine), mock.patch(
                "sys.platform", plat
            ), mock.patch("platform.machine", return_value=machine):
                self.assertIsNone(_main._get_binary_path("a"))
                self.assertIsNone(_main._get_binary_path("aplexer"))


class TestMissingOrUnsupportedExitsWithError(unittest.TestCase):
    def test_unsupported_platform_exits_with_error(self):
        stderr = StringIO()
        with mock.patch("sys.platform", "freebsd"), mock.patch(
            "platform.machine", return_value="armv7l"
        ), mock.patch("sys.stderr", stderr), self.assertRaises(SystemExit) as cm:
            _main.main_a()
        self.assertEqual(cm.exception.code, 1)
        self.assertIn("Linux x86_64", stderr.getvalue())
        self.assertIn("Linux aarch64 (arm64)", stderr.getvalue())
        self.assertNotIn("macOS", stderr.getvalue())
        self.assertNotIn("Windows", stderr.getvalue())

    def test_missing_binary_exits_with_error(self):
        """Supported platform, but no binary bundled in the source tree."""
        with mock.patch("sys.platform", "linux"), mock.patch(
            "platform.machine", return_value="x86_64"
        ), mock.patch("sys.stderr"), self.assertRaises(SystemExit) as cm:
            _main.main_aplexer()
        self.assertEqual(cm.exception.code, 1)


class TestPythonModuleInvocation(unittest.TestCase):
    def test_python_m_aplexer_cli_invokes_entry_point(self):
        """``python -m aplexer_cli`` should invoke the aplexer entry point.

        No binary is bundled in the source tree, so it should exit 1 with
        an error message naming the missing binary.
        """
        result = subprocess.run(
            [sys.executable, "-m", "aplexer_cli"],
            capture_output=True,
            text=True,
            cwd=os.path.join(os.path.dirname(__file__), ".."),
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("aplexer", result.stderr.lower())


class TestArgumentForwarding(unittest.TestCase):
    @mock.patch("os.execvp")
    def test_unix_args_forwarded(self, mock_execvp):
        with tempfile.NamedTemporaryFile(suffix="aplexer", delete=False) as f:
            fake_binary = f.name
        try:
            with mock.patch.object(
                _main, "_get_binary_path", return_value=fake_binary
            ), mock.patch("sys.platform", "linux"), mock.patch(
                "sys.argv", ["aplexer", "run", "--"]
            ):
                _main.main_aplexer()
                mock_execvp.assert_called_once_with(
                    fake_binary,
                    [fake_binary, "run", "--"],
                )
        finally:
            os.unlink(fake_binary)

if __name__ == "__main__":
    unittest.main()
