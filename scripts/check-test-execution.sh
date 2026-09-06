#!/usr/bin/env bash
# Run a cargo test command and fail unless it actually executed tests.
#
# `cargo test` reports "test result: ok. 0 passed; 0 failed; ...; 12 filtered
# out" with exit status 0. That is a pass by every mechanical measure and proves
# nothing at all. It is easy to reach by accident: a featureless `cargo test`
# build leaves a second `<suite>-<hash>` binary next to the feature-gated one,
# and a glob that expands to both hands the first binary to the second as a
# test-NAME FILTER. Both then "pass", one of them vacuously:
#
#     $ target/release/deps/startup_rollback-*
#     running 0 tests
#     test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out
#
# So assert on the executed count instead of on the exit status:
#
#     scripts/check-test-execution.sh --min 12 -- cargo test --release ...
#
# --min is a floor, not an equality, so adding a test never breaks the gate
# while deleting a suite's worth of them does.
set -euo pipefail

usage() {
    echo "usage: $0 --min N -- <command...>" >&2
    echo "       $0 --self-test" >&2
    exit 2
}

# Sum of the executed tests reported by every `test result:` line on stdin.
# `filtered out` is deliberately NOT counted: filtered tests are exactly the
# ones that did not run.
count_executed() {
    awk '
        /^test result:/ {
            for (i = 1; i <= NF; i++) {
                if ($(i + 1) ~ /^(passed;?|failed;?|ignored;?)$/) {
                    total += $i
                }
            }
        }
        END { print total + 0 }
    '
}

self_test() {
    local failures=0
    check() {
        local label=$1 expected=$2 sample=$3 actual
        actual=$(printf '%s\n' "$sample" | count_executed)
        if [ "$actual" != "$expected" ]; then
            echo "self-test FAILED: $label: expected $expected, counted $actual" >&2
            failures=$((failures + 1))
        fi
    }

    # The exact vacuous line this guard exists to reject.
    check 'filtered-out run counts as zero' 0 \
        'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out; finished in 0.00s'
    check 'a real run counts its tests' 12 \
        'test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.26s'
    check 'failures and ignores count as executed' 3 \
        'test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 11 filtered out; finished in 5.06s'
    check 'multi-binary runs add up' 17 \
        'test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.26s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.10s'
    check 'no test result line at all counts as zero' 0 'error: could not compile'

    if [ "$failures" -ne 0 ]; then
        echo "check-test-execution.sh self-test: $failures failure(s)" >&2
        exit 1
    fi
    echo 'check-test-execution.sh self-test: ok'
}

if [ "${1:-}" = --self-test ]; then
    self_test
    exit 0
fi

[ "${1:-}" = --min ] || usage
minimum=${2:-}
[ -n "$minimum" ] || usage
shift 2
[ "${1:-}" = -- ] || usage
shift
[ "$#" -gt 0 ] || usage

log=$(mktemp)
trap 'rm -f "$log"' EXIT

status=0
"$@" 2>&1 | tee "$log" || status=${PIPESTATUS[0]}
if [ "$status" -ne 0 ]; then
    echo "check-test-execution.sh: command failed with status $status" >&2
    exit "$status"
fi

executed=$(count_executed <"$log")
if [ "$executed" -lt "$minimum" ]; then
    echo "check-test-execution.sh: only $executed test(s) executed, expected at least $minimum" >&2
    echo "check-test-execution.sh: a green run that executed nothing is not a pass" >&2
    exit 1
fi
echo "check-test-execution.sh: $executed test(s) executed (floor $minimum)"
