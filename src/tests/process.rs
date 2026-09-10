//! Unit tests for process primitives.

use super::*;

#[test]
fn kill_grace_is_bounded_before_duration_or_deadline_math() {
    assert_eq!(
        kill_grace_duration(MAX_KILL_GRACE_MS).unwrap(),
        Duration::from_millis(MAX_KILL_GRACE_MS)
    );
    assert!(kill_grace_duration(MAX_KILL_GRACE_MS + 1).is_err());
    assert!(kill_grace_duration(u64::MAX).is_err());
}

/// `kill(pid, 0)` succeeds for a zombie, so the raw signalability test
/// reported exited-but-unreaped processes as alive. Under a worker's
/// child subreaper that is not a corner case: a session started inside
/// another session reparents onto the outer worker, and until it is
/// reaped every liveness answer about it -- `worker_alive`,
/// `workload_leader_alive`, and therefore `reap_verdict` and `a prune` --
/// was wrong in the direction of "still running, keep it".
#[test]
fn process_alive_reports_an_unreaped_zombie_as_dead() {
    let mut child = Command::new("/bin/true").spawn().unwrap();
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_is_zombie(pid) {
        assert!(
            Instant::now() < deadline,
            "child {pid} never became a zombie"
        );
        thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(
        process_state(pid).unwrap(),
        'Z',
        "the test needs a real unreaped zombie"
    );
    assert_eq!(
        unsafe { libc::kill(pid as libc::pid_t, 0) },
        0,
        "a zombie is still signalable, which is exactly the trap"
    );
    assert!(
        !process_alive(pid),
        "zombie {pid} must not be reported alive"
    );

    child.wait().unwrap();
    assert!(!process_alive(pid));
    assert!(
        !process_is_zombie(pid),
        "a reaped pid has no state to read, so it is not a zombie either"
    );
}

/// A `Z` in `/proc/<pid>/stat` is not by itself proof that a process is
/// finished: a thread group leader that exited while its siblings kept
/// running reads exactly the same (verified against a real process --
/// `state=Z` with two entries under `/proc/<pid>/task`). Treating that
/// as dead would let a multi-threaded workload be declared contained
/// while it was still executing, so the thread group must be down to the
/// leader's corpse alone.
#[test]
fn zombie_detection_requires_an_empty_thread_group() {
    let root = tempfile::tempdir().unwrap();
    let write_process = |pid: u32, state: char, threads: &[u32]| {
        let dir = root.path().join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        // A comm containing spaces and a ')' is legal and must not shift
        // the state field.
        fs::write(
            dir.join("stat"),
            format!("{pid} (od d) ba) {state} 1 {pid} 0 -1 4194304 0 0\n"),
        )
        .unwrap();
        for tid in threads {
            fs::create_dir_all(dir.join("task").join(tid.to_string())).unwrap();
        }
    };

    write_process(11, 'Z', &[11]);
    write_process(12, 'Z', &[12, 13]);
    write_process(14, 'S', &[14]);
    write_process(15, 'R', &[15, 16]);

    assert_eq!(process_state_in(root.path(), 11).unwrap(), 'Z');
    assert_eq!(process_state_in(root.path(), 12).unwrap(), 'Z');

    assert!(
        process_is_zombie_in(root.path(), 11),
        "a Z leader alone in its thread group is a reapable zombie"
    );
    assert!(
        !process_is_zombie_in(root.path(), 12),
        "a Z leader with a live sibling thread is still running code"
    );
    assert!(!process_is_zombie_in(root.path(), 14));
    assert!(!process_is_zombie_in(root.path(), 15));
    assert!(
        !process_is_zombie_in(root.path(), 99),
        "an unreadable process must not be subtracted from liveness"
    );
}

/// Field 22 of `/proc/<pid>/stat` is the start time, counted from the
/// state field that follows the comm -- a comm with spaces and a `)` in
/// it must not shift the count.
#[test]
fn process_start_time_is_field_22_after_the_comm() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("7");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("stat"),
        "7 (od d) ba) S 1 7 7 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 987654 1000 2\n",
    )
    .unwrap();
    assert_eq!(process_start_time_ticks_in(root.path(), 7).unwrap(), 987654);

    fs::write(dir.join("stat"), "7 (short) S 1 7\n").unwrap();
    let error = process_start_time_ticks_in(root.path(), 7).unwrap_err();
    assert!(
        error.to_string().contains("no process start time"),
        "{error:#}"
    );
}

/// A persisted worker pid is fed straight to these probes. Pid 0 asks
/// `kill(2)` about the caller's own process group and a pid above
/// `i32::MAX` wraps to a negative `pid_t`, so both used to answer "alive"
/// for something that is not the recorded process at all.
#[test]
fn pids_that_cannot_name_one_process_are_dead() {
    for pid in [0, i32::MAX as u32 + 1, u32::MAX] {
        assert!(!process_alive(pid), "pid {pid} must not read as alive");
        let error = pidfd_open(pid).expect_err("no pidfd for an unaddressable pid");
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH), "pid {pid}");
    }
    assert!(process_alive(std::process::id()));
}

/// A live process must never be mistaken for a zombie by the state read.
#[test]
fn process_alive_still_reports_a_running_child_as_alive() {
    let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    assert!(process_alive(pid));
    assert!(!process_is_zombie(pid));
    assert!(matches!(process_state(pid).unwrap(), 'R' | 'S' | 'D'));
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn executable_available_requires_execute_permission() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("tool");
    fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(!executable_available(program.to_str().unwrap()));

    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(executable_available(program.to_str().unwrap()));
}
