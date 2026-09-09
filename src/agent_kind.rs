//! Which coding agent is running inside a session, detected at query time.
//!
//! Every pocketshell-created aplexer session is `engine: "shell"` with the
//! agent launched by hand inside it, so `engine` cannot name the agent. What
//! aplexer does own is the workload process (`SessionRecord::workload_pid`)
//! and, through `/proc/<pid>/task/*/children`, its whole descendant tree --
//! the same walk `worker::descendant_pids` uses for containment. This module
//! reuses that walker to answer "which agent is live in this session right
//! now" from the process tree instead of from configuration.
//!
//! The token rules mirror pocketshell's server-side classifier
//! (`tools/pocketshell/src/pocketshell/cgroup_agents.py`, itself mirroring
//! `AgentDetector.namesAgent`): a comm/cmdline names an agent when the
//! agent's command token appears as a whole word -- bounded by the start/end
//! of the string or by shell/path delimiters. That is what lets a bare
//! `codex` comm and a node-wrapped `node /…/bin/codex` cmdline both classify
//! as codex while `codex-helper` buried in an unrelated path does not.
//!
//! Two deliberate properties:
//!
//! * **Nothing is persisted.** Detection runs only when a caller asks for
//!   `a list --json` / `a snapshot` / `a status --json`. A record on disk
//!   never carries an `agent` field, so it cannot go stale, and the worker's
//!   hot path never pays for this.
//! * **Every read is defensive.** A pid that exits between the `children`
//!   read and the `comm` read, an unreadable `/proc` entry, a permission
//!   error -- all are skipped. Detection degrades to "no agent found"
//!   (`None`), never to an error that would fail the whole listing.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::Path;

use serde::Serialize;

/// The `/proc` root detection reads. Injectable so the unit tests classify a
/// synthetic tree with zero live processes.
pub const DEFAULT_PROC_ROOT: &str = "/proc";

/// Safety bound on a single detection walk. A session's workload subtree is
/// a handful of processes; this only stops a pathological (or hostile) tree
/// from turning a `a list --json` into an unbounded `/proc` scan.
const MAX_SCANNED_PIDS: usize = 4096;

/// An agent aplexer can recognise from a workload's process tree. The serde
/// representation is the lowercase name that appears on the wire, identical
/// to the kinds pocketshell's own classifier returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
    Grok,
}

impl AgentKind {
    /// The wire/display name, identical to this enum's serde representation.
    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
            AgentKind::Grok => "grok",
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Characters that may precede an agent's command token. Mirrors
/// `cgroup_agents.py`'s `_BOUNDARY_LEAD`: start-of-string, whitespace, or a
/// shell/path delimiter. Note `-` is deliberately absent, so `my-claude`
/// does not name claude.
const LEAD_DELIMITERS: &[u8] = b" \t\n\r\x0b\x0c/|;&('\"`";
/// Characters that may follow an agent's command token. Mirrors
/// `_BOUNDARY_TAIL`, which additionally allows `)` and `:`.
const TAIL_DELIMITERS: &[u8] = b" \t\n\r\x0b\x0c/|;&):'\"`";

/// One agent's command-token rule, expressed as the literal alternatives the
/// Python/Kotlin regexes accept.
struct TokenRule {
    kind: AgentKind,
    /// Literal stems the token may start with (regex alternation).
    stems: &'static [&'static str],
    /// Optional literal continuations directly after a stem (`claude` also
    /// matches `claudecode` and `claude-code`, per `claude(?:-?code)?`).
    literal_suffixes: &'static [&'static str],
    /// Whether a `[-_][a-z0-9]+` continuation is accepted after the stem,
    /// the tail of `open[-_]?code(?:[-_][a-z0-9]+)?`.
    alnum_suffix: bool,
}

