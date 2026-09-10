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

// -- `Examples:` sections appended to each command's --help output. Kept
// as consts so the derive attributes below stay one line each; every
// block leads with copy-pasteable commands and explains in the comment.
pub(crate) const ROOT_EXAMPLES: &str = r#"Examples:
  a                                      sessions at a glance (bare `a` == `a list`)
  a -                                    create-or-attach session "main" right here
  a - codex review                       create-or-attach codex, tagged "review"
  a new                                  always-create a fresh session here, attached
  a new --engine codex --tag refactor    create with start's full options, attached
  a 2                                    attach to workspace #2 from `a list`
  a 2 review                             attach to "review" in workspace #2
  a open review                          attach by tag in the current workspace
  a send review "cargo test" --enter     type into a session without attaching
  a capture review --screen              peek at what that session shows right now
  a kill review                          hang up a session you are done with
  a keys                                 keys available while attached (Ctrl-b ...)

A session is addressed as a UUID/prefix, `workspace:tag`, or a bare tag in
the current workspace. Every command also takes --json for machine-readable
output, and every subcommand's --help ends with its own examples.
"#;

pub(crate) const START_EXAMPLES: &str = r#"Examples:
  a start                                  new session "main" here, in the background
  a start --attach                         same, but attach as soon as it is up
  a start --engine codex --tag review      pick engine and tag
  a start --workspace ~/git/api --tag dev  session for another directory
  a start --env OPENAI_API_KEY=sk-...      pass env through (repeatable)
  a start -- cargo watch -x check          run a literal command, not an engine
"#;

pub(crate) const NEW_EXAMPLES: &str = r#"Examples:
  a new                              fresh session here, attached (main-2 if taken)
  a new --engine codex --tag review  always creates: a taken tag becomes review-2
  a new --engine shell               a second plain shell alongside `a here`
"#;

pub(crate) const HERE_EXAMPLES: &str = r#"Examples:
  a here                        create-or-attach session "main" in this directory
  a here codex review           create-or-attach codex, tagged "review"
  a here coz                    a configured shortcut: engine+profile in one word
  a here htop                   not a known engine, so runs htop literally
"#;

pub(crate) const LIST_EXAMPLES: &str = r#"Examples:
  a list                        live sessions, grouped by workspace (bare `a` too)
  a list --all                  include exited sessions too
  a list --running              only live sessions
  a list --sort activity        busiest workspace first; the choice is remembered
  a list --json                 machine-readable
"#;

pub(crate) const SNAPSHOT_EXAMPLES: &str = r#"Examples:
  a snapshot                    `a list`'s data, always machine-readable
  a snapshot --running          live sessions only
"#;

pub(crate) const ATTACH_EXAMPLES: &str = r#"Examples:
  a attach review                            tag in the current workspace
  a attach myrepo:review                     workspace:tag, works from anywhere
  a attach 7f3c                              UUID prefix
  a attach --workspace ~/git/api --tag main  explicit flags instead
"#;

pub(crate) const SEND_EXAMPLES: &str = r#"Examples:
  a send review "cargo test" --enter  type a command and press Enter for them
  a send myrepo:review "hi"           any session selector works
  a send review --stdin < patch.diff  stream a file's bytes into the session
  a send review --hex 1b5b41          send raw bytes, hex-encoded on the way in
"#;

pub(crate) const CAPTURE_EXAMPLES: &str = r#"Examples:
  a capture review                   recent output, as received bytes
  a capture review --screen --plain  the screen right now, as plain text
  a capture review --bytes 8192      read more (or fewer) history bytes
  a capture review -o tail.log       write to a file instead of stdout
"#;

pub(crate) const STATUS_EXAMPLES: &str = r#"Examples:
  a status review               is it running, exited, since when, and why
  a status myrepo:review        any session selector works
  a status review --json        machine-readable
"#;

pub(crate) const KILL_EXAMPLES: &str = r#"Examples:
  a kill review                       hang it up (SIGHUP, 2s grace, then SIGKILL)
  a kill myrepo:review --signal TERM  graceful TERM-first shutdown
  a kill review --grace-ms 10000      more time to exit cleanly
"#;

pub(crate) const FORGET_EXAMPLES: &str = r#"Examples:
  a forget 7f3c --force         drop dead-session records; processes may live on
"#;

pub(crate) const PRUNE_EXAMPLES: &str = r#"Examples:
  a prune                       remove dead, unreclaimable sessions + their history
  a prune --json                machine-readable report of what went away
