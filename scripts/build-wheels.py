#!/usr/bin/env -S uv run python
"""Build native-platform wheels for the ``aplexer`` CLI package.

Usage:
    uv run python scripts/build-wheels.py --binaries-dir ./artifacts --output-dir ./dist
    uv run python scripts/build-wheels.py --platform linux-amd64 \
        --binaries-dir ./staging --output-dir ./dist

The binaries-dir is expected to contain one subdirectory per platform,
named "aplexer-bins-<platform>" (matching the artifact names uploaded by
the build matrix in .github/workflows/release.yml), each holding the two
release binaries for that platform:

    aplexer-bins-linux-amd64/a
    aplexer-bins-linux-amd64/aplexer
Each platform's pair of binaries is packaged into a single wheel (PyPI
project "aplexer", import package "aplexer_cli"), exposing both as console
scripts ("a" and "aplexer"). Linux wheels intentionally start with the
conservative ``linux_<arch>`` tag. Release CI builds the binaries in a pinned
PyPA manylinux image and uses auditwheel to validate/repair that wheel into
the advertised ``manylinux_2_28`` tag.

The default is strict: every target in ``TARGETS`` must be present before any
wheel is written. Local/manual builds can select one or more ``--platform``
values, or opt into skipping absent targets with ``--allow-partial``.
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

# This is the release matrix, not a wishlist. Adding a target here makes it a
# required input to a default build and therefore must be paired with a build
# job in .github/workflows/release.yml.
# (platform key, conservative pre-audit wheel platform tag, binary suffix)
TARGETS = [
    ("linux-amd64", "linux_x86_64", ""),
    ("linux-arm64", "linux_aarch64", ""),
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


def main(argv=None):
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
    parser.add_argument(
        "--platform",
        action="append",
        choices=[target[0] for target in TARGETS],
        help="Build only this release platform (repeatable; default: require the full matrix)",
    )
    parser.add_argument(
        "--allow-partial",
        action="store_true",
        help="Explicitly skip missing selected platforms (intended for local builds only)",
    )
    args = parser.parse_args(argv)

    version = args.version or read_version()
    requested = set(args.platform or [])
    selected_targets = [target for target in TARGETS if not requested or target[0] in requested]
    # A flat directory cannot identify an architecture. Accept it only when
    # the caller selected exactly one target; full-matrix builds must use the
    # artifact subdirectories and cannot accidentally relabel one binary pair.
    allow_flat_layout = len(selected_targets) == 1

    ready = []
    missing_targets = []
    for platform_key, platform_tag, suffix in selected_targets:
        artifact_name = "aplexer-bins-{platform}".format(platform=platform_key)
        binary_paths = {}
        missing = []
        for name in BINARY_NAMES:
            filename = name + suffix
            # download-artifact creates a subdirectory per artifact name
            candidate = os.path.join(args.binaries_dir, artifact_name, filename)
            if allow_flat_layout and not os.path.isfile(candidate):
                # also accept a flat layout for local/manual testing
                candidate = os.path.join(args.binaries_dir, filename)
            if not os.path.isfile(candidate):
                missing.append(filename)
                continue
            binary_paths[name] = candidate

        if missing:
            missing_targets.append((platform_key, missing))
            continue
        ready.append((platform_key, platform_tag, suffix, binary_paths))

    if missing_targets and not args.allow_partial:
        for platform_key, missing in missing_targets:
            print(
                "ERROR: missing {missing} for required platform {platform}".format(
                    missing=missing, platform=platform_key
                ),
                file=sys.stderr,
            )
        print(
            "ERROR: refusing a partial wheel matrix; pass --allow-partial only for a local build",
            file=sys.stderr,
        )
        return 1

    for platform_key, missing in missing_targets:
        print(
            "WARNING: missing {missing} for {platform}, skipping because --allow-partial was set".format(
                missing=missing, platform=platform_key
            )
        )

    if not ready:
        print("ERROR: No complete platform inputs were found", file=sys.stderr)
        return 1

    os.makedirs(args.output_dir, exist_ok=True)
    built = []
    for platform_key, platform_tag, suffix, binary_paths in ready:

        wheel_path = build_wheel(binary_paths, platform_tag, suffix, version, args.output_dir)
        built.append(wheel_path)
        print("Built: {path}".format(path=wheel_path))

    print(
        "\nSummary: {built} wheels built, {skipped} skipped".format(
            built=len(built), skipped=len(missing_targets)
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main() or 0)
