"""Entry points that locate and execute the bundled aplexer binaries.

Same pattern as the ``a``/``aplexer`` lookup in ``aplexer-client``'s
``resolve_cli()``: the binaries this wheel ships live next to this file,
under ``bin/``, not on ``$PATH``.
"""

import os
import platform
import subprocess
import sys


# Map (sys.platform, platform.machine()) to the binary suffix used inside
# the package. platform.machine() returns values like 'x86_64', 'AMD64',
# 'aarch64', 'arm64'.
_PLATFORM_MAP = {
    ("linux", "x86_64"): "",
    ("linux", "aarch64"): "",
    ("darwin", "x86_64"): "",
    ("darwin", "arm64"): "",
    ("win32", "AMD64"): ".exe",
    ("win32", "x86_64"): ".exe",
    ("win32", "ARM64"): ".exe",
    ("win32", "aarch64"): ".exe",
}

_SUPPORTED_PLATFORMS = [
    "linux x86_64 (amd64)",
    "linux aarch64 (arm64)",
    "macOS x86_64 (amd64)",
    "macOS arm64 (Apple Silicon)",
    "Windows AMD64",
    "Windows ARM64",
]


def _binary_suffix():
    """Return the bundled-binary filename suffix for this platform, or None."""
    return _PLATFORM_MAP.get((sys.platform, platform.machine()))


def _get_binary_path(name):
    """Return the path to the bundled ``name`` binary for the current platform."""
    suffix = _binary_suffix()
    if suffix is None:
        return None
    package_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.join(package_dir, "bin", name + suffix)


def _run(name):
    """Locate the bundled ``name`` binary and execute it, forwarding all arguments."""
    binary_path = _get_binary_path(name)

    if binary_path is None or not os.path.isfile(binary_path):
        plat = sys.platform
        machine = platform.machine()
        print(
            "Error: no bundled '{name}' binary for this platform "
            "({platform} {machine}).\n"
            "\n"
            "Supported platforms:\n"
            "{platforms}\n"
            "\n"
            "You can build from source with: cargo install --path . --bin {name}".format(
                name=name,
                platform=plat,
                machine=machine,
                platforms="\n".join("  - " + p for p in _SUPPORTED_PLATFORMS),
            ),
            file=sys.stderr,
        )
        sys.exit(1)

    args = [binary_path] + sys.argv[1:]

    if sys.platform == "win32":
        # On Windows, os.execvp is not reliable; use subprocess instead.
        try:
            result = subprocess.run(args)
            sys.exit(result.returncode)
        except KeyboardInterrupt:
            sys.exit(130)  # Standard exit code for Ctrl+C (128 + SIGINT)
    else:
        # On Unix, replace the current process with the binary.
        os.execvp(binary_path, args)


def main_a():
    """Console-script entry point for the short ``a`` CLI."""
    _run("a")


def main_aplexer():
    """Console-script entry point for the ``aplexer`` worker binary."""
    _run("aplexer")
