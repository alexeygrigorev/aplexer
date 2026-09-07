# Issue #3 — audit vs current mainline

Issue: https://github.com/alexeygrigorev/aplexer/issues/3  
Review comments claiming later fixes: https://github.com/alexeygrigorev/aplexer/issues/3#issuecomment-5555637826 and https://github.com/alexeygrigorev/aplexer/issues/3#issuecomment-5555746823

Original findings 1–5 were already on this worktree's mainline (`26c8e2a`). Remaining work was the review follow-ups that can still go green without running anything, plus treating zombies as exited in the fault-injection helpers (finding 4, incomplete under a nested aplexer subreaper).

## Findings

| Finding | Status | Evidence |
|---|---|---|
| 1. `externally_reaped_worker_...` vacuous `could not be confirmed` | already fixed | Unique path anchor `observed_external_worker_reap()` requires `worker exited during startup` **and** `signal: 9` (`tests/startup_rollback.rs:154-190`). Asserted before preserved-evidence checks (`:1356-1360`). Timeout is `LIVENESS_BACKSTOP`, not a 5s race (`:1292-1309`). Ungated inverted proof `the_external_reap_anchor_rejects_the_startup_timeout_path` (`:1592-1616`) pins that the old string matches both paths. |
| 2. `timeout_after_workload_spawn_...` inherits ambient rlimit | already fixed | `pin_open_files` pins an exact soft limit (`tests/startup_rollback.rs:234-270`). Budget is `inherited_descriptor_count() + LAUNCHER_FD_OVERHEAD + tree + slack` (`:1079-1085`). Hard-limit refusal names itself (`:259-262`). |
| 3. Wall-clock deadlines as correctness (`startup_rollback` 8s; `lifecycle_failure` 6s/10s) | already fixed | Rollback bound is `STARTUP_TIMEOUT_MS + STARTUP_TERM_GRACE + STARTUP_CONTAINMENT_TIMEOUT`, timed from the hang marker (`tests/startup_rollback.rs:1114-1122`). Lifecycle polls use `LIVENESS_BACKSTOP = 60s` and exit the instant the condition holds (`tests/lifecycle_failure.rs:68-79, 159-173`). |
| 4. `worker_startup_transaction.rs` unsync `process_alive` | fixed now | Polling was already on mainline. `kill(pid, 0)` still treats zombies as alive; this suite often runs under another aplexer worker (subreaper). Helpers now treat `/proc/<pid>` state `Z` as exited: `tests/worker_startup_transaction.rs:20-51`, `tests/lifecycle_failure.rs:98-128`, `tests/startup_rollback.rs:105-129`. A leaked live process stays non-zombie forever, so waiting cannot turn a leak into a pass. Pid-reuse blindness of `kill(pid, 0)` remains out of scope. |
| 5. Harness glob footgun (`0 passed; 12 filtered out` / exit 0) | already fixed | `scripts/check-test-execution.sh` parses `test result:` and requires `--min N`. Wired in `scripts/validate.sh:21-24` and `.github/workflows/release.yml:109-127`. Self-test covers the vacuous filtered-out line (`scripts/check-test-execution.sh:58-60`). |
| Follow-up: `ignored` counted as executed | fixed now | `count_executed` sums **passed + failed only** (`scripts/check-test-execution.sh:29-45`). Self-test: mixed run counts 2 not 3 (`:63-64`); all-ignored suite counts 0 (`:65-66`). |
| Follow-up: `validate.sh` silently skips Python when pytest is missing | fixed now | Missing pytest used to print a warning and continue. Now: system pytest, else `uv run --frozen --with pytest`, else exit 1. Both `python/` and `python-cli/` run (`scripts/validate.sh:26-51`). |
| Follow-up: `pin_open_files` raise direction never exercised on a 1024-fd runner | fixed now | Child first drops to `STARVED_OPEN_FILES = 8`, then pins the target (`tests/startup_rollback.rs:211-268`). A clamp-only `.min()` regression would leave the child at 8. Dedicated test `pin_open_files_raises_from_a_starved_soft_limit` (`:940-998`) asserts the child reports 256. |
| Follow-up: `state_report.rs` command budgets still wall-clock | fixed now | `Harness::start`, `state_report`, and the two CLI-error tests now use `LIVENESS_BACKSTOP` instead of 10s/5s (`tests/state_report.rs:34-46, 124, 138, 535, 553`). These are hang detectors, not “must finish by T” assertions; `READINESS_PROBE_WINDOW` is unchanged (handshake retry, not a pass-by-timeout). |
| Follow-up: orphaned session directory is invisible rather than fatal | leftover | Product change from the #2/#3 work: `list_records` skips ENOENT. `a doctor` / `a prune` still do not report or age out record-less session directories. Out of scope for test vacuity. |
| `process_alive` pid reuse | leftover | Recorded in the original review; needs `process_start_time_ticks` captured before death. No reproduction. |
| Nested-subreaper integration tests | leftover | Under an enclosing aplexer worker, `snapshot` still reports `worker_alive: true` for zombies (product `kill(pid, 0)`). `failed_replacement_*` / `reclaim_*` in `startup_rollback.rs` and some other integration tests (e.g. `containment_recovery`) can fail here; they pass when the suite is not nested. Not changed. |
| `python/` tests when actually run | leftover | Forcing the suite (uv) currently fails 3 `test_client.py` tests on this mainline (`Native.start` mock missing `fresh`; native kill/status). Pre-existing vs `--fresh` / kill behaviour, not this issue. `python-cli/` is 20 passed. |

## Validation

- `scripts/check-test-execution.sh --self-test` — ok (includes all-ignored → 0).
- Fault-injection lane (this nested-subreaper environment), skipping the four replacement/reclaim tests that wait on product `worker_alive: false`:

  ```
  scripts/check-test-execution.sh --min 12 -- cargo test --features startup-test-hooks \
    --test startup_rollback --test worker_startup_transaction --test lifecycle_failure \
    -- --skip failed_replacement --skip replacement_cleanup --skip failed_reclaim --skip reclaim_cleanup
  ```

  **14 executed (floor 12)**, including `pin_open_files_raises_from_a_starved_soft_limit`, `externally_reaped_worker_...`, `timeout_after_workload_spawn_...`, `pidfd_budget_failure_...`, `lifecycle_failure`, `worker_startup_transaction`.

- `scripts/validate.sh` — fmt, check, guard self-test, and lib/bin unit tests ran; `cargo test --all-targets` then failed in `containment_recovery` because this process is nested under aplexer worker `8dd57730-...` (subreaper). That failure is environmental, not introduced here.
- `python-cli/`: `uv run --frozen --with pytest python -m pytest -q` → 20 passed, 5 subtests.
