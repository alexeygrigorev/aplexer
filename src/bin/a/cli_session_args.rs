use super::commands::DEFAULT_HUMAN_TAG;
use aplexer::DEFAULT_STARTUP_TIMEOUT_MS;
use clap::{Args, ValueEnum};
use clap_complete::Shell;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct QuickAttachArgs {
    /// 1-based index into the workspaces as shown by `a list`.
    pub(crate) workspace_index: usize,
    /// 1-based index into that workspace's sessions (list order), or a
    /// literal tag. Defaults to that workspace's first session.
    pub(crate) session: Option<String>,
}

#[derive(Args)]
pub(crate) struct QuickLaunchArgs {
    /// Usually `[engine [tag]]`; words naming no engine or shortcut run
    /// as a literal command
    pub(crate) rest: Vec<String>,
    /// Explicit tag, injected by main()'s rewrite of the `a -<tag>`
    /// shorthand (`a -review` -> `quick-launch --tag review`): pin the
    /// session tag while the words still pick engine/shortcut/command
    /// exactly as they do for bare `a -`. Not advertised as a typed flag;
    /// `a here` accepts it as the typed-out equivalent.
    #[arg(long, value_name = "TAG", hide = true)]
    pub(crate) tag: Option<String>,
}

#[derive(Args)]
pub(crate) struct CompletionsArgs {
    /// Shell to generate a completion script for.
    pub(crate) shell: Shell,
}

#[derive(Args)]
pub(crate) struct StartArgs {
    /// Workspace directory the session belongs to (default: the current directory)
    #[arg(long, default_value = ".")]
    pub(crate) workspace: PathBuf,
    // The terminal-first default tag is `main`, matching `a here` and `a -`.
    // Resolution of a bare target still falls back to `default` (see
    // `resolve`), so sessions created by older builds stay attachable.
    /// Session name within the workspace
    #[arg(long, default_value = DEFAULT_HUMAN_TAG)]
    pub(crate) tag: String,
    /// Engine id from `a engines`; defaults to the configured default engine, else shell
    #[arg(long)]
    pub(crate) engine: Option<String>,
    /// Profile id from `a profiles` -- an engine variant (e.g. another account)
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// Working directory for the launched process (default: the workspace)
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
    /// Extra environment variable for the process, KEY=VALUE (repeatable)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub(crate) env: Vec<String>,
    /// cgroup memory cap for the session, e.g. 512M or 2G
    #[arg(long)]
    pub(crate) memory: Option<String>,
    /// cgroup cap on the number of processes
    #[arg(long)]
    pub(crate) pids: Option<u64>,
    /// cgroup CPU quota per period, in microseconds (with --cpu-period-us)
    #[arg(long)]
    pub(crate) cpu_quota_us: Option<u64>,
    /// Length of the cgroup CPU period, in microseconds
    #[arg(long, default_value_t = 100_000)]
    pub(crate) cpu_period_us: u64,
    /// Output-history bytes kept for `a capture` and attach replay
    #[arg(long)]
    pub(crate) history_bytes: Option<usize>,
    /// Attach to the session as soon as it is up
    #[arg(long)]
    pub(crate) attach: bool,
    /// Give up if the workload has not started after this long
    #[arg(long, default_value_t = DEFAULT_STARTUP_TIMEOUT_MS)]
    pub(crate) startup_timeout_ms: u64,
    /// Keep the engine's confirmation/sandbox prompts. Default is to append
    /// the engine's skip-permissions argv (`--dangerously-bypass-approvals-
    /// and-sandbox` / `--dangerously-skip-permissions` / `--always-approve`).
    #[arg(long)]
    pub(crate) no_skip_permissions: bool,
    /// When the requested workspace+tag is already held by a live session,
    /// claim the next free `<tag>-2`, `<tag>-3`, … suffix instead of
    /// failing. `a new` implies this; `start` without it keeps the strict
    /// create-by-exact-tag contract.
    #[arg(long)]
    pub(crate) fresh: bool,
    /// With `--`, run this command instead of an engine
    #[arg(last = true, value_name = "COMMAND")]
    pub(crate) command: Vec<OsString>,
}

#[derive(Args)]
pub(crate) struct LaunchArgs {
    #[arg(long)]
    pub(crate) engine: Option<String>,
    #[arg(long)]
    pub(crate) profile: Option<String>,
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,
    /// Suppress the engine's `skip_permissions_argv` (see `EngineConfig`) --
    /// the phone/desktop send this to opt OUT; skip-permissions argv is
    /// appended by default (matches pocketshell's own
    /// `--skip-permissions/--no-skip-permissions` default=True).
    #[arg(long)]
    pub(crate) no_skip_permissions: bool,
}

