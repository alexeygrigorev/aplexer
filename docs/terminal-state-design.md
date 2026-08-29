# Server-side terminal state: correct reattach via a live screen model

Status: **implemented** (worker/protocol/client in `58fe950`, merged in
`0a34738`; erase-trigger and scrollback-invariant follow-ups in `e8dd853`
and `3a59d4e`). Checklist items 1-14 are done and verified end to end — see
section 12 for the per-item outcomes recorded against the shipped code. All
function/line references are against the design-time commit `961ebac` and
have drifted since; the module/function names are still accurate.
This is the aplexer equivalent of tmux's per-pane virtual
terminal: the worker feeds every PTY byte through a terminal-state parser
(the `vt100` crate) and keeps a live screen model, continuously, attached or
not; reattach renders the *current screen* to the new client instead of
replaying raw byte history.

**This proposal deliberately supersedes spec.md §17 ("V1 should not require
implementing a full terminal emulator") and the §27 non-goal "full terminal
emulator state" — see section 2 for the explicit case.** It does not touch
the other §27 non-goals (panes, copy mode, tmux compatibility, ...), and it
does not reverse docs/scrollback-design.md's conclusion (host-native
scrollback stays the scrolling mechanism; see §9.3).

## 1. Problem — reproduced, not hypothetical

`a attach` is a raw byte-passthrough client. The worker
(`src/worker.rs::spawn_pty_reader`, line 444 → `OutputHub::append`, line 46)
relays PTY bytes verbatim to whichever clients are attached and keeps a
capped raw-byte ring (`History`, src/lib.rs:939, inside `OutputHub`) whose
only reattach role is replaying a 32 KB tail
(`DEFAULT_ATTACH_REPLAY_BYTES`, src/bin/a.rs:1766) to a newly-attaching
client.

