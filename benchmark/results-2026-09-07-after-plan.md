# Follow-up results — 2026-09-07 (after PLAN implementation)

Harness: `bench_mux.py` (with PLAN P2.1/P2.2 additions: `switch` op,
`scale/a-worker-rss`, 20 ms budget warnings). Same methodology as the
baseline: 25 iterations, 25 sessions present for `list`, 6 roundtrips,
5 attach cycles, 4 MiB heavy flood. Raw data:
`results-2026-09-07-after-plan.json`.
Host: Linux 6.8.0-138-generic x86_64, tmux 3.4, aplexer 0.1.4,
Python 3.14.3. Quieter box than the baseline run (e.g. tmux
`new-session` 20.2 ms here vs 26.1 ms there), so **ratios within this run**
are the signal, not absolutes across runs.

All numbers below are **p50 wall ms** around the CLI.

## Standard ops (idle sessions)

| op | tmux | aplexer | a vs tmux | baseline a vs tmux |
|---|---|---|---|---|
| create (detached) | 20.2 | 93.9 | 4.7x slower | 5.8x slower (152.1 ms) |
| list (25 present) | 5.2 | 4.8 table / 6.8 json | 0.92x / 1.30x | 3.3x / 2.1x slower |
| status | 4.1 | 2.5 (3.4 json) | 0.63x (faster) | ~parity |
| switch proxy (new) | 4.6 | 2.9 | 0.64x, budget 20 ms met | — |
| send (one line) | 3.9 | 2.8 | 0.71x (faster) | 0.65x (faster) |
| capture (idle) | 3.6 | 2.5 (3.0 screen-plain) | 0.69x (faster) | 0.58x (faster) |
| roundtrip echo→visible | 9.8 | 9.6 | 0.98x (~parity) | 0.79x (faster) |
| kill | 10.1 | 54.3 grace-0 / 55.0 default-grace | 5.4x at grace-0 | 5.2x at grace-0 / 2096.9 default |
| attach+detach cycle | 11.8 (p90 16.0) | 3.7 (p90 4.6) | 0.31x (faster) | 0.52x (faster) |

## Heavy ops (~4 MiB scrollback retained)

| op | tmux | aplexer | a vs tmux |
|---|---|---|---|
| attach+detach | 12.4 | 3.6 screen / 4.0 32k-tail | 0.29x / 0.32x |
| full scrollback read | 127.7 (~4.5 MiB) | 14.5 (~4 MiB) | 0.11x |
| screen query | — | 4.4 | — |

## Scale (new, PLAN P2.1)

25 seeded sessions: 25 workers, RSS total ~96 MiB, mean/max ~3.9 MiB
per worker, 50 runtime files (socket + lock each). That is the quantified
daemonless cost at spec §30 scale. `--scale 100` runs are supported by the
harness (`--scale 100 --only list,status,switch`) but not run here.

## Per-item outcomes

1. **P0.1 TERM escalation — fixed.** Root cause confirmed empirically, not a
   routing bug: an interactive `bash -l` on a PTY ignores SIGTERM (also INT
   and QUIT) delivered straight to its pid, and dies promptly on SIGHUP.
   Default `a kill --signal` is now `HUP` (session hangup, matching what a
   closing terminal delivers; tmux kills panes the same way). Default-grace
   kill of the benchmark's `bash -l` went 2096.9 ms → 55.0 ms. Explicit
   `--signal TERM` keeps TERM-first semantics (still eats the grace for
   TERM-ignoring shells, as it should).
2. **P0.2 grace-0 mechanism — improved 82.3 → 54.3 ms (-34%), ratio still
   ~5.4x vs `kill-session` (target was ~2x).** Wins taken: killed sessions
   skip the fsync-and-delete finalization (history flush + terminal record
   + screen.txt were persisted then deleted moments later by the state-dir
   removal), kill wait and record-removal watch poll at 5 ms instead of
   25 ms. Remainder is structural (dual `/proc` descendant scans, process
   reap, RPC round trip) — further gains need pidfd-based waiting or a
   synchronous remove-before-respond redesign, deliberately not done here.
3. **P0.3 `a start` — 152.1 → 93.9 ms (-38%), ratio 5.8x → 4.7x.** Wins:
   readiness poll 25 → 5 ms, launch-environment handoff without fsync
   (one-shot runtime file, deleted after use; the session record keeps full
   durability), skip of the redundant worker record write when no cgroup is
   requested. `worker_startup_transaction` (kill -9 mid-start coherence)
   still passes, including under `startup-test-hooks` fault injection.
4. **P1.1 list — gap closed, now at/below tmux.** Table 20.3 → 4.8 ms
   (0.92x tmux), json 13.4 → 6.8 ms (1.30x). Fixes: `worker_alive()` probed
   3x per record in the redirected rendering (header + summary recount +
   row) → exactly 1x via a per-call liveness map; boot id cached
   process-wide instead of re-read from `/proc` per probe.
5. **P1.2 attach tail — gone at baseline methodology, characterized.**
   Full run (n=5): p50 3.7 / p90 4.6 (1.24x, was 7.4/36.6). A separate
   `--attach-iters 15` run still shows a tail (p50 20.0 / p90 35.8) — but
   raw tmux shows the identical tail there (28.6/36.7), so it is PTY /
   scheduler noise on this box, not status-bar or snapshot rendering.
   Separately, every attach used to fsync the record for `last_accessed_ms`;
   that write is now throttled to once per minute per session (repeat
   benchmark attaches skip it).
6. **P2 — harness + docs.** `switch` op (alternating A/B resolve +
   handshake) at 2.9 ms vs the 20 ms budget; `scale/a-worker-rss` in every
   JSON; over-budget switch/attach p50s print a WARNING, never a failure;
   `README.md` records the CI decision (`--quick` = correctness smoke, full
   runs manual with dated results, no latency gates in CI).

## Caveats

- Same as the baseline: one host, ratios are the stable signal. This box
  was quieter than for the baseline (tmux absolutes moved), which is why
  the table above carries both runs' ratios.
- The 8 failing integration suites (`containment_recovery`,
  `descendant_lifecycle`, `history_crash_recovery`, `history_limit`,
  `prune_dead_records`, `reclaim_zombie_tag`, `startup_rollback`,
  `status_json_state`) fail identically with and without these changes
  (verified via `git stash` on the same HEAD) — pre-existing breakage from
  the concurrently landed list-sort/last-access work, not from this plan.
- Rerun with `python3 benchmark/bench_mux.py --json-out results-<date>.json`.
