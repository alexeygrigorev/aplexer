# Improvement plan (from 2026-09-07 baseline)

Source: `results-2026-09-07.md` / `.json` in this folder.
Goal: keep the hot-path wins, close the lifecycle gaps, no regressions
on the ops where aplexer already leads. Re-run the harness after each
item and compare p50/p90, not just p50.

## P0 — kill path

- **P0.1 TERM escalation eats the full grace for `bash -l`.**
  Baseline: default `a kill` took 2097 ms — the workload survived TERM
  and the path escalated after the whole `--grace-ms 2000` wait.
  Question: is TERM going to the wrong target (child vs process group
  vs cgroup), or does `bash -l` legitimately ignore it? Instrument
  which signal actually reaps the workload; fix delivery before
  touching the wait.
- **P0.2 grace-0 mechanism cost is 5x `kill-session` (82 vs 16 ms).**
  Profile the breakdown: cgroup/containment teardown vs record
  finalize (fsync + rename) vs socket/lock removal. Take the cheap
  wins (ordering, redundant syncs); keep durability guarantees for
  the dead-session record path — `kill` deliberately leaves no record,
  so anything purely about post-mortem evidence is suspect here.
- Acceptance: grace-0 p50 within ~2x of `kill-session`; default-grace
  kill of a TERM-responsive workload returns well under the grace
  (no full-wait when the process is already gone).

## P0 — create path

- **P0.3 Break down the ~150 ms `a start`.**
  Suspects: worker spawn + PTY setup, durable record writes
  (fsync + rename), attach-handshake round trip, Python-free but
  still serial startup steps. Time each stage; parallelize or defer
  what isn't needed before returning the id.
- Acceptance: p50 clearly down vs the 152 ms baseline with no
  durability loss (kill -9 the worker mid-start must still leave a
  coherent record or none — see `worker_startup_transaction` tests).

## P1 — list and attach tail

- **P1.1 `a list` table costs ~7 ms over `--json` (20.3 vs 13.4),
  both over `list-sessions` (6.2).**
  Profile the scan (per-record stat/socket probe?) vs table
  formatting. The scan is the durable cost — formatting is pure win
  if separable.
- **P1.2 attach p90 tail (36.6 ms vs 7.4 ms p50, n=5).**
  Re-run with `--attach-iters 15` first; if the tail persists,
  check status-bar draw and screen-snapshot render on the slow
  samples.
- Acceptance: table/json gap closed; attach p90 within ~2x of p50.

## P2 — scale and coverage

- **P2.1 Scale test.** spec §30 targets "dozens of sessions";
  baseline ran 25. Add `--scale 100` runs for `list`/`status` and
  record per-worker RSS (N workers/sockets/locks is the known cost
  of daemonless — quantify it).
- **P2.2 Switch benchmark.** Once in-process switching lands, add a
  chord-to-first-byte op (resolve + handshake) with the 20 ms budget
  as the assertion.
- **P2.3 CI shape.** Decide what runs in CI: `--quick` as a smoke
  test (it already exits non-zero on any op failure) vs full runs
  kept as manual, results checked in dated like this one.

## Non-goals

- Chasing tmux on `create` millisecond-for-millisecond at the cost
  of record durability or per-session isolation.
- Optimizing tmuxctl's numbers (different project; it is the
  baseline, not the target).
- Over-reading a single run: ±2x noise day to day is normal;
  compare ratios across runs.