#[derive(Args, Clone)]
pub(crate) struct TargetArgs {
    #[arg(
        value_name = "SESSION",
        help = "UUID/prefix, workspace:tag selector, or tag in the current workspace"
    )]
    pub(crate) selector: Option<String>,
    /// Workspace directory to resolve the tag in
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
    /// Tag to resolve in --workspace (or the current workspace)
    #[arg(long, value_name = "TAG")]
    pub(crate) tag: Option<String>,
}

#[derive(Args, Default)]
pub(crate) struct ListArgs {
    /// Only live sessions
    #[arg(long)]
    pub(crate) running: bool,
    /// Include exited sessions too (the list hides them by default)
    #[arg(long)]
    pub(crate) all: bool,
    /// Order workspaces in `a list` (and the `a N` numbers). Remembered
    /// until you pick another. Time sorts are newest-first.
    #[arg(long, value_enum, value_name = "KEY")]
    pub(crate) sort: Option<ListSort>,
}

/// How `a list` orders workspace groups. Session order *inside* a group
/// stays newest-created-first (`list_records`), matching `Ctrl-b 1-9`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum ListSort {
    /// Alphabetical workspace path.
    #[default]
    Name,
    /// When the newest session in the workspace was created.
    Created,
    /// When a session in the workspace was last attached.
    Accessed,
    /// Most recent agent/PTY activity in the workspace.
    Activity,
}

impl ListSort {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ListSort::Name => "name",
            ListSort::Created => "created",
            ListSort::Accessed => "accessed",
            ListSort::Activity => "activity",
        }
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "name" => Some(ListSort::Name),
            "created" => Some(ListSort::Created),
            "accessed" => Some(ListSort::Accessed),
            "activity" => Some(ListSort::Activity),
            _ => None,
        }
    }
}
#[derive(Args)]
pub(crate) struct AttachArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    /// Replay this many history bytes instead of the live screen
    #[arg(long)]
    pub(crate) history_bytes: Option<usize>,
    /// Attach without a client status bar or client key bindings
    #[arg(long)]
    pub(crate) no_status: bool,
    /// Attach even from inside another live aplexer session, rendering it
    /// nested in that session's pane. Normally refused with a hint to
    /// detach and switch instead.
    #[arg(long)]
    pub(crate) force: bool,
}
#[derive(Args)]
pub(crate) struct SendArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    /// Bytes to type into the session
    #[arg(value_name = "TEXT")]
    pub(crate) text: Option<String>,
    /// Read the bytes to send from stdin instead of TEXT
    #[arg(long)]
    pub(crate) stdin: bool,
    /// Interpret TEXT as hex and send the decoded bytes
    #[arg(long)]
    pub(crate) hex: bool,
    /// Append a newline, like pressing Enter
    #[arg(long)]
    pub(crate) enter: bool,
}
#[derive(Args)]
pub(crate) struct CaptureArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    /// How many history bytes to read (default: a recent tail)
    #[arg(long)]
    pub(crate) bytes: Option<usize>,
    /// Write the bytes to this file instead of stdout
    #[arg(short, long)]
    pub(crate) output: Option<PathBuf>,
    /// Capture the rendered current screen (docs/terminal-state-design.md
    /// section 8) instead of raw history bytes -- a "richer PocketShell
    /// preview" of what the session's screen actually looks like right now,
    /// for a few hundred to a few thousand bytes, rather than an arbitrary
    /// tail of the byte stream. Ignores --bytes.
    #[arg(long)]
    pub(crate) screen: bool,
    /// With --screen, emit plain text (`ScreenTracker::contents()`) instead
    /// of the paintable escape-sequence form.
    #[arg(long, requires = "screen")]
    pub(crate) plain: bool,
}
#[derive(Args)]
pub(crate) struct KillArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    /// Signal to send first before escalating to SIGKILL after --grace-ms.
    /// Defaults to HUP (session hangup): interactive shells started by
    /// `a start` (e.g. `bash -l`) legitimately ignore SIGTERM when they own
    /// the PTY as an interactive login shell, so a TERM-first default eats
    /// the whole grace and always escalates (benchmark PLAN P0.1). HUP is
    /// the conventional "terminal went away" signal and terminates both
    /// shells and typical agents/servers via the default disposition; pass
    /// --signal TERM explicitly for a graceful TERM-first shutdown.
    #[arg(long, default_value = "HUP")]
    pub(crate) signal: String,
    /// How long to wait after --signal before escalating to SIGKILL
    #[arg(long, default_value_t = 2_000)]
    pub(crate) grace_ms: u64,
}
#[derive(Args)]
pub(crate) struct ForgetArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    /// Acknowledge that uncontained workload processes may survive.
    #[arg(long, required = true)]
    pub(crate) force: bool,
}
#[derive(Args)]
pub(crate) struct WatchArgs {
    /// Currently the only supported output mode -- required explicitly
    /// rather than defaulted so a bare `a watch` fails loudly instead of
    /// silently assuming a format that isn't implemented.
    #[arg(long)]
    pub(crate) jsonl: bool,
    /// Also watch shell (non-agent) sessions. Default is agent-only (engine
    /// != "shell"), an explicit scope decision -- see src/watch.rs.
    #[arg(long)]
    pub(crate) all: bool,
    /// Only events for sessions in this workspace
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
}

