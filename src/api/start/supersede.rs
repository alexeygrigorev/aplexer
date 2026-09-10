//! Retiring the predecessor a start takes its `workspace+tag` from: the
//! archive-restore-delete transaction that keeps two durable records from
//! ever claiming one selector, and the re-judged verdict that gates it.

use super::*;

pub(super) const RETIRED_SESSIONS_DIR: &str = "retired-sessions";

pub(super) fn archive_superseded_session(paths: &Paths, id: Uuid) -> Result<PathBuf> {
    let retired_root = paths.state_root.join(RETIRED_SESSIONS_DIR);
    ensure_private_dir(&retired_root)?;
    let source = paths.state_session(id);
    let archived = retired_root.join(id.to_string());
    if archived.try_exists()? {
        bail!(
            "cannot retire superseded session {id}: archive {} already exists",
            archived.display()
        );
    }
    fs::rename(&source, &archived).with_context(|| {
        format!(
            "atomically retire superseded session {id} from {} to {}",
            source.display(),
            archived.display()
        )
    })?;
    if let Err(error) = (|| -> Result<()> {
        File::open(paths.state_root.join("sessions"))?.sync_all()?;
        File::open(&retired_root)?.sync_all()?;
        Ok(())
    })() {
        return match restore_superseded_session(paths, id, &archived) {
            Ok(()) => Err(error).context("sync retired predecessor transaction"),
            Err(restore_error) => Err(anyhow!(
                "sync retired predecessor transaction: {error:#}; restore also failed: {restore_error:#}"
            )),
        };
    }
    Ok(archived)
}

/// Retire the predecessor that `start_session` decided it could take the
/// `workspace+tag` from, re-deciding against the record as it stands on disk
/// right now.
///
/// The verdict formed before the spawn is advisory by construction: a worker
/// startup can take seconds, and `reap_verdict` is built out of live probes
/// (`/proc` liveness for the worker and the workload leader, and the
/// kernel's own view of a recorded cgroup), none of which the registry lock
/// freezes. It holds off other aplexer commands, not the world: a recycled
/// pid can make a dead `workload_pid` read alive again, and a containment
/// domain the caller could not inspect a moment ago may answer now. So
/// re-read and re-run the same predicate before destroying anything -- the
/// same rule `a prune`'s `reap_session_state` follows, for the same reason.
///
/// Refusing here is a start FAILURE, not a silent downgrade: the caller
/// rolls the freshly started replacement back rather than leaving two
/// durable records claiming one selector.
pub(super) fn archive_reclaimed_predecessor(
    paths: &Paths,
    existing: &SessionRecord,
) -> Result<PathBuf> {
    let current = read_session_record(paths, existing.id).with_context(|| {
        format!(
            "re-read superseded session {} before retiring it",
            existing.id
        )
    })?;
    if reap_verdict(&current).is_none() {
        bail!(
            "superseded session {} is live again (state: {}); refusing to retire it",
            current.id,
            current.observed_state()
        );
    }
    archive_superseded_session(paths, existing.id)
}

