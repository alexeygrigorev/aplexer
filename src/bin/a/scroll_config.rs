use super::*;

// ---------------------------------------------------------------------------
// Scroll mode (`Ctrl-b [`, or the wheel) -- aplexer's copy-mode
// ---------------------------------------------------------------------------
//
// The problem it solves. `a attach` holds the host terminal on the alternate
// screen for the whole attach (`ATTACH_ALT_SCREEN_ENTER`) so the pre-attach
// `a` session list cannot bleed into the live view. The alternate screen has
// no scrollback, so from the host terminal there is nothing to scroll back
// *to* -- and worse, a terminal with xterm's `alternateScroll` answers a
// wheel event there by synthesizing cursor-up/down key presses and sending
// them to the workload, i.e. scrolling to read types into the user's agent.
// That translation is now off (`?1007l`), which stopped the harm and left the
// user with no way to read earlier output at all.
//
// The shape of the fix is tmux's, not a terminal's. A tmux pane's virtual
// terminal retains a scrollback grid above the visible screen, and copy-mode
// pages through that grid; tmux never asks the host for scrollback and never
// re-parses a byte log. aplexer's equivalent emulator is `ScreenTracker`,
// which the attach client already runs over every relayed byte -- it was just
// built with a scrollback length of zero. Giving the *client's* model a real
// scrollback length (`ClientScreen::try_new_with_scrollback`) makes the
// history accumulate as a side effect of the parse that was happening anyway,
// and `Screen::set_scrollback` pages it.
//
// Why the client's model and not the worker's. The worker is the tmux-faithful
// home for it -- one parse, survives detach -- but it would need a protocol
// addition to serve scrolled-back rows, and the worker parses every session
// whether or not anyone is attached, so the memory would be spent on sessions
// nobody is reading. The client pays only while attached, is already at the
// exact geometry the pager has to render at, and reaches the same "scroll
// back through what happened while I was away" outcome by priming its grid
// once from the worker's retained raw history at attach
// (`ClientScreen::seed_history`, over the `capture` RPC that already exists).
// Only the priming replay reads bytes; from then on the live model *is* the
// history.

/// Retained history depth, in lines, for an attach client's model --
/// `history-limit` in tmux, whose default this deliberately matches.
///
/// Overridable with `APLEXER_HISTORY_LIMIT`; `0` disables scroll mode's
/// history entirely (the pager then has only the current screen, and the
/// model costs exactly what it did before this feature). The value is
/// clamped against `MAX_SCROLLBACK_CELLS` at the terminal's width, so a
/// large number cannot turn into a large allocation.
pub(crate) fn history_limit() -> usize {
    match env::var("APLEXER_HISTORY_LIMIT") {
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .unwrap_or(aplexer::screen::DEFAULT_SCROLLBACK_LINES),
        Err(_) => aplexer::screen::DEFAULT_SCROLLBACK_LINES,
    }
}

/// How much of the worker's retained raw history is replayed into a fresh
/// client model to give it a past (`ClientScreen::seed_history`).
///
/// This used to be sized from the line limit at an assumed ~512 raw bytes per
/// rendered line, which put the default 2000-line grid at ~1 MiB. **A byte
/// budget is not a line budget**, and for the workload aplexer exists for the
/// two diverge in the direction that empties the pager: an agent CLI that has
/// been idle spends its bytes on animation, not on rows. Measured over the
/// retained history of thirteen live agent sessions, one had spent 500 KiB on
/// a spinner containing *zero* line feeds -- 18,000 absolute cursor addresses
/// and not one row of transcript. Any fixed per-line guess is one idle hour
/// away from being a budget of pure noise, so this is simply a flat budget
/// with its cost measured rather than a guess dressed as arithmetic.
///
/// A line-feed-counting budget was tried and refused by measurement: agent
/// CLIs emit many `\n` per *rendered* row (wrapped and redrawn rows), so
/// "the suffix holding 4000 line feeds" cut four sessions from ~2000 retained
/// lines to 51-245. Counting line feeds is no better a proxy for rows than
/// counting bytes is.
///
/// **Why 2 MiB.** Replaying real captures through the real seed path, at
/// 23x100 into a 2000-line grid, minimum of five runs -- retained lines, and
/// the parse those lines cost:
///
/// ```text
///                  worst session   parse (mean / worst)
///   1 MiB shipped      0 lines        16.1 / 20.3 ms
///   2 MiB             83 lines        28.7 / 38.5 ms
///   4 MiB            225 lines        55.8 / 86.0 ms
/// ```
///
/// The seed is synchronous on the attach path *and* on every `Ctrl-b Right`
/// switch, where the protocol round trip it sits beside is 0.3-6 ms at p50
/// and 13-22 ms at p95 (`attach_round_trip_latency`). 2 MiB buys every one of
/// those thirteen sessions a pager with real content in it for ~13 ms; 4 MiB
/// spends another ~27 ms on every switch anyone ever makes to take a single
/// pathological session from 83 rows of history to 225. That is not a trade
/// worth making, and 83 rows is already three and a half screens.
pub(crate) fn scrollback_seed_bytes() -> usize {
    (2 * 1024 * 1024).min(aplexer::DEFAULT_HISTORY_BYTES)
}

