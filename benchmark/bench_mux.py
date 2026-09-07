#!/usr/bin/env python3
"""Benchmark tmux vs tmuxctl vs aplexer (a) on session operations.

Isolated by design: everything runs under a fresh temp dir, so your live
sessions are never touched.

  - raw tmux uses an explicit private socket (-S $BENCH/raw/mux.sock)
  - tmuxctl uses a private TMUX_TMPDIR ($BENCH/tc) -> per-session servers
  - aplexer uses private APLEXER_RUNTIME_DIR / APLEXER_STATE_DIR

Ops measured (wall time around the CLI, perf_counter):
  create      detached session creation (unique name/tag per iteration)
  list        listing with --scale sessions already present
  status      single-session status/describe
  switch      chord-to-first-byte proxy (PLAN P2.2): resolve the *other*
              seeded session + status handshake, alternating A/B so no
              lookup can hit a hot cache entry. Design budget is < 20 ms;
              the report flags an over-budget aplexer p50 as a WARNING
              (not a failure -- loaded-box noise is ±2x, see results doc).
              In-process Ctrl-b n/p itself is resolve + attach handshake,
              bounded above by the attach op.
  send        injecting one line without attaching
  capture     reading output back (raw; aplexer also --screen --plain)
  roundtrip   send a unique marker, poll capture until visible (user-visible
              input-to-output latency, includes shell scheduling)
  kill        destroying a session created just before timing starts
              (default `a kill` is HUP-first since interactive shells
              ignore TERM -- PLAN P0.1 -- plus a `--grace-ms 0` row for
              the mechanism cost comparable to kill-session)
  attach      full attach+detach cycle under a pty:
                tmux: attach, wait for client, detach-client, wait for exit
                a:    attach, wait for banner, send Ctrl-], wait for exit
              (tmuxctl attach is interactive-only; it execs tmux attach, so
              raw-tmux attach numbers apply to it.)
  scale       (implicit, PLAN P2.1) with --scale N>=25: per-worker RSS
              (N workers/sockets/locks is the known daemonless cost) is
              recorded as scale/worker-rss alongside the list/status rows.
              Run --scale 100 for the spec §30 "dozens of sessions" check.

Switching note: aplexer's in-process switch (docs/fast-session-switching-
design.md) is resolve (list scan) + attach handshake. The attach op above
is its upper bound; the design target is chord-to-first-byte < 20ms.

Usage:
  python3 benchmark/bench_mux.py [--quick] [--scale N] [--only create,list]
                                  [--json-out results.json] [--keep]

  --scale 100 runs list/status/switch against 100 sessions and records
  per-worker RSS (spec §30 scale check). --quick is the CI smoke shape:
  it exits non-zero on any op failure; full runs stay manual with dated
  results checked in (see README.md).

Stdlib only.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import pty
import select
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
A_BIN = os.path.join(REPO, "target", "release", "a")
TMUXCTL_BIN = shutil.which("tmuxctl") or os.path.expanduser("~/.local/bin/tmuxctl")
TMUX_BIN = shutil.which("tmux") or "tmux"
SHELL = "/bin/bash"


def run(argv, env, timeout=60):
    p = subprocess.run(argv, env=env, capture_output=True, timeout=timeout)
    return p


def timed(argv, env, timeout=60):
    t0 = time.perf_counter()
    p = run(argv, env, timeout=timeout)
    dt = (time.perf_counter() - t0) * 1000.0
    return dt, p


def stats(samples):
    s = sorted(samples)
    n = len(s)
    p50 = s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2
    p90 = s[min(n - 1, int(n * 0.9))]
    return {
        "n": n,
        "min": s[0],
        "p50": p50,
        "p90": p90,
        "max": s[-1],
        "mean": statistics.fmean(s),
        "stdev": statistics.pstdev(s) if n > 1 else 0.0,
    }


class Bench:
    def __init__(self, args):
        self.args = args
        self.root = tempfile.mkdtemp(prefix="muxbench-")
        self.ws = os.path.join(self.root, "ws")
        self.raw_dir = os.path.join(self.root, "raw")
        self.tc_dir = os.path.join(self.root, "tc")
        self.arun = os.path.join(self.root, "arun")
        self.astate = os.path.join(self.root, "astate")
        for d in (self.ws, self.raw_dir, self.tc_dir, self.arun, self.astate):
            os.makedirs(d, exist_ok=True)
        self.sock = os.path.join(self.raw_dir, "mux.sock")

        base = dict(os.environ)
        # A real TERM: tmux clients refuse to attach on TERM=dumb, and the
        # aplexer status bar assumes a capable terminal too.
        base["TERM"] = "xterm-256color"
        # tmuxctl isolation: a private HOME keeps bench sessions out of the
        # user's real ~/.config/tmuxctl/tmuxctl.db. (A shipped .tmux.conf
        # cannot raise scrollback for tmuxctl's servers -- they boot under
        # systemd-run without our env -- so seed_heavy raises the limit via
        # the dedicated socket + a fresh window instead.)
        tchome = os.path.join(self.root, "tchome")
        os.makedirs(tchome, exist_ok=True)
        with open(os.path.join(tchome, ".tmux.conf"), "w") as f:
            f.write("set -g history-limit 100000\n")
        # Raw tmux: explicit socket, TMUX_TMPDIR pointed somewhere harmless.
        self.raw_env = dict(base, TMUX_TMPDIR=self.raw_dir)
        self.raw_env.pop("TMUX", None)
        # tmuxctl: private socket dir -> sees only bench sessions; private
        # HOME -> bench-only sqlite DB + big-scrollback .tmux.conf (see above).
        self.tc_env = dict(base, TMUX_TMPDIR=self.tc_dir, HOME=tchome)
        self.tc_env.pop("TMUX", None)
        # aplexer: private runtime/state. Keep user config (engines).
        self.a_env = dict(
            base,
            APLEXER_RUNTIME_DIR=self.arun,
            APLEXER_STATE_DIR=self.astate,
        )
        self.results = {}
        self.seq = 0

    # -- helpers ---------------------------------------------------------
    def fresh(self, prefix):
        self.seq += 1
        return f"{prefix}-{os.getpid()}-{self.seq}"

    def check(self, cond, msg):
        if not cond:
            raise RuntimeError(msg)

    # raw tmux ----------------------------------------------------------
    def tmux(self, *tmux_args, env=None, timeout=60):
        return run(
            [TMUX_BIN, "-S", self.sock, *tmux_args],
            env=env or self.raw_env,
            timeout=timeout,
        )

    def tmux_create(self, name):
        return self.tmux(
            "new-session", "-d", "-s", name, "-c", self.ws,
            "-x", "200", "-y", "50", SHELL, "-l",
        )

    def tmux_kill(self, name):
        return self.tmux("kill-session", "-t", name)

    # tmuxctl -----------------------------------------------------------
    def tc_create(self, name):
        return run(
            [TMUXCTL_BIN, "create-detached", name, "-c", self.ws],
            env=self.tc_env, timeout=60,
        )

    def tc_kill(self, name):
        return run([TMUXCTL_BIN, "kill", "--yes", name], env=self.tc_env, timeout=60)

    # aplexer -----------------------------------------------------------
    def a_create(self, tag):
        return run(
            [A_BIN, "start", "--workspace", self.ws, "--tag", tag,
             "--", SHELL, "-l"],
            env=self.a_env, timeout=60,
        )

    def a_kill(self, tag):
        return run(
            [A_BIN, "kill", "--workspace", self.ws, "--tag", tag],
            env=self.a_env, timeout=60,
        )

    # -- generic timing loop ---------------------------------------------
    def measure(self, label, fn, iters, warmup=2):
        for _ in range(warmup):
            fn()
        samples = []
        for _ in range(iters):
            dt, p = fn()
            self.check(p.returncode == 0,
                       f"{label} failed rc={p.returncode}: {(p.stderr or b'')[:500]!r}")
            samples.append(dt)
        self.results[label] = stats(samples)
        s = self.results[label]
        print(f"  {label:<34} n={s['n']:>3}  "
              f"min={s['min']:7.1f} p50={s['p50']:7.1f} "
              f"p90={s['p90']:7.1f} max={s['max']:7.1f} ms", flush=True)

    # -- setup -----------------------------------------------------------
    def preflight(self):
        for path, name in ((A_BIN, "a"), (TMUXCTL_BIN, "tmuxctl"), (TMUX_BIN, "tmux")):
            self.check(os.path.exists(path), f"missing binary for {name}: {path}")
        self.check(run([A_BIN, "--version"], env=self.a_env).returncode == 0,
                   "a --version failed")
        # Start the shared raw-tmux server up front (excluded from timings).
        p = self.tmux_create("__warmup__")
        self.check(p.returncode == 0, f"tmux warmup create failed: {p.stderr[:300]!r}")
        self.tmux_kill("__warmup__")

    def seed(self, scale):
        """Create `scale` persistent sessions per system for list/status/send/capture."""
        print(f"seeding {scale} sessions per system ...", flush=True)
        self.tmux_names, self.tc_names, self.a_tags = [], [], []
        for i in range(scale):
            name = self.fresh("seed-tmux")
            p = self.tmux_create(name)
            self.check(p.returncode == 0, f"seed tmux failed: {p.stderr[:300]!r}")
            self.tmux_names.append(name)
            name = self.fresh("seed-tc")
            p = self.tc_create(name)
            self.check(p.returncode == 0, f"seed tmuxctl failed: {p.stderr[:300]!r}")
            self.tc_names.append(name)
            tag = self.fresh("seed-a")
            p = self.a_create(tag)
            self.check(p.returncode == 0, f"seed a failed: {p.stderr[:300]!r}")
            self.a_tags.append(tag)
        self.collect_scale_stats(scale)

    def collect_scale_stats(self, scale):
        """Record the daemonless scale cost (PLAN P2.1): N workers/sockets/
        locks is the known price of no central server. Read worker PIDs
        straight from the state dir (no RPC per session) and sum VmRSS
        from /proc, so the measurement itself does not perturb timings."""
        rss_kb = []
        sessions_root = os.path.join(self.astate, "sessions")
        try:
            for entry in os.listdir(sessions_root):
                rec_path = os.path.join(sessions_root, entry, "session.json")
                try:
                    with open(rec_path) as f:
                        rec = json.load(f)
                except (OSError, ValueError):
                    continue
                pid = rec.get("worker_pid")
                if not isinstance(pid, int):
                    continue
                try:
                    with open(f"/proc/{pid}/status") as f:
                        for line in f:
                            if line.startswith("VmRSS:"):
                                rss_kb.append(int(line.split()[1]))
                                break
                except (OSError, ValueError, IndexError):
                    continue
        except OSError:
            pass
        # Runtime sockets/locks: one control socket + worker lock per live
        # session, plus the registry lock.
        runtime_files = 0
        for _, _, files in os.walk(self.arun):
            runtime_files += len(files)
        stat = {
            "sessions_seeded": scale,
            "worker_count": len(rss_kb),
            "worker_rss_kb_total": int(sum(rss_kb)),
            "worker_rss_kb_mean": int(statistics.fmean(rss_kb)) if rss_kb else 0,
            "worker_rss_kb_max": int(max(rss_kb)) if rss_kb else 0,
            "runtime_files": runtime_files,
        }
        self.results["scale/a-worker-rss"] = stat
        print(f"  scale: {stat['worker_count']} workers, "
              f"RSS total={stat['worker_rss_kb_total'] // 1024} MiB "
              f"mean={stat['worker_rss_kb_mean'] // 1024} MiB "
              f"max={stat['worker_rss_kb_max'] // 1024} MiB "
              f"runtime_files={runtime_files}", flush=True)

    # -- attach/detach under a pty ---------------------------------------
    def _spawn_pty(self, argv, env):
        mfd, sfd = pty.openpty()
        p = subprocess.Popen(argv, stdin=sfd, stdout=sfd, stderr=sfd,
                             env=env, close_fds=True)
        os.close(sfd)
        return p, mfd

    def _drain(self, mfd, until=None, min_bytes=0, timeout=10.0):
        out = b""
        deadline = time.time() + timeout
        while time.time() < deadline:
            r, _, _ = select.select([mfd], [], [], 0.1)
            if r:
                try:
                    chunk = os.read(mfd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                out += chunk
                if until and until in out:
                    break
                if min_bytes and len(out) >= min_bytes:
                    break
            elif until is None and min_bytes == 0 and out:
                break
        return out

    def attach_tmux(self, name=None, sock=None, env=None):
        name = name or self.tmux_names[0]
        sock = sock or self.sock
        env = env or self.raw_env
        t0 = time.perf_counter()
        p, mfd = self._spawn_pty([TMUX_BIN, "-S", sock, "attach", "-t", name], env)
        try:
            # First evidence of a live attached client rendering. Mirrors
            # the a-path's break-on-banner: stop waiting as soon as the
            # client is demonstrably attached, then send the detach chord.
            out = self._drain(mfd, min_bytes=500, timeout=10.0)
            self.check(len(out) >= 500,
                       f"tmux attach never rendered: {len(out)} bytes")
            # Readiness gate: don't send the chord until the server reports
            # the client (an early chord is swallowed during startup).
            deadline = time.time() + 5.0
            while time.time() < deadline:
                p2 = run([TMUX_BIN, "-S", sock, "list-clients", "-t", name,
                          "-F", "#{client_tty}"], env=env, timeout=15)
                if (p2.stdout or b"").strip():
                    break
                time.sleep(0.01)
            else:
                raise RuntimeError("tmux client never appeared in list-clients")
            # Client-side detach chord, mirroring the a-attach path (C-b d).
            try:
                os.write(mfd, b"\x02d")
            except OSError:
                pass
            rc = p.wait(timeout=10)
            self.check(rc == 0, f"tmux attach exited rc={rc}")
        finally:
            try:
                os.close(mfd)
            except OSError:
                pass
            if p.poll() is None:
                p.kill()
        return (time.perf_counter() - t0) * 1000.0, len(out)

    def attach_a(self, tag=None, extra=()):
        tag = tag or self.a_tags[0]
        t0 = time.perf_counter()
        p, mfd = self._spawn_pty(
            [A_BIN, "attach", "--workspace", self.ws, "--tag", tag, *extra],
            self.a_env)
        try:
            out = self._drain(mfd, until=b"attached to", timeout=10.0)
            self.check(b"attached to" in out,
                       f"a attach never printed banner: {out[:200]!r}")
            try:
                os.write(mfd, b"\x02d")  # Ctrl-b d detaches
            except OSError:
                pass
            rc = p.wait(timeout=10)
            self.check(rc == 0, f"a attach exited rc={rc}")
        finally:
            try:
                os.close(mfd)
            except OSError:
                pass
            if p.poll() is None:
                p.kill()
        return (time.perf_counter() - t0) * 1000.0, len(out)

    def measure_attach(self, label, fn, iters):
        for _ in range(2):  # warmup
            fn()
        timed_samples, byte_samples = [], []
        for _ in range(iters):
            dt, nbytes = fn()
            timed_samples.append(dt)
            byte_samples.append(nbytes)
        self.results[label] = stats(timed_samples)
        self.results[label]["payload_bytes_mean"] = int(statistics.fmean(byte_samples))
        s = self.results[label]
        print(f"  {label:<34} n={s['n']:>3}  "
              f"min={s['min']:7.1f} p50={s['p50']:7.1f} "
              f"p90={s['p90']:7.1f} max={s['max']:7.1f} ms  "
              f"~{int(statistics.fmean(byte_samples))} bytes attach payload",
              flush=True)

    # -- roundtrip: send marker, poll capture until visible ---------------
    def roundtrip(self, send_fn, cap_fn, tag, iters):
        samples = []
        for i in range(iters):
            marker = f"RT{os.getpid()}{i}{time.monotonic_ns() % 100000}"
            t0 = time.perf_counter()
            p = send_fn(f"echo {marker}")
            assert p.returncode == 0, f"roundtrip send failed: {p.stderr[:300]!r}"
            deadline = time.time() + 10.0
            seen = False
            while time.time() < deadline:
                out = cap_fn()
                if marker.encode() in (out.stdout or b""):
                    seen = True
                    break
                time.sleep(0.005)
            dt = (time.perf_counter() - t0) * 1000.0
            self.check(seen, f"roundtrip marker {marker} never appeared for {tag}")
            samples.append(dt)
        return samples

    # -- heavy-output sessions (emulated huge agent session) ---------------
    def tc_socket(self, name):
        uid = os.getuid()
        return os.path.join(self.tc_dir, f"tmux-{uid}", f"tmuxctl-{name}")

    def seed_heavy(self, mb):
        """One session per system flooded with ~mb MB of ANSI-heavy output."""
        print(f"seeding heavy sessions (~{mb} MB scrollback each) ...", flush=True)
        lines = max(1000, int(mb * 1_000_000 / 150))
        flood = os.path.join(self.root, "flood.py")
        with open(flood, "w") as f:
            f.write(
                "import sys\n"
                "n = int(sys.argv[1])\n"
                "w = sys.stdout.write\n"
                "ESC = chr(27)\n"
                "for i in range(n):\n"
                "    w(f\"{ESC}[1;3{i % 8}m{i:08d}{ESC}[0m \"\n"
                "      \"lorem-ipsum-dolor-sit-amet-consectetur-adipiscing-elit-sed-do \"\n"
                "      \"eiusmod-tempor-incididunt-ut-labore-et-dolore-magna-aliqua \"\n"
                "      f\"{'x' * 40}\\n\")\n"
                "w(\"__FLOOD_DONE__\\n\")\n"
            )
        # NOTE: the completion marker is a fixed string printed by flood.py
        # itself -- it must NOT appear on the invocation command line, since
        # the shell echoes input into scrollback and would fake completion.
        token = "__FLOOD_DONE__"
        cmd = f"python3 {flood} {lines}"

        self.heavy_tmux = self.fresh("heavy-tmux")
        # tmux applies history-limit only to windows created after the
        # option is set: raise the server-global first via a throwaway
        # session, then create the real heavy session.
        pre = self.tmux_create("__heavy_pre__")
        self.check(pre.returncode == 0, f"pre create failed: {pre.stderr[:200]!r}")
        p = self.tmux("set-option", "-g", "history-limit", "100000")
        self.check(p.returncode == 0, f"history-limit set failed: {p.stderr[:200]!r}")
        self.tmux_kill("__heavy_pre__")
        p = self.tmux_create(self.heavy_tmux)
        self.check(p.returncode == 0, f"heavy tmux create failed: {p.stderr[:200]!r}")

        self.heavy_tc = self.fresh("heavy-tc")
        p = self.tc_create(self.heavy_tc)
        self.check(p.returncode == 0, f"heavy tmuxctl create failed: {p.stderr[:200]!r}")
        self.tc_heavy_sock = self.tc_socket(self.heavy_tc)

        def tc_sock_run(*args):
            return run([TMUX_BIN, "-S", self.tc_heavy_sock, *args],
                       env=self.tc_env, timeout=30)

        # tmuxctl servers boot under systemd-run, which does not inherit the
        # private-HOME .tmux.conf -- and tmux grants history-limit only to
        # windows created after it is raised. So: raise the server-global,
        # open a fresh window (born with the big buffer), drop window 0.
        p = tc_sock_run("set-option", "-g", "history-limit", "100000")
        self.check(p.returncode == 0,
                   f"tc history-limit set failed: {p.stderr[:200]!r}")
        p = tc_sock_run("new-window", "-t", self.heavy_tc, "-c", self.ws,
                        SHELL, "-l")
        self.check(p.returncode == 0, f"tc new-window failed: {p.stderr[:200]!r}")
        p = tc_sock_run("kill-window", "-t", f"{self.heavy_tc}:0")
        self.check(p.returncode == 0, f"tc kill-window failed: {p.stderr[:200]!r}")
        p = tc_sock_run("display-message", "-p", "-t", self.heavy_tc,
                        "#{history_limit}")
        self.check((p.stdout or b"").strip() == b"100000",
                   f"tc heavy window limit not 100000: {(p.stdout or b'')[:100]!r}")

        self.heavy_a = self.fresh("heavy-a")
        p = self.a_create(self.heavy_a)
        self.check(p.returncode == 0, f"heavy a create failed: {p.stderr[:200]!r}")

        self.tmux("send-keys", "-t", self.heavy_tmux, cmd, "Enter")
        run([TMUXCTL_BIN, "send", self.heavy_tc, "--message", cmd,
             "--enter-delay-ms", "0"], env=self.tc_env, timeout=30)
        run([A_BIN, "send", "--workspace", self.ws, "--tag", self.heavy_a,
             "--enter", cmd], env=self.a_env, timeout=30)

        # Wait for the flood to land in all three sessions.
        deadline = time.time() + 180.0
        tok = token.encode()
        while time.time() < deadline:
            t_full = self.tmux("capture-pane", "-p", "-t", self.heavy_tmux,
                               "-S", "-100000")
            c_full = run([TMUX_BIN, "-S", self.tc_heavy_sock, "capture-pane",
                          "-p", "-t", self.heavy_tc, "-S", "-100000"],
                         env=self.tc_env, timeout=60)
            a_scr = run([A_BIN, "capture", "--workspace", self.ws, "--tag",
                         self.heavy_a, "--screen", "--plain"],
                        env=self.a_env, timeout=60)
            if (tok in (t_full.stdout or b"") and tok in (c_full.stdout or b"")
                    and tok in (a_scr.stdout or b"")):
                break
            time.sleep(0.5)
        else:
            raise RuntimeError("heavy flood never completed")
        self.heavy_bytes = {
            "tmux": len(t_full.stdout or b""),
            "tmuxctl": len(c_full.stdout or b""),
            "a": len(run([A_BIN, "capture", "--workspace", self.ws, "--tag",
                          self.heavy_a], env=self.a_env, timeout=120).stdout or b""),
        }
        print(f"  scrollback retained: tmux={self.heavy_bytes['tmux'] // 1024} KiB "
              f"tmuxctl={self.heavy_bytes['tmuxctl'] // 1024} KiB "
              f"a={self.heavy_bytes['a'] // 1024} KiB", flush=True)
        self.results["heavy/retained-bytes"] = dict(self.heavy_bytes)

    def measure_capture(self, label, fn, iters):
        for _ in range(2):  # warmup
            fn()
        timed_samples, byte_samples = [], []
        for _ in range(iters):
            dt, p = fn()
            self.check(p.returncode == 0,
                       f"{label} failed rc={p.returncode}: {(p.stderr or b'')[:300]!r}")
            timed_samples.append(dt)
            byte_samples.append(len(p.stdout or b""))
        self.results[label] = stats(timed_samples)
        self.results[label]["payload_bytes_mean"] = int(statistics.fmean(byte_samples))
        s = self.results[label]
        print(f"  {label:<34} n={s['n']:>3}  "
              f"min={s['min']:7.1f} p50={s['p50']:7.1f} "
              f"p90={s['p90']:7.1f} max={s['max']:7.1f} ms  "
              f"~{int(statistics.fmean(byte_samples)) // 1024} KiB payload",
              flush=True)

    def selected(self, name):
        only = self.args.only
        return not only or name in only

    def run_all(self):
        it = self.args.iters
        scale = self.args.scale
        self.preflight()
        self.seed(scale)

        if self.selected("create"):
            print("create (detached, unique session each iteration):", flush=True)

            def fn_tmux():
                n = self.fresh("c-tmux")
                t0 = time.perf_counter()
                p = self.tmux_create(n)
                dt = (time.perf_counter() - t0) * 1000.0
                self.tmux_kill(n)
                return dt, p

            def fn_tc():
                n = self.fresh("c-tc")
                t0 = time.perf_counter()
                p = self.tc_create(n)
                dt = (time.perf_counter() - t0) * 1000.0
                self.tc_kill(n)
                return dt, p

            def fn_a():
                t = self.fresh("c-a")
                t0 = time.perf_counter()
                p = self.a_create(t)
                dt = (time.perf_counter() - t0) * 1000.0
                self.a_kill(t)
                return dt, p

            self.measure("create/tmux-new-session", fn_tmux, it)
            self.measure("create/tmuxctl-create-detached", fn_tc, it)
            self.measure("create/a-start", fn_a, it)

        if self.selected("list"):
            print(f"list (with {scale} sessions present):", flush=True)
            self.measure("list/tmux-list-sessions",
                         lambda: timed([TMUX_BIN, "-S", self.sock, "list-sessions"],
                                       self.raw_env), it)
            self.measure("list/tmuxctl-list",
                         lambda: timed([TMUXCTL_BIN, "list"], self.tc_env), it)
            self.measure("list/a-list",
                         lambda: timed([A_BIN, "list"], self.a_env), it)
            self.measure("list/a-list-json",
                         lambda: timed([A_BIN, "--json", "list"], self.a_env), it)

        if self.selected("status"):
            print("status (single session):", flush=True)
            tn, cn, at = self.tmux_names[0], self.tc_names[0], self.a_tags[0]
            self.measure("status/tmux-display-message",
                         lambda: timed([TMUX_BIN, "-S", self.sock, "display-message",
                                        "-p", "-t", tn, "-F",
                                        "#{session_name} #{session_created}"],
                                       self.raw_env), it)
            self.measure("status/tmuxctl-describe",
                         lambda: timed([TMUXCTL_BIN, "describe", cn], self.tc_env), it)
            self.measure("status/a-status",
                         lambda: timed([A_BIN, "status", "--workspace", self.ws,
                                        "--tag", at], self.a_env), it)
            self.measure("status/a-status-json",
                          lambda: timed([A_BIN, "--json", "status", "--workspace",
                                         self.ws, "--tag", at], self.a_env), it)

        if self.selected("switch"):
            # PLAN P2.2 chord-to-first-byte proxy: resolve the *other* seeded
            # session + status handshake, alternating A/B so neither the
            # registry scan nor the worker connection can sit hot. This is
            # the resolve + handshake half of an in-process Ctrl-b n/p
            # switch; the full attach handshake above remains its upper
            # bound. Budget: 20 ms (design doc); over-budget aplexer p50 is
            # reported as a WARNING, not a failure (see report()).
            print("switch resolve+handshake proxy (alternating A/B):", flush=True)
            tmux_a = self.tmux_names[0]
            tmux_b = self.tmux_names[1] if len(self.tmux_names) > 1 else self.tmux_names[0]
            tc_a = self.tc_names[0]
            tc_b = self.tc_names[1] if len(self.tc_names) > 1 else self.tc_names[0]
            a_a = self.a_tags[0]
            a_b = self.a_tags[1] if len(self.a_tags) > 1 else self.a_tags[0]
            flip = {"i": 0}

            def fn_tmux_switch():
                flip["i"] += 1
                name = tmux_b if flip["i"] % 2 else tmux_a
                return timed([TMUX_BIN, "-S", self.sock, "display-message",
                              "-p", "-t", name, "-F",
                              "#{session_name} #{session_created}"],
                             self.raw_env)

            def fn_tc_switch():
                flip["i"] += 1
                name = tc_b if flip["i"] % 2 else tc_a
                return timed([TMUXCTL_BIN, "describe", name], self.tc_env)

            def fn_a_switch():
                flip["i"] += 1
                tag = a_b if flip["i"] % 2 else a_a
                return timed([A_BIN, "status", "--workspace", self.ws,
                              "--tag", tag], self.a_env)

            self.measure("switch/tmux-resolve-handshake", fn_tmux_switch, it)
            self.measure("switch/tmuxctl-resolve-handshake", fn_tc_switch, it)
            self.measure("switch/a-resolve-handshake", fn_a_switch, it)

        if self.selected("send"):
            print("send (one line, no attach):", flush=True)
            tn, cn, at = self.tmux_names[0], self.tc_names[0], self.a_tags[0]
            self.measure("send/tmux-send-keys",
                         lambda: timed([TMUX_BIN, "-S", self.sock, "send-keys",
                                        "-t", tn, "echo bench-send", "Enter"],
                                       self.raw_env), it)
            self.measure("send/tmuxctl-send",
                         lambda: timed([TMUXCTL_BIN, "send", cn, "--message",
                                        "echo bench-send", "--enter-delay-ms", "0"],
                                       self.tc_env), it)
            self.measure("send/a-send",
                         lambda: timed([A_BIN, "send", "--workspace", self.ws,
                                        "--tag", at, "--enter", "echo bench-send"],
                                       self.a_env), it)

        if self.selected("capture"):
            print("capture (read back output):", flush=True)
            tn, at = self.tmux_names[0], self.a_tags[0]
            # NOTE: tmuxctl has no capture command of its own; its sessions
            # are plain tmux servers, so raw-tmux capture-pane numbers apply.
            self.measure("capture/tmux-capture-pane",
                         lambda: timed([TMUX_BIN, "-S", self.sock, "capture-pane",
                                        "-p", "-t", tn], self.raw_env), it)
            self.measure("capture/a-capture",
                         lambda: timed([A_BIN, "capture", "--workspace", self.ws,
                                        "--tag", at], self.a_env), it)
            self.measure("capture/a-capture-screen-plain",
                         lambda: timed([A_BIN, "capture", "--workspace", self.ws,
                                        "--tag", at, "--screen", "--plain"],
                                       self.a_env), it)

        if self.selected("roundtrip"):
            print("roundtrip send->visible (includes shell scheduling):", flush=True)
            tn, at = self.tmux_names[0], self.a_tags[0]
            r = self.args.roundtrips
            s = self.roundtrip(
                lambda msg: self.tmux("send-keys", "-t", tn, msg, "Enter"),
                lambda: self.tmux("capture-pane", "-p", "-t", tn),
                "tmux", r)
            self.results["roundtrip/tmux"] = stats(s)
            s = self.roundtrip(
                lambda msg: run([A_BIN, "send", "--workspace", self.ws, "--tag", at,
                                 "--enter", msg], env=self.a_env),
                lambda: run([A_BIN, "capture", "--workspace", self.ws, "--tag", at],
                            env=self.a_env),
                "a", r)
            self.results["roundtrip/a"] = stats(s)
            for k in ("roundtrip/tmux", "roundtrip/a"):
                v = self.results[k]
                print(f"  {k:<34} n={v['n']:>3}  min={v['min']:7.1f} "
                      f"p50={v['p50']:7.1f} p90={v['p90']:7.1f} max={v['max']:7.1f} ms",
                      flush=True)

        if self.selected("kill"):
            print("kill (session pre-created, timing only the kill):", flush=True)

            def fn_tmux():
                n = self.fresh("k-tmux")
                self.tmux_create(n)
                return timed([TMUX_BIN, "-S", self.sock, "kill-session", "-t", n],
                             self.raw_env)

            def fn_tc():
                n = self.fresh("k-tc")
                self.tc_create(n)
                return timed([TMUXCTL_BIN, "kill", "--yes", n], self.tc_env)

            def fn_a():
                t = self.fresh("k-a")
                self.a_create(t)
                return timed([A_BIN, "kill", "--workspace", self.ws, "--tag", t],
                             self.a_env)

            def fn_a_fast():
                t = self.fresh("k-a")
                self.a_create(t)
                return timed([A_BIN, "kill", "--workspace", self.ws, "--tag", t,
                              "--grace-ms", "0"], self.a_env)

            self.measure("kill/tmux-kill-session", fn_tmux, it)
            self.measure("kill/tmuxctl-kill", fn_tc, it)
            # NOTE: a kill defaults to --grace-ms 2000 (TERM, wait, escalate),
            # so the default row includes ~2s of intentional waiting. The
            # grace-0 row is the mechanism cost, comparable to kill-session.
            self.measure("kill/a-kill(default-grace-2s)", fn_a, it)
            self.measure("kill/a-kill-grace-0", fn_a_fast, it)

        if self.selected("attach"):
            print("attach+detach cycle (pty):", flush=True)
            self.measure_attach("attach/tmux-attach-detach",
                                self.attach_tmux, self.args.attach_iters)
            self.measure_attach("attach/a-attach-detach",
                                self.attach_a, self.args.attach_iters)

        if self.selected("heavy"):
            self.seed_heavy(self.args.heavy_mb)
            print("heavy attach+detach (session holding MBs of scrollback):", flush=True)
            self.measure_attach(
                "heavy-attach/tmux",
                lambda: self.attach_tmux(name=self.heavy_tmux),
                self.args.attach_iters)
            self.measure_attach(
                "heavy-attach/tmuxctl",
                lambda: self.attach_tmux(name=self.heavy_tc,
                                         sock=self.tc_heavy_sock, env=self.tc_env),
                self.args.attach_iters)
            self.measure_attach(
                "heavy-attach/a-screen",
                lambda: self.attach_a(tag=self.heavy_a),
                self.args.attach_iters)
            self.measure_attach(
                "heavy-attach/a-history32k",
                lambda: self.attach_a(tag=self.heavy_a,
                                      extra=("--history-bytes", "32768")),
                self.args.attach_iters)
            print("heavy capture (full scrollback read-back):", flush=True)
            self.measure_capture(
                "heavy-capture/tmux-full",
                lambda: timed([TMUX_BIN, "-S", self.sock, "capture-pane",
                               "-p", "-t", self.heavy_tmux, "-S", "-100000"],
                              self.raw_env, timeout=120), it)
            self.measure_capture(
                "heavy-capture/tmuxctl-full",
                lambda: timed([TMUX_BIN, "-S", self.tc_heavy_sock, "capture-pane",
                               "-p", "-t", self.heavy_tc, "-S", "-100000"],
                              self.tc_env, timeout=120), it)
            self.measure_capture(
                "heavy-capture/a-full",
                lambda: timed([A_BIN, "capture", "--workspace", self.ws,
                               "--tag", self.heavy_a],
                              self.a_env, timeout=120), it)
            self.measure_capture(
                "heavy-capture/a-screen-plain",
                lambda: timed([A_BIN, "capture", "--workspace", self.ws,
                               "--tag", self.heavy_a, "--screen", "--plain"],
                              self.a_env, timeout=120), it)

    def cleanup(self):
        for n in getattr(self, "tmux_names", []):
            try:
                self.tmux_kill(n)
            except Exception:
                pass
        for n in getattr(self, "tc_names", []):
            try:
                self.tc_kill(n)
            except Exception:
                pass
        for t in getattr(self, "a_tags", []):
            try:
                self.a_kill(t)
            except Exception:
                pass
        for n in [getattr(self, "heavy_tmux", None)]:
            if n:
                try:
                    self.tmux_kill(n)
                except Exception:
                    pass
        if getattr(self, "heavy_tc", None):
            try:
                self.tc_kill(self.heavy_tc)
            except Exception:
                pass
        if getattr(self, "heavy_a", None):
            try:
                self.a_kill(self.heavy_a)
            except Exception:
                pass
        try:
            run([TMUX_BIN, "-S", self.sock, "kill-server"], env=self.raw_env, timeout=15)
        except Exception:
            pass
        if not self.args.keep:
            shutil.rmtree(self.root, ignore_errors=True)
        else:
            print(f"kept bench dir: {self.root}")

    def versions(self):
        def out(argv, env):
            try:
                p = run(argv, env=env, timeout=15)
                return (p.stdout or b"").decode().strip().splitlines()[:1]
            except Exception as e:
                return [f"error: {e}"]

        return {
            "tmux": out([TMUX_BIN, "-V"], self.raw_env),
            "a": out([A_BIN, "--version"], self.a_env),
            "tmuxctl": "n/a (typer, no --version)",
            "python": platform.python_version(),
            "uname": platform.uname()._asdict(),
        }

    def report(self):
        print("\n================ results (ms) ================")
        hdr = f"  {'op':<34} {'n':>3}  {'min':>7} {'p50':>7} {'p90':>7} {'max':>7} {'mean':>7}"
        print(hdr)
        stat_items = {k: v for k, v in self.results.items() if "p50" in v}
        for k in sorted(stat_items):
            v = stat_items[k]
            print(f"  {k:<34} {v['n']:>3}  {v['min']:7.1f} {v['p50']:7.1f} "
                  f"{v['p90']:7.1f} {v['max']:7.1f} {v['mean']:7.1f}")
        print("\nrelative to raw tmux p50 (higher = slower):")
        base = {}
        for k, v in stat_items.items():
            fam = k.split("/")[0]
            sysname = k.split("/")[1].split("-")[0]
            if sysname == "tmux":
                base[fam] = v["p50"]
        for k in sorted(stat_items):
            fam = k.split("/")[0]
            v = stat_items[k]
            b = base.get(fam)
            rel = f"{v['p50'] / b:.2f}x" if b else "n/a (no tmux baseline)"
            print(f"  {k:<34} p50={v['p50']:7.1f} ms  vs-tmux={rel}")
        # PLAN P2.2: 20 ms chord-to-first-byte budget. Attach is the upper
        # bound for an in-process switch (resolve + handshake); the switch
        # proxy above is the direct resolve + handshake number. Flag, don't
        # fail: loaded-box noise is ±2x run to run.
        for key, budget in (("switch/a-resolve-handshake", 20.0),
                            ("attach/a-attach-detach", 20.0)):
            v = stat_items.get(key)
            if v and v["p50"] > budget:
                print(f"  WARNING: {key} p50={v['p50']:.1f} ms exceeds "
                      f"{budget:.0f} ms chord-to-first-byte budget")
        rss = self.results.get("scale/a-worker-rss")
        if isinstance(rss, dict):
            print(f"\nscale: {rss.get('worker_count')} aplexer workers, "
                  f"RSS total={rss.get('worker_rss_kb_total', 0) // 1024} MiB "
                  f"mean={rss.get('worker_rss_kb_mean', 0) // 1024} MiB "
                  f"max={rss.get('worker_rss_kb_max', 0) // 1024} MiB "
                  f"runtime_files={rss.get('runtime_files')}")


def main():
    ap = argparse.ArgumentParser(description="Benchmark tmux vs tmuxctl vs aplexer")
    ap.add_argument("--iters", type=int, default=25)
    ap.add_argument("--scale", type=int, default=25)
    ap.add_argument("--roundtrips", type=int, default=6)
    ap.add_argument("--attach-iters", type=int, default=5)
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--only", default="", help="comma list: create,list,status,switch,send,capture,roundtrip,kill,attach,heavy")
    ap.add_argument("--heavy-mb", type=int, default=4)
    ap.add_argument("--json-out", default="")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    if args.quick:
        args.iters = 8
        args.scale = 10
        args.roundtrips = 3
        args.attach_iters = 2
    args.only = [o.strip() for o in args.only.split(",") if o.strip()] if args.only else []

    b = Bench(args)
    try:
        b.run_all()
    finally:
        b.cleanup()
    b.report()
    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump({"versions": b.versions(), "results": b.results}, f, indent=2)
        print(f"\nwrote {args.json_out}")
    print("\nversions:", json.dumps(b.versions(), default=str))


if __name__ == "__main__":
    sys.exit(main())
