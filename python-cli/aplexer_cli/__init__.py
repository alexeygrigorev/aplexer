"""aplexer - prebuilt CLI binaries for the aplexer PTY session runtime.

This distribution ships the compiled ``a`` and ``aplexer`` Rust binaries
for the current platform under ``aplexer_cli/bin/`` and exposes them as
console scripts, so ``uvx aplexer`` (or ``pip install aplexer``) works
without a Rust toolchain.

This is a different PyPI project from ``aplexer-client`` (import name
``aplexer``), which is a pure-Python socket client library that talks to
an already-running aplexer worker. The two are meant to be installed
together when a Python program embeds aplexer end to end.
"""

__version__ = "0.1.1"
