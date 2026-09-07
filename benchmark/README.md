# Benchmarks: aplexer vs tmux vs tmuxctl

`bench_mux.py` measures session-operation latency for three systems on the
same host: raw `tmux` (one shared server), `tmuxctl` (one tmux server per
session, Python CLI), and aplexer (`a`, one worker per session, Rust CLI).

## Quick start

```bash
python3 benchmark/bench_mux.py --quick                    # smoke (~1 min)
python3 benchmark/bench_mux.py --json-out results.json    # full (~4 min)
python3 benchmark/bench_mux.py --only heavy --heavy-mb 8  # just the scrollback ops
python3 benchmark/bench_mux.py --only create,list --iters 10 --scale 10
```

Stdlib only. Needs `tmux`, `tmuxctl`, and `./target/release/a`
(`cargo build --release --bins` first). Exits non-zero on any op failure,
so it doubles as a cross-system smoke test.

## Isolation

Everything runs under a fresh temp dir; live sessions are never touched:

- raw tmux: explicit private socket (`-S $BENCH/raw/mux.sock`).
- tmuxctl: private `TMUX_TMPDIR` (sees only bench sockets) and private
  `HOME` (bench-only sqlite DB — no rows in your real tmuxctl DB).
- aplexer: private `APLEXER_RUNTIME_DIR` / `APLEXER_STATE_DIR`.
  Your `APLEXER_CONFIG` (engines/profiles) is intentionally kept.

## Op catalog

Wall time around the CLI (`perf_counter`), min/p50/p90/max/mean over
N iterations with warmup. `create`/`kill` use a fresh session per
iteration; `list` runs with `--scale` sessions present.

| group | tmux | tmuxctl | aplexer | notes |
|---|---|---|---|---|
| create | `new-session -d` | `create-detached` | `a start` (+`kill` outside timing) | detached session creation |
| list | `list-sessions` | `list` | `a list`, `a --json list` | with `--scale` sessions present |
| status | `display-message -p` | `describe` | `a status` (+`--json`) | single session |
| send | `send-keys … Enter` | `send --enter-delay-ms 0` | `a send --enter` | one line, no attach |
| capture | `capture-pane -p` | — (no capture cmd; it's a plain tmux server) | `a capture`, `a capture --screen --plain` | read back idle output |
| roundtrip | send + poll `capture-pane` | — | send + poll `a capture` | unique marker → visible; includes shell scheduling |
| kill | `kill-session` | `kill --yes` | `a kill` (default grace **and** `--grace-ms 0`) | session pre-created; default `a kill` waits `--grace-ms 2000` by design |
| attach | `attach` under a pty, `C-b d` chord, wait exit | — (execs tmux attach; raw numbers apply) | `a attach` under a pty, `C-b d`, wait exit | full cycle; banner/client-render asserted, rc checked |
| heavy | same attach/capture ops | same via dedicated socket | `a attach` (screen + `--history-bytes 32768`), `a capture` (full + screen) | sessions flooded with `--heavy-mb` (default 4) MiB of ANSI-heavy output first; scrollback parity enforced (see quirks) |

`attach` breaks the wait as soon as the client is demonstrably live
(tmux: ≥500 bytes rendered + `list-clients` readiness gate; `a`: the
`attached to` banner), so both sides measure spawn→live→chord→exit.

## Methodology quirks (earned the hard way)

- **tmux grants `history-limit` only to windows created after the raise.**
  Raising it on an existing window is silently capped at the old 2000
  lines. The harness raises the server-global *before* creating heavy
  sessions (throwaway session first).
- **tmuxctl servers boot under `systemd-run`**, which drops our env, so no
  config-file trick can raise their scrollback. The harness raises the
  dedicated server's global limit, opens a fresh window (born big), and
  drops window 0.
- **`TERM` is forced to `xterm-256color`**: tmux clients refuse to attach
  on `TERM=dumb`.
- **`a kill` default includes ~2s of intentional waiting** (TERM, wait
  `--grace-ms`, escalate). The `grace-0` row is the mechanism cost
  comparable to `kill-session`.
- **tmuxctl `send` defaults to a 200ms enter delay**; the harness passes
  `--enter-delay-ms 0`.

## Results

Dated runs live next to this file: `results-YYYY-MM-DD.md` (analysis) +
`results-YYYY-MM-DD.json` (raw numbers, including `payload_bytes_mean`
for attach/capture ops and `heavy/retained-bytes`). Compare runs, don't
over-read one: ±2x run-to-run noise on this box is normal under load;
p50 is the headline, p90/max show the tail.