/// Rules in the same order `cgroup_agents.py` applies them, with one
/// aplexer-local addition: `zcodex` (the codex-rs build this box runs as
/// `…/codex-rs/target/dev-small/zcodex`) is absent from pocketshell's
/// classifier, and without a rule the `z` lead kills the codex whole-word
/// match, so every zcodex session would degrade to plain shell. It classifies
/// as `Codex`, matching how aplexer treats it everywhere else
/// (`config::engine_family` maps the engine variant onto codex: same hooks,
/// same `CODEX_HOME`, same wire protocol -- the binary name is the only
/// difference). Order only matters in the impossible case of one string
/// naming two agents: no string matches both the `zcodex` and `codex` rules,
/// since `codex` inside `zcodex` never sits at a lead boundary.
const TOKEN_RULES: &[TokenRule] = &[
    TokenRule {
        kind: AgentKind::Claude,
        stems: &["claude"],
        literal_suffixes: &["code", "-code"],
        alnum_suffix: false,
    },
    TokenRule {
        kind: AgentKind::Codex,
        stems: &["zcodex"],
        literal_suffixes: &[],
        alnum_suffix: false,
    },
    TokenRule {
        kind: AgentKind::Codex,
        stems: &["codex"],
        literal_suffixes: &[],
        alnum_suffix: false,
    },
    TokenRule {
        kind: AgentKind::Opencode,
        stems: &["opencode", "open-code", "open_code"],
        literal_suffixes: &[],
        alnum_suffix: true,
    },
    TokenRule {
        kind: AgentKind::Grok,
        stems: &["grok"],
        literal_suffixes: &[],
        alnum_suffix: false,
    },
];

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| offset + from)
}

fn is_lead_boundary(bytes: &[u8], start: usize) -> bool {
    start == 0 || LEAD_DELIMITERS.contains(&bytes[start - 1])
}

fn is_tail_boundary(bytes: &[u8], end: usize) -> bool {
    end == bytes.len() || TAIL_DELIMITERS.contains(&bytes[end])
}

/// End of a `[-_][a-z0-9]+` continuation starting at `start`, if present.
/// Only the longest run is considered: any shorter one would end on an
/// alphanumeric character, which is never a tail boundary.
fn alnum_suffix_end(bytes: &[u8], start: usize) -> Option<usize> {
    if !matches!(bytes.get(start), Some(b'-') | Some(b'_')) {
        return None;
    }
    let mut end = start + 1;
    while matches!(bytes.get(end), Some(byte) if byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        end += 1;
    }
    (end > start + 1).then_some(end)
}

