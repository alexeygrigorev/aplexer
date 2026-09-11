//! The `agent.state` vocabulary: the PTY-recency heuristic and how a fresh
//! `a state-report` push overrides it while it stays trustworthy.

use crate::*;

/// How long a session's PTY must stay silent before its derived
/// `agent.state` flips from `running` to `waiting`. Set to 4x POLL_INTERVAL
/// so a single late/missed poll tick, or output landing right at a poll
/// boundary, cannot flap the state back and forth -- a few polls of margin.
///
/// This is a COARSE, HONEST PROXY, not real agent-semantic-state detection.
/// spec.md section 20 explicitly defers true per-engine state derivation
/// (parsing claude/codex/gemini's own output, native agent state sources) to
/// future work; "PTY went quiet" also describes a long compute-bound step
/// with no terminal output, which this heuristic cannot tell apart from an
/// agent genuinely waiting on user input. Treat `waiting` as "no PTY output
/// recently", not as "the agent is idle".
pub(super) const ACTIVITY_THRESHOLD_MS: u64 = 3_000;

/// How long a `working`/`waiting` value pushed by `a state-report`
/// (docs/pocketshell-integration-plan.md Open question #2) stays
/// authoritative over the PTY-recency heuristic below, counted from
/// `SessionRecord::reported_state_at_ms`.
///
/// Merge rule (see `fresh_reported_state`): while a push is within this
/// window, it wins outright -- the PTY-recency heuristic does not run at
/// all for that poll. Once the window elapses with no fresh push to
/// refresh the timestamp, `derive_agent_state_with_source` falls straight
/// back to the heuristic, honest per-poll, with no separate "was it ever
/// pushed" memory. Terminal phases (`Phase::Exited`/`Phase::Failed`)
/// already bypass this branch entirely, which is what gives "or process
/// exit" from the design brief for free -- a dead workload's exit event
/// always wins over a stale push.
///
/// In ordinary operation this window rarely matters: `a init` installs a
/// hook firing at every stop/waiting/submit/start boundary (and, if a
/// future hook also fires on resume/tool-start, at every "back to work"
/// boundary too -- see `aplexer::hooks` for the installed per-engine
/// mapping), so `reported_state_at_ms` keeps refreshing well inside the
/// window. The window exists as a safety net for the case that motivates
/// "or process exit" in the first place: a hook process that reported once
/// and then the engine was killed, crashed, or the session was torn down
/// without a final hook firing to say so. Chosen at roughly 10x `a
/// watch`'s own poll interval so ordinary poll jitter cannot flap a
/// fresh push back to the heuristic mid-window.
///
/// `idle` deliberately does NOT use this elapsed-time window. A rest has
/// no follow-up push to refresh its timestamp -- the next hook fires only
/// when the agent works again -- so windowing `idle` made every resting
/// agent fall back to the heuristic after its turn
/// ended, and the heuristic's best guess for a quiet terminal
/// ("running" for a shell session) is exactly the "says working but
/// actually idle" failure. An `idle` push instead stays authoritative
/// until the PTY contradicts it; see `fresh_reported_state`.
pub(super) const REPORTED_STATE_STALE_MS: u64 = 8_000;

/// How much PTY output is allowed to land *after* an `idle` push without
/// contradicting it, counted from `reported_state_at_ms`. A Stop hook
/// fires right as the turn's last render completes, and the hook process
/// (`sh -c` + a client RPC) can lose that race by a beat: tail output
/// stamped a few hundred ms after the push is still the turn ending, not
/// the agent (or the user at a prompt) doing something new. Anything
/// stamped later than this grace means real activity happened after the
/// agent said it was resting, and the push stops being authoritative --
/// the heuristic takes over and says `running`/`waiting` from actual PTY
/// recency. (This is PocketShell issue #1570's "a resting push is
/// invalidated the moment newer PTY activity appears", with a grace so
/// the invalidation cannot be tripped by the very output that ended the
/// turn.)
pub(super) const IDLE_ACTIVITY_GRACE_MS: u64 = 2_000;

