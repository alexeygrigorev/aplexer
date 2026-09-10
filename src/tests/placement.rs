//! Unit tests for process cgroup placement classification.

use super::*;
use crate::placement::*;

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
