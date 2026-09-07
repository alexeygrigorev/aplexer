//! Process cgroup placement: recording, classification, and the per-user
//! systemd manager failure domain (issue #1).
//!
//! On 2026-08-27 a per-user systemd manager entered `exit.target` and killed
//! every aplexer worker and workload living beneath `user@1000.service` in
//! one stroke, while unlimited workers launched from plain SSH (which live in
//! logind's `session-*.scope`, a sibling of `user@.service` under the user
//! slice) survived. This module is the vocabulary for that failure domain:
//!
//! * [`read_process_cgroup`] records where a process actually is, straight
//!   from `/proc/<pid>/cgroup` -- the durable evidence the issue asks the
//!   session record to carry, because after the processes die, the exact
//!   causation can no longer be proven retrospectively (the issue's `yolo`
//!   session).
//! * [`classify_cgroup_path`] says what that placement means for survival of
//!   a `systemctl --user exit`.
//!
//! `setsid()` is *not* an escape from any of this: it creates a new session
//! and detaches from the controlling terminal, but the calling process stays
//! exactly where it was in the cgroup hierarchy and therefore stays inside
//! whichever service manager owns that subtree. Session separation is not
//! cgroup or manager durability; spec.md section 7.4 now says so too.

use crate::SessionRecord;
use serde_json::json;
use std::fs;

/// Opt-in switch for the launch path to escape the per-user manager by
/// placing the worker (and resource-limited workloads) in a *system*
/// manager scope via `systemd-run --system --scope`. Set to `system` to
/// request it; anything else keeps the default ambient placement. See
/// `system_scope_requested` and `probe_system_scope_backend` in lib.rs,
/// plus README's "Worker placement" section.
pub const LAUNCH_SYSTEM_SCOPE_ENV: &str = "APLEXER_LAUNCH_SYSTEM_SCOPE";

/// The value of [`LAUNCH_SYSTEM_SCOPE_ENV`] that requests the escape.
pub const LAUNCH_SYSTEM_SCOPE_VALUE: &str = "system";

/// Whether the ambient process explicitly opted into the system-scope
/// escape. Read once per launch by the `a start` client (worker placement)
/// and by `Cgroup::create` (workload scope placement), so both ends of one
/// launch answer the same way without new plumbing between them.
pub fn system_scope_requested() -> bool {
    std::env::var(LAUNCH_SYSTEM_SCOPE_ENV).as_deref() == Ok(LAUNCH_SYSTEM_SCOPE_VALUE)
}

/// Where a process sits in the cgroup hierarchy, as far as that predicts
/// survival of a per-user systemd manager exit (issue #1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupPlacement {
    /// The root cgroup (`/`) or systemd's own `init.scope`: owned by the
    /// init/system manager itself, not by any login or user manager.
    InitOwned,
    /// A logind login-session scope
    /// (`/user.slice/user-<uid>.slice/session-<n>.scope`). Owned by the
    /// system manager: it survives `systemctl --user exit` (this is where
    /// the issue's surviving SSH-launched workers lived), but logind can
    /// reap it on session end when `KillUserProcesses=yes`.
    LoginSession,
    /// Beneath `user@<uid>.service`: the per-user manager's own subtree.
    /// Killed wholesale when that manager enters `exit.target` -- the
    /// failure domain of issue #1.
    UserManager,
    /// A system-manager service domain (`/system.slice/...`): not affected
    /// by any per-user manager exit.
    SystemSlice,
    /// A container runtime's subtree (docker, LXC, systemd-nspawn's
    /// `machine.slice`, kubernetes/crio/containerd pods).
    Container,
    /// A hierarchy location this classifier does not name. Reported
    /// honestly rather than guessed: an unknown placement must not read as
    /// safe.
    Unknown,
}