/// Reads a fresh `a state-report` push off `record` and maps it onto `a
/// watch`'s wire vocabulary, or `None` when there is nothing to trust (no
/// push ever recorded, a `working`/`waiting` push older than
/// `REPORTED_STATE_STALE_MS`, or an `idle` push retracted by newer PTY
/// output).
///
/// `idle` and `waiting` map onto themselves -- `idle` is a genuinely new
/// wire value this feature introduces (the PTY-recency heuristic cannot
/// tell "resting, nothing to do" apart from "blocked on a question", so it
/// never emitted `idle` before; see the doc comment this replaces).
/// `working` folds onto the heuristic's existing `running` value rather
/// than adding a second word for the same idea -- any consumer that
/// already understands the heuristic's `running` handles a *reported*
/// `working` push for free, and the two genuinely mean the same thing
/// (actively producing/thinking).
pub(super) fn fresh_reported_state(record: &SessionRecord, now: u64) -> Option<&'static str> {
    let state = record.reported_state.as_deref()?;
    let at = record.reported_state_at_ms?;
    match state {
        // An `idle` push is not windowed by the clock but by the PTY: it
        // describes "the agent finished its turn and is resting", a fact
        // that stays true -- however long the rest -- until the terminal
        // sees new output (the agent working again, or the user typing at
        // a prompt). Output stamped beyond IDLE_ACTIVITY_GRACE_MS after
        // the push retracts it; the heuristic then speaks from real PTY
        // recency. Elapsed time alone must not expire the push: unlike
        // working/waiting there is no follow-up push to refresh it, and
        // letting go of it mid-rest is what made resting sessions read
        // RUNNING.
        "idle" => match record.last_activity_ms {
            Some(ts) if ts > at.saturating_add(IDLE_ACTIVITY_GRACE_MS) => None,
            _ => Some("idle"),
        },
        "waiting" => {
            if now.saturating_sub(at) > REPORTED_STATE_STALE_MS {
                None
            } else {
                Some("waiting")
            }
        }
        "working" => {
            if now.saturating_sub(at) > REPORTED_STATE_STALE_MS {
                None
            } else {
                Some("running")
            }
        }
        // Defensive only: the worker validates every write
        // (WorkerRuntime::report_state), so this arm only fires against a
        // foreign/hand-edited session.json. Fall back to the heuristic
        // rather than propagate an unrecognised value into the stream.
        _ => None,
    }
}

/// `starting/running/waiting/idle/exited/oom/error/unknown` is spec.md
/// section 20's full vocabulary. `starting`, the PTY-recency
/// `running`/`waiting` (see ACTIVITY_THRESHOLD_MS), and the terminal
/// `exited`/`oom`/`error` are the original v1 proxy's output, unchanged.
/// `idle` -- and an authoritative rather than guessed `running`/`waiting`
/// -- now also come from a fresh `a state-report` push
/// (`fresh_reported_state`), which is checked first and, while fresh,
/// replaces the heuristic outright rather than merely tie-breaking it.
/// `unknown` is still not emitted (no source ever produces it).
///
/// Returns `(state, source)`; `source` is `"reported"` when a fresh push
/// won, `"heuristic"` otherwise, surfaced on the `agent.state` event as
/// `metadata.state_source` so a consumer can tell which is authoritative
/// without hard-coding the staleness window itself.
pub fn derive_agent_state_with_source(
    record: &SessionRecord,
    now: u64,
) -> (&'static str, &'static str) {
    match record.phase {
        Phase::Starting => ("starting", "heuristic"),
        Phase::Running | Phase::Exiting => {
            if let Some(state) = fresh_reported_state(record, now) {
                return (state, "reported");
            }
            let state = match record.last_activity_ms {
                Some(ts) if now.saturating_sub(ts) < ACTIVITY_THRESHOLD_MS => "running",
                Some(_) => "waiting",
                // No PTY output observed yet (e.g. worker just flipped to
                // Running but the periodic activity-persist tick hasn't
                // fired): assume running rather than waiting, since the
                // session just started and there is no evidence yet of it
                // going quiet.
                None => "running",
            };
            (state, "heuristic")
        }
        Phase::Exited => {
            let state = if record.exit.as_ref().map(|e| e.oom_killed).unwrap_or(false) {
                "oom"
            } else {
                "exited"
            };
            (state, "heuristic")
        }
        Phase::Failed => ("error", "heuristic"),
    }
}

pub(super) fn derive_agent_state(record: &SessionRecord, now: u64) -> &'static str {
    derive_agent_state_with_source(record, now).0
}
