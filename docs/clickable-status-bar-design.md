# Clickable status bar: session switching and cross-workspace browsing by mouse

Status: design, opt-in v1 slice implementable. Function/line references are
against commit `0a34738`. All work is client-side (`src/bin/a.rs`); the one
optional protocol touch is additive and backward-compatible (section 5.3).

## 1. Problem

`a attach` reserves the terminal's bottom row for a persistent status bar
(`draw_status_bar`, `status_bar_text`, section 6.1 of
docs/fast-session-switching-design.md) that already lists every sibling
session in the current workspace, numbered exactly the way `Ctrl-b 1..9`
addresses them (`workspace_summary`, src/bin/a.rs:2552). Switching today is
keyboard-only (`Ctrl-b n/p/N/P/l/1-9`, `InputScanner::scan`,
src/bin/a.rs:2987). The user's ask: let a mouse click on a sibling's name in
the bar do what `Ctrl-b <N>` does, and give the bar a second clickable
region for reaching sessions in *other* workspaces, without needing to
detach and run `a attach` elsewhere.

Two sub-features, one shared mechanism (mouse reporting) with one hard
coexistence problem: the attached workload (vim, htop, an agent TUI) may
also want mouse events for itself, and the client cannot silently break
that.

## 2. Terminal mechanics: SGR mouse reporting

xterm-compatible terminals report clicks only after being told to via a
private-mode DECSET. Two independent switches matter here:

- `CSI ?1000h` / `CSI ?1000l` — basic click tracking (button press *and*
  release, no motion).
- `CSI ?1006h` / `CSI ?1006l` — SGR extended coordinate encoding. Without
  it, coordinates are encoded as `byte value = 32 + coordinate`, which
  breaks past column/row 223 and can collide with control bytes; with it,
  coordinates are plain decimal ASCII with no upper bound. Always paired
  with `?1000h` in this design; v1 needs no drag/hover, so `?1002h`
  (button-motion) is not enabled.

With both enabled, every press/release sends, on stdin:

```
ESC [ < Cb ; Cx ; Cy M      -- button press
ESC [ < Cb ; Cx ; Cy m      -- button release
```

`Cb` is the button code (0/1/2 = left/middle/right, plus modifier bits),
`Cx`/`Cy` are **1-based** column/row of the physical terminal. This project
already depends on the `vt100` crate for the worker-side screen model, and
`vt100::Screen::mouse_protocol_mode()` already exists and is exercised in
`src/screen.rs`'s round-trip tests (line 633) — the terminal-state design
(docs/terminal-state-design.md section 11) already lists "mouse protocol
mode+encoding" as an in-scope, verified-restorable piece of input-mode
state. No prior doc scopes the *client* enabling mouse mode for its own UI;
this doc is that missing half.

## 3. The hard problem: the workload wants mouse events too

If the client unconditionally turns on `?1000h`/`?1006h` for the real
terminal, **every** click — whether the user meant it for the bar or for
whatever is running inside the session — arrives on the client's stdin as
one of the sequences above. The client, not the terminal, must decide where
each click goes. Two failure directions to avoid:

- **Swallowing a click the workload wanted.** If vim has entered visual
  mode and enabled its own mouse tracking, a click meant to extend a visual
  selection must reach vim's stdin verbatim, untranslated, at the same
  coordinates — vim parses the SGR sequence itself.
- **Leaking mouse escape junk into a workload that never asked for it.** A
  plain shell prompt has never enabled mouse mode. If the client enables it
  anyway (for the bar) and then forwards an outside-the-bar click's raw
  bytes to the shell as if they were typed, the shell receives literal
  bytes `<`, digits, `;`, `M` and inserts them into the command line —
  visible, confusing corruption of a session the user never asked to be
  touched. Silently discarding a click is far less surprising.

### 3.1 Precedent: this is structurally the Ctrl-b problem

aplexer already has exactly this "recognize a locally-meaningful prefix or
otherwise get out of the way" problem for the keyboard: `Ctrl-b` is
provisionally consumed, and depending on the next byte either turns into a
local action (`d`/`n`/`p`/`N`/`P`/`l`/`1`-`9`) or — if the next byte isn't
bound — both bytes are reinjected into the forward stream untouched
(`InputScanner::scan`, src/bin/a.rs:2987-3044, and the design rationale in
docs/fast-session-switching-design.md section 6, "Newly consumed keys").
Mouse handling should follow the same shape: **recognize-or-forward-
verbatim**, never "recognize-or-translate" and never "recognize-or-drop"
except in the one case (section 3.2) where forwarding would actively
corrupt a workload that isn't listening for these bytes at all.

