use super::cli_examples::*;
use super::commands::{run, DEFAULT_HUMAN_TAG};
use aplexer::DEFAULT_STARTUP_TIMEOUT_MS;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use std::ffi::OsString;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "a",
    version,
    about = "Run, inspect, and switch between durable agent sessions",
    after_help = ROOT_EXAMPLES
)]
pub(crate) struct Cli {
    #[arg(
        long,
        global = true,
        help = "Emit machine-readable JSON where applicable"
    )]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Start a new session (workspace + tag + engine/profile) and its worker.
    #[command(after_help = START_EXAMPLES)]
    Start(StartArgs),
    /// Start a new session and immediately attach to it (`--attach` and
    /// `--fresh` implied; every other flag is `start`'s). When the requested
    /// workspace+tag is already live, the next free `<tag>-2` suffix is
    /// started instead -- `new` always creates, `here`/`a -` create-or-attach.
    #[command(
        about = "Start a new session and immediately attach to it",
        long_about = "Start a new session and immediately attach to it (`--attach` and\n`--fresh` implied; every other flag is `start`'s). `new` always creates:\nwhen the requested workspace+tag is already live it claims the next free\n`<tag>-2` suffix instead, where `here`/`a -` would reattach.",
        after_help = NEW_EXAMPLES
    )]
    New(StartArgs),
    /// Create-or-attach in the current workspace -- the typed-out form of
    /// `a -`: `a here [engine [tag]]`, or `a here <command...>` to run a
    /// literal command. Takes the same words `a -` takes, not flags; use
    /// `a new` for start's full flag surface.
    #[command(
        about = "Create-or-attach in the current workspace (typed-out form of `a -`)",
        long_about = "Create-or-attach in the current workspace -- the typed-out form of\n`a -`: `a here [engine [tag]]`, or `a here <command...>` to run a literal\ncommand. Takes the same words `a -` takes, not flags; use `a new` for\nstart's full flag surface.",
        after_help = HERE_EXAMPLES
    )]
    Here(QuickLaunchArgs),
    /// List sessions grouped by workspace.
    #[command(visible_aliases = ["ls", "ps"], after_help = LIST_EXAMPLES)]
    List(ListArgs),
    /// Same as `list`, but always machine-readable (see also `list --json`).
    #[command(after_help = SNAPSHOT_EXAMPLES)]
    Snapshot(ListArgs),
    /// Attach to a session's live PTY (Ctrl-b d to detach).
    #[command(visible_alias = "open", after_help = ATTACH_EXAMPLES)]
    Attach(AttachArgs),
    /// Send input to a session without attaching.
    #[command(after_help = SEND_EXAMPLES)]
    Send(SendArgs),
    /// Print a session's captured output/scrollback.
    #[command(after_help = CAPTURE_EXAMPLES)]
    Capture(CaptureArgs),
    /// Show a session's phase, exit info, and liveness.
    #[command(visible_alias = "show", after_help = STATUS_EXAMPLES)]
    Status(TargetArgs),
    /// Signal a session's workload and clean up its records.
    #[command(after_help = KILL_EXAMPLES)]
    Kill(KillArgs),
    /// Forget a dead session's records without claiming its workloads stopped.
    #[command(after_help = FORGET_EXAMPLES)]
    Forget(ForgetArgs),
    /// Remove dead, unreclaimable session records and their durable history.
    #[command(after_help = PRUNE_EXAMPLES)]
    Prune,
    /// Change a session's tag.
    #[command(after_help = RENAME_EXAMPLES)]
    Rename(RenameArgs),
    /// List configured/discovered engines.
    #[command(after_help = ENGINES_EXAMPLES)]
    Engines,
    /// List configured profiles.
    #[command(after_help = PROFILES_EXAMPLES)]
    Profiles,
    /// Resolve engine/profile/env for a launch and print it (no session
    /// created) -- internal integration point for pocketshell's launcher
    /// shim, not meant for interactive use, so hidden from `a --help`.
    #[command(hide = true)]
    LaunchSpec(LaunchArgs),
    /// Resolve engine/profile/env for a launch and exec it (no session
    /// created) -- internal integration point for pocketshell's launcher
    /// shim, not meant for interactive use, so hidden from `a --help`.
    #[command(hide = true)]
    LaunchExec(LaunchArgs),
    /// Check aplexer's environment/config for problems.
    #[command(visible_alias = "check", after_help = DOCTOR_EXAMPLES)]
    Doctor,
    /// Install agent-state hooks so sessions report working/waiting/idle
    /// instead of guessing from PTY output (see `a init --help`).
    #[command(
        about = "Install agent-state hooks so sessions report working/waiting/idle",
        after_help = INIT_EXAMPLES
    )]
    Init(InitArgs),
    /// Print the current session's identity (workspace/tag/engine/profile).
    #[command(visible_alias = "current", after_help = WHOAMI_EXAMPLES)]
    Whoami,
    /// Push the current session's semantic agent state, for a hook script
    /// to call from inside it (see `a state-report --help`).
    #[command(
        visible_alias = "state",
        about = "Push the current session's agent state (for hooks, see `a init`)",
        after_help = STATE_REPORT_EXAMPLES
    )]
    StateReport(StateReportArgs),
    /// Send or read messages between sibling agent sessions.
    #[command(after_help = MESSAGE_EXAMPLES)]
    Message(MessageArgs),
    /// Stream session lifecycle events.
    #[command(after_help = WATCH_EXAMPLES)]
    Watch(WatchArgs),
    /// Read/follow a session's conversation transcript.
    #[command(after_help = TRANSCRIPT_EXAMPLES)]
    Transcript(TranscriptArgs),
    /// Print a shell completion script for `a` to stdout.
    #[command(after_help = COMPLETIONS_EXAMPLES)]
    Completions(CompletionsArgs),
    /// Print the attach-mode keyboard shortcuts (Ctrl-b prefix, detach).
    #[command(visible_alias = "keys", after_help = HOTKEYS_EXAMPLES)]
    Hotkeys,
    /// `a <workspace-index> [session-index-or-tag]`, rewritten into this by
    /// main() before argument parsing -- not a name a user types directly.
    #[command(hide = true)]
    QuickAttach(QuickAttachArgs),
    /// `a - [engine [tag]] [command...]`, rewritten into this by main()
    /// before argument parsing -- not a name a user types directly.
    #[command(hide = true)]
    QuickLaunch(QuickLaunchArgs),
}

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
    /// Session to retag: UUID/prefix, workspace:tag, or tag in the current workspace
    #[arg(value_name = "SESSION")]
    pub(crate) selector: String,
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

