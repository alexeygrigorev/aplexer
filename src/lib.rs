#![cfg(target_os = "linux")]

pub mod agent_events;
pub mod agent_kind;
pub mod api;
pub mod hooks;
pub mod messaging;
pub mod placement;
pub mod screen;
pub mod watch;
pub mod worker;

mod cgroup;
pub use cgroup::*;

mod history;
pub use history::*;

mod protocol;
pub use protocol::*;

mod config;
pub use config::*;

mod util;
pub use util::*;

mod process;
pub use process::*;

mod record;
pub use record::*;

mod registry;
pub use registry::{read_record, read_session_record, list_records, resolve_record};

mod paths;
pub use paths::{Paths, ensure_private_dir, canonical_workspace};

mod persist;
pub use persist::{atomic_write_json, atomic_write_bytes, FileLock};

#[cfg(feature = "python")]
mod python;


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::env;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use anyhow::{anyhow, bail, Result};
    use std::ffi::CString;
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use uuid::Uuid;
    use crate::paths::{absolute_override_path, absolute_xdg_path};
    use serde::Deserialize;
    use std::io::Write;

    fn load_config_text(text: &str) -> Result<Config> {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        fs::write(&paths.config_file, text).unwrap();
        Config::load(&paths)
    }

    fn registry_record(paths: &Paths, id: Uuid) -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: paths.state_root.clone(),
            tag: "registry-test".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: paths.state_root.clone(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Exited,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(true),
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn zcodex_is_a_built_in_codex_variant() {
        let config = load_config_text("").unwrap();
        let zcodex = config
            .engines
            .get("zcodex")
            .expect("built-in zcodex engine");
        assert_eq!(
            zcodex.command,
            vec![
                "zcodex".to_string(),
                "-c".to_string(),
                "check_for_update_on_startup=false".to_string(),
            ]
        );
        assert_eq!(
            zcodex.skip_permissions_argv,
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
        assert_eq!(engine_family("zcodex"), "codex");
        assert_eq!(engine_family("codex"), "codex");
        assert_eq!(engine_family("claude"), "claude");
    }

    /// `config_keep_exited` is a second reader of the same setting, chosen
    /// so the worker's exit path does not depend on the whole config file
    /// validating. It must agree with `Config::load` on every shape that
    /// matters, or the escape hatch would silently mean different things to
    /// `a` and to the worker that acts on it.
    #[test]
    fn config_keep_exited_matches_full_config_load() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };

        // No config file at all: the documented default.
        assert!(!config_keep_exited(&paths));

        for (text, expected) in [
            ("version = 1\n", false),
            ("version = 1\nkeep_exited = false\n", false),
            ("version = 1\nkeep_exited = true\n", true),
        ] {
            fs::write(&paths.config_file, text).unwrap();
            assert_eq!(
                Config::load(&paths).unwrap().keep_exited,
                expected,
                "Config::load disagreed for {text:?}"
            );
            assert_eq!(
                config_keep_exited(&paths),
                expected,
                "config_keep_exited disagreed for {text:?}"
            );
        }

        // An unrelated invalid entry fails `Config::load` outright. The
        // worker's reader must not treat that as "keep records": a typo in
        // an engine definition is not a retention decision.
        fs::write(
            &paths.config_file,
            "version = 1\nkeep_exited = true\ndefault_engine = \"nope\"\n",
        )
        .unwrap();
        assert!(Config::load(&paths).is_err());
        assert!(config_keep_exited(&paths));

        // Unparsable or unreadable config: default, never a panic.
        fs::write(&paths.config_file, "this is not toml {{{").unwrap();
        assert!(!config_keep_exited(&paths));
    }

    #[test]
    fn registry_enumeration_reports_corrupt_records() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        fs::write(paths.record(id), b"{truncated").unwrap();

        let error = list_records(&paths).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(message.contains("parse"), "{message}");
    }

    /// The window `start_session` opens between creating a session directory
    /// and writing that session's first record. Any reader that does not hold
    /// the registry lock can land in it, and treating it as corruption killed
    /// `a watch` outright (see `list_records`). The same fixture must still be
    /// reported once the record appears, so the entry is skipped, not
    /// blacklisted.
    #[test]
    fn registry_enumeration_skips_a_session_whose_record_is_not_written_yet() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let pending = Uuid::new_v4();
        fs::create_dir(paths.state_session(pending)).unwrap();
        let written = Uuid::new_v4();
        fs::create_dir(paths.state_session(written)).unwrap();
        atomic_write_json(&paths.record(written), &registry_record(&paths, written)).unwrap();

        let records = list_records(&paths).unwrap();
        assert_eq!(
            records.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![written],
            "a session mid-creation must be skipped, not reported and not fatal"
        );

        // ... and picked up as soon as its record lands.
        atomic_write_json(&paths.record(pending), &registry_record(&paths, pending)).unwrap();
        let mut ids = list_records(&paths)
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();
        ids.sort();
        let mut expected = vec![pending, written];
        expected.sort();
        assert_eq!(ids, expected);
    }

    /// The complement of the test above: skipping a missing record must not
    /// weaken the fail-closed contract for a record that is present and wrong.
    #[test]
    fn registry_enumeration_still_fails_closed_on_an_empty_record_file() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        fs::write(paths.record(id), b"").unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("parse"), "{error:#}");
    }

    #[test]
    fn registry_enumeration_reports_unsupported_schema() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        let mut record = registry_record(&paths, id);
        record.schema_version = SCHEMA_VERSION + 1;
        atomic_write_json(&paths.record(id), &record).unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported session schema"));
    }

    #[test]
    fn registry_enumeration_validates_directory_id_and_paths() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();

        let mut record = registry_record(&paths, id);
        record.id = Uuid::new_v4();
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("directory id"));

        record = registry_record(&paths, id);
        record.socket_path = paths.socket(Uuid::new_v4());
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("socket path"));

        record = registry_record(&paths, id);
        record.history_path = paths.history(Uuid::new_v4());
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("history path"));
    }

    #[test]
    fn registry_enumeration_grandfathers_legacy_history_capacity() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();

        let mut record = registry_record(&paths, id);
        record.history_bytes = MAX_HISTORY_BYTES + 1;
        atomic_write_json(&paths.record(id), &record).unwrap();

        let records = list_records(&paths).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].history_bytes, MAX_HISTORY_BYTES + 1);
    }

    #[test]
    fn registry_enumeration_rejects_unexpected_entries() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let unexpected = paths.state_root.join("sessions").join("leftover");
        fs::write(&unexpected, b"not a session directory").unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("is not a directory"));
    }

    #[test]
    fn ensure_private_dir_rejects_leaf_symlink_without_chmodding_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();

        let error = ensure_private_dir(&link).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without following symbolic links"),
            "{error:#}"
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn ensure_private_dir_rejects_symlink_ancestor_without_creating_beneath_it() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();

        assert!(ensure_private_dir(&link.join("child")).is_err());
        assert!(!target.join("child").exists());
    }

    #[test]
    fn ensure_private_dir_validates_type_before_chmod() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("ordinary-file");
        fs::write(&file, b"not a directory").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(ensure_private_dir(&file).is_err());
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn ensure_private_dir_chmods_verified_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&directory).unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn atomic_write_json_removes_temp_after_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("record.json");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write_json(&destination, &serde_json::json!({"secret": "value"})).is_err());
        assert_no_atomic_temps(root.path());
    }

    #[test]
    fn atomic_write_bytes_removes_temp_after_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("history.bin");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write_bytes(&destination, b"secret bytes").is_err());
        assert_no_atomic_temps(root.path());
    }

    fn assert_no_atomic_temps(directory: &Path) {
        let leftovers = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn session_record_write_persists_worker_start_identity_once() {
        let root = tempfile::tempdir().unwrap();
        let record_path = root.path().join("session.json");
        let pid = std::process::id();
        atomic_write_json(
            &record_path,
            &serde_json::json!({"worker_pid": pid, "value": 1}),
        )
        .unwrap();
        let identity_path = root.path().join(WORKER_IDENTITY_FILE);
        let original: ProcessIdentity =
            serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        assert_eq!(original.pid, pid);
        assert_eq!(original.boot_id, linux_boot_id().unwrap());
        assert_eq!(
            original.start_time_ticks,
            process_start_time_ticks(pid).unwrap()
        );

        // A later write must not refresh the immutable registration.
        atomic_write_json(
            &record_path,
            &serde_json::json!({"worker_pid": pid, "value": 2}),
        )
        .unwrap();
        let after: ProcessIdentity =
            serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        assert_eq!(after.pid, original.pid);
        assert_eq!(after.start_time_ticks, original.start_time_ticks);
    }

    fn liveness_record(state_dir: &Path) -> SessionRecord {
        let pid = std::process::id();
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: state_dir.to_path_buf(),
            tag: "identity-test".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: state_dir.to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Running,
            worker_pid: Some(pid),
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: state_dir.join("control.sock"),
            history_path: state_dir.join("history.bin"),
            exit: None,
            error: None,
        }
    }

    /// The two proof shapes that short-circuit before the kernel is ever
    /// consulted, and the no-locator shape that is the reported zombie.
    #[test]
    fn containment_reap_verdict_reads_durable_proof_without_probing() {
        let state = tempfile::tempdir().unwrap();
        let base = liveness_record(state.path());
        let refuse = |_: Uuid, _: &Path, _: Option<&CgroupIdentity>| -> Result<bool> {
            panic!("probe must not run when the record already answers the question")
        };

        let mut proven = base.clone();
        proven.containment_empty = Some(true);
        assert_eq!(
            containment_reap_verdict_with(&proven, refuse),
            ContainmentReap::Proven,
            "a worker's own durable proof must still be trusted"
        );

        let mut legacy_exit = base.clone();
        legacy_exit.containment_empty = None;
        legacy_exit.exit = Some(ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 2,
        });
        assert_eq!(
            containment_reap_verdict_with(&legacy_exit, refuse),
            ContainmentReap::Proven,
            "the legacy pre-field ExitInfo proof must still be trusted"
        );

        // The reported zombie shape: unlimited session, worker SIGKILLed
        // before it could prove anything. No locator, so nothing to probe.
        let unlimited = base.clone();
        assert_eq!(unlimited.containment_cgroup, None);
        assert_eq!(unlimited.containment_empty, Some(false));
        assert_eq!(
            containment_reap_verdict_with(&unlimited, refuse),
            ContainmentReap::NoRemainingHandle
        );
    }

    /// Every outcome the kernel probe can return, including the one arm that
    /// stands between `a prune` and deleting the last handle to a live
    /// containment domain: a locator that validates and is still POPULATED
    /// must retain. Injected rather than staged on a real cgroup so this
    /// runs everywhere, on every `cargo test`, with no delegation needed;
    /// `recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup`
    /// covers the probe itself against a real one.
    #[test]
    fn containment_reap_verdict_maps_every_cgroup_probe_outcome() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_empty = Some(false);
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));

        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| Ok(true)),
            ContainmentReap::Proven,
            "an observed-empty domain is proof at least as strong as the persisted bit"
        );
        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| Ok(false)),
            ContainmentReap::Retain,
            "a populated containment domain must keep its locator"
        );
        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| bail!("cgroup inspection failed")),
            ContainmentReap::Retain,
            "an unreadable containment domain must fail closed"
        );

        // The probe is handed the record's own identity triple -- a mixed-up
        // locator would validate against the wrong domain.
        let mut seen = None;
        containment_reap_verdict_with(&record, |id, locator, identity| {
            seen = Some((id, locator.to_path_buf(), identity.cloned()));
            Ok(false)
        });
        let (id, locator, identity) = seen.expect("probe ran");
        assert_eq!(id, record.id);
        assert_eq!(Some(locator), record.containment_cgroup);
        assert!(identity.is_none());
    }

    /// A recorded cgroup with no identity cannot be validated, so it cannot
    /// be declared empty either -- keep the locator.
    #[test]
    fn containment_reap_verdict_retains_an_unvalidatable_locator() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));
        assert!(record.containment_cgroup_identity.is_none());
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Retain,
            "an unvalidatable containment locator must fail closed"
        );
    }

    /// A cgroup recorded under a different boot cannot hold a live process:
    /// the hierarchy and every task in it ceased to exist at reboot. Without
    /// this, `validate_recorded_cgroup`'s (correct, for destructive
    /// recovery) refusal to touch a foreign-boot identity would make a
    /// rebooted-away record permanently unreapable.
    #[test]
    fn containment_reap_verdict_treats_a_previous_boot_as_empty() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));
        let mut identity = current_cgroup_identity().unwrap_or(CgroupIdentity {
            boot_id: String::new(),
            cgroup_namespace_device: 0,
            cgroup_namespace_inode: 0,
            mount_namespace_device: 0,
            mount_namespace_inode: 0,
            cgroup_mount_id: 0,
            cgroup_root_device: 0,
            cgroup_root_inode: 0,
        });
        identity.boot_id = "00000000-0000-0000-0000-000000000000".into();
        assert_ne!(identity.boot_id, linux_boot_id().unwrap());
        record.containment_cgroup_identity = Some(identity);
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Proven,
            "a cgroup from a previous boot cannot hold a live process"
        );
    }

    /// The membership half of the real probe, without needing a real
    /// cgroup: `cgroup.events` says `populated 1` while tasks remain, and a
    /// collected cgroup loses the file entirely (ENOENT means empty).
    #[test]
    fn cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !cgroup_path_populated(dir.path()).unwrap(),
            "a collected cgroup (no cgroup.events) is empty, not an error"
        );
        fs::write(dir.path().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(cgroup_path_populated(dir.path()).unwrap());
        fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(!cgroup_path_populated(dir.path()).unwrap());
        fs::write(dir.path().join("cgroup.events"), "frozen 0\n").unwrap();
        assert!(
            cgroup_path_populated(dir.path()).is_err(),
            "a cgroup.events with no populated key must fail closed, not read as empty"
        );
    }

    /// A cgroup created inside the caller's own delegated subtree, named
    /// exactly the way a real session's containment scope is named, so
    /// `validate_recorded_cgroup`'s full chain (locator shape, cgroup-v2
    /// filesystem, mount device, identity triple) runs for real. Returns
    /// None when the environment has no writable cgroup-v2 parent.
    struct DelegatedCgroup {
        path: PathBuf,
        members: Vec<std::process::Child>,
    }

    impl DelegatedCgroup {
        fn create(id: Uuid) -> Option<Self> {
            let own = fs::read_to_string("/proc/self/cgroup").ok()?;
            let relative = own
                .lines()
                .find_map(|line| line.strip_prefix("0::"))?
                .trim()
                .trim_start_matches('/')
                .to_string();
            let mut candidate = Path::new(CGROUP_V2_ROOT).join(&relative);
            let leaf = format!("aplexer-workload-{id}.scope");
            // Walk up until a parent accepts a new child cgroup: the leaf a
            // test process sits in is usually not delegated, its user@.service
            // ancestor is.
            loop {
                let path = candidate.join(&leaf);
                if fs::create_dir(&path).is_ok() {
                    return Some(Self {
                        path,
                        members: Vec::new(),
                    });
                }
                candidate = candidate.parent()?.to_path_buf();
                if !candidate.starts_with(CGROUP_V2_ROOT) || candidate == Path::new(CGROUP_V2_ROOT)
                {
                    return None;
                }
            }
        }

        fn populate(&mut self) -> u32 {
            let child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn cgroup member");
            let pid = child.id();
            self.members.push(child);
            fs::write(self.path.join("cgroup.procs"), format!("{pid}\n"))
                .expect("move member into the delegated cgroup");
            pid
        }

        /// Stop every member and reap it, so the cgroup can be collected and
        /// no `sleep` outlives the test.
        fn drain_members(&mut self) {
            for mut member in self.members.drain(..) {
                let _ = member.kill();
                let _ = member.wait();
            }
        }
    }

    impl Drop for DelegatedCgroup {
        fn drop(&mut self) {
            self.drain_members();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.path.exists() && Instant::now() < deadline {
                if fs::remove_dir(&self.path).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// The real kernel probe, end to end, against a genuinely delegated
    /// cgroup: empty, then POPULATED (the arm that must retain), then empty
    /// again, then collected. `#[ignore]`d for the same reason
    /// `tests/oom_isolation.rs`'s destructive tests are -- it needs a
    /// cgroup-v2 tree with delegation to the running user, which a CI
    /// container generally lacks. Run it explicitly:
    ///
    ///   cargo test --lib recorded_cgroup_observed_empty -- --ignored --nocapture
    ///
    /// The decision arms it feeds are pinned unconditionally by
    /// `containment_reap_verdict_maps_every_cgroup_probe_outcome`, and the
    /// membership read by
    /// `cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty`;
    /// this test is what proves those two meet reality.
    #[test]
    #[ignore = "needs cgroup-v2 delegation to the running user; run explicitly"]
    fn recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_empty = Some(false);
        let mut cgroup = DelegatedCgroup::create(record.id)
            .expect("this environment has no writable cgroup-v2 parent");
        record.containment_cgroup = Some(cgroup.path.clone());
        record.containment_cgroup_identity = Some(current_cgroup_identity().unwrap());
        let probe = || {
            recorded_cgroup_observed_empty(
                record.id,
                record.containment_cgroup.as_deref().unwrap(),
                record.containment_cgroup_identity.as_ref(),
            )
            .unwrap()
        };

        assert!(probe(), "a freshly created cgroup is empty");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

        let pid = cgroup.populate();
        assert!(
            !probe(),
            "a cgroup holding a live process must not read as empty"
        );
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Retain,
            "prune must keep the locator of a populated containment domain"
        );
        assert!(process_alive(pid), "probing must not signal anything");

        cgroup.drain_members();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !probe() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(probe(), "an emptied cgroup must read as empty again");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

        // Collected: cgroup v2 cannot remove a populated cgroup, so a
        // durably recorded locator that has since disappeared is empty by
        // construction.
        fs::remove_dir(&cgroup.path).expect("remove the now-empty cgroup");
        assert!(probe(), "a collected cgroup is empty by construction");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);
    }

    /// `state` is derived from both facts and rewrites neither.
    #[test]
    fn observed_state_reports_broken_only_for_a_contradicted_phase() {
        let now = DEFAULT_STARTUP_TIMEOUT_MS * 2;
        let aged = |age: u64| now - age;
        // A `Starting` record with no live worker is the shape
        // `start_session` persists before the worker registers its pid, and
        // also the shape a crashed start leaves behind. Only age tells them
        // apart, and the boundary is exactly the startup budget.
        assert_eq!(
            observed_state(&Phase::Starting, false, aged(0), now),
            "starting"
        );
        assert_eq!(
            observed_state(
                &Phase::Starting,
                false,
                aged(DEFAULT_STARTUP_TIMEOUT_MS - 1),
                now
            ),
            "starting"
        );
        assert_eq!(
            observed_state(
                &Phase::Starting,
                false,
                aged(DEFAULT_STARTUP_TIMEOUT_MS),
                now
            ),
            "broken",
            "past the startup budget a pre-PID record is a crashed start"
        );
        // A record whose clock ran backwards (or was written by a machine
        // with a different clock) must not become permanently `starting`.
        assert_eq!(
            observed_state(&Phase::Starting, false, now + 1_000, now),
            "starting"
        );
        assert_eq!(observed_state(&Phase::Starting, true, 0, now), "starting");
        // Running/Exiting are only ever written by a worker that already
        // registered, so a dead worker there is broken at any age.
        for phase in [Phase::Running, Phase::Exiting] {
            assert_eq!(observed_state(&phase, false, aged(0), now), "broken");
            assert_eq!(observed_state(&phase, false, aged(1), now), "broken");
            assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
        }
        for phase in [Phase::Exited, Phase::Failed] {
            assert_eq!(observed_state(&phase, false, aged(0), now), phase.name());
            assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
        }
    }

    #[test]
    fn worker_liveness_rejects_recycled_pid_identity() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        let pid = record.worker_pid.unwrap();
        let identity = ProcessIdentity {
            pid,
            start_time_ticks: process_start_time_ticks(pid).unwrap() + 1,
            boot_id: linux_boot_id().unwrap(),
        };
        fs::write(
            state.path().join(WORKER_IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();

        assert!(!record.worker_alive());
        record.phase = Phase::Failed;
        assert!(record.worker_finished());
    }

    #[test]
    fn worker_liveness_uses_safe_legacy_fallback_for_missing_or_corrupt_identity() {
        let state = tempfile::tempdir().unwrap();
        let record = liveness_record(state.path());
        assert!(record.worker_alive(), "missing sidecar uses numeric pid");

        fs::write(state.path().join(WORKER_IDENTITY_FILE), b"not-json").unwrap();
        assert!(record.worker_alive(), "corrupt sidecar fails closed");

        let identity = ProcessIdentity {
            pid: record.worker_pid.unwrap() + 1,
            start_time_ticks: 0,
            boot_id: "corrupt".into(),
        };
        fs::write(
            state.path().join(WORKER_IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        assert!(record.worker_alive(), "pid mismatch fails closed");
    }

    #[test]
    fn frame_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FrameKind::Data, b"a\0b").unwrap();
        let mut cursor = io::Cursor::new(bytes);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(frame.kind, FrameKind::Data);
        assert_eq!(frame.payload, b"a\0b");
    }

    #[test]
    fn bound_request_remains_readable_by_legacy_workers() {
        #[derive(Deserialize)]
        struct LegacyRequest {
            version: u16,
            request_id: String,
            #[serde(flatten)]
            operation: Operation,
        }

        let request = Request::new(Uuid::new_v4(), Operation::Ping);
        let legacy: LegacyRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(legacy.version, PROTOCOL_VERSION);
        assert_eq!(legacy.request_id, request.request_id);
        assert!(matches!(legacy.operation, Operation::Ping));
    }

    #[test]
    fn bounded_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(dir.path().join("h"), 4).unwrap();
        h.append(b"abcdef").unwrap();
        assert_eq!(h.snapshot(None), b"cdef");
    }

    #[test]
    fn history_incremental_flush_writes_only_delta_and_recovers_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 8).unwrap();

        history.append(b"abcdef").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 6);
        history.append(b"\0g").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 8);
        history.append(b"hi").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 10);
        assert_eq!(history.snapshot(None), b"cdef\0ghi");

        let reopened = History::open(path.clone(), 8).unwrap();
        assert_eq!(reopened.snapshot(None), b"cdef\0ghi");
        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"cdef\0ghi"
        );
    }

    #[test]
    fn history_uncommitted_suffix_is_ignored_and_truncated_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"safe").unwrap();
        history.flush().unwrap();

        let data_path = history_data_path(&path, 0);
        OpenOptions::new()
            .append(true)
            .open(&data_path)
            .unwrap()
            .write_all(b"torn")
            .unwrap();
        let mut reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"safe");
        reopened.append(b"-next").unwrap();
        reopened.flush().unwrap();
        assert_eq!(
            History::open(path, 16).unwrap().snapshot(None),
            b"safe-next"
        );
    }

    #[test]
    fn history_corrupt_newest_commit_recovers_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"prior").unwrap();
        history.flush().unwrap();
        history.append(b"-newest").unwrap();
        history.flush().unwrap();

        fs::write(history_commit_path(&path, 0), b"{torn").unwrap();
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"prior");
        assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"prior");
    }

    #[test]
    fn history_corrupt_v2_pair_never_falls_back_to_stale_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"prior").unwrap();
        history.flush().unwrap();
        history.append(b"-newest").unwrap();
        history.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"prior-newest");

        fs::write(history_commit_path(&path, 0), b"{torn-newest").unwrap();
        fs::write(history_commit_path(&path, 1), b"{torn-prior").unwrap();
        let read_error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{read_error:#}").contains("no valid committed history generation"),
            "{read_error:#}"
        );
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(
            fs::read(path).unwrap(),
            b"prior-newest",
            "fail-closed v2 recovery mutated the raw compatibility evidence"
        );
    }

    #[test]
    fn history_marker_prevents_raw_fallback_when_all_commits_disappear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"v2-authoritative").unwrap();
        history.flush().unwrap();
        assert!(history_marker_path(&path).is_file());
        assert!(history_data_path(&path, 0).is_file());

        fs::write(&path, b"stale-raw").unwrap();
        for slot in 0..HISTORY_COMMIT_COUNT {
            let commit = history_commit_path(&path, slot);
            if commit.exists() {
                fs::remove_file(commit).unwrap();
            }
        }

        let read_error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{read_error:#}").contains("no valid committed history generation"),
            "{read_error:#}"
        );
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(fs::read(path).unwrap(), b"stale-raw");
    }

    #[test]
    fn history_unpublished_first_bank_without_marker_still_recovers_legacy_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"raw-precommit").unwrap();
        let blocked_commit = history_commit_path(&path, 1);
        fs::create_dir(&blocked_commit).unwrap();

        assert!(history.flush().is_err());
        assert!(history_data_path(&path, 0).is_file());
        assert!(!history_marker_path(&path).exists());
        assert_eq!(fs::read(&path).unwrap(), b"raw-precommit");
        drop(history);
        fs::remove_dir(blocked_commit).unwrap();

        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"raw-precommit"
        );
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"raw-precommit");
        assert!(history_marker_path(&path).is_file());
    }

    #[test]
    fn history_markerless_v2_is_readable_and_next_writable_open_publishes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"pre-marker-v2").unwrap();
        history.flush().unwrap();
        fs::remove_file(history_marker_path(&path)).unwrap();

        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"pre-marker-v2"
        );
        assert!(!history_marker_path(&path).exists());
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"pre-marker-v2");
        assert!(history_marker_path(&path).is_file());
    }

    #[test]
    fn history_marker_is_bounded_checksummed_and_a_safe_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let marker_path = history_marker_path(&path);
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"committed").unwrap();
        history.flush().unwrap();
        let valid_marker = fs::read(&marker_path).unwrap();

        let mut bad_checksum: HistoryMarker = serde_json::from_slice(&valid_marker).unwrap();
        bad_checksum.store_id = Uuid::new_v4();
        fs::write(&marker_path, serde_json::to_vec(&bad_checksum).unwrap()).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "{error:#}"
        );

        let wrong_store = bad_checksum.seal().unwrap();
        fs::write(&marker_path, serde_json::to_vec(&wrong_store).unwrap()).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("no valid committed history generation"),
            "{error:#}"
        );

        fs::write(&marker_path, vec![b'x'; HISTORY_MARKER_MAX_BYTES + 1]).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds the"), "{error:#}");

        fs::remove_file(&marker_path).unwrap();
        let target = dir.path().join("marker-target");
        fs::write(&target, b"unrelated").unwrap();
        symlink(&target, &marker_path).unwrap();
        assert!(read_persisted_history_tail(&path, None).is_err());
        fs::remove_file(&marker_path).unwrap();

        let marker_c = CString::new(marker_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(marker_c.as_ptr(), 0o600) }, 0);
        assert!(read_persisted_history_tail(&path, None).is_err());
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(fs::read(target).unwrap(), b"unrelated");
    }

    #[test]
    fn history_compaction_is_bounded_and_amortized_by_new_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 4).unwrap();
        history.append(b"abcd").unwrap();
        history.flush().unwrap();
        history.append(b"efgh").unwrap();
        history.flush().unwrap();
        history.append(b"i").unwrap();
        history.flush().unwrap();

        assert_eq!(history.snapshot(None), b"fghi");
        assert_eq!(history.data_bytes_written, 12);
        assert_eq!(fs::read(&path).unwrap(), b"fghi");
        for slot in 0..HISTORY_BANK_COUNT {
            let data_path = history_data_path(&path, slot);
            if let Ok(metadata) = fs::metadata(data_path) {
                assert!(
                    metadata.len() <= HISTORY_BANK_HEADER_BYTES as u64 + 2 * history.cap as u64
                );
            }
        }
        assert_eq!(History::open(path, 4).unwrap().snapshot(None), b"fghi");
    }

    #[test]
    fn history_legacy_migration_and_capacity_changes_keep_only_exact_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        fs::write(&path, b"0123456789").unwrap();

        let mut migrated = History::open(path.clone(), 4).unwrap();
        assert_eq!(migrated.snapshot(None), b"6789");
        assert_eq!(fs::read(&path).unwrap(), b"6789");
        migrated.append(b"AB").unwrap();
        migrated.flush().unwrap();
        assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"89AB");
        assert_eq!(fs::read(&path).unwrap(), b"6789AB");
        migrated.flush_final().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"89AB");

        let shrunk = History::open(path.clone(), 3).unwrap();
        assert_eq!(shrunk.snapshot(None), b"9AB");
        let mut grown = History::open(path.clone(), 6).unwrap();
        assert_eq!(grown.snapshot(None), b"9AB");
        grown.append(b"CD").unwrap();
        grown.flush().unwrap();
        assert_eq!(History::open(path, 6).unwrap().snapshot(None), b"9ABCD");
    }

    #[test]
    fn history_special_files_fail_without_becoming_persistence_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"unrelated").unwrap();
        let legacy = dir.path().join("history.bin");
        symlink(&target, &legacy).unwrap();
        assert!(History::open(legacy.clone(), 8).is_err());
        assert!(read_persisted_history_tail(&legacy, None).is_err());
        fs::remove_file(&legacy).unwrap();

        let commit = history_commit_path(&legacy, 0);
        let commit_c = CString::new(commit.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(commit_c.as_ptr(), 0o600) }, 0);
        assert!(History::open(legacy.clone(), 8).is_err());
        assert!(read_persisted_history_tail(&legacy, None).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unrelated");
    }

    #[test]
    fn history_capacity_has_one_global_limit_and_zero_stays_disabled() {
        for value in [0, 1, DEFAULT_HISTORY_BYTES, MAX_HISTORY_BYTES] {
            assert_eq!(validate_history_bytes(value).unwrap(), value);
        }
        for value in [MAX_HISTORY_BYTES + 1, usize::MAX] {
            let error = validate_history_bytes(value).unwrap_err().to_string();
            assert!(error.contains("history_bytes"), "{error}");
            assert!(error.contains(&MAX_HISTORY_BYTES.to_string()), "{error}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disabled-history.bin");
        let mut history = History::open(path.clone(), 0).unwrap();
        history.append(b"not retained").unwrap();
        history.flush().unwrap();
        assert!(history.snapshot(None).is_empty());
        assert!(!path.exists());
        assert!(read_persisted_history_tail(&path, None).unwrap().is_empty());
        assert!(History::open(dir.path().join("too-large"), MAX_HISTORY_BYTES + 1).is_err());
    }

    #[test]
    fn config_rejects_oversized_profile_history() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        fs::write(
            &paths.config_file,
            format!(
                "version = 1\n[profiles.too_large]\nhistory_bytes = {}\n",
                MAX_HISTORY_BYTES + 1
            ),
        )
        .unwrap();

        let error = Config::load(&paths).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("profile \"too_large\" history_bytes"),
            "{message}"
        );
        assert!(
            message.contains(&MAX_HISTORY_BYTES.to_string()),
            "{message}"
        );
    }

    #[test]
    fn config_schema_rejects_unknown_fields_at_every_level() {
        for (label, text, unknown) in [
            (
                "root",
                "version = 1\ndefualt_engine = \"shell\"\n",
                "defualt_engine",
            ),
            (
                "engine",
                "version = 1\n[engines.custom]\ncommand = [\"true\"]\ncomand = [\"false\"]\n",
                "comand",
            ),
            (
                "profile",
                "version = 1\n[profiles.review]\nhistroy_bytes = 1024\n",
                "histroy_bytes",
            ),
            (
                "profile limits",
                "version = 1\n[profiles.review.limits]\nmemroy_bytes = 1024\n",
                "memroy_bytes",
            ),
            (
                "shortcut",
                "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofiel = \"review\"\n",
                "profiel",
            ),
        ] {
            let message = format!("{:#}", load_config_text(text).unwrap_err());
            assert!(message.contains("unknown field"), "{label}: {message}");
            assert!(message.contains(unknown), "{label}: {message}");
        }
    }

    #[test]
    fn config_semantics_reject_dangling_references_and_invalid_commands() {
        for (label, text, expected) in [
            (
                "default engine",
                "version = 1\ndefault_engine = \"missing\"\n",
                "default_engine \"missing\"",
            ),
            (
                "default profile",
                "version = 1\ndefault_profile = \"missing\"\n",
                "default_profile \"missing\"",
            ),
            (
                "profile engine",
                "version = 1\n[profiles.review]\nengine = \"missing\"\n",
                "profile \"review\" engine \"missing\"",
            ),
            (
                "shortcut engine",
                "version = 1\n[shortcuts.review]\nengine = \"missing\"\n",
                "shortcut \"review\" engine \"missing\"",
            ),
            (
                "shortcut profile",
                "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofile = \"missing\"\n",
                "shortcut \"review\" profile \"missing\"",
            ),
            (
                "shortcut profile engine",
                "version = 1\n[profiles.review]\nengine = \"claude\"\n[shortcuts.review]\nengine = \"codex\"\nprofile = \"review\"\n",
                "selects engine \"codex\", but profile \"review\" selects engine \"claude\"",
            ),
            (
                "empty engine command",
                "version = 1\n[engines.shell]\ncommand = []\n",
                "engine \"shell\" command must not be empty",
            ),
            (
                "empty profile command",
                "version = 1\n[profiles.review]\ncommand = []\n",
                "profile \"review\" command must not be empty",
            ),
            (
                "empty profile executable",
                "version = 1\n[profiles.review]\nexecutable = \"\"\n",
                "profile \"review\" executable must not be empty",
            ),
            (
                "ignored profile executable",
                "version = 1\n[profiles.review]\ncommand = [\"true\"]\nexecutable = \"false\"\n",
                "cannot set both command and executable",
            ),
            (
                "ignored profile args",
                "version = 1\n[profiles.review]\ncommand = [\"true\"]\nargs = [\"--ignored\"]\n",
                "cannot set both command and args",
            ),
        ] {
            let message = format!("{:#}", load_config_text(text).unwrap_err());
            assert!(message.contains(expected), "{label}: {message}");
        }
    }

    #[test]
    fn config_semantics_reject_invalid_numeric_limits() {
        for (label, field, expected) in [
            ("memory", "memory_bytes = 0", "memory_bytes must be greater"),
            ("pids", "pids = 0", "pids must be greater"),
            ("quota", "cpu_quota_us = 0", "cpu_quota_us must be greater"),
            (
                "period",
                "cpu_quota_us = 1\ncpu_period_us = 0",
                "cpu_period_us must be greater",
            ),
            (
                "orphan period",
                "cpu_period_us = 100000",
                "cpu_period_us requires cpu_quota_us",
            ),
        ] {
            let text = format!("version = 1\n[profiles.review.limits]\n{field}\n");
            let message = format!("{:#}", load_config_text(&text).unwrap_err());
            assert!(message.contains(expected), "{label}: {message}");
        }

        let config = load_config_text("version = 1\n").unwrap();
        let error = config
            .resolve(
                vec!["/bin/true".into()],
                Some("shell"),
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits {
                    pids: Some(0),
                    ..Limits::default()
                },
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("resolved launch limits pids"),
            "{error:#}"
        );
    }

    #[test]
    fn valid_config_references_commands_and_limits_still_load() {
        let config = load_config_text(
            "version = 1\n\
             default_engine = \"custom\"\n\
             default_profile = \"review\"\n\
             [engines.custom]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"CUSTOM_SECRET\"]\n\
             [profiles.review]\n\
             engine = \"custom\"\n\
             args = [\"--review\"]\n\
             history_bytes = 0\n\
             [profiles.review.limits]\n\
             memory_bytes = 1048576\n\
             pids = 4\n\
             cpu_quota_us = 50000\n\
             cpu_period_us = 100000\n\
             [shortcuts.rev]\n\
             engine = \"custom\"\n\
             profile = \"review\"\n",
        )
        .unwrap();

        assert_eq!(config.default_engine.as_deref(), Some("custom"));
        assert_eq!(config.default_profile.as_deref(), Some("review"));
        for (name, shortcut) in &config.shortcuts {
            assert!(config.engines.contains_key(&shortcut.engine), "{name}");
            if let Some(profile) = &shortcut.profile {
                assert!(config.profiles.contains_key(profile), "{name}");
            }
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn kill_grace_is_bounded_before_duration_or_deadline_math() {
        assert_eq!(
            kill_grace_duration(MAX_KILL_GRACE_MS).unwrap(),
            Duration::from_millis(MAX_KILL_GRACE_MS)
        );
        assert!(kill_grace_duration(MAX_KILL_GRACE_MS + 1).is_err());
        assert!(kill_grace_duration(u64::MAX).is_err());
    }

    #[test]
    fn cgroup_counter_read_errors_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("cgroup.events");

        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "frozen 0\n").unwrap();
        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "populated nope\n").unwrap();
        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "populated 1\n").unwrap();
        assert_eq!(read_counter(&events, "populated").unwrap(), 1);
    }

    #[test]
    fn recorded_cgroup_cleanup_checks_deadline_before_locator_io() {
        let id = Uuid::new_v4();
        let locator = PathBuf::from(format!("/sys/fs/cgroup/aplexer-workload-{id}.scope"));
        let error = cleanup_recorded_cgroup_until(
            id,
            &locator,
            None,
            libc::SIGKILL,
            Duration::ZERO,
            Instant::now(),
        )
        .expect_err("expired cleanup must stop before locator inspection");
        assert!(error.to_string().contains("timed out validating"));
    }

    #[test]
    fn recorded_cgroup_cleanup_rejects_untrusted_locator() {
        let id = Uuid::new_v4();
        let identity = current_cgroup_identity().unwrap();
        let error = cleanup_recorded_cgroup_until(
            id,
            Path::new("/tmp/not-a-cgroup"),
            Some(&identity),
            libc::SIGKILL,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("untrusted locator must fail closed");
        assert!(error
            .to_string()
            .contains("untrusted recorded cgroup locator"));
    }

    #[test]
    fn cgroup_identity_captures_current_v2_kernel_domain() {
        let identity = current_cgroup_identity().unwrap();
        assert_eq!(identity.boot_id, linux_boot_id().unwrap());
        assert_ne!(identity.cgroup_namespace_inode, 0);
        assert_ne!(identity.mount_namespace_inode, 0);
        assert_ne!(identity.cgroup_mount_id, 0);
        assert_ne!(identity.cgroup_root_inode, 0);
        ensure_cgroup2_filesystem(Path::new(CGROUP_V2_ROOT)).unwrap();
    }

    #[test]
    fn live_cgroup_disappearance_is_empty_only_in_matching_domain() {
        let identity = current_cgroup_identity().unwrap();
        let missing_path =
            Path::new(CGROUP_V2_ROOT).join(format!("aplexer-workload-{}.scope", Uuid::new_v4()));
        assert!(!missing_path.exists());
        let collected = Cgroup {
            path: missing_path,
            identity: identity.clone(),
            anchor: Arc::new(Mutex::new(None)),
            initial_oom_kill: 0,
        };
        assert!(!collected.populated().unwrap());

        assert!(!live_cgroup_populated_with(&identity, || {
            Err(io::Error::from(io::ErrorKind::NotFound).into())
        })
        .unwrap());

        let mut wrong_mount = identity.clone();
        wrong_mount.cgroup_mount_id ^= 1;
        let mismatch = live_cgroup_populated_with(&wrong_mount, || {
            panic!("membership must not be read in a mismatched kernel domain")
        })
        .expect_err("mismatched identity must fail closed");
        assert!(mismatch.to_string().contains("before reading membership"));

        let malformed =
            live_cgroup_populated_with(&identity, || Err(anyhow!("malformed cgroup.events")))
                .expect_err("non-ENOENT membership errors must fail closed");
        assert!(malformed
            .to_string()
            .contains("read live cgroup membership"));
    }

    #[test]
    fn control_group_locator_is_uuid_bound_and_cannot_escape_root() {
        let id = Uuid::new_v4();
        let valid = format!("/user.slice/user-1000.slice/aplexer-workload-{id}.scope");
        assert_eq!(
            control_group_locator(id, &valid).unwrap(),
            Path::new(CGROUP_V2_ROOT).join(valid.trim_start_matches('/'))
        );
        assert!(
            control_group_locator(id, &format!("/user.slice/../aplexer-workload-{id}.scope"))
                .is_err()
        );
        assert!(control_group_locator(id, "relative.scope").is_err());
        assert!(control_group_locator(
            id,
            &format!("/user.slice/aplexer-workload-{}.scope", Uuid::new_v4())
        )
        .is_err());
    }

    #[test]
    fn scope_wait_retries_empty_control_group_then_accepts_valid_path() {
        let dir = tempfile::tempdir().unwrap();
        let systemctl = dir.path().join("systemctl");
        let id = Uuid::new_v4();
        let unit = format!("aplexer-workload-{id}");
        let reported = format!("/user.slice/{unit}.scope");
        fs::write(
            &systemctl,
            format!(
                "#!/bin/sh\nif [ ! -e \"$0.seen\" ]; then : > \"$0.seen\"; printf '\\n'; else printf '%s\\n' '{}'; fi\n",
                reported
            ),
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();

        let mut validations = 0;
        let path = wait_for_scope_cgroup_with(
            id,
            &unit,
            &systemctl,
            "--user",
            Duration::from_secs(1),
            |path| {
                validations += 1;
                Ok(Some(path.to_path_buf()))
            },
        )
        .unwrap();

        assert_eq!(
            path,
            Path::new(CGROUP_V2_ROOT).join(reported.trim_start_matches('/'))
        );
        assert_eq!(validations, 1, "empty value must not reach validation");
        assert!(systemctl.with_extension("seen").exists());
    }

    #[test]
    fn system_helpers_resolve_without_ambient_path() {
        for helper in ["systemd-run", "systemctl", "sleep"] {
            let path = trusted_system_helper(helper).unwrap();
            assert!(path.is_absolute());
            assert_eq!(fs::metadata(path).unwrap().uid(), 0);
        }

        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("systemctl");
        fs::write(&shadow, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&shadow, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_trusted_helper(&shadow).is_err());
    }

    #[test]
    fn missing_cgroup_leaf_requires_matching_persisted_identity() {
        let id = Uuid::new_v4();
        let locator = PathBuf::from(format!("{CGROUP_V2_ROOT}/aplexer-workload-{id}.scope"));
        let missing = validate_recorded_cgroup(id, &locator, None)
            .expect_err("legacy locator must not prove emptiness");
        assert!(missing
            .to_string()
            .contains("no boot/namespace/mount identity"));

        let mut wrong_boot = current_cgroup_identity().unwrap();
        wrong_boot.boot_id = Uuid::new_v4().to_string();
        let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_boot))
            .expect_err("cross-boot locator must not prove emptiness");
        assert!(mismatch.to_string().contains("does not match"));

        let mut wrong_mount = current_cgroup_identity().unwrap();
        wrong_mount.cgroup_mount_id = wrong_mount.cgroup_mount_id.saturating_add(1);
        let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_mount))
            .expect_err("replacement mount must not prove emptiness");
        assert!(mismatch.to_string().contains("does not match"));

        let identity = current_cgroup_identity().unwrap();
        assert_eq!(
            validate_recorded_cgroup(id, &locator, Some(&identity)).unwrap(),
            None,
            "same-domain missing cgroup is empty"
        );
    }

    #[test]
    fn cgroup_recovery_pidfds_preserve_descriptor_reserve() {
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE, 0),
            0
        );
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE + 7, 3),
            4
        );
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(u64::MAX, 0),
            MAX_CGROUP_RECOVERY_MEMBERS
        );
    }

    #[test]
    fn cgroup_member_fallback_uses_identity_pinned_signal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cgroup.procs"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        signal_cgroup_path_until(dir.path(), 0, Instant::now() + Duration::from_secs(1))
            .expect("pidfd signal-zero probe");
    }

    #[test]
    fn cgroup_setup_helper_obeys_wall_clock_deadline() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error = command_output_until(
            &mut command,
            Instant::now() + Duration::from_millis(50),
            "exercise setup timeout",
        )
        .expect_err("wedged setup helper must time out");
        assert!(error.to_string().contains("timed out waiting"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cgroup_setup_helper_collects_bounded_output() {
        let mut command = Command::new("/bin/printf");
        command.arg("/user.slice/example.scope\n");
        let output = command_output_until(
            &mut command,
            Instant::now() + Duration::from_secs(1),
            "exercise setup output",
        )
        .expect("short-lived setup helper");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"/user.slice/example.scope\n");
    }

    #[test]
    fn cgroup_setup_helper_pipe_cannot_outlive_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 0.2 & exit 0"]);
        let started = Instant::now();
        let error = command_output_until(
            &mut command,
            Instant::now() + Duration::from_millis(50),
            "exercise inherited output pipe",
        )
        .expect_err("inherited helper pipe must not defeat deadline");
        assert!(error.to_string().contains("timed out waiting"));
        assert!(started.elapsed() < Duration::from_millis(500));
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
    fn cgroup_anchor_release_owns_child_through_kill_and_reap() {
        let anchor = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let anchor_pid = anchor.id();
        let cgroup = Cgroup {
            path: PathBuf::from("/does/not/exist"),
            identity: current_cgroup_identity().unwrap(),
            anchor: Arc::new(Mutex::new(Some(anchor))),
            initial_oom_kill: 0,
        };
        let clone = cgroup.clone();

        cgroup.release_anchor().unwrap();
        assert!(cgroup.anchor.lock().unwrap().is_none());
        clone.release_anchor().unwrap();

        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(anchor_pid as libc::pid_t, &mut status, libc::WNOHANG) },
            -1,
            "anchor must already be reaped exactly once"
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn cgroup_anchor_release_retains_handle_when_reaping_fails() {
        let mut slot = Some(7_u8);
        let error = release_anchor_slot(&mut slot, |_| bail!("injected release failure"))
            .expect_err("release must fail");
        assert!(error.to_string().contains("injected release failure"));
        assert_eq!(slot, Some(7), "failed release must preserve ownership");
    }

    #[test]
    fn legacy_exit_info_remains_a_containment_proof() {
        let state = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(liveness_record(state.path())).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("containment_cgroup");
        object.remove("containment_cgroup_identity");
        object.remove("containment_empty");
        object.insert("phase".into(), serde_json::json!("exited"));
        object.insert(
            "exit".into(),
            serde_json::json!({
                "code": 0,
                "signal": null,
                "oom_killed": false,
                "exited_at_ms": 2
            }),
        );
        let terminal: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(terminal.containment_proven_empty());

        value
            .as_object_mut()
            .unwrap()
            .insert("containment_empty".into(), serde_json::json!(false));
        let explicit_failure: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(!explicit_failure.containment_proven_empty());

        value.as_object_mut().unwrap().remove("exit");
        value
            .as_object_mut()
            .unwrap()
            .insert("phase".into(), serde_json::json!("failed"));
        let ambiguous: SessionRecord = serde_json::from_value(value).unwrap();
        assert!(!ambiguous.containment_proven_empty());
    }

    #[test]
    fn session_metadata_keeps_only_transcript_roots() {
        let env = BTreeMap::from([
            ("CODEX_HOME".to_string(), "/profiles/codex".to_string()),
            ("API_TOKEN".to_string(), "secret".to_string()),
        ]);
        assert_eq!(
            session_metadata_env(&env),
            BTreeMap::from([("CODEX_HOME".to_string(), "/profiles/codex".to_string())])
        );
    }
    /// The load-bearing property from pocketshell-integration-plan.md 0.2: a
    /// custom engine's own (smaller/different) `env_unset` can only ADD to
    /// the forced provider-key union, never replace or shrink it.
    #[test]
    fn env_unset_union_is_forced() {
        let mut config = Config {
            default_engine: Some("custom".into()),
            ..Config::default()
        };
        config.engines.insert(
            "custom".into(),
            EngineConfig {
                command: vec!["true".into()],
                env: BTreeMap::new(),
                // deliberately includes a name already in the forced list
                // (to exercise dedup) plus one new name.
                env_unset: vec!["ANTHROPIC_API_KEY".into(), "MY_CUSTOM_VAR".into()],
                skip_permissions_argv: Vec::new(),
            },
        );
        let launch = config
            .resolve(
                Vec::new(),
                None,
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits::default(),
                None,
            )
            .unwrap();
        for name in PROVIDER_ENV_UNSET_VARS {
            assert!(
                launch.env_unset.iter().any(|v| v == name),
                "forced provider var {name} missing from env_unset"
            );
        }
        assert!(launch.env_unset.iter().any(|v| v == "MY_CUSTOM_VAR"));
        let count = launch
            .env_unset
            .iter()
            .filter(|v| v.as_str() == "ANTHROPIC_API_KEY")
            .count();
        assert_eq!(count, 1, "ANTHROPIC_API_KEY must not be duplicated");
        assert_eq!(
            launch.env_unset.len(),
            PROVIDER_ENV_UNSET_VARS.len() + 1,
            "union must be exactly the forced list plus the one new custom name"
        );
    }

    #[test]
    fn shell_env_unset_preserves_provider_overrides_and_configured_removals() {
        let config = load_config_text(
            "version = 1\n\
             [engines.shell]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"SHELL_SECRET\", \"SHELL_SECRET\"]\n",
        )
        .unwrap();
        let overrides = BTreeMap::from([
            ("OPENAI_API_KEY".into(), "literal-shell-value".into()),
            ("SHELL_SECRET".into(), "remove-me".into()),
        ]);
        let launch = config
            .resolve(
                vec!["/bin/true".into()],
                Some("shell"),
                None,
                Path::new("/tmp"),
                None,
                &overrides,
                &Limits::default(),
                None,
            )
            .unwrap();

        assert_eq!(
            launch.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("literal-shell-value")
        );
        assert_eq!(launch.env_unset, vec!["SHELL_SECRET"]);
        assert!(!launch.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));

        let agent = config
            .resolve(
                vec!["/bin/true".into()],
                Some("codex"),
                None,
                Path::new("/tmp"),
                None,
                &overrides,
                &Limits::default(),
                None,
            )
            .unwrap();
        assert!(agent.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));
    }

    #[test]
    fn skip_permissions_argv_ported_values() {
        let config = Config {
            engines: BTreeMap::from([(
                "claude".to_string(),
                EngineConfig {
                    command: vec!["claude".into()],
                    env: BTreeMap::new(),
                    env_unset: Vec::new(),
                    skip_permissions_argv: vec!["--dangerously-skip-permissions".into()],
                },
            )]),
            ..Config::default()
        };
        let launch = config
            .resolve(
                Vec::new(),
                Some("claude"),
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            launch.skip_permissions_argv,
            vec!["--dangerously-skip-permissions".to_string()]
        );
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

    #[test]
    fn explicit_relative_path_overrides_are_resolved_once() {
        let resolved = absolute_override_path(PathBuf::from("state"), "APLEXER_STATE_DIR").unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, env::current_dir().unwrap().join("state"));
    }

    #[test]
    fn xdg_paths_must_be_absolute() {
        let error = absolute_xdg_path(PathBuf::from("runtime"), "XDG_RUNTIME_DIR").unwrap_err();
        assert!(error.to_string().contains("must be an absolute path"));
        assert_eq!(
            absolute_xdg_path(PathBuf::from("/run/user/1000"), "XDG_RUNTIME_DIR").unwrap(),
            PathBuf::from("/run/user/1000")
        );
    }
}
