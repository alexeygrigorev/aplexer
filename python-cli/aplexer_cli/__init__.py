"""aplexer - prebuilt CLI binaries for the aplexer PTY session runtime.

This distribution ships the compiled ``a`` and ``aplexer`` Rust binaries
for the current platform under ``aplexer_cli/bin/`` and exposes them as
console scripts, so ``uvx aplexer`` (or ``pip install aplexer``) works
without a Rust toolchain.

The separately built ``aplexer-client`` distribution provides the public
``aplexer`` import package and an in-process Python API backed by the compiled
Rust library through PyO3. This CLI distribution requires the exact same
``aplexer-client`` version, so a normal ``pip install aplexer`` installs both
the executables and the embeddable bindings.
"""

__version__ = "0.1.3"