### 3.2 Resolution: ask the worker whether the workload wants mouse mode

The needed fact — "does the thing currently running in this PTY have mouse
reporting enabled right now" — already exists, continuously maintained,
inside the worker: `OutputHub`'s `ScreenTracker` (`src/worker.rs:41`) feeds
every PTY byte through `vt100::Parser`, and `Screen::mouse_protocol_mode()`
reflects whatever DECSET/DECRST sequences the workload itself has sent, in
real time. This is precisely the tracked bit the terminal-state design
already relies on to restore mouse mode correctly on reattach
(`ScreenTracker::snapshot` → `state_formatted()`, `src/screen.rs:342`) — it
just isn't exposed to the *client* yet.

Expose it the same low-ceremony way `foreground_command` and `cgroup`
already piggyback onto the existing per-redraw `Operation::Status` RPC
(`src/worker.rs:726-746`, `live_status`/`memory_indicator`,
src/bin/a.rs:2479-2519): merge one more field into the JSON response.

```rust
// src/worker.rs, inside the Operation::Status arm, alongside the existing
// cgroup/foreground_command merges:
if let Ok(hub) = /* however OutputHub is reached from WorkerRuntime */ {
    value["mouse_mode"] = json!(hub.mouse_mode_active()); // bool
}
```

```rust
// src/screen.rs, on ScreenTracker:
pub fn mouse_mode_active(&self) -> bool {
    !matches!(
        self.parser.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    )
}
```

This is additive JSON (old clients ignore the field, exactly like
`foreground_command` today) and read-only (no new control operation, no
change to the attach/detach handshake). The client's status-bar thread
already performs one `Operation::Status` round-trip per redraw
(`live_status`, src/bin/a.rs:2485); store the returned bit in
`StatusBarCtx` next to `flash`/`last_drawn`:

```rust
/// Last-known "does the attached workload currently want mouse reports"
/// bit, refreshed once per status-bar redraw via the existing
/// Operation::Status round-trip. Read by the input thread to decide
/// whether an outside-the-bar click may be forwarded (section 4.2).
child_mouse_mode: Arc<Mutex<bool>>,
```

**Accepted staleness**: this bit is at most one status-bar debounce
interval old (same category of staleness `memory_indicator` already has).
A click landing in the narrow window right after a workload toggles its own
mouse mode can be mis-routed once. Acceptable: the failure mode is "one
click dropped" or "one click forwarded a beat late," never corruption, and
it self-corrects within one redraw tick.

### 3.3 Enable/disable lifecycle

Mouse mode is a *client*-owned terminal mode, paired with the client's
other raw-mode setup/teardown:

- **Enable**: once, when entering raw mode for an interactive (`tty`)
  attach — same lifetime as the DECSTBM reservation
  (`apply_terminal_layout`). Sent alongside the initial layout write:
  `\x1b[?1000h\x1b[?1006h`.
- **Disable**: in `reset_terminal` (src/bin/a.rs:2425), prepended the same
  way `\x1b[?1049l` already is, so a detach or process exit never leaves
  the user's real terminal (and the shell they return to) stuck emitting
  mouse escape sequences on every click: `\x1b[?1006l\x1b[?1000l` before
  the existing `\x1b[r\x1b[2J\x1b[H\x1b[?25h`.
- **Across a switch** (`perform_switch`): no-op. Mouse mode, like the
  DECSTBM region, is terminal-session state, not per-attached-session
  state (docs/fast-session-switching-design.md section 2's "survives a
  switch untouched" list) — it stays enabled across `Ctrl-b n`/a bar click
  exactly as raw mode and the scroll region do.
- **This is enabling a mode the workload did not ask for.** That is the
  entire point (the bar needs it even for a plain shell), and it is why
  section 3.2's routing check exists — the client takes on the
  responsibility of not leaking the side effect into a workload that never
  opted in.

### 3.4 v1 gate: opt-in, not default-on

Given how broadly `a attach` is already in daily use, turning on mouse
reporting for every interactive attach by default is a bigger behavioral
change than this one exploratory pass should ship unreviewed — an errant
click during totally ordinary terminal use (selecting text with the mouse
to copy it, for instance, which on many terminals is itself intercepted
differently once `?1000h` is active) now does *something* instead of
nothing. v1 gates the enable behind an environment variable,
`APLEXER_MOUSE=1`, checked once at the top of `attach()` next to the other
env-based knobs; default is off, so building and shipping this code changes
no behavior for any existing session until a user opts in. Promoting to
default-on is a follow-up decision after some real usage, not part of this
doc.

## 4. Click-region mapping

### 4.1 Data structure

The bar's content is rebuilt every redraw (`status_bar_text`,
src/bin/a.rs:2628); the click-region map must be recomputed alongside it,
never cached across redraws, because sibling lists, tags, and terminal
width all change out from under a static mapping otherwise.

