#!/usr/bin/env -S uv run python
"""Build platform-tagged wheels for the ``aplexer`` PyPI package.

Usage:
    uv run python scripts/build-wheels.py --binaries-dir ./artifacts --output-dir ./dist

The binaries-dir is expected to contain one subdirectory per platform,
named "aplexer-bins-<platform>" (matching the artifact names uploaded by
the build matrix in .github/workflows/release.yml), each holding the two
release binaries for that platform:

    aplexer-bins-linux-amd64/a
    aplexer-bins-linux-amd64/aplexer
    aplexer-bins-windows-amd64/a.exe
    aplexer-bins-windows-amd64/aplexer.exe
    ...

Each platform's pair of binaries is packaged into a single platform-tagged
wheel (PyPI project "aplexer", import package "aplexer_cli"), exposing both
as console scripts ("a" and "aplexer").
"""

import argparse
import base64
import csv
import hashlib
import io
import os
import re
import stat
import sys
import zipfile


PROJECT_NAME = "aplexer"
IMPORT_PACKAGE = "aplexer_cli"

# (platform key, wheel platform tag, binary suffix)
TARGETS = [
    ("linux-amd64", "manylinux_2_17_x86_64.manylinux2014_x86_64", ""),
    ("linux-arm64", "manylinux_2_17_aarch64.manylinux2014_aarch64", ""),
    ("darwin-amd64", "macosx_10_12_x86_64", ""),
    ("darwin-arm64", "macosx_11_0_arm64", ""),
    ("windows-amd64", "win_amd64", ".exe"),
    ("windows-arm64", "win_arm64", ".exe"),
]

BINARY_NAMES = ["a", "aplexer"]


def read_version():
    """Read version from python-cli/pyproject.toml."""
    pyproject_path = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "python-cli",
        "pyproject.toml",
    )
    with open(pyproject_path, "r") as f:
        for line in f:
            m = re.match(r'^version\s*=\s*"([^"]+)"', line)
            if m:
                return m.group(1)
    raise RuntimeError("Could not find version in python-cli/pyproject.toml")


def sha256_digest(data):
    """Return the SHA-256 digest of bytes data in urlsafe-base64 (no padding)."""
    h = hashlib.sha256(data)
    return base64.urlsafe_b64encode(h.digest()).rstrip(b"=").decode("ascii")


