"""Tests for the wheel-building script (scripts/build-wheels.py)."""

import importlib.util
import os
import sys
import tempfile
import unittest
import zipfile

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
                platform_tag="manylinux_2_17_x86_64.manylinux2014_x86_64",
                suffix="",
                version="0.1.0",
                output_dir=output_dir,
            )

            self.assertTrue(os.path.isfile(wheel_path))
            self.assertIn("aplexer-0.1.0", os.path.basename(wheel_path))
            self.assertIn("manylinux_2_17_x86_64", os.path.basename(wheel_path))
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


class TestMainMissingBinariesSkips(unittest.TestCase):
    def test_missing_binaries_dir_reports_all_skipped(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            empty_binaries_dir = os.path.join(tmpdir, "artifacts")
            os.makedirs(empty_binaries_dir)
            output_dir = os.path.join(tmpdir, "dist")

            old_argv = sys.argv
            try:
                sys.argv = [
                    "build-wheels.py",
                    "--binaries-dir",
                    empty_binaries_dir,
                    "--output-dir",
                    output_dir,
                    "--version",
                    "0.1.0",
                ]
                with self.assertRaises(SystemExit) as cm:
                    build_wheels.main()
                self.assertEqual(cm.exception.code, 1)
            finally:
                sys.argv = old_argv


if __name__ == "__main__":
    unittest.main()