```rust
/// One clickable span of the rendered status-bar line, in 0-based character
/// columns `[start, end)` — the same units `pad_or_truncate` counts in, so a
/// click's `Cx` (1-based) maps in with a single `- 1`.
struct BarRegion {
    cols: std::ops::Range<usize>,
    action: BarClick,
}

enum BarClick {
    /// Click a sibling's `{i}:{tag}` token: switch to it. `i` is exactly
    /// the digit `Ctrl-b <i>` would send (`SwitchTarget::Index`).
    Sibling(usize),
    /// Click the `[+N workspaces]` indicator (section 5): browse siblings.
    WorkspacePicker,
    /// Click a session name while browsing another workspace (section 5):
    /// jump straight to it, no matter which workspace it's in.
    RemoteSession(Uuid),
}
```

`StatusBarCtx` gains one more shared slot, written by `draw_status_bar`
right after it computes `text`, read by the input thread on a bar-row
click:

```rust
regions: Arc<Mutex<Vec<BarRegion>>>,
```

### 4.2 Building the map alongside the text, not by re-parsing it

Re-deriving column ranges by string-searching the final, already-padded
bar text is fragile (tag names can themselves look like `N:word`, ANSI
reverse-video wraps the whole line but not sub-spans, truncation can cut a
token mid-way). Instead, the same builder that assembles the text also
records the byte range it just wrote, character-counted the same way
`pad_or_truncate` does:

```rust
fn workspace_summary_with_regions(
    ctx: &StatusBarCtx,
    record: &SessionRecord,
) -> (String, Vec<BarRegion>) {
    // Same filtering/sorting as workspace_summary (src/bin/a.rs:2552): all
    // sessions sharing record.workspace, list_records order.
    let mut text = String::new();
    let mut regions = Vec::new();
    for (i, r) in siblings.iter().enumerate() {
        if i > 0 { text.push(' '); }
        let start = text.chars().count();
        // ... push "{i}:{tag}", optional '*', optional "(state)" ...
        regions.push(BarRegion { cols: start..text.chars().count(), action: BarClick::Sibling(i + 1) });
    }
    (text, regions)
}
```

`status_bar_text` then offsets each range by the character position where
the siblings segment starts in the full line (known exactly, since it is
built by concatenation in a fixed order) before storing them in
`ctx.regions`. Truncation (`pad_or_truncate` on an overlong line) is
handled by clamping stored ranges to `cols` at store time — a region
partially cut off by truncation is still clickable on its visible prefix,
which matches what the user can actually see and click.

### 4.3 Row check

The bar occupies exactly the physical terminal's last row. A click's `Cy`
(1-based) is a bar click iff `Cy == geom.rows` (`TermGeom.rows`, the
physical row count `apply_terminal_layout` recorded) — not `reserved_rows`,
which is the *server's* PTY height. Any other `Cy` is inside the workload's
own rows and is routed per section 3.2/4.4.

### 4.4 Routing algorithm (input thread)

```
on MouseReport { press: true, col, row, .. }:      // ignore releases in v1
    if row == geom.rows:
        find region in ctx.regions covering (col - 1)
        match region.action:
            Sibling(i)        -> perform_switch(SwitchTarget::Index(i), ...)
            WorkspacePicker   -> enter/advance browse mode (section 5)
            RemoteSession(id) -> perform_switch(SwitchTarget::ToId(id), ...)
        (never forward the click's bytes to the workload)
    else:
        if *ctx.child_mouse_mode.lock():
            forward the raw SGR bytes verbatim (Forward action, unchanged
            coordinates -- the workload's PTY occupies rows 1..rows-1
            physically, identical to what it always occupied, so no
            coordinate translation is needed)
        else:
            drop the bytes (do not forward, do not act) -- section 3.2
```