impl CgroupPlacement {
    /// Stable machine-readable name, the wire form `a status --json` /
    /// `a list --json` rows carry in `worker_placement.placement`.
    pub fn name(&self) -> &'static str {
        match self {
            CgroupPlacement::InitOwned => "init_owned",
            CgroupPlacement::LoginSession => "login_session",
            CgroupPlacement::UserManager => "user_manager",
            CgroupPlacement::SystemSlice => "system_slice",
            CgroupPlacement::Container => "container",
            CgroupPlacement::Unknown => "unknown",
        }
    }

    /// Whether a process here dies when the per-user systemd manager enters
    /// `exit.target`. Exactly one placement is: the user manager's own
    /// subtree. Login sessions are deliberately *not* flagged vulnerable to
    /// this specific failure -- the incident proved SSH session scopes
    /// survive -- but see [`CgroupPlacement::LoginSession`] for the separate
    /// `KillUserProcesses` caveat doctor mentions.
    pub fn vulnerable_to_user_manager_exit(&self) -> bool {
        matches!(self, CgroupPlacement::UserManager)
    }

    /// One line of human advice for a placement, as `a doctor` and the
    /// `a start` warning render it. `None` for placements with nothing to
    /// act on.
    pub fn advice(&self) -> Option<&'static str> {
        match self {
            CgroupPlacement::UserManager => Some(
                "start aplexer from a login session (SSH or console) instead of inside a \
                 user service/scope, or set APLEXER_LAUNCH_SYSTEM_SCOPE=system to launch \
                 workers in a system-manager scope",
            ),
            CgroupPlacement::LoginSession => Some(
                "survives `systemctl --user exit`, but `KillUserProcesses=yes` hosts reap \
                 login-session scopes at logout; a system scope \
                 (APLEXER_LAUNCH_SYSTEM_SCOPE=system) is immune to both",
            ),
            _ => None,
        }
    }
}

/// Extract the cgroup-v2 (unified hierarchy) path from the body of a
/// `/proc/<pid>/cgroup` file. The v2 line is the one whose hierarchy-id
/// fields are `0::`; older v1-only files have no such line (and a process
/// can only be moved into a cgroup namespace where that is the whole story).
/// Returns the path with its leading slash, e.g.
/// `/user.slice/user-1000.slice/session-8.scope`.
pub fn parse_cgroup_v2_path(cgroup_file_text: &str) -> Option<String> {
    cgroup_file_text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_string())
        .filter(|path| path.starts_with('/'))
}

/// Best-effort read of a live process's cgroup-v2 path. `None` when the
/// process is gone, `/proc` is unreadable, or the file has no v2 line --
/// recording nothing beats recording a guess (issue #1's post-mortem could
/// not even prove causation because the dead processes' cgroups were gone).
pub fn read_process_cgroup(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_cgroup_v2_path(&text)
}

/// [`read_process_cgroup`] plus classification, for launch-time validation.
pub fn classify_process(pid: u32) -> Option<CgroupPlacement> {
    read_process_cgroup(pid)
        .as_deref()
        .map(classify_cgroup_path)
}

