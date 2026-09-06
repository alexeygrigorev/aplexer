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
# Run both suites through the executed-count guard. `cargo test` exits 0 for a
# run that executed nothing -- see scripts/check-test-execution.sh -- so the
# exit status alone cannot tell "everything passed" from "nothing ran". The
# floors are collapse detectors, not ratchets: they sit well under the real
# counts (about 290 and 15 at the time of writing) so adding or removing a test
# never trips them, while a suite that silently stops running does.
scripts/check-test-execution.sh --self-test
scripts/check-test-execution.sh --min 250 -- cargo test --all-targets
scripts/check-test-execution.sh --min 12 -- cargo test --features startup-test-hooks \
  --test startup_rollback --test worker_startup_transaction --test lifecycle_failure

if [ -d python ]; then
  printf '==> Python syntax and tests\n'
  python3 -m compileall -q python
  if python3 -c 'import pytest' >/dev/null 2>&1; then
    python3 -m pytest -q
  else
    echo 'pytest is not installed; Python unit tests were not run' >&2
  fi
fi