This is one more `InputAction` variant recognized by `InputScanner::scan`,
alongside `Forward`/`Detach`/`Switch`, following the exact same
"provisionally-consume-until-disambiguated" shape `pending_ctrl_b` already
uses — except the prefix is multi-byte and variable-length (`ESC [ <
digits ; digits ; digits [Mm]`, 9–15 bytes) instead of one byte, so the
scanner needs a small buffering state (`pending_mouse: Vec<u8>`) that
survives across `scan()` calls the same way `pending_ctrl_b` does, and a
byte budget (bail out and forward-as-plain-bytes if the buffered prefix
exceeds a generous cap, e.g. 32 bytes, without resolving to `M`/`m` —
guards against a corrupted/foreign escape sequence wedging the scanner
forever). Recognizing this prefix is unambiguous and safe to add
unconditionally (`ESC [ <` is not a sequence any real keyboard or
terminal-generated input otherwise produces), so `InputScanner` gains this
case regardless of whether `APLEXER_MOUSE` is set — it simply never fires
if mouse mode was never enabled, since then the terminal never emits these
bytes in the first place.

## 5. Interaction model

### 5.1 Click a sibling name: switch (feature 1)

Direct: click lands on a `{i}:{tag}` token in the bar's existing sibling
list → `perform_switch(SwitchTarget::Index(i), ...)`, the exact same call
`Ctrl-b <i>` already makes. No new switching logic; this feature is purely
"give the existing numbered list a mouse binding." Failure behavior
(flash-and-stay) is unchanged (docs/fast-session-switching-design.md
section 7).

### 5.2 Click to browse another workspace: a one-row picker (feature 2)

One reserved row leaves no room for a popup, dropdown, or second bar —
whatever this looks like has to fit in the same line the sibling list
already lives on. Proposal: an indicator segment, `[+N ws]` (N = number of
*other* workspaces with at least one session), appended to the bar
whenever N > 0, e.g.:

```
~/proj:main [claude]  |  1:main* 2:review  |  [+2 ws]
```

Clicking `[+N ws]` doesn't switch anything — it puts the bar itself into a
transient **browse mode**: the sibling segment is replaced, for that
workspace, with that *other* workspace's own session list (same
`{i}:{tag}` rendering, but every entry becomes a `BarClick::RemoteSession`
region instead of `BarClick::Sibling`, since jumping there crosses
workspaces and must resolve by UUID, not by an index scoped to the current
workspace). The `[+N ws]` indicator itself stays in place and now reads
`[< ws 1/2]` (workspace picker with a position indicator), so clicking it
again cycles to the next of the other workspaces, wrapping — matching
`Ctrl-b N`'s cross-workspace cycling order (`group_by_workspace`'s flat
order) so there is exactly one "next workspace" notion in the whole
program, not two competing ones.

Browse mode is purely a **display** state — the actually-attached session
never changes until the user clicks one of the browsed names. It's
sensible to time it out (revert to the normal current-workspace sibling
list) after a few seconds of no further bar clicks, reusing the existing
flash-expiry pattern (`FLASH_DURATION`-style field on `StatusBarCtx`,
`Instant`-stamped, checked at the top of `status_bar_text` the same way the
flash check already is) so an unattended terminal doesn't sit forever
showing someone else's session list. Design-only for v1 (section 7);
sketched here for completeness of the interaction model, not included in
the implemented slice.

**Rejected alternative**: making `[+N ws]` a plain synonym for `Ctrl-b N`
(immediately global-cycle-switch on click, no browse step). Simpler, but
loses the entire value of "see other workspaces" the user asked for — it
would jump blind, one workspace at a time, with no way to see what's
*in* the next workspace before committing to it. Browse-then-click costs
one extra click for a real gain in visibility, and degrades gracefully
(clicking a name still switches immediately, exactly one click, same as
feature 1) once you know the layout.

### 5.3 New switch-resolution primitive needed: jump to an arbitrary session