/// Prime a freshly built client model with a tail of the worker's retained
/// raw history, so `Ctrl-b [` has a past to page through from the first
/// second of the attach rather than only what arrives afterwards.
///
/// Failure is not an error the user should see: a worker too old to answer,
/// a session that just exited, or a history file that has been rotated all
/// mean "no history to seed", and the attach continues with an empty grid
/// that fills from the live stream.
pub(crate) fn seed_client_scrollback(
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    record: &SessionRecord,
) {
    if history_limit() == 0 {
        return;
    }
    let Ok(tail) = rpc_capture(record, Some(scrollback_seed_bytes())) else {
        return;
    };
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .seed_history(&tail);
}

/// Rebuild the pager's history from the worker's *current* retained tail
/// before `Ctrl-b [` (or the wheel) opens it.
///
/// The attach-time seed (`seed_client_scrollback`) only helps a client that
/// attached after the output happened. A session attached from before it --
/// `a new`, the default flow -- accumulates history through the live relay,
/// where `vt100` is right to drop every row scrolled out of a DECSTBM
/// sub-range; the codex TUI holds a sub-range almost constantly, so its pager
/// opened on `SCROLL 0/0` no matter how long it had been running. Re-running
/// the seed at entry gives the live attach the same past a fresh attach gets
/// (`ClientScreen::refresh_scrollback` for the model-side mechanics).
///
/// The gate keeps every other workload exactly as it is. `subregion_seen` is
/// false for anything that never sent a sub-range -- shells, logs, tail -f --
/// and those sessions' live-maintained history is complete, so they pay
/// neither the two bounded RPCs (capture + screen, ~13-40 ms of replay
/// measured at `scrollback_seed_bytes`) nor the risk of a byte-capped replay
/// holding fewer rows than their live scrollback already does. The alternate
/// screen skips too: that grid has no scrollback anywhere, so there is
/// nothing to rebuild for a full-screen application, only RPCs to spend.
///
/// The tail is fetched **whole** (`rpc_capture`'s `None`: everything the
/// worker still retains, bounded by `DEFAULT_HISTORY_BYTES`), not at the
/// 2 MiB attach-seed budget. A byte budget is not a row budget -- an agent
/// idling between turns spends its bytes on a spinner that replays to zero
/// rows -- so a budgeted tail can rebuild to *less* than the model already
/// holds, and `refresh_scrollback` rightly refuses to adopt it: the pager
/// would fossilize at whatever its entry happened to catch instead of
/// tracking the transcript. The whole buffer is the most any rebuild can
/// ever know; its parse is bounded by the worker's retention and runs once
/// per explicit pager entry, never on the attach or `Ctrl-b` switch paths
/// (`scrollback_seed_bytes` keeps those cheap). A refusal now costs the
/// replay alone, never the history.
///
/// Failure is invisible, exactly like the attach seed: a worker that has gone
/// away or stopped answering means "page through whatever the live model
/// has", which is the pre-refresh behavior.
pub(crate) fn refresh_pager_history(ctx: &StatusBarCtx) {
    if history_limit() == 0 {
        return;
    }
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        if screen.alternate_screen() || !screen.subregion_seen() {
            return;
        }
    }
    let Ok(tail) = rpc_capture(&record, None) else {
        return;
    };
    if tail.is_empty() {
        return;
    }
    let Ok(snapshot) = rpc_capture_screen(&record, false) else {
        return;
    };
    ctx.screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .refresh_scrollback(&tail, &snapshot);
}

/// Whether the client may borrow mouse reporting from the host terminal.
///
/// It has to, to see a wheel event at all: the host reports the wheel only
/// while some mouse protocol is enabled, and with `?1007l` in force nothing
/// else turns a wheel roll into anything. The cost is tmux's cost with
/// `mouse on` -- while the client owns the mouse, drag-to-select needs the
/// terminal's usual Shift override -- so `APLEXER_MOUSE=off` turns the
/// borrowing off and leaves `Ctrl-b [` as the way in.
pub(crate) fn mouse_capture_enabled() -> bool {
    !matches!(
        env::var("APLEXER_MOUSE").as_deref(),
        Ok("off") | Ok("0") | Ok("no") | Ok("false")
    )
}

/// The client's own mouse reporting: every protocol and encoding this client
/// knows about turned off, then button press/release (`?1000h`) in SGR
/// encoding (`?1006h`).
///
/// `?1000h` rather than `?1002h`/`?1003h` deliberately: press/release is all
/// a wheel needs, and not asking for motion reports keeps the terminal from
/// streaming a report per cell of mouse movement across the socket.
pub(crate) const CLIENT_MOUSE_ENABLE: &[u8] =
    b"\x1b[?9l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1000h\x1b[?1006h";

/// `CAN` -- "abandon any control sequence in flight". Leads every write made
/// under `BoundaryPolicy::StreamSuspended`; see that variant's doc comment
/// for why that is what makes those writes safe without the boundary gate.
pub(crate) const SCROLL_CANCEL: &[u8] = b"\x18";

/// Lines a wheel notch moves, matching tmux's own three.
pub(crate) const WHEEL_LINES: usize = 3;

/// SGR mouse button numbers for the wheel (xterm: 64 + button index).
pub(crate) const MOUSE_WHEEL_UP: u32 = 64;
pub(crate) const MOUSE_WHEEL_DOWN: u32 = 65;