Directly reproduced failure: attach `a - codex`, let codex's full-screen TUI
render (bordered box, input prompt, status line), detach, reattach. The TUI
kept running live server-side the whole time (detach only disconnects the
client), but the replayed 32 KB tail leaves the host terminal's cursor at a
*stale historical* position unrelated to codex's live cursor, and the
client's own `[aplexer attached...]` banner (src/bin/a.rs:2035) lands
*inside codex's input box*, visibly overwriting "Ask Codex to do anything".
Related symptoms from the same root cause: aplexer's DECSTBM-reserved
status-bar row disappears when the workload does a full-screen redraw or
alt-screen switch (aplexer cannot see that the workload reset the host
terminal's assumptions), and reattach shows stale/wrong content generally.

The root cause is structural, not a tuning problem: a byte-history tail is a
fragment of an *animation*, not a description of a *screen*. It starts
mid-escape-sequence and mid-state (SGR colors, cursor visibility, alt-screen
mode, scroll margins all unknown at the cut point), and no replay length
fixes that — 4 MB replays the same wrongness for longer. The scrollback
design (docs/scrollback-design.md §5.1) already reached the same conclusion
for a different feature. The user's requirement is explicit: reattach must
work like tmux ("can we reimplement what tmux is doing for that? it works
fine"), and this is a hard blocker for PocketShell, where reattach happens
constantly (app backgrounding, network drops, tower handoffs) — a reattach
that corrupts a live agent session's display is not shippable there.

### 1.1 What tmux actually does

tmux never replays byte history on reattach. Its server maintains a full
virtual terminal per pane — cell grid, attributes/colors, cursor, scroll
regions, alternate screen — continuously updated by feeding all PTY output
through its own parser, whether or not a client is attached. Reattach is
"render the current grid to the new client": correct and instant by
construction, because there is no ambiguity about what the screen looks
like. shpool (Google's tmux-lite) does exactly the same with a Rust vt100
crate: "shpool continually maintains an in-memory render of the terminal
state via the shpool_vt100 crate; on reattach, shpool uses this in-memory
render to re-draw the screen." This design gives aplexer the same property.

## 2. Superseding spec.md §17 / §27 — the explicit case

The non-goal was written for two reasons, both stated in §17: (a) do not
*hand-roll* a terminal emulator, and (b) do not *block the core runtime* on
one. Both premises are now resolved rather than violated:

- The core runtime exists and works; nothing here blocks Milestones 0–6.
- No emulator is hand-rolled: the model is a mature, 3-small-dependency
  crate (`vt100`, 10.3M downloads) whose entire purpose is this use case
  (§3). The integration is ~a few hundred lines of worker/client glue.
- §17 itself already anticipated exactly this step: "Later, Aplexer may
  integrate a terminal-state parser for: proper screen snapshots,
  alternate-screen semantics, copy mode, richer PocketShell previews."
  This design is that "later", pulled forward because the replacement
  strategy §17 prescribed (bounded byte history replayed on attach) has now
  been *reproduced corrupting live agent sessions* — the project's primary
  workload — and because spec §22 makes PocketShell the primary client and
  PocketShell's usage pattern (constant reattach) hits this bug hardest.

What is *not* being adopted from the crossed-out non-goal: panes, windows,
layouts, copy mode, tmux compatibility. The model is one screen per session,
same as one PTY per session. spec.md itself stays unedited (it is the
project's own document); if this design is adopted, treat this file as the
authoritative amendment to §17 and to the single §27 bullet "full terminal
emulator state".

## 3. Crate decision: `vt100` 0.16.2

Requirements for the model crate: feed it raw PTY bytes incrementally;
track grid + attributes + cursor + alternate screen + scroll regions +
input modes; **render the current state back out as a byte stream** a dumb
terminal can consume (this is the make-or-break feature — a grid you cannot
serialize back to escape sequences means writing that serializer yourself,
which is the hardest part); handle resize; small dependency footprint
(aplexer currently has 7 direct deps, no async runtime).

### 3.1 The pick, verified against the actual 0.16.2 source

[`vt100`](https://github.com/doy/vt100-rust) 0.16.2 (MIT, 10.3M downloads,
last release 2025-07; deps: `vte` 0.15 + `itoa` + `unicode-width` — no
async, no transitive tree to speak of). Its README states the use case
verbatim: "programs that want to run other terminal programs, like screen
or tmux". API shape, confirmed by reading the source (not just docs):

- `Parser::new(rows, cols, scrollback_len)`; `parser.process(&[u8])` feeds
  raw bytes (vte-based, robust to arbitrary/binary input, resumable across
  chunk boundaries — exactly what a PTY read loop produces).
- `parser.screen() -> &Screen` — the live model. `Screen::set_size(rows,
  cols)` resizes with content preservation and margin clamping
  (src/grid.rs:56-87 in the crate).
- **`Screen::state_formatted() -> Vec<u8>`** — "escape codes sufficient to
  reproduce the entire contents of the current terminal state": clear
  attrs, ED2 clear, every visible row with inline SGR, final cursor
  position (including pending-wrap state), cursor visibility, and the
  input modes: application keypad, application cursor, **bracketed
  paste**, **mouse protocol mode + encoding** (`write_contents_formatted`
  + `write_input_mode_formatted`, crate src/screen.rs:224-416). The input
  modes matter enormously for agent TUIs: codex/claude enable bracketed
  paste, and restoring it on reattach is invisible state no byte replay
  ever gets right (symptom: multi-line paste executes line-by-line).
- `Screen::alternate_screen() -> bool`, `cursor_position()`, `contents()`
  (plain text — feeds `a capture --screen`, §8), `contents_diff(prev)` /
  `state_diff(prev)` (minimal update streams — the future framed/diff
  attach mode for PocketShell, §10.2).
- Tracks DECSTBM scroll regions and DECOM origin mode correctly during
  *parsing* (crate src/perform.rs:128 `'r' => self.screen.decstbm(...)`,
  grid scroll_top/scroll_bottom/origin_mode) — so grid contents are right
  even for margin-using workloads.

Two verified gaps, both compensable (§5.4, §6.2):

1. `state_formatted()` renders only the **active** grid and does not emit
   the alt-screen switch itself. Compensation: `alternate_screen()` is
   exposed; the worker prefixes `\x1b[?1049h` itself when set. (The
   inactive primary grid is not publicly accessible; consequence and
   upstream-PR path in §11.)
2. It does not re-emit DECSTBM margins in the snapshot, and does not expose
   the current margins as a getter. Compensation: a ~50-line worker-side
   `MarginTracker` state machine over the same byte stream (§5.4), which
   the status-bar fix (§7) needs anyway. Upstream PR (a trivial
   `Screen::scroll_region()` getter — the grid already has the fields) is
   filed in parallel and deletes half the tracker when merged.

### 3.2 Alternatives, actually evaluated

- **`avt` 0.18.0** (asciinema's virtual terminal; Apache-2.0; very active,
  last release 2026-05) — the serious runner-up. Its `Vt::dump()` is
  *more* complete than `state_formatted()`: it reconstructs **both**
  screen buffers, DECSTBM margins, origin mode, tab stops, charsets
  (DEC drawing), saved cursor contexts, insert/new-line modes (verified in
  its src/terminal.rs:1327-1592) — asciinema uses it to sync late joiners
  of live streams, which is literally reattach. Two disqualifiers for
  aplexer: (a) it **does not track bracketed paste or mouse protocol
  modes at all** (its `DecMode` enum stops at 1047/1048/1049) — so a
  reattach would silently break paste/mouse in agent TUIs, the exact
  workload aplexer exists for, and that gap cannot be compensated without
  tracking those modes ourselves; (b) its feed API is `feed_str(&str)` —
  PTY output is bytes, so a streaming UTF-8 decode shim with carryover
  would be needed. vt100's gaps are compensable in the worker; avt's gap
  (a) is not. If vt100 ever dies upstream, avt is the migration target
  (and gap (a) an upstream contribution).
- **`alacritty_terminal`** — excellent grid, but no state→escape-sequence
  serializer (alacritty renders to a GPU, not to another terminal); we
  would write `state_formatted` ourselves, i.e. the hard part. Heavier
  deps. Rejected.
- **`wezterm-term`** (via the `tattoy-wezterm-term` republish) — full
  featured (images, hyperlinks) but a large dependency tree and, again, no
  built-in "serialize current screen back to bytes". Rejected.
- **`shpool_vt100`** 0.1.3 — Google's fork of vt100 0.15, made while
  upstream was dormant. Upstream is active again (0.16.x, 2025) and ahead
  of the fork; use upstream, keep the fork in mind as precedent that this
  exact crate family carries this exact feature in production.
- **Hand-rolling** — rejected for the same reasons scrollback-design §5
  rejected byte-slicing: every sub-problem is a slice of a terminal
  emulator, and a 10.3M-download crate already exists.

### 3.3 Measured, not estimated

Measured with vt100 0.16.2 (release build, ordinary dev box) on a
synthesized codex-like screen (80×24, alt screen, bordered box, colored
status/prompt lines, bracketed paste on):

| Quantity | Measured |
|---|---|
| Snapshot of codex-like 80×24 TUI (`state_formatted`) | **2,482 bytes** (vs the fixed 32,768-byte replay today: 13× smaller — and correct) |
| Snapshot of a plain shell screen | **119 bytes** (275× smaller) |
| Pathological worst case, 200×50 with a different color on every cell | 112,914 bytes (bounded by one screen; still finite and correct, unlike any replay) |
| `state_formatted()` render time | ~37 µs |
| Parse throughput, escape-heavy TUI stream | ~57 MB/s on one core |
| Round-trip check (feed snapshot to a fresh parser, compare `contents()` + cursor) | equal — this property is the unit-test strategy (§12 item 10) |

Cell storage is 32 bytes (`const _: () = assert!(size_of::<Cell>() == 32)`
in the crate): two grids (primary + alternate) cost ≈123 KB at 80×24,
≈640 KB at 200×50, per worker process, with `scrollback_len = 0` (§5.2).

## 4. Architecture: the model lives in the worker

The worker is the only place that (a) sees every PTY byte from birth
(`spawn_pty_reader` → `OutputHub::append`), (b) survives client
disconnects, and (c) already serializes output consumers behind one lock
(`HubInner`). A client-side model would miss all output produced while
detached — the whole point. So:

```text
workload → PTY master → spawn_pty_reader (32 KB reads)
                             │
                             ▼
                   OutputHub::append(&data)      [one HubInner mutex]
                     ├── history.append(data)         (raw ring — kept, §8)
                     ├── screen.process(data)         (NEW: vt100 parser)
                     ├── margins.scan(data)           (NEW: MarginTracker, §5.4)
                     ├── layout-change detection      (NEW: §7)
                     └── fan out Data/Layout events to subscribers
```

Feeding the parser under the same `HubInner` mutex that guards `subscribe`
is what makes attach exact: `OutputHub::subscribe` (src/worker.rs:64)
already renders the replay *and* registers the subscriber under one lock
hold, so the snapshot plus subsequent Data frames are gap-free and
overlap-free by construction. The screen model inherits that guarantee for
free — no torn frames, no missed bytes, no double-painted bytes.

The model runs **always**, attached or not. The "only parse while attached"
optimization was considered and rejected: it saves ~0.2% of a core (§9) at
the cost of the primary case — attaching to a session that has been running
detached (or whose client dropped). That trade is upside-down.

## 5. Worker changes (src/worker.rs, src/lib.rs)

### 5.1 `ScreenTracker`

New struct (new file `src/screen.rs` or inside worker.rs):

```rust
pub struct ScreenTracker {
    parser: vt100::Parser,     // Parser::new(rows, cols, 0)
    margins: MarginTracker,    // §5.4
    alt_screen: bool,          // last observed, for flip detection
}

pub struct LayoutChange { pub alt_screen: bool, pub margins_reset: bool }

impl ScreenTracker {
    fn new(rows: u16, cols: u16) -> Self;
    /// Feed PTY bytes; returns Some(LayoutChange) when the workload did
    /// something the attached client must react to (§7).
    fn process(&mut self, data: &[u8]) -> Option<LayoutChange>;
    fn set_size(&mut self, rows: u16, cols: u16);   // parser + margins reset
    /// §6.2's full snapshot: 1049-prefix + state_formatted + margin suffix.
    fn snapshot(&self) -> Vec<u8>;
    /// Plain text of the current screen, for `a capture --screen` (§8).
    fn contents(&self) -> String;
}
```

`HubInner` (src/worker.rs:25) gains `screen: ScreenTracker`.
`OutputHub::new` (line 36) takes the initial (rows, cols) — the same
`initial_size.unwrap_or((24, 80))` `run_worker` (line 265) already computes
for `open_pty`; pass it through.

`OutputHub::append` (line 46) becomes: `history.append(data)?;` then
`let layout = inner.screen.process(data);` then fan out
`OutputEvent::Data(...)` as today, followed by
`OutputEvent::Layout(change)` when `layout` is `Some` (new `OutputEvent`
variant). Ordering matters and is automatic: the Layout event travels the
same per-subscriber mpsc channel and the same socket as the Data frame that
caused it, so the client always sees the bytes before the reaction cue.

### 5.2 Scrollback stays out of the model

`Parser::new(rows, cols, 0)` — zero model scrollback. Scrolling is the host
terminal's job per docs/scrollback-design.md; the raw `History` ring keeps
serving `a capture` (§8). This also caps model memory at the two-grid cost
(§3.3).

### 5.3 Resize

`WorkerRuntime::resize` (src/worker.rs:157) additionally calls a new
`self.output.set_size(rows, cols)` (locks `HubInner`, calls
`ScreenTracker::set_size`) **before** the `set_winsize` ioctl. Output
already in flight when a resize lands is parsed at the new size — a
transient tmux shares; the workload's SIGWINCH-triggered repaint heals it
within one frame. ~~`MarginTracker` resets to full-screen margins on resize
(xterm resets margins on resize; matching that is the least-surprise
approximation).~~

**Correction, from implementation (this sentence was wrong).** Resetting
margins on resize makes `MarginTracker` disagree with the `vt100` grid
standing next to it, and `snapshot()` pairs the grid's `state_formatted()`
with *the tracker's* margins — so the two must agree or the snapshot lies.
Measured against vt100 0.16.2 (`grid.rs::set_size`, lines 66–99),
`Screen::set_size` **keeps** the scroll region and re-fits it with three
rules, in this order:

1. a **bottom-anchored** region — one whose bottom edge sits on the old
   screen's last row — follows the screen, in *both* directions: `(3,23)` at
   23 rows becomes `(3,39)` at 39 rows;
2. a bottom past the new end is clamped to it, top preserved: `(5,23)` at 20
   rows becomes `(5,20)`;
3. a top that no longer fits below the clamped bottom degenerates to
   full-screen: `(21,23)` at 10 rows.

A region that still fits is untouched: `(5,15)` at 23 rows stays `(5,15)`.
`MarginTracker::set_rows` follows all three exactly, with no exemption: after
any sequence of resizes the region it *tracks* is what the real grid holds.
Checked against the real crate by
`margin_tracker_resize_matches_real_vt100_set_size` (case-by-case),
`margin_tracker_resize_divergence_from_vt100_is_only_the_degenerate_row`
(an exhaustive sweep of every sub-range at 2–24 rows resized to 1–30) and
`margin_tracker_tracked_region_matches_vt100_across_two_resizes` (every
sub-range at 2–14 rows through two consecutive resizes to 1–18, 147,420
cases), all reading vt100's region back out through DECOM rather than
assuming it.

**Second correction, from review (the paragraph this replaces was wrong).**
That paragraph said the tracker had one deliberate divergence — when a clamp
collapses the region onto a single row (`top == bottom`, only reachable as
`top == bottom == rows`), vt100 keeps the degenerate region and the tracker
dropped it to full-screen — and called it inconsequential *by construction*,
because "a one-row region is not expressible as a DECSTBM at all and
`snapshot()` could not re-emit it". The premise is true; the conclusion does
not follow. It reasons only about what can be emitted at that instant and
says nothing about what the region becomes at the *next* resize — and a
collapsed region is bottom-anchored by construction (`bottom == rows`), so
rule 1 grows it back with the screen. Dropping it made the loss **sticky**:
`\x1b[8;9r` at 20 rows, shrunk to 8 and re-grown to 24, is `(8,24)` in the
grid and was full-screen in the tracker, permanently. A two-step sweep found
4,823 of 147,420 resize pairs reporting a region that disagreed with vt100
about a perfectly ordinary, expressible sub-range. Real-world severity was
low — most TUIs re-emit DECSTBM on SIGWINCH, which heals it — but the
reasoning was wrong, not merely conservative, and this is the second time in
this effort that a comment overclaimed a limitation as inconsequential.

The tracker therefore keeps the degenerate region as `Some((rows, rows))` and
filters it out at the single emission-facing accessor,
`MarginTracker::margins()`, whose `None` both emission sites
(`ScreenTracker::snapshot` and `draw_status_bar`) already read as "no
sub-range, leave the client's own reservation in force". The only remaining
difference from vt100 anywhere is therefore what is *reported* while the
region is degenerate — an emission decision rather than lost state, and no
longer sticky. Pinned by
`margin_tracker_regrows_a_region_a_shrink_collapsed_onto_one_row` (the case
above, differentially against the real crate) plus the two sweeps, which
assert the tracked region with no exemption and the reported one with exactly
that emission exemption.

Rule 1 was missed on the first pass. It matters for the most ordinary TUI
layout there is — fixed header rows, everything below scrolls — whose region
*is* bottom-anchored, so any enlargement (window maximize, on-screen
keyboard hiding, a pane unsplit) made the tracker and the grid disagree and
put a stale region into the next snapshot. Covered behaviourally by
`round_trip_preserves_a_bottom_anchored_region_when_the_screen_grows`.

The xterm rule looked harmless because it seemed to apply only on a real
terminal resize, where the workload repaints anyway. It does not: **every
attach resizes the PTY by one row** to reserve the status-bar row (§6.3
step 1), so with the reset in place a workload holding a scroll region lost
it from the snapshot on *every single attach*, while the workload itself
kept line-feeding at the bottom of the region it still believed in. The host
terminal then walked the cursor past the region and painted over the fixed
rows below it. Reproduced end to end and covered by
`round_trip_preserves_scroll_region_across_resize`, whose oracle has to be
behavioural — immediately after the resize the live and restored screens are
identical, and they only diverge once the workload next scrolls.

### 5.4 `MarginTracker` — the one thing vt100 doesn't expose

A ~50-line byte state machine (Ground → Esc → Csi{param_buf, private,
intermediate}), persistent across chunks (so sequences split between PTY
reads are handled by construction), recognizing only:

- `ESC c` (RIS): margins ← full, report `margins_reset`.
- `ESC [ params r` with **no** private markers (`?`/`<`/`=`/`>`) and no
  intermediates: parse `top;bottom` (1-based, validated `top < bottom ≤
  rows`); empty params or full-range ⇒ margins ← full + report
  `margins_reset`; a proper sub-range ⇒ store it (used by `snapshot()`,
  §6.2) — no reset report, because a sub-range of the workload's rows
  cannot cover the client's reserved bar row (§7).
- Param buffer capped (32 bytes; overflow ⇒ discard sequence unparsed).

Everything else passes through untouched — this is a recognizer for two
sequences, not a second emulator. When the upstream
`Screen::scroll_region()` getter PR lands, the margin *values* come from
vt100 and the tracker shrinks to reset-event detection (or disappears, if
flips are then detected by comparing the getter before/after `process`).

### 5.5 Exit path

`OutputHub::finish` (line 82) additionally writes the final plain-text
screen (`ScreenTracker::contents()`) to
`state_session/<id>/screen.txt` alongside the history flush — a cheap
post-mortem "what was on screen when it died" that `a capture --screen`
can fall back to for dead sessions, mirroring `cmd_capture`'s existing
history-file fallback (src/bin/a.rs:1006). The live grid itself dies with
the worker; the raw history file remains the durable byte artifact.

## 6. Attach: snapshot instead of replay

### 6.1 Protocol (additive; graceful both ways across worker/client skew)

`Operation::Attach` (src/lib.rs:877) grows three optional serde fields:

```rust
Attach {
    history_bytes: Option<usize>,          // unchanged meaning (raw-tail size)
    #[serde(default)] want_screen: bool,   // NEW: prefer the screen snapshot
    #[serde(default)] rows: Option<u16>,   // NEW: client geometry (reserved_rows applied)
    #[serde(default)] cols: Option<u16>,
}
```

Compatibility matrix — this matters because aplexer is daemonless and old
*workers* keep running across CLI upgrades:

| | old worker | new worker |
|---|---|---|
| **old client** (sends `history_bytes: Some(32768)`) | today's behavior | `want_screen` defaults false → raw-tail replay, today's behavior |
| **new client** (sends `history_bytes: Some(32768)`, `want_screen: true`, rows/cols) | serde ignores unknown fields → 32 KB replay (today's behavior, no worse) | snapshot |

`a attach --history-bytes N` becomes the explicit escape hatch: the client
sets `want_screen: false` and the old raw-tail semantics apply (useful for
"seed my native scrollback with the tail", scrollback-design §3.4, and for
scripted byte-exact consumers). The attach `Response` gains
`"screen": true|false` so the client knows which it got.

`handle_attach` (src/worker.rs:674): if `rows`/`cols` are present, call
`runtime.resize(rows, cols)` **and** `output.set_size` *before*
subscribing, so the snapshot is rendered at the client's real geometry (no
wrong-size frame followed by a SIGWINCH repaint). Then
`subscribe(SnapshotMode::Screen | SnapshotMode::Tail(n))` renders the
payload under the hub lock and returns it as the existing Data frame — the
frame protocol does not change shape at all.

### 6.2 Snapshot composition (worker side, `ScreenTracker::snapshot`)

In order:

1. `\x1b[?1049h` — only if `screen.alternate_screen()`. This puts the
   *host* terminal genuinely on the alt screen, so the workload's eventual
   live `\x1b[?1049l` restores the host's primary screen instead of being
   a confusing no-op. (What the host's primary screen holds at that moment
   is whatever pre-attach content the user's terminal had — vt100 cannot
   render the inactive primary grid; accepted v1 approximation, §11.)
2. `screen.state_formatted()` — clear + full active-grid repaint + cursor
   position/visibility + input modes (bracketed paste, mouse, application
   keypad/cursor). This is the bulk (measured sizes in §3.3).
3. If `MarginTracker` holds non-default margins: `\x1b[{top};{bottom}r`
   followed by `\x1b[{row+1};{col+1}H` re-fixing the cursor from
   `screen.cursor_position()` (DECSTBM homes the cursor as a side effect).
   Skipped when margins are default — the common case — leaving the
   client's own reservation region (§7) in force.

   Two implementation notes, each of which independently defeated step 3
   until fixed (see §5.3's correction and §7's): the tracker must still
   *hold* the region at snapshot time (it did not, across the resize every
   attach performs), and the client must not overwrite the restored region
   afterwards (its status bar did, on the very next redraw).

Property: feeding the snapshot to a fresh, same-sized virtual terminal
must reproduce `contents()`, `cursor_position()`, `alternate_screen()`,
and the input-mode flags of the live one. That is directly testable with a
second `vt100::Parser` and is the core unit test (§12 item 10), already
verified to hold for the composition above (§3.3, round-trip row).

### 6.3 Client changes (`attach()`, src/bin/a.rs:1997)

Order of operations changes — today the initial payload is written *before*
raw mode and layout (lines 2012-2017), which is exactly how the banner
ended up inside codex's input box. New order for a tty:

1. Connect; send `Attach { history_bytes: Some(DEFAULT_ATTACH_REPLAY_BYTES),
   want_screen: !explicit_history, rows: Some(reserved_rows(rows)),
   cols: Some(cols) }` (geometry read up front; it already is for `a start
   --attach`). Read the response + snapshot Data frame.
2. Enter `RawMode`, construct `TerminalUiGuard`.
3. `apply_terminal_layout` (src/bin/a.rs:1834) — the DECSTBM reservation is
   asserted *first*, so a workload sub-range margin from snapshot step 3
   (numerically within rows 1..rows−1, since the workload PTY is one row
   short) lands after and wins, while a default-margin workload leaves the
   reservation standing.
4. Write the snapshot (one `write_locked`).
5. `draw_status_bar` immediately (the snapshot's ED2 blanked the bar row).
6. No `eprintln!` banner into the stream — the attach notice becomes a
   status-bar flash (the same flash slot fast-session-switching §6.1
   defines). Until the flash mechanism exists, printing the banner *before*
   step 4 is acceptable: the snapshot repaints over it, which is exactly
   the fix for the reproduced corruption.
7. The explicit post-connect `Resize` control send (line 2051-2057) becomes
   unnecessary when the Attach carried geometry; keep it only for the
   old-worker fallback path (response lacked `"screen"`).

`reset_terminal` (src/bin/a.rs:1855) prepends `\x1b[?1049l` to its
sequence: if the session was on the alt screen (whether entered by the
snapshot or live passthrough), detach must return the host to the primary
screen — today detaching from a live full-screen TUI leaves the host
terminal stuck on the alt screen, a pre-existing latent bug this fixes in
passing. On a host already on the primary screen it is a no-op. This does
not conflict with scrollback-design §4.1's "no alt-screen for aplexer's own
UI" rule: aplexer still never *enters* the alt screen for itself; entering
it to reproduce workload state (6.2 step 1) is the workload's own live
behavior, faithfully restored.

New `ServerEvent::Layout { alt_screen: bool, margins_reset: bool }`
(src/lib.rs:921): sent **only to subscribers that attached with
`want_screen: true`** — old clients' `serde_json::from_slice::<ServerEvent>`
would hard-fail on an unknown tag, so gating on the request flag keeps them
safe. Client handling: `apply_terminal_layout` (re-assert the reservation)
+ `draw_status_bar`. See §7.

`handle_attach`'s writer thread maps `OutputEvent::Layout` to that JSON
frame (or drops it for non-`want_screen` subscribers).

## 7. The status bar: what this fixes, what it doesn't

Today the bar's DECSTBM reservation is blind: the client cannot see the
workload reset the host's margins (`\x1b[r`, RIS) or flip screens, so the
bar's survival depends on timers (`STATUS_BAR_IDLE_GAP`, whose own doc
comment at src/bin/a.rs:1768 names the missing terminal-state tracking as
the reason it exists). With the worker parsing every byte, the triggers
become *observed events* instead of guesses:

- **Workload emits `\x1b[r` or RIS** — the sequences that widen the host's
  scroll region back onto the bar row. `MarginTracker` reports
  `margins_reset`; client re-asserts the reservation and redraws the bar,
  within one socket round-trip of the bytes that caused it.

  **What "re-asserts the reservation" has to mean, from implementation.**
  Not, as the client originally did, writing `\x1b[1;{rows-1}r`
  unconditionally on every bar redraw. That very sequence is what a
  workload's own sub-range has to survive, and it did not: the bar destroyed
  the region within one redraw cycle, including the one the snapshot had
  just restored. So the client runs the same `MarginTracker` over the bytes
  it writes (including the snapshot, whose trailing DECSTBM is how it learns
  the region at attach time) and re-asserts whichever region is actually
  current — its own only when the workload is on full-screen margins. The
  scan costs ~1 GB/s, i.e. nothing next to the terminal write it
  accompanies.

  **Correction, measured.** An earlier version of this section (and the code
  comments that quoted it) justified that by claiming a workload sub-range
  "is numerically confined to rows 1..rows−1 and cannot touch the bar". That
  is false, and §7.1 below records what actually happens. The justification
  that survives is weaker but still decisive: re-asserting the workload's
  region is the *lesser* exposure. Clobbering it corrupts every frame a
  margin-using TUI draws; keeping it leaves one narrow, self-healing way for
  the bar row to be written over.
- **Alt-screen enter/exit** (`ScreenTracker` compares
  `alternate_screen()` across `process`) — margins are formally preserved
  across 1049 on xterm, but emulator variance exists and TUIs commonly
  wrap transitions in `\x1b[r`; the re-assert is idempotent and cheap, so
  fire it on every flip. This addresses the observed "bar disappears on
  alt-screen switch" directly.

What this does **not** fully fix, stated honestly: a workload ED2/`\x1b[2J`
clears the *entire host screen including the bar row* (ED ignores margins)
without resetting any margins — the bar blanks until the next redraw. The
existing idle-gap redraw (≤450 ms) plus a Layout-event-driven redraw on the
cases above shrinks the visible window, but only full client-side
composition eliminates it: the client rendering "workload grid + bar" as
one atomic frame from `state_diff` updates, i.e. the v2 sketched in §10.2.
That is deliberately out of this design's v1 — it changes the client from
a passthrough into a renderer and belongs with the PocketShell framed-attach
work. The interim defensive re-assert patch (§10.1 item b) covers the
residue meanwhile.

### 7.1 Known limitation: a sub-range does not protect the reserved row

Open, deliberately not fixed in v1. DECSTBM constrains scrolling *inside*
the region, not cursor motion outside it, so while the client is
re-asserting a workload's sub-range the reserved bottom row is exposed in a
way it is not under the client's own `1;{rows-1}`:

| host region in force | workload at row 23 emits `\n` | result |
| --- | --- | --- |
| `1;23` (client's own) | rows 1–23 scroll | cursor stays on row 23, row 24 untouched |
| `5;15` (workload's) | row 23 is outside the region, and for the host terminal row 24 is just the screen bottom | cursor lands on row 24 and writes there |

Measured against a real `vt100::Parser` at the client's geometry, and pinned
by `workload_line_feed_can_still_reach_the_reserved_row_under_a_sub_range`
in src/bin/a.rs. Reaching it needs the workload's cursor on its own last row
— which is *outside* its own region — plus a line feed; the bar text is
repainted on the next redraw (≤450 ms idle, ≤3 s forced), but the workload's
cursor is left one row below where its own screen model believes it is.

Not fixable by choosing a different region to re-assert: the two demands
(scroll the workload's rows correctly, and stop its cursor at row `rows-1`)
cannot both be expressed as one DECSTBM. Closing it means the client
emulating the workload's stream well enough to clamp cursor motion — the
same full client-side composition §10.2's v2 needs for the ED2 residue
above, and the same place that fix belongs.

## 8. What happens to `History` / `a capture`

**Kept, unchanged in contract.** The raw ring and its persisted file remain:

- `a capture` stays byte-preserving (spec §16.6) — the grid model cannot
  substitute: it holds *decoded cells*, not the bytes, and post-mortem
  byte-level debugging plus `a capture | less -R` (scrollback-design §5.3)
  need the real stream.
- The persisted history file remains the after-death artifact
  (`cmd_capture`'s fallback, src/bin/a.rs:1006).
- The attach *replay* role of `History` is what the snapshot replaces; the
  `--history-bytes` escape hatch (§6.1) keeps the old behavior reachable.

**Added:** `a capture --screen [--plain]` → new
`Operation::CaptureScreen { plain: bool }`: the snapshot bytes (paintable
form) or `ScreenTracker::contents()` (plain text, exactly what spec §17
called "richer PocketShell previews" — a PocketShell session card can show
the actual current screen text for a few hundred bytes). Dead-session
fallback: `screen.txt` (§5.5).

## 9. Performance

The user's constraint is explicit ("we need faster"). Numbers from §3.3:

- **Reattach payload**: 2.5 KB typical TUI / 119 B shell screen vs a fixed
  32 KB today — 13–275× less data, and it is *the* screen, not an
  approximation. Over PocketShell's cellular link (low-bandwidth doc §1.2:
  32 KB ≈ 260 ms at 1 Mbit/s before compression): ≈20 ms. Locally: the
  ~37 µs render + one small socket write — effectively instant; the host
  terminal's paint becomes the floor.
- **Steady-state parse cost**: ~57 MB/s/core on escape-heavy input. A busy
  agent session emits ~10–100 KB/s → ≈0.2% of a core; a pathological
  10 MB/s burst (`cat` of a huge file) → ~18% of one core for the burst's
  duration, on a thread that already exists and already copies every chunk
  per subscriber. No new wakeups, zero cost at idle (event-driven), no
  radio/battery implication for detached sessions.
- **Memory**: ~123 KB (80×24) to ~640 KB (200×50) per worker for the two
  grids. At spec §30's 200-session scale, worst case ~128 MB across 200
  *separate processes* — visible but acceptable; typical (80×24-ish)
  ~25 MB total.
- **What could regress**: only the parse cost above. Checklist item 12
  benchmarks the worker's PTY throughput before/after on the validate
  harness; the escape hatch if a real workload ever suffers is a
  per-session `screen_tracking = false` config knob (falls back to raw-tail
  attach), not lazy enablement (§4).

## 10. Relationship to in-flight and adjacent work

### 10.1 The patch branch being implemented right now

A sibling worktree is currently landing symptom-level patches. Triage once
this design is implemented:

- **(a) SIGWINCH-repaint on reconnect** — **superseded**: the snapshot *is*
  the current screen; asking the workload to repaint is redundant, strictly
  less correct (repaint-on-SIGWINCH is workload-cooperation-dependent; a
  bare shell doesn't), and slower (round-trip through the workload). Land
  it now anyway — it is small and delivers real interim relief — then
  delete it in this design's client/worker attach path (checklist item 14).
- **(b) Defensive DECSTBM margin re-assert** — **partially superseded**:
  the *blind periodic* re-assert is replaced by the event-driven
  Layout-triggered re-assert (§7), which is both prompter and quieter (no
  redundant writes for the low-bandwidth doc §2.1 to suppress). The
  re-assert *helper* itself (re-run `apply_terminal_layout` + redraw) is
  exactly what the Layout handler calls — keep the code, change the
  trigger. The ED2-blanking residue (§7) means a slow periodic fallback
  re-draw (the existing status thread) still earns its keep; don't remove
  that.
- **(c) Moving the attach banner out of the output stream** — **keep
  permanently**: printing diagnostics into the workload's screen area is
  wrong under any architecture (the snapshot would repaint over it, but
  between switch/flash paths the bar flash is the right home). This design
  assumes it (§6.3 step 6).

Sequencing: land the patch branch first (it is nearly done and independently
valuable), then implement this design against the merged result; items (a)
and blind-(b) are removed *by* this design's checklist, not by editing the
patch branch.

### 10.2 Other design docs

- **docs/low-bandwidth-remote-access-design.md**: §3 item 3 (`--repaint`
  SIGWINCH wiggle) — **superseded by this design**; do not build it. §3
  item 4 said a terminal-state parser is "not load-bearing here... do not
  build it for this" — correct then, moot now: it is being built for
  reattach *correctness*, and the bandwidth win (2.5 KB vs 32 KB) arrives
  free, better than the wiggle it replaces. §4's framed-attach/resume for
  PocketShell is untouched and gets a strict upgrade path: `state_diff`/
  `contents_diff` (§3.1) are the natural payloads for a future framed mode
  in which PocketShell holds a client-side `vt100::Parser` and receives
  diffs — that is also the full status-bar composition fix (§7) and the
  eventual real copy-mode substrate (scrollback-design §5.3). Design that
  when Phase B starts; nothing here forecloses it.
- **docs/scrollback-design.md**: conclusion unchanged (host-native
  scrollback; no aplexer-rendered scrollback view). Interactions: attach no
  longer seeds a 32 KB tail into native scrollback — which that doc listed
  as a wart ("N attach cycles leave N copies"); users who *want* tail
  seeding have `--history-bytes` (§6.1). Its §4.1 forbidden-sequence rule
  ("no alt-screen enter for aplexer's own UI") is respected per §6.3.
- **docs/fast-session-switching-design.md**: `establish()` uses the same
  `Operation::Attach`, so switches get snapshots automatically. Two
  coordination points: `establish` should pass the current `TermGeom`
  geometry in the Attach (making that doc's §5.2 explicit post-switch
  Resize unnecessary on new workers), and its `\x1b[2J\x1b[H\x1b[?25h`
  switch-installation clear becomes redundant (the snapshot begins with
  ED2) but is harmless — fold in whichever lands second.

## 11. Scope: v1 of this feature

**Must be correct** (all verified vt100-supported, §3.1):

- Cursor position + visibility, including pending-wrap.
- SGR: 16/256/RGB colors, bold, dim, italic, underline, inverse.
- Alternate screen (1047/1048/1049) — contents and mode restored.
- Scroll regions: grid contents always correct (vt100 parses DECSTBM);
  margin re-established on the host via `MarginTracker` (§6.2 step 3).
- Input modes: bracketed paste, mouse protocol mode+encoding, application
  keypad/cursor keys.
- Resize (worker-side model resize + geometry-carrying attach).
- Multi-client attach (snapshot is a stateless render; each subscriber
  gets its own, under the same lock discipline as today).

**Approximate / accepted v1 limitations** (each is strictly no worse than
today's byte replay, which gets all of them wrong *and* the screen too):

- Primary-screen contents behind an active alt screen are not restored on
  attach (vt100 exposes only the active grid) — on TUI exit the host shows
  its pre-attach primary content until the shell next paints. Upstream PR
  opportunity (expose the inactive grid), tracked, not blocking.
- DECOM origin-mode state is tracked for parsing but not re-established on
  the host after snapshot; workloads that keep DECOM enabled across
  frames are vanishingly rare.
- Window title (OSC 0/2): vt100 0.16 moved title to callbacks; live
  passthrough still forwards titles in real time, the snapshot just doesn't
  restore the current one. Wire the callback later if it itches.
- Custom tab stops, G0/G1 charset *state* (cell glyphs are already decoded
  to Unicode in the grid, so display is right; only a workload caught
  mid-drawing-mode emits wrong glyphs until it re-shifts), sixel/iTerm2
  images (unsupported by vt100; forwarded live, absent from snapshots).
- ED2-blanking of the status bar between redraws (§7) — fully fixed only
  by v2 composition.

**Deferred (explicitly not this design)**: client-side composition /
framed diff attach (§10.2), copy mode, model-side scrollback, panes.

## 12. Implementation checklist

Work top to bottom. Items 1–8 are worker/protocol; 9 client; 10–12 tests
and validation; 13–15 docs/coordination. Estimated new code: ~450 lines
plus tests.

1. **Dependency**: add `vt100 = "0.16"` to Cargo.toml. Verify MSRV against
   `rust-version = "1.78"` (`cargo +1.78 check`; vt100 0.16.1 deliberately
   reverted to edition 2021, but `vte` 0.15 must also pass — if it does
   not, either bump `rust-version` or pin `vt100 = "=0.15.2"` and add the
   then-missing pieces of §6.2 by hand; record the outcome here).

   **Outcome: no bump and no pin needed on account of this dependency.**
   `vt100` 0.16.2 and its whole subtree (`vte` 0.15.0, `itoa`,
   `unicode-width`, `arrayvec`, `bitflags`, `memchr`) compile cleanly under
   `cargo +1.78.0 check` in isolation; every crate in it declares an MSRV of
   1.70 or lower. Note, separately and *not* caused by this design:
   `cargo +1.78.0 check --all-targets` on the whole workspace now fails on
   `clap` 4.6.6 ("feature `edition2024` is required"), so aplexer's declared
   `rust-version = "1.78"` is already inaccurate for reasons unrelated to
   the screen model — worth fixing, but as its own dependency-hygiene task.
2. **`MarginTracker`** (new module, `src/screen.rs`): the §5.4 state
   machine. Pure function over bytes + internal state; unit-test with
   sequences split at every byte boundary (`"\x1b"`+`"[3;20r"`, RIS,
   private-marker `\x1b[?1049h` ignored, oversized params discarded,
   `\x1b[r` and `\x1b[1;24r` both reported as reset at 24 rows).
3. **`ScreenTracker`** (same module): §5.1 API. `process` = parser.process
   + margins.scan + alt-flip detection → `Option<LayoutChange>`;
   `snapshot()` = §6.2 composition; `contents()`; `set_size` (parser
   `Screen::set_size` + `MarginTracker::set_rows`, which re-fits the region
   the same way the grid does rather than resetting it — §5.3's correction).
4. **`HubInner`/`OutputHub`** (src/worker.rs:25-100): add
   `screen: ScreenTracker` (constructed with `run_worker`'s initial size —
   plumb `(rows, cols)` into `OutputHub::new`); feed it in `append` and
   fan out a new `OutputEvent::Layout(LayoutChange)`; add
   `set_size(rows, cols)`; extend `subscribe` to take an enum
   `AttachPayload { Screen, Tail(Option<usize>) }` and render the snapshot
   or the history tail under the same lock hold as subscriber
   registration; add `screen_contents()` for capture.
5. **Protocol** (src/lib.rs): `Operation::Attach` gains
   `want_screen`/`rows`/`cols` (serde defaults, §6.1);
   `Operation::CaptureScreen { plain: bool }`; `ServerEvent::Layout`;
   `OutputEvent::Layout` stays worker-internal.
6. **`handle_attach`** (src/worker.rs:674): apply geometry (resize PTY +
   model) before subscribing; choose `AttachPayload` from
   `want_screen`; include `"screen": bool` in the response; writer thread
   forwards `OutputEvent::Layout` as a `ServerEvent::Layout` JSON frame
   only for `want_screen` subscribers (drop otherwise — old-client
   safety, §6.3).
7. **`WorkerRuntime::resize`** (line 157): call `output.set_size` before
   the ioctl (§5.3).
8. **Exit path** (`OutputHub::finish`, line 82): write `screen.txt` (§5.5);
   `Operation::CaptureScreen` handler in `handle_connection`
   (src/worker.rs:614) mirroring `Operation::Capture`'s response+Data
   shape; `cmd_capture --screen` falls back to `screen.txt` for dead
   sessions.
9. **Client** (`attach()`, src/bin/a.rs:1997): reorder per §6.3 (geometry
   in the Attach request; raw mode + `apply_terminal_layout` before
   writing the snapshot; immediate `draw_status_bar`; banner → flash or
   pre-snapshot); handle `ServerEvent::Layout` in the frame loop
   (re-assert layout + redraw bar); `reset_terminal` prepends
   `\x1b[?1049l`; `--history-bytes` sets `want_screen: false`; keep the
   explicit post-connect Resize only when the response lacks
   `"screen"` (old worker).
10. **Unit tests** (worker side, no PTY needed): the round-trip property —
    for each scripted stream (plain shell scrollout; codex-like alt-screen
    TUI with colors + bracketed paste; DECSTBM sub-range workload; a
    stream ending mid-escape-sequence; post-resize), feed stream to
    tracker A, feed `A.snapshot()` to a fresh `vt100::Parser` B at the
    same size, assert equal `contents()`, `cursor_position()`,
    `alternate_screen()`, `bracketed_paste()`, `mouse_protocol_mode()`.
    Plus MarginTracker tests (item 2) and a Layout-event test (alt-screen
    enter emits exactly one `LayoutChange`).
11. **Integration test** (tests/, harness style of tests/oom_isolation.rs):
    start a session running a script that paints a box + moves the cursor
    into it + enters the alt screen; attach (non-tty is fine — the
    snapshot arrives as the first Data frame either way), assert the first
    Data frame starts with `\x1b[?1049h` and reproduces the box through a
    `vt100::Parser`; detach; print more; reattach and assert the *new*
    content is present and the frame is a fresh snapshot, not history
    bytes. Old-client compat: attach with `want_screen: false` and assert
    raw-tail bytes.

    **Extended, follow-up round.** The raw-socket attach above deliberately
    skips `a attach`'s tty machinery — which is also where every piece of
    client-side scroll-region handling lives (`isatty` gates all of it), so
    none of it was covered: the snapshot scan, the per-Data-frame scan, the
    resize thread's seeding, the switch reset and the switch's re-scan could
    each be deleted with the suite still green. `tests/screen_snapshot.rs`
    now also spawns the real `a attach` on a PTY (`aplexer::open_pty`),
    captures every byte it writes and replays them through a
    `vt100::Parser` standing in for the user's terminal, so the assertions
    are about what the terminal ends up holding — read back with DECOM, and
    cross-checked against the worker's own `a capture --screen --plain`.
    Four tests: the region survives the attach and the first second
    (`attach_keeps_the_workload_scroll_region_alive_on_the_host_terminal`),
    a region set live is picked up
    (`attach_learns_a_scroll_region_the_workload_sets_while_attached`), a
    session switch drops the old session's region and learns the new one's
    (`switching_sessions_drops_the_previous_sessions_scroll_region`), and a
    bottom-anchored region follows a terminal enlargement through the real
    resize path -- `TIOCSWINSZ` on the pty master, with the host model
    resized at the same point a real terminal would be
    (`growing_the_terminal_grows_a_bottom_anchored_workload_region`, §5.3
    rule 1). Each of the five client-side call sites was verified to be
    killed by at least one of them, by deleting it and re-running.
12. **Benchmark gate**: the existing throughput concern — run a
    high-volume workload (`seq 1 2000000` or `yes | head -c 100M`)
    through a session before/after the change and compare wall time; the
    §9 expectation is a low-single-digit-percent difference. If it exceeds
    ~10%, profile before shipping (the append lock hold is the suspect).

    **Outcome: gate passes.** Measured against an otherwise-identical
    worker with `inner.screen.process(data)` removed from `OutputHub::append`,
    with the two arms run as two concurrently-live sessions and reps strictly
    alternating between them (this de-trending matters — a naive
    block-A-then-block-B run on a loaded box produced a spurious +20%):

    | workload | tracking | no tracking | delta (paired median) |
    |---|---|---|---|
    | `seq 1 2000000` (~17 MB), n=20 | 1.060 s median | 1.058 s median | **-0.5%** |
    | `yes \| head -c 50000000` (~100 MB through the PTY), n=12 | 12.005 s median | 11.542 s median | **+4.0%** |

    Isolated parse cost on the same box (release, 24×80, 32 KB chunks):
    73.1 MB/s on the `seq` stream, 42.2 MB/s on the scroll-heavy `yes`
    stream, and `state_formatted()` in 15-24 µs producing 99-1791 bytes —
    all consistent with §3.3/§9. The `seq` arm shows no measurable delta
    because the PTY line discipline, not the worker's read loop, is that
    workload's bottleneck, so the parse fits in existing headroom; the
    scroll-heavy arm is where the model actually costs something, and it
    lands inside the §9 expectation. No profiling of the append lock hold
    was needed.
13. **README**: two-line design-doc pointer (done alongside this doc);
    update the attach section's reattach description ("reattach repaints
    the live screen, tmux-style").
14. **Patch-branch reconciliation** (§10.1): after this lands, remove the
    SIGWINCH-repaint-on-reconnect path (a) and retarget the defensive
    re-assert (b)'s trigger to the Layout event, keeping its helper and
    the slow periodic fallback; banner relocation (c) stays.

    **Outcome: done.** (a) No SIGWINCH-repaint-on-reconnect path exists in
    the merged result — the sibling worktree branches named in §10.1 are all
    ancestors of `main`, and neither it nor
    docs/low-bandwidth-remote-access-design.md §3's `--repaint` wiggle was
    ever built, so there was nothing to delete. (b) `ServerEvent::Layout` now
    drives the re-assert (`attach()`'s frame loop), while
    `STATUS_BAR_IDLE_GAP`/`STATUS_BAR_MAX_INTERVAL` stay as the slow
    fallback for the ED2-blanking residue §7 documents. (c) The banner is
    still an `eprintln!`, but is now emitted *before* the snapshot write, so
    the snapshot's ED2 repaints over it — §6.3 step 6's accepted interim
    until a status-bar flash slot exists.
15. **Upstream PRs** (parallel, non-blocking): vt100 `scroll_region()`
    getter (+ optionally inactive-grid access); shrink `MarginTracker`
    when merged. **Validate**: `cargo build --release --bins`,
    `cargo test`, `./scripts/validate.sh`, then the §1 codex repro by
    hand — attach, let the TUI render, detach, reattach: the box, prompt,
    and cursor must be exactly where codex thinks they are.

    **Outcome: validated.** `cargo build --release --bins` and `cargo test`
    are green: 70 `--lib` unit + 40 `--bin a` unit, and 7 `screen_snapshot`
    integration tests (plus the `#[ignore]`d `attach_round_trip_latency`
    perf measurement), all passing. Counts as of the closing round that added
    the two-step resize sweep and the sticky-collapse regression test
    (§5.3's second correction), on top of the round that added the vt100
    resize differential (§5.3), the PTY-driven client tests covering the
    snapshot/Data-frame/switch margin scans and the resize thread's seeding,
    and §7.1's reserved-row limitation.
    `./scripts/validate.sh` stops at its first step, `cargo fmt --all --
    --check`, and **does so on `main` too**: the repo has never been
    `cargo fmt`'d, so 85 hunks across 11 files are already unformatted before
    this work, under rustfmt 1.78 and rustfmt 1.9 alike (it is genuine drift,
    not a newer-toolchain style change). Left alone rather than reformatted,
    since a repo-wide `cargo fmt` is its own change and would bury this one;
    the code added here was hand-formatted so the count stays at exactly 85,
    i.e. this adds no new formatting debt. Every later step of `validate.sh`
    was run by hand and passes (`cargo check --all-targets`,
    `cargo test --all-targets`, `python3 -m compileall python`; `pytest` is
    not installed on this box, which `validate.sh` itself treats as a skip).

    The §1 codex repro was then run for real against `codex-cli` 0.150.1 in
    an 80×24 PTY: attach, let the TUI render, `Ctrl-b d`, reattach. On
    reattach the worker sent a **1,529-byte** snapshot (vs. the old fixed
    32 KB tail) that reproduced codex's bordered header box on rows 0-6, the
    `›` input prompt on row 13, the model/context status line on row 16, the
    cursor at (14, 2) — exactly where the live worker-side model had it —
    and `bracketed_paste=true` restored. No banner text anywhere on screen,
    no stale rows, no misplaced cursor. The alt-screen path (which codex
    0.150 does not exercise) was covered by the same repro against `vim`,
    where the snapshot correctly led with `\x1b[?1049h` and the host
    terminal came up on the alternate screen with the file rendered and the
    cursor at (0, 8). The upstream `scroll_region()` PR remains open work
    and is explicitly non-blocking.