/// Classify a cgroup-v2 path (the `/proc/<pid>/cgroup` `0::` form).
///
/// The distinction that matters is *which service manager owns the subtree*:
///
/// * `user@<uid>.service` anywhere in the path means the per-user manager
///   owns it -- vulnerable (issue #1).
/// * `session-<n>.scope` directly under `user-<uid>.slice` (no
///   `user@.service` component) is logind's own login-session scope,
///   managed by the *system* manager.
/// * `/system.slice/...` is system services; the root cgroup and
///   `init.scope` are init's own.
/// * Container runtimes get their own name so a containerized aplexer is
///   not misread as init-owned (containers have their own private manager
///   topologies; honest naming beats false comfort).
pub fn classify_cgroup_path(path: &str) -> CgroupPlacement {
    let path = path.trim();
    if path == "/" {
        return CgroupPlacement::InitOwned;
    }
    if path == "/init.scope" {
        return CgroupPlacement::InitOwned;
    }
    // A single-component unit directory at the hierarchy root (e.g.
    // `/aplexer-worker-<id>.scope`) is owned by whatever manager owns that
    // root -- the system manager on a normal host, which is exactly where
    // `systemd-run --system --scope` places transient units when it works.
    if path
        .split('/')
        .filter(|component| !component.is_empty())
        .count()
        == 1
        && (path.ends_with(".scope") || path.ends_with(".service"))
    {
        return CgroupPlacement::SystemSlice;
    }
    if path == "/user.slice" || path == "/system.slice" {
        // A bare slice directory with no member subtree names no manager
        // relationship precisely enough to claim one.
        return CgroupPlacement::Unknown;
    }
    if path.starts_with("/system.slice/") {
        return CgroupPlacement::SystemSlice;
    }
    for container in [
        "/docker/",
        "/lxc/",
        "/machine.slice/",
        "/kubepods",
        "/crio-containers",
        "/systemd-container/",
    ] {
        if path.starts_with(container) {
            return CgroupPlacement::Container;
        }
    }
    if path.starts_with("/user.slice/") {
        let user_manager = path
            .split('/')
            .any(|component| component.starts_with("user@") && component.ends_with(".service"));
        if user_manager {
            return CgroupPlacement::UserManager;
        }
        let login_session = path
            .split('/')
            .any(|component| component.starts_with("session-") && component.ends_with(".scope"));
        if login_session {
            return CgroupPlacement::LoginSession;
        }
        // Something else inside the user slice (a hand-made delegation
        // subtree, user-<uid>.slice directly, ...): do not guess.
        return CgroupPlacement::Unknown;
    }
    CgroupPlacement::Unknown
}

/// The derived placement facts `a status --json` / `a list --json` /
/// `a snapshot --json` rows carry next to the recorded cgroup path, so a
/// machine consumer never has to re-implement this classification (and can
/// never disagree with `a doctor` about what is vulnerable).
pub fn placement_summary(cgroup: Option<&str>) -> serde_json::Value {
    let placement = cgroup.map(classify_cgroup_path);
    json!({
        "cgroup": cgroup,
        "placement": placement.map(|p| p.name()),
        "vulnerable_to_user_manager_exit": placement
            .map(|p| p.vulnerable_to_user_manager_exit())
            .unwrap_or(false),
    })
}