`pick_switch_target`/`resolve_switch_target` (src/bin/a.rs:2801,2848)
currently offer `Next`/`Prev`/`NextGlobal`/`PrevGlobal`/`Last`/`Index(n)` —
none of these address "this specific session, wherever it is, by identity,"
which `BarClick::RemoteSession(Uuid)` needs (feature 2's whole point is
crossing workspace boundaries by name, not by relative cycling).
`SwitchTarget::Last` already resolves by UUID
(`resolve_switch_target`/`pick_switch_target`'s `Last` arm) — this is the
same shape, generalized:

```rust
enum SwitchTarget {
    // ...existing variants unchanged...
    /// Jump directly to this session, wherever it is (bar clicks on a
    /// browsed remote workspace's sibling list; feature 2 only —  no
    /// keyboard chord is proposed for this).
    ToId(Uuid),
}
```

`pick_switch_target`'s `ToId` arm: search every group (like `Last`) for a
record with that UUID; `bail!` "session no longer exists" if not found
(the browse-mode list can go stale between redraw and click, same
staleness class as any other status-bar data). No workspace-membership
check — that's the entire feature.

## 6. What is *not* changing

- `Operation::Attach`/`Detach`, the socket protocol, and the worker's PTY
  handling are untouched apart from the one additive `Status` field
  (section 3.2), which is read-only and ignored by old clients.
- The DECSTBM reservation, raw-mode lifecycle, and switch machinery
  (`perform_switch`, `InputScanner`'s existing `Ctrl-b` rules) are reused
  verbatim, not modified in shape — mouse handling adds a new
  `InputAction` case, it does not touch the existing `Detach`/`Switch`
  cases.
- No drag, hover, or scroll-wheel handling (`?1002h`/`?1003h` are never
  enabled) — v1 is click-only, matching the user's literal ask ("click on
  the sessions").

## 7. What this doc implements now vs. leaves as design

Given real, actively-used sessions on this machine and a file
(`src/bin/a.rs`) that has had heavy concurrent edits tonight, the riskiest
pieces — enabling mouse mode on the live raw-mode path, wiring a new
`InputAction` into the production input thread, and the `Operation::Status`
protocol touch — are **left as design only** in this pass, specified
precisely enough above to implement directly. What *is* implemented
tonight, self-contained and exercised by unit tests, with zero effect on
any existing attach path (nothing in the existing call graph reaches it):

- `parse_sgr_mouse`: a pure parser for `ESC [ < Cb ; Cx ; Cy [Mm]`,
  handling complete sequences, genuinely-incomplete prefixes (needs more
  bytes — the shape a live scanner must detect to keep buffering), and
  non-mouse input (ordinary CSI sequences like arrow keys) as a fast,
  unambiguous "not this" — the exact primitive `InputScanner`'s future
  `pending_mouse` buffering (section 4.4) would call per byte.
- `workspace_summary_regions`: builds the same `{i}:{tag}[*][(state)]`
  sibling list `workspace_summary` renders, alongside the `BarRegion`
  column ranges for each token, so the mapping in section 4.2 is proven
  correct against real tag strings (unicode tags, `*`-marked current
  session, non-running state suffixes) before it's wired into
  `draw_status_bar`.

Both are additive functions plus tests; nothing existing was modified.
Follow-up work (section 8) wires these into `attach()` behind
`APLEXER_MOUSE=1`.

## 8. Implementation checklist (follow-up, not done tonight)

1. `src/screen.rs`: `ScreenTracker::mouse_mode_active() -> bool`.
2. `src/worker.rs`: merge `mouse_mode` into `Operation::Status`'s response
   (section 3.2).
3. `src/bin/a.rs`: `StatusBarCtx.child_mouse_mode: Arc<Mutex<bool>>`, set
   from `live_status`'s existing per-redraw RPC result.
4. `src/bin/a.rs`: `APLEXER_MOUSE` env check; on set, `attach()` sends
   `\x1b[?1000h\x1b[?1006h` alongside its initial `apply_terminal_layout`
   call, and `reset_terminal` sends `\x1b[?1006l\x1b[?1000l` first.
5. `InputScanner`: add `pending_mouse` buffering + `InputAction::Mouse`
   (or resolve directly to `Forward`/`Switch`/a new drop-silently case
   inside `scan`, per section 4.4), built on tonight's `parse_sgr_mouse`.
   Unit tests: split-across-`scan()`-calls mouse sequences (mirroring the
   existing Ctrl-b split-read tests), a non-mouse CSI passed through
   untouched, an oversized/unterminated `ESC [ <` prefix eventually
   forwarded rather than hanging forever.
6. `draw_status_bar`: call `workspace_summary_regions`, offset into
   `ctx.regions`, replacing the plain `workspace_summary` call.
7. Wire `BarClick::Sibling`/`WorkspacePicker`/`RemoteSession` handling into
   the input thread's new mouse case (section 4.4).
8. `SwitchTarget::ToId(Uuid)` + `pick_switch_target` arm (section 5.3).
9. Browse-mode state + `[+N ws]`/`[< ws i/N]` rendering (section 5.2) —
   the largest remaining piece; consider shipping features 1 and 2's
   click-to-switch-immediate variant first, browse-mode as a fast-follow.
10. Manual verification with `APLEXER_MOUSE=1` in a throwaway session
    (`APLEXER_RUNTIME_DIR`/`APLEXER_STATE_DIR` overrides): click a sibling
    name, confirm switch; attach a mouse-using program (`vim`, `:set
    mouse=a`) and confirm its own clicks still reach it unmodified; click
    in a plain shell with no mouse mode of its own and confirm no garbage
    lands on the command line.
11. `cargo build --release`, `cargo test`, `./scripts/validate.sh` (if
    present) before flipping any default.