"#;

pub(crate) const RENAME_EXAMPLES: &str = r#"Examples:
  a rename main --tag docs         retag "main" as "docs" in the current workspace
  a rename myrepo:main --tag docs  any session selector works
"#;

pub(crate) const ENGINES_EXAMPLES: &str = r#"Examples:
  a engines                     ids `a start --engine` accepts
  a engines --json
"#;

pub(crate) const PROFILES_EXAMPLES: &str = r#"Examples:
  a profiles                    engine variants, e.g. another account via CODEX_HOME
  a profiles --json
"#;

pub(crate) const DOCTOR_EXAMPLES: &str = r#"Examples:
  a doctor                      check config, engines, cgroups, state dir
  a doctor --json
"#;

pub(crate) const INIT_EXAMPLES: &str = r#"Examples:
  a init                        install agent-state hooks for every engine found
  a init --check                installed already? exit code 0 says yes
  a init --engine codex         just codex
  a init --uninstall            remove aplexer's hooks again
"#;

pub(crate) const WHOAMI_EXAMPLES: &str = r#"Examples:
  a whoami                      identity of the session containing this shell
  a whoami --json
"#;

pub(crate) const STATE_REPORT_EXAMPLES: &str = r#"Examples:
  a state-report waiting        pushed by hooks from `a init`; not typed by hand
"#;

pub(crate) const MESSAGE_EXAMPLES: &str = r#"Examples:
  a message send --to review "done, see api.md"  note to one sibling, by tag
  a message send --all "standup in 5"            every sibling in the workspace
  a message inbox                                what is unread for this session
  a message log                                  the whole workspace conversation
"#;

pub(crate) const MESSAGE_SEND_EXAMPLES: &str = r#"Examples:
  a message send --to review "done, see api.md"     one session, by tag
  a message send --all "standup in 5"               every sibling in this workspace
  a message send --to-engine codex "status?"        every codex sibling
  a message send --to review --pane "git push"      inject as terminal input instead
  a message send --to watcher --queue "anyone up?"  target need not exist yet
  a message send --to review "ctx" --kind handoff --data '{"branch":"feat"}'
"#;

pub(crate) const MESSAGE_REPLY_EXAMPLES: &str = r#"Examples:
  a message reply <id> "on it"            the id comes from `a message inbox`
  a message reply <id> --pane "approved"  deliver into the sender's terminal
"#;

pub(crate) const MESSAGE_INBOX_EXAMPLES: &str = r#"Examples:
  a message inbox               unread messages addressed to this session
  a message inbox --from main   read as a different consumer tag
"#;

pub(crate) const MESSAGE_LOG_EXAMPLES: &str = r#"Examples:
  a message log                 the workspace conversation, oldest first
  a message log --workspace ~/git/api
"#;

pub(crate) const MESSAGE_SHOW_EXAMPLES: &str = r#"Examples:
  a message show <message-id>   one message envelope, full detail
"#;

pub(crate) const MESSAGE_ACK_EXAMPLES: &str = r#"Examples:
  a message ack <id>            mark one message read
  a message ack --all           clear the whole inbox
"#;

pub(crate) const MESSAGE_GC_EXAMPLES: &str = r#"Examples:
  a message gc                  drop expired/over-cap messages from the mailbox
"#;

pub(crate) const WATCH_EXAMPLES: &str = r#"Examples:
  a watch --jsonl                        lifecycle events for every agent session
  a watch --jsonl --workspace ~/git/api  scope to one workspace
  a watch --jsonl --all                  include plain shell sessions too
"#;

pub(crate) const TRANSCRIPT_EXAMPLES: &str = r#"Examples:
  a transcript review --last 50              last 50 conversation events
  a transcript review --follow               print new events as the agent writes them
  a transcript review --kind tool_call       only one event kind
  a transcript review --before 12 --last 20  page backward through history
"#;

pub(crate) const COMPLETIONS_EXAMPLES: &str = r#"Examples:
  source <(a completions bash)          try it in the current shell
  a completions zsh > "${fpath[1]}/_a"  then restart the shell to compinit
  a completions fish > ~/.config/fish/completions/a.fish
"#;

pub(crate) const HOTKEYS_EXAMPLES: &str = r#"Examples:
  a hotkeys                     the Ctrl-b key reference for attached mode
"#;

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
