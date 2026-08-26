# Fast in-process session switching (`Ctrl-b n/p/l/1-9`)

Status: design, ready to implement. Target file is almost entirely
`src/bin/a.rs`; **no worker or protocol changes are required** (section 4).
All function/line references are against commit `3451c0b`.

## 1. Problem

Today the only way to move from session A to session B while attached is:
detach (`Ctrl-]` or `Ctrl-b d`), then run a whole new `a <N>` / `a attach`
command. That round-trip spawns a fresh `a` process, re-parses argv, re-runs
`Paths::discover()`, re-enters raw mode, re-negotiates the DECSTBM scroll
region, reconnects a Unix socket, and replays the target's history tail --
plus the human cost of typing a command. All of that is wasted work when the
terminal is already in the right state and the goal is "point this live
terminal at a different session's PTY".

The most common switch in practice is between two tags of the *same*
workspace (e.g. `main` and `review`, exactly the sibling set the status bar
already displays), so the design optimizes that case first.

## 2. What the client already has, and what a switch actually needs

`attach()` (src/bin/a.rs:1965) currently owns, in one flat function:

**Terminal-session state -- survives a switch untouched:**

- `_raw: Option<RawMode>` -- raw termios, entered once, restored on drop.
- `_ui_guard: Option<TerminalUiGuard>` -- runs `reset_terminal` on final exit.
- `stdout: Arc<Mutex<io::Stdout>>` -- the shared, tear-free output lock.
- `term: Arc<Mutex<TermGeom>>` -- physical geometry + `reserved` flag,
  maintained by `apply_terminal_layout` / the resize-poll thread. The DECSTBM
  scroll region depends only on the physical terminal, not on which session
  is attached, so it is **not** re-negotiated on switch.
- `last_activity: Arc<Mutex<Instant>>` -- status-bar debounce clock.
- The three threads themselves (input-forwarding, resize-poll, status-bar).
  They are spawned once and live across switches.

**Per-session state -- must become swappable:**

- `reader: UnixStream` -- the socket the main frame loop reads PTY output
  from. Owned by the main loop; the main loop itself installs the new one.
- `writer: Arc<Mutex<UnixStream>>` -- a `try_clone` of the same socket,
  shared with the input and resize threads via `send_data`/`send_control`.
  Because it is already behind an `Arc<Mutex<..>>`, swapping the
  `UnixStream` *inside* the mutex instantly repoints every forwarding
  thread at the new session -- no thread teardown or respawn.
- `record` -- currently cloned by value into the status-bar thread as
  `status_record` (src/bin/a.rs:2138) and used for `status_bar_text` /
  `memory_indicator`. Must become `Arc<Mutex<SessionRecord>>` so the status
  bar (and its per-redraw memory RPC) follows the switch automatically.

So the before/after in one line: **before**, `attach()` = one connection,
threads capture per-session values by clone, one frame loop, return on any
disconnect. **After**, `attach()` = an outer `'session` loop; the connection,
the writer's inner stream, and a shared `Arc<Mutex<SessionRecord>>` are the
only things replaced per iteration; threads, raw mode, scroll region, and
geometry are set up exactly once.

## 3. New shared state and helpers (all in `src/bin/a.rs`)

```rust
/// Which session a Ctrl-b switch key asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SwitchTarget {
    Next,           // Ctrl-b n : next session in the current workspace
    Prev,           // Ctrl-b p : previous session in the current workspace
    NextGlobal,     // Ctrl-b N : next session across all workspaces
    PrevGlobal,     // Ctrl-b P : previous session across all workspaces
    Last,           // Ctrl-b l : the session attached before this one
    Index(usize),   // Ctrl-b 1..9 : Nth session of the current workspace
}

/// A fully established connection to the new session, handed from the
/// input thread to the main frame loop.
struct SwitchOutcome {
    record: SessionRecord,
    reader: UnixStream,   // Attach handshake already completed on it
    history: Vec<u8>,     // the replay tail read during the handshake
}

/// Everything a status-bar redraw needs; one value cloned into each thread
/// instead of five loose Arc parameters.
#[derive(Clone)]
struct StatusBarCtx {
    stdout: Arc<Mutex<io::Stdout>>,
    term: Arc<Mutex<TermGeom>>,
    paths: Paths,
    record: Arc<Mutex<SessionRecord>>,               // NEW: shared, swappable
    flash: Arc<Mutex<Option<(String, Instant)>>>,    // NEW: transient error line
}

const FLASH_DURATION: Duration = Duration::from_secs(2);
```

