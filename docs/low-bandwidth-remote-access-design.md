# Low-bandwidth remote access design

Status: planning document, 2026-08-26. Nothing here is implemented; this is a design for review.
It targets the PocketShell-over-SSH scenario from spec.md §22.3 and
[docs/pocketshell-integration-plan.md](pocketshell-integration-plan.md) (Phase B): a phone on a
cellular link — high RTT (100–400 ms), modest throughput, frequent disconnects (backgrounding,
tower handoff, tunnels) and a real per-byte battery/data cost — attaching to aplexer sessions on
a remote host.

Sources: spec.md (§5, §16.3, §17, §18, §22.3, §26, §30), the current implementation
(`src/bin/a.rs::attach()` and the status-bar code around it, `src/worker.rs::handle_attach`,
`src/lib.rs::History`), [docs/pocketshell-integration-plan.md](pocketshell-integration-plan.md),
and [docs/inter-agent-messaging-design.md](inter-agent-messaging-design.md) §3.3 (whose
pull-vs-push transport decision this design deliberately stays consistent with).

---

## 1. Where the bytes actually go today

Two distinct SSH shapes exist, per spec §22.3 and the integration plan (§1.1: PocketShell holds a
warm SSH connection, "lease", and opens channels on it):

**Attach (interactive):** one SSH *PTY/shell channel* running `a attach <ws>:<tag>` on the host.

```text
phone terminal emulator (PocketShell)
    ▲ │ SSH channel (the ONLY network leg; TCP, encrypted, optionally zlib-compressed)
    │ ▼
remote sshd ── sshd-allocated PTY
    ▲ │ stdout/stdin
    │ ▼
`a attach` client process (src/bin/a.rs::attach)
    ▲ │ Unix domain socket, aplexer frames (local, effectively free)
    │ ▼
per-session worker (src/worker.rs::handle_attach)
    ▲ │ PTY master
    │ ▼
workload (claude / codex / shell / ...)
```

**Snapshot/list (polling):** one SSH *exec channel* per call running `a snapshot --json` /
`a list --json`, output read to EOF.

What crosses the network in the attach case is exactly what `a attach` writes to its stdout:

1. the initial history replay (default 32 KB tail, `DEFAULT_ATTACH_REPLAY_BYTES` in
   `src/bin/a.rs`, overridable with `--history-bytes`);
2. live PTY output frames, forwarded verbatim as they arrive;
3. **client-generated chrome**: the DECSTBM layout escapes from `apply_terminal_layout`, and a
   full-width reverse-video status-bar redraw from `draw_status_bar` **every 1500 ms,
   unconditionally**, from the status-bar thread in `attach()`.

Note that (3) is generated *by the attach client on the host*, not by the worker: it never enters
the worker's history buffer (so `capture`/replay stay unpolluted), but it *does* traverse the SSH
channel on every tick.

### 1.1 What SSH already solves for free

SSH has stream-level compression (`ssh -C` / `Compression yes`, `zlib@openssh.com`): zlib with a
persistent per-connection dictionary applied to the whole channel stream. This is close to ideal
for terminal traffic: full-screen TUI redraws, cursor-position spam, and the status bar's
mostly-identical 1.5-second repaints are precisely the highly repetitive byte streams zlib eats
— multi-x reduction is typical on TUI-heavy output (exact ratio unmeasured here; see §8).

**A structural point makes this more than a convenience:** in the current architecture, the only
network leg carries *raw terminal bytes between sshd and the phone*. The aplexer frame protocol
lives entirely on a local Unix socket. If aplexer compressed worker→client frames, `a attach`
would decompress them before writing to stdout, and nothing compressed would ever touch the
network. **Aplexer-protocol compression is therefore not merely redundant with SSH compression —
in the current passthrough model it is structurally useless.** It could only matter in a future
"framed attach" mode where the phone speaks aplexer frames directly (§4.2), and even there SSH
compression underneath makes it redundant.