#[derive(Args)]
pub(crate) struct MessageArgs {
    #[command(subcommand)]
    pub(crate) command: MessageCommand,
}

#[derive(Subcommand)]
pub(crate) enum MessageCommand {
    /// Send a message to a tag, a broadcast, or an engine filter.
    #[command(after_help = MESSAGE_SEND_EXAMPLES)]
    Send(MessageSendArgs),
    /// Reply to a received message (threads via reply_to).
    #[command(after_help = MESSAGE_REPLY_EXAMPLES)]
    Reply(MessageReplyArgs),
    /// List unread messages addressed to the calling session.
    #[command(after_help = MESSAGE_INBOX_EXAMPLES)]
    Inbox(MessageInboxArgs),
    /// Show the whole workspace conversation, in id (time) order.
    #[command(after_help = MESSAGE_LOG_EXAMPLES)]
    Log(MessageLogArgs),
    /// Show one message by id.
    #[command(after_help = MESSAGE_SHOW_EXAMPLES)]
    Show(MessageShowArgs),
    /// Acknowledge messages so they stop appearing in `inbox`.
    #[command(after_help = MESSAGE_ACK_EXAMPLES)]
    Ack(MessageAckArgs),
    /// Prune expired/over-cap messages from a workspace mailbox.
    #[command(after_help = MESSAGE_GC_EXAMPLES)]
    Gc(MessageGcArgs),
}