Plus, alongside the existing `active: Arc<AtomicBool>`:

```rust
let pending_switch: Arc<Mutex<Option<SwitchOutcome>>> = Arc::new(Mutex::new(None));
let switch_in_progress = Arc::new(AtomicBool::new(false));
let last_session: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
```

### 3.1 `establish` -- extracted handshake

Extract the current attach handshake (src/bin/a.rs:1967-1983: `connect` +
`Operation::Attach` request + response check + initial history frame) into:

```rust
fn establish(record: &SessionRecord, replay_bytes: Option<usize>)
    -> Result<(UnixStream, Vec<u8>)>
```

Used by the initial attach and by every switch. `replay_bytes` is the same
`Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES))` value computed
once at the top of `attach()` -- a switch replays the same 32KB-default tail
a fresh attach does (and honors `--history-bytes` if the user passed it),
because the target has typically been running unattended and the user needs
to see "what's on its screen now". Jumping straight to live streaming with
no replay would show a blank screen until the workload next writes.

### 3.2 Target resolution -- reuses the `a list` ordering, nothing new

`list_records` (src/lib.rs:331) sorts by `Reverse(created_at_ms)`, and
`group_by_workspace` (src/bin/a.rs:666) preserves that order within each
workspace group -- this is exactly the ordering `a list` prints and that
`a <N> <M>`'s session index `M` already means (`resolve_quick_index`,
src/bin/a.rs:826). Switching reuses it verbatim; no second numbering scheme.

```rust
/// True iff check_attachable would pass; used to skip dead sessions when
/// cycling with n/p (never for explicit Index addressing).
fn is_attachable(r: &SessionRecord) -> bool { check_attachable(r).is_ok() }

/// Pure candidate selection over the same groups `a list` prints.
/// Split from the paths-touching wrapper so it is unit-testable.
fn pick_switch_target(
    groups: &[(PathBuf, Vec<SessionRecord>)],
    current_workspace: &Path,
    current_id: Uuid,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord>

fn resolve_switch_target(
    paths: &Paths,
    current: &SessionRecord,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let groups = group_by_workspace(list_records(paths)?);
    pick_switch_target(&groups, &current.workspace, current.id, target, last)
}
```

`pick_switch_target` semantics:

