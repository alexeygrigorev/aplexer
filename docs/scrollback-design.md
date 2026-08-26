# Scrollback: scrolling through a session's recent output without blocking input

Status: design, ready to implement. The recommendation is deliberately small:
**let the host terminal's native scrollback do the scrolling, and make aplexer
stop polluting it** (section 4). Almost all of the "implementation" is hygiene
fixes and verification, not new scroll machinery. All function/line references
are against commit `f46273b`.

## 1. Problem

The user wants to scroll up through a session's recent output "like in tmux" —
but explicitly **without** tmux's biggest copy-mode wart: tmux's `Ctrl-b [`
freezes the pane. While scrolled up in tmux, keystrokes drive the copy-mode
viewport instead of the program; you must exit copy-mode (`q`/Escape) before
you can type again. For agent sessions that is exactly wrong: while reviewing
what an agent did five minutes ago, the user still wants to interject an
instruction, answer a prompt, or hit Ctrl-C — without "coming back down"
first.

Hard requirement, therefore: **scrolling must never block or redirect input to
the live session.**

## 2. What "scrolling" can even mean here

aplexer has **no terminal emulator** — an explicit v1 non-goal (spec.md §17:
"V1 should not require implementing a full terminal emulator"; §27 lists
"copy mode" and "full terminal emulator state" as non-goals). There is no
screen grid anywhere in this codebase to scroll a viewport over. There are
exactly two stores of past output:

1. **The host terminal's own native scrollback.** `attach()`
   (src/bin/a.rs:1997) relays PTY bytes to the real terminal byte-for-byte on
   the **primary screen** (aplexer never enters the alternate screen for its
   own UI). Whatever scrolls off the top of the visible area is captured — or
   not — by the user's terminal emulator, entirely outside aplexer's control.
   Crucially, this is a real difference from tmux: tmux runs its client on
   the *alternate* screen, so pane output never reaches the host terminal's
   scrollback at all — which is *why* tmux needs copy-mode. aplexer's
   primary-screen relay means the host terminal's scrollback is already
   accumulating session output today.

2. **The server-side bounded byte ring.** `History` (src/lib.rs:939, a
   `VecDeque<u8>` capped at `history_bytes`, default `DEFAULT_HISTORY_BYTES`
   = 4MB) inside the worker's `OutputHub` (src/worker.rs:32). It is a raw
   byte log of everything the PTY ever emitted — ANSI escapes, cursor moves,
   colors, clears, all interleaved — with no line or screen structure.
   `Operation::Capture` / `a capture --bytes N` already snapshots its tail,
   and `Operation::Attach { history_bytes }` replays a tail on attach
   (32KB default, `DEFAULT_ATTACH_REPLAY_BYTES`, src/bin/a.rs:1766).

So the two candidate designs are:

- **(a)** use the host terminal's native scrollback; aplexer's only job is to
  stop actively corrupting it (section 3).
- **(b)** aplexer renders its own scrollback view from the `History` ring
  inside the attached screen (section 5).

## 3. Approach (a): native terminal scrollback

### 3.1 It already mostly works, and it is non-blocking by construction

Attach to a session, run something chatty, and scroll up with the terminal's
own gesture (wheel, Shift+PageUp, touchpad): the output that streamed while
attached is there, because it scrolled off the top of the primary screen and
the terminal saved it. And the hard requirement is satisfied **trivially**:
native scrollback is a *viewport* operation inside the terminal emulator.
The pty/stdin path is completely independent of where the viewport is —
typing while scrolled up delivers bytes to aplexer's input-forwarding thread
(src/bin/a.rs:2062) exactly as when live. Ctrl-C, answering a prompt,
interjecting an instruction: all work mid-scroll. No mode, no freeze, no
"exit scrollback first". This is not something we implement; it is how every
mainstream terminal already behaves.

One nuance, stated honestly: many terminals default to **scroll-on-keystroke**
(GNOME Terminal calls it exactly that), which snaps the viewport back to the
bottom when you type. The input is still delivered either way — the
non-blocking property always holds — but *whether the viewport stays put
while you type* is a host-terminal preference, not something aplexer can
control. Users who want to type while staring at history turn that setting
off in their terminal. Document this; do not try to outsmart it.

### 3.2 What interferes today: aplexer's own status-bar hygiene