So: a client willing to enable SSH compression gets the majority of the raw-byte-volume problem
solved with zero aplexer changes. PocketShell should enable it on its connections (verify its SSH
library supports `zlib@openssh.com` — a PocketShell-side task; the desktop fork's `ssh2` supports
it). This is recommendation #1 in §7.

### 1.2 What SSH compression does NOT solve

1. **Bytes that shouldn't exist at all.** Compression shrinks the status-bar repaint; it does not
   stop the *packet*. On cellular the dominant cost of small periodic traffic is not bytes but
   radio state: any packet keeps the LTE/5G radio out of its idle state (inactivity timers are on
   the order of ~10 s), so a 1.5 s cadence keeps the radio effectively always-on while attached.
   Battery, not bandwidth. Only *not sending* fixes this. (§2)
2. **Redundant replay on reconnect.** The 32 KB tail is re-sent on every reattach even though the
   phone usually still has the screen content. Compression divides the cost; resume semantics
   eliminate it. (§3, §4)
3. **Latency/jitter mechanics.** Everything shares one TCP stream; a replay burst delays the
   first interactive byte behind it (32 KB at 1 Mbit/s ≈ 0.26 s, at 128 kbit/s ≈ 2 s — before
   compression). Smaller bursts help regardless of compression.
4. **Per-exec overhead of polling.** Channel setup and process spawn per snapshot call, and the
   radio-wakeup cadence of polling itself. Compression is irrelevant there. (§6)

The rest of this design scopes aplexer-level work to exactly these four leftovers.

---

## 2. The status bar and other aplexer-originated chatter

Current behavior (`src/bin/a.rs`): the status-bar thread calls `draw_status_bar` every 1500 ms
whether or not anything changed. Each redraw writes ~25 bytes of escapes plus a *full-width
padded row* (`pad_or_truncate` to `cols`), so ~105–250 bytes per tick depending on width. Rough
idle cost per attached session (estimate, to be measured — §8):

- payload: 2400 redraws/h × ~150 B ≈ **0.35 MB/h**; with per-packet TCP/IP/SSH overhead,
  order of **0.5–1 MB/h**;
- radio: **2400 wakeups/h**, i.e. the radio never idles;
- host side: each tick also runs a `Status` RPC to the worker (`memory_indicator`) and a full
  `list_records` scan of every session's JSON (`sibling_summary`) — cheap per spec §30, but
  pointless at 0.67 Hz when nothing changed.

### 2.1 Fix one: dirty-checked redraws (unconditional, benefits everyone)

Only write when the rendered text or the geometry actually differs from the last write. The bar's
content is naturally quantized — `format_bytes` rounds memory to whole units, sibling states
change rarely — so an idle session's bar is byte-identical tick after tick. Keep the existing
periodic *computation* (it is the change detector) but suppress the *write*. This removes nearly
all idle network chatter and needs no flag, no mode, no protocol change. It also fixes an
unrelated wart: redrawing over the top of nothing 40 times a minute.