impl TokenRule {
    /// Whether `lowered` (already lowercased) contains this rule's command
    /// token as a whole word.
    fn matches(&self, lowered: &str) -> bool {
        let bytes = lowered.as_bytes();
        for stem in self.stems {
            let stem = stem.as_bytes();
            let mut cursor = 0;
            while let Some(start) = find_from(bytes, stem, cursor) {
                cursor = start + 1;
                if !is_lead_boundary(bytes, start) {
                    continue;
                }
                let stem_end = start + stem.len();
                if is_tail_boundary(bytes, stem_end) {
                    return true;
                }
                for suffix in self.literal_suffixes {
                    let end = stem_end + suffix.len();
                    if bytes.len() >= end
                        && &bytes[stem_end..end] == suffix.as_bytes()
                        && is_tail_boundary(bytes, end)
                    {
                        return true;
                    }
                }
                if self.alnum_suffix {
                    if let Some(end) = alnum_suffix_end(bytes, stem_end) {
                        if is_tail_boundary(bytes, end) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

/// The agent named by a single comm or cmdline string, if any.
///
/// Mirrors `cgroup_agents.py::classify_token`: the text is lowercased first
/// and the rules themselves are lowercase, so `CLAUDE` and `claude` classify
/// identically.
pub fn classify_token(text: &str) -> Option<AgentKind> {
    let lowered = text.to_lowercase();
    TOKEN_RULES
        .iter()
        .find(|rule| rule.matches(&lowered))
        .map(|rule| rule.kind)
}

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

/// Classify one pid by its `comm` first (cheap and definitive for an
/// unwrapped CLI) and then its `cmdline` (which catches the node-wrapped
/// form whose comm is just `node`).
fn classify_pid(proc_root: &Path, pid: u32) -> Option<AgentKind> {
    if let Some(comm) = read_comm(proc_root, pid) {
        if let Some(kind) = classify_token(&comm) {
            return Some(kind);
        }
    }
    let cmdline = read_cmdline(proc_root, pid)?;
    if cmdline.is_empty() {
        return None;
    }
    classify_token(&cmdline)
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
/// Never fails: an unreadable pid, a `children` file that vanished
/// mid-walk, or a pid that exited between enumeration and classification is
/// skipped. `None` means "no agent found", which is also the honest answer
/// for a workload that is no longer alive.
pub fn detect_agent(proc_root: &Path, workload_pid: u32) -> Option<AgentKind> {
    let mut pending = VecDeque::from([workload_pid]);
    let mut seen = HashSet::from([workload_pid]);
    let mut scanned = 0usize;
    while let Some(pid) = pending.pop_front() {
        scanned += 1;
        if scanned > MAX_SCANNED_PIDS {
            return None;
        }
        if let Some(kind) = classify_pid(proc_root, pid) {
            return Some(kind);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Materialise one pid in a synthetic `/proc`: `comm`, `cmdline`, and the
    /// `task/<pid>/children` file the descendant walker reads.
    fn write_proc(root: &Path, pid: u32, comm: &str, cmdline: &[&str], children: &[u32]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        let mut raw = Vec::new();
        for arg in cmdline {
            raw.extend_from_slice(arg.as_bytes());
            raw.push(0);
        }
        fs::write(dir.join("cmdline"), raw).unwrap();
        let task = dir.join("task").join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        let listed = children
            .iter()
            .map(|child| child.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        fs::write(task.join("children"), format!("{listed}\n")).unwrap();
    }

    fn proc_tree() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn claude_under_bash_is_detected() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 100, "bash", &["/bin/bash", "-l"], &[101]);
        write_proc(&root, 101, "claude", &["claude"], &[]);

        assert_eq!(detect_agent(&root, 100), Some(AgentKind::Claude));
    }

    #[test]
    fn node_wrapped_codex_is_detected() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 200, "bash", &["/bin/bash", "-l"], &[201]);
        write_proc(
            &root,
            201,
            "node",
            &["node", "/home/alexey/.local/bin/codex", "--model", "gpt"],
            &[],
        );

        assert_eq!(detect_agent(&root, 200), Some(AgentKind::Codex));
    }

    #[test]
    fn zcodex_under_bash_is_detected_as_codex() {
        // The shape every zcodex session on this box has: a login shell whose
        // child is the codex-rs dev build, `comm` = `zcodex`. It is a codex
        // variant, so it reports the codex kind.
        let (_dir, root) = proc_tree();
        write_proc(&root, 210, "bash", &["/bin/bash", "-l"], &[211]);
        write_proc(
            &root,
            211,
            "zcodex",
            &[
                "/home/alexey/git/codex-zcode/codex-rs/target/dev-small/zcodex",
                "--dangerously-bypass-approvals-and-sandbox",
            ],
            &[],
        );

        assert_eq!(detect_agent(&root, 210), Some(AgentKind::Codex));
    }

    #[test]
    fn codex_helper_path_alone_does_not_match() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 300, "bash", &["/bin/bash", "-l"], &[301]);
        write_proc(
            &root,
            301,
            "node",
            &["node", "/opt/tools/codex-helper/index.js"],
            &[],
        );

        assert_eq!(detect_agent(&root, 300), None);
    }

    #[test]
    fn workload_without_descendants_has_no_agent() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 400, "bash", &["/bin/bash", "-l"], &[]);

        assert_eq!(detect_agent(&root, 400), None);
    }

    #[test]
    fn workload_leader_itself_can_be_the_agent() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 500, "claude", &["claude"], &[]);

        assert_eq!(detect_agent(&root, 500), Some(AgentKind::Claude));
    }