pub(super) fn restore_superseded_session(paths: &Paths, id: Uuid, archived: &Path) -> Result<()> {
    let destination = paths.state_session(id);
    fs::rename(archived, &destination).with_context(|| {
        format!(
            "restore superseded session {id} from {} to {}",
            archived.display(),
            destination.display()
        )
    })?;
    File::open(paths.state_root.join("sessions"))?.sync_all()?;
    if let Some(parent) = archived.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(super) fn cleanup_superseded_archive(path: &Path) -> Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if std::env::var_os("APLEXER_TEST_FAIL_SUPERSEDED_CLEANUP").is_some() {
        bail!("injected superseded-session cleanup failure");
    }
    fs::remove_dir_all(path).with_context(|| format!("remove archive {}", path.display()))?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    use crate::{atomic_write_json, ContainmentReap};

    /// A registry containing exactly one record, with its paths wired to the
    /// throwaway state/runtime roots so `read_session_record`'s identity
    /// checks accept it.
    fn seeded_registry(
        record: &mut SessionRecord,
    ) -> (Paths, tempfile::TempDir, tempfile::TempDir) {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        record.socket_path = paths.socket(record.id);
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), record).unwrap();
        (paths, state_dir, runtime_dir)
    }

    /// The reported zombie shape: worker dead, `phase` stuck at `running`,
    /// nothing left running.
    fn zombie_record() -> SessionRecord {
        SessionRecord::fixture("/ws/zombie", "zt")
    }

    /// A reclaimable predecessor is retired by the ordinary archive
    /// transaction: durable state moves to `retired-sessions/<id>`, nothing
    /// is deleted yet, so the caller can still restore it.
    #[test]
    fn a_reclaimable_predecessor_is_archived_not_destroyed() {
        let mut record = zombie_record();
        let (paths, _state, _runtime) = seeded_registry(&mut record);
        assert!(reap_verdict(&record).is_some());

        let archived = archive_reclaimed_predecessor(&paths, &record).expect("archive predecessor");
        assert!(archived.join("session.json").exists(), "archive is empty");
        assert!(!paths.state_session(record.id).exists());

        restore_superseded_session(&paths, record.id, &archived).expect("restore predecessor");
        assert!(paths.record(record.id).exists());
    }

    /// The verdict `start_session` forms before it spawns is stale by
    /// construction: worker startup takes time, and `reap_verdict` is built
    /// from live probes the registry lock does not freeze (`/proc` liveness
    /// and the kernel's view of a cgroup). So the record is re-read and
    /// re-judged immediately before it is retired.
    ///
    /// Driven here through the fact that can genuinely change under a held
    /// registry lock: the workload leader pid coming back alive (a recycled
    /// pid). The caller's copy still says "dead, reclaimable"; disk says a
    /// process is running; the retire must refuse and leave the predecessor
    /// exactly where it was.
    #[test]
    fn retiring_a_predecessor_re_reads_the_record_before_destroying_it() {
        let stale = zombie_record();
        let mut on_disk = stale.clone();
        let mut leader = Command::new("sleep").arg("30").spawn().unwrap();
        on_disk.workload_pid = Some(leader.id());
        let (paths, _state, _runtime) = seeded_registry(&mut on_disk);

        // What the caller believes, formed before the spawn.
        assert_eq!(
            reap_verdict(&stale),
            Some(ContainmentReap::NoRemainingHandle)
        );

        let error = archive_reclaimed_predecessor(&paths, &stale)
            .expect_err("retire must refuse a predecessor that is live on disk");
        let error = format!("{error:#}");
        assert!(error.contains("is live again"), "{error}");
        assert!(error.contains(&stale.id.to_string()), "{error}");
        assert!(
            paths.record(stale.id).exists(),
            "a refused retire still moved the predecessor's durable state"
        );
        assert!(
            !paths
                .state_root
                .join(RETIRED_SESSIONS_DIR)
                .join(stale.id.to_string())
                .exists(),
            "a refused retire stranded the predecessor in the archive"
        );
        assert!(
            leader.try_wait().unwrap().is_none(),
            "the retire path must never signal anything"
        );

        // Same record, leader gone: reclaimable again. Proves the refusal
        // came from the re-read and not from a blanket refusal.
        leader.kill().unwrap();
        leader.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::process_alive(on_disk.workload_pid.unwrap()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        archive_reclaimed_predecessor(&paths, &stale).expect("archive once the leader is gone");
        assert!(!paths.state_session(stale.id).exists());
    }

    /// The fence itself, without a spawn: a record in the spawn-to-worker-lock
    /// gap (`phase: starting, worker_pid: null`) reads as `worker_alive:
    /// false` and is otherwise perfectly reclaimable, so only the worker
    /// lock stands between a live spawn and having its state taken.
    #[test]
    fn a_pre_pid_record_is_fenced_by_its_worker_lock() {
        let mut record = zombie_record();
        record.phase = Phase::Starting;
        let (paths, _state, _runtime) = seeded_registry(&mut record);
        assert!(!record.worker_alive());
        assert!(reap_verdict(&record).is_some());

        let held = FileLock::exclusive(&paths.worker_lock(record.id), true).unwrap();
        assert!(matches!(
            fence_pre_pid_worker(&paths, &record).unwrap(),
            PrePidFence::WorkerHoldsLock(_)
        ));
        drop(held);
        assert!(matches!(
            fence_pre_pid_worker(&paths, &record).unwrap(),
            PrePidFence::Fenced(Some(_))
        ));

        // Past the gap, the pid is the authority and no fence is taken --
        // otherwise every ordinary reclaim would contend on a lock the live
        // worker legitimately holds.
        let mut registered = record.clone();
        registered.worker_pid = Some(std::process::id());
        assert!(matches!(
            fence_pre_pid_worker(&paths, &registered).unwrap(),
            PrePidFence::Fenced(None)
        ));
    }
}
