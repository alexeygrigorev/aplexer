// -- The structural guard --------------------------------------------
//
// Issue #14 was a missed *caller*, not a subtle race, and the next one
// will be too: someone adds a writer, does not know the gate exists,
// and no test notices. So the rule is enforced over the source text --
// every function in this file that can put bytes on the host terminal
// is enumerated here, with how it is allowed to do so.

/// Every function in the `src/bin/a/` production modules that calls
/// `write_all` -- i.e. that reaches the terminal without going through
/// the funnel -- and the reason it is allowed to. A new one fails
/// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`.
const RAW_TERMINAL_WRITERS: &[(&str, &str)] = &[
    (
        "write_client_locked",
        "the gate itself: it performs the boundary check it is named for",
    ),
    (
        "write_locked",
        "attach start and detach only, pinned by WRITE_LOCKED_CALLERS below -- there is \
             no relayed stream to splice before the first workload byte or after the last, \
             and neither may be deferrable",
    ),
    (
        "relay_to_terminal",
        "relays the workload's own bytes; it *is* the stream, not an injection",
    ),
    (
        "feed_and_write",
        "the attach snapshot and a switch's replayed screen: a full repaint that replaces \
             the stream rather than splicing into it, fed to the model under the same lock",
    ),
    (
        "cmd_capture",
        "`a capture` on a plain stdout; no attach, no relayed stream",
    ),
];

/// Every client-originated injection into a live relayed stream. Each
/// goes through `write_client_locked`, so each is boundary-gated by
/// construction rather than by remembering. Listed so the census is
/// visible: `apply_terminal_layout` was the ninth writer that nobody had
/// written down (issue #14).
const FUNNELLED_WRITERS: &[&str] = &[
    "apply_terminal_layout_to",
    "draw_status_bar",
    "redraw_live_screen",
    "paint_scroll_view",
    "refresh_scroll_bar",
    "paint_live_screen",
    "sync_client_mouse",
    "paint_key_overlay",
    "dismiss_key_overlay",
];

/// `write_locked` writes unconditionally, so it is a second route to the
/// terminal and would be a hole in the funnel if it could be called from
/// anywhere. These are the only two places allowed to.
const WRITE_LOCKED_CALLERS: &[&str] = &["attach", "reset_terminal"];

/// The production source slices, in the same order that `app.rs` includes
/// them. Compiled in, so the write census checks the exact source used by the
/// binary rather than a hand-maintained copy.
const PRODUCTION_SOURCE: &str = concat!(
    include_str!("../a/cli.rs"),
    "\n",
    include_str!("../a/commands.rs"),
    "\n",
    include_str!("../a/capture_commands.rs"),
    "\n",
    include_str!("../a/list_plain.rs"),
    "\n",
    include_str!("../a/list_tty.rs"),
    "\n",
    include_str!("../a/list_helpers.rs"),
    "\n",
    include_str!("../a/lifecycle_commands.rs"),
    "\n",
    include_str!("../a/session_commands.rs"),
    "\n",
    include_str!("../a/session_diagnostics.rs"),
    "\n",
    include_str!("../a/diagnostics.rs"),
    "\n",
    include_str!("../a/doctor.rs"),
    "\n",
    include_str!("../a/message_commands.rs"),
    "\n",
    include_str!("../a/mouse.rs"),
    "\n",
    include_str!("../a/rpc.rs"),
    "\n",
    include_str!("../a/rpc_connection.rs"),
    "\n",
    include_str!("../a/terminal.rs"),
    "\n",
    include_str!("../a/terminal_status.rs"),
    "\n",
    include_str!("../a/status_bar.rs"),
    "\n",
    include_str!("../a/scroll.rs"),
    "\n",
    include_str!("../a/scroll_config.rs"),
    "\n",
    include_str!("../a/scroll_input.rs"),
    "\n",
    include_str!("../a/status_commands.rs"),
    "\n",
    include_str!("../a/switch_targets.rs"),
    "\n",
    include_str!("../a/switching.rs"),
    "\n",
    include_str!("../a/input_scanner.rs"),
    "\n",
    include_str!("../a/key_overlay.rs"),
    "\n",
    include_str!("../a/launch_commands.rs"),
    "\n",
    include_str!("../a/attach.rs"),
    "\n",
    include_str!("../a/attach_input.rs"),
    "\n",
    include_str!("../a/attach_protocol.rs"),
    "\n",
    include_str!("../a/attach_session.rs"),
    "\n",
    include_str!("../a/attach_threads.rs"),
    "\n",
    include_str!("../a/system.rs"),
);