    #[test]
    fn agent_nested_several_levels_below_the_workload_is_found() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 600, "bash", &["/bin/bash", "-l"], &[601]);
        write_proc(&root, 601, "sh", &["/bin/sh", "-c", "run"], &[602]);
        write_proc(&root, 602, "tmux", &["tmux", "attach"], &[603]);
        write_proc(&root, 603, "grok", &["grok", "--always-approve"], &[]);

        assert_eq!(detect_agent(&root, 600), Some(AgentKind::Grok));
    }

    #[test]
    fn vanished_pid_mid_walk_is_skipped_and_the_live_sibling_still_matches() {
        let (_dir, root) = proc_tree();
        // 701 is listed as a child but its /proc entry is gone -- exactly what
        // a process exiting between the children read and the comm read
        // looks like.
        write_proc(&root, 700, "bash", &["/bin/bash", "-l"], &[701, 702]);
        write_proc(&root, 702, "claude", &["claude"], &[]);

        assert_eq!(detect_agent(&root, 700), Some(AgentKind::Claude));
    }

    #[test]
    fn every_descendant_vanishing_yields_none_without_error() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 800, "bash", &["/bin/bash", "-l"], &[801, 802]);

        assert_eq!(detect_agent(&root, 800), None);
    }

    #[test]
    fn missing_workload_pid_yields_none_without_error() {
        let (_dir, root) = proc_tree();

        assert_eq!(detect_agent(&root, 999_999), None);
    }

    #[test]
    fn a_child_cycle_cannot_loop_forever() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 900, "bash", &["/bin/bash"], &[901]);
        write_proc(&root, 901, "sh", &["/bin/sh"], &[900, 901]);

        assert_eq!(detect_agent(&root, 900), None);
    }

    #[test]
    fn shallower_match_wins_over_a_deeper_one() {
        let (_dir, root) = proc_tree();
        write_proc(&root, 1000, "bash", &["/bin/bash"], &[1001, 1002]);
        write_proc(&root, 1001, "codex", &["codex"], &[1003]);
        write_proc(&root, 1002, "sh", &["/bin/sh"], &[]);
        write_proc(&root, 1003, "claude", &["claude"], &[]);

        assert_eq!(detect_agent(&root, 1000), Some(AgentKind::Codex));
    }

    #[test]
    fn command_tokens_are_matched_as_whole_words() {
        for (text, expected) in [
            ("claude", Some(AgentKind::Claude)),
            ("claude-code", Some(AgentKind::Claude)),
            ("claudecode", Some(AgentKind::Claude)),
            ("/usr/local/bin/claude --resume", Some(AgentKind::Claude)),
            ("sh -c 'claude'", Some(AgentKind::Claude)),
            ("node /home/a/.bun/bin/codex", Some(AgentKind::Codex)),
            ("codex exec", Some(AgentKind::Codex)),
            ("zcodex", Some(AgentKind::Codex)),
            ("/opt/dev-small/zcodex -c key=value", Some(AgentKind::Codex)),
            ("zcodex-helper", None),
            ("azcodex", None),
            ("opencode", Some(AgentKind::Opencode)),
            ("open-code", Some(AgentKind::Opencode)),
            ("open_code", Some(AgentKind::Opencode)),
            ("opencode-dev run", Some(AgentKind::Opencode)),
            ("grok --always-approve", Some(AgentKind::Grok)),
            ("(grok)", Some(AgentKind::Grok)),
            ("bash -l", None),
            ("claudette", None),
            ("my-claude", None),
            ("codex-helper", None),
            ("/opt/codex-helper/run", None),
            ("grokking", None),
            ("xcodex", None),
            ("", None),
        ] {
            assert_eq!(classify_token(text), expected, "classifying {text:?}");
        }
    }

    #[test]
    fn tokens_are_lowercase_rules_applied_to_lowercased_text() {
        // Mirrors cgroup_agents.py::classify_token, which lowercases the comm
        // /cmdline (`lowered = text.lower()`) before applying the same
        // lowercase token patterns -- so the rules are lowercase-only, and an
        // upper/mixed-case command still classifies.
        assert_eq!(classify_token("CLAUDE"), Some(AgentKind::Claude));
        assert_eq!(classify_token("/usr/bin/Codex"), Some(AgentKind::Codex));
    }

    #[test]
    fn comm_is_classified_before_cmdline() {
        let (_dir, root) = proc_tree();
        // A wrapper whose comm already names the agent must not need its
        // cmdline read at all -- the cmdline here names nothing.
        write_proc(&root, 1100, "claude", &["-zsh"], &[]);

        assert_eq!(detect_agent(&root, 1100), Some(AgentKind::Claude));
    }

    #[test]
    fn agent_kind_serialises_to_its_lowercase_name() {
        for kind in [
            AgentKind::Claude,
            AgentKind::Codex,
            AgentKind::Opencode,
            AgentKind::Grok,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.name().to_owned()),
            );
        }
        assert_eq!(
            serde_json::to_value(Option::<AgentKind>::None).unwrap(),
            serde_json::Value::Null,
        );
    }
}
