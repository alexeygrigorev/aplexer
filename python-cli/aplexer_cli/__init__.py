"""aplexer - prebuilt CLI binaries for the aplexer PTY session runtime.

This distribution ships the compiled ``a`` and ``aplexer`` Rust binaries
for the current platform under ``aplexer_cli/bin/`` and exposes them as
console scripts, so ``uvx aplexer`` (or ``pip install aplexer``) works
without a Rust toolchain.

This is a different PyPI project from ``aplexer-client`` (import name
``aplexer``), which provides an in-process Python API backed by the compiled
Rust library through PyO3. The two distributions can be installed together:
``aplexer`` supplies the CLI executables, while ``aplexer-client`` supplies
the embeddable bindings.
"""

__version__ = "0.1.1"
