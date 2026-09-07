# Improvement plan (from 2026-09-07 baseline)

Source: `results-2026-09-07.md` / `.json` in this folder.
Goal: keep the hot-path wins, close the lifecycle gaps, no regressions
on the ops where aplexer already leads. Re-run the harness after each
item and compare p50/p90, not just p50.

Status 2026-09-07 (same day): implemented; follow-up run
`results-2026-09-07-after-plan.md` / `.json`. Per-item outcomes below.

## P0 — kill path

- **P0.1 TERM escalation eats the full grace for `bash -l`.** — DONE.
  Root cause was legitimate ignoring, not wrong-target delivery: an
  interactive `bash -l` on a PTY ignores SIGTERM (also INT/QUIT) sent
  straight to its pid, and dies promptly on SIGHUP (verified empirically).
  Default `a kill --signal` changed TERM → HUP (with help text explaining
  why; `--signal TERM` still available). Follow-up: default-grace kill
  2096.9 → 55.0 ms.
- **P0.2 grace-0 mechanism cost is 5x `kill-session` (82 vs 16 ms).** —
  IMPROVED, target not fully met (follow-up: 54.3 ms, -34% absolute,
  ratio ~5.4x on a quieter box; ~2x target needs pidfd waiting or
  remove-before-respond). Wins taken: killed sessions skip
  fsync-and-delete finalization (history flush + terminal record +
  screen.txt removed moments later anyway), kill wait / record-removal
  watch / connection drain poll at 5 ms not 25 ms. Durability kept: the
  skip applies only when kill was accepted, nothing failed, and the domain
  is proven empty; every failure keeps the evidence via the old path.
- Acceptance: grace-0 p50 within ~2x of `kill-session`; default-grace
  kill of a TERM-responsive workload returns well under the grace
  (no full-wait when the process is already gone).
  Outcome: default-grace (now HUP-first) 55.0 ms — met. Grace-0 ratio
  ~5.4x — improved, not met; see above for what remains.

## P0 — create path

- **P0.3 Break down the ~150 ms `a start`.** — DONE (152.1 → 93.9 ms,
  -38%; ratio 5.8x → 4.7x). Wins: readiness poll 25 → 5 ms,
  launch-environment handoff without fsync (one-shot runtime file; the
  session record keeps full durability), skip of the redundant worker
  record write when no cgroup is requested. No durability loss:
  `worker_startup_transaction` (kill -9 mid-start coherence) passes,
  including under `startup-test-hooks` fault injection.
- Acceptance: p50 clearly down vs the 152 ms baseline with no
  durability loss (kill -9 the worker mid-start must still leave a
  coherent record or none — see `worker_startup_transaction` tests).
  Outcome: met.

## P1 — list and attach tail

- **P1.1 `a list` table costs ~7 ms over `--json` (20.3 vs 13.4),
  both over `list-sessions` (6.2).** — DONE. The scan cost was triple
  `worker_alive()` probing per record in the redirected rendering plus an
  uncached boot-id read per probe. Now: exactly 1 probe per record (shared
  liveness map) and a process-wide cached boot id. Follow-up: table
  4.8 ms (0.92x tmux, i.e. faster), json 6.8 ms (1.30x) — gap closed.
- **P1.2 attach p90 tail (36.6 ms vs 7.4 ms p50, n=5).** — CHARACTERIZED,
  tail gone at baseline methodology. Follow-up full run (n=5): p50 3.7 /
  p90 4.6 (1.24x). A separate `--attach-iters 15` run still shows a tail
  (p50 20.0 / p90 35.8) — but raw tmux shows the identical tail there
  (28.6/36.7), so it is PTY/scheduler noise, not status-bar or snapshot
  rendering. Separately fixed a real attach-path wart found while looking:
  every attach fsync'd the record for `last_accessed_ms`; now throttled to
  once per minute per session.
- Acceptance: table/json gap closed; attach p90 within ~2x of p50.
  Outcome: met at n=5 (table faster than json; attach p90 1.24x p50).

## P2 — scale and coverage

- **P2.1 Scale test.** — HARNESS DONE, one manual run banked.
  `bench_mux.py` records `scale/a-worker-rss` on every run (worker PIDs
  read from the state dir, VmRSS summed from `/proc`: no per-session RPC
  to perturb timings). Follow-up run at `--scale 25`: 25 workers, ~96 MiB
  total, ~3.9 MiB/worker, 50 runtime files. `--scale 100` runs for
  `list`/`status`/`switch` are supported and remain a manual step.
- **P2.2 Switch benchmark.** — DONE. New `switch` op: alternating-A/B
  resolve + status handshake (the resolve + handshake half of an
  in-process Ctrl-b n/p switch; full attach remains the upper bound).
  Follow-up: 2.9 ms vs the 20 ms budget (tmux equivalent 4.6 ms).
  Over-budget switch/attach p50s print a WARNING, never a failure.
- **P2.3 CI shape.** — DECIDED and documented in `README.md`: `--quick`
  is the correctness smoke (exits non-zero on any op failure; suitable
  for CI), full runs stay manual with dated results checked in, no
  latency gates in CI (±2x noise day to day).

## Non-goals

- Chasing tmux on `create` millisecond-for-millisecond at the cost
  of record durability or per-session isolation.
- Optimizing tmuxctl's numbers (different project; it is the
  baseline, not the target).
- Over-reading a single run: ±2x noise day to day is normal;
  compare ratios across runs.
