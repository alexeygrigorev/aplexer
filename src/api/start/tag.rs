//! Tag allocation for `--fresh` starts.

use super::*;

/// The tag a `--fresh` start should claim: the requested base itself while
/// nothing live holds it, otherwise the first `<base>-2`, `<base>-3`, …
/// suffix no live session holds either. "Live" here means exactly what the
/// supersede check in `start_session` refuses to take -- a holder
/// `reap_verdict` would hand over does not count, so a dead `main-2` is
/// reclaimed under its own name rather than skipped. `None` means no
/// candidate fits `validate_tag` any more, which for a valid base can only
/// be the 64-byte length cap.
pub fn pick_fresh_tag(records: &[SessionRecord], workspace: &Path, base: &str) -> Option<String> {
    // Any live holder counts, not just the first record with the pair: a
    // rename that took a dead holder's name leaves the corpse next to the
    // live session (issue #13), and `--fresh` must read that pair as taken.
    let live_holder = |tag: &str| {
        records
            .iter()
            .filter(|r| r.workspace == workspace && r.tag == tag)
            .any(|r| crate::reap_verdict(r).is_none())
    };
    // Suffixes start at 2: a bare `main` plus `main-2` reads as "the main
    // one and its first sibling", not as an off-by-one list. A base that
    // already ends in `-<number>` (or cannot be suffixed numerically at all)
    // simply continues from the next integer.
    let mut candidate = base.to_string();
    while live_holder(&candidate) {
        candidate = match candidate.rsplit_once('-').and_then(|(stem, n)| {
            let next = n.parse::<u64>().ok()?.checked_add(1)?;
            Some(format!("{stem}-{next}"))
        }) {
            Some(next) => next,
            None => format!("{base}-2"),
        };
        if validate_tag(&candidate).is_err() {
            return None;
        }
    }
    Some(candidate)
}

#[cfg(test)]
mod fresh_tag_tests {
    use super::*;

    /// A record liveness is decided by pid probes (`reap_verdict`), so a
    /// "live" holder only needs a pid that exists -- the test process's own
    /// -- and a reclaimable one needs no pids plus an empty-containment
    /// shape, exactly like `mod reclaim_tests`' zombie fixture.
    fn record(workspace: &str, tag: &str, worker_pid: Option<u32>) -> SessionRecord {
        let mut record = SessionRecord::fixture(workspace, tag);
        record.worker_pid = worker_pid;
        record
    }

    fn live(workspace: &str, tag: &str) -> SessionRecord {
        record(workspace, tag, Some(std::process::id()))
    }

    fn dead(workspace: &str, tag: &str) -> SessionRecord {
        record(workspace, tag, None)
    }

    #[test]
    fn free_base_is_used_verbatim() {
        let ws = Path::new("/ws");
        assert_eq!(pick_fresh_tag(&[], ws, "main"), Some("main".into()));
    }

    #[test]
    fn live_base_moves_to_the_next_free_suffix() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-2".into()));
    }

    #[test]
    fn suffix_walk_skips_taken_numbers() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main"), live("/ws", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-3".into()));
    }

    #[test]
    fn reclaimable_holder_keeps_the_requested_tag() {
        // A dead `main` is not "someone else's session": the ordinary
        // reclaim path takes the exact name, so `--fresh` must not skip it.
        let ws = Path::new("/ws");
        let records = vec![dead("/ws", "main")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main".into()));
    }

    #[test]
    fn reclaimable_suffix_is_taken_under_its_own_name() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main"), dead("/ws", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-2".into()));
    }

    #[test]
    fn other_workspaces_do_not_count() {
        let ws = Path::new("/ws");
        let records = vec![live("/elsewhere", "main"), live("/elsewhere", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main".into()));
    }

    #[test]
    fn base_with_trailing_number_increments_from_it() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "review-2")];
        assert_eq!(
            pick_fresh_tag(&records, ws, "review-2"),
            Some("review-3".into())
        );
    }

    #[test]
    fn saturated_numeric_suffix_restarts_from_the_base() {
        // `n + 1` on a parsed u64 suffix overflowed for `x-18446744073709551615`;
        // an unsuffixable candidate falls back to `<base>-2` like any other.
        let ws = Path::new("/ws");
        let base = format!("x-{}", u64::MAX);
        let records = vec![live("/ws", &base)];
        assert_eq!(
            pick_fresh_tag(&records, ws, &base),
            Some(format!("{base}-2"))
        );
    }

    #[test]
    fn length_capped_base_reports_no_candidate() {
        let ws = Path::new("/ws");
        let base = "a".repeat(64);
        let records = vec![live("/ws", &base)];
        assert_eq!(pick_fresh_tag(&records, ws, &base), None);
    }
}