fn production_source_lines() -> Vec<&'static str> {
    PRODUCTION_SOURCE.lines().collect()
}

/// Maps each line matching `needle` to the name of the nearest
/// preceding `fn` declaration. Comment lines are skipped so a doc
/// comment mentioning a call is not mistaken for one.
fn enclosing_fns_of(lines: &[&str], needle: &str) -> Vec<(String, String)> {
    let mut current = String::new();
    let mut hits = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        for prefix in ["fn ", "pub fn ", "pub(crate) fn ", "unsafe fn "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                current = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                break;
            }
        }
        if trimmed.contains(needle) {
            hits.push((current.clone(), trimmed.to_string()));
        }
    }
    hits
}

/// The body of a top-level `fn`, from its declaration to the `}` that
/// closes it at column 0.
fn top_level_fn_body(lines: &[&str], name: &str) -> String {
    let private_decl = format!("fn {name}(");
    let crate_decl = format!("pub(crate) fn {name}(");
    let start = lines
        .iter()
        .position(|l| l.starts_with(&private_decl) || l.starts_with(&crate_decl))
        .unwrap_or_else(|| panic!("no top-level `fn {name}` in the production source slices"));
    let end = start
        + 1
        + lines[start + 1..]
            .iter()
            .position(|l| *l == "}")
            .unwrap_or_else(|| panic!("`fn {name}` is not closed at column 0"));
    lines[start..=end].join("\n")
}

/// The criterion the issue calls the most valuable one: a *new* ungated
/// writer fails here, instead of shipping and being found by a reviewer
/// reading the file (which is how #14 was found, after #5 declared the
/// class fixed).
///
/// Four things are pinned:
///
/// 1. the exact set of functions that can write to the terminal;
/// 2. that each one either goes through `write_client_locked` or carries
///    a written reason why it is not an injection;
/// 3. that `write_client_locked` really is the boundary check, and that
///    `write_locked` -- the unconditional second route -- is reachable
///    only from attach start and detach;
/// 4. that the deadline exemption stays a single call site.
#[test]
fn every_client_terminal_write_site_is_gated_or_explicitly_exempt() {
    use std::collections::BTreeSet;

    let lines = production_source_lines();

    // 1. Nobody new may reach `write_all` directly.
    let raw: BTreeSet<String> = enclosing_fns_of(&lines, "write_all")
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    let declared_raw: BTreeSet<String> = RAW_TERMINAL_WRITERS
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    let undeclared: Vec<&String> = raw.difference(&declared_raw).collect();
    assert!(
        undeclared.is_empty(),
        "new raw write(s) to the host terminal, not listed in RAW_TERMINAL_WRITERS: \
             {undeclared:?}. Client-originated bytes must go through `write_client_locked` \
             (the escape-boundary gate) instead; if the write genuinely cannot splice a \
             relayed stream, add it to RAW_TERMINAL_WRITERS with that reason. This is issue \
             #14: an ungated writer corrupts a workload's half-emitted escape sequences."
    );
    let stale: Vec<&String> = declared_raw.difference(&raw).collect();
    assert!(
        stale.is_empty(),
        "RAW_TERMINAL_WRITERS lists function(s) that no longer call write_all: {stale:?}. \
             Drop them, so the list stays an accurate census rather than folklore."
    );

    // 2. The injection census: gated by construction, but written down,
    //    because #14 was a writer nobody had written down.
    let funnelled: BTreeSet<String> = enclosing_fns_of(&lines, "write_client_locked(")
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| f != "write_client_locked")
        .collect();
    let declared_funnelled: BTreeSet<String> =
        FUNNELLED_WRITERS.iter().map(|n| (*n).to_string()).collect();
    assert_eq!(
        funnelled, declared_funnelled,
        "the set of client-originated injections changed. A new one is already \
             boundary-gated (that is what `write_client_locked` is for) -- add its name to \
             FUNNELLED_WRITERS so the census stays true, and check that it parks a refused \
             write for a later boundary instead of dropping it."
    );
    for name in FUNNELLED_WRITERS {
        let body = top_level_fn_body(&lines, name);
        assert!(
            body.contains("write_client_locked("),
            "`{name}` no longer goes through `write_client_locked`, so its bytes are not \
                 boundary-gated (issue #14)"
        );
    }

    let gate = top_level_fn_body(&lines, "write_client_locked");
    assert!(
        gate.contains("at_escape_boundary()"),
        "`write_client_locked` must be the escape-boundary check; every Funnelled writer \
             above relies on it being one"
    );

    let raw_callers: Vec<String> = enclosing_fns_of(&lines, "write_locked(")
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| f != "write_locked" && f != "write_client_locked")
        .collect();
    for caller in &raw_callers {
        assert!(
            WRITE_LOCKED_CALLERS.contains(&caller.as_str()),
            "`{caller}` calls `write_locked`, which writes without consulting the escape \
                 boundary. Only attach start and detach may (there is no relayed stream to \
                 splice at either); a live injection must use `write_client_locked`."
        );
    }

    let past_deadline: Vec<(String, String)> =
        enclosing_fns_of(&lines, "BoundaryPolicy::PastDeadline")
            .into_iter()
            .filter(|(f, _)| f != "write_client_locked")
            .collect();
    assert_eq!(
            past_deadline.len(),
            1,
            "`BoundaryPolicy::PastDeadline` is the client's only exemption from the boundary \
             gate and must stay one narrow call site (the resize deadline), found: {past_deadline:?}"
        );
    assert_eq!(
        past_deadline[0].0, "apply_terminal_layout_to",
        "the deadline exemption belongs to the resize path and nothing else"
    );
}