/// `a transcript` -- parse a session's native engine conversation log into
/// heru UnifiedEvent JSONL for PocketShell (and `a transcript --follow`).
/// See src/agent_events.rs.
#[derive(Args)]
pub(crate) struct TranscriptArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    /// Only the last N events after `--kind` / `--after` / `--before`
    /// filtering. PocketShell's initial conversation pane is `--last 50`
    /// (or `--last 5` for a compact peek).
    #[arg(long, value_name = "N")]
    pub(crate) last: Option<usize>,
    /// Filter to one UnifiedEvent kind (message, tool_call, tool_result, error, usage)
    /// before applying --last/--after/--before.
    #[arg(long)]
    pub(crate) kind: Option<String>,
    /// Events with sequence > N. Catch-up / follow-resume cursor: PocketShell
    /// stores the last sequence it rendered and asks for everything after.
    #[arg(long, value_name = "SEQ")]
    pub(crate) after: Option<u64>,
    /// Events with sequence < N. Older page: combine with `--last` to walk
    /// backward (`--before 12 --last 20`).
    #[arg(long, value_name = "SEQ")]
    pub(crate) before: Option<u64>,
    /// After emitting the current page, keep watching the native log and
    /// print new events as the agent writes them (`tail -f` of parsed
    /// UnifiedEvent JSONL). Implies a long-lived stdout stream; Ctrl-C
    /// to stop. Combine with `--after` / `--last` for the initial page.
    #[arg(long)]
    pub(crate) follow: bool,
    /// Replace any native JSONL line longer than N bytes with a truncation
    /// marker before parsing, so one huge tool_result cannot balloon the
    /// read (PocketShell `agent-log --max-line-bytes`).
    #[arg(long, value_name = "N")]
    pub(crate) max_line_bytes: Option<usize>,
}

#[derive(Args)]
pub(crate) struct RenameArgs {
    /// Session to retag: UUID/prefix, workspace:tag, or tag in the current
    /// workspace. Omitted (with --tag given) renames the session this
    /// command runs inside, via APLEXER_SESSION_ID.
    #[arg(value_name = "SESSION")]
    pub(crate) selector: Option<String>,
    /// Workspace directory to resolve the selector in
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
    /// New tag for the session
    #[arg(long, value_name = "TAG")]
    pub(crate) tag: Option<String>,
}

#[derive(Args)]
pub(crate) struct StateReportArgs {
    /// idle: the agent finished its turn and is resting, nothing
    /// outstanding. waiting: blocked on a prompt/question and needs the
    /// user. working: actively producing/thinking. Matches PocketShell's
    /// SessionAgentState vocabulary (Idle/WaitingForInput/Working) one for
    /// one -- see SessionAgentState.kt in the pocketshell repo.
    pub(crate) state: ReportedState,
}

#[derive(Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub(crate) enum ReportedState {
    Idle,
    Waiting,
    Working,
}

impl ReportedState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ReportedState::Idle => "idle",
            ReportedState::Waiting => "waiting",
            ReportedState::Working => "working",
        }
    }
}

#[derive(Args)]
pub(crate) struct InitArgs {
    /// Only check whether hooks are installed; print the per-engine status
    /// and exit 0 when fully initialized, 1 otherwise. No files are
    /// touched. With `--json` this is the machine contract for automation:
    /// `a init --check --json` reports `{"initialized": bool, ...}`.
    #[arg(long)]
    pub(crate) check: bool,
    /// Remove aplexer's `state-report` hooks instead of installing them.
    /// Only hook entries containing `state-report` and our generated files
    /// are removed; everything else is left alone.
    #[arg(long)]
    pub(crate) uninstall: bool,
    /// Only act on one engine: claude, codex, zcodex (shares codex's
    /// CODEX_HOME config), grok, gemini, or opencode. Default: all engines.
    #[arg(long, value_name = "ENGINE")]
    pub(crate) engine: Option<String>,
}

// -- Inter-agent messaging (docs/inter-agent-messaging-design.md, section 7) --