The host-side RPC/scan per tick can stay (it's how change is detected); optionally back the
computation interval off (e.g. to 5 s) after N unchanged ticks, resetting on PTY activity.

### 2.2 Fix two: an explicit frugal mode, not auto-detection

Add attach flags:

- `--no-status` — no status bar, no DECSTBM reservation, no redraw thread. What a programmatic
  client (PocketShell) should pass anyway: the app renders its own native status UI from
  `a snapshot --json` data and does not want a server-drawn ANSI row (see also §4.2 — in the
  framed-attach future the bar disappears from the wire by construction).
- `--lean` (alias: `--low-bandwidth`) — umbrella flag for constrained links: implies a slower
  status recompute interval (e.g. 10 s), a smaller default replay (§3), and any future frugal
  behaviors, so clients don't chase individual knobs. Individual flags still win over `--lean`.

**Why explicit flags and not RTT/throughput probing:** aplexer's attach client cannot see the
link — it sits between a local Unix socket and a local PTY; the SSH hop is invisible to it.
Probing would mean inventing an in-band measurement protocol to estimate a property that
PocketShell, a mobile app, *already knows first-hand* (network type, metered-ness, signal, its
own observed SSH latency). The client passing a flag is one line; probing is a subsystem with
false positives. Not worth it. One cheap, honest middle ground worth taking: when `$SSH_TTY` /
`$SSH_CONNECTION` is set (attach is running under sshd), default the status recompute interval
to the slower value — a remote human attacher gets frugality without knowing the flag. Explicit
flags remain the contract; the env check is a courtesy default, not detection.

This mirrors the messaging design's posture (§3.3 there): the simple, explicit, client-driven
mechanism is the contract; anything adaptive is a later optimization layered on top, never the
substrate.

---

## 3. History replay on (re)attach

Current behavior: the client defaults `Attach{history_bytes}` to 32 KB
(`DEFAULT_ATTACH_REPLAY_BYTES`); the worker returns that many tail bytes of its ring
(`History::snapshot`) as one Data frame before live streaming. `--history-bytes N` already
plumbs through end to end, **including `--history-bytes 0`** — so a bandwidth-constrained client
can already choose its replay size today with zero aplexer changes.

Recommendations:

1. **Keep 32 KB as the default for human/local attaches.** It was chosen for UX (not "rewinding
   everything") and is fine on a LAN.
2. **PocketShell passes a small value on constrained links** (folded into `--lean`: default e.g.
   4 KB ≈ 1–2 screens of typical output). No adaptivity inside aplexer — the client picks the
   number from its own network signal, same argument as §2.2. With SSH compression, 4 KB of TUI
   tail is likely ~1 KB on the wire; reconnect becomes visually instant even on bad links.
3. **Cheaper "what does the screen look like now", without a terminal emulator:** let the
   *workload* repaint. Add `--repaint` (implied by `--lean` for agent sessions): attach with a
   tiny (or zero) replay, then have the worker do a SIGWINCH wiggle — resize the PTY to
   (rows−1, cols) and immediately back. Full-screen TUIs (every agent CLI aplexer targets)
   respond by redrawing the *current* screen — which is both smaller and more correct than N KB
   of raw scrollback tail, and is exactly the trick tmux relies on when client sizes change. The
   worker already has `resize`; the wiggle is a few lines. Honest caveats: (a) it only helps
   programs that repaint on SIGWINCH — a bare shell prompt won't replay history, so shell
   sessions should keep a small tail replay; (b) PTY size is global to the session, so the wiggle
   is visible to any *other* attached client. Multi-client sizing now follows tmux's
   `window-size=latest` policy rather than unconditional last-resize-wins, but a synthetic wiggle
   would still temporarily affect the one shared PTY.
4. **A terminal-state parser (real screen snapshots) remains the eventual clean answer** — spec
   §17 already lists it as a later feature, and it is what "send exactly one screenful,
   perfectly" requires. It is *not* load-bearing here: SSH compression + small tail + repaint
   wiggle get ~all of the practical benefit at ~1% of the cost. Do not build it for this.

---

## 4. Reconnection/resume semantics

The question: after an SSH drop, is plain re-attach (pay the replay each time) good enough, or
should the worker support "give me only what's new since position N"?

### 4.1 What resume would require

The worker has no absolute stream position today — `History` is a capped `VecDeque` with no
running total. The design, concretely:

- **Worker:** keep a monotonically increasing `u64 stream_offset` = total bytes ever appended to
  the output hub, plus an **attach epoch** (worker start time + session UUID, or a random ID
  minted at worker start) since offsets reset when a worker restarts. `Attach` grows an optional
  `since: {epoch, offset}` field. If `epoch` matches and `offset` is within
  `[stream_offset − ring_len, stream_offset]`, replay exactly `ring[offset..]` and report
  `resumed: true`; otherwise fall back to the normal tail replay and report `resumed: false` so
  the client knows to clear and repaint rather than append.
- **Client:** learn the offset at the *end* of the initial replay from the attach response
  (extend the existing `{"attached":true,"history_bytes":N}` with `epoch` and `offset_end`), then
  count Data-frame payload bytes received. Frames are in-order, so this is exact — **on the Unix
  socket**.

### 4.2 The honest complication: a PTY passthrough can't count reliably

That client-side counting works for `a attach` the host process, but *PocketShell* receives
bytes through an sshd-allocated PTY, where the line discipline may translate output (and where
"bytes my emulator consumed" ≠ "bytes `a attach` received" across a drop that kills the host
process holding the count). Exact resume for the phone therefore effectively requires the phone
to consume aplexer's *frames* directly — i.e. `a attach --framed` over an SSH **exec** channel
(no PTY): length-prefixed Data frames with offsets in the protocol, status/exit as JSON frames.
This is precisely the shape PocketShell already speaks for tmux (`-CC` control mode with
`%output` events, per the integration plan §1.1), so it is the natural Phase B integration
surface anyway — and it eliminates the status-bar bytes by construction (§2.2) and gives the app
byte-exact resume for free.

### 4.3 Recommendation

- **Do not build resume now.** After §2 and §3 land, a reconnect costs roughly a compressed 1–4
  KB replay — the savings from resume are small in bytes. What resume actually buys is *UX*:
  seamless continuation with no clear/repaint flicker, which matters at PocketShell Phase B
  polish, not before.
- **Design it as above and build it as part of the framed-attach mode** when the integration
  plan's Phase B attach work happens. The worker-side pieces (offset counter, epoch, `since`
  handling with tail fallback) are modest and independently testable; nothing about the current
  protocol needs breaking (all fields are additive). Plain `a attach` for humans keeps
  reconnect-and-replay semantics forever — it's the right behavior for a dumb terminal.

---

## 5. Snapshot/list polling over SSH exec

Payload size is a non-issue, confirming spec §30's design goal: tens of sessions × ~200 B of
JSON ≈ a few KB, milliseconds to produce, compresses further. The costs that actually exist on
mobile, per poll:

- **latency**: on PocketShell's warm connection ("lease"), an exec channel is ~2 RTTs plus `a`
  process startup (Rust binary + metadata scan, milliseconds) — ~0.3–0.8 s wall time at mobile
  RTTs. A *cold* SSH connection would be 4–6+ RTTs plus auth; the warm-lease model already
  avoids that.
- **battery**: the poll *cadence* is the whole story. Polling every 5 s = 720 radio
  wakeups/hour, dwarfing the payload cost — the same shape as the status bar in §2.

So yes: **polling frequency, not payload size, is the mobile concern here**, and the
inter-agent-messaging design's §3.3 decision applies unchanged: pull/polling is the v1 contract
(cheap per call, well within spec §30 budgets), and push — a single long-lived SSH channel
running `a watch --jsonl` (spec §19, integration plan Part 2) — is the later optimization that
replaces N polls/hour with near-zero idle traffic. This design adds no new mechanism and should
not accelerate `watch` for bandwidth reasons alone; what it adds is client guidance:

- PocketShell should poll **adaptively by app state** (foreground-visible: 3–5 s; background:
  stop or near-stop; refresh opportunistically on user interaction and on events it already has,
  like attach/detach) — all client-side, zero aplexer changes;
- batch: one exec returning `a snapshot --json` already covers list+state; avoid multiple execs
  per UI refresh;
- when `watch` lands, one idle SSH channel (with keepalives) replaces the poll loop; its
  reconnect/gap story is already sketched in the integration plan (generation-gap detection +
  snapshot fallback) and the messaging design (§3.3: watchers are rebuildable, durable state is
  the truth).

---

## 6. Rejected / non-goals

- **Aplexer-protocol compression** — structurally useless in the passthrough model (§1.1);
  redundant with SSH compression even in a framed mode. Revisit only if a measured framed-attach
  deployment somehow can't use SSH compression.
- **Link-quality auto-detection (RTT/throughput probing) in aplexer** — the client knows the
  link; aplexer can't see it (§2.2). Explicit flags + the `$SSH_TTY` courtesy default.
- **A terminal emulator for screen snapshots** — spec §17 v1 non-goal; not load-bearing (§3, item 4).
- **Output micro-batching** (coalescing many small PTY frames in a ~5–10 ms window before
  writing, to cut packet counts during TUI redraw storms) — plausible, cheap, but unmeasured;
  TCP already coalesces somewhat. Parked pending the §8 measurements; do not build on spec.

---

## 7. Prioritized recommendations

Ranked by (bandwidth/battery impact) × (implementation cost), in the spirit of the integration
plan's phase scoring. Items 1–2 are **zero aplexer code**.

| # | What | Where | Impact | Cost |
| --- | --- | --- | --- | --- |
| 1 | **Enable SSH compression** (`Compression yes` / `zlib@openssh.com`) on PocketShell's SSH connections; document it in aplexer's README/remote docs for human users (`ssh -C`) | PocketShell + docs only | High: multi-x on all TUI/redraw traffic, the bulk of raw byte volume | ~zero |
| 2 | **PocketShell passes `--history-bytes <small>`** (and later `--lean`) on constrained links — the knob already exists end to end | PocketShell only | Medium: cheap reconnects | ~zero |
| 3 | **Dirty-checked status-bar redraws** (write only on change; slow the recompute after N unchanged ticks) | `src/bin/a.rs` | High for battery/idle chatter, benefits every user unconditionally | Small |
| 4 | **`--no-status`, `--lean` attach flags** + `$SSH_TTY` courtesy default; `--lean` implies small replay + slow status + (later) `--repaint` | `src/bin/a.rs` | Medium–high on constrained links | Small |
| 5 | **SIGWINCH repaint wiggle** (`--repaint`) for agent sessions: current screen from the workload instead of raw tail | worker + client flag | Medium: better *and* smaller reconnect picture | Small |
| 6 | PocketShell adaptive poll cadence + batching guidance (§5) | PocketShell only | High for battery | Small, client-side |
| 7 | **Framed attach with resume** (`--framed`, `since:{epoch,offset}`, additive protocol fields per §4) — the tmux-`-CC` analogue; subsumes status-bar elimination and byte-exact resume | worker + client + PocketShell | Medium bytes, high reconnect UX | Medium; build with integration-plan Phase B |
| 8 | `a watch --jsonl` replacing polling | per spec §19 / integration plan | High for battery, long term | Large; already planned elsewhere, not accelerated by this doc |

Suggested build order for aplexer itself: **3 → 4 → 5**, then 7 when Phase B starts. 1, 2, 6 are
documentation and PocketShell work that can happen immediately.

---

## 8. What to measure first (numbers this doc refuses to invent)

All byte/interval figures above marked "estimate" or "e.g." are starting points, not answers.
Before tuning defaults:

1. **Compression ratio of real agent-TUI output** under `ssh -C`: record 10 minutes of a live
   claude/codex session's PTY stream, replay through zlib with a streaming dictionary. Decides
   how much of the problem item #1 already solved.
2. **Idle attached-session wire cost**: bytes and packet count per hour over SSH with the status
   bar as-is, with dirty-checking, and with `--no-status`. Validates §2's ~0.5–1 MB/h estimate
   and the radio-wakeup claim.
3. **Reconnect cost end-to-end on a real cellular link**: time-to-usable-screen for
   `--history-bytes` ∈ {0 + repaint, 4 KB, 32 KB}, compressed and not. Picks `--lean`'s replay
   default.
4. **PocketShell reconnect frequency** in the field (drops per session-hour) — decides whether
   §4's resume mode is worth pulling forward or stays a Phase B nicety.
5. **Exec-channel wall time on the warm lease** at mobile RTTs — sets a floor for useful poll
   cadence in §5.