/// Flags shared by `send` and `reply` for choosing/framing pane delivery
/// (design doc section 6.2).
#[derive(Args)]
pub(crate) struct PaneDeliveryArgs {
    #[arg(
        long,
        help = "Inject as terminal input into the target's PTY instead of the durable inbox"
    )]
    pub(crate) pane: bool,
    #[arg(
        long = "or-inbox",
        help = "If --pane delivery fails, fall back to an inbox send instead of erroring"
    )]
    pub(crate) or_inbox: bool,
    #[arg(
        long,
        help = "With --pane: suppress the '[aplexer message from ...]' frame"
    )]
    pub(crate) raw: bool,
    #[arg(
        long = "no-enter",
        help = "With --pane: do not append a trailing return. Enter is sent by default (the tmuxctl behavior) so an injected message actually submits"
    )]
    pub(crate) no_enter: bool,
}

#[derive(Args)]
pub(crate) struct MessageSendArgs {
    #[arg(
        long,
        value_name = "TAG",
        help = "Send to one session, addressed by tag"
    )]
    pub(crate) to: Option<String>,
    #[arg(long, help = "Broadcast to every other session in the workspace")]
    pub(crate) all: bool,
    #[arg(
        long = "to-engine",
        value_name = "ENGINE",
        help = "Broadcast to sessions of one engine"
    )]
    pub(crate) to_engine: Option<String>,
    #[arg(
        long,
        help = "Allow sending to a tag that has never existed in this workspace"
    )]
    pub(crate) queue: bool,
    #[arg(
        long,
        default_value = "note",
        help = "note (default) | handoff | reply | any string"
    )]
    pub(crate) kind: String,
    #[arg(long, value_name = "JSON", help = "Opaque structured payload")]
    pub(crate) data: Option<String>,
    #[command(flatten)]
    pub(crate) pane_delivery: PaneDeliveryArgs,
    #[arg(
        long,
        value_name = "TAG",
        help = "Sender identity override (default: APLEXER_TAG or anonymous)"
    )]
    pub(crate) from: Option<String>,
    /// The message body
    #[arg(value_name = "TEXT")]
    pub(crate) text: String,
}

#[derive(Args)]
pub(crate) struct MessageReplyArgs {
    /// Id of the message being replied to (from `a message inbox`)
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_id: Uuid,
    #[command(flatten)]
    pub(crate) pane_delivery: PaneDeliveryArgs,
    /// Sender identity override (default: APLEXER_TAG or anonymous)
    #[arg(long, value_name = "TAG")]
    pub(crate) from: Option<String>,
    /// Opaque structured payload
    #[arg(long, value_name = "JSON")]
    pub(crate) data: Option<String>,
    #[arg(long, value_name = "KIND", help = "Defaults to \"reply\"")]
    pub(crate) kind: Option<String>,
    /// The reply body
    #[arg(value_name = "TEXT")]
    pub(crate) text: String,
}

#[derive(Args)]
pub(crate) struct MessageInboxArgs {
    #[arg(
        long,
        help = "Unread messages only (this is also the default with no flag)"
    )]
    pub(crate) new: bool,
    #[arg(
        long,
        value_name = "TAG",
        help = "Consumer identity override (default: APLEXER_SESSION_ID)"
    )]
    pub(crate) from: Option<String>,
}

#[derive(Args)]
pub(crate) struct MessageLogArgs {
    /// Workspace whose conversation to show (default: the current workspace)
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct MessageShowArgs {
    /// Id of the message to show
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_id: Uuid,
}

#[derive(Args)]
pub(crate) struct MessageAckArgs {
    /// Ids to acknowledge (from `a message inbox`)
    #[arg(value_name = "MESSAGE_ID")]
    pub(crate) message_ids: Vec<Uuid>,
    #[arg(
        long,
        help = "Ack every currently-unread message addressed to this consumer"
    )]
    pub(crate) all: bool,
    /// Consumer identity override (default: APLEXER_SESSION_ID)
    #[arg(long, value_name = "TAG")]
    pub(crate) from: Option<String>,
}

#[derive(Args)]
pub(crate) struct MessageGcArgs {
    /// Workspace whose mailbox to prune (default: the current workspace)
    #[arg(long, value_name = "PATH")]
    pub(crate) workspace: Option<PathBuf>,
}

pub(crate) fn main() {
    if let Err(error) = run() {
        eprintln!("a: {error:#}");
        std::process::exit(1);
    }
}