This is where the real work is. `attach()` reserves the bottom row for a
status bar tmux-style: `apply_terminal_layout` (src/bin/a.rs:1834) sets a
DECSTBM scroll region `\x1b[1;rows-1r` excluding the last row, the server is
told the PTY is one row shorter (`reserved_rows`, :1808), and a status thread
periodically rewrites that row via absolute cursor addressing
(`draw_status_bar`, :1979). On a spec-compliant terminal the reserved row is
outside the scrolling region and its rewrites never scroll anything — so it
should contribute *nothing* to scrollback.

In practice this session's bug reports show **duplicate/corrupted status-bar
entries leaking into the host terminal's real scrollback**. A separate,
already-in-progress bug fix is addressing that; this design does not
re-litigate its root cause, but §4.2 pins down the exact invariants any such
fix must satisfy, and the checklist (§7) makes verifying them the bulk of the
work. Known leak vectors the fix must cover:

- **ED2 pushing the bar into scrollback on detach.** `reset_terminal`
  (src/bin/a.rs:1855) emits `\x1b[r\x1b[2J`. Several emulators implement
  "clear screen" by scrolling the current contents *into scrollback* rather
  than destroying them ("scroll-on-clear"). At that moment the bottom row
  still holds a full-width reverse-video status bar — so every detach can
  deposit one bar copy into scrollback. The in-process switch design
  (docs/fast-session-switching-design.md §5.2) adds another `\x1b[2J` per
  switch with the same exposure.
- **Redraws of unchanged content.** The status thread redraws at least every
  `STATUS_BAR_MAX_INTERVAL` (3s) even when nothing changed
  (src/bin/a.rs:2197). Combined with emulator-side region-scroll quirks
  (below), every redundant rewrite is another chance to smear a bar copy into
  history — and it's pure waste regardless. The dirty-check that suppresses
  byte-identical redraws is **already designed** in
  docs/low-bandwidth-remote-access-design.md §2.1 (for bandwidth reasons) and
  is adopted here as a scrollback-hygiene requirement too. As of `f46273b`
  it is not yet implemented on main or any sibling branch; whichever effort
  lands it first, the other references it — do not build it twice.
- **Resize races.** The resize poll (200ms, src/bin/a.rs:2142) means a short
  window where `TermGeom` is stale: a bar redraw can target a row that is,
  post-shrink, inside the (emulator-clamped) scroll region, and emulators
  that reflow on width change may reflow the bar row into scrollback. Both
  windows are bounded (one poll tick, then layout + redraw heal it); accept
  and document rather than chase — eliminating them entirely would need
  SIGWINCH-synchronous handling and still couldn't stop emulator-side reflow.

### 3.3 Emulator variance: the honest catch in approach (a)

Whether output scrolled out of a **top-anchored partial scroll region**
(`\x1b[1;rows-1r` — exactly what the status bar reservation creates) is saved
to scrollback is *emulator-dependent*. This is the one genuine weakness of
approach (a) and it must be verified, not assumed:

| Emulator family | Expected behavior with region `[1, rows-1]` | Confidence |
|---|---|---|
| VTE-based (GNOME Terminal, xfce4-terminal, …) | Saves scrolled lines to scrollback; historically the family with the notorious "status row duplicated into scrollback during region scrolls" artifact — very likely the observed bug | medium — verify |
| alacritty | Saves (region top at screen top rotates into history) | medium — verify |
| xterm | Commonly reported **not** to save while a bottom margin is set (scrollback freezes during attach) | low — verify |
| kitty | Reported to save only when the region covers the whole screen | low — verify |
| Windows Terminal, iTerm2, others | Unknown | verify |

Consequences, plainly:

- On save-with-margins emulators (likely the user's, given the observed
  pollution): once the hygiene fix lands, **scrolling up "like tmux" works
  with zero new aplexer code**, and non-blocking input comes free.
- On emulators that don't save margin-scrolled lines, attach means scrollback
  stops accumulating. The already-designed escape hatch is
  **`a attach --no-status`** (docs/low-bandwidth-remote-access-design.md
  §2.2): no bar, no DECSTBM reservation → all scrolling is full-screen
  scrolling → every emulator saves it. When that flag is implemented, it
  doubles as the universal-scrollback mode; the low-bandwidth doc should
  note this second motivation when it lands (checklist item 6).

### 3.4 What native scrollback does and doesn't contain

Be upfront in user docs about the semantics:

