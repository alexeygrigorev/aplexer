#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

printf '==> repository hygiene\n'
test -f Cargo.toml
test -f README.md
test -d src
! find . -path './target' -prune -o -type f \( -name '*.pyc' -o -name '.DS_Store' \) -print -quit | grep -q .

printf '==> Rust formatting, lints, and tests\n'
command -v cargo >/dev/null 2>&1 || { echo 'cargo is required' >&2; exit 127; }
cargo fmt --all -- --check
# Clippy is the type-check gate: it is a superset of `cargo check`, and
# `-D warnings` makes a new lint a failed run rather than scrollback nobody
# reads. .github/workflows/ci.yml runs this same script, so the bar is
# identical on a laptop and on a pull request.
cargo clippy --all-targets -- -D warnings
# Run both suites through the executed-count guard. `cargo test` exits 0 for a
# run that executed nothing -- see scripts/check-test-execution.sh -- so the
# exit status alone cannot tell "everything passed" from "nothing ran". The
# floors are collapse detectors, not ratchets: they sit well under the real
# counts (a full run executes about 480 and 18 today) so adding or removing a test
# never trips them, while a suite that silently stops running does.
#
# The floors count only executed tests, never `ignored` ones (see the guard's
# own self-test), so the deliberately quarantined tests -- the cgroup-v2
# delegation cases, the live-agent transcript case and the manual attach
# latency measurement, all listed in README.md's Validation section -- cannot
# pad a run that executed nothing.
scripts/check-test-execution.sh --self-test
scripts/check-test-execution.sh --min 250 -- cargo test --all-targets
scripts/check-test-execution.sh --min 12 -- cargo test --features startup-test-hooks \
  --test startup_rollback --test worker_startup_transaction --test lifecycle_failure

# Python suites must actually run. Skipping when pytest is missing is the
# same "green that ran nothing" shape check-test-execution.sh exists to
# reject, in this same script. Prefer system pytest; fall back to uv (what
# CI uses) so a machine with uv but no system pytest still runs the suites
# instead of going green. Missing both is a hard failure.
run_python_suite() {
  local dir=$1
  printf '==> Python syntax and tests (%s)\n' "$dir"
  python3 -m compileall -q "$dir"
  if python3 -c 'import pytest' >/dev/null 2>&1; then
    (cd "$dir" && python3 -m pytest -q)
  elif command -v uv >/dev/null 2>&1 && [ -f "$dir/uv.lock" ]; then
    (cd "$dir" && uv run --frozen --with pytest python -m pytest -q)
  else
    echo "pytest is not installed; ${dir} tests were not run" >&2
    echo "install pytest or uv so a missing Python suite cannot pass silently" >&2
    exit 1
  fi
}

if [ -d python ]; then
  run_python_suite python
fi
if [ -d python-cli ]; then
  run_python_suite python-cli
fi