/// The one-line warning `a start` prints when the freshly started session's
/// worker landed in the issue #1 failure domain. `None` when the worker's
/// placement is unknown (never warn on absent data) or not vulnerable --
/// the issue asks aplexer to warn clearly, not to fail, and a session that
/// is running fine should not be talked down.
pub fn start_placement_warning(record: &SessionRecord) -> Option<String> {
    let cgroup = record.worker_cgroup.as_deref()?;
    let placement = classify_cgroup_path(cgroup);
    if !placement.vulnerable_to_user_manager_exit() {
        return None;
    }
    let advice = placement
        .advice()
        .unwrap_or("see `a doctor` for placement advice");
    Some(format!(
        "warning: session {} worker is in cgroup {cgroup}, beneath the per-user systemd \
         manager (user@<uid>.service): `systemctl --user exit` (e.g. at logout) will kill \
         this session's worker and workload together; {advice}",
        record.id
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-shaped `/proc/<pid>/cgroup` bodies, including a v1+v2 hybrid
    /// (controllers pinned per-line) and a body with no v2 line at all.
    #[test]
    fn parse_takes_the_unified_hierarchy_line() {
        assert_eq!(
            parse_cgroup_v2_path("0::/user.slice/user-1000.slice/session-2976.scope\n"),
            Some("/user.slice/user-1000.slice/session-2976.scope".to_string())
        );
        // A hybrid file: the `0::` line is the v2 answer, everything else is
        // legacy controllers that name nothing we classify.
        assert_eq!(
            parse_cgroup_v2_path(
                "12:pids:/system.slice/dockerd.service\n11:cpu:/docker/8f2\n0::/init.scope\n"
            ),
            Some("/init.scope".to_string())
        );
        // A pure cgroup v1 host has no unified line: nothing to record.
        assert_eq!(
            parse_cgroup_v2_path("12:pids:/system.slice/x.service\n"),
            None
        );
        assert_eq!(parse_cgroup_v2_path(""), None);
        // Trailing whitespace is file formatting, not path.
        assert_eq!(
            parse_cgroup_v2_path("0::/system.slice/foo.service  \n"),
            Some("/system.slice/foo.service".to_string())
        );
        // A malformed v2 line (no leading slash) is refused, not trimmed
        // into something that looks authoritative.
        assert_eq!(parse_cgroup_v2_path("0::relative/path\n"), None);
    }

    /// The four placements the issue actually distinguishes, with real
    /// incident-shaped inputs: the surviving SSH workers (login session),
    /// the killed user-manager subtree, plain system services, and the
    /// init/root domains.
    #[test]
    fn classify_names_the_owner_of_each_real_shape() {
        // The incident's survivors: plain-SSH launches.
        assert_eq!(
            classify_cgroup_path("/user.slice/user-1000.slice/session-3380267.scope"),
            CgroupPlacement::LoginSession
        );
        // The incident's victims: everything beneath user@1000.service,
        // at every depth (scopes, services, slices beneath it).
        assert_eq!(
            classify_cgroup_path("/user.slice/user-1000.slice/user@1000.service"),
            CgroupPlacement::UserManager
        );
        assert_eq!(
            classify_cgroup_path(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/aplexer-workload-….scope"
            ),
            CgroupPlacement::UserManager
        );
        assert_eq!(
            classify_cgroup_path("/user.slice/user-1000.slice/user@1000.service/session-2.scope"),
            CgroupPlacement::UserManager,
            "a session scope *beneath* user@.service is still the user manager's"
        );
        // System manager domains.
        assert_eq!(
            classify_cgroup_path("/system.slice/nginx.service"),
            CgroupPlacement::SystemSlice
        );
        assert_eq!(
            classify_cgroup_path("/system.slice/system-getty.slice"),
            CgroupPlacement::SystemSlice
        );
        // A transient unit dropped straight at the hierarchy root is the
        // system manager's shape (systemd-run --system --scope).
        assert_eq!(
            classify_cgroup_path("/aplexer-worker-019d4d1f.scope"),
            CgroupPlacement::SystemSlice
        );
        // Init-owned domains.
        assert_eq!(classify_cgroup_path("/"), CgroupPlacement::InitOwned);
        assert_eq!(
            classify_cgroup_path("/init.scope"),
            CgroupPlacement::InitOwned
        );
        // Container runtimes: named, not misread as safe.
        assert_eq!(
            classify_cgroup_path("/docker/8f2e01abcd"),
            CgroupPlacement::Container
        );
        assert_eq!(
            classify_cgroup_path("/lxc/1234"),
            CgroupPlacement::Container
        );
        assert_eq!(
            classify_cgroup_path("/machine.slice/machine-qemu\x2dwin.scope/libvirt"),
            CgroupPlacement::Container
        );
        assert_eq!(
            classify_cgroup_path("/kubepods.slice/kubepods-burstable.slice/xyz"),
            CgroupPlacement::Container
        );
        // Honest unknowns: a bare slice names no member manager, and an
        // unrecognised subtree must not read as safe.
        assert_eq!(
            classify_cgroup_path("/user.slice"),
            CgroupPlacement::Unknown
        );
        assert_eq!(
            classify_cgroup_path("/user.slice/user-1000.slice"),
            CgroupPlacement::Unknown
        );
        assert_eq!(
            classify_cgroup_path("/somewhere.else.entirely"),
            CgroupPlacement::Unknown
        );
    }

    /// The vulnerability predicate has exactly one true placement. This is
    /// the property the incident hinges on: flagging login sessions too
    /// would cry wolf on the launches that are known to survive, and
    /// flagging nothing would repeat the loss.
    #[test]
    fn only_the_user_manager_subtree_is_vulnerable_to_user_exit() {
        for (path, vulnerable) in [
            ("/user.slice/user-0.slice/user@0.service/user.slice", true),
            ("/user.slice/user-1000.slice/session-8.scope", false),
            ("/system.slice/cron.service", false),
            ("/", false),
            ("/docker/abc", false),
            ("/user.slice", false),
        ] {
            assert_eq!(
                classify_cgroup_path(path).vulnerable_to_user_manager_exit(),
                vulnerable,
                "{path}"
            );
        }
    }

    /// `placement_summary` is the wire contract: cgroup passthrough (null
    /// when unknown), the stable placement name, and a vulnerability bit
    /// that is false -- not null -- when the cgroup is unknown, so a
    /// consumer filtering on `== true` cannot crash on a dead session.
    #[test]
    fn placement_summary_is_the_wire_shape() {
        let summary = placement_summary(Some("/user.slice/user-0.slice/user@0.service"));
        assert_eq!(summary["cgroup"], "/user.slice/user-0.slice/user@0.service");
        assert_eq!(summary["placement"], "user_manager");
        assert_eq!(summary["vulnerable_to_user_manager_exit"], true);

        let unknown = placement_summary(Some("/somewhere.else"));
        assert_eq!(unknown["placement"], "unknown");
        assert_eq!(unknown["vulnerable_to_user_manager_exit"], false);

        let missing = placement_summary(None);
        assert!(missing["cgroup"].is_null());
        assert!(missing["placement"].is_null());
        assert_eq!(missing["vulnerable_to_user_manager_exit"], false);
    }

    /// The `a start` warning fires only for the vulnerable placement,
    /// names the cgroup and the command that kills it, and stays silent
    /// for unknown data (a dead record must not be warned about as if it
    /// were a placement decision).
    #[test]
    fn start_warning_fires_only_for_the_user_manager_subtree() {
        use std::collections::BTreeMap;

        fn record_with_worker_cgroup(cgroup: Option<&str>) -> SessionRecord {
            let id = uuid::Uuid::new_v4();
            SessionRecord {
                parent_session: None,
                schema_version: crate::SCHEMA_VERSION,
                id,
                workspace: "/tmp/w".into(),
                tag: "t".into(),
                engine: "shell".into(),
                profile: None,
                command: vec!["sh".into()],
                cwd: "/tmp".into(),
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                limits: crate::Limits::default(),
                history_bytes: 1024,
                created_at_ms: 0,
                updated_at_ms: 0,
                last_activity_ms: None,
                last_accessed_ms: None,
                reported_state: None,
                reported_state_at_ms: None,
                phase: crate::Phase::Running,
                worker_pid: None,
                workload_pid: None,
                worker_cgroup: cgroup.map(str::to_string),
                workload_cgroup: None,
                containment_cgroup: None,
                containment_cgroup_identity: None,
                containment_empty: Some(false),
                socket_path: format!("/tmp/{id}.sock").into(),
                history_path: format!("/tmp/{id}.history").into(),
                exit: None,
                error: None,
            }
        }

        let vulnerable = record_with_worker_cgroup(Some(
            "/user.slice/user-1000.slice/user@1000.service/app.slice",
        ));
        let warning = start_placement_warning(&vulnerable).expect("vulnerable placement warns");
        assert!(warning.contains("user@1000.service"), "{warning}");
        assert!(warning.contains("systemctl --user exit"), "{warning}");
        assert!(
            warning.contains(&vulnerable.id.to_string()),
            "warning must name the session: {warning}"
        );

        assert!(start_placement_warning(&record_with_worker_cgroup(Some(
            "/user.slice/user-1000.slice/session-8.scope"
        )))
        .is_none());
        assert!(start_placement_warning(&record_with_worker_cgroup(None)).is_none());
    }
}