- **Only what streamed while this client was attached** (plus the 32KB replay
  tail written at attach). Output produced while detached lives only in the
  server-side 4MB `History` ring — reachable via `a capture --bytes N`
  (pipe it to `less -R` from any terminal), not by scrolling. This is the
  gap tmux copy-mode covers that (a) does not; §5 explains why closing it
  properly is a later-milestone project, and `a capture` is the v1 answer.
- **Re-attach duplicates the replay tail.** Every attach writes the 32KB tail
  into the primary screen again, so N attach cycles leave N copies of that
  tail in native scrollback. Accepted: it is bounded, honest (the terminal
  displayed those bytes), and tunable via `--history-bytes`.
- **Alternate-screen workloads contribute nothing** — a TUI that enters
  `\x1b[?1049h` (vim, htop, codex's TUI) redraws in place on the alt screen,
  which every emulator correctly excludes from scrollback. That is identical
  to running the TUI without aplexer, and no non-emulating design can do
  better: an in-place-redrawing TUI has no linear "history" to scroll, only
  app-internal state. (tmux only manages it because it *is* a full emulator.)
- **Mouse-mode workloads capture the wheel.** If the workload enables mouse
  reporting, wheel events go to the app instead of scrolling the viewport —
  again identical to running the app bare; Shift+wheel / Shift+PageUp bypass
  it in most emulators.

## 4. Approach (a) hardening: the actual design content

### 4.1 Sequences that are safe / required / forbidden for the reserved row

- **Reservation:** DECSTBM `\x1b[1;rows-1r`, wrapped in DEC save/restore
  cursor (`\x1b7`/`\x1b8`) — exactly what `apply_terminal_layout` already
  does. This is the correct, spec-compliant mechanism; no change.
- **Bar redraws:** absolute addressing `\x1b[rows;1H` + `\x1b[2K` + reverse
  video + padded text + `\x1b[0m`, wrapped in `\x1b7`/`\x1b8`, serialized
  under the stdout mutex — `draw_status_bar` as-is is byte-safe. The required
  change is *frequency*, not mechanism: suppress redraws whose rendered text
  and geometry are byte-identical to the last write (low-bandwidth §2.1).
- **Never write a wrapping line to the bottom row:** `pad_or_truncate` to
  exactly `cols` already guarantees no autowrap off the reserved row (writing
  exactly `cols` chars leaves the terminal in pending-wrap without scrolling).
  Preserve this property in any future bar change.
- **Erase the bar before any full clear.** New rule: every `\x1b[2J` the
  client emits while a bar may be on screen must be preceded by blanking the
  reserved row, so scroll-on-clear emulators push a blank line — not a
  reverse-video bar — into scrollback:
  `\x1b7\x1b[rows;1H\x1b[2K\x1b8` then `\x1b[r\x1b[2J\x1b[H\x1b[?25h`.
  Applies to `reset_terminal` (which therefore needs access to the current
  `TermGeom`; give `TerminalUiGuard` a `term: Arc<Mutex<TermGeom>>` field and
  pass it through) and to the switch-installation clear in
  fast-session-switching §5.2 (coordination note: whichever branch merges
  second folds this in — it is a 1-line prefix on an existing sequence).
- **Forbidden, aplexer-originated:** `\x1b[3J` (erases the user's saved
  lines — the exact asset this design exists to protect), RIS `\x1bc`
  (already deliberately avoided, see `reset_terminal`'s doc comment), and
  alternate-screen enter `\x1b[?1049h`/`\x1b[?47h` for aplexer's own UI
  (primary-screen relay is what makes native scrollback accumulate at all).

### 4.2 The relay itself: no workload-sequence filtering

Is scrollback pollution ever the *workload's* fault, and should the client
filter relayed bytes? **No filtering, by design.** Byte-preserving relay is a
spec contract (spec.md §16.6 "Output capture is byte-preserving"), filtering
would require exactly the escape-sequence parser v1 refuses to build, and a
workload emitting e.g. `\x1b[3J` (some `clear` implementations do) affects
the host terminal identically with or without aplexer in the middle. The
pollution problem is purely aplexer-status-bar hygiene; the relay is already
correct.

### 4.3 Keybinding: `Ctrl-b [` stays unbound (reserved)

The prefix state machine (today's `pending_ctrl_b` in src/bin/a.rs:2082,
becoming `InputScanner` per fast-session-switching §5.1) gets **no new
binding from this design**. Rationale, so nobody re-derives it:

- There is **no escape sequence that scrolls the host terminal's viewport**
  over its scrollback. Nothing in xterm's ctlseqs, and nothing portable in
  kitty/iTerm2 extensions, lets an application command "view history N lines
  up". `\x1b[S`/`\x1b[T` (SU/SD) scroll the *screen content within the
  region* — emitting them would corrupt the live display, not navigate
  history. So "`Ctrl-b [` scrolls up" cannot be implemented on top of (a);
  the terminal's own gestures (wheel, Shift+PageUp) are the interface, and
  they already work without any aplexer involvement.
- Binding `Ctrl-b [` to anything less (flashing a hint, dumping capture
  output into the live stream) is noise or garbage respectively.

Per the existing contract, an unbound `Ctrl-b [` simply forwards both bytes
to the workload (the "Ctrl-b is not a real prefix" rule, preserved by
`InputScanner`'s pending+anything-else branch). The chord is **reserved in
the dispatch table** for a future real copy-mode (section 5.3): the
fast-session-switching table's "`Ctrl-b` + anything else: forwards" row
implicitly covers it today; add `[` to that doc's reserved-for-future list
when convenient rather than duplicating the table here.

## 5. Approach (b): aplexer-rendered scrollback view — considered and rejected for v1

Recorded so a future implementer sees it was designed against, not ignored.

### 5.1 What it would take

The only substrate is the raw `History` byte ring. To show "what was on
screen N pages ago" from it, the client must at minimum:

- **Segment bytes into visual lines** — requires tracking CR-overwrites,
  autowrap at historical widths (which the ring does not record: bytes
  emitted at 120 cols render as garbage wrapped at 80), erase-line/scroll
  sequences, and cursor motion. Agent-CLI output is *dominated* by
  cursor-up + rewrite-region animation frames (spinners, streaming panes):
  a byte slice of it is not "old screen content", it is a fragment of an
  animation with no fixed screen meaning.
- **Avoid mid-sequence cuts and stale state** at the slice start — begin
  rendering only at a *safe resync point*: immediately after `\x1b[2J`,
  `\x1b[H\x1b[2J`, RIS, or an alt-screen exit found by scanning the ring.
  Tractable to scan for, but such anchors are workload-dependent and often
  sparse (a long-running agent may emit none for megabytes), so "scroll to
  arbitrary depth" degrades to "jump between a handful of clear-points",
  with SGR color state, hidden-cursor state, and wrap state still wrong
  after each anchor unless also tracked.
- **Share one physical screen with the still-live session** to satisfy
  non-blocking input: either freeze live rendering while viewing (you can
  type, but you type *blind* — echo invisible, which fails the requirement
  in spirit), or split the screen / overlay a popup — every variant requires
  knowing what is under/around the overlay to redraw it correctly against
  live output racing beneath, which is precisely the screen-state knowledge
  a non-emulating client does not have (the same reasoning as
  `STATUS_BAR_IDLE_GAP`'s doc comment, src/bin/a.rs:1768, where one reserved
  row is already acknowledged as the safe maximum). A persistent "scrolled —
  Ctrl-b ] returns to live" indicator on the reserved status row solves the
  ambiguity problem but none of the rendering ones.

### 5.2 Why rejected

Every sub-problem above is a slice of a terminal emulator. Building enough of
one to make (b) legible — grid, SGR machine, wrap model, per-width reflow —
is exactly what spec.md §17 defers ("Later, Aplexer may integrate a
terminal-state parser for: proper screen snapshots, … copy mode") and §27
excludes from v1. Meanwhile (a) delivers the user's actual ask — scroll up
through recent output while still typing to the live session — for the cost
of bug-fix hygiene, on the emulators the user runs. Shipping a byte-slicing
approximation of (b) would produce visibly corrupt views (stale colors,
animation fragments, wrong wrap) and permanently taint the feature's
reputation for no gain over `a capture | less -R`.

### 5.3 What v1 does instead, and the later path

- Gap "output from while I was detached": `a capture --bytes N | less -R`
  (any terminal, any time, non-blocking by definition — it doesn't even
  touch the attached client). Document it next to the scrollback note.
- The later real copy-mode: build on a terminal-state parser (spec §17
  "later"), rendered server-side per session, entered via the reserved
  `Ctrl-b [` — with aplexer's twist that input stays live and the status row
  carries the "viewing history" indicator. That is a design for the day the
  parser exists; nothing in this document forecloses it.

## 6. Recommendation

**Build (a). Do not build (b) in v1.** Concretely: verify/land the in-flight
status-bar scrollback fix against §4.1's invariants, add the ED2 bar-erase
rule, adopt the low-bandwidth doc's dirty-checked redraws, reserve
`Ctrl-b [`, and document the semantics (§3.4) honestly — including the
emulator-variance caveat and its `--no-status` escape hatch. The user's hard
requirement (never block input) is not merely met by (a); it is unbreakable
there, because scrolling and input never pass through the same code at all.

## 7. Implementation checklist

Work top to bottom. Items 1–3 are code; 4–6 are verification and docs.
Coordinate with the in-progress status-bar bug fix and the
fast-session-switching implementation — items 1 and 2 may already be
partially covered by them; verify against the invariants rather than
re-implementing blindly.

1. **Bar-erase before full clears** (src/bin/a.rs):
   - Give `TerminalUiGuard` a `term: Arc<Mutex<TermGeom>>` field;
     `reset_terminal(stdout, term)` reads the geometry and, when
     `reserved`, emits `\x1b7\x1b[{rows};1H\x1b[2K\x1b8` **before** the
     existing `\x1b[r\x1b[2J\x1b[H\x1b[?25h`.
   - If the fast-session-switching branch has merged: prepend the same
     erase to its per-switch `\x1b[2J\x1b[H\x1b[?25h` sequence (§5.2 of that
     doc). If it hasn't: leave a note in this doc's section 4.1 pointing at
     it (already present) and fold it in at merge time.
2. **Dirty-checked status-bar redraws**, exactly per
   docs/low-bandwidth-remote-access-design.md §2.1: keep the periodic
   *computation* (it is the change detector; it also drives `memory_indicator`
   freshness), suppress the *write* when rendered text and `TermGeom` are
   byte-identical to the last write; always write after `apply_terminal_layout`
   runs (geometry changed) and after the switch/flash paths dirty the row.
   Check `git log`/sibling branches first — if another agent landed it for
   the bandwidth reason, this item is already done.
3. **Grep-proof the forbidden sequences**: assert (code comment + a unit test
   over the client's emitted-sequence constants if practical) that no
   client-originated write contains `\x1b[3J`, `\x1bc`, or `\x1b[?1049h`.
   Today none does; the test is to keep it that way.
4. **Empirical emulator matrix** (manual, ~15 min per emulator; record
   results in this doc's §3.3 table, replacing the confidence column):
   for each of GNOME Terminal (VTE), xterm, kitty, alacritty (+ whatever the
   user actually runs): `a start --workspace /tmp/sb --tag t -- bash -l`,
   attach, `seq 1 5000`, then check (i) numbered lines accumulate in native
   scrollback, in order, no gaps; (ii) zero reverse-video bar copies
   interleaved, including after several detach/reattach cycles and window
   resizes; (iii) scroll up, type `echo hi<Enter>` — verify it executes
   (input never blocked) and note whether the viewport snapped
   (scroll-on-keystroke). Optional scripted proxy: run the same inside
   `kitty @ get-text --extent all` or a tmux pane with
   `tmux capture-pane -S -5000` — useful for regression automation, but each
   proxy measures only its own emulator's semantics; it does not replace the
   matrix.
5. **README**: add a short "Scrolling back" note under "First session":
   scroll with the terminal's own gesture (wheel / Shift+PageUp); it never
   blocks typing to the session; turn off the terminal's scroll-on-keystroke
   setting to type while staying scrolled; output from while you were
   detached is `a capture --bytes N | less -R`; alt-screen TUIs don't
   produce scrollback (same as running them bare). Plus the two-line design-
   doc pointer matching the existing pattern (done alongside this doc).
6. **Cross-doc notes**: when low-bandwidth §2.2's `--no-status` is
   implemented, document its second role — universal native scrollback on
   emulators that don't save margin-scrolled lines (§3.3). When
   fast-session-switching's keybinding table next gets touched, add
   `Ctrl-b [` to it as "reserved (future copy mode, see
   docs/scrollback-design.md §4.3); currently forwards like any unbound
   chord".
7. **Validate**: `cargo build --release --bins`, `cargo test`,
   `./scripts/validate.sh`; then item 4's matrix as the acceptance test —
   the feature is "done" when the matrix shows clean, bar-free scrollback
   plus live typing on the user's emulator, with zero new scroll machinery
   shipped.