/// `BoundaryPolicy::StreamSuspended` says "the relay is not writing to
/// the host at all right now", which is true of exactly one class of
/// thing: a *client modal* that has taken the host terminal away from the
/// relay entirely -- the scroll-mode pager, and the `Ctrl-b` key overlay,
/// which suspends the relay the same way and for the same reason (see
/// `KeyOverlay`). Pinned the same way the deadline exemption is, so it
/// cannot quietly become a general-purpose way around the gate.
///
/// Two properties, both of which the variant's correctness rests on:
/// only a modal's writers may pass it, and every write that is the *first*
/// one after the suspension must lead with `SCROLL_CANCEL` -- the `CAN`
/// that ends whatever sequence the host was part-way through when the
/// relay was suspended.
#[test]
fn scroll_mode_writes_are_the_only_stream_suspended_ones() {
    use std::collections::BTreeSet;

    /// Every function allowed to write while the relay is suspended, and
    /// where its `SCROLL_CANCEL` prefix comes from.
    const SUSPENDED_WRITERS: &[(&str, bool)] = &[
        // (name, must build its own SCROLL_CANCEL-led sequence)
        ("paint_scroll_view", true),
        ("paint_live_screen", true),
        // The overlay's two frames. Either can be the first write after
        // the relay was suspended -- `paint_key_overlay` always is, and
        // `dismiss_key_overlay` is whenever nothing was repainted in
        // between (a resize, say) -- so both build their own.
        ("paint_key_overlay", true),
        ("dismiss_key_overlay", true),
        // The bar row is drawn *into* a screen the pager already owns
        // and already cancelled; it is not the first write after the
        // suspension, so it needs no CAN of its own.
        ("refresh_scroll_bar", false),
        // Hands the mouse over while the pager is up; same reasoning.
        ("sync_client_mouse", false),
    ];

    let lines = production_source_lines();
    let found: BTreeSet<String> = enclosing_fns_of(&lines, "BoundaryPolicy::StreamSuspended")
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| f != "write_client_locked")
        .collect();
    let declared: BTreeSet<String> = SUSPENDED_WRITERS
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    assert_eq!(
        found, declared,
        "`BoundaryPolicy::StreamSuspended` bypasses the escape-boundary gate on the \
             grounds that scroll mode has suspended the relay entirely. Only scroll mode may \
             claim that. If a new writer genuinely runs with the relay suspended, add it here \
             with whether it must lead with SCROLL_CANCEL; otherwise use BoundaryPolicy::Defer."
    );
    for (name, needs_cancel) in SUSPENDED_WRITERS {
        if !needs_cancel {
            continue;
        }
        let body = top_level_fn_body(&lines, name);
        assert!(
            body.contains("SCROLL_CANCEL"),
            "`{name}` writes the first bytes after the relay is suspended, so it must lead \
                 with SCROLL_CANCEL (CAN) to end whatever escape sequence the host was \
                 part-way through -- that prefix is what makes skipping the boundary gate safe"
        );
    }
}