- **Next/Prev**: candidates = the group whose workspace equals
  `current_workspace` (if the current session's workspace has no group --
  e.g. it was killed underneath us -- error "current workspace has no
  sessions"). Walk from the current session's position +1 / -1 with
  wraparound, **skipping** the current session and any candidate failing
  `is_attachable`. If nothing remains: `bail!("no other running session in
  this workspace")`. Skipping dead sessions is deliberate: cycling exists to
  land somewhere useful, and stopping on an exited session only to flash an
  error would make n/p worse than useless in a workspace with old corpses.
  If `current_id` is not found in the group (record vanished), start the
  walk from position 0.
- **NextGlobal/PrevGlobal**: same walk, but candidates = all groups
  flattened in group order (alphabetical workspace, then list order) --
  i.e. exactly the top-to-bottom order of `a list`'s tree. Wraps.
- **Index(n)**: 1-based index into the current workspace's group, **no
  skipping** -- the number must mean exactly what the status bar shows
  (section 6), dead or alive. Out of range: `bail!("no session {n} here:
  this workspace has {len} session(s)")`. A dead target is not filtered
  here; `perform_switch`'s `check_attachable` produces the error, which is
  flashed.
- **Last**: `last.ok_or_else(|| anyhow!("no previous session"))`, then
  `read_record(&paths.record(id))` mapped to "previous session is gone".
  (Resolved by UUID, so it survives renames and works across workspaces.)
- Resolving to the current session itself (e.g. `Ctrl-b 1` while on #1) is
  detected in `perform_switch` and is a silent no-op.

### 3.3 `perform_switch` -- atomic switch-or-stay, runs on the input thread

The critical ordering property: **the new connection is fully established
before the old one is touched.** Any failure leaves the attachment to the
current session completely undisturbed -- resolution, `check_attachable`,
and `establish` all happen while A is still live on screen.

```rust
fn perform_switch(
    paths: &Paths,
    target: SwitchTarget,
    replay_bytes: Option<usize>,
    shared_record: &Arc<Mutex<SessionRecord>>,
    last_session: &Arc<Mutex<Option<Uuid>>>,
    writer: &Arc<Mutex<UnixStream>>,
    pending_switch: &Arc<Mutex<Option<SwitchOutcome>>>,
    switch_in_progress: &Arc<AtomicBool>,
) -> Result<()> {
    let current = shared_record.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let next = resolve_switch_target(paths, &current, target,
                                     *last_session.lock().unwrap_or_else(PoisonError::into_inner))?;
    if next.id == current.id {
        return Ok(());                       // switching to yourself: no-op
    }
    check_attachable(&next)?;
    switch_in_progress.store(true, Ordering::Relaxed);
    let result = (|| {
        let (reader, history) = establish(&next, replay_bytes)?;
        let writer_clone = reader.try_clone()?;      // before mutating anything
        // Repoint every forwarding thread (input, resize) at B, then
        // retire A's stream. From this instant keystrokes land in B.
        let old = {
            let mut w = writer.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *w, writer_clone)
        };
        *pending_switch.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(SwitchOutcome { record: next.clone(), reader, history });
        *last_session.lock().unwrap_or_else(PoisonError::into_inner) = Some(current.id);
        // Polite detach from A, then shutdown so the main loop's blocked
        // read_frame on A's socket returns immediately. shutdown() is
        // socket-wide, so it also unblocks the reader fd cloned from this
        // stream -- the same mechanism the existing detach path relies on.
        let mut old = old;
        let _ = write_json(&mut old, &AttachControl::Detach);
        let _ = old.shutdown(std::net::Shutdown::Both);
        Ok(())
    })();
    switch_in_progress.store(false, Ordering::Relaxed);
    result
}
```

On `Err`, the caller (input thread) sets the flash and forces a status-bar
redraw (section 6); nothing was sent to A, nothing swapped -- the user just
stays where they were with an explanation on the bar.

## 4. No worker changes

`handle_attach` in src/worker.rs (line 621) already: supports any number of
sequential/concurrent attach subscriptions (`runtime.output.subscribe`),
tears one down cleanly on `AttachControl::Detach` or EOF, and replays a
bounded tail per `Operation::Attach { history_bytes }`. A switch is, from
each worker's point of view, indistinguishable from one client detaching
and another attaching. The brief overlap where the client is subscribed to
both A and B (between `establish` and the old stream's shutdown) is safe:
they are different workers, and multi-client attach is already supported.

## 5. The switch as seen by each thread

### 5.1 Input thread: extend the existing Ctrl-b state machine

The Ctrl-b,d handling (src/bin/a.rs:2050-2099) already tracks
`pending_ctrl_b` across `read()` boundaries. Extend that same machine --
do not add a second scanner. To make it unit-testable (the split-across-
reads cases especially), extract the pure byte-scanning into:

```rust
enum InputAction {
    Forward(Vec<u8>),        // ordinary input for the current session
    Detach,                  // Ctrl-] or Ctrl-b d
    Switch(SwitchTarget),    // Ctrl-b n/p/N/P/l/1-9
}

#[derive(Default)]
struct InputScanner { pending_ctrl_b: bool }   // survives across read() calls

impl InputScanner {
    fn scan(&mut self, buffer: &[u8]) -> Vec<InputAction>
}
```

Scan rules (existing semantics preserved exactly, new keys added):

- `0x1d` (Ctrl-]) outside pending state: flush accumulated `Forward`, emit
  `Detach`, stop scanning (the rest of the buffer is discarded -- identical
  to today's `break 'outer`).
- `0x02` (Ctrl-b): set `pending_ctrl_b`, consume the byte. Pending state
  persists to the next `scan` call if the buffer ends here (today's
  behavior).
- pending + `d`: flush, emit `Detach`, stop.
- pending + `n`/`p`/`N`/`P`/`l`: flush, emit `Switch(Next/Prev/NextGlobal/
  PrevGlobal/Last)`, clear pending, **continue scanning** the remainder.
- pending + `1`..`9`: flush, emit `Switch(Index(digit))`, clear pending,
  continue.
- pending + anything else (including `0`): emit the withheld `0x02` into the
  forward buffer and reprocess the byte normally -- the existing "Ctrl-b is
  not a real prefix" contract: unbound sequences pass through to the
  workload untouched, so programs that use literal Ctrl-b keep working.

The thread body becomes: read stdin -> `scanner.scan(&buffer[..n])` ->
execute actions in order:

- `Forward(bytes)`: `send_data(&input_writer, &bytes)`; on error, break
  (unchanged). Note ordering matters: a `Forward` before a `Switch` goes to
  the old session (keystrokes typed before the chord), a `Forward` after it
  goes to the new one, automatically, because `perform_switch` swapped the
  stream inside `input_writer`'s mutex.
- `Detach`: flush, `send_control(Detach)`, `input_active.store(false)`,
  thread exits (unchanged).
- `Switch(t)`: `perform_switch(...)`; on `Err(e)`, set
  `*flash = Some((format!("{e:#}"), Instant::now()))` and call
  `draw_status_bar(&status_ctx)` immediately, then keep going. The
  consumed chord bytes are never forwarded.

The non-tty path (raw pass-through at src/bin/a.rs:2058) is unchanged:
switching is a tty-only feature.

The input thread needs these additional captures: `paths.clone()`,
`replay_bytes`, `shared_record`, `last_session`, `pending_switch`,
`switch_in_progress`, and a `StatusBarCtx` clone.

### 5.2 Main frame loop: outer `'session` loop

Restructure the tail of `attach()` (the `loop` at src/bin/a.rs:2174) into:

```rust
let mut reader = reader;   // from the initial establish()
'session: loop {
    // ---- existing frame loop, verbatim: read_frame(&mut reader),
    //      Data -> write_locked + last_activity bump, End/Exit/Error/EOF -> break ----

    // The frame loop broke: either the session ended/we detached, or the
    // input thread killed the old stream to hand us a switch.
    let outcome = take_pending_switch(&pending_switch, &switch_in_progress);
    let Some(outcome) = outcome else { break };

    *shared_record.lock().unwrap_or_else(PoisonError::into_inner) = outcome.record;
    reader = outcome.reader;   // old stream dropped (closed) here

    // Light-variant reset: clear screen + home + show cursor, but keep the
    // DECSTBM scroll region and raw mode -- they are terminal state, not
    // session state. (?25h because A's TUI may have hidden the cursor and
    // B never knows to show it; same rationale as reset_terminal's.)
    let mut seq: Vec<u8> = b"\x1b[2J\x1b[H\x1b[?25h".to_vec();
    seq.extend_from_slice(&outcome.history);
    let _ = write_locked(&stdout, &seq);
    if let Ok(mut t) = last_activity.lock() { *t = Instant::now(); }

    // B's PTY may still be sized for its previous client (or the 24x80
    // default). The resize thread won't resend an unchanged terminal size
    // (its `last` cache), so push the current geometry explicitly.
    let geom = term.lock().map(|g| *g).unwrap_or(TermGeom { rows: 0, cols: 0, reserved: false });
    if geom.rows > 0 {
        let _ = send_control(&writer, &AttachControl::Resize {
            rows: reserved_rows(geom.rows), cols: geom.cols,
        });
    }
    draw_status_bar(&status_ctx);   // clear wiped the reserved row; redraw now
    continue 'session;
}
active.store(false, Ordering::Relaxed);           // unchanged final cleanup
if let Ok(stream) = writer.lock() { let _ = stream.shutdown(std::net::Shutdown::Both); }
Ok(())
```

`take_pending_switch` closes one race: the frame loop can break because A's
worker died at the same moment the user pressed a switch chord, *before*
the input thread stored the outcome. If `pending_switch` is `None` but
`switch_in_progress` is true, poll (10ms sleep) for up to 500ms for the
outcome before giving up and treating it as a normal exit:

```rust
fn take_pending_switch(
    pending: &Arc<Mutex<Option<SwitchOutcome>>>,
    in_progress: &Arc<AtomicBool>,
) -> Option<SwitchOutcome> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(o) = pending.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Some(o);
        }
        if !in_progress.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
```

The initial `[aplexer attached; ...]` banner (src/bin/a.rs:2003) prints
once, before the loop, updated to:
`[aplexer attached; Ctrl-] or Ctrl-b d detaches; Ctrl-b n/p/1-9/l switches]`.
No banner on switch -- the status bar already names the new session.

### 5.3 Resize thread: no changes

It sends through the same `Arc<Mutex<UnixStream>>` (`resize_writer`), so it
follows the swap for free. Its `size != last` cache is why the main loop
sends one explicit Resize per switch (above). One nuance: `send_control`
failing currently `break`s the resize loop (src/bin/a.rs:2124). During a
switch the old socket dies deliberately; if a real terminal resize races
that exact window, the thread could exit early and leave resizes dead for
the rest of the attach. Fix while here: replace `break` with `continue`
(keep looping while `resize_active`); the post-switch explicit Resize
covers any lost update, and on final detach `resize_active` goes false
anyway.

### 5.4 Status-bar thread: shared record instead of a clone

Replace the per-thread `status_record = record.clone()` /
`status_paths` / `status_stdout` / `status_term` captures
(src/bin/a.rs:2134-2138) with one `StatusBarCtx` clone. `draw_status_bar`,
`status_bar_text`, and `sibling_summary` change signature to take the ctx
(locking `ctx.record` and cloning at the top of a redraw). The debounce
loop itself is unchanged. `memory_indicator`'s per-redraw Status RPC now
automatically targets the new worker after a switch.

## 6. Keybindings and numbering: decisions and rationale

| Chord | Action |
|---|---|
| `Ctrl-]` | detach (unchanged) |
| `Ctrl-b d` | detach (unchanged) |
| `Ctrl-b n` / `Ctrl-b p` | next / previous running session in the **current workspace**, `a list` order, wraps |
| `Ctrl-b N` / `Ctrl-b P` | next / previous running session across **all workspaces**, `a list` top-to-bottom order, wraps |
| `Ctrl-b 1`..`Ctrl-b 9` | session #N of the **current workspace** (the numbers the status bar shows) |
| `Ctrl-b l` | last session -- toggle back to whatever was attached before (tmux's last-window muscle memory) |
| `Ctrl-b` + anything else | not a prefix: both bytes forward to the workload (unchanged contract) |

**Digits address sessions within the current workspace, not workspaces.**
This deliberately does *not* copy `a <N>`'s top-level number. `a <N>`
numbers workspaces because at the shell you're outside everything and the
workspace is the natural first coordinate. Mid-session the situation is
inverted: you are already *in* a workspace, the overwhelmingly common jump
is to a sibling tag (`main` <-> `review`), and the status bar's sibling
list is already workspace-scoped. Making `Ctrl-b 3` mean "workspace 3's
first session" would be almost never what's wanted and would collide
visually with the sibling numbers on the bar. So the digit reuses the
*other half* of the existing scheme -- the per-workspace session index `M`
from `a <N> <M>` (`resolve_quick_index`'s session-index branch,
src/bin/a.rs:847-857) -- which is the same list order, same code path
(`group_by_workspace` over `list_records`), just surfaced in-session. No
new numbering is invented; the two coordinates of `a <N> <M>` are simply
split between out-of-session (workspace) and in-session (session) use.
Cross-workspace reach stays available via `Ctrl-b N`/`P` (cycling the flat
`a list` order) and `Ctrl-b l`; an arbitrary cross-workspace jump remains
detach + `a <N> <M>`, which is rare enough not to deserve a chord.

**n/p scope defaults to the current workspace.** Matches the status bar's
own scope, keeps the cycle short and predictable, and the shifted variants
cover the global case. Both wrap (a two-session workspace makes `n` and `p`
equivalent toggles; a one-session workspace flashes "no other running
session in this workspace" rather than silently doing nothing).

**Newly consumed keys.** Today `Ctrl-b x` forwards both bytes for any
`x != d`; after this change `n p N P l 1-9` are also consumed. That is the
same documented tradeoff `d` already made; programs needing a literal
`Ctrl-b n` sequence are rarer than session switching is common, and
`Ctrl-b` followed by anything unbound still passes through.

### 6.1 Status bar: number the siblings, show flashes

Two changes to make the digits discoverable and give errors a home:

1. `sibling_summary` (src/bin/a.rs:1884) becomes `workspace_summary`,
   listing **all** sessions of the current workspace (not just the others),
   in the same filtered `list_records` order (identical to the
   `group_by_workspace` within-group order -- both preserve `list_records`'
   `Reverse(created_at_ms)` sort), each as `{i}:{tag}`, with `*` appended
   to the current session and `({state})` appended only when the state is
   not `running` (compactness: running is the common case; dead/broken is
   what needs calling out). Example bar tail: `1:main* 2:review 3:build(broken)`.
   The `siblings:` prefix is dropped. Empty (single-session workspace):
   omit the segment, as today.
2. `draw_status_bar` first checks `ctx.flash`: if `Some((msg, at))` with
   `at.elapsed() < FLASH_DURATION`, render `pad_or_truncate(&format!("[{msg}]"), cols)`
   in the same reverse video instead of the normal text; if the flash has
   expired, clear it to `None` and render normally. The status thread's
   existing poll cadence restores the normal bar at most ~3s after a flash
   (2s flash + up to one `STATUS_BAR_MAX_INTERVAL`-bounded redraw gap) --
   acceptable; no extra timer.

## 7. Failure modes

| Failure | Behavior |
|---|---|
| Index out of range / unknown target / `no previous session` | Flash the resolver's error on the status bar; stay attached to A untouched. |
| Target exists but exited/broken (`check_attachable` fails) | Flash (e.g. `session ... has already exited ...` first line); stay attached. Reachable via explicit `Index`/`Last`; `n`/`p`/`N`/`P` skip such sessions. |
| Only session in workspace (`n`/`p`) | Flash `no other running session in this workspace`; stay. |
| Worker dies between resolve and `establish` (connect/handshake error) | Flash; stay attached -- A's connection was never touched (section 3.3 ordering). |
| A's worker dies mid-switch | `take_pending_switch`'s 500ms grace completes the switch anyway (the user was leaving A regardless). |
| B's worker dies immediately after handoff | Frame loop hits EOF with no pending switch: falls through to the normal detach path -- `TerminalUiGuard`/`RawMode` restore the terminal and `attach()` returns, exactly like a session exiting under you today. A hard post-switch failure degrades to a clean detach, never a wedged terminal. |
| Switch to self (`Ctrl-b 1` while on #1, `l` with last == current is impossible by construction) | Silent no-op. |
| Flash while a full-screen TUI is redrawing | Same byte-safety as every status redraw: serialized under the `stdout` mutex, save/restore-cursor wrapped (`draw_status_bar`'s existing sequence). |

Errors are flashed rather than detaching on: a failed switch means the user
still has a perfectly good live session on screen; throwing them out to a
shell over a typo'd digit would be strictly worse. Only post-handoff death
of the *new* session falls back to detach, because at that point there is
no live session to stay on.

## 8. Performance target

**Target: chord-to-first-byte-of-B's-replay under 20ms on the same host;
typical well under 10ms.** Budget, given the current implementation:

- `list_records` scan for resolution: one `read_dir` + one small JSON read
  per session; tens of microseconds each, sub-millisecond for dozens of
  sessions (spec.md 30's stated scale).
- `UnixStream::connect` + Attach RPC round-trip to a local worker:
  well under 1ms.
- History replay: the same `DEFAULT_ATTACH_REPLAY_BYTES` = 32KB lever the
  recent replay fix (commit c3a209c) already tuned. Writing 32KB to a local
  terminal is single-digit milliseconds; the terminal emulator's paint is
  the true floor and is outside our control.
- Explicitly **not** paid, versus today's detach+reattach: process spawn +
  dynamic linking, clap parsing, `Paths::discover`, `Config` load, raw-mode
  exit/enter, DECSTBM re-negotiation, full screen reset/redraw cycle, and
  the human typing a command.

Nothing on the switch path may do unbounded work: no `Config::load`, no
per-candidate socket round-trips (resolution uses `process_alive` pid
checks via `is_attachable`, same as `a list`).

## 9. Implementation checklist

Work top to bottom; everything is in `src/bin/a.rs` unless noted.

1. **Constants/types.** Add `FLASH_DURATION`, `SwitchTarget`,
   `SwitchOutcome`, `InputAction`, `InputScanner`, `StatusBarCtx`
   (section 3). Derive `Clone` for `StatusBarCtx` (requires `Paths: Clone`
   -- already satisfied, the status thread clones it today).
2. **Extract `establish(record, replay_bytes) -> Result<(UnixStream, Vec<u8>)>`**
   from attach()'s lines 1967-1983 (connect, Attach request, response-id
   check, `into_result`, initial history frame). Call it for the initial
   attach; the initial history write stays where it is (before raw mode).
3. **`InputScanner::scan`** implementing section 5.1's rules; port the
   existing loop body at 2064-2099 into it. Keep `pending_ctrl_b` as
   scanner state so chords split across `read()` calls keep working.
4. **`is_attachable`, `pick_switch_target`, `resolve_switch_target`**
   (section 3.2). `pick_switch_target` is pure over
   `&[(PathBuf, Vec<SessionRecord>)]` for testability.
5. **`perform_switch`** exactly as section 3.3: resolve -> self-check ->
   `check_attachable` -> `establish` -> `try_clone` -> swap writer stream
   via `mem::replace` -> store `pending_switch` -> update `last_session` ->
   `Detach` + `shutdown(Both)` on the old stream; `switch_in_progress` set
   around the fallible span; all-or-nothing on error.
6. **`take_pending_switch`** (section 5.2) with the 500ms grace poll.
7. **Rework `attach()`:**
   - Build `shared_record: Arc<Mutex<SessionRecord>>`, `pending_switch`,
     `switch_in_progress`, `last_session`, `flash`, and a `StatusBarCtx`
     before spawning threads.
   - Input thread: replace the inline scanning with
     `InputScanner`/`InputAction` execution (section 5.1), capturing
     `paths.clone()`, `replay_bytes`, and the new Arcs. `Switch` errors:
     set flash, `draw_status_bar(&ctx)`, continue reading stdin.
   - Resize thread: change the `send_control` error `break` to `continue`
     (section 5.3). No other changes.
   - Status thread: capture one `StatusBarCtx`; drop `status_record`/
     `status_paths`/`status_stdout`/`status_term`.
   - Wrap the frame loop in `'session: loop { ... }` with the
     switch-installation epilogue verbatim from section 5.2 (take outcome,
     swap `reader`, update `shared_record`, `\x1b[2J\x1b[H\x1b[?25h` +
     history in one `write_locked`, bump `last_activity`, explicit
     `AttachControl::Resize` from current `term` geometry via
     `reserved_rows`, `draw_status_bar`, `continue 'session`).
   - Update the attach banner text (section 5.2).
   - Final cleanup after the loop unchanged.
8. **Status bar:** rename `sibling_summary` -> `workspace_summary` with
   numbering/`*`/conditional-state format (section 6.1); adapt
   `status_bar_text` and `draw_status_bar` to `StatusBarCtx` and the flash
   check. Keep the ordering identical to `group_by_workspace`'s
   within-group order (plain `list_records` filter -- add a comment
   asserting that equivalence next to `resolve_quick_index`).
9. **Unit tests** (in `src/bin/a.rs` `#[cfg(test)]` or a new
   `tests/switching.rs` using the binary's functions where visible;
   prefer in-file unit tests since the helpers are private):
   - `InputScanner`: `[0x02, b'n']` -> `[Switch(Next)]`; split across two
     `scan` calls (`[0x02]` then `[b'n']`) -> same; `[0x02, b'x']` ->
     `[Forward([0x02, b'x'])]`; `[b'a', 0x02, b'3', b'z']` ->
     `[Forward([b'a']), Switch(Index(3)), Forward([b'z'])]`;
     `[0x02, b'd']` -> `[Detach]`; `[0x1d]` mid-buffer discards the rest;
     `[0x02, 0x02, b'd']` -> `[Forward([0x02]), Detach]` (matches the
     existing reprocess rule); `[0x02, b'0']` forwards both bytes.
   - `pick_switch_target`: build synthetic groups (two workspaces, three
     sessions each, one `Exited`); assert Next/Prev wrap and skip the dead
     one, Index(2) returns the dead one (no skip), Index(9) errors,
     NextGlobal crosses the workspace boundary in group order, Last
     resolves by id, single-live-session workspace errors on Next.
10. **Integration smoke test** (optional but recommended,
    `tests/oom_isolation.rs` harness style): start two sessions in one
    workspace, run `a attach` under a real PTY (`script -qec` or a small
    `openpty` helper), write `\x02n` to it, then assert via `a capture`
    that subsequent input landed in the second session; mark `#[ignore]`
    if PTY availability in CI is doubtful.
11. **Docs:** README two-line pointer (done alongside this doc); mention
    the new chords in the attach section of README's "First session" block
    (`# Ctrl-] detaches; Ctrl-b n/p/1-9/l switches sessions.`).
12. **Validate:** `cargo build --release --bins`, `cargo test`,
    `./scripts/validate.sh`; manual pass: two tags in one workspace,
    `Ctrl-b n` ping-pong, `Ctrl-b <digit>` to a dead session (flash, stay),
    `Ctrl-b n` in a single-session workspace (flash), `Ctrl-b N` across
    workspaces, `Ctrl-b l` toggle, resize immediately after a switch,
    `Ctrl-b d` and `Ctrl-]` still detach cleanly.
