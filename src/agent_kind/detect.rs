//! The `/proc` descendant walk that turns comm/cmdline evidence into a
//! detected agent, plus the profile-resolution read off the process's
//! environment.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::Path;

use super::rules::classify_token_detailed;
use super::{AgentKind, DetectedAgent, ProfileVariants};

/// Safety bound on a single detection walk. A session's workload subtree is
/// a handful of processes; this only stops a pathological (or hostile) tree
/// from turning a `a list --json` into an unbounded `/proc` scan.
const MAX_SCANNED_PIDS: usize = 4096;

/// `/proc/<pid>/comm`, trimmed. `None` when the pid is gone or unreadable.
fn read_comm(proc_root: &Path, pid: u32) -> Option<String> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("comm")).ok()?;
    Some(text.trim().to_owned())
}

/// `/proc/<pid>/cmdline` (NUL-delimited) joined with spaces. `None` when the
/// pid is gone or unreadable; an empty string for a kernel thread.
fn read_cmdline(proc_root: &Path, pid: u32) -> Option<String> {
    let raw = fs::read(proc_root.join(pid.to_string()).join("cmdline")).ok()?;
    let decoded = String::from_utf8_lossy(&raw);
    Some(
        decoded
            .split('\0')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_owned(),
    )
}

/// The value of `var` in `/proc/<pid>/environ` (NUL-delimited `KEY=value`
/// words). `None` when the pid is gone, unreadable, or does not carry the
/// variable -- the same defensive read every other /proc probe here does.
fn read_environ_var(proc_root: &Path, pid: u32, var: &str) -> Option<String> {
    let raw = fs::read(proc_root.join(pid.to_string()).join("environ")).ok()?;
    let prefix = format!("{var}=");
    String::from_utf8_lossy(&raw)
        .split('\0')
        .find(|word| word.starts_with(&prefix))
        .map(|word| word[prefix.len()..].to_owned())
}

/// The profile id of the variation `kind` is running as on `pid`, from the
/// same evidence `config::discovery` registers profiles from:
///
/// * A non-default profile env (`CODEX_HOME`/`CLAUDE_CONFIG_DIR`, per
///   `config::discovery`'s rule table) wins: its dir's stem minus the
///   leading dot is the profile id, matching discovery's keying exactly. An
///   env value pointing at the *default* dir names no variation, so the
///   token rule still gets its say.
/// * Otherwise the variation token the process was classified by
///   (`profile_variants`): a session running a configured variation's
///   binary is that profile even with no env override -- the usual
///   hand-launched shape.
/// * Anything else is the engine's own default config: `None`.
///
/// Agents without a profile env rule (opencode, grok) can only ever be the
/// default profile or a configured variation token, so their env arm never
/// fires.
fn resolve_profile(
    proc_root: &Path,
    pid: u32,
    kind: AgentKind,
    variant: Option<&str>,
) -> Option<String> {
    if let Some((env_var, default_dirname)) = kind.profile_env() {
        if let Some(dir) = read_environ_var(proc_root, pid, env_var) {
            let stem = Path::new(&dir)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.trim_start_matches('.').to_owned())
                .filter(|stem| !stem.is_empty());
            if stem.as_deref() != Some(default_dirname.trim_start_matches('.')) {
                return stem;
            }
        }
    }
    variant.map(str::to_owned)
}

/// Classify one pid by its `comm` first (cheap and definitive for an
/// unwrapped CLI) and then its `cmdline` (which catches the node-wrapped
/// form whose comm is just `node`).
fn classify_pid(
    proc_root: &Path,
    pid: u32,
    variants: &ProfileVariants,
) -> Option<(AgentKind, Option<String>)> {
    if let Some(comm) = read_comm(proc_root, pid) {
        if let Some(named) = classify_token_detailed(&comm, variants) {
            return Some(named);
        }
    }
    let cmdline = read_cmdline(proc_root, pid)?;
    if cmdline.is_empty() {
        return None;
    }
    classify_token_detailed(&cmdline, variants)
}

/// The agent running in `workload_pid`'s process tree, or `None`.
///
/// The walk is breadth-first from the workload leader itself (a session
/// started directly as `a start -- claude` has the agent AS its workload,
/// while a shell session has it one or more levels below), visiting each
/// level's children in ascending pid order so the answer is deterministic
/// rather than dependent on directory-read order. The first pid whose
/// comm/cmdline names an agent wins.
///
/// `variants` is [`super::profile_variants`] over the caller's loaded
/// config -- the variation tokens (`zcodex`, a user profile's executable,
/// ...) whose binary names classify as that variation. Canonical agent
/// commands need no entry and empty variants simply detect canonical agents
/// only.
///
/// Never fails: an unreadable pid, a `children` file that vanished
/// mid-walk, or a pid that exited between enumeration and classification is
/// skipped. `None` means "no agent found", which is also the honest answer
/// for a workload that is no longer alive.
pub fn detect_agent(
    proc_root: &Path,
    workload_pid: u32,
    variants: &ProfileVariants,
) -> Option<AgentKind> {
    detect_agent_detailed(proc_root, workload_pid, variants).map(|detected| detected.kind)
}

/// `detect_agent` plus the variation the agent runs as (`DetectedAgent`).
pub fn detect_agent_detailed(
    proc_root: &Path,
    workload_pid: u32,
    variants: &ProfileVariants,
) -> Option<DetectedAgent> {
    let mut pending = VecDeque::from([workload_pid]);
    let mut seen = HashSet::from([workload_pid]);
    let mut scanned = 0usize;
    while let Some(pid) = pending.pop_front() {
        scanned += 1;
        if scanned > MAX_SCANNED_PIDS {
            return None;
        }
        if let Some((kind, variant)) = classify_pid(proc_root, pid, variants) {
            return Some(DetectedAgent {
                kind,
                profile: resolve_profile(proc_root, pid, kind, variant.as_deref()),
            });
        }
        // A read error here is "this pid told us nothing", never a failure:
        // the strict, error-propagating variant is the containment walker's
        // contract (a truncated kill list is a bug), not detection's.
        let mut children = crate::worker::direct_child_pids_in(proc_root, pid).unwrap_or_default();
        children.sort_unstable();
        for child in children {
            if seen.insert(child) {
                pending.push_back(child);
            }
        }
    }
    None
}