def build_wheel(binary_paths, platform_tag, suffix, version, output_dir):
    """Build a single platform-tagged wheel bundling both binaries.

    binary_paths: dict mapping binary name ("a", "aplexer") to its file path.
    """
    wheel_name = "{project}-{version}-py3-none-{platform}.whl".format(
        project=PROJECT_NAME, version=version, platform=platform_tag
    )
    wheel_path = os.path.join(output_dir, wheel_name)

    dist_info = "{project}-{version}.dist-info".format(project=PROJECT_NAME, version=version)

    records = []

    with zipfile.ZipFile(wheel_path, "w", zipfile.ZIP_DEFLATED) as whl:
        # Write both binaries into <package>/bin/
        for name in BINARY_NAMES:
            binary_path = binary_paths[name]
            with open(binary_path, "rb") as f:
                binary_data = f.read()

            bin_path_in_wheel = "{pkg}/bin/{name}{suffix}".format(
                pkg=IMPORT_PACKAGE, name=name, suffix=suffix
            )
            info = zipfile.ZipInfo(bin_path_in_wheel)
            if not suffix:
                # Set Unix executable permissions: rwxr-xr-x, and mark this as
                # a regular file (S_IFREG). pip's wheel installer only chmods
                # +x on extraction when `stat.S_ISREG(mode)` is true, which
                # reads the file-type bits, not just the permission bits.
                # Without S_IFREG here, the binary loses +x under plain `pip
                # install` (uv's installer applies the permission bits as-is
                # and is unaffected either way).
                info.external_attr = (
                    stat.S_IFREG
                    | stat.S_IRWXU
                    | stat.S_IRGRP
                    | stat.S_IXGRP
                    | stat.S_IROTH
                    | stat.S_IXOTH
                ) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            whl.writestr(info, binary_data)
            records.append(
                (bin_path_in_wheel, "sha256=" + sha256_digest(binary_data), str(len(binary_data)))
            )

        # Write __init__.py
        init_content = (
            '"""aplexer - prebuilt CLI binaries for the aplexer PTY session runtime."""\n\n'
            '__version__ = "{version}"\n'
        ).format(version=version)
        init_data = init_content.encode("utf-8")
        whl.writestr("{pkg}/__init__.py".format(pkg=IMPORT_PACKAGE), init_data)
        records.append(
            (
                "{pkg}/__init__.py".format(pkg=IMPORT_PACKAGE),
                "sha256=" + sha256_digest(init_data),
                str(len(init_data)),
            )
        )

        # Write __main__.py
        main_content = (
            '"""Allow running the bundled worker as `python -m aplexer_cli`."""\n\n'
            "from aplexer_cli._main import main_aplexer\n\n"
            'if __name__ == "__main__":\n'
            "    main_aplexer()\n"
        )
        main_data = main_content.encode("utf-8")
        whl.writestr("{pkg}/__main__.py".format(pkg=IMPORT_PACKAGE), main_data)
        records.append(
            (
                "{pkg}/__main__.py".format(pkg=IMPORT_PACKAGE),
                "sha256=" + sha256_digest(main_data),
                str(len(main_data)),
            )
        )

        # Write _main.py (copied verbatim from the source tree)
        main_py_path = os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "python-cli",
            "aplexer_cli",
            "_main.py",
        )
        with open(main_py_path, "rb") as f:
            main_py_data = f.read()
        whl.writestr("{pkg}/_main.py".format(pkg=IMPORT_PACKAGE), main_py_data)
        records.append(
            (
                "{pkg}/_main.py".format(pkg=IMPORT_PACKAGE),
                "sha256=" + sha256_digest(main_py_data),
                str(len(main_py_data)),
            )
        )

        # Write METADATA
        metadata = """\
Metadata-Version: 2.1
Name: {project}
Version: {version}
Summary: Prebuilt CLI binaries for aplexer, the daemonless PTY session runtime
License: Apache-2.0
Requires-Python: >=3.8
""".format(
            project=PROJECT_NAME, version=version
        )
        metadata_data = metadata.encode("utf-8")
        metadata_path = "{dist_info}/METADATA".format(dist_info=dist_info)
        whl.writestr(metadata_path, metadata_data)
        records.append(
            (metadata_path, "sha256=" + sha256_digest(metadata_data), str(len(metadata_data)))
        )

        # Write WHEEL
        wheel_meta = """\
Wheel-Version: 1.0
Generator: aplexer-build-wheels
Root-Is-Purelib: false
Tag: py3-none-{platform}
""".format(
            platform=platform_tag
        )
        wheel_meta_data = wheel_meta.encode("utf-8")
        wheel_meta_path = "{dist_info}/WHEEL".format(dist_info=dist_info)
        whl.writestr(wheel_meta_path, wheel_meta_data)
        records.append(
            (
                wheel_meta_path,
                "sha256=" + sha256_digest(wheel_meta_data),
                str(len(wheel_meta_data)),
            )
        )

        # Write entry_points.txt (console_scripts for both binaries)
        entry_points = (
            "[console_scripts]\n"
            "aplexer = aplexer_cli._main:main_aplexer\n"
            "a = aplexer_cli._main:main_a\n"
        )
        entry_points_data = entry_points.encode("utf-8")
        ep_path = "{dist_info}/entry_points.txt".format(dist_info=dist_info)
        whl.writestr(ep_path, entry_points_data)
        records.append(
            (ep_path, "sha256=" + sha256_digest(entry_points_data), str(len(entry_points_data)))
        )

        # Write top_level.txt
        top_level = "{pkg}\n".format(pkg=IMPORT_PACKAGE)
        top_level_data = top_level.encode("utf-8")
        tl_path = "{dist_info}/top_level.txt".format(dist_info=dist_info)
        whl.writestr(tl_path, top_level_data)
        records.append(
            (tl_path, "sha256=" + sha256_digest(top_level_data), str(len(top_level_data)))
        )

        # Write RECORD (must be last, and its own entry has no hash)
        record_path = "{dist_info}/RECORD".format(dist_info=dist_info)
        records.append((record_path, "", ""))

        record_buf = io.StringIO()
        writer = csv.writer(record_buf, lineterminator="\n")
        for row in records:
            writer.writerow(row)
        record_data = record_buf.getvalue().encode("utf-8")
        whl.writestr(record_path, record_data)

    return wheel_path


def main():
    parser = argparse.ArgumentParser(description="Build platform-tagged wheels for aplexer")
    parser.add_argument(
        "--binaries-dir",
        required=True,
        help="Directory containing per-platform aplexer-bins-<platform> subdirectories",
    )
    parser.add_argument(
        "--output-dir",
        default="dist",
        help="Directory to write wheels to (default: dist)",
    )
    parser.add_argument(
        "--version",
        default=None,
        help="Version override (default: read from python-cli/pyproject.toml)",
    )
    args = parser.parse_args()

    version = args.version or read_version()
    os.makedirs(args.output_dir, exist_ok=True)

    built = []
    skipped = []

    for platform_key, platform_tag, suffix in TARGETS:
        artifact_name = "aplexer-bins-{platform}".format(platform=platform_key)
        binary_paths = {}
        missing = []
        for name in BINARY_NAMES:
            filename = name + suffix
            # download-artifact creates a subdirectory per artifact name
            candidate = os.path.join(args.binaries_dir, artifact_name, filename)
            if not os.path.isfile(candidate):
                # also accept a flat layout for local/manual testing
                candidate = os.path.join(args.binaries_dir, filename)
            if not os.path.isfile(candidate):
                missing.append(filename)
                continue
            binary_paths[name] = candidate

        if missing:
            print(
                "WARNING: missing {missing} for {platform}, skipping".format(
                    missing=missing, platform=platform_key
                )
            )
            skipped.append(platform_key)
            continue

        wheel_path = build_wheel(binary_paths, platform_tag, suffix, version, args.output_dir)
        built.append(wheel_path)
        print("Built: {path}".format(path=wheel_path))

    print(
        "\nSummary: {built} wheels built, {skipped} skipped".format(
            built=len(built), skipped=len(skipped)
        )
    )

    if not built:
        print("ERROR: No wheels were built!", file=sys.stderr)
        sys.exit(1)

    return 0


if __name__ == "__main__":
    sys.exit(main() or 0)
