use aplexer::{process_start_time_ticks, read_record, Phase};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    workspace: TempDir,
    config: PathBuf,
    ids: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            workspace,
            config,
            ids: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run a")
    }

    fn start(&mut self, tag: &str, command: &[&str]) -> Value {
        let workspace = self.workspace.path().to_str().unwrap().to_owned();
        let mut args = vec![
            "--json",
            "start",
            "--workspace",
            &workspace,
            "--tag",
            tag,
            "--",
        ];
        args.extend_from_slice(command);
        let output = self.output(&args);
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let started: Value = serde_json::from_slice(&output.stdout).expect("start JSON");
        self.ids
            .push(started["id"].as_str().expect("session id").to_owned());
        started
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.state
            .path()
            .join("sessions")
            .join(id)
            .join("session.json")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for id in &self.ids {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

#[derive(Debug)]
struct ThreadCounter {
    status_path: PathBuf,
    before: u64,
}

fn voluntary_switches(path: &Path) -> u64 {
    let status = fs::read_to_string(path).expect("read thread status");
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("voluntary_ctxt_switches:")
                .and_then(|value| value.trim().parse().ok())
        })
        .expect("voluntary_ctxt_switches")
}

fn idle_thread_counters(worker_pid: u32) -> Vec<ThreadCounter> {
    let task_dir = PathBuf::from(format!("/proc/{worker_pid}/task"));
    let mut counters = Vec::new();
    for entry in fs::read_dir(&task_dir).expect("read worker tasks") {
        let entry = entry.expect("read worker task");
        let name = fs::read_to_string(entry.path().join("comm"))
            .expect("read thread name")
            .trim()
            .to_owned();
        if name.starts_with("aplexer-termin") || name.starts_with("aplexer-lifec") {
            let status_path = entry.path().join("status");
            counters.push(ThreadCounter {
                before: voluntary_switches(&status_path),
                status_path,
            });
        }
    }
    counters
}

fn wait_for_idle_threads(worker_pid: u32) -> Vec<ThreadCounter> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let counters = idle_thread_counters(worker_pid);
        if counters.len() == 2 {
            return counters;
        }
        assert!(
            Instant::now() < deadline,
            "worker {worker_pid} did not expose termination and lifecycle threads: {counters:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn multiple_idle_workers_do_not_timer_wake_termination_or_lifecycle_threads() {
    const SESSION_COUNT: usize = 3;
    let mut harness = Harness::new();
    let mut workers = Vec::new();
    for index in 0..SESSION_COUNT {
        let tag = format!("idle-{index}");
        let started = harness.start(&tag, &["/bin/sleep", "300"]);
        workers.push(started["worker_pid"].as_u64().expect("worker pid") as u32);
    }

    let counters: Vec<_> = workers
        .into_iter()
        .flat_map(wait_for_idle_threads)
        .collect();
    assert_eq!(counters.len(), SESSION_COUNT * 2);
    thread::sleep(Duration::from_millis(350));

    let delta: u64 = counters
        .iter()
        .map(|counter| voluntary_switches(&counter.status_path) - counter.before)
        .sum();
    eprintln!(
        "idle wakeup measurement: {delta} voluntary switches in 350ms across \
         {} termination/lifecycle threads from {SESSION_COUNT} sessions",
        counters.len()
    );
    assert!(
        delta <= SESSION_COUNT as u64 * 2,
        "idle termination/lifecycle threads woke {delta} times in 350ms across \
         {SESSION_COUNT} sessions: {counters:?}"
    );
}

fn same_process_is_alive(pid: u32, start_time: u64) -> bool {
    process_start_time_ticks(pid).ok() == Some(start_time)
}

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "missing {}", path.display());
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sigterm_wakes_worker_and_kills_ignoring_setsid_descendant() {
    assert!(Path::new("/usr/bin/setsid").is_file(), "setsid is required");
    let mut harness = Harness::new();
    let marker = harness.runtime.path().join("descendant.pid");
    let marker_arg = marker.to_str().unwrap().to_owned();
    let script = "trap '' TERM; /usr/bin/setsid /bin/sh -c \
        'trap \"\" HUP TERM; echo $$ > \"$1\"; exec /bin/sleep 300' \
        descendant \"$1\" & wait";
    let started = harness.start(
        "termination-signal",
        &["/bin/sh", "-c", script, "aplexer-leader", &marker_arg],
    );
    let id = started["id"].as_str().unwrap();
    let worker_pid = started["worker_pid"].as_u64().unwrap() as u32;
    let workload_pid = started["workload_pid"].as_u64().unwrap() as u32;
    let descendant_pid = wait_for_pid_file(&marker);
    let worker_start = process_start_time_ticks(worker_pid).unwrap();
    let workload_start = process_start_time_ticks(workload_pid).unwrap();
    let descendant_start = process_start_time_ticks(descendant_pid).unwrap();

    let began = Instant::now();
    assert_eq!(
        unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = began + Duration::from_secs(3);
    let record = loop {
        let record = read_record(&harness.record_path(id)).expect("read session record");
        if !same_process_is_alive(worker_pid, worker_start)
            && !same_process_is_alive(workload_pid, workload_start)
            && !same_process_is_alive(descendant_pid, descendant_start)
            && matches!(record.phase, Phase::Exited | Phase::Failed)
        {
            break record;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not promptly terminate its whole containment domain: {record:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(record.phase, Phase::Exited, "unexpected terminal record");
    assert_eq!(record.containment_empty, Some(true));
    assert!(began.elapsed() < Duration::from_secs(3));
}
