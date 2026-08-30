"""Tests for the wheel-building script (scripts/build-wheels.py)."""

import importlib.util
import os
import sys
import tempfile
import unittest
import zipfile
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO

sys.path.insert(
    0, os.path.join(os.path.dirname(__file__), "..", "..", "scripts")
)

spec = importlib.util.spec_from_file_location(
    "build_wheels",
    os.path.join(os.path.dirname(__file__), "..", "..", "scripts", "build-wheels.py"),
)
build_wheels = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build_wheels)


class TestReadVersion(unittest.TestCase):
    def test_reads_version(self):
        version = build_wheels.read_version()
        self.assertIsInstance(version, str)
        self.assertRegex(version, r"^\d+\.\d+\.\d+")


class TestBuildWheel(unittest.TestCase):
    def test_build_linux_amd64_wheel(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            a_path = os.path.join(tmpdir, "a")
            aplexer_path = os.path.join(tmpdir, "aplexer")
            with open(a_path, "wb") as f:
                f.write(b"#!/bin/sh\necho a\n")
            with open(aplexer_path, "wb") as f:
                f.write(b"#!/bin/sh\necho aplexer\n")

            output_dir = os.path.join(tmpdir, "dist")
            os.makedirs(output_dir)

            wheel_path = build_wheels.build_wheel(
                binary_paths={"a": a_path, "aplexer": aplexer_path},
                platform_tag="linux_x86_64",
                suffix="",
                version="0.1.0",
                output_dir=output_dir,
            )

            self.assertTrue(os.path.isfile(wheel_path))
            self.assertIn("aplexer-0.1.0", os.path.basename(wheel_path))
            self.assertIn("linux_x86_64", os.path.basename(wheel_path))
            self.assertTrue(wheel_path.endswith(".whl"))

            with zipfile.ZipFile(wheel_path, "r") as whl:
                names = whl.namelist()

                self.assertIn("aplexer_cli/bin/a", names)
                self.assertIn("aplexer_cli/bin/aplexer", names)
                self.assertIn("aplexer_cli/__init__.py", names)
                self.assertIn("aplexer_cli/__main__.py", names)
                self.assertIn("aplexer_cli/_main.py", names)

                self.assertIn("aplexer-0.1.0.dist-info/METADATA", names)
                self.assertIn("aplexer-0.1.0.dist-info/WHEEL", names)
                self.assertIn("aplexer-0.1.0.dist-info/RECORD", names)
                self.assertIn("aplexer-0.1.0.dist-info/entry_points.txt", names)

                entry_points = whl.read(
                    "aplexer-0.1.0.dist-info/entry_points.txt"
                ).decode("utf-8")
                self.assertIn("aplexer = aplexer_cli._main:main_aplexer", entry_points)
                self.assertIn("a = aplexer_cli._main:main_a", entry_points)

                metadata = whl.read(
                    "aplexer-0.1.0.dist-info/METADATA"
                ).decode("utf-8")
                self.assertIn("Requires-Python: >=3.11\n", metadata)
                self.assertIn("Requires-Dist: aplexer-client==0.1.0\n", metadata)

    def test_build_windows_wheel_uses_exe_suffix(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            a_path = os.path.join(tmpdir, "a.exe")
            aplexer_path = os.path.join(tmpdir, "aplexer.exe")
            with open(a_path, "wb") as f:
                f.write(b"MZ-fake-exe")
            with open(aplexer_path, "wb") as f:
                f.write(b"MZ-fake-exe")

            output_dir = os.path.join(tmpdir, "dist")
            os.makedirs(output_dir)

            wheel_path = build_wheels.build_wheel(
                binary_paths={"a": a_path, "aplexer": aplexer_path},
                platform_tag="win_amd64",
                suffix=".exe",
                version="0.1.0",
                output_dir=output_dir,
            )

            with zipfile.ZipFile(wheel_path, "r") as whl:
                names = whl.namelist()
                self.assertIn("aplexer_cli/bin/a.exe", names)
                self.assertIn("aplexer_cli/bin/aplexer.exe", names)


class TestMainMatrixEnforcement(unittest.TestCase):
    @staticmethod
    def write_binaries(root, platform):
        artifact_dir = os.path.join(root, "aplexer-bins-" + platform)
        os.makedirs(artifact_dir)
        for name in build_wheels.BINARY_NAMES:
            with open(os.path.join(artifact_dir, name), "wb") as f:
                f.write(("#!/bin/sh\necho " + name + "\n").encode("ascii"))

    @staticmethod
    def run_main(binaries_dir, output_dir, *extra_args):
        argv = [
            "--binaries-dir",
            binaries_dir,
            "--output-dir",
            output_dir,
            "--version",
            "0.1.0",
            *extra_args,
        ]
        stdout = StringIO()
        stderr = StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            result = build_wheels.main(argv)
        return result, stdout.getvalue(), stderr.getvalue()

    def test_missing_default_matrix_fails_before_writing_any_wheel(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            binaries_dir = os.path.join(tmpdir, "artifacts")
            os.makedirs(binaries_dir)
            self.write_binaries(binaries_dir, "linux-amd64")
            output_dir = os.path.join(tmpdir, "dist")

            result, stdout, stderr = self.run_main(binaries_dir, output_dir)

            self.assertEqual(result, 1)
            self.assertEqual(stdout, "")
            self.assertIn("required platform linux-arm64", stderr)
            self.assertIn("refusing a partial wheel matrix", stderr)
            self.assertFalse(os.path.exists(output_dir))

    def test_allow_partial_is_an_explicit_local_escape_hatch(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            binaries_dir = os.path.join(tmpdir, "artifacts")
            os.makedirs(binaries_dir)
            self.write_binaries(binaries_dir, "linux-amd64")
            output_dir = os.path.join(tmpdir, "dist")

            result, stdout, stderr = self.run_main(
                binaries_dir, output_dir, "--allow-partial"
            )

            self.assertEqual(result, 0)
            self.assertEqual(stderr, "")
            self.assertIn("skipping because --allow-partial was set", stdout)
            self.assertEqual(len(os.listdir(output_dir)), 1)
            self.assertIn("linux_x86_64", os.listdir(output_dir)[0])

    def test_complete_default_matrix_builds_every_release_wheel(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            binaries_dir = os.path.join(tmpdir, "artifacts")
            os.makedirs(binaries_dir)
            self.write_binaries(binaries_dir, "linux-amd64")
            self.write_binaries(binaries_dir, "linux-arm64")
            output_dir = os.path.join(tmpdir, "dist")

            result, stdout, stderr = self.run_main(binaries_dir, output_dir)

            self.assertEqual(result, 0)
            self.assertEqual(stderr, "")
            self.assertIn("2 wheels built, 0 skipped", stdout)
            self.assertEqual(
                set(os.listdir(output_dir)),
                {
                    "aplexer-0.1.0-py3-none-linux_x86_64.whl",
                    "aplexer-0.1.0-py3-none-linux_aarch64.whl",
                },
            )

    def test_selected_platform_accepts_flat_staging_and_is_strict(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            binaries_dir = os.path.join(tmpdir, "staging")
            os.makedirs(binaries_dir)
            for name in build_wheels.BINARY_NAMES:
                with open(os.path.join(binaries_dir, name), "wb") as f:
                    f.write(("#!/bin/sh\necho " + name + "\n").encode("ascii"))
            output_dir = os.path.join(tmpdir, "dist")

            result, _stdout, stderr = self.run_main(
                binaries_dir, output_dir, "--platform", "linux-amd64"
            )

            self.assertEqual(result, 0)
            self.assertEqual(stderr, "")
            self.assertEqual(len(os.listdir(output_dir)), 1)
            self.assertIn("linux_x86_64", os.listdir(output_dir)[0])

    def test_flat_layout_is_not_reused_for_multiple_architectures(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            binaries_dir = os.path.join(tmpdir, "staging")
            os.makedirs(binaries_dir)
            for name in build_wheels.BINARY_NAMES:
                with open(os.path.join(binaries_dir, name), "wb") as f:
                    f.write(b"not-an-architecture-neutral-binary")
            output_dir = os.path.join(tmpdir, "dist")

            result, _stdout, stderr = self.run_main(binaries_dir, output_dir)

            self.assertEqual(result, 1)
            self.assertIn("required platform linux-amd64", stderr)
            self.assertIn("required platform linux-arm64", stderr)
            self.assertFalse(os.path.exists(output_dir))


if __name__ == "__main__":
    unittest.main()
