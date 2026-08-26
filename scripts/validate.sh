#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

printf '==> repository hygiene\n'
test -f Cargo.toml
test -f README.md
test -d src
! find . -path './target' -prune -o -type f \( -name '*.pyc' -o -name '.DS_Store' \) -print -quit | grep -q .

printf '==> Rust formatting, type checking, and tests\n'
command -v cargo >/dev/null 2>&1 || { echo 'cargo is required' >&2; exit 127; }
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets

if [ -d python ]; then
  printf '==> Python syntax and tests\n'
  python3 -m compileall -q python
  if python3 -c 'import pytest' >/dev/null 2>&1; then
    python3 -m pytest -q
  else
    echo 'pytest is not installed; Python unit tests were not run' >&2
  fi
fi
