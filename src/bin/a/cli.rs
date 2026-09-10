use super::cli_examples::*;
use super::cli_session_args::*;
use super::commands::run;
use clap::{Args, Parser, Subcommand};
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
