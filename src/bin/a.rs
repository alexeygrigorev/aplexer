use anyhow::{anyhow, bail, Context, Result};
use aplexer::messaging::*;
use aplexer::*;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::ffi::CString;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "a",
    version,
    about = "Run, inspect, and switch between durable agent sessions",
    after_help = ROOT_EXAMPLES
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Emit machine-readable JSON where applicable"
    )]
    json: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
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
const ROOT_EXAMPLES: &str = r#"Examples:
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

const START_EXAMPLES: &str = r#"Examples:
  a start                                  new session "main" here, in the background
  a start --attach                         same, but attach as soon as it is up
  a start --engine codex --tag review      pick engine and tag
  a start --workspace ~/git/api --tag dev  session for another directory
  a start --env OPENAI_API_KEY=sk-...      pass env through (repeatable)
  a start -- cargo watch -x check          run a literal command, not an engine
"#;

const NEW_EXAMPLES: &str = r#"Examples:
  a new                              fresh session here, attached (main-2 if taken)
  a new --engine codex --tag review  always creates: a taken tag becomes review-2
  a new --engine shell               a second plain shell alongside `a here`
"#;

const HERE_EXAMPLES: &str = r#"Examples:
  a here                        create-or-attach session "main" in this directory
  a here codex review           create-or-attach codex, tagged "review"
  a here coz                    a configured shortcut: engine+profile in one word
  a here htop                   not a known engine, so runs htop literally
"#;

const LIST_EXAMPLES: &str = r#"Examples:
  a list                        live sessions, grouped by workspace (bare `a` too)
  a list --all                  include exited sessions too
  a list --running              only live sessions
  a list --sort activity        busiest workspace first; the choice is remembered
  a list --json                 machine-readable
"#;

const SNAPSHOT_EXAMPLES: &str = r#"Examples:
  a snapshot                    `a list`'s data, always machine-readable
  a snapshot --running          live sessions only
"#;

const ATTACH_EXAMPLES: &str = r#"Examples:
  a attach review                            tag in the current workspace
  a attach myrepo:review                     workspace:tag, works from anywhere
  a attach 7f3c                              UUID prefix
  a attach --workspace ~/git/api --tag main  explicit flags instead
"#;

const SEND_EXAMPLES: &str = r#"Examples:
  a send review "cargo test" --enter  type a command and press Enter for them
  a send myrepo:review "hi"           any session selector works
  a send review --stdin < patch.diff  stream a file's bytes into the session
  a send review --hex 1b5b41          send raw bytes, hex-encoded on the way in
"#;

const CAPTURE_EXAMPLES: &str = r#"Examples:
  a capture review                   recent output, as received bytes
  a capture review --screen --plain  the screen right now, as plain text
  a capture review --bytes 8192      read more (or fewer) history bytes
  a capture review -o tail.log       write to a file instead of stdout
"#;

const STATUS_EXAMPLES: &str = r#"Examples:
  a status review               is it running, exited, since when, and why
  a status myrepo:review        any session selector works
  a status review --json        machine-readable
"#;

const KILL_EXAMPLES: &str = r#"Examples:
  a kill review                       hang it up (SIGHUP, 2s grace, then SIGKILL)
  a kill myrepo:review --signal TERM  graceful TERM-first shutdown
  a kill review --grace-ms 10000      more time to exit cleanly
"#;

const FORGET_EXAMPLES: &str = r#"Examples:
  a forget 7f3c --force         drop dead-session records; processes may live on
"#;

const PRUNE_EXAMPLES: &str = r#"Examples:
  a prune                       remove dead, unreclaimable sessions + their history
  a prune --json                machine-readable report of what went away
"#;

const RENAME_EXAMPLES: &str = r#"Examples:
  a rename main --tag docs         retag "main" as "docs" in the current workspace
  a rename myrepo:main --tag docs  any session selector works
"#;

const ENGINES_EXAMPLES: &str = r#"Examples:
  a engines                     ids `a start --engine` accepts
  a engines --json
"#;

const PROFILES_EXAMPLES: &str = r#"Examples:
  a profiles                    engine variants, e.g. another account via CODEX_HOME
  a profiles --json
"#;

const DOCTOR_EXAMPLES: &str = r#"Examples:
  a doctor                      check config, engines, cgroups, state dir
  a doctor --json
"#;

const INIT_EXAMPLES: &str = r#"Examples:
  a init                        install agent-state hooks for every engine found
  a init --check                installed already? exit code 0 says yes
  a init --engine codex         just codex
  a init --uninstall            remove aplexer's hooks again
"#;

const WHOAMI_EXAMPLES: &str = r#"Examples:
  a whoami                      identity of the session containing this shell
  a whoami --json
"#;

const STATE_REPORT_EXAMPLES: &str = r#"Examples:
  a state-report waiting        pushed by hooks from `a init`; not typed by hand
"#;

const MESSAGE_EXAMPLES: &str = r#"Examples:
  a message send --to review "done, see api.md"  note to one sibling, by tag
  a message send --all "standup in 5"            every sibling in the workspace
  a message inbox                                what is unread for this session
  a message log                                  the whole workspace conversation
"#;

const MESSAGE_SEND_EXAMPLES: &str = r#"Examples:
  a message send --to review "done, see api.md"     one session, by tag
  a message send --all "standup in 5"               every sibling in this workspace
  a message send --to-engine codex "status?"        every codex sibling
  a message send --to review --pane "git push"      inject as terminal input instead
  a message send --to watcher --queue "anyone up?"  target need not exist yet
  a message send --to review "ctx" --kind handoff --data '{"branch":"feat"}'
"#;

const MESSAGE_REPLY_EXAMPLES: &str = r#"Examples:
  a message reply <id> "on it"            the id comes from `a message inbox`
  a message reply <id> --pane "approved"  deliver into the sender's terminal
"#;

const MESSAGE_INBOX_EXAMPLES: &str = r#"Examples:
  a message inbox               unread messages addressed to this session
  a message inbox --from main   read as a different consumer tag
"#;

const MESSAGE_LOG_EXAMPLES: &str = r#"Examples:
  a message log                 the workspace conversation, oldest first
  a message log --workspace ~/git/api
"#;

const MESSAGE_SHOW_EXAMPLES: &str = r#"Examples:
  a message show <message-id>   one message envelope, full detail
"#;

const MESSAGE_ACK_EXAMPLES: &str = r#"Examples:
  a message ack <id>            mark one message read
  a message ack --all           clear the whole inbox
"#;

const MESSAGE_GC_EXAMPLES: &str = r#"Examples:
  a message gc                  drop expired/over-cap messages from the mailbox
"#;

const WATCH_EXAMPLES: &str = r#"Examples:
  a watch --jsonl                        lifecycle events for every agent session
  a watch --jsonl --workspace ~/git/api  scope to one workspace
  a watch --jsonl --all                  include plain shell sessions too
"#;

const TRANSCRIPT_EXAMPLES: &str = r#"Examples:
  a transcript review --last 50              last 50 conversation events
  a transcript review --follow               print new events as the agent writes them
  a transcript review --kind tool_call       only one event kind
  a transcript review --before 12 --last 20  page backward through history
"#;

const COMPLETIONS_EXAMPLES: &str = r#"Examples:
  source <(a completions bash)          try it in the current shell
  a completions zsh > "${fpath[1]}/_a"  then restart the shell to compinit
  a completions fish > ~/.config/fish/completions/a.fish
"#;

const HOTKEYS_EXAMPLES: &str = r#"Examples:
  a hotkeys                     the Ctrl-b key reference for attached mode
"#;

#[derive(Args)]
struct QuickAttachArgs {
    /// 1-based index into the workspaces as shown by `a list`.
    workspace_index: usize,
    /// 1-based index into that workspace's sessions (list order), or a
    /// literal tag. Defaults to that workspace's first session.
    session: Option<String>,
}

#[derive(Args)]
struct QuickLaunchArgs {
    /// Usually `[engine [tag]]`; words naming no engine or shortcut run
    /// as a literal command
    rest: Vec<String>,
}

#[derive(Args)]
struct CompletionsArgs {
    /// Shell to generate a completion script for.
    shell: Shell,
}

#[derive(Args)]
struct StartArgs {
    /// Workspace directory the session belongs to (default: the current directory)
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    // The terminal-first default tag is `main`, matching `a here` and `a -`.
    // Resolution of a bare target still falls back to `default` (see
    // `resolve`), so sessions created by older builds stay attachable.
    /// Session name within the workspace
    #[arg(long, default_value = DEFAULT_HUMAN_TAG)]
    tag: String,
    /// Engine id from `a engines`; defaults to the configured default engine, else shell
    #[arg(long)]
    engine: Option<String>,
    /// Profile id from `a profiles` -- an engine variant (e.g. another account)
    #[arg(long)]
    profile: Option<String>,
    /// Working directory for the launched process (default: the workspace)
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Extra environment variable for the process, KEY=VALUE (repeatable)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// cgroup memory cap for the session, e.g. 512M or 2G
    #[arg(long)]
    memory: Option<String>,
    /// cgroup cap on the number of processes
    #[arg(long)]
    pids: Option<u64>,
    /// cgroup CPU quota per period, in microseconds (with --cpu-period-us)
    #[arg(long)]
    cpu_quota_us: Option<u64>,
    /// Length of the cgroup CPU period, in microseconds
    #[arg(long, default_value_t = 100_000)]
    cpu_period_us: u64,
    /// Output-history bytes kept for `a capture` and attach replay
    #[arg(long)]
    history_bytes: Option<usize>,
    /// Attach to the session as soon as it is up
    #[arg(long)]
    attach: bool,
    /// Give up if the workload has not started after this long
    #[arg(long, default_value_t = DEFAULT_STARTUP_TIMEOUT_MS)]
    startup_timeout_ms: u64,
    /// Keep the engine's confirmation/sandbox prompts. Default is to append
    /// the engine's skip-permissions argv (`--dangerously-bypass-approvals-
    /// and-sandbox` / `--dangerously-skip-permissions` / `--always-approve`).
    #[arg(long)]
    no_skip_permissions: bool,
    /// When the requested workspace+tag is already held by a live session,
    /// claim the next free `<tag>-2`, `<tag>-3`, … suffix instead of
    /// failing. `a new` implies this; `start` without it keeps the strict
    /// create-by-exact-tag contract.
    #[arg(long)]
    fresh: bool,
    /// With `--`, run this command instead of an engine
    #[arg(last = true, value_name = "COMMAND")]
    command: Vec<OsString>,
}

#[derive(Args)]
struct LaunchArgs {
    #[arg(long)]
    engine: Option<String>,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Suppress the engine's `skip_permissions_argv` (see `EngineConfig`) --
    /// the phone/desktop send this to opt OUT; skip-permissions argv is
    /// appended by default (matches pocketshell's own
    /// `--skip-permissions/--no-skip-permissions` default=True).
    #[arg(long)]
    no_skip_permissions: bool,
}

#[derive(Args, Clone)]
struct TargetArgs {
    #[arg(
        value_name = "SESSION",
        help = "UUID/prefix, workspace:tag selector, or tag in the current workspace"
    )]
    selector: Option<String>,
    /// Workspace directory to resolve the tag in
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
    /// Tag to resolve in --workspace (or the current workspace)
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
}

#[derive(Args, Default)]
struct ListArgs {
    /// Only live sessions
    #[arg(long)]
    running: bool,
    /// Include exited sessions too (the list hides them by default)
    #[arg(long)]
    all: bool,
    /// Order workspaces in `a list` (and the `a N` numbers). Remembered
    /// until you pick another. Time sorts are newest-first.
    #[arg(long, value_enum, value_name = "KEY")]
    sort: Option<ListSort>,
}

/// How `a list` orders workspace groups. Session order *inside* a group
/// stays newest-created-first (`list_records`), matching `Ctrl-b 1-9`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ListSort {
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
    fn as_str(self) -> &'static str {
        match self {
            ListSort::Name => "name",
            ListSort::Created => "created",
            ListSort::Accessed => "accessed",
            ListSort::Activity => "activity",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
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
struct AttachArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Replay this many history bytes instead of the live screen
    #[arg(long)]
    history_bytes: Option<usize>,
    /// Attach without a client status bar or client key bindings
    #[arg(long)]
    no_status: bool,
}
#[derive(Args)]
struct SendArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Bytes to type into the session
    #[arg(value_name = "TEXT")]
    text: Option<String>,
    /// Read the bytes to send from stdin instead of TEXT
    #[arg(long)]
    stdin: bool,
    /// Interpret TEXT as hex and send the decoded bytes
    #[arg(long)]
    hex: bool,
    /// Append a newline, like pressing Enter
    #[arg(long)]
    enter: bool,
}
#[derive(Args)]
struct CaptureArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// How many history bytes to read (default: a recent tail)
    #[arg(long)]
    bytes: Option<usize>,
    /// Write the bytes to this file instead of stdout
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Capture the rendered current screen (docs/terminal-state-design.md
    /// section 8) instead of raw history bytes -- a "richer PocketShell
    /// preview" of what the session's screen actually looks like right now,
    /// for a few hundred to a few thousand bytes, rather than an arbitrary
    /// tail of the byte stream. Ignores --bytes.
    #[arg(long)]
    screen: bool,
    /// With --screen, emit plain text (`ScreenTracker::contents()`) instead
    /// of the paintable escape-sequence form.
    #[arg(long, requires = "screen")]
    plain: bool,
}
#[derive(Args)]
struct KillArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Signal to send first before escalating to SIGKILL after --grace-ms.
    /// Defaults to HUP (session hangup): interactive shells started by
    /// `a start` (e.g. `bash -l`) legitimately ignore SIGTERM when they own
    /// the PTY as an interactive login shell, so a TERM-first default eats
    /// the whole grace and always escalates (benchmark PLAN P0.1). HUP is
    /// the conventional "terminal went away" signal and terminates both
    /// shells and typical agents/servers via the default disposition; pass
    /// --signal TERM explicitly for a graceful TERM-first shutdown.
    #[arg(long, default_value = "HUP")]
    signal: String,
    /// How long to wait after --signal before escalating to SIGKILL
    #[arg(long, default_value_t = 2_000)]
    grace_ms: u64,
}
#[derive(Args)]
struct ForgetArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Acknowledge that uncontained workload processes may survive.
    #[arg(long, required = true)]
    force: bool,
}
#[derive(Args)]
struct WatchArgs {
    /// Currently the only supported output mode -- required explicitly
    /// rather than defaulted so a bare `a watch` fails loudly instead of
    /// silently assuming a format that isn't implemented.
    #[arg(long)]
    jsonl: bool,
    /// Also watch shell (non-agent) sessions. Default is agent-only (engine
    /// != "shell"), an explicit scope decision -- see src/watch.rs.
    #[arg(long)]
    all: bool,
    /// Only events for sessions in this workspace
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

/// `a transcript` -- parse a session's native engine conversation log into
/// heru UnifiedEvent JSONL for PocketShell (and `a transcript --follow`).
/// See src/agent_events.rs.
#[derive(Args)]
struct TranscriptArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Only the last N events after `--kind` / `--after` / `--before`
    /// filtering. PocketShell's initial conversation pane is `--last 50`
    /// (or `--last 5` for a compact peek).
    #[arg(long, value_name = "N")]
    last: Option<usize>,
    /// Filter to one UnifiedEvent kind (message, tool_call, tool_result, error, usage)
    /// before applying --last/--after/--before.
    #[arg(long)]
    kind: Option<String>,
    /// Events with sequence > N. Catch-up / follow-resume cursor: PocketShell
    /// stores the last sequence it rendered and asks for everything after.
    #[arg(long, value_name = "SEQ")]
    after: Option<u64>,
    /// Events with sequence < N. Older page: combine with `--last` to walk
    /// backward (`--before 12 --last 20`).
    #[arg(long, value_name = "SEQ")]
    before: Option<u64>,
    /// After emitting the current page, keep watching the native log and
    /// print new events as the agent writes them (`tail -f` of parsed
    /// UnifiedEvent JSONL). Implies a long-lived stdout stream; Ctrl-C
    /// to stop. Combine with `--after` / `--last` for the initial page.
    #[arg(long)]
    follow: bool,
    /// Replace any native JSONL line longer than N bytes with a truncation
    /// marker before parsing, so one huge tool_result cannot balloon the
    /// read (PocketShell `agent-log --max-line-bytes`).
    #[arg(long, value_name = "N")]
    max_line_bytes: Option<usize>,
}

#[derive(Args)]
struct RenameArgs {
    /// Session to retag: UUID/prefix, workspace:tag, or tag in the current workspace
    #[arg(value_name = "SESSION")]
    selector: String,
    /// Workspace directory to resolve the selector in
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
    /// New tag for the session
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
}

#[derive(Args)]
struct StateReportArgs {
    /// idle: the agent finished its turn and is resting, nothing
    /// outstanding. waiting: blocked on a prompt/question and needs the
    /// user. working: actively producing/thinking. Matches PocketShell's
    /// SessionAgentState vocabulary (Idle/WaitingForInput/Working) one for
    /// one -- see SessionAgentState.kt in the pocketshell repo.
    state: ReportedState,
}

#[derive(Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
enum ReportedState {
    Idle,
    Waiting,
    Working,
}

impl ReportedState {
    fn as_str(self) -> &'static str {
        match self {
            ReportedState::Idle => "idle",
            ReportedState::Waiting => "waiting",
            ReportedState::Working => "working",
        }
    }
}

#[derive(Args)]
struct InitArgs {
    /// Only check whether hooks are installed; print the per-engine status
    /// and exit 0 when fully initialized, 1 otherwise. No files are
    /// touched. With `--json` this is the machine contract for automation:
    /// `a init --check --json` reports `{"initialized": bool, ...}`.
    #[arg(long)]
    check: bool,
    /// Remove aplexer's `state-report` hooks instead of installing them.
    /// Only hook entries containing `state-report` and our generated files
    /// are removed; everything else is left alone.
    #[arg(long)]
    uninstall: bool,
    /// Only act on one engine: claude, codex, zcodex (shares codex's
    /// CODEX_HOME config), grok, gemini, or opencode. Default: all engines.
    #[arg(long, value_name = "ENGINE")]
    engine: Option<String>,
}

// -- Inter-agent messaging (docs/inter-agent-messaging-design.md, section 7) --

#[derive(Args)]
struct MessageArgs {
    #[command(subcommand)]
    command: MessageCommand,
}

#[derive(Subcommand)]
enum MessageCommand {
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
struct PaneDeliveryArgs {
    #[arg(
        long,
        help = "Inject as terminal input into the target's PTY instead of the durable inbox"
    )]
    pane: bool,
    #[arg(
        long = "or-inbox",
        help = "If --pane delivery fails, fall back to an inbox send instead of erroring"
    )]
    or_inbox: bool,
    #[arg(
        long,
        help = "With --pane: suppress the '[aplexer message from ...]' frame"
    )]
    raw: bool,
    #[arg(
        long = "no-enter",
        help = "With --pane: do not append a trailing return. Enter is sent by default (the tmuxctl behavior) so an injected message actually submits"
    )]
    no_enter: bool,
}

#[derive(Args)]
struct MessageSendArgs {
    #[arg(
        long,
        value_name = "TAG",
        help = "Send to one session, addressed by tag"
    )]
    to: Option<String>,
    #[arg(long, help = "Broadcast to every other session in the workspace")]
    all: bool,
    #[arg(
        long = "to-engine",
        value_name = "ENGINE",
        help = "Broadcast to sessions of one engine"
    )]
    to_engine: Option<String>,
    #[arg(
        long,
        help = "Allow sending to a tag that has never existed in this workspace"
    )]
    queue: bool,
    #[arg(
        long,
        default_value = "note",
        help = "note (default) | handoff | reply | any string"
    )]
    kind: String,
    #[arg(long, value_name = "JSON", help = "Opaque structured payload")]
    data: Option<String>,
    #[command(flatten)]
    pane_delivery: PaneDeliveryArgs,
    #[arg(
        long,
        value_name = "TAG",
        help = "Sender identity override (default: APLEXER_TAG or anonymous)"
    )]
    from: Option<String>,
    /// The message body
    #[arg(value_name = "TEXT")]
    text: String,
}

#[derive(Args)]
struct MessageReplyArgs {
    /// Id of the message being replied to (from `a message inbox`)
    #[arg(value_name = "MESSAGE_ID")]
    message_id: Uuid,
    #[command(flatten)]
    pane_delivery: PaneDeliveryArgs,
    /// Sender identity override (default: APLEXER_TAG or anonymous)
    #[arg(long, value_name = "TAG")]
    from: Option<String>,
    /// Opaque structured payload
    #[arg(long, value_name = "JSON")]
    data: Option<String>,
    #[arg(long, value_name = "KIND", help = "Defaults to \"reply\"")]
    kind: Option<String>,
    /// The reply body
    #[arg(value_name = "TEXT")]
    text: String,
}

#[derive(Args)]
struct MessageInboxArgs {
    #[arg(
        long,
        help = "Unread messages only (this is also the default with no flag)"
    )]
    new: bool,
    #[arg(
        long,
        value_name = "TAG",
        help = "Consumer identity override (default: APLEXER_SESSION_ID)"
    )]
    from: Option<String>,
}

#[derive(Args)]
struct MessageLogArgs {
    /// Workspace whose conversation to show (default: the current workspace)
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

#[derive(Args)]
struct MessageShowArgs {
    /// Id of the message to show
    #[arg(value_name = "MESSAGE_ID")]
    message_id: Uuid,
}

#[derive(Args)]
struct MessageAckArgs {
    /// Ids to acknowledge (from `a message inbox`)
    #[arg(value_name = "MESSAGE_ID")]
    message_ids: Vec<Uuid>,
    #[arg(
        long,
        help = "Ack every currently-unread message addressed to this consumer"
    )]
    all: bool,
    /// Consumer identity override (default: APLEXER_SESSION_ID)
    #[arg(long, value_name = "TAG")]
    from: Option<String>,
}

#[derive(Args)]
struct MessageGcArgs {
    /// Workspace whose mailbox to prune (default: the current workspace)
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("a: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // `a` is a standalone process, so it can safely repair an inherited
    // auto-reaping SIGCHLD disposition before any subcommand spawns a child.
    // The embeddable Rust/Python API only validates and preserves its host.
    normalize_sigchld_for_child_management()?;
    let args = rewrite_quick_attach_args(std::env::args().collect());
    let cli = Cli::parse_from(args);
    let paths = Paths::discover()?;
    // Bare `a` with no subcommand defaults to `a list`, matching how tmux
    // and similar tools default to a listing rather than printing usage.
    let command = cli.command.unwrap_or(Commands::List(ListArgs::default()));
    match command {
        Commands::Start(args) => cmd_start(&paths, args, cli.json),
        Commands::New(mut args) => {
            args.attach = true;
            // `new` is the "always creates" verb: a live session holding the
            // tag is a reason to take the next free suffix, never an error.
            // `here`/`a -` stay create-or-attach.
            args.fresh = true;
            cmd_start(&paths, args, cli.json)
        }
        Commands::Here(args) => {
            if cli.json {
                bail!(
                    "`a here` is an interactive create-or-attach command; use \
                     `a --json start --workspace ... --tag ...` for automation"
                );
            }
            cmd_quick_launch(&paths, args)
        }
        Commands::List(args) => cmd_list(&paths, args, cli.json),
        Commands::Snapshot(args) => cmd_list(&paths, args, true),
        Commands::Attach(args) => {
            let record = resolve(&paths, &args.target)?;
            attach(&paths, &record, args.history_bytes, args.no_status)
        }
        Commands::Send(args) => cmd_send(&paths, args, cli.json),
        Commands::Capture(args) => cmd_capture(&paths, args, cli.json),
        Commands::Status(target) => cmd_status(&paths, target, cli.json),
        Commands::Kill(args) => cmd_kill(&paths, args, cli.json),
        Commands::Forget(args) => cmd_forget(&paths, args, cli.json),
        Commands::Prune => cmd_prune(&paths, cli.json),
        Commands::Rename(args) => cmd_rename(&paths, args, cli.json),
        Commands::Engines => cmd_engines(&paths, cli.json),
        Commands::Profiles => cmd_profiles(&paths, cli.json),
        Commands::LaunchSpec(args) => cmd_launch_spec(&paths, args, cli.json),
        Commands::LaunchExec(args) => cmd_launch_exec(&paths, args),
        Commands::Whoami => cmd_whoami(&paths, cli.json),
        Commands::StateReport(args) => cmd_state_report(&paths, args.state),
        Commands::Doctor => cmd_doctor(&paths, cli.json),
        Commands::Init(args) => cmd_init(&paths, args, cli.json),
        Commands::Message(args) => cmd_message(&paths, args, cli.json),
        Commands::Watch(args) => cmd_watch(&paths, args),
        Commands::Transcript(args) => cmd_transcript(&paths, args, cli.json),
        Commands::Completions(args) => cmd_completions(args),
        Commands::Hotkeys => cmd_hotkeys(),
        Commands::QuickAttach(args) => cmd_quick_attach(&paths, args),
        Commands::QuickLaunch(args) => cmd_quick_launch(&paths, args),
    }
}

/// `a <N> [session]` is rewritten to `a quick-attach <N> [session]` before
/// clap ever sees it, the same trick tmuxctl's `t` uses in its own
/// argv-rewriting main() (see ~/git/tmuxctl/tmuxctl/cli.py) to let a bare
/// positional number mean "attach" without a subcommand keyword. Only the
/// first argument is inspected, and only when it's non-empty and all
/// digits -- none of `a`'s real subcommand names collide with that.
fn rewrite_quick_attach_args(args: Vec<String>) -> Vec<String> {
    // (hidden subcommand name, how many leading args to drop before it --
    // the "-" marker itself carries no information once rewritten, but a
    // quick-attach index like "1" is itself the first real argument).
    let rewrite = match args.get(1).map(String::as_str) {
        // `a -` / `a - claude` / `a - claude review` / `a - <command...>`,
        // the same "-" marks-current-directory idiom tmuxctl's `t` uses for
        // create-or-attach, adapted to aplexer's engine/tag model in
        // cmd_quick_launch.
        Some("-") => Some(("quick-launch", 2)),
        // `a <N>` / `a <N> <M>` / `a <N> <tag>` -- see rewrite doc below.
        Some(first) if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) => {
            Some(("quick-attach", 1))
        }
        _ => None,
    };
    let Some((hidden_name, skip)) = rewrite else {
        return args;
    };
    let mut rewritten = Vec::with_capacity(args.len() + 1);
    rewritten.push(args[0].clone());
    rewritten.push(hidden_name.to_string());
    rewritten.extend(args.into_iter().skip(skip));
    rewritten
}

/// The tag the terminal-first vocabulary creates and resolves by default:
/// `a here`, `a -`, and `a start` all mean tag `main` in the current
/// workspace, so "work on the main thing here" is one word in every form.
const DEFAULT_HUMAN_TAG: &str = "main";
/// Sessions created before the terminal-first default stay attachable with
/// no flags: after `main`, a bare `a attach`/`a status` falls back to this
/// pre-UX tag before giving up.
const LEGACY_DEFAULT_TAG: &str = "default";

/// Whether a selector could plausibly be a UUID or UUID prefix: only hex
/// digits and dashes, with 8..=32 digits (a full UUID is 32, the shortest
/// useful prefix `resolve_record` honors is 8). Anything containing a
/// non-hex character is a word -- i.e. a candidate tag -- never a UUID.
fn looks_like_uuid_selector(raw: &str) -> bool {
    let mut hex_digits = 0usize;
    for byte in raw.bytes() {
        if byte == b'-' {
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
        hex_digits += 1;
    }
    (8..=32).contains(&hex_digits)
}

fn resolve(paths: &Paths, target: &TargetArgs) -> Result<SessionRecord> {
    // `a attach 1`, `a status 1`, `a kill 1`, etc. should mean the same
    // thing as the bare `a 1` shortcut, not just work for `attach`. Only
    // kick in for selectors shorter than 8 characters, the minimum length
    // resolve_record treats as a UUID prefix -- so this can never shadow a
    // real UUID/UUID-prefix selector, and workspace counts realistically
    // never reach 8 digits.
    if target.workspace.is_none() && target.tag.is_none() {
        if let Some(selector) = &target.selector {
            let is_quick_index = !selector.is_empty()
                && selector.len() < 8
                && selector.bytes().all(|b| b.is_ascii_digit());
            if is_quick_index {
                let index: usize = selector.parse().unwrap_or(0);
                return resolve_quick_index(paths, index, None);
            }
        }
    }
    // Terminal-first default target: no selector and no --tag means "the
    // main session here" -- the same session `a here` creates. Prefers
    // `main`, then falls back to the pre-UX `default` tag so existing
    // sessions remain one-command attachable after upgrading. An explicit
    // --workspace pins the lookup; with no flag the workspace is
    // $APLEXER_WORKSPACE (set inside every session) or the cwd, matching
    // `resolve_message_workspace` so all "here" resolution agrees.
    if target.selector.is_none() && target.tag.is_none() {
        let workspace = match &target.workspace {
            Some(ws) => Some(canonical_workspace(ws)?),
            None => resolve_message_workspace(None).ok(),
        };
        if let Some(workspace) = workspace {
            let records = list_records(paths)?;
            if let Some(record) = records
                .iter()
                .find(|r| r.workspace == workspace && r.tag == DEFAULT_HUMAN_TAG)
                .or_else(|| {
                    records
                        .iter()
                        .find(|r| r.workspace == workspace && r.tag == LEGACY_DEFAULT_TAG)
                })
            {
                return Ok(record.clone());
            }
            if target.workspace.is_none() {
                bail!(
                    "no {DEFAULT_HUMAN_TAG} session in {}; run `a here` to create one, or `a` to list every session",
                    display_workspace(&workspace, env::var_os("HOME").as_deref().map(Path::new))
                );
            }
            // Explicit --workspace with no main/default session: fall
            // through to resolve_record's legacy `default`-tag resolution
            // and its own error message.
        }
    }
    // `a open review`, `a show review`: a plain word that is not a candidate
    // UUID resolves as a tag in the current workspace, after UUID and
    // full `workspace:tag` selectors have had their chance -- so existing
    // machine-facing selector semantics can never be shadowed by a tag.
    if let Some(selector) = &target.selector {
        if target.workspace.is_none() && target.tag.is_none() && !looks_like_uuid_selector(selector)
        {
            if let Ok(record) = resolve_record(paths, Some(selector), None, None) {
                return Ok(record);
            }
            if let Ok(workspace) = resolve_message_workspace(None) {
                let matches: Vec<SessionRecord> = list_records(paths)?
                    .into_iter()
                    .filter(|r| r.workspace == workspace && r.tag == selector.as_str())
                    .collect();
                match matches.len() {
                    1 => return Ok(matches.into_iter().next().expect("one match")),
                    0 => bail!(
                        "no session tagged '{selector}' in {}; run `a` to list sessions, \
                         or use a full workspace:tag selector",
                        display_workspace(
                            &workspace,
                            env::var_os("HOME").as_deref().map(Path::new)
                        )
                    ),
                    // A pair can transiently be held by a corpse next to the
                    // live session that took its name (`a rename`, issue
                    // #13); the tag still means the live session until prune
                    // clears the corpse. Same rule as `resolve_record`.
                    _ => {
                        let live: Vec<SessionRecord> = matches
                            .iter()
                            .filter(|r| aplexer::reap_verdict(r).is_none())
                            .cloned()
                            .collect();
                        if live.len() == 1 {
                            return Ok(live.into_iter().next().expect("exactly one live match"));
                        }
                        bail!("tag '{selector}' is ambiguous in this workspace")
                    }
                }
            }
        }
    }
    resolve_record(
        paths,
        target.selector.as_deref(),
        target.workspace.as_deref(),
        target.tag.as_deref(),
    )
}

fn cmd_start(paths: &Paths, args: StartArgs, json_output: bool) -> Result<()> {
    if json_output && args.attach {
        bail!(
            "--json cannot be combined with `start --attach`: JSON session metadata and terminal bytes cannot share stdout; run `a --json start ...` and `a attach SESSION` separately"
        );
    }
    let env = parse_env(&args.env)?;
    let command = args
        .command
        .iter()
        .map(|v| os_to_utf8(v, "command argument"))
        .collect::<Result<Vec<_>>>()?;
    let mut worker_rows = None;
    let mut worker_cols = None;
    if args.attach {
        let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if let Some((rows, cols)) = tty.then(|| terminal_size(libc::STDIN_FILENO)).flatten() {
            worker_rows = Some(reserved_rows(rows));
            worker_cols = Some(cols);
        }
    }
    let req = aplexer::api::StartRequest {
        workspace: args.workspace,
        tag: args.tag,
        engine: args.engine,
        profile: args.profile,
        cwd: args.cwd,
        env,
        command,
        memory: args.memory,
        pids: args.pids,
        cpu_quota_us: args.cpu_quota_us,
        cpu_period_us: args.cpu_period_us,
        history_bytes: args.history_bytes,
        no_skip_permissions: args.no_skip_permissions,
        startup_timeout_ms: args.startup_timeout_ms,
        worker_rows,
        worker_cols,
        python: None,
        fresh: args.fresh,
    };
    let ready = aplexer::api::start_session(paths, &req)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&ready)?);
    } else {
        println!("{}", ready.id);
        println!("{}", ready.selector());
    }
    if args.attach {
        // `None` here means "use attach()'s small default replay", not
        // "replay the whole configured history buffer" -- ready.history_bytes
        // is the session's *storage capacity* (up to DEFAULT_HISTORY_BYTES =
        // 4MB), an unrelated setting from how much of it a fresh attach
        // should actually replay onto the screen.
        attach(paths, &ready, None, false)?;
    }
    Ok(())
}

fn cmd_list(paths: &Paths, args: ListArgs, json_output: bool) -> Result<()> {
    // `--sort` remembers even on the JSON path, so a later human `a list` /
    // `a N` uses the same workspace order. JSON row order itself stays
    // newest-created-first (spec.md §18).
    if args.sort.is_some() {
        resolve_list_sort(paths, args.sort)?;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&aplexer::api::snapshot_json(paths, args.running)?)?
        );
        return Ok(());
    }
    // Human output is terminal-first: on a real TTY the richer tree below
    // adds semantic state and current-workspace orientation; redirected
    // output keeps the pre-UX plain rendering byte-for-byte, so scripts
    // piping `a list` never see presentation changes.
    if io::stdout().is_terminal() {
        return cmd_list_tty(paths, args);
    }
    cmd_list_plain(paths, args)
}

/// Exited sessions are corpses: the one listed state that is neither active
/// (`ui_state_is_active`) nor needs-attention (`ui_state_needs_attention`),
/// and `check_attachable` refuses them outright. The terminal list hides
/// them by default -- a workspace whose every session has exited drops out
/// with them -- and `a list --all` brings them back. Before rendering, the
/// default list first sweeps what `a prune` would take outright (see
/// `sweep_prunable_corpses`), so the hide only ever covers the shapes prune
/// itself retains. `resolve_quick_index`
/// shares this so the numbers `a <workspace#>` understands stay the numbers
/// the default list prints; a corpse you found via `a list --all` is
/// addressed by tag or UUID prefix, not by its --all index.
fn session_is_listed(record: &SessionRecord, now: u64) -> bool {
    session_ui_state(record, now).0 != "exited"
}

/// The terminal rendering of `a list` -- see cmd_list's redirect contract.
fn cmd_list_tty(paths: &Paths, args: ListArgs) -> Result<()> {
    // The default view is self-cleaning: sweep first, so a corpse a killed
    // worker left behind is gone from the registry rather than merely
    // hidden. Same verdict and locked removal as `a prune`; best-effort,
    // because a list that cannot sweep (registry mid-write, a lost race)
    // must still list. Explicit views opt out: `--all` exists to show
    // post-mortems, and `--running` would throw the sweep's work away
    // unseen.
    if !args.running && !args.all {
        let _ = sweep_prunable_corpses(paths);
    }
    let mut records = list_records(paths)?;
    // Lineage labels: a session started from inside another session (`a
    // start` ran with its parent's APLEXER_SESSION_ID still in the
    // environment) shows where it came from -- the parent's tag while its
    // record exists, a short id once it doesn't, since the recorded lineage
    // deliberately survives a killed or forgotten parent. Computed over the
    // full registry, before the corpse filter below, so a live child of an
    // exited parent keeps the parent's tag in its `↳` label even though the
    // parent no longer has a row of its own.
    let lineage_labels: BTreeMap<Uuid, String> = {
        let by_id: BTreeMap<Uuid, &SessionRecord> =
            records.iter().map(|record| (record.id, record)).collect();
        records
            .iter()
            .filter_map(|record| {
                let parent = record.parent_session?;
                let label = match by_id.get(&parent) {
                    Some(parent_record) => parent_record.tag.clone(),
                    None => parent.to_string()[..8].to_string(),
                };
                Some((record.id, format!(" ↳ {label}")))
            })
            .collect()
    };
    let mut hidden_exited = 0usize;
    if args.running {
        records.retain(|record| record.worker_phase_active() && record.worker_alive());
    } else if !args.all {
        let before = records.len();
        let now = now_ms();
        records.retain(|record| session_is_listed(record, now));
        hidden_exited = before - records.len();
    }
    if records.is_empty() {
        if args.running {
            println!("No running sessions.");
        } else if hidden_exited > 0 {
            println!(
                "No live sessions -- {hidden_exited} exited hidden (`a list --all` shows them)."
            );
        } else {
            println!("No aplexer sessions yet.");
            println!();
            println!("Start and attach in this directory:");
            println!("  a here                 default engine, tag main");
            println!("  a here codex review    codex, tag review");
            println!("  a new --engine shell   full start options, attached");
            println!();
            println!("Discover: a engines · a profiles · a help");
        }
        return Ok(());
    }

    let sort = load_list_sort(paths);
    let groups = group_by_workspace(records, sort);
    let home = env::var_os("HOME").map(PathBuf::from);
    let current_workspace = resolve_message_workspace(None).ok();
    let color = color_enabled();
    let now = now_ms();

    // One query-time detection walk per record (`api::record_agent`, the same
    // source every `a list --json` row's `agent` field carries), shared by the
    // engine-column width computation and every row below -- the same
    // probe-once shape the liveness map in cmd_list_plain has (PLAN P1.1).
    //
    // The walks run fan-out across cores: one session's tree walk is
    // sub-millisecond, but a registry of process-heavy sessions (an agent
    // mid-build has hundreds of descendants, and a no-agent session pays the
    // full walk) made the serial loop the dominant cost of the whole command
    // -- 83 of 95 ms on a live 17-session registry. Chunked scoped threads
    // keep it a fraction of the /proc reads it is made of, with the same
    // per-record answers as the serial order (each row's `agent` is
    // independent of every other's).
    let agents: BTreeMap<Uuid, Option<aplexer::agent_kind::AgentKind>> = {
        let records: Vec<&SessionRecord> = groups
            .iter()
            .flat_map(|(_, sessions)| sessions.iter())
            .collect();
        let worker_count = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, records.len().max(1));
        let chunk_size = records.len().div_ceil(worker_count);
        thread::scope(|scope| {
            let handles: Vec<_> = records
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|record| (record.id, aplexer::api::record_agent(record)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| {
                    handle.join().expect(
                        "agent detection worker panicked; detection is infallible by contract",
                    )
                })
                .collect()
        })
    };

    for (workspace_index, (workspace, sessions)) in groups.iter().enumerate() {
        if workspace_index > 0 {
            println!();
        }
        let states: Vec<(&'static str, bool, bool)> = sessions
            .iter()
            .map(|record| {
                let (state, _) = session_ui_state(record, now);
                (
                    state,
                    ui_state_is_active(state),
                    ui_state_needs_attention(state),
                )
            })
            .collect();
        let active = states.iter().filter(|(_, active, _)| *active).count();
        let attention = states.iter().filter(|(_, _, attention)| *attention).count();
        let stopped = sessions.len().saturating_sub(active);
        let mut summary = format!("{active} active");
        if attention > 0 {
            summary.push_str(&format!(" · {attention} needs you"));
        }
        if stopped > 0 {
            summary.push_str(&format!(" · {stopped} stopped"));
        }
        let here = current_workspace.as_deref() == Some(workspace.as_path());
        let badge = paint(
            color,
            &format!("{ANSI_BOLD}{ANSI_CYAN}"),
            &format!("[{}]", workspace_index + 1),
        );
        let name = paint(
            color,
            ANSI_BOLD,
            &display_workspace(workspace, home.as_deref()),
        );
        let marker = if here {
            paint(color, ANSI_CYAN, "  ← here")
        } else {
            String::new()
        };
        let recency = workspace_recency_label(sort, sessions, now);
        let recency = if recency.is_empty() {
            String::new()
        } else {
            format!(" · {recency}")
        };
        println!(
            "{badge} {name}{marker}  {}",
            paint(color, ANSI_DIM, &format!("{summary}{recency}"))
        );

        // Column widths adapt to the widest tag/engine actually present, so
        // a registry of long agent tags doesn't force every row to wrap.
        let tag_width = sessions
            .iter()
            .map(|record| terminal_display_width(&record.tag))
            .max()
            .unwrap_or(3)
            .clamp(6, 20);
        let engine_width = sessions
            .iter()
            .map(|record| {
                terminal_display_width(&engine_label(
                    record,
                    agents.get(&record.id).copied().flatten(),
                ))
            })
            .max()
            .unwrap_or(6)
            .clamp(6, 28);
        let last = sessions.len().saturating_sub(1);
        for (index, record) in sessions.iter().enumerate() {
            let (state, _, attention) = states[index];
            let connector = if index == last { "└─" } else { "├─" };
            let engine = engine_label(record, agents.get(&record.id).copied().flatten());
            let tag = paint(color, ANSI_BOLD, &fit_column(&record.tag, tag_width));
            let engine = paint(color, ANSI_DIM, &fit_column(&engine, engine_width));
            let (sdot, scolor) = state_glyph(state);
            let state_text = fit_column(&format!("{sdot} {state}"), 11);
            let state_text = paint(color, scolor, &state_text);
            let timestamp = state_timestamp(record, state, now);
            let age = paint(
                color,
                ANSI_DIM,
                &format!("{:>12}", human_age_phrase(now.saturating_sub(timestamp))),
            );
            let attention_mark = if attention {
                paint(color, ANSI_YELLOW, " !")
            } else {
                String::new()
            };
            let lineage = match lineage_labels.get(&record.id) {
                Some(label) => paint(color, ANSI_DIM, label),
                None => String::new(),
            };
            println!(
                "{} {:>2}  {}  {}  {}{} {}{}",
                paint(color, ANSI_GRAY, connector),
                index + 1,
                tag,
                engine,
                state_text,
                attention_mark,
                age,
                lineage
            );
        }
    }

    println!();
    println!(
        "{}",
        paint(
            color,
            ANSI_DIM,
            "Attach: a <workspace#> [session#|tag] · Here: a here [engine] [tag] · Another: a new · Help: a help"
        )
    );
    println!(
        "{}",
        paint(
            color,
            ANSI_DIM,
            &format!(
                "Sort: {} · a list --sort name|created|accessed|activity",
                sort.as_str()
            )
        )
    );
    if hidden_exited > 0 {
        println!(
            "{}",
            paint(
                color,
                ANSI_DIM,
                &format!("{hidden_exited} exited hidden · a list --all shows them")
            )
        );
    }
    Ok(())
}

/// The redirected rendering of `a list` -- the pre-UX format, unchanged so
/// piped/parsed output is stable across the UX work.
fn cmd_list_plain(paths: &Paths, args: ListArgs) -> Result<()> {
    let mut records = list_records(paths)?;
    if args.running {
        records.retain(|r| r.worker_phase_active() && r.worker_alive());
    }
    // Liveness is probed once per record here and reused for the workspace
    // header (`running_count`/`running_summary`) and every row below.
    // Benchmark PLAN P1.1: the old code called `worker_alive()` three times
    // per record (header count + header summary which recounts + row), and
    // each probe reads /proc, the identity sidecar, and (now cached) boot_id
    // -- with 25 sessions that triple-probe was the ~7 ms table-over-json
    // gap, since `--json` probes once. The map below makes plain rendering
    // probe exactly once per record.
    let alive: BTreeMap<Uuid, bool> = records.iter().map(|r| (r.id, r.worker_alive())).collect();
    let alive_of = |r: &SessionRecord| alive.get(&r.id).copied().unwrap_or(false);
    // Group by workspace as a compact tree -- spec.md's own presentation of
    // the model (sections 2 and 22.1) is a workspace tree with tags
    // underneath, not a flat table repeating the workspace on every row.
    // `a <N>` quick-attach (see cmd_quick_attach/resolve_quick_index) numbers
    // workspaces and sessions using this exact same grouping, so the
    // `[N]`/session-index prefixes printed below are not decoration -- they
    // are the literal numbers `a <N>` and `a <N> <M>` resolve against.
    let sort = load_list_sort(paths);
    let by_workspace = group_by_workspace(records, sort);
    let home = env::var_os("HOME").map(PathBuf::from);
    let color = color_enabled();
    for (workspace_index, (workspace, group)) in by_workspace.iter().enumerate() {
        if workspace_index > 0 {
            println!();
        }
        let (running, total) = running_count(group, &alive);
        let (dot, dot_color) = workspace_glyph(running, total);
        let badge = paint(
            color,
            &format!("{ANSI_BOLD}{ANSI_CYAN}"),
            &format!("[{}]", workspace_index + 1),
        );
        let name = paint(
            color,
            ANSI_BOLD,
            &display_workspace(workspace, home.as_deref()),
        );
        let summary = paint(
            color,
            dot_color,
            &format!("{dot} {}", running_summary(group, &alive)),
        );
        println!("{badge} {name} ({summary})");
        let last = group.len().saturating_sub(1);
        for (i, r) in group.iter().enumerate() {
            let connector_raw = if i == last {
                "\u{2514}\u{2500}\u{2500}"
            } else {
                "\u{251c}\u{2500}\u{2500}"
            };
            let connector = paint(color, ANSI_GRAY, connector_raw);
            let idx = paint(color, ANSI_DIM, &format!("{:>2}", i + 1));
            let tag = paint(color, ANSI_BOLD, &format!("{:<14}", r.tag));
            let ep = match &r.profile {
                Some(p) => format!("{}/{}", r.engine, p),
                None => r.engine.clone(),
            };
            let ep = paint(color, ANSI_DIM, &format!("{:<16}", ep));
            let state = derived_liveness(&r.phase, alive_of(r), r.created_at_ms);
            let (sdot, scolor) = state_glyph(state);
            let state = paint(color, scolor, &format!("{sdot} {state}"));
            println!("{connector} {idx}  {tag} {ep} {state}");
        }
    }
    if !by_workspace.is_empty() {
        println!();
        println!(
            "{}",
            paint(color, ANSI_DIM, "Attach: a <workspace#> [session#|tag]")
        );
        println!(
            "{}",
            paint(
                color,
                ANSI_DIM,
                "e.g. a 3 2, a 3 zsp, or a 3 for its first session"
            )
        );
    }
    Ok(())
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_GRAY: &str = "\x1b[90m";

/// Colors only when stdout is a real terminal and the user hasn't opted out
/// via `NO_COLOR` (https://no-color.org) -- `a list | grep foo` or similar
/// piping must never see escape codes.
fn color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

/// Wraps already-padded plain text in `code`/reset -- callers must pad
/// widths (`{:<14}` etc.) on the plain string BEFORE calling this, since
/// padding a string that already contains escape codes counts the invisible
/// bytes toward the width and breaks column alignment.
fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("{code}{text}{ANSI_RESET}")
    } else {
        text.to_string()
    }
}

fn state_glyph(state: &str) -> (&'static str, &'static str) {
    match state {
        "running" | "working" | "active" => ("\u{25CF}", ANSI_GREEN),
        "waiting" => ("!", ANSI_YELLOW),
        "starting" | "exiting" | "stopping" => ("\u{25D0}", ANSI_YELLOW),
        "failed" | "broken" | "oom" => ("\u{2717}", ANSI_RED),
        _ => ("\u{25CB}", ANSI_GRAY), // "exited", "idle", "quiet"
    }
}

/// The status bar's animated state glyph: a braille spinner frame while the
/// attached session's state is `working` -- a fresh `a state-report` push,
/// i.e. the agent *said* it is running -- and `None` for every other state,
/// meaning "keep `state_glyph`'s static glyph". Deliberately not `active`:
/// that state is a PTY-recency guess (it fires while the user merely types
/// at an agent TUI's prompt, and for any record with no activity sample at
/// all), so spinning on it would promise work that is not happening. The
/// reported state is the only signal that means "the agent is working" and
/// not just "the terminal is warm"; without hooks installed the bar simply
/// stays on its static glyph. So the bar only ever moves while there is
/// work to point at: an idle, waiting, or dead session renders byte-stable
/// text, and the dirty check in `draw_status_bar` keeps it write-free.
///
/// The frame index is a pure function of the wall clock, deliberately not
/// thread-local counter state: the status thread, the frame loop's pending
/// flush, and the input thread's flash redraw all render the bar
/// independently, and this way any two calls within the same
/// `SPINNER_FRAME_MS` window agree on the frame without sharing anything.
/// (Sanitized-then-padded like all bar text, and the same width-1 as the
/// `●` it replaces, so truncation math is unchanged.)
fn spinner_frame(state: &str, now_ms: u64) -> Option<char> {
    if state != "working" {
        return None;
    }
    let idx = (now_ms / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len();
    Some(SPINNER_FRAMES[idx])
}

/// The single human-facing state derivation: lifecycle facts first (a dead
/// worker behind a non-terminal phase is `broken` regardless of anything the
/// record claims), then the authoritative agent-state derivation shared with
/// `a watch` (`aplexer::watch::derive_agent_state_with_source` -- one set of
/// freshness thresholds, never a copy), mapped onto the honest human
/// vocabulary from docs/cli-ux.md section 4:
///
/// - a fresh `a state-report` push is semantic fact: `working`/`waiting`/`idle`
///   -- including for `shell`-engine sessions, where the agent was started
///   by hand inside the shell and the push is the only semantic signal
/// - PTY-recency inference never claims semantics: recent output is
///   `active`, silence is `quiet` -- deliberately NOT `waiting`, because
///   "the terminal went quiet" cannot tell a blocked agent from a long
///   compute step
/// - a plain shell that never reported any agent state is just `running`
///   no matter how quiet its PTY is; a shell an agent has lived in (any
///   state-report push in its history) gets the same activity words as a
///   first-class engine once nothing is fresh
///
/// Returns `(state, source)` where source is `reported`, `activity`, or
/// `lifecycle`, so callers can qualify inferred states instead of faking
/// certainty.
/// `observed_state` against the wall clock, for the query-time commands that
/// have no injected clock of their own. Every derived `state` a CLI command
/// prints goes through here or through `observed_state` directly, so none of
/// them can disagree about the startup window (issue #9).
fn derived_liveness(phase: &Phase, worker_alive: bool, created_at_ms: u64) -> &'static str {
    observed_state(phase, worker_alive, created_at_ms, now_ms())
}

fn session_ui_state(record: &SessionRecord, now: u64) -> (&'static str, &'static str) {
    // Deferred to `observed_state` rather than repeating its predicate, so
    // the TTY UI cannot go on painting a mid-create session `broken` after
    // the derived state stopped saying so.
    if observed_state(
        &record.phase,
        record.worker_alive(),
        record.created_at_ms,
        now,
    ) == "broken"
    {
        return ("broken", "lifecycle");
    }
    match record.phase {
        Phase::Starting => ("starting", "lifecycle"),
        Phase::Exiting => ("stopping", "lifecycle"),
        Phase::Exited => {
            let oom = record
                .exit
                .as_ref()
                .map(|exit| exit.oom_killed)
                .unwrap_or(false);
            (if oom { "oom" } else { "exited" }, "lifecycle")
        }
        Phase::Failed => ("failed", "lifecycle"),
        Phase::Running => {
            // A fresh state-report push is what the agent says it is --
            // checked before the shell early return below, because a hook
            // firing inside a shell session (the normal case: the agent was
            // launched by hand, `APLEXER_SESSION_ID` is still injected) is
            // fact, not a guess. Without it, an idle opencode/grok inside a
            // shell session would show `running` forever.
            let (state, source) = aplexer::watch::derive_agent_state_with_source(record, now);
            if source == "reported" {
                return match state {
                    "running" => ("working", "reported"),
                    "waiting" => ("waiting", "reported"),
                    "idle" => ("idle", "reported"),
                    // Defensive only: fresh_reported_state only ever
                    // produces the three values above.
                    _ => (state, source),
                };
            }
            // A shell an agent has lived in (any state-report push in the
            // record's history) is not a "plain shell": when nothing is
            // fresh, its quiet is an agent sitting at a prompt or thinking,
            // not a shell doing work, so it gets the same honest activity
            // words as a first-class engine. Only a shell that never
            // reported anything keeps the lifecycle `running` -- for a bare
            // prompt (or `tail -f`) that really is all that is known.
            if record.engine == "shell" && record.reported_state.is_none() {
                return ("running", "lifecycle");
            }
            match (state, source) {
                // The heuristic's "running/waiting" words imply agent
                // semantics the PTY cannot actually know; translate to
                // activity words that don't.
                ("running", _) => ("active", "activity"),
                ("waiting", _) => ("quiet", "activity"),
                (state, source) => (state, source),
            }
        }
    }
}

/// Whether a state word counts as "alive/working" in workspace summaries --
/// everything a live worker can be in, including the merely-quiet.
fn ui_state_is_active(state: &str) -> bool {
    matches!(
        state,
        "working" | "waiting" | "idle" | "active" | "quiet" | "running" | "starting" | "stopping"
    )
}

/// Whether a state word means "the human should look at this": reported
/// waits and every failure/health condition. Inferred quiet is deliberately
/// not attention -- it is usually just a long-running command.
fn ui_state_needs_attention(state: &str) -> bool {
    matches!(state, "waiting" | "broken" | "failed" | "oom")
}

/// How long a state word's evidence is stale-able, for the list's age
/// column: the state-report push for semantic states, last PTY output for
/// activity states, the exit for terminal ones. Falls back to the record's
/// own update time so the column always has something honest to show.
fn state_timestamp(record: &SessionRecord, state: &str, now: u64) -> u64 {
    let candidate = match state {
        "working" | "waiting" | "idle" => record.reported_state_at_ms,
        "active" | "quiet" => record.last_activity_ms,
        "exited" | "oom" => record.exit.as_ref().map(|exit| exit.exited_at_ms),
        _ => None,
    };
    candidate
        .filter(|at| *at <= now)
        .unwrap_or(record.updated_at_ms)
}

/// Compact age: `now`, `30s`, `5m`, `5h 1m`, `5d 5h`. Two units once the
/// span is at least an hour, so a week-old session is not just `5d`.
fn compact_elapsed(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 5 {
        return "now".to_string();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// The phrase form for prose contexts (`a status`): "just now", "4m ago".
fn human_age_phrase(ms: u64) -> String {
    match compact_elapsed(ms).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    }
}

/// Qualifier appended to a semantic state so a human can tell what kind of
/// fact they are looking at; empty for authoritative sources.
fn state_source_suffix(source: &str) -> &'static str {
    match source {
        "activity" => " (inferred from output activity)",
        _ => "",
    }
}

/// Pads or truncates to exactly `width` display cells, marking a truncation
/// with `…` (counted against the width), Unicode-width safe: CJK-width
/// glyphs and combining sequences never split mid-cluster or misalign the
/// columns built from `fit_column` calls.
fn fit_column(text: &str, width: usize) -> String {
    let display_width = terminal_display_width(text);
    if display_width <= width {
        return format!("{text}{}", " ".repeat(width - display_width));
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = terminal_display_width(grapheme);
        if used + grapheme_width > width - 1 {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result.push('…');
    result.push_str(&" ".repeat(width.saturating_sub(used + 1)));
    result
}

fn workspace_glyph(running: usize, total: usize) -> (&'static str, &'static str) {
    if running == total {
        ("\u{25CF}", ANSI_GREEN)
    } else if running == 0 {
        ("\u{25CB}", ANSI_GRAY)
    } else {
        ("\u{25D0}", ANSI_YELLOW)
    }
}

/// Shortens a workspace path under $HOME to `~/...`, matching spec.md's own
/// display examples (e.g. section 2's `~/git/pocketshell` tree).
fn display_workspace(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

fn running_count(group: &[SessionRecord], alive: &BTreeMap<Uuid, bool>) -> (usize, usize) {
    let running = group
        .iter()
        .filter(|r| {
            derived_liveness(
                &r.phase,
                alive.get(&r.id).copied().unwrap_or(false),
                r.created_at_ms,
            ) == "running"
        })
        .count();
    (running, group.len())
}

fn running_summary(group: &[SessionRecord], alive: &BTreeMap<Uuid, bool>) -> String {
    let (running, total) = running_count(group, alive);
    if running == total {
        format!("running {running}")
    } else if running == 0 {
        format!("stopped {total}")
    } else {
        format!("running {running}/{total}")
    }
}

fn group_by_workspace(
    records: Vec<SessionRecord>,
    sort: ListSort,
) -> Vec<(PathBuf, Vec<SessionRecord>)> {
    let mut groups: Vec<(PathBuf, Vec<SessionRecord>)> = Vec::new();
    for r in records {
        match groups.iter_mut().find(|(ws, _)| *ws == r.workspace) {
            Some((_, group)) => group.push(r),
            None => groups.push((r.workspace.clone(), vec![r])),
        }
    }
    groups.sort_by(|a, b| compare_workspaces(a, b, sort));
    groups
}

fn compare_workspaces(
    left: &(PathBuf, Vec<SessionRecord>),
    right: &(PathBuf, Vec<SessionRecord>),
    sort: ListSort,
) -> std::cmp::Ordering {
    let time_order =
        |left_ms: u64, right_ms: u64| right_ms.cmp(&left_ms).then_with(|| left.0.cmp(&right.0));
    match sort {
        ListSort::Name => left.0.cmp(&right.0),
        ListSort::Created => time_order(
            workspace_created_ms(&left.1),
            workspace_created_ms(&right.1),
        ),
        ListSort::Accessed => time_order(
            workspace_accessed_ms(&left.1),
            workspace_accessed_ms(&right.1),
        ),
        ListSort::Activity => time_order(
            workspace_activity_ms(&left.1),
            workspace_activity_ms(&right.1),
        ),
    }
}

fn workspace_created_ms(sessions: &[SessionRecord]) -> u64 {
    sessions.iter().map(|s| s.created_at_ms).max().unwrap_or(0)
}

/// Recency of human access: last attach, falling back to created so
/// never-attached records (including those from before `last_accessed_ms`
/// existed) still have a stable place in the order.
fn workspace_accessed_ms(sessions: &[SessionRecord]) -> u64 {
    sessions
        .iter()
        .map(|s| s.last_accessed_ms.unwrap_or(s.created_at_ms))
        .max()
        .unwrap_or(0)
}

fn last_agent_activity_ms(record: &SessionRecord) -> Option<u64> {
    match (record.last_activity_ms, record.reported_state_at_ms) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn workspace_activity_ms(sessions: &[SessionRecord]) -> u64 {
    sessions
        .iter()
        .filter_map(last_agent_activity_ms)
        .max()
        .unwrap_or(0)
}

fn workspace_recency_label(sort: ListSort, sessions: &[SessionRecord], now: u64) -> String {
    let ago = |at: u64| human_age_phrase(now.saturating_sub(at));
    match sort {
        ListSort::Name => String::new(),
        ListSort::Created => format!("created {}", ago(workspace_created_ms(sessions))),
        ListSort::Accessed => match sessions.iter().filter_map(|s| s.last_accessed_ms).max() {
            Some(at) => format!("opened {}", ago(at)),
            None => "never opened".to_string(),
        },
        ListSort::Activity => match workspace_activity_ms(sessions) {
            0 => "no activity".to_string(),
            at => format!("active {}", ago(at)),
        },
    }
}

fn list_sort_path(paths: &Paths) -> PathBuf {
    paths.state_root.join("list-sort")
}

fn load_list_sort(paths: &Paths) -> ListSort {
    fs::read_to_string(list_sort_path(paths))
        .ok()
        .and_then(|text| ListSort::parse(text.trim()))
        .unwrap_or(ListSort::Name)
}

fn save_list_sort(paths: &Paths, sort: ListSort) -> Result<()> {
    fs::write(list_sort_path(paths), format!("{}\n", sort.as_str()))
        .with_context(|| format!("write {}", list_sort_path(paths).display()))
}

/// `--sort KEY` both applies and remembers; a bare `a list` (and `a N`)
/// reuse the last choice so the numbers on the tree stay stable.
fn resolve_list_sort(paths: &Paths, requested: Option<ListSort>) -> Result<ListSort> {
    if let Some(sort) = requested {
        save_list_sort(paths, sort)?;
        return Ok(sort);
    }
    Ok(load_list_sort(paths))
}

/// `a <N>` / `a <N> <M>` / `a <N> <tag>` -- attach by position in the same
/// workspace tree `a list` prints, or by tag within a chosen workspace.
/// `a -` and friends -- create-or-attach in the current directory, agent
/// engines and tags used the same way spec.md's own worked examples do
/// (workspace ~/git/pocketshell, tags main/review/issue-2294, engines
/// claude/codex). Whether the first word after "-" names a real engine, a
/// shortcut, or is a literal command to run (mirroring tmuxctl's `t -
/// <command>`) is decided against the real engine registry and the
/// `config.shortcuts` map -- never a fixed word list -- and in that
/// precedence order:
///
///   1. real engine id (`config.engines`)
///   2. shortcut id (`config.shortcuts`)
///   3. literal command
///
/// Engines are checked first so a real engine name always means exactly
/// what it says -- `a - claude` must never behave differently just because
/// someone also configured a shortcut named "claude". Shortcuts are checked
/// next, ahead of the literal-command fallback: a shortcut is meant to be a
/// fast path onto exactly what typing the full `--engine`/`--profile` pair
/// would already produce (see spec.md 9/23), so it sits directly below real
/// engine names and above running an arbitrary binary. In practice a
/// shortcut id realistically never collides with a real engine id (they're
/// deliberately short, e.g. "cl"/"coz") or with a command someone would
/// actually type standalone, but the ordering is still deliberate rather
/// than incidental.
///
///   a -                  tag "main", default engine
///   a - claude           tag "claude" (defaults to the engine name), engine claude
///   a - claude review    tag "review", engine claude
///   a - clz              tag "clz" (defaults to the shortcut's own id, not
///                        "claude" -- so `a - cl` and `a - clz` don't
///                        collide on the same tag), engine claude, profile zlaude
///   a - clz review       tag "review", engine claude, profile zlaude
///   a - htop             tag "htop" (defaults to the command name), runs `htop` literally
///
/// Re-running the same shortcut reattaches to a live matching session
/// instead of erroring, like tmuxctl's own create_or_attach.
/// Default tag for a literal-command quick-launch: the command's own base
/// name, normalized to the charset validate_tag accepts. Deliberately NOT
/// "main" for every arbitrary command -- `a - htop` reusing the same tag as
/// `a -`'s plain shell would silently reattach to that shell instead of
/// ever running htop.
fn command_tag(word: &str) -> String {
    let base = Path::new(word)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(word);
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "cmd".to_string()
    } else {
        sanitized
    }
}

fn cmd_quick_launch(paths: &Paths, args: QuickLaunchArgs) -> Result<()> {
    let workspace = canonical_workspace(Path::new("."))?;
    let config = Config::load(paths)?;
    // See the precedence note on the doc comment above: real engine id,
    // then shortcut id, then literal command.
    let (tag, engine, profile, command): (String, Option<String>, Option<String>, Vec<OsString>) =
        match args.rest.as_slice() {
            [] => ("main".to_string(), None, None, vec![]),
            [engine] if config.engines.contains_key(engine) => {
                (engine.clone(), Some(engine.clone()), None, vec![])
            }
            [engine, tag] if config.engines.contains_key(engine) => {
                (tag.clone(), Some(engine.clone()), None, vec![])
            }
            [word] if config.shortcuts.contains_key(word) => {
                let shortcut = &config.shortcuts[word];
                (
                    word.clone(),
                    Some(shortcut.engine.clone()),
                    shortcut.profile.clone(),
                    vec![],
                )
            }
            [word, tag] if config.shortcuts.contains_key(word) => {
                let shortcut = &config.shortcuts[word];
                (
                    tag.clone(),
                    Some(shortcut.engine.clone()),
                    shortcut.profile.clone(),
                    vec![],
                )
            }
            words => (
                command_tag(&words[0]),
                None,
                None,
                words.iter().map(OsString::from).collect(),
            ),
        };
    if let Some(existing) = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == tag)
    {
        // `a -` attaches only to something that can actually be attached
        // to: a live worker in a non-terminal phase. Everything else falls
        // through to cmd_start, which owns the single claim decision
        // (`reap_verdict`, applied by `start_session`) -- so this is not a
        // second copy of the ownership rule that could drift from it.
        //
        // That means a *broken* holder (non-terminal phase, dead worker,
        // nothing left running) no longer needs an explicit `a kill` or
        // `a prune` first: start reclaims the pair, archives the corpse and
        // creates the session, which is what `a -` promised all along. A
        // holder whose worker or workload is still alive is still refused
        // there, so `a -` can never create a second session for a pair that
        // something is still using.
        if existing.worker_phase_active() && existing.worker_alive() {
            return attach(paths, &existing, None, false);
        }
    }
    cmd_start(
        paths,
        StartArgs {
            workspace: PathBuf::from("."),
            tag,
            engine,
            profile,
            cwd: None,
            env: vec![],
            memory: None,
            pids: None,
            cpu_quota_us: None,
            cpu_period_us: 100_000,
            history_bytes: None,
            attach: true,
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            no_skip_permissions: false,
            fresh: false,
            command,
        },
        false,
    )
}

fn cmd_quick_attach(paths: &Paths, args: QuickAttachArgs) -> Result<()> {
    let record = resolve_quick_index(paths, args.workspace_index, args.session.as_deref())?;
    attach(paths, &record, None, false)
}

/// Total wall-clock budget one `a prune` run may spend waiting for workers
/// that its own record says are on their way out. Shared across every
/// record in the run so a registry full of dying sessions cannot make prune
/// hang: `a kill` only returns once the workload's containment domain is
/// empty, so the worker it leaves behind is milliseconds from exiting, not
/// seconds -- this budget is sized for a saturated box, and burning all of
/// it can only produce the pre-existing "retained" answer, never a wrong
/// removal.
const PRUNE_TERMINATION_BUDGET: Duration = Duration::from_secs(5);
const PRUNE_TERMINATION_POLL: Duration = Duration::from_millis(25);

struct PruneOutcome {
    removed: Vec<Uuid>,
    removed_without_containment_proof: Vec<Uuid>,
    retained_count: usize,
}

enum ReapResult {
    Removed {
        containment_proven: bool,
    },
    Retained,
    /// The record disappeared between the registry scan and the lock --
    /// another `a prune`/`a kill`/`a forget` got there first. Neither
    /// removed by us nor still present to retain.
    Vanished,
}

/// Wait, within the run's shared budget, for a worker whose own record says
/// it is terminating. Returns the record as it stands afterwards: the worker
/// finishes its lifecycle while we wait (writing its exit, its terminal
/// phase and its containment proof), so the stale in-memory copy must not be
/// the one the reap decision is made from.
fn settle_terminating_record(
    paths: &Paths,
    record: SessionRecord,
    deadline: Instant,
) -> SessionRecord {
    if !record.worker_alive() || !record.worker_is_terminating() {
        return record;
    }
    let mut current = record;
    while Instant::now() < deadline {
        thread::sleep(PRUNE_TERMINATION_POLL);
        match read_session_record(paths, current.id) {
            Ok(fresh) => current = fresh,
            // Vanished or unreadable mid-flight: hand back what we have and
            // let the locked re-read below decide.
            Err(_) => return current,
        }
        if !current.worker_alive() {
            break;
        }
    }
    current
}

/// Remove one record's durable state, re-deciding under the registry lock.
///
/// The scan-time verdict is advisory: `start_session` holds this same lock
/// across the whole spawn, so a record that looked like a dead `Starting`
/// stub during the scan can be a fully live session by the time the lock is
/// ours. Re-read and re-check before destroying anything, and fence a
/// pre-PID worker the same way `a forget` does, so a worker spawned but not
/// yet registered cannot come up on top of a removed record.
fn reap_session_state(paths: &Paths, id: Uuid) -> Result<ReapResult> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let current = match read_session_record(paths, id) {
        Ok(record) => record,
        Err(_) if !paths.record(id).exists() => return Ok(ReapResult::Vanished),
        Err(error) => return Err(error).with_context(|| format!("re-read session {id}")),
    };
    let Some(verdict) = reap_verdict(&current) else {
        return Ok(ReapResult::Retained);
    };
    let _startup_absence_lock = if current.worker_phase_active() && current.worker_pid.is_none() {
        let lock_path = paths.worker_lock(current.id);
        match FileLock::exclusive(&lock_path, true) {
            Ok(lock) => Some(lock),
            // Held: a worker exists for this record even though it has not
            // registered a pid yet. Not ours to remove.
            Err(_) => return Ok(ReapResult::Retained),
        }
    } else {
        None
    };
    fs::remove_dir_all(paths.state_session(id))
        .with_context(|| format!("remove stale session {id} durable state"))?;
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    Ok(ReapResult::Removed {
        containment_proven: verdict == ContainmentReap::Proven,
    })
}

fn prune_dead_sessions(paths: &Paths) -> Result<PruneOutcome> {
    reap_sweep(paths, true)
}

/// The opportunistic sweep the default list runs before rendering: the same
/// verdict and locked-removal machinery as `a prune`, minus the wait for an
/// in-flight worker teardown. A worker that is still alive is retained here
/// and its own teardown (or a later sweep) decides the outcome, so nothing
/// a later `a prune` would have kept can be removed early.
fn sweep_prunable_corpses(paths: &Paths) -> Result<PruneOutcome> {
    reap_sweep(paths, false)
}

fn reap_sweep(paths: &Paths, wait_for_terminating: bool) -> Result<PruneOutcome> {
    let deadline = Instant::now() + PRUNE_TERMINATION_BUDGET;
    let mut outcome = PruneOutcome {
        removed: Vec::new(),
        removed_without_containment_proof: Vec::new(),
        retained_count: 0,
    };
    for record in list_records(paths)? {
        let record = if wait_for_terminating {
            settle_terminating_record(paths, record, deadline)
        } else {
            record
        };
        if reap_verdict(&record).is_none() {
            outcome.retained_count += 1;
            continue;
        }
        match reap_session_state(paths, record.id)? {
            ReapResult::Removed { containment_proven } => {
                outcome.removed.push(record.id);
                if !containment_proven {
                    outcome.removed_without_containment_proof.push(record.id);
                }
            }
            ReapResult::Retained => outcome.retained_count += 1,
            ReapResult::Vanished => {}
        }
    }
    Ok(outcome)
}

fn cmd_prune(paths: &Paths, json_output: bool) -> Result<()> {
    let outcome = prune_dead_sessions(paths)?;
    // Say plainly which reaps rested on "nothing left to hold on to" rather
    // than on a worker's own proof that its containment domain was empty --
    // the same distinction `a forget --force` reports, minus its scarier
    // wording, which was never accurate for a record whose leader is also
    // provably gone.
    for id in &outcome.removed_without_containment_proof {
        eprintln!(
            "a: removed broken session {id} without a containment proof; its worker died without recording one and nothing addressable remained"
        );
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": outcome.removed,
                "removed_without_containment_proof": outcome.removed_without_containment_proof,
                "retained_count": outcome.retained_count,
            }))?
        );
    } else if outcome.removed.is_empty() {
        println!("no dead sessions to prune");
    } else {
        for id in &outcome.removed {
            println!("removed {id}");
        }
        println!("removed {} session(s)", outcome.removed.len());
    }
    Ok(())
}

fn cmd_forget(paths: &Paths, args: ForgetArgs, json_output: bool) -> Result<()> {
    // Only the CLI's target spellings (quick index, tag, `workspace:tag`) and
    // its presentation live here. The destructive body -- force gate,
    // live-worker refusal, pre-PID fence, both removals, and the survival
    // warning -- is `api::forget_session`, shared with the Python binding so
    // the two cannot diverge (issue #11). Re-resolving by id there is cheap
    // and keeps the record re-read under the registry lock where it belongs.
    let selected = resolve(paths, &args.target)?;
    let value = aplexer::api::forget_session(paths, &selected.id.to_string(), args.force)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("forgotten {}", selected.id);
    }
    Ok(())
}

/// Shared by the bare `a <N>` shortcut and by `resolve()` (so `a attach 1`,
/// `a status 1`, `a kill 1`, etc. all understand the same numbers `a list`
/// prints, not just the no-subcommand form). Exited sessions are skipped
/// (session_is_listed), so the numbers track the default list's rows rather
/// than the full registry -- a corpse found via `a list --all` is addressed
/// by tag or UUID prefix, not by its --all index.
fn resolve_quick_index(
    paths: &Paths,
    workspace_index: usize,
    session: Option<&str>,
) -> Result<SessionRecord> {
    let mut records = list_records(paths)?;
    let now = now_ms();
    records.retain(|record| session_is_listed(record, now));
    let groups = group_by_workspace(records, load_list_sort(paths));
    if groups.is_empty() {
        bail!("no sessions found (see `a start`)");
    }
    if workspace_index < 1 || workspace_index > groups.len() {
        bail!(
            "workspace index {workspace_index} out of range: {} workspace(s) found (see `a list`)",
            groups.len()
        );
    }
    let (workspace, sessions) = &groups[workspace_index - 1];
    if sessions.is_empty() {
        bail!("workspace {} has no sessions", workspace.display());
    }
    match session {
        None => Ok(sessions[0].clone()),
        Some(selector) if !selector.is_empty() && selector.bytes().all(|b| b.is_ascii_digit()) => {
            let index: usize = selector.parse().unwrap_or(0);
            if index < 1 || index > sessions.len() {
                bail!(
                    "session index {index} out of range: workspace {} has {} session(s)",
                    workspace.display(),
                    sessions.len()
                );
            }
            Ok(sessions[index - 1].clone())
        }
        Some(tag) => sessions
            .iter()
            .find(|r| r.tag == tag)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no session tagged {tag:?} in workspace {}",
                    workspace.display()
                )
            }),
    }
}

fn cmd_status(paths: &Paths, target: TargetArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &target)?;
    // Process existence and control-plane reachability are separate facts:
    // a wedged worker can still have a live pid, while a successfully reached
    // worker is stronger evidence than a stale persisted pid. Preserve both
    // instead of folding them into one optimistic `worker_alive` bit, and
    // surface the actual RPC failure so recovery tooling has evidence to act
    // on rather than a mysteriously stale record.
    let (raw, worker_reachable, rpc_error) = match rpc_simple(&record, Operation::Status, None) {
        Ok(raw) => (raw, true, None),
        Err(error) => (
            serde_json::to_value(public_session_record(&record)).unwrap_or(Value::Null),
            false,
            Some(format!("{error:#}")),
        ),
    };
    let current: SessionRecord = serde_json::from_value(raw.clone()).unwrap_or(record);
    let cgroup_stats = raw.get("cgroup").cloned();
    let history_persistence_error = raw
        .get("history_persistence_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    let record_persistence_error = raw
        .get("record_persistence_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Live-only (see foreground_command in lib.rs / Operation::Status):
    // never persisted to session.json, so this is only available while the
    // worker is reachable -- absent on a dead/unreachable session, same as
    // cgroup_stats above.
    let foreground_command = raw
        .get("foreground_command")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let worker_alive = current.worker_alive();
    if json_output {
        let mut value = serde_json::to_value(public_session_record(&current))?;
        if let Some(stats) = cgroup_stats {
            value["cgroup"] = stats;
        }
        if let Some(fg) = &foreground_command {
            value["foreground_command"] = json!(fg);
        }
        if let Some(error) = &history_persistence_error {
            value["history_persistence_error"] = json!(error);
        }
        if let Some(error) = &record_persistence_error {
            value["record_persistence_error"] = json!(error);
        }
        value["worker_alive"] = json!(worker_alive);
        // The same derived fact the human branch prints as `state:` and
        // every `a list --json`/`a snapshot` row carries, from the same
        // helper so the three can never disagree: a SIGKILLed worker
        // leaves `phase` at "running" forever, so a machine consumer of
        // `status` reading `phase` alone could not tell a zombie record
        // from a live session -- while the same command was telling a
        // human "broken".
        value["state"] = json!(derived_liveness(
            &current.phase,
            worker_alive,
            current.created_at_ms
        ));
        // Which agent is running inside the session's workload tree right
        // now, from the same query-time detection every `a list --json` row
        // carries (`api::record_agent`). Always present; `null` when no
        // agent is detectable.
        value["agent"] = json!(aplexer::api::record_agent(&current));
        // Same derived placement facts every `a list --json`/`a snapshot`
        // row carries, from the same helper so no two commands can
        // disagree about whether a session shares the per-user manager's
        // failure domain (issue #1).
        value["worker_placement"] =
            aplexer::placement::placement_summary(current.worker_cgroup.as_deref());
        value["workload_placement"] =
            aplexer::placement::placement_summary(current.workload_cgroup.as_deref());
        value["worker_reachable"] = json!(worker_reachable);
        if let Some(error) = &rpc_error {
            value["rpc_error"] = json!(error);
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if io::stdout().is_terminal() {
        cmd_status_tty(
            paths,
            &current,
            &raw,
            worker_reachable,
            rpc_error.as_deref(),
            history_persistence_error.as_deref(),
            record_persistence_error.as_deref(),
        )?;
    } else {
        println!("id: {}", current.id);
        println!("selector: {}", current.selector());
        println!(
            "state: {}",
            derived_liveness(&current.phase, worker_alive, current.created_at_ms)
        );
        let ep = match &current.profile {
            Some(p) => format!("{}/{p}", current.engine),
            None => current.engine.clone(),
        };
        // Filtered the same way the attach status bar filters it
        // (`foreground_override`): omit a bare interactive shell or a
        // foreground command that's just the engine's own launch command
        // running as expected, so this line matches what `a status` calls
        // out as "different from what you started."
        match foreground_override(&current, &raw) {
            Some(fg) => println!("engine: {ep} (foreground: {fg})"),
            None => println!("engine: {ep}"),
        }
        println!(
            "worker_pid: {}",
            current
                .worker_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!("worker_alive: {worker_alive}");
        println!("worker_reachable: {worker_reachable}");
        if let Some(error) = rpc_error {
            println!("rpc_error: {error}");
        }
        println!(
            "workload_pid: {}",
            current
                .workload_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "command: {}",
            current
                .command
                .iter()
                .map(|v| shell_quote(v))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Some(exit) = current.exit {
            println!(
                "exit: code={:?} signal={:?} oom_killed={}",
                exit.code, exit.signal, exit.oom_killed
            );
        }
        if let Some(stats) = cgroup_stats {
            println!("cgroup: {stats}");
        }
        if let Some(error) = current.error {
            println!("error: {error}");
        }
        if let Some(error) = history_persistence_error {
            println!("history_persistence_error: {error}");
        }
        if let Some(error) = record_persistence_error {
            println!("record_persistence_error: {error}");
        }
    }
    Ok(())
}

/// The terminal rendering of `a status` -- task-first (tag and state lead;
/// pids and sockets are evidence at the bottom), state qualified by its
/// source, and exactly one next action chosen from lifecycle/reachability/
/// containment evidence rather than a generic "try these commands" list.
/// The redirected rendering above stays byte-identical to the pre-UX format.
fn cmd_status_tty(
    paths: &Paths,
    current: &SessionRecord,
    raw: &Value,
    worker_reachable: bool,
    rpc_error: Option<&str>,
    history_persistence_error: Option<&str>,
    record_persistence_error: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    let (mut state, mut source) = session_ui_state(current, now);
    // A live worker that will not answer is its own condition -- more
    // specific than any state the record could claim.
    if current.worker_alive() && !worker_reachable {
        state = "unreachable";
        source = "lifecycle";
    }
    let color = color_enabled();
    let short_id = current.id.to_string()[..8].to_string();
    let workspace = display_workspace(
        &current.workspace,
        env::var_os("HOME").as_deref().map(Path::new),
    );
    let engine = match &current.profile {
        Some(profile) => format!("{}/{}", current.engine, profile),
        None => current.engine.clone(),
    };

    let (glyph, glyph_color) = state_glyph(state);
    println!(
        "{}  {}",
        paint(color, ANSI_BOLD, &current.tag),
        paint(color, glyph_color, &format!("{glyph} {state}"))
    );
    let suffix = state_source_suffix(source);
    if !suffix.is_empty() {
        println!("  {}", paint(color, ANSI_DIM, suffix.trim()));
    }
    let lifecycle = derived_liveness(
        &current.phase,
        current.worker_alive(),
        current.created_at_ms,
    );
    if lifecycle != state {
        println!(
            "  {}",
            paint(color, ANSI_DIM, &format!("lifecycle: {lifecycle}"))
        );
    }
    println!("  workspace   {workspace}");
    println!("  engine      {engine}");
    // Same display rule as the list and the attach status bar: the detected
    // agent gets its own line only when the declared engine doesn't already
    // name it (`api::record_agent`, the value `a status --json` reports as
    // `agent`).
    if let Some(agent) = extra_agent_label(current, aplexer::api::record_agent(current)) {
        println!("  agent       {agent}");
    }
    println!("  session     {}", current.id);
    if let Some(parent) = current.parent_session {
        // Same rendering rule as `a list`: the parent's tag while its
        // record exists, a short id once it doesn't.
        let label = read_record(&paths.record(parent))
            .map(|record| record.tag)
            .unwrap_or_else(|_| parent.to_string()[..8].to_string());
        println!(
            "  {}",
            paint(color, ANSI_DIM, &format!("parent      {label}"))
        );
    }
    if let Some(foreground) = foreground_override(current, raw) {
        println!("  foreground  {foreground}");
    }
    let activity = raw
        .get("last_activity_ms")
        .and_then(Value::as_u64)
        .or(current.last_activity_ms);
    let activity_text = match activity {
        Some(at) if at <= now => human_age_phrase(now - at),
        _ => "unknown".to_string(),
    };
    println!("  activity    {activity_text}");
    println!(
        "  command     {}",
        current
            .command
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "  processes   worker {} ({}) · workload {}",
        current
            .worker_pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "—".to_string()),
        if worker_reachable {
            "reachable"
        } else {
            "unreachable"
        },
        current
            .workload_pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "—".to_string())
    );
    if let Some(exit) = &current.exit {
        println!(
            "  exit        code={:?} signal={:?} oom={}",
            exit.code, exit.signal, exit.oom_killed
        );
    }
    if let Some(error) = current.error.as_deref() {
        println!("  error       {error}");
    }
    if let Some(error) = rpc_error {
        println!("  rpc         {error}");
    }
    if let Some(error) = history_persistence_error {
        println!("  history     {error}");
    }
    if let Some(error) = record_persistence_error {
        println!("  record      {error}");
    }
    if let Some(cgroup) = raw.get("cgroup") {
        if !cgroup.is_null() {
            println!("  resources   {cgroup}");
        }
    }
    println!();

    // One next action, chosen from the same evidence model `a doctor` uses:
    // attach what is live, capture what is over, and get dead records out of
    // the way by the cheapest safe route (`a prune` when the record is one
    // it can reap, else `a kill` -- whose own refusal message is the right
    // teacher for the rare uncontainable case). A live-but-unreachable
    // worker gets a diagnosis pointer, not a destructive command.
    let attachable = matches!(
        current.phase,
        Phase::Starting | Phase::Running | Phase::Exiting
    ) && current.worker_alive()
        && worker_reachable;
    if attachable {
        println!("Attach: a open {short_id}");
        return Ok(());
    }
    if matches!(current.phase, Phase::Exited | Phase::Failed) || state == "broken" {
        println!("Inspect output: a capture {short_id} --screen --plain");
    }
    if !current.worker_alive() {
        if reap_verdict(current).is_some() {
            println!("Remove record:  a prune");
        } else {
            println!("Remove record:  a kill {short_id}");
        }
    } else if !worker_reachable {
        println!("Diagnose:       a check");
    }
    Ok(())
}

fn cmd_send(paths: &Paths, mut args: SendArgs, json_output: bool) -> Result<()> {
    // `a send --workspace W --tag T "text"` parses "text" into the flattened
    // TargetArgs selector positional (clap fills positionals in declaration
    // order), which then fails to resolve as a session -- or worse, silently
    // matches one. When the target is already fully named by flags, a lone
    // positional can only have been meant as the text.
    if args.text.is_none()
        && !args.stdin
        && args.target.selector.is_some()
        && (args.target.workspace.is_some() || args.target.tag.is_some())
    {
        args.text = args.target.selector.take();
    }
    let record = resolve(paths, &args.target)?;
    check_attachable(&record)?;
    let mut data = if args.stdin {
        let mut v = Vec::new();
        io::stdin().read_to_end(&mut v)?;
        v
    } else {
        args.text.unwrap_or_default().into_bytes()
    };
    if args.hex {
        data = parse_hex(&data)?;
    }
    if args.enter {
        data.push(b'\n');
    }
    if data.is_empty() {
        bail!("no bytes to send");
    }
    let mut sent = 0usize;
    for chunk in data.chunks(MAX_FRAME_BYTES) {
        rpc_send(&record, chunk)?;
        sent += chunk.len();
    }
    if json_output {
        println!("{}", json!({"id":record.id,"bytes":sent}));
    }
    Ok(())
}

fn base64_standard(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn capture_json_value(record: &SessionRecord, data: &[u8]) -> Value {
    let mut value = json!({
        "id": record.id,
        "bytes": data.len(),
        "encoding": "base64",
        "data": base64_standard(data),
    });
    // Preserve the old ergonomic field for text consumers, but only when it
    // is exact. `from_utf8_lossy` corrupted arbitrary PTY bytes while still
    // presenting the replacement-filled string as if it were authoritative.
    if let Ok(text) = std::str::from_utf8(data) {
        value["utf8"] = json!(text);
    }
    value
}

fn cmd_capture(paths: &Paths, args: CaptureArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let data = if args.screen {
        match rpc_capture_screen(&record, args.plain) {
            Ok(data) => data,
            // Dead-session fallback (design doc section 5.5/8): screen.txt
            // is the plain-text screen as it looked the moment the worker
            // exited, written once by OutputHub::finish. Unlike the raw
            // history fallback below, there is no paintable-form fallback
            // for a dead session -- the live grid died with the worker, and
            // only the plain text was preserved -- so --screen without
            // --plain against a dead session still surfaces the "worker
            // unavailable" error rather than silently downgrading to text.
            Err(_) if args.plain => match fs::read(paths.screen_txt(record.id)) {
                Ok(bytes) => bytes,
                Err(read_error) => {
                    check_attachable(&record)?;
                    return Err(read_error)
                        .context("worker unavailable and persisted screen.txt cannot be read");
                }
            },
            Err(error) => {
                check_attachable(&record)?;
                return Err(error).context("worker unavailable");
            }
        }
    } else {
        match rpc_capture(&record, args.bytes) {
            Ok(data) => data,
            // Persisted history is authoritative post-mortem data only once
            // the record is terminal or the worker process is known gone. A
            // live process returning an RPC error may merely be wedged or
            // temporarily unreachable; silently returning an older file in
            // that case makes stale output look current and hides the actual
            // operational failure.
            Err(_)
                if matches!(record.phase, Phase::Exited | Phase::Failed)
                    || !record.worker_alive() =>
            {
                match read_persisted_history_tail(&record.history_path, args.bytes) {
                    Ok(bytes) => bytes,
                    Err(read_error) => {
                        check_attachable(&record)?;
                        return Err(read_error)
                            .context("worker unavailable and persisted history cannot be read");
                    }
                }
            }
            Err(error) => {
                return Err(error).context(
                    "capture RPC failed while the worker process is still alive; refusing to return potentially stale persisted history",
                );
            }
        }
    };
    if let Some(path) = args.output {
        fs::write(&path, &data).with_context(|| format!("write {}", path.display()))?;
    } else if json_output {
        println!("{}", capture_json_value(&record, &data));
    } else {
        io::stdout().write_all(&data)?;
    }
    Ok(())
}

/// Deletes a session's full on-disk state: the state dir holding
/// `session.json` (the record itself), plus a best-effort cleanup of its
/// runtime dir (control socket, worker lock -- already gone or about to be,
/// in every caller). Takes the registry lock the same way `cmd_start`'s
/// superseding logic does, to avoid racing a concurrent `a start` that
/// might be reclaiming the same workspace+tag at the same moment. Shared by
/// every `a kill` path that actually retires a session's record, so
/// "removed" means the same thing everywhere instead of each call site
/// growing its own slightly-different deletion routine.
fn remove_session_state(paths: &Paths, id: Uuid) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    fs::remove_dir_all(paths.state_session(id))?;
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    Ok(())
}

/// How long `cmd_kill` waits, after an accepted kill RPC, for the worker to
/// remove the killed session's durable record itself. Normal finalization
/// lands within milliseconds (bounded above by the worker's attach-drain
/// window), so this is a settling pause, not a retry campaign; the deadline
/// only bounds the pathological cases, which are reported, never looped on.
const KILL_RECORD_REMOVAL_WAIT: Duration = Duration::from_secs(5);

/// Outcome of waiting for a killed session's record to disappear. The
/// worker that accepted the kill RPC removes the record during
/// finalization, but only when finalization ran clean and proved the
/// containment domain empty -- so `Kept` (worker exited, record stayed)
/// means the worker had something to say about this exit, and `Pending`
/// (worker still alive at the deadline) means the removal is still in
/// flight or the worker is holding the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillRecordOutcome {
    Removed,
    Kept,
    Pending,
}

/// After an accepted kill RPC, watch the record directory until the worker
/// deletes it (or until [`KILL_RECORD_REMOVAL_WAIT`] runs out). Only the
/// worker may remove a record whose worker process is still finishing --
/// deleting it client-side would race the worker's own final record write
/// into a persist-error retry loop -- so this observes instead of acting.
fn wait_for_kill_record_removal(paths: &Paths, id: Uuid) -> KillRecordOutcome {
    let deadline = Instant::now() + KILL_RECORD_REMOVAL_WAIT;
    // Polled at 5 ms, not 25 ms: the worker's fast-path finalization for an
    // accepted kill (benchmark PLAN P0.2) removes the record in tens of
    // milliseconds, so a 25 ms quantum here is a large fraction of the whole
    // `a kill` latency. Short-lived and infrequent -- one wait per kill.
    while Instant::now() < deadline {
        if !paths.record(id).exists() {
            return KillRecordOutcome::Removed;
        }
        thread::sleep(Duration::from_millis(5));
    }
    match read_record(&paths.record(id)) {
        Ok(record) if record.worker_alive() => KillRecordOutcome::Pending,
        _ => KillRecordOutcome::Kept,
    }
}

/// A worker pid may still exist even though its control socket is gone.
/// Only this one rare case counts as "force-cleanable": a live, reachable
/// worker can also fail an RPC, but then it must not be signalled directly.
/// ESRCH is success because the process may exit between checks.
fn force_kill_stale_worker(record: &SessionRecord) -> Result<()> {
    signal_recorded_worker(record, libc::SIGKILL).context("force-kill unreachable worker")
}

fn cmd_kill(paths: &Paths, args: KillArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let signal = parse_signal(&args.signal)?;
    kill_grace_duration(args.grace_ms)?;
    let rpc = rpc_simple(
        &record,
        Operation::Kill {
            signal,
            grace_ms: args.grace_ms,
        },
        None,
    );
    if let Err(error) = rpc {
        let worker_alive = record.worker_alive();
        // A missing socket file, or a leftover socket with no listener
        // (SIGKILL leaves the file; connect then fails with
        // ConnectionRefused), proves an "alive" pid is unreachable. A
        // mere RPC timeout/reset can be transient, so those still return.
        let socket_missing = worker_alive && !record.socket_path.exists();
        let stale_socket = error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|cause| cause.kind() == io::ErrorKind::ConnectionRefused)
        });
        if worker_alive && !socket_missing && !stale_socket {
            return Err(error);
        }
        if socket_missing || stale_socket {
            preflight_broken_containment_recovery(&record)?;
            force_kill_stale_worker(&record)?;
            if record.containment_proven_empty() {
                remove_session_state(paths, record.id)
                    .with_context(|| format!("remove stale session {}", record.id))?;
                eprintln!(
                    "a: removed session {} after stopping unreachable worker pid {}",
                    record.id,
                    record.worker_pid.unwrap_or(0),
                );
                if json_output {
                    println!("{}", json!({"id":record.id,"signal":signal}));
                }
                return Ok(());
            }
            recover_broken_containment(&record, signal, args.grace_ms)?;
            mark_broken_workload_killed(paths, &record)?;
            eprintln!(
                "a: killed session {} (worker pid {} was unreachable; containment cleanup confirmed)",
                record.id,
                record.worker_pid.unwrap_or(0),
            );
            if json_output {
                println!("{}", json!({"id":record.id,"signal":signal}));
            }
            return Ok(());
        }
        if !record.worker_finished() {
            recover_broken_containment(&record, signal, args.grace_ms)?;
            mark_broken_workload_killed(paths, &record)?;
            // That finalization was client-side and deliberately kept the
            // evidence for a broken workload; the worker is already gone,
            // so there is no worker-side removal to wait for below.
            if json_output {
                println!(
                    "{}",
                    json!({"id":record.id,"signal":signal,"record_removed":false})
                );
            }
            return Ok(());
        }
        if !record.containment_proven_empty() {
            recover_broken_containment(&record, signal, args.grace_ms)?;
        }
        remove_session_state(paths, record.id)
            .with_context(|| format!("remove finished session {}", record.id))?;
        eprintln!("a: removed {} session {}", record.phase.name(), record.id);
        if json_output {
            println!(
                "{}",
                json!({"id":record.id,"signal":signal,"record_removed":true})
            );
        }
        return Ok(());
    }
    // The RPC was accepted, so the worker removes the record itself during
    // finalization. Give it a moment so `a kill` returns with the session
    // already gone from `a list` (a client that kills-then-lists must never
    // observe the exited corpse the old behavior left behind), and say so
    // plainly on the two outcomes where the record is still there.
    let removed = wait_for_kill_record_removal(paths, record.id);
    match removed {
        KillRecordOutcome::Removed => {}
        KillRecordOutcome::Kept => eprintln!(
            "a: killed session {}, but its worker kept the record (a finalize failure worth inspecting: `a status {}`)",
            record.id, record.id
        ),
        KillRecordOutcome::Pending => eprintln!(
            "a: killed session {}; its worker is still finalizing, the record disappears on its own unless the worker failed",
            record.id
        ),
    }
    if json_output {
        println!(
            "{}",
            json!({
                "id": record.id,
                "signal": signal,
                "record_removed": removed == KillRecordOutcome::Removed,
            })
        );
    }
    Ok(())
}

fn preflight_broken_containment_recovery(record: &SessionRecord) -> Result<()> {
    if record.containment_proven_empty() {
        return Ok(());
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        bail!(
            "session {} has no authoritative containment locator; refusing to stop its worker or remove runtime evidence",
            record.id
        );
    };
    validate_recorded_cgroup_locator(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
    )
    .context("validate recorded cgroup before stopping unreachable worker")
}

/// Record that the client killed an orphaned workload after its worker died.
fn mark_broken_workload_killed(paths: &Paths, record: &SessionRecord) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let mut current = read_record(&paths.record(record.id)).unwrap_or_else(|_| record.clone());
    current.phase = Phase::Failed;
    current.containment_empty = Some(true);
    current.error =
        Some("worker died without recording workload exit; workload killed by `a kill`".into());
    current.updated_at_ms = now_ms();
    atomic_write_json(&paths.record(record.id), &current)?;
    let _ = fs::remove_dir_all(paths.runtime_session(record.id));
    Ok(())
}

/// Recover a session whose worker can no longer perform containment cleanup.
/// A leader PID or process group is intentionally insufficient: a workload
/// may daemonize through `setsid`, and after the subreaper worker dies there
/// is no complete process-tree root left to inspect. Resource-limited
/// sessions retain an authoritative cgroup locator; every other broken
/// session is preserved for manual investigation rather than reporting a
/// false cleanup success.
///
/// That preservation is `a kill`'s rule and is unchanged. It is NOT a
/// promise that the record survives forever: once the workload leader is
/// also gone, `a prune` reaps such a record on the grounds that no
/// programmatic handle to a survivor remains (see
/// `aplexer::containment_reap_verdict`, and
/// `tests/prune_dead_records.rs::prune_reaps_a_record_whose_setsid_descendant_escaped`,
/// which pins the case where an escaped `setsid` descendant outlives the
/// reap). `a kill` never does that: it still refuses, and still preserves
/// both directories, because unlike prune it would be claiming a cleanup.
fn recover_broken_containment(record: &SessionRecord, signal: i32, grace_ms: u64) -> Result<()> {
    let grace = kill_grace_duration(grace_ms)?;
    if record.containment_proven_empty() {
        return Ok(());
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        bail!(
            "session {} has no authoritative containment locator; refusing to claim cleanup or remove its runtime evidence",
            record.id
        );
    };
    cleanup_recorded_cgroup(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
        signal,
        grace,
    )
    .context("recover recorded cgroup containment")
}

fn cmd_rename(paths: &Paths, args: RenameArgs, json_output: bool) -> Result<()> {
    let old = resolve_record(paths, Some(&args.selector), None, None)?;
    let workspace = canonical_workspace(args.workspace.as_deref().unwrap_or(&old.workspace))?;
    let tag = args.tag.unwrap_or_else(|| old.tag.clone());
    validate_tag(&tag)?;
    let result = rpc_simple(&old, Operation::Rename { workspace, tag }, None)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let record: SessionRecord = serde_json::from_value(result)?;
        println!("{}", record.selector());
    }
    Ok(())
}

fn cmd_engines(paths: &Paths, json_output: bool) -> Result<()> {
    let values = aplexer::api::engines_json(paths)?;
    let values = values.as_array().cloned().unwrap_or_default();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        for v in values {
            println!(
                "{:<16} {:<9} {}",
                v["name"].as_str().unwrap(),
                if v["available"].as_bool().unwrap() {
                    "available"
                } else {
                    "missing"
                },
                v["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| shell_quote(x.as_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
    Ok(())
}

fn cmd_profiles(paths: &Paths, json_output: bool) -> Result<()> {
    let profiles = aplexer::api::profiles_json(paths)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&profiles)?);
    } else if profiles.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        println!("no configured profiles");
    } else {
        let config_profiles: BTreeMap<String, aplexer::ProfileConfig> =
            serde_json::from_value(profiles)?;
        for (name, p) in config_profiles {
            println!(
                "{:<20} engine={}",
                name,
                p.engine.as_deref().unwrap_or("(default)")
            );
        }
    }
    Ok(())
}

/// Resolution result shared by `a launch-spec` and `a launch-exec` -- both
/// wrap the exact same `Config::resolve` that `a start` uses
/// (pocketshell-integration-plan.md 0.3/0.4); they differ only in what they
/// do with it (print JSON vs execvpe). Neither creates a session or spawns
/// a worker -- pure resolution/preview.
struct LaunchPreview {
    engine: String,
    profile: Option<String>,
    argv: Vec<String>,
    env_set: BTreeMap<String, String>,
    env_unset: Vec<String>,
    cwd: PathBuf,
}

fn build_launch_preview(paths: &Paths, args: &LaunchArgs) -> Result<LaunchPreview> {
    let config = Config::load(paths)?;
    // launch-spec/launch-exec intentionally have no --workspace flag (only
    // --cwd, matching the plan doc's exact flag list) -- the process's own
    // current directory is only a fallback for Config::resolve's cwd
    // default when neither --cwd nor a selected profile supplies one; a
    // future pocketshell shim always passes --cwd explicitly (its --dir).
    let workspace = canonical_workspace(Path::new("."))?;
    let launch = config.resolve(
        Vec::new(),
        args.engine.as_deref(),
        args.profile.as_deref(),
        &workspace,
        args.cwd.as_deref(),
        &BTreeMap::new(),
        &Limits::default(),
        None,
    )?;
    // The DEFAULT includes the engine's skip-permissions argv appended;
    // --no-skip-permissions opts OUT (matches pocketshell's own
    // `--skip-permissions/--no-skip-permissions` default=True). `a start`
    // never does this -- unlike env_unset, skip-permissions argv is a
    // launch-spec/launch-exec-only behavior, not forced onto every session.
    let mut argv = launch.command.clone();
    if !args.no_skip_permissions {
        argv.extend(launch.skip_permissions_argv.clone());
    }
    let cwd = canonical_workspace(&launch.cwd).unwrap_or(launch.cwd);
    Ok(LaunchPreview {
        engine: launch.engine,
        profile: launch.profile,
        argv,
        env_set: launch.env,
        env_unset: launch.env_unset,
        cwd,
    })
}

/// `a launch-spec [--engine E] [--profile P] [--no-skip-permissions]
/// [--cwd D] --json` (pocketshell-integration-plan.md 0.3) -- prints the
/// resolved `{engine, profile, argv, env_set, env_unset, cwd}` without
/// creating a session or spawning anything.
fn cmd_launch_spec(paths: &Paths, args: LaunchArgs, json_output: bool) -> Result<()> {
    let preview = build_launch_preview(paths, &args)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "engine": preview.engine,
                "profile": preview.profile,
                "argv": preview.argv,
                "env_set": preview.env_set,
                "env_unset": preview.env_unset,
                "cwd": preview.cwd,
            }))?
        );
    } else {
        println!("engine: {}", preview.engine);
        if let Some(p) = &preview.profile {
            println!("profile: {p}");
        }
        println!("cwd: {}", preview.cwd.display());
        println!(
            "argv: {}",
            preview
                .argv
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for (k, v) in &preview.env_set {
            println!("env set:   {k}={v}");
        }
        println!(
            "env unset: {} vars ({})",
            preview.env_unset.len(),
            preview.env_unset.join(" ")
        );
    }
    Ok(())
}

/// `a launch-exec [same flags as launch-spec]`
/// (pocketshell-integration-plan.md 0.4) -- the `execvpe` variant of
/// `launch-spec`: same resolution, but replaces this process with the
/// resolved command instead of printing it. The resolved `env_unset` is
/// applied (via `env_remove`) AFTER `env_set`, so the provider-key strip
/// always wins even over an explicitly-set value -- same ordering worker.rs's
/// spawn_workload uses. Drop-in exec-step target for a future pocketshell
/// `agents.py::launch_agent` shim.
fn cmd_launch_exec(paths: &Paths, args: LaunchArgs) -> Result<()> {
    let preview = build_launch_preview(paths, &args)?;
    let program = preview
        .argv
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("resolved launch has an empty argv"))?;
    let mut command = Command::new(&program);
    command
        .args(&preview.argv[1..])
        .current_dir(&preview.cwd)
        .envs(&preview.env_set);
    for name in &preview.env_unset {
        command.env_remove(name);
    }
    // CommandExt::exec() only returns on failure (it replaces this process
    // on success), so reaching this line is always an error.
    let error = command.exec();
    Err(error).with_context(|| format!("exec {program}"))
}

/// `a whoami` -- lets an agent or script running INSIDE a session (or a
/// human at its prompt) ask "am I in an aplexer session, and if so which
/// one" without hand-parsing environment variables. Every workload already
/// has APLEXER_SESSION_ID/WORKSPACE/TAG injected (see spawn_workload in
/// worker.rs) -- this just resolves the id against the session's persisted
/// record for the fuller picture (engine, profile, phase) and gives a
/// stable, scriptable "nothing/non-zero if not inside one" contract, the
/// same shape `$TMUX` serves for tmux but structured instead of a bare path.
fn cmd_whoami(paths: &Paths, json_output: bool) -> Result<()> {
    let Some(id) = discover_session_id() else {
        // Deliberately silent on stdout either way -- a script doing
        // `id=$(a whoami --json)` should see empty output and rely on the
        // exit code, not have to filter out a "not in a session" sentence.
        if !json_output {
            eprintln!("not inside an aplexer session");
        }
        std::process::exit(1);
    };
    let record = read_record(&paths.record(id)).with_context(|| {
        format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
    })?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&public_session_record(&record))?
        );
    } else {
        println!("id: {}", record.id);
        println!("selector: {}", record.selector());
        println!("engine: {}", record.engine);
        if let Some(profile) = &record.profile {
            println!("profile: {profile}");
        }
        // On a terminal, lead with the honest semantic state and show the
        // underlying lifecycle when it differs; redirected output keeps the
        // raw phase word the pre-UX build printed.
        if io::stdout().is_terminal() {
            let (state, source) = session_ui_state(&record, now_ms());
            println!("state: {state}{}", state_source_suffix(source));
            let lifecycle =
                derived_liveness(&record.phase, record.worker_alive(), record.created_at_ms);
            if lifecycle != state {
                println!("lifecycle: {lifecycle}");
            }
        } else {
            println!("state: {}", record.phase.name());
        }
    }
    Ok(())
}

/// `a state-report <idle|waiting|working>`
/// (docs/pocketshell-integration-plan.md Open question #2, "Agent-state
/// ingestion"): lets a hook running INSIDE a session push its own semantic
/// state -- the missing half of `a watch --jsonl`'s `agent.state` event,
/// which otherwise only has a coarse PTY-recency heuristic to go on (see
/// watch.rs's `fresh_reported_state`/`derive_agent_state_with_source` for
/// exactly how a push is merged with that heuristic and for how long it
/// stays authoritative).
///
/// Resolves its target exactly like `a whoami` -- via the injected
/// `APLEXER_SESSION_ID`, never a selector -- because a hook script has no
/// notion of "which session" other than the one it is running inside; see
/// `cmd_whoami`'s doc comment for the shared mechanism (`discover_session_id`,
/// the same env var `worker.rs::spawn_workload` injects into every
/// session). Same exit-code contract as `a whoami`: a plain `exit(1)` with
/// one stderr line when `APLEXER_SESSION_ID` is unset (so a hook wired as
/// `a state-report waiting || true` degrades silently outside aplexer);
/// any other failure (record missing, worker dead/unreachable, invalid
/// state rejected by the worker) propagates through `?` to `main`'s
/// generic `a: {error}` / exit(1) handler, same as every other subcommand.
///
/// What this repo does NOT do here (deliberately): install the hooks that
/// call this command. That wiring lives in `a init` (`aplexer::hooks`),
/// which merges a `state-report` hook into every configured engine
/// (Claude Stop/Notification, Codex hooks/notify, OpenCode plugin, Grok
/// and Gemini hooks) — this command is the ingestion primitive it builds
/// on. `a init --check --json` is the machine-readable way to verify the
/// wiring is present.
fn cmd_state_report(paths: &Paths, state: ReportedState) -> Result<()> {
    let Some(id) = discover_session_id() else {
        eprintln!("a state-report: not inside an aplexer session (APLEXER_SESSION_ID not set)");
        std::process::exit(1);
    };
    let record = read_record(&paths.record(id)).with_context(|| {
        format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
    })?;
    rpc_simple(
        &record,
        Operation::ReportState {
            state: state.as_str().to_string(),
        },
        None,
    )?;
    Ok(())
}

/// `a init [--check] [--uninstall] [--engine NAME]`
///
/// Machine-wide agent-state hook installation: merges an `a state-report`
/// hook into every agent engine aplexer knows how to launch (claude, codex
/// — which also covers the zcodex variant via shared `CODEX_HOME` — grok,
/// gemini, opencode), including each configured profile's config dir, so a
/// session reports `working`/`waiting`/`idle` instead of leaving every
/// consumer to guess from PTY-output recency. See `aplexer::hooks` for the
/// per-engine mechanisms and the merge-never-clobber rules.
///
/// Modes (exactly one):
///
/// - default: install (idempotent; only writes files that change).
/// - `--check`: touch nothing; print per-engine status and exit 0 when
///   fully initialized, 1 otherwise. With `--json` this prints
///   `{"initialized": bool, "engines": [...]}` — the machine contract the
///   PocketShell host CLI automates against (run `a init --check --json`;
///   when it reports `initialized: false`, run `a init`).
/// - `--uninstall`: remove our hooks again.
///
/// `--engine` limits any mode to one engine (`zcodex` maps onto `codex`).
fn cmd_init(paths: &Paths, args: InitArgs, json_output: bool) -> Result<()> {
    if args.check && args.uninstall {
        bail!("`a init --check` and `a init --uninstall` cannot be combined");
    }
    let filter = args
        .engine
        .as_deref()
        .map(aplexer::hooks::normalize_engine_filter)
        .transpose()?;
    // Profile config dirs (CLAUDE_CONFIG_DIR / CODEX_HOME) extend the
    // install targets past the default homes, so a profile session reports
    // state just like a default one. A broken user config fails here the
    // same way it fails every other command.
    let config = Config::load(paths)?;
    let profile_envs: Vec<BTreeMap<String, String>> = config
        .profiles
        .values()
        .map(|profile| profile.env.clone())
        .collect();
    let targets = aplexer::hooks::resolve_targets_from_env(&profile_envs)?;
    let a_bin = aplexer::hooks::resolve_a_bin();

    if args.check {
        let statuses = aplexer::hooks::check(&targets, filter);
        let initialized = statuses.iter().all(|status| status.installed);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "initialized": initialized,
                    "engines": statuses,
                }))?
            );
        } else {
            for status in &statuses {
                println!(
                    "{} {:<9} {}",
                    if status.installed { "OK  " } else { "MISS" },
                    status.engine,
                    status.message
                );
            }
            if initialized {
                println!("hooks initialized for all engines");
            } else {
                println!("hooks missing for some engines; run `a init` to install");
            }
        }
        if !initialized {
            bail!("agent-state hooks are not fully installed");
        }
        return Ok(());
    }

    if args.uninstall {
        let statuses = aplexer::hooks::uninstall(&targets, filter);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "engines": statuses,
                }))?
            );
        } else {
            for status in &statuses {
                println!("{}: {} — {}", status.engine, status.action, status.message);
            }
        }
        return Ok(());
    }

    let statuses = aplexer::hooks::install(&targets, &a_bin, filter);
    let ok = statuses.iter().all(|status| status.action != "error");
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": ok,
                "engines": statuses,
            }))?
        );
    } else {
        for status in &statuses {
            println!("{}: {} — {}", status.engine, status.action, status.message);
        }
    }
    if !ok {
        bail!("agent-state hook installation hit errors");
    }
    Ok(())
}

#[derive(Debug)]
struct CgroupLimitProbe {
    cgroup_v2: bool,
    controllers: Vec<String>,
    delegated_scope: bool,
    detail: String,
}

fn probe_cgroup_limits() -> CgroupLimitProbe {
    if let Err(error) = current_cgroup_identity() {
        return CgroupLimitProbe {
            cgroup_v2: false,
            controllers: Vec::new(),
            delegated_scope: false,
            detail: format!("cgroup v2 unavailable: {error:#}"),
        };
    }

    let controllers_path = Path::new("/sys/fs/cgroup/cgroup.controllers");
    let controllers: Vec<String> = match fs::read_to_string(controllers_path) {
        Ok(value) => value.split_whitespace().map(str::to_string).collect(),
        Err(error) => {
            return CgroupLimitProbe {
                cgroup_v2: true,
                controllers: Vec::new(),
                delegated_scope: false,
                detail: format!("cannot read {}: {error}", controllers_path.display()),
            };
        }
    };
    let required_controllers = ["cpu", "memory", "pids"];
    let missing: Vec<&str> = required_controllers
        .into_iter()
        .filter(|required| !controllers.iter().any(|found| found == required))
        .collect();
    if !missing.is_empty() {
        return CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: format!(
                "cgroup v2 is mounted but required controller(s) are absent: {}",
                missing.join(", ")
            ),
        };
    }

    // Exercise the exact launch implementation with a short-lived placeholder
    // scope: trusted systemd-run/systemctl/sleep discovery, the systemd --user
    // manager, Delegate=yes, all three supported controllers, and write-open
    // access to cgroup.procs. The scope contains only the probe's `sleep`
    // process and is cleaned immediately; no existing cgroup or workload is
    // modified.
    let probe_limits = Limits {
        memory_bytes: Some(64 * 1024 * 1024),
        pids: Some(16),
        cpu_quota_us: Some(10_000),
        cpu_period_us: Some(100_000),
    };
    let probe_result = Cgroup::create(Uuid::new_v4(), &probe_limits, || {});
    match probe_result {
        Ok(Some(cgroup)) => {
            let validation = (|| -> Result<()> {
                let _procs = cgroup.open_procs()?;
                for controller_file in ["memory.max", "pids.max", "cpu.max"] {
                    let path = cgroup.locator().join(controller_file);
                    if !path.is_file() {
                        bail!("delegated scope is missing {}", path.display());
                    }
                }
                Ok(())
            })();
            cgroup.cleanup();
            match validation {
                Ok(()) => CgroupLimitProbe {
                    cgroup_v2: true,
                    controllers,
                    delegated_scope: true,
                    detail: "verified a temporary delegated systemd --user scope with memory, pids, and cpu controls".into(),
                },
                Err(error) => CgroupLimitProbe {
                    cgroup_v2: true,
                    controllers,
                    delegated_scope: false,
                    detail: format!("delegated scope validation failed: {error:#}"),
                },
            }
        }
        Ok(None) => CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: "limit probe unexpectedly created no cgroup".into(),
        },
        Err(error) => CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: format!("delegated systemd --user scope probe failed: {error:#}"),
        },
    }
}

fn cgroup_limits_check(probe: CgroupLimitProbe) -> Value {
    let required_controllers = ["cpu", "memory", "pids"];
    let controllers_ok = required_controllers
        .iter()
        .all(|required| probe.controllers.iter().any(|found| found == required));
    let available = probe.cgroup_v2 && controllers_ok && probe.delegated_scope;
    let detail = if available {
        probe.detail.clone()
    } else {
        format!(
            "{}; resource limits unavailable, but unlimited sessions still work",
            probe.detail
        )
    };
    json!({
        "name": "cgroup_limits",
        "ok": available,
        "severity": if available { "ok" } else { "warning" },
        "required": false,
        "available": available,
        "detail": detail,
        "prerequisites": {
            "cgroup_v2": probe.cgroup_v2,
            "controllers": {
                "ok": controllers_ok,
                "required": required_controllers,
                "available": probe.controllers,
            },
            "delegated_systemd_user_scope": {
                "ok": probe.delegated_scope,
                "detail": probe.detail,
                "method": "temporary_scope_via_launch_path",
                "verifies": [
                    "trusted_systemd_run_systemctl_sleep",
                    "systemd_user_manager",
                    "delegate_yes",
                    "writable_cgroup_procs",
                ],
            },
        },
    })
}

fn doctor_checks_ok(checks: &[Value]) -> bool {
    checks
        .iter()
        .all(|check| check["ok"].as_bool().unwrap_or(false) || check["severity"] == "warning")
}

/// The `launch_placement` doctor check (issue #1). Two questions in one:
/// (1) which service manager owns the cgroup this process is running in --
/// the placement every `a start` launched from this context hands its
/// worker, since setsid() changes session, not cgroup -- and (2) how many
/// active recorded sessions sit in the per-user manager's exit.target
/// failure domain, from the `worker_cgroup` evidence the worker now records
/// at launch. Warning-severity by design: the issue asks aplexer to warn
/// clearly, and a vulnerable placement has actionable workarounds (launch
/// context, or the opt-in system scope), so it must not fail the host.
fn launch_placement_check(paths: &Paths) -> Value {
    let own_cgroup = aplexer::placement::read_process_cgroup(std::process::id());
    let own_placement = own_cgroup
        .as_deref()
        .map(aplexer::placement::classify_cgroup_path);
    let vulnerable = own_placement
        .map(|placement| placement.vulnerable_to_user_manager_exit())
        .unwrap_or(false);
    let mut vulnerable_sessions: Vec<Value> = Vec::new();
    if let Ok(records) = list_records(paths) {
        for record in records {
            if !record.worker_phase_active() {
                continue;
            }
            let session_vulnerable = record
                .worker_cgroup
                .as_deref()
                .map(|cgroup| {
                    aplexer::placement::classify_cgroup_path(cgroup)
                        .vulnerable_to_user_manager_exit()
                })
                .unwrap_or(false);
            if session_vulnerable {
                vulnerable_sessions.push(json!({
                    "id": record.id.to_string(),
                    "selector": record.selector(),
                    "worker_cgroup": record.worker_cgroup,
                }));
            }
        }
    }
    let placement_name = own_placement.map(|placement| placement.name());
    let advice = own_placement.and_then(|placement| placement.advice());
    let mut detail = format!(
        "aplexer commands launched here run in cgroup {} ({})",
        own_cgroup.as_deref().unwrap_or("<unknown>"),
        placement_name.unwrap_or("unknown"),
    );
    if vulnerable {
        if let Some(advice) = advice {
            detail.push_str(&format!(
                "; sessions started here will die at `systemctl --user exit`; {advice}"
            ));
        }
    } else if let Some(advice) = advice {
        detail.push_str(&format!("; note: {advice}"));
    }
    if !vulnerable_sessions.is_empty() {
        detail.push_str(&format!(
            "; {} active session(s) recorded inside the per-user manager failure domain",
            vulnerable_sessions.len()
        ));
    }
    json!({
        "name": "launch_placement",
        "ok": !vulnerable,
        "severity": if vulnerable { "warning" } else { "ok" },
        "required": false,
        "detail": detail,
        "own_cgroup": own_cgroup,
        "own_placement": placement_name,
        "vulnerable_to_user_manager_exit": vulnerable,
        "vulnerable_sessions": vulnerable_sessions,
        "advice": advice,
        // Doctor only reads /proc and session records; it never probes the
        // system-scope backend (that would create a transient scope just by
        // asking for a checkup). The escape is documented here, and its
        // availability is proven at the opted-in `a start` that uses it.
        "escape": {
            "env": aplexer::placement::LAUNCH_SYSTEM_SCOPE_ENV,
            "value": aplexer::placement::LAUNCH_SYSTEM_SCOPE_VALUE,
            "requested": aplexer::placement::system_scope_requested(),
        },
    })
}

fn cmd_doctor(paths: &Paths, json_output: bool) -> Result<()> {
    let mut checks = Vec::<Value>::new();
    checks.push(json!({"name":"linux","ok":true,"detail":std::env::consts::OS}));
    checks.push(path_check("runtime_root", &paths.runtime_root));
    checks.push(path_check("state_root", &paths.state_root));
    let sample = paths.socket(Uuid::nil());
    checks.push(json!({"name":"unix_socket_path","ok":sample.as_os_str().len()<108,"detail":sample.display().to_string()}));
    checks.push(cgroup_limits_check(probe_cgroup_limits()));
    checks.push(launch_placement_check(paths));
    match Config::load(paths){Ok(config)=>checks.push(json!({"name":"config","ok":true,"detail":format!("{} engines, {} profiles",config.engines.len(),config.profiles.len())})),Err(e)=>checks.push(json!({"name":"config","ok":false,"detail":format!("{e:#}")}))}
    match list_records(paths) {
        Ok(records) => {
            let record_count = records.len();
            let mut reapable_count = 0usize;
            let broken: Vec<Value> = records
                .into_iter()
                .filter_map(|record| {
                    if !record.worker_phase_active() {
                        return None;
                    }
                    let worker_alive = record.worker_alive();
                    let rpc_error = rpc_simple(&record, Operation::Status, None)
                        .err()
                        .map(|error| format!("{error:#}"));
                    let worker_reachable = rpc_error.is_none();
                    if worker_alive && worker_reachable {
                        return None;
                    }
                    let state = derived_liveness(&record.phase, worker_alive, record.created_at_ms);
                    // A `Starting` record inside the startup window has no
                    // worker pid yet and no socket to answer an RPC: that is
                    // `a start` in flight, not wreckage. Reporting it here
                    // sent the user at `a prune` / `a kill` for a session
                    // that was about to come up on its own (issue #9).
                    if state == "starting" {
                        return None;
                    }
                    // Recovery advice has to follow the same predicate prune
                    // actually uses, or doctor sends the user at a command
                    // that hard-fails. `a kill` on a broken unlimited record
                    // exits 1 with "no authoritative containment locator",
                    // and `a forget --force`'s "workload processes may
                    // survive" warning is not what this needs -- for a
                    // record prune can reap, `a prune` is the whole answer.
                    let recovery = if reap_verdict(&record).is_some() {
                        reapable_count += 1;
                        json!({ "prune": "a prune" })
                    } else {
                        json!({
                            "kill": format!("a kill {}", record.id),
                            "forget": format!("a forget {} --force", record.id),
                        })
                    };
                    Some(json!({
                        "id": record.id,
                        "selector": record.selector(),
                        "phase": record.phase.name(),
                        "state": state,
                        "worker_alive": worker_alive,
                        "worker_reachable": worker_reachable,
                        "rpc_error": rpc_error,
                        "recovery": recovery,
                    }))
                })
                .collect();
            let detail = if broken.is_empty() {
                format!("{record_count} session record(s), none broken")
            } else if reapable_count == broken.len() {
                format!(
                    "{} broken/stale session(s), all reapable; run `a prune`",
                    broken.len()
                )
            } else if reapable_count == 0 {
                format!(
                    "{} broken/stale session(s); run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
                    broken.len()
                )
            } else {
                format!(
                    "{} broken/stale session(s); `a prune` removes {reapable_count} of them, for the rest run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
                    broken.len()
                )
            };
            checks.push(json!({
                "name": "sessions",
                "ok": broken.is_empty(),
                "detail": detail,
                "broken_sessions": broken,
            }));
        }
        Err(error) => checks.push(json!({
            "name": "sessions",
            "ok": false,
            "detail": format!("cannot inspect session records: {error:#}"),
            "broken_sessions": [],
        })),
    }
    let warnings = checks
        .iter()
        .filter(|check| check["severity"] == "warning")
        .count();
    let ok = doctor_checks_ok(&checks);
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok":ok,"warnings":warnings,"checks":checks}))?
        );
    } else {
        for check in &checks {
            let label = if check["severity"] == "warning" {
                "WARN"
            } else if check["ok"].as_bool().unwrap_or(false) {
                "OK"
            } else {
                "FAIL"
            };
            println!(
                "{:<5} {:<20} {}",
                label,
                check["name"].as_str().unwrap(),
                check["detail"].as_str().unwrap_or("")
            );
        }
    }
    if !ok {
        bail!("one or more doctor checks failed");
    }
    Ok(())
}

/// `a completions <shell>` -- writes the clap_complete-generated script for
/// the given shell to stdout, completing for the `a` binary name itself
/// (from `#[command(name = "a")]` on `Cli` above, not the `aplexer` package
/// name), so callers just redirect it into whatever path their shell's
/// completion loader scans.
fn cmd_completions(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(args.shell, &mut cmd, name, &mut io::stdout());
    Ok(())
}

/// `a hotkeys` -- a lookup command for the attach-mode Ctrl-b chords,
/// rendered from `ATTACH_BINDINGS`, the same table the attach status bar's
/// `?` flash (`attach_key_help`) renders. There is one authoritative keymap
/// and one place it is written down; this just prints it somewhere you can
/// look it up without already being attached.
fn cmd_hotkeys() -> Result<()> {
    println!("Attach-mode keys (press Ctrl-b, then one of these):");
    println!();
    let width = ATTACH_BINDINGS
        .iter()
        .map(|b| b.keys.len())
        .max()
        .unwrap_or(0);
    for binding in ATTACH_BINDINGS {
        println!(
            "  {:width$}  {}",
            binding.keys,
            binding.description,
            width = width
        );
    }
    println!();
    println!("Hold Ctrl-b without pressing anything and this list appears on screen;");
    println!("the next key runs its binding and takes it away again.");
    println!();
    println!("Any other key after Ctrl-b is forwarded through untouched.");
    println!();
    println!("Scrolling back (aplexer's copy-mode, like tmux's Ctrl-b [):");
    println!();
    println!("  the mouse wheel enters it on its own, with no prefix -- unless the");
    println!("  workload has asked the terminal for the mouse itself, in which case");
    println!("  the wheel belongs to the workload and Ctrl-b [ is the way in.");
    println!();
    println!("  PgUp/PgDn  a screen at a time      Up/Down, k/j   a line at a time");
    println!("  Home / End top / back to live      g / G          the same");
    println!("  Space / b  a screen at a time      u / d          half a screen");
    println!("  q, Esc     back to the live screen");
    println!();
    println!("  While scrolling, keys go to the pager and never to the session --");
    println!("  press i to hand the keyboard to the session anyway (Esc pages again).");
    println!(
        "  History is {} lines by default (APLEXER_HISTORY_LIMIT);",
        aplexer::screen::DEFAULT_SCROLLBACK_LINES
    );
    println!("  APLEXER_MOUSE=off leaves the mouse to the terminal for selection.");
    Ok(())
}

/// `a watch --jsonl [--all] [--workspace PATH]` -- see src/watch.rs for the
/// poll/diff loop and the heru UnifiedEvent mapping it emits.
fn cmd_watch(paths: &Paths, args: WatchArgs) -> Result<()> {
    if !args.jsonl {
        bail!("a watch currently requires --jsonl (no other output format is implemented yet)");
    }
    let workspace = args
        .workspace
        .as_deref()
        .map(canonical_workspace)
        .transpose()?;
    aplexer::watch::run(paths, args.all, workspace.as_deref())
}

/// `a transcript [SESSION] [--last N] [--after SEQ] [--before SEQ]
/// [--kind K] [--follow] [--json]` -- parse the native conversation log of
/// an aplexer session (the JSONL the engine CLI already writes) into heru
/// UnifiedEvent JSONL. PocketShell's conversation pane is the consumer:
/// last-N for the initial view, `--before` for older pages, `--after` plus
/// `--follow` for live tail. See src/agent_events.rs for capture/bind.
///
/// With no SESSION, `--workspace`, or `--tag`, falls back to
/// `$APLEXER_SESSION_ID` (`a whoami`) so an agent or hook inside a session
/// can dump its own log without addressing itself.
fn cmd_transcript(paths: &Paths, args: TranscriptArgs, json_output: bool) -> Result<()> {
    let record = resolve_transcript_target(paths, &args)?;
    let bind_path = paths.state_session(record.id).join("transcript.json");
    let located = aplexer::agent_events::resolve_transcript(&record, &bind_path)?;
    let path = located.path;
    if !json_output && !args.follow {
        println!("transcript: {} (engine {})", path.display(), record.engine);
    }
    aplexer::agent_events::run_transcript(
        &record,
        &path,
        aplexer::agent_events::TranscriptQuery {
            last: args.last,
            kind: args.kind.clone(),
            after: args.after,
            before: args.before,
            follow: args.follow,
            max_line_bytes: args.max_line_bytes,
        },
        json_output,
    )
}

/// Prefer an explicit selector; otherwise the session this process is
/// running inside (`APLEXER_SESSION_ID` from worker spawn / `a whoami`).
fn resolve_transcript_target(paths: &Paths, args: &TranscriptArgs) -> Result<SessionRecord> {
    let targeted = args.target.selector.is_some()
        || args.target.workspace.is_some()
        || args.target.tag.is_some();
    if !targeted {
        if let Some(id) = discover_session_id() {
            return read_record(&paths.record(id)).with_context(|| {
                format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
            });
        }
    }
    resolve(paths, &args.target)
}

fn path_check(name: &str, path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(meta) => json!({"name":name,"ok":meta.is_dir(),"detail":path.display().to_string()}),
        Err(e) => json!({"name":name,"ok":false,"detail":format!("{}: {e}",path.display())}),
    }
}
/// Attach/send/capture have no sensible action against a session with no
/// live worker other than saying so plainly -- left to `connect()`, a
/// terminal-phase session (worker gone, socket removed on its way out) or a
/// broken one (worker dead, socket simply not listening) both surface as a
/// bare `UnixStream::connect` OS error, e.g. "No such file or directory",
/// which reads like a bug rather than "this session is done". `a kill` is
/// deliberately exempt: for a terminal-phase session it now has a real
/// action to take (removing the state, see cmd_kill), and it already
/// handles the broken case itself via `recover_broken_containment`.
///
/// A third, rarer case: `phase` is non-terminal and `worker_pid` is alive,
/// but `socket_path` doesn't exist on disk. This happens when the worker's
/// runtime directory (which holds `control.sock`) got removed out from under
/// it. The worker process is technically still running, but it's
/// unreachable, so treating it as attachable would just trade the clear
/// checks above for the same bare `UnixStream::connect` OS error this
/// function exists to avoid. `a kill` again has a real action to take here
/// (see cmd_kill's socket-missing force-clean path), so it's not exempted
/// from this check the way the other two cases exempt it -- `a kill` relies
/// on `rpc_simple` failing and inspects the socket itself rather than going
/// through `check_attachable`.
///
/// That third case used to swallow a fourth that is not a fault at all: the
/// worker binds `control.sock` some milliseconds after it registers its pid,
/// so a client racing a healthy `a start` saw the same missing socket and
/// was told the runtime directory had been destroyed and to run `a kill` --
/// on a session that was about to come up. Both that and the missing-pid
/// window before it are now answered by `state == "starting"` (issue #9),
/// which is bounded by `DEFAULT_STARTUP_TIMEOUT_MS`: past the startup
/// budget the record really is a crashed start and the advice above applies
/// again.
fn check_attachable(record: &SessionRecord) -> Result<()> {
    if matches!(record.phase, Phase::Exited | Phase::Failed) {
        bail!(
            "session {} has already exited (see `a status {}` for details); run `a kill {}` to remove it",
            record.id,
            record.id,
            record.id
        );
    }
    let worker_alive = record.worker_alive();
    let state = derived_liveness(&record.phase, worker_alive, record.created_at_ms);
    let socket_missing = !record.socket_path.exists();
    // A session still inside its startup budget is coming up, not wreckage.
    // The worker writes the record, then its pid, then binds the socket, and
    // only then sets `phase: running` -- so `Starting` plus a missing pid or
    // a missing socket is exactly `a start` in flight. Both bails below used
    // to send the user at `a kill` for a perfectly healthy start that had
    // simply been raced (issue #9); the socket bail's own doc comment names
    // that race and then advised killing it anyway. Past the budget the
    // record is a crashed start and the original advice is right again.
    //
    // The liveness conjunct matters: `Starting` is still `Starting` after
    // the worker is up and listening, and that session is perfectly
    // attachable -- only a missing pid or a missing socket is a reason to
    // refuse at all.
    let still_starting = within_startup_window(&record.phase, record.created_at_ms, now_ms());
    if still_starting && (!worker_alive || socket_missing) {
        bail!(
            "session {} is still starting (its worker has not finished coming up); \
             retry in a moment, or run `a status {}` if it never does",
            record.id,
            record.id
        );
    }
    if !worker_alive {
        bail!(
            "session {}'s worker is not running (state: {}); run `a status` for details, `a kill` to reclaim it",
            record.id,
            state
        );
    }
    if socket_missing {
        bail!(
            "session {} looks alive (worker pid {} running) but its control socket is gone \
             ({}); this usually means the worker's runtime directory was removed out from \
             under it -- run `a kill {}` to force-clean the record, or investigate why that \
             directory disappeared",
            record.id,
            record.worker_pid.unwrap_or(0),
            record.socket_path.display(),
            record.id
        );
    }
    Ok(())
}

// -- Inter-agent messaging (docs/inter-agent-messaging-design.md) --

/// Workspace for `send`/`reply`/`inbox`/`ack`/`show`, which take no
/// `--workspace` flag (design doc section 7): `$APLEXER_WORKSPACE`, else
/// cwd. `log`/`gc` accept an explicit override, passed as `explicit`.
fn resolve_message_workspace(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return canonical_workspace(p);
    }
    if let Ok(v) = env::var("APLEXER_WORKSPACE") {
        if !v.is_empty() {
            return canonical_workspace(Path::new(&v));
        }
    }
    canonical_workspace(Path::new("."))
}

/// Resolves the `--to`/`--all`/`--to-engine` triple into a `Recipient`,
/// applying the typo guard of design doc section 2.3: a tag that has never
/// existed in this workspace is rejected with the list of known tags unless
/// `--queue` is passed. Broadcast/engine forms always succeed.
fn build_recipient(
    paths: &Paths,
    workspace: &Path,
    to: Option<&str>,
    all: bool,
    to_engine: Option<&str>,
    queue: bool,
) -> Result<Recipient> {
    let chosen = [to.is_some(), all, to_engine.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if chosen == 0 {
        bail!("specify exactly one of --to TAG, --all, or --to-engine ENGINE");
    }
    if chosen > 1 {
        bail!("--to, --all, and --to-engine are mutually exclusive");
    }
    if let Some(tag) = to {
        let existing = list_records(paths)?
            .into_iter()
            .find(|r| r.workspace == workspace && r.tag == tag);
        if existing.is_none() && !queue {
            let known = known_tags(paths, workspace);
            let hint = if known.is_empty() {
                "no session has ever run in this workspace".to_string()
            } else {
                format!("known tags: {}", known.join(", "))
            };
            bail!(
                "no session tagged {tag:?} has ever existed in this workspace ({hint}); pass \
                 --queue to park a message for a session that will be created later"
            );
        }
        return Ok(Recipient::Tag {
            tag: tag.to_string(),
            session_id: existing.map(|r| r.id),
        });
    }
    if all {
        return Ok(Recipient::Broadcast { broadcast: true });
    }
    Ok(Recipient::Engine {
        engine: to_engine.unwrap().to_string(),
    })
}

/// Pane delivery (design doc section 6.2): reuses `a send`'s own PTY-write
/// RPC path (`Operation::Send`, `rpc_send` below) -- the client resolves the
/// target session and connects to its worker socket directly, exactly like
/// `a send <target> <text>` does today. No new server-side RPC operation.
fn deliver_pane(
    paths: &Paths,
    workspace: &Path,
    tag: &str,
    from_tag: Option<&str>,
    body: &str,
    raw: bool,
    no_enter: bool,
) -> Result<()> {
    if body.len() > MAX_BODY_BYTES {
        bail!("message body exceeds the {MAX_BODY_BYTES}-byte cap");
    }
    let record = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == tag)
        .ok_or_else(|| anyhow!("no session tagged {tag:?} in this workspace"))?;
    let alive = record.worker_alive();
    if !alive {
        bail!("session {tag:?} is not running; pane delivery requires a live target");
    }
    rpc_send(&record, &pane_input_bytes(body, from_tag, raw, no_enter))
        .with_context(|| format!("inject into session {tag:?}'s PTY"))
}

/// The bytes `--pane` delivery injects: the message, framed with its sender
/// unless `raw`, and -- by default, the tmuxctl behavior -- a trailing
/// return, so a message typed into an agent's prompt actually submits
/// instead of sitting there unconfirmed. `--no-enter` drops the return for
/// the rare target that should compose rather than submit.
fn pane_input_bytes(body: &str, from_tag: Option<&str>, raw: bool, no_enter: bool) -> Vec<u8> {
    let mut out = if raw {
        body.as_bytes().to_vec()
    } else {
        let sender = from_tag.unwrap_or("external");
        format!("[aplexer message from {sender}] {body}").into_bytes()
    };
    if !no_enter {
        out.push(b'\r');
    }
    out
}

fn parse_data_arg(raw: Option<&str>) -> Result<Option<Value>> {
    raw.map(|s| serde_json::from_str::<Value>(s).context("--data must be valid JSON"))
        .transpose()
}

/// Shared send/reply tail: attempts `--pane` delivery if requested (falling
/// back to inbox on failure iff `--or-inbox`), then always writes the
/// message to the durable mailbox -- pane-delivered messages are recorded
/// too (with `delivery: pane`, pre-acked for the recipient) so the mailbox
/// stays a complete account of inter-agent traffic (design doc section 6.2).
fn finish_send(
    paths: &Paths,
    workspace: &Path,
    mut envelope: MessageEnvelope,
    pane: &PaneDeliveryArgs,
) -> Result<MessageEnvelope> {
    if pane.pane {
        let Recipient::Tag { tag, .. } = &envelope.to else {
            bail!("--pane requires a single --to TAG target: no pane broadcast");
        };
        let tag = tag.clone();
        match deliver_pane(
            paths,
            workspace,
            &tag,
            envelope.from.tag.as_deref(),
            &envelope.body,
            pane.raw,
            pane.no_enter,
        ) {
            Ok(()) => envelope.delivery = Delivery::Pane,
            Err(e) => {
                if pane.or_inbox {
                    eprintln!("a: pane delivery failed ({e:#}); falling back to inbox");
                } else {
                    return Err(e);
                }
            }
        }
    }
    let recorded = match write_message(paths, &envelope) {
        Ok(()) => true,
        Err(error) if envelope.delivery == Delivery::Pane => {
            // PTY injection is the pane-delivery commit point. Reporting
            // total failure here invites a retry that injects the same
            // message twice; the missing mailbox copy is only a warning.
            eprintln!(
                "a: pane delivery succeeded, but recording it in the mailbox failed: {error:#}"
            );
            false
        }
        Err(error) => return Err(error),
    };
    if recorded && envelope.delivery == Delivery::Pane {
        if let Recipient::Tag {
            session_id: Some(sid),
            ..
        } = &envelope.to
        {
            // Best-effort: a pane message is already delivered by
            // definition, so a failure to also pre-ack it here is a
            // cosmetic mailbox-record issue, not a delivery failure
            // (design doc section 6.2).
            let _ = ack_messages(paths, workspace, *sid, &[envelope.id]);
        }
    }
    if recorded {
        let _ = maybe_gc(paths, workspace);
    }
    Ok(envelope)
}

fn print_message_line(m: &MessageEnvelope) {
    let sender = m.from.tag.clone().unwrap_or_else(|| {
        if m.from.external {
            "external".into()
        } else {
            "?".into()
        }
    });
    let to_desc = match &m.to {
        Recipient::Tag { tag, .. } => format!("to:{tag}"),
        Recipient::Broadcast { .. } => "to:*".to_string(),
        Recipient::Engine { engine } => format!("to:engine:{engine}"),
    };
    let delivery = match m.delivery {
        Delivery::Inbox => "",
        Delivery::Pane => " [pane]",
    };
    let first_line = m.body.lines().next().unwrap_or("");
    println!(
        "{}  [{}] {sender} -> {to_desc}{delivery}  {first_line}",
        m.id, m.kind
    );
}

fn print_message_details(m: &MessageEnvelope) {
    println!("id: {}", m.id);
    println!("workspace: {}", m.workspace.display());
    println!("created_at: {}", m.created_at);
    let sender = m.from.tag.clone().unwrap_or_else(|| {
        if m.from.external {
            "(external)".into()
        } else {
            "(unknown)".into()
        }
    });
    println!(
        "from: {sender}{}",
        m.from
            .engine
            .as_deref()
            .map(|e| format!(" [{e}]"))
            .unwrap_or_default()
    );
    match &m.to {
        Recipient::Tag { tag, .. } => println!("to: {tag}"),
        Recipient::Broadcast { .. } => println!("to: * (broadcast)"),
        Recipient::Engine { engine } => println!("to: engine:{engine}"),
    }
    println!("kind: {}", m.kind);
    if let Some(r) = m.reply_to {
        println!("reply_to: {r}");
    }
    println!(
        "delivery: {}",
        match m.delivery {
            Delivery::Inbox => "inbox",
            Delivery::Pane => "pane",
        }
    );
    println!("---");
    println!("{}", m.body);
    if let Some(d) = &m.data {
        println!("---");
        println!("data: {d}");
    }
}

fn cmd_message(paths: &Paths, args: MessageArgs, json_output: bool) -> Result<()> {
    match args.command {
        MessageCommand::Send(a) => cmd_message_send(paths, a, json_output),
        MessageCommand::Reply(a) => cmd_message_reply(paths, a, json_output),
        MessageCommand::Inbox(a) => cmd_message_inbox(paths, a, json_output),
        MessageCommand::Log(a) => cmd_message_log(paths, a, json_output),
        MessageCommand::Show(a) => cmd_message_show(paths, a, json_output),
        MessageCommand::Ack(a) => cmd_message_ack(paths, a, json_output),
        MessageCommand::Gc(a) => cmd_message_gc(paths, a, json_output),
    }
}

fn cmd_message_send(paths: &Paths, args: MessageSendArgs, json_output: bool) -> Result<()> {
    if args.pane_delivery.pane && (args.all || args.to_engine.is_some()) {
        bail!("--pane cannot be combined with --all or --to-engine: no pane broadcast");
    }
    if args.pane_delivery.pane && args.to.is_none() {
        bail!("--pane requires --to TAG");
    }
    check_body_size(&args.text)?;
    let workspace = resolve_message_workspace(None)?;
    let data = parse_data_arg(args.data.as_deref())?;
    let from = resolve_sender(paths, &workspace, args.from.as_deref())?;
    let to = build_recipient(
        paths,
        &workspace,
        args.to.as_deref(),
        args.all,
        args.to_engine.as_deref(),
        args.queue,
    )?;
    let envelope = MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: workspace.clone(),
        created_at: now_secs(),
        from,
        to,
        kind: args.kind,
        reply_to: None,
        body: args.text,
        data,
        delivery: Delivery::Inbox,
    };
    let envelope = finish_send(paths, &workspace, envelope, &args.pane_delivery)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        println!("{}", envelope.id);
    }
    Ok(())
}

fn cmd_message_reply(paths: &Paths, args: MessageReplyArgs, json_output: bool) -> Result<()> {
    check_body_size(&args.text)?;
    let workspace = resolve_message_workspace(None)?;
    let original = read_message(paths, &workspace, args.message_id)
        .with_context(|| format!("no such message {}", args.message_id))?;
    let to_tag = original.from.tag.clone().ok_or_else(|| {
        anyhow!("original message {} was sent anonymously (no sender tag); reply with `a message send --to <tag>` instead", args.message_id)
    })?;
    let data = parse_data_arg(args.data.as_deref())?;
    let from = resolve_sender(paths, &workspace, args.from.as_deref())?;
    let target = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == to_tag);
    let to = Recipient::Tag {
        tag: to_tag,
        session_id: target.map(|r| r.id).or(original.from.session_id),
    };
    let envelope = MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id: Uuid::now_v7(),
        workspace: workspace.clone(),
        created_at: now_secs(),
        from,
        to,
        kind: args.kind.unwrap_or_else(|| "reply".to_string()),
        reply_to: Some(original.id),
        body: args.text,
        data,
        delivery: Delivery::Inbox,
    };
    let envelope = finish_send(paths, &workspace, envelope, &args.pane_delivery)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    } else {
        println!("{}", envelope.id);
    }
    Ok(())
}

fn cmd_message_inbox(paths: &Paths, args: MessageInboxArgs, json_output: bool) -> Result<()> {
    let _ = args.new; // `--new` is accepted for CLI-surface compatibility; unread is already the default (design doc section 7).
    let workspace = resolve_message_workspace(None)?;
    let (consumer_id, consumer_tag, consumer_engine) =
        resolve_consumer(paths, &workspace, args.from.as_deref())?;
    let _ = maybe_gc(paths, &workspace);
    let cursor = read_cursor(paths, &workspace, consumer_id)?;
    let messages: Vec<MessageEnvelope> = list_messages(paths, &workspace)?
        .into_iter()
        .filter(|m| addressed_to(m, consumer_id, &consumer_tag, &consumer_engine))
        .filter(|m| !cursor.is_acked(m.id))
        .collect();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no unread messages");
    } else {
        for m in &messages {
            print_message_line(m);
        }
    }
    Ok(())
}

fn cmd_message_log(paths: &Paths, args: MessageLogArgs, json_output: bool) -> Result<()> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let _ = maybe_gc(paths, &workspace);
    let messages = list_messages(paths, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else if messages.is_empty() {
        println!("no messages");
    } else {
        for m in &messages {
            print_message_line(m);
        }
    }
    Ok(())
}

fn cmd_message_show(paths: &Paths, args: MessageShowArgs, json_output: bool) -> Result<()> {
    let workspace = resolve_message_workspace(None)?;
    let message = read_message(paths, &workspace, args.message_id)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&message)?);
    } else {
        print_message_details(&message);
    }
    Ok(())
}

fn cmd_message_ack(paths: &Paths, args: MessageAckArgs, json_output: bool) -> Result<()> {
    if args.all && !args.message_ids.is_empty() {
        bail!("cannot combine --all with explicit message ids");
    }
    if !args.all && args.message_ids.is_empty() {
        bail!("specify at least one message id, or --all");
    }
    let workspace = resolve_message_workspace(None)?;
    let (consumer_id, consumer_tag, consumer_engine) =
        resolve_consumer(paths, &workspace, args.from.as_deref())?;
    let ids: Vec<Uuid> = if args.all {
        let cursor = read_cursor(paths, &workspace, consumer_id)?;
        list_messages(paths, &workspace)?
            .into_iter()
            .filter(|m| addressed_to(m, consumer_id, &consumer_tag, &consumer_engine))
            .filter(|m| !cursor.is_acked(m.id))
            .map(|m| m.id)
            .collect()
    } else {
        args.message_ids
    };
    ack_messages(paths, &workspace, consumer_id, &ids)?;
    if json_output {
        println!("{}", json!({"acked": ids}));
    } else {
        println!("acked {} message(s)", ids.len());
    }
    Ok(())
}

fn cmd_message_gc(paths: &Paths, args: MessageGcArgs, json_output: bool) -> Result<()> {
    let workspace = resolve_message_workspace(args.workspace.as_deref())?;
    let report = gc_workspace(paths, &workspace)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "removed {} message(s), {} remaining",
            report.removed, report.remaining
        );
    }
    Ok(())
}

#[cfg(not(test))]
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_millis(100);

fn set_control_deadlines(stream: &UnixStream) -> Result<()> {
    stream
        .set_read_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker response deadline")?;
    stream
        .set_write_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker request deadline")?;
    Ok(())
}

fn clear_streaming_deadlines(stream: &UnixStream) -> Result<()> {
    stream
        .set_read_timeout(None)
        .context("clear attach streaming read deadline")?;
    stream
        .set_write_timeout(None)
        .context("clear attach streaming write deadline")?;
    Ok(())
}

fn connect_with_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let path_bytes = path.as_os_str().as_bytes();
    let _ = CString::new(path_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL"))?;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path_bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path_bytes.len(),
        );
    }
    let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len() + 1)
        as libc::socklen_t;
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let deadline = Instant::now() + timeout;
    loop {
        let connected = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                address_len,
            )
        };
        if connected == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            // AF_UNIX reports EAGAIN rather than EINPROGRESS when its listen
            // backlog is full. In that case no connection attempt is queued,
            // so retry with a small backoff until the same absolute deadline.
            // Polling this unconnected fd can report POLLOUT immediately and
            // would otherwise turn a stopped worker into a busy-spin.
            Some(libc::EAGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("connect {} timed out", path.display()),
                    ));
                }
                thread::sleep(remaining.min(Duration::from_millis(10)));
                continue;
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {}
            _ => return Err(error),
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect {} timed out", path.display()),
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready < 0 {
            let poll_error = io::Error::last_os_error();
            if poll_error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(poll_error);
        }
        if ready == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect {} timed out", path.display()),
            ));
        }
        let mut socket_error: libc::c_int = 0;
        let mut socket_error_len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast::<libc::c_void>(),
                &raw mut socket_error_len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if socket_error == 0 {
            // EINPROGRESS completes here. EAGAIN can also become writable
            // without having queued a connection, in which case retrying
            // connect above distinguishes success from another EAGAIN.
            let peer_len_result = unsafe {
                let mut peer: libc::sockaddr_un = std::mem::zeroed();
                let mut peer_len = std::mem::size_of_val(&peer) as libc::socklen_t;
                libc::getpeername(
                    fd.as_raw_fd(),
                    (&raw mut peer).cast::<libc::sockaddr>(),
                    &raw mut peer_len,
                )
            };
            if peer_len_result == 0 {
                break;
            }
        } else if socket_error != libc::EAGAIN && socket_error != libc::EINPROGRESS {
            return Err(io::Error::from_raw_os_error(socket_error));
        }
    }

    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) })
}

fn connect(record: &SessionRecord) -> Result<UnixStream> {
    let stream = connect_with_timeout(&record.socket_path, CONTROL_RPC_TIMEOUT)
        .with_context(|| format!("connect {}", record.socket_path.display()))?;
    set_control_deadlines(&stream)?;
    Ok(stream)
}
fn rpc_simple(record: &SessionRecord, operation: Operation, data: Option<&[u8]>) -> Result<Value> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, operation);
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    if let Some(bytes) = data {
        write_frame(&mut stream, FrameKind::Data, bytes)?;
    }
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed connection"))?;
    let response: Response = frame_json(frame)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()
}
fn rpc_send(record: &SessionRecord, data: &[u8]) -> Result<()> {
    rpc_simple(record, Operation::Send { bytes: data.len() }, Some(data))?;
    Ok(())
}
fn rpc_capture(record: &SessionRecord, max: Option<usize>) -> Result<Vec<u8>> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, Operation::Capture { max_bytes: max });
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    let response: Response =
        frame_json(read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()?;
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing capture data"))?;
    if frame.kind != FrameKind::Data {
        bail!("expected capture data");
    }
    Ok(frame.payload)
}
/// `a capture --screen [--plain]` (docs/terminal-state-design.md section 8):
/// mirrors `rpc_capture`'s shape exactly, against `Operation::CaptureScreen`.
fn rpc_capture_screen(record: &SessionRecord, plain: bool) -> Result<Vec<u8>> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, Operation::CaptureScreen { plain });
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    let response: Response =
        frame_json(read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()?;
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing capture data"))?;
    if frame.kind != FrameKind::Data {
        bail!("expected capture data");
    }
    Ok(frame.payload)
}

/// Default amount of history replayed on attach when the caller didn't ask
/// for more via `--history-bytes`. The old default -- passing `None` through
/// to the server, which `History::snapshot` treats as "the whole buffer" --
/// meant every attach replayed up to the session's entire configured
/// history capacity (DEFAULT_HISTORY_BYTES = 4MB), which looks like the
/// session "rewinding" through its whole scrollback instead of showing
/// anything resembling the current screen. There's no real terminal
/// emulator here (spec.md's v1 non-goal), so this can only approximate
/// "current state" by replaying a short tail of raw bytes -- in practice
/// that tail still usually contains the shell/TUI's own recent
/// cursor-position/clear escapes and renders close enough.
const DEFAULT_ATTACH_REPLAY_BYTES: usize = 32 * 1024;

/// **These timers no longer decide whether a redraw is *safe*, only when one
/// is *wanted*.** The previous version of this comment said aplexer had "no
/// real terminal emulation (spec.md's v1 non-goal)" and that building it was
/// "a much bigger project than this fix", so the timers were the whole
/// mitigation: redraw in an idle gap and hope. That was already stale when it
/// was written -- docs/terminal-state-design.md shipped a live `vt100` model
/// in the worker -- and the hope did not survive contact with the workload
/// aplexer exists for. An agent CLI mid-generation never goes quiet, so
/// `STATUS_BAR_IDLE_GAP` never opened and `STATUS_BAR_MAX_INTERVAL` fired into
/// an arbitrary byte offset of the relayed stream, forever. Measured on a real
/// `a attach` against a continuously-streaming full-screen TUI (issue #5), 5 of
/// 10 redraws were spliced into the middle of an unterminated CSI sequence:
/// the host terminal abandoned the workload's half-read sequence and printed
/// its remaining parameter bytes as literal text into the workload's own
/// frame.
///
/// The client now keeps its own `ClientScreen` (`aplexer::screen`) over the
/// bytes it relays, so "is this a safe place to write?" is answered from the
/// stream's actual parser state rather than guessed from a clock:
/// `draw_status_bar` refuses to write unless the stream is between complete
/// escape sequences and characters, and defers to `ctx.pending` otherwise --
/// which the frame loop flushes at the first safe boundary. What is left for
/// these constants is scheduling: `STATUS_BAR_IDLE_GAP` still debounces an
/// idle session's redraws, and `STATUS_BAR_MAX_INTERVAL` still bounds how
/// stale a continuously-streaming session's bar may get.
const STATUS_BAR_IDLE_GAP: Duration = Duration::from_millis(450);
const STATUS_BAR_MAX_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_BAR_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// While the attached session's state is `working` (a fresh `a state-report`
/// push -- the agent said it is running; see `spinner_frame` for why the
/// guessed `active` state deliberately does not animate), the state glyph
/// becomes a braille spinner so the bar shows liveness a static state word
/// cannot. The spinner is the only thing that moves: the rest of the bar is
/// byte-identical frame to frame, so `draw_status_bar`'s dirty-check means
/// the animation itself is the entire incremental write cost, and the
/// instant the state word leaves working the glyph freezes back to
/// `state_glyph`'s static one. One frame per `STATUS_BAR_POLL_INTERVAL`
/// tick keeps the cadence aligned with the thread that drives it; ten
/// frames is a 1.5s revolution -- standard spinner speed, deliberately
/// unhurried.
const SPINNER_FRAME_MS: u64 = STATUS_BAR_POLL_INTERVAL.as_millis() as u64;
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How long a redraw may be held back purely because the workload has an
/// unclosed synchronized-output block (`CSI ? 2026 h`).
///
/// Staying out of a declared frame is a genuine improvement -- opencode and
/// codex bracket every frame in `?2026`, so the block boundaries are exact
/// frame boundaries handed to us for free -- but it is a *preference*, not a
/// correctness requirement: an injection at an escape boundary inside a
/// synchronized block is transparent anyway, since the cursor and pen are
/// restored absolutely. Bounding it means a workload that opens a block and
/// never closes it (or a terminal-side sync timeout that already released it)
/// cannot freeze the bar indefinitely.
const STATUS_BAR_SYNC_DEFER_LIMIT: Duration = Duration::from_millis(500);

/// How long a terminal resize's DECSTBM may be held back by the escape
/// boundary gate before it is written anyway (issue #14).
///
/// The gate is the same one `draw_status_bar` uses, but the escape hatch is
/// not. A status redraw that never happens costs a stale bar; a *resize* that
/// never happens leaves the host terminal scrolling a region sized for the
/// old geometry for the rest of the attach, which is exactly the "workload
/// renders at the wrong size indefinitely" outcome that is worse than the
/// splice the gate exists to prevent. A stream normally reaches a boundary
/// within one PTY chunk, so this deadline only fires when a workload has
/// stopped emitting part-way through an escape sequence -- a state in which
/// the host terminal is already stuck waiting for bytes that are not coming.
const LAYOUT_DEFER_LIMIT: Duration = Duration::from_millis(500);

/// A terminal resize whose DECSTBM the boundary gate held back, and when it
/// was first held back (`LAYOUT_DEFER_LIMIT`'s deadline is measured from the
/// first deferral, not from the most recent resize).
#[derive(Clone, Copy)]
struct PendingLayout {
    rows: u16,
    cols: u16,
    since: Instant,
}

/// Physical terminal geometry as last observed by the resize-poll thread,
/// shared with the status-bar thread so its redraws always target the
/// current last row/width without a second ioctl.
#[derive(Clone, Copy)]
struct TermGeom {
    rows: u16,
    cols: u16,
    /// Whether the bottom row is reserved for the status bar. False for
    /// terminals too small to spare a row (see `reserved_rows`), in which
    /// case the scroll region is left/reset to full-screen and the status
    /// bar is simply not drawn.
    reserved: bool,
}

/// The row count told to the SERVER: one less than the physical terminal
/// when a status row is reserved, exactly like tmux tells the remote PTY its
/// terminal is one row shorter than reality so its own output never
/// overwrites the reserved line.
fn reserved_rows(rows: u16) -> u16 {
    if rows > 2 {
        rows - 1
    } else {
        rows
    }
}

/// Serializes a write behind the shared stdout lock so the main frame loop
/// (writing PTY data) and the status-bar/layout threads (writing redraws)
/// can never tear/interleave each other's output.
///
/// **The stdout lock is also the client's terminal-state lock.** Anything that
/// consults `ClientScreen` in order to decide *what* to write -- where the
/// workload's cursor is, whether the stream is between escape sequences --
/// must hold this lock across both the decision and the write, or the answer
/// can go stale in the gap. It did: the first version of the issue #5 fix
/// checked the escape boundary in the status thread and wrote afterwards, and
/// a real capture caught 4 of 39 redraws still landing mid-CSI because the
/// frame loop had written another chunk in between.
///
/// Lock order everywhere is **`stdout` -> `term` -> `screen`**, with
/// `last_drawn`/`flash`/`record` as leaves. Nothing acquires `stdout` while
/// holding `term` or `screen`: `apply_terminal_layout` writes and only then
/// records the new geometry, and the switch path reads the geometry before
/// taking `stdout`.
///
/// **Not for live injections.** This helper writes unconditionally, so it is
/// only correct where there is no relayed stream to splice: attach start
/// (before the first workload byte) and detach (after the last one). A
/// status redraw, a `Ctrl-b r` refresh, or a resize DECSTBM goes through
/// `write_client_locked`, which is the boundary gate. `write_locked`'s two
/// call sites are pinned by
/// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`.
fn write_locked(stdout: &Arc<Mutex<io::Stdout>>, bytes: &[u8]) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.write_all(bytes)?;
    out.flush()
}

/// Whether a client-originated write may still go out when the relayed
/// stream is *not* between complete escape sequences.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BoundaryPolicy {
    /// Refuse and let the caller park the write for the next boundary.
    /// Everything the user can wait for: the status bar, `Ctrl-b r`, and a
    /// resize that has not yet hit `LAYOUT_DEFER_LIMIT`.
    Defer,
    /// Write anyway. **The one narrow exemption in the client** (issue #14):
    /// a resize whose DECSTBM has already been held back for
    /// `LAYOUT_DEFER_LIMIT`, i.e. a workload that stopped emitting part-way
    /// through an escape sequence and is never going to finish it. One
    /// spliced frame beats a host terminal left scrolling the old geometry
    /// for the rest of the attach. Exactly one call site may pass this, and
    /// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`
    /// fails if a second one appears.
    PastDeadline,
    /// Write anyway, because **there is no relayed stream to splice into**.
    ///
    /// Not a second exemption from the gate so much as a case the gate does
    /// not apply to. While scroll mode is active (`Ctrl-b [`, or a wheel
    /// roll) the client has taken the host terminal away from the relay
    /// entirely: `relay_to_terminal` still feeds every workload byte to the
    /// model -- that is what keeps the history growing and makes the exit
    /// repaint correct -- but writes none of them, so the host is not
    /// part-way through anything the workload emitted. `at_escape_boundary`
    /// would still be answering for the *model*, which by then is many
    /// chunks ahead of the host, so consulting it here would be consulting
    /// the wrong stream: it can sit false indefinitely on a workload that
    /// stopped mid-sequence, and deferring on that would freeze the pager
    /// the user is actively driving.
    ///
    /// What the host may genuinely be part-way through is the *last* chunk
    /// written before the relay was suspended. `SCROLL_CANCEL` (`CAN`, the
    /// control every VT parser treats as "abandon the sequence in flight")
    /// leads every write made under this policy, which is what makes it
    /// safe; the sites are pinned by
    /// `scroll_mode_writes_are_the_only_stream_suspended_ones`.
    StreamSuspended,
}

/// **The single funnel for client-originated bytes**, and therefore the one
/// place the escape-boundary gate has to live.
///
/// Issue #5 put the gate inside `draw_status_bar`, which covered its eight
/// callers and silently did not cover `apply_terminal_layout` -- a ninth
/// writer that reached stdout by another route and spliced DECSTBM into
/// half-emitted CSI sequences from the resize poller's wall clock (issue
/// #14). A gate that each new writer has to *remember* is a gate that the
/// next writer forgets, so it moved here: writer eleven is gated because it
/// cannot put bytes on the terminal any other way.
///
/// The caller must already hold the stdout lock. That is not tidiness: the
/// first version of the #5 fix checked the boundary in the status thread and
/// wrote afterwards, and a real capture caught 4 of 39 redraws still landing
/// mid-CSI, because the frame loop wrote another chunk in the gap. Checking
/// and writing under one lock is what closes it -- see `write_locked`.
///
/// Returns whether the bytes went out. `false` means the stream was
/// mid-sequence and the caller must park the write for a later boundary
/// rather than drop it.
fn write_client_locked(
    out: &mut impl Write,
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    bytes: &[u8],
    policy: BoundaryPolicy,
) -> bool {
    let at_boundary = screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .at_escape_boundary();
    if !at_boundary && policy == BoundaryPolicy::Defer {
        return false;
    }
    let _ = out.write_all(bytes);
    let _ = out.flush();
    true
}

/// The DECSTBM reservation (or its removal, on a terminal too small to spare
/// a row) followed by an absolute cursor restore from the client's model.
///
/// No `\x1b7`/`\x1b8` bracket: see `status_bar_sequence` for why the client
/// must never write to the shared save-cursor register.
fn terminal_layout_sequence(rows: u16, restore: &[u8]) -> Vec<u8> {
    let mut seq = Vec::new();
    if rows > 2 {
        seq.extend_from_slice(format!("\x1b[1;{}r", rows - 1).as_bytes());
    } else {
        seq.extend_from_slice(b"\x1b[r");
    }
    seq.extend_from_slice(restore);
    seq
}

/// Sets (or, for a too-small terminal, clears) the DECSTBM scrolling region
/// and records the resulting geometry for the status-bar thread.
///
/// DECSTBM moves the cursor to the region's home position as a side effect on
/// real terminals, so something has to put it back. That used to be a
/// `\x1b7`/`\x1b8` (DECSC/DECRC) bracket, which is exactly the bug issue #5
/// exists for: a terminal has one save-cursor register, and writing to it from
/// a stream we are only relaying destroys whatever the workload had saved
/// there. The cursor is restored from `ClientScreen` instead -- absolutely,
/// and including the workload's pen -- so the register stays the workload's
/// private property.
///
/// **Boundary-gated, exactly like `draw_status_bar`** (issue #14). This is
/// client-originated output spliced into a stream the client is only
/// relaying, so the same rule applies: a resize landing while the workload is
/// mid-escape-sequence would make the host terminal abandon the workload's
/// half-emitted CSI and print its remaining parameter bytes as literal text.
/// The resize poller fires from a wall clock, so its writes land at arbitrary
/// byte offsets by construction -- the identical defect the status bar had.
///
/// Two deliberate differences from `draw_status_bar`'s use of the same gate:
///
/// - **No synchronized-output deferral.** Holding a *status bar* out of a
///   workload's declared frame is a cosmetic preference (see
///   `STATUS_BAR_SYNC_DEFER_LIMIT`); holding the *scroll region* back is not
///   cosmetic, because until DECSTBM is reasserted the host is scrolling a
///   region sized for the old terminal. An injection at a genuine escape
///   boundary is transparent anyway, so the escape boundary is the whole
///   requirement here.
/// - **A deadline** (`BoundaryPolicy::PastDeadline`, the client's only
///   exemption), because an undelivered resize is worse than a spliced one.
///
/// What is *not* deferred is the workload's own notification: the resize
/// poller's `AttachControl::Resize` goes to the worker unconditionally, so
/// the PTY is resized and SIGWINCH delivered on time no matter what the
/// host-side reservation is doing. A workload blocked on the new size never
/// waits on this gate -- only the client's own row reservation does.
///
/// Returns whether bytes actually reached the terminal. A deferral is
/// recorded in `ctx.pending_layout` and flushed by `flush_pending_layout`;
/// see that function for why deferring here never loses a resize.
fn apply_terminal_layout(ctx: &StatusBarCtx, rows: u16, cols: u16) -> bool {
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    apply_terminal_layout_to(&mut *out, ctx, rows, cols)
}

/// `apply_terminal_layout` with the destination passed in, the stdout lock
/// already held by the caller.
///
/// Split out for the reason `status_bar_redraw` exists: a test can drive the
/// real gate, and the real bytes, into a `vt100` host terminal without
/// redirecting the process's fd 1 out from under a concurrently-running test
/// harness. Production has exactly one caller pair -- `apply_terminal_layout`
/// and `flush_pending_layout` -- and both hold the lock across it, because
/// the boundary check and the write must not be separable.
fn apply_terminal_layout_to(
    out: &mut impl Write,
    ctx: &StatusBarCtx,
    rows: u16,
    cols: u16,
) -> bool {
    let reserved = rows > 2;
    // The deadline is a clock rather than stream state, so unlike the
    // boundary check it cannot go stale under the lock.
    let policy = match layout_deferred_since(ctx) {
        Some(since) if since.elapsed() >= LAYOUT_DEFER_LIMIT => BoundaryPolicy::PastDeadline,
        _ => BoundaryPolicy::Defer,
    };
    let wrote = {
        let restore = ctx
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursor_restore();
        let seq = terminal_layout_sequence(rows, &restore);
        write_client_locked(out, &ctx.screen, &seq, policy)
    };
    {
        // Park or clear the deferral. Re-parking keeps the *original*
        // deadline, so a stream that never reaches a boundary cannot
        // postpone delivery indefinitely by resizing again; the geometry is
        // overwritten, because only the latest physical size is correct.
        let mut pending = ctx
            .pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *pending = if wrote {
            None
        } else {
            let since = pending.map(|p| p.since).unwrap_or_else(Instant::now);
            Some(PendingLayout { rows, cols, since })
        };
    }
    // Recorded even when the bytes were deferred, and deliberately so: the
    // physical terminal has *already* changed size, so the status bar must
    // start targeting the new last row immediately or it draws over a row
    // the workload now owns. `TermGeom` is internal state, not output --
    // recording it puts nothing on the wire, and the bar's own write is
    // independently gated. `term` is taken under `stdout`, which is the
    // order `write_locked` documents.
    if let Ok(mut g) = ctx.term.lock() {
        *g = TermGeom {
            rows,
            cols,
            reserved,
        };
    }
    wrote
}

/// When the currently-parked resize was *first* held back, if there is one.
fn layout_deferred_since(ctx: &StatusBarCtx) -> Option<Instant> {
    ctx.pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .map(|p| p.since)
}

/// Delivers a resize whose DECSTBM was deferred by the boundary gate.
///
/// Deferring must never mean dropping (issue #14): a lost resize leaves the
/// host scrolling a region sized for the old terminal for the rest of the
/// attach. Two independent callers guarantee delivery, and they cover
/// disjoint failure modes:
///
/// - the main frame loop, after every relayed chunk -- the boundary the gate
///   was waiting for is by construction reached by relaying more bytes, so
///   this is the normal path and it fires within one PTY chunk;
/// - the status-bar thread's tick, every `STATUS_BAR_POLL_INTERVAL` -- the
///   frame loop only runs when the workload sends something, so a workload
///   that stops mid-sequence would otherwise park the resize forever. This
///   is also what makes `LAYOUT_DEFER_LIMIT` actually fire.
///
/// Peeks rather than takes: `apply_terminal_layout` clears the slot when it
/// writes and re-parks it (keeping the original deadline) when it cannot, so
/// a flush that loses the race with a still-unsafe stream does not drop the
/// resize on the floor.
fn flush_pending_layout(ctx: &StatusBarCtx) -> bool {
    // Cheap pre-check, before the stdout lock. The frame loop calls this
    // after *every* PTY chunk and there is almost never a resize parked, so
    // the common case must not queue behind the status thread's redraw for
    // nothing. Released before `stdout` is taken, so this adds no nesting to
    // the lock order; the authoritative read happens under the lock below.
    if layout_deferred_since(ctx).is_none() {
        return false;
    }
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    flush_pending_layout_to(&mut *out, ctx)
}

/// `flush_pending_layout` with the destination passed in and the stdout lock
/// already held. Calls `apply_terminal_layout_to`, never
/// `apply_terminal_layout`: the lock is not reentrant.
fn flush_pending_layout_to(out: &mut impl Write, ctx: &StatusBarCtx) -> bool {
    let pending = *ctx
        .pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match pending {
        Some(PendingLayout { rows, cols, .. }) => apply_terminal_layout_to(out, ctx, rows, cols),
        None => false,
    }
}

/// Writes a live PTY chunk to the terminal, after feeding it through the
/// client's own model of the workload's screen (`ClientScreen`).
///
/// The model is what makes the status bar safe to inject at all: it knows
/// where the workload's cursor and pen actually are, whether the relayed
/// stream is currently between complete escape sequences, and whether the
/// workload is part-way through a synchronized-output frame. It can also
/// rewrite the chunk -- the one case being the reserved-row walk, see
/// `ClientScreen::relay`.
///
/// The worker already pays the identical parse cost per chunk
/// (docs/terminal-state-design.md section 9's steady-state parse budget);
/// paying it a second time in the client is the price of the client no longer
/// writing blind into someone else's byte stream.
fn relay_to_terminal(
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    stdout: &Arc<Mutex<io::Stdout>>,
    scroll: &Arc<ScrollMode>,
    overlay: &Arc<KeyOverlay>,
    data: &[u8],
) -> io::Result<()> {
    let mut out = stdout.lock().unwrap_or_else(PoisonError::into_inner);
    {
        let mut s = screen.lock().unwrap_or_else(PoisonError::into_inner);
        let rewritten = s.relay(data);
        // A client modal -- the pager, or the `Ctrl-b` key overlay -- owns
        // the screen: the model still consumes every byte (that is what grows
        // the retained history the user is reading, and what makes the
        // repaint on the way out show everything that arrived meanwhile) but
        // nothing reaches the host. Checked here, under the same stdout lock
        // `enter_scroll_mode` and `show_key_overlay` flip their flags under,
        // so a chunk can never be half-written across a modal's first frame.
        // Type-through is the one exception: while `i` has handed the
        // keyboard over, the pager keeps only the bar row and the offset,
        // and the stream flows -- typing with no echo would be worse than
        // the reading view the user chose to give up. Esc takes it back.
        if (scroll.is_active() && !scroll.is_typing()) || overlay.is_active() {
            return Ok(());
        }
        let src = rewritten.as_deref().unwrap_or(data);
        if let Some(filtered) = s.filter_host(src) {
            out.write_all(&filtered)?;
        } else {
            out.write_all(src)?;
        }
    }
    out.flush()
}

/// Writes bytes the client is emitting verbatim -- the attach snapshot, a
/// switch's replayed screen -- and feeds them to the model under the *same*
/// stdout lock, so a concurrent status redraw can never see a model that is
/// ahead of what the terminal has actually been sent.
fn feed_and_write(
    stdout: &Arc<Mutex<io::Stdout>>,
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    prefix: &[u8],
    payload: &[u8],
    reset_to: Option<(u16, u16)>,
) -> io::Result<()> {
    let mut out = stdout.lock().unwrap_or_else(PoisonError::into_inner);
    {
        let mut s = screen.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((rows, cols)) = reset_to {
            s.reset(rows, cols);
        }
        s.feed(payload);
        if let Some(filtered) = s.filter_host(prefix) {
            out.write_all(&filtered)?;
        } else {
            out.write_all(prefix)?;
        }
        if let Some(filtered) = s.filter_host(payload) {
            out.write_all(&filtered)?;
        } else {
            out.write_all(payload)?;
        }
    }
    out.flush()
}

/// Undoes `apply_terminal_layout` and clears the screen, exactly like tmux
/// does on detach (Ctrl-b d) -- otherwise whatever was last drawn (including
/// the status bar) just sits in the user's terminal after attach() returns.
/// `\x1b[2J\x1b[H` (full clear + cursor home) is used rather than a fuller
/// reset (`\x1bc`) because it doesn't disturb terminal scrollback history.
const TERMINAL_RESET_SEQUENCE: &[u8] = b"\
\x1b[?1049l\
\x1b[?1007h\
\x1b>\
\x1b[?1l\
\x1b[?2004l\
\x1b[?9l\
\x1b[?1000l\
\x1b[?1002l\
\x1b[?1003l\
\x1b[?1005l\
\x1b[?1006l\
\x1b[r\
\x1b[0m\
\x1b[2J\
\x1b[H\
\x1b[?25h";

/// Written once at attach start, before layout or the snapshot. Isolates the
/// live session from the host's primary-screen scrollback (the `a` list), and
/// stops the host translating the mouse wheel into arrow keys.
///
/// `?1049h` alone created a second bug: the alternate screen has no
/// scrollback, so a terminal with xterm's `alternateScroll` (DECSET 1007,
/// on by default nearly everywhere) answers a wheel event by *synthesizing
/// cursor-up/down key presses* and sending them to the workload. Inside an
/// agent TUI that is not merely useless -- the wheel silently walks the
/// agent's own menus and prompt history, i.e. scrolling to read types input
/// into the session. `?1007l` turns that translation off for the duration of
/// the attach, so the wheel does nothing instead of something destructive.
/// `reset_terminal` restores it on detach, since the mode is terminal-global
/// and the user's next `less`/`vim` expects the default back.
const ATTACH_ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h\x1b[?1007l";

fn reset_terminal(stdout: &Arc<Mutex<io::Stdout>>) {
    // `\x1b[?1049l` first (docs/terminal-state-design.md section 6.3): the
    // attach client holds the host on the alternate screen for the whole
    // session (see `ATTACH_ALT_SCREEN_ENTER`) so the pre-attach primary
    // scrollback -- typically the `a` session list -- cannot mix into the
    // live view. Detach must return the host to that primary screen.
    // Workload-originated 1049l is stripped from the relay and never
    // reaches the host; this write is the one exit that does.
    //
    // The snapshot path also reproduces every input mode tracked by vt100.
    // Disable all of their possible variants unconditionally: application
    // keypad/cursor, bracketed paste, the four mouse protocols, and both
    // non-default mouse encodings. Sending the resets is harmless when a
    // mode was already off and avoids leaving the user's shell consuming
    // application key or mouse reports after any attach exit.
    //
    // `\x1b[?25h` (DECTCEM show cursor) is included unconditionally: a
    // full-screen TUI in the workload (htop, vim, an agent CLI's spinner,
    // ...) commonly hides the cursor with `\x1b[?25l` while it owns the
    // screen and relies on its own exit path to show it again -- but that
    // exit path runs on the *workload's* side, and detaching doesn't wait
    // for or depend on it. Without this, a detach can leave the user's real
    // terminal with an invisible cursor after the workload's last draw
    // happened to hide it. Showing an already-visible cursor is a no-op, so
    // this is safe to send regardless of what state the workload (or our
    // own status-bar redraw, which never hides the cursor) left it in.
    let _ = write_locked(stdout, TERMINAL_RESET_SEQUENCE);
}

/// RAII guard that runs `reset_terminal` on every exit path out of attach()
/// -- explicit Ctrl-b d detach, the remote session exiting, a connection
/// error, or an early `?` return -- so a new exit path added later can't
/// forget the cleanup. Constructed whenever stdout is a tty, independently
/// of whether stdin is interactive; `RawMode` remains stdin-specific.
struct TerminalUiGuard {
    stdout: Arc<Mutex<io::Stdout>>,
}
impl Drop for TerminalUiGuard {
    fn drop(&mut self) {
        reset_terminal(&self.stdout);
    }
}

fn format_bytes(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = KI * 1024;
    const GI: u64 = MI * 1024;
    if bytes >= GI {
        format!("{:.1}G", bytes as f64 / GI as f64)
    } else if bytes >= MI {
        format!("{:.0}M", bytes as f64 / MI as f64)
    } else if bytes >= KI {
        format!("{:.0}K", bytes as f64 / KI as f64)
    } else {
        format!("{bytes}B")
    }
}

/// One `Operation::Status` round-trip per status-bar redraw, shared by the
/// memory and foreground-command indicators below so a single bar refresh
/// costs one worker round-trip, not one per indicator. `None` on any RPC
/// failure (worker briefly unreachable) -- every indicator built from this
/// just degrades to "omitted" in that case, same as before this was
/// shared.
fn live_status(record: &SessionRecord) -> Option<Value> {
    rpc_simple(record, Operation::Status, None).ok()
}

/// The attached session's record as the state derivation should see it.
///
/// `ctx.record` is a snapshot from attach/switch time, but state-report
/// pushes land in the worker's in-memory record (and on disk) with no event
/// reaching the attached client -- deriving the state from the snapshot
/// alone would trust a push that is minutes old and miss every push made
/// after attach, which is exactly the "agent started working while I
/// watched" case the spinner exists for. The Status answer already
/// serializes the worker's live record (`public_session_record`), so
/// overlay its reported-state pair and its activity stamp onto the
/// snapshot: the activity stamp is half of the `idle` push's validity rule
/// (`watch::fresh_reported_state` retracts a resting push once newer PTY
/// output appears), so deriving from the attach-time stamp would judge
/// every post-attach rest against pre-attach output -- an agent that went
/// back to work after attach would keep its stale `idle` claim forever
/// from the bar's point of view. A missing field (older worker) or a
/// failed RPC (`raw` None) leaves the snapshot untouched, same degradation
/// as the memory indicator.
fn overlay_reported_state(record: &SessionRecord, raw: Option<&Value>) -> SessionRecord {
    let mut fresh = record.clone();
    let Some(raw) = raw else {
        return fresh;
    };
    if let Some(s) = raw.get("reported_state").and_then(Value::as_str) {
        fresh.reported_state = Some(s.to_string());
    }
    if let Some(ms) = raw.get("reported_state_at_ms").and_then(Value::as_u64) {
        fresh.reported_state_at_ms = Some(ms);
    }
    if let Some(ms) = raw.get("last_activity_ms").and_then(Value::as_u64) {
        fresh.last_activity_ms = Some(ms);
    }
    fresh
}

/// Live memory indicator from the session's cgroup, if it has one -- a
/// small "useful for our application" touch given aplexer's whole reason
/// for existing is resource-isolated agent sessions. Best-effort: absence
/// of cgroup stats in `raw` (no cgroup configured) just omits the
/// indicator rather than disrupting the status bar.
fn memory_indicator(record: &SessionRecord, raw: &Value) -> Option<String> {
    let current = raw.get("cgroup")?.get("memory_current")?.as_u64()?;
    let used = format_bytes(current);
    Some(match record.limits.memory_bytes {
        Some(max) => format!("{used}/{}", format_bytes(max)),
        None => used,
    })
}

/// Plain interactive shells: showing e.g. `[shell -> bash]` for an ordinary
/// shell session would be redundant noise (that's what `shell` already
/// means), not information. Only an actually interesting foreground
/// program -- something manually run inside the session that isn't just
/// its own shell -- is worth surfacing.
const PLAIN_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh", "tcsh", "csh"];

/// The live foreground-command override for the status bar, if there's
/// anything worth showing beyond `record.engine` alone (see
/// `foreground_command` in lib.rs and `Operation::Status`'s worker-side
/// handler for where `raw["foreground_command"]` comes from -- a live,
/// never-persisted read of the pty's current foreground process, the same
/// mechanism tmux uses for `pane_current_command`). `None` when: the
/// worker didn't report one (RPC failure, no foreground process group
/// yet); it's a bare interactive shell (`PLAIN_SHELLS`); or it's just the
/// engine's own launch command running as expected (e.g. a `codex`-engine
/// session actually running `codex` shouldn't redundantly show
/// `[codex -> codex]`).
fn foreground_override(record: &SessionRecord, raw: &Value) -> Option<String> {
    let fg = raw.get("foreground_command")?.as_str()?;
    if PLAIN_SHELLS.contains(&fg) {
        return None;
    }
    let launched = record
        .command
        .first()
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str());
    if launched == Some(fg) {
        return None;
    }
    Some(fg.to_string())
}

/// The detected agent's display name when it adds information beyond the
/// declared engine, `None` when it doesn't. One display rule for every
/// human surface (list rows, `a status`, the attach status bar): a session
/// declared `engine: "claude"` that is running claude says "claude" once;
/// a `shell` session running claude, or a `claude` session someone started
/// codex inside, gets the detected name appended.
fn extra_agent_label(
    record: &SessionRecord,
    detected: Option<aplexer::agent_kind::AgentKind>,
) -> Option<&'static str> {
    let agent = detected?;
    (agent.name() != record.engine).then_some(agent.name())
}

/// The list/status engine cell. A plain `shell` workload that detection
/// found an agent inside is labeled by the agent alone: `shell` is the
/// absence of a choice, so `shell -> codex` spent the column on noise when
/// `codex` is the fact. A declared engine keeps the `engine -> agent` form,
/// where the base carries real information (a claude session someone
/// started codex inside).
fn engine_label(
    record: &SessionRecord,
    detected: Option<aplexer::agent_kind::AgentKind>,
) -> String {
    let agent = extra_agent_label(record, detected);
    if record.engine == "shell" {
        if let Some(agent) = agent {
            return agent.to_string();
        }
    }
    let base = match &record.profile {
        Some(profile) => format!("{}/{}", record.engine, profile),
        None => record.engine.clone(),
    };
    match agent {
        Some(agent) => format!("{base} -> {agent}"),
        None => base,
    }
}

/// `{i}:{tag}[*][({state})]` for every session in the current workspace,
/// mirroring how `a list`'s tree groups sessions by workspace (see
/// `group_by_workspace`) -- a live glance at what else is running here
/// without detaching, and (unlike the old `sibling_summary` it replaces)
/// self-documenting: `i` is exactly the number `Ctrl-b 1`..`9` jumps to
/// (`pick_switch_target`'s `Index` arm), because both walk the same
/// `list_records` order (`Reverse(created_at_ms)`) that `group_by_workspace`
/// preserves within a group -- see the equivalence note on
/// `resolve_quick_index`. `*` marks the currently attached session;
/// `(state)` is appended only when the state is not "running" (the common
/// case needs no label). Lists **all** sessions including the current one
/// (the old version listed only "the others") because the numbering only
/// makes sense as a complete index. Example: `1:main* 2:review
/// 3:build(broken)`. A single-session workspace omits the segment (empty
/// string), same as before.
fn workspace_summary(ctx: &StatusBarCtx, record: &SessionRecord) -> String {
    let records = match list_records(&ctx.paths) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let siblings: Vec<SessionRecord> = records
        .into_iter()
        .filter(|r| r.workspace == record.workspace)
        .collect();
    if siblings.len() <= 1 {
        return String::new();
    }
    siblings
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (state, _) = session_ui_state(r, now_ms());
            let mut part = format!("{}:{}", i + 1, r.tag);
            if r.id == record.id {
                part.push('*');
            }
            // Running-ish states are the expected background; anything else
            // (a reported wait, a death, a broken worker) is worth seeing
            // while attached. The same rule `workspace_summary_regions`
            // mirrors for the click map.
            if !matches!(state, "running" | "working" | "active" | "quiet") {
                part.push_str(&format!("({state})"));
            }
            part
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Makes plain status-bar data safe to interpolate into terminal output.
/// Session records and transient errors can contain arbitrary persisted or
/// remote text; C0/C1 controls (including ESC, BEL, CR, and LF) must never be
/// allowed to become terminal instructions when the bar is drawn.
fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

fn terminal_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Pads or truncates to exactly `cols` terminal display cells without
/// splitting an extended grapheme cluster. This keeps wide glyphs, combining
/// sequences, and emoji aligned while the reverse-video bar spans the full
/// terminal width like tmux's own.
fn pad_or_truncate(text: &str, cols: usize) -> String {
    let cols = cols.max(1);
    let mut rendered = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = terminal_display_width(grapheme);
        if grapheme_width > cols.saturating_sub(width) {
            break;
        }
        rendered.push_str(grapheme);
        width += grapheme_width;
    }
    rendered.push_str(&" ".repeat(cols - width));
    rendered
}

/// Everything a status-bar redraw needs, cloned into each thread that might
/// trigger one (status thread, input thread on a switch flash, main loop
/// after a switch) instead of five loose `Arc` parameters -- see
/// docs/fast-session-switching-design.md section 3. `record` is shared and
/// swappable so an in-process switch is visible to the bar without
/// respawning the thread; `flash` is a transient error line (switch
/// failures); `last_drawn` backs the dirty-check in `draw_status_bar`.
#[derive(Clone)]
struct StatusBarCtx {
    stdout: Arc<Mutex<io::Stdout>>,
    term: Arc<Mutex<TermGeom>>,
    paths: Paths,
    record: Arc<Mutex<SessionRecord>>,
    flash: Arc<Mutex<Option<(String, Instant)>>>,
    /// (text, rows, cols, workload margins) last actually written, so an
    /// unchanged bar isn't rewritten every debounce tick -- see
    /// `draw_status_bar`'s doc comment and
    /// docs/low-bandwidth-remote-access-design.md section 2.1.
    last_drawn: Arc<Mutex<LastDrawnStatus>>,
    /// The client's own live model of the *workload's* screen, fed every PTY
    /// byte this client writes to the terminal (including the attach
    /// snapshot, which is a full repaint of that screen per
    /// docs/terminal-state-design.md section 6.2). It answers the three
    /// questions a status-bar redraw has to answer before it may write
    /// anything at all:
    ///
    /// - *May I write here?* -- `at_escape_boundary()`. The relayed stream
    ///   must be between complete escape sequences and complete characters.
    ///   A PTY read boundary is not one of those by construction, which is
    ///   how the redraw used to land inside a workload's half-emitted
    ///   `\x1b[38;5;` and turn its remaining parameter bytes into literal
    ///   text (issue #5).
    /// - *Where do I put the cursor back?* -- `cursor_restore()`. Absolutely,
    ///   from the model, instead of through the single shared DECSC register
    ///   the workload also owns.
    /// - *Which scroll region should be in force?* -- `margins()`, the same
    ///   distinction the previous `MarginTracker`-only field existed for:
    ///   re-asserting `\x1b[1;{rows-1}r` unconditionally destroys a
    ///   workload's own sub-range, including the one the attach snapshot just
    ///   restored.
    screen: Arc<Mutex<aplexer::screen::ClientScreen>>,
    /// Set when a redraw was wanted but the stream was not at a safe boundary
    /// (or was inside a synchronized-output frame). The main frame loop
    /// flushes it at the first boundary that is safe, so deferring never
    /// means dropping.
    pending: Arc<AtomicBool>,
    /// Set when `Ctrl-b r` wanted a full live-screen repaint but the stream
    /// was not at a safe boundary. Flushed by the main frame loop the same
    /// way as `pending`; a successful refresh also redraws the status bar,
    /// so it subsumes a pending bar redraw.
    pending_refresh: Arc<AtomicBool>,
    /// The physical geometry a terminal resize wanted to reserve a row out
    /// of, parked here because the relayed stream was mid-escape-sequence
    /// when the resize poller fired (issue #14). Flushed by
    /// `flush_pending_layout` from both the frame loop and the status
    /// thread, so a deferred resize is delivered late, never dropped.
    pending_layout: Arc<Mutex<Option<PendingLayout>>>,
    /// When the current synchronized-output deferral started, so
    /// `STATUS_BAR_SYNC_DEFER_LIMIT` can bound it.
    sync_deferred_since: Arc<Mutex<Option<Instant>>>,
    /// Scroll mode (`Ctrl-b [`, or a wheel roll): whether the pager is up
    /// and where in the retained history it is looking. Read by the relay on
    /// every chunk to decide whether the host may be written to at all.
    scroll: Arc<ScrollMode>,
    /// The which-key overlay: whether the `Ctrl-b` keymap is currently drawn
    /// over the screen. Read by the relay on every chunk for the same reason
    /// `scroll` is -- while a modal owns the host, the model keeps eating
    /// bytes and the terminal is written nothing.
    overlay: Arc<KeyOverlay>,
    /// Who currently owns mouse reporting on the host: `Some(true)` this
    /// client (so the wheel reaches `a`), `Some(false)` the workload,
    /// `None` nothing asserted yet. See `sync_client_mouse`.
    mouse_owned: Arc<Mutex<Option<bool>>>,
    /// Whether borrowing the mouse is permitted at all (`APLEXER_MOUSE`).
    mouse_capture: bool,
}

type LastDrawnStatus = Option<(String, u16, u16, Option<(u16, u16)>)>;

/// How long a transient status-bar message (switch failure, attach hint,
/// `Ctrl-b ?` help) stays visible before the normal text resumes
/// (docs/fast-session-switching-design.md section 6.1). Three seconds
/// rather than two: help text has to be readable, not merely noticed.
const FLASH_DURATION: Duration = Duration::from_secs(3);

/// One attach-mode chord, as every rendering of it needs it.
///
/// The keymap is defined **once**, here. Three things render it -- the
/// `Ctrl-b ?` status-bar flash (`attach_key_help`, from `brief`), the
/// `a keys`/`a hotkeys` listing (`cmd_hotkeys`, from `keys` + `description`)
/// and the which-key overlay a held `Ctrl-b` raises (`key_overlay_lines`,
/// from the same two) -- and none of them holds a string of its own, so a
/// binding can no longer be changed in the scanner and updated in only some
/// of the places that document it. (It used to be two hand-maintained lists
/// with a comment asking future editors to keep them in sync.) Anything else
/// that has to show the keymap reads this table too rather than adding
/// another copy; if it needs something the table does not carry, the field
/// belongs here.
struct AttachBinding {
    /// The keys, as the `a keys` listing's left column shows them.
    keys: &'static str,
    /// `key label` for the one-line status-bar flash, which has a terminal
    /// width to live inside; `None` keeps a binding out of that line only.
    /// Order here is the order shown, and the flash is truncated from the
    /// right, so the entries most worth seeing on an 80-column terminal come
    /// first.
    brief: Option<&'static str>,
    /// The sentence `a keys` prints.
    description: &'static str,
}

const ATTACH_BINDINGS: &[AttachBinding] = &[
    AttachBinding {
        keys: "Right / Left",
        brief: Some("←/→ session"),
        description: "next / previous session in this workspace",
    },
    AttachBinding {
        keys: "Down / Up",
        brief: Some("↑/↓ workspace"),
        description: "next / previous workspace (at its most recent session)",
    },
    AttachBinding {
        keys: "n",
        brief: Some("n new"),
        description: "create another session in this workspace and switch to it",
    },
    AttachBinding {
        keys: "d",
        brief: Some("d detach"),
        description: "detach (the workload keeps running)",
    },
    AttachBinding {
        keys: "[",
        brief: Some("[ scroll"),
        description: "scroll back through this session's output (i types, q or Esc leaves)",
    },
    AttachBinding {
        keys: "N / P",
        brief: Some("N/P global"),
        description: "next / previous session across all workspaces",
    },
    AttachBinding {
        keys: "1-9",
        brief: Some("1-9 jump"),
        description: "jump to the numbered session in the status bar",
    },
    AttachBinding {
        keys: "l",
        brief: Some("l last"),
        description: "return to the previously attached session",
    },
    AttachBinding {
        keys: "r",
        brief: Some("r redraw"),
        description: "redraw the live screen (recover a garbled display)",
    },
    AttachBinding {
        keys: "?",
        brief: Some("? help"),
        description: "show this reference in the status bar",
    },
];

/// The one-line key reference `Ctrl-b ?` flashes onto the status bar --
/// the same chords `a keys`/`a hotkeys` print, compressed to what fits a
/// terminal line (and truncated by the bar renderer when it does not).
/// Consumed locally: no byte reaches the workload.
fn attach_key_help() -> String {
    let brief: Vec<&str> = ATTACH_BINDINGS.iter().filter_map(|b| b.brief).collect();
    format!("Ctrl-b: {}", brief.join(" · "))
}

/// Shows a transient message on the status bar and redraws immediately --
/// the single channel for attach hints, help, and switch failures, so
/// nothing is ever printed into the workload's output stream (the original
/// attach banner's corruption failure mode, docs/terminal-state-design.md
/// section 6.3 step 6).
fn flash_status(ctx: &StatusBarCtx, message: impl Into<String>) {
    if let Ok(mut flash) = ctx.flash.lock() {
        *flash = Some((message.into(), Instant::now()));
    }
    draw_status_bar(ctx, true);
}

/// Status-bar text, adaptive by width. All layouts lead with identity and
/// state -- the two things a returning human needs -- and drop detail from
/// the right as the terminal narrows: full (workspace:tag, state, detected
/// agent, engine/foreground, memory, sibling list, help affordance), medium
/// (tag-first), compact (tag + state + detected agent + help), and a minimum
/// that keeps state and `^b ?` alive on even a few columns. Renders a flashed
/// message instead of all of these while one is active (section 6.1).
fn status_bar_text(ctx: &StatusBarCtx, cols: usize) -> String {
    {
        let mut flash = ctx.flash.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((msg, at)) = flash.clone() {
            if at.elapsed() < FLASH_DURATION {
                return pad_or_truncate(&sanitize_terminal_text(&format!("[{msg}]")), cols);
            }
            *flash = None;
        }
    }
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let home = env::var_os("HOME").map(PathBuf::from);
    let ws = display_workspace(&record.workspace, home.as_deref());
    let mut ep = match &record.profile {
        Some(p) => format!("{}/{}", record.engine, p),
        None => record.engine.clone(),
    };
    let raw = live_status(&record);
    // Which agent is live in this session right now -- the same query-time
    // detection every JSON surface carries (`api::record_agent`): one walk
    // of this session's own, shallow process tree per bar refresh, cheap
    // next to the Status round-trip the bar already pays. When it names the
    // same program as the live foreground read, the foreground annotation
    // steps aside -- `claude  shell -> claude` would say claude twice -- so
    // an agent not in the foreground (claude running, vim in front) shows
    // both facts: `claude  shell -> vim`.
    let agent = extra_agent_label(&record, aplexer::api::record_agent(&record));
    let foreground = raw
        .as_ref()
        .and_then(|raw| foreground_override(&record, raw))
        .filter(|fg| Some(fg.as_str()) != agent);
    if let Some(fg) = foreground {
        ep.push_str(&format!(" -> {fg}"));
    }
    let agent_segment = agent.map(|name| format!("  {name}")).unwrap_or_default();
    let mem = raw.as_ref().and_then(|raw| memory_indicator(&record, raw));
    let siblings = workspace_summary(ctx, &record);
    let state_record = overlay_reported_state(&record, raw.as_ref());
    let now = now_ms();
    let (state_word, _) = session_ui_state(&state_record, now);
    let (glyph, _) = state_glyph(state_word);
    // Agent-busy states animate: the static dot is replaced by the current
    // braille frame, and the bar starts moving (see the status thread's
    // animation tick, which is what makes redraws actually happen at the
    // frame rate even when the PTY itself is quiet).
    let glyph = match spinner_frame(state_word, now) {
        Some(frame) => frame.to_string(),
        None => glyph.to_string(),
    };
    let state = format!("{glyph} {}", state_word.to_uppercase());

    let mut full = format!("{ws}:{}  {state}{agent_segment}  {ep}", record.tag);
    if let Some(mem) = &mem {
        full.push_str(&format!("  mem {mem}"));
    }
    if !siblings.is_empty() {
        full.push_str("  |  ");
        full.push_str(&siblings);
    }
    full.push_str("  |  ^b ?");

    let mut medium = format!("{}  {state}{agent_segment}  {ep}", record.tag);
    if !siblings.is_empty() {
        medium.push_str("  |  ");
        medium.push_str(&siblings);
    }
    medium.push_str("  |  ^b ?");

    let compact = format!("{}  {state}{agent_segment}  ^b ?", record.tag);
    let minimum = format!("{state}  ^b ?");

    let rendered = [full, medium, compact]
        .into_iter()
        .map(|candidate| sanitize_terminal_text(&candidate))
        .find(|candidate| terminal_display_width(candidate) <= cols)
        .unwrap_or_else(|| sanitize_terminal_text(&minimum));
    pad_or_truncate(&rendered, cols)
}

/// Redraws the reserved bottom row in place: jump to the last row, clear it,
/// draw the (reverse-video, full-width) status line, and put the workload's
/// cursor and pen back absolutely from the client's own screen model. No-ops
/// when the current terminal is too small to have a reserved row.
///
/// Four properties, each load-bearing:
///
/// - **Only writes at a safe boundary.** The relayed stream must be between
///   complete escape sequences and complete characters
///   (`ClientScreen::at_escape_boundary`), and preferably not inside a
///   workload's synchronized-output frame (`sync_defer`). A PTY read boundary
///   is neither of those by construction: measured on a real `a attach`
///   against a continuously-streaming full-screen TUI, 5 of 10 redraws landed
///   inside an unterminated CSI sequence, whose remaining parameter bytes the
///   host then printed as literal text into the workload's frame. When the
///   stream is not safe the redraw is *deferred*, not dropped -- `ctx.pending`
///   is flushed by the main frame loop at the next boundary.
/// - **Never touches the shared save-cursor register.** See
///   `status_bar_sequence`.
/// - **Dirty-checked**: skips the write entirely when the rendered text and
///   geometry are byte-identical to the last actual write (`ctx.last_drawn`).
///   An idle session's bar is naturally quantized (memory rounds to whole
///   units, sibling states rarely change), so this removes nearly all idle
///   redraw chatter with no behavior change when something *did* change.
///   See docs/low-bandwidth-remote-access-design.md section 2.1.
/// - **Defensively reasserts the DECSTBM scroll region** every time it
///   actually writes. A full-screen TUI switching to the alternate screen
///   buffer, or resetting margins itself before laying out its own UI, can
///   silently undo the reservation outside our control; the resize-poll
///   thread only reapplies it when the physical terminal *size* changes, so
///   a clobbered margin would otherwise stay clobbered for the rest of the
///   attach. Reasserting it here means the reservation self-heals within one
///   redraw cycle instead of being lost permanently. Which region gets
///   reasserted is `ClientScreen::margins`-aware -- see `status_bar_sequence`
///   for why reasserting `1;{rows-1}` unconditionally is a bug, reproduced
///   directly as a workload holding `\x1b[5;15r` rendering
///   `SCROLLER-70M-ROW-16` over its own fixed row 16.
///
/// `force`: bypass the dirty-check and write unconditionally. The
/// dirty-check alone would let a *clobbered margin* go unrepaired
/// indefinitely during a long idle stretch where the bar's *text* never
/// changes (nothing to detect); callers that need the margin-defense
/// guarantee to actually bound in time -- the status thread's own
/// `STATUS_BAR_MAX_INTERVAL` forced tick, and every switch/flash redraw,
/// which are already low-frequency, user-triggered events where bandwidth
/// isn't the concern -- pass `true`. `force` does **not** bypass the boundary
/// gate: nothing does, because writing at an unsafe point is the bug.
///
/// Returns whether a real write to the terminal happened (`false` when the
/// reserved row doesn't exist, the redraw was deferred, or the dirty-check
/// skipped an unchanged redraw). Callers that drive
/// `STATUS_BAR_MAX_INTERVAL`'s overdue timer must only reset it on `true` --
/// resetting on a dirty-check no-op would let a workload with
/// frequent-but-unchanging redraws (a spinner, streamed tokens with pauses)
/// keep the timer perpetually "recently fired" without ever actually
/// rewriting a margin a full-screen erase clobbered, breaking the self-heal
/// guarantee this constant exists for.
fn draw_status_bar(ctx: &StatusBarCtx, force: bool) -> bool {
    // The stdout lock is taken *before* `status_bar_redraw` consults the
    // client's terminal model, and held across the write. Checking the escape
    // boundary and then writing without the lock is a race the frame loop
    // wins about 10% of the time (measured: 4 of 39 redraws in a real capture
    // still landed mid-CSI) -- it writes another chunk in between, and the
    // "safe" answer the status thread got is stale by the time its bytes go
    // out. See `write_locked` for the lock order this relies on.
    // Cheap pre-gate, before anything is rendered. The main frame loop calls
    // this after *every* PTY chunk while a redraw is pending, and rendering
    // the bar text reads session records off disk -- doing that per chunk
    // under a streaming workload is a throughput cliff. The authoritative
    // check is the one inside `status_bar_redraw_locked`, which runs under
    // the stdout lock; this one only avoids the work when the answer is
    // already known to be "not here".
    {
        let (at_boundary, in_sync) = {
            let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
            (screen.at_escape_boundary(), screen.in_synchronized_update())
        };
        if !at_boundary || sync_defer(ctx, in_sync) {
            ctx.pending.store(true, Ordering::Relaxed);
            return false;
        }
    }
    let Some((geom, text)) = status_bar_render(ctx) else {
        return false;
    };
    let mut out = ctx
        .stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match status_bar_redraw_locked(ctx, geom, &text, force) {
        Some(seq) => {
            // `status_bar_redraw_locked` already refused at an unsafe
            // boundary, so this can only say no if the stream moved under a
            // lock nothing else can hold -- but the funnel is where the
            // guarantee lives, not in each caller remembering, so the
            // deferral is re-armed rather than assumed impossible.
            if write_client_locked(&mut *out, &ctx.screen, &seq, BoundaryPolicy::Defer) {
                true
            } else {
                ctx.pending.store(true, Ordering::Relaxed);
                false
            }
        }
        None => false,
    }
}

/// Repaint the host terminal from the client's live screen model (`Ctrl-b r`).
///
/// This is the recovery for a garbled display: native scrollback mixed with
/// the pre-attach `a` list, a status-bar injection that the inner TUI did
/// not expect, a missed alt-screen frame. It writes the same snapshot
/// attach uses -- current grid, cursor, input modes -- then redraws the
/// status bar, whose reserved row the snapshot's ED2 just blanked.
///
/// Same boundary rules as `draw_status_bar`: never splice into a half-
/// emitted CSI. Deferring sets `pending_refresh`, which the main frame loop
/// flushes at the next safe chunk.
fn redraw_live_screen(ctx: &StatusBarCtx) -> bool {
    {
        let (at_boundary, in_sync) = {
            let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
            (screen.at_escape_boundary(), screen.in_synchronized_update())
        };
        if !at_boundary || sync_defer(ctx, in_sync) {
            ctx.pending_refresh.store(true, Ordering::Relaxed);
            return false;
        }
    }
    let mut out = ctx
        .stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match live_screen_refresh_locked(ctx) {
        Some(seq) => {
            if write_client_locked(&mut *out, &ctx.screen, &seq, BoundaryPolicy::Defer) {
                true
            } else {
                ctx.pending_refresh.store(true, Ordering::Relaxed);
                false
            }
        }
        None => false,
    }
}

/// Snapshot plus a forced status-bar sequence, or `None` when the stream is
/// not at a safe boundary (in which case `pending_refresh` is set).
fn live_screen_refresh_locked(ctx: &StatusBarCtx) -> Option<Vec<u8>> {
    let (at_boundary, in_sync, snapshot) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (
            screen.at_escape_boundary(),
            screen.in_synchronized_update(),
            screen.snapshot(),
        )
    };
    if !at_boundary || sync_defer(ctx, in_sync) {
        ctx.pending_refresh.store(true, Ordering::Relaxed);
        return None;
    }
    ctx.pending_refresh.store(false, Ordering::Relaxed);
    let snapshot = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        screen.filter_host(&snapshot).unwrap_or(snapshot)
    };
    let mut seq = snapshot;
    if let Some((geom, text)) = status_bar_render(ctx) {
        if let Some(bar) = status_bar_redraw_locked(ctx, geom, &text, true) {
            seq.extend_from_slice(&bar);
        }
    }
    Some(seq)
}

/// Geometry plus the rendered bar text, or `None` when the terminal has no
/// reserved row. Deliberately computed *before* the stdout lock is taken:
/// `status_bar_text` reads session records off disk, and the PTY relay must
/// not block behind that.
fn status_bar_render(ctx: &StatusBarCtx) -> Option<(TermGeom, String)> {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return None,
    };
    if !geom.reserved {
        return None;
    }
    let text = status_bar_text(ctx, geom.cols as usize);
    Some((geom, text))
}

/// `status_bar_render` + `status_bar_redraw_locked`, for tests and for
/// callers with no concurrent writer.
#[cfg(test)]
fn status_bar_redraw(ctx: &StatusBarCtx, force: bool) -> Option<Vec<u8>> {
    let (geom, text) = status_bar_render(ctx)?;
    status_bar_redraw_locked(ctx, geom, &text, force)
}

/// `draw_status_bar` minus the write: every gate (reserved row, escape
/// boundary, synchronized-output deferral, dirty check) and the exact bytes
/// that would go to the terminal, or `None` when nothing should be written.
///
/// Split out so tests can drive the real decision path and feed the real
/// bytes through a real `vt100` host terminal, without redirecting the
/// process's fd 1 out from under a concurrently-running test harness.
fn status_bar_redraw_locked(
    ctx: &StatusBarCtx,
    geom: TermGeom,
    text: &str,
    force: bool,
) -> Option<Vec<u8>> {
    // -- Boundary gate, before anything is rendered or written --------------
    //
    // The client is a raw byte relay, so a PTY read boundary lands at an
    // arbitrary offset in the workload's output: "between two chunks" is not
    // "between two escape sequences". Writing anywhere else splices our
    // `\x1b...` into the middle of the workload's half-emitted sequence (or
    // its half-emitted UTF-8 character); the host terminal abandons the
    // partial sequence and prints its remaining parameter bytes as literal
    // text into the workload's own frame. That is the reported corruption,
    // and it is not fixable by re-timing -- only by asking the stream.
    //
    // Deferring is never dropping: `ctx.pending` is flushed by the main frame
    // loop at the first safe boundary, which is at most one PTY chunk away.
    let (at_boundary, in_sync, restore, workload_margins) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (
            screen.at_escape_boundary(),
            screen.in_synchronized_update(),
            screen.cursor_restore(),
            screen.margins(),
        )
    };
    if !at_boundary || sync_defer(ctx, in_sync) {
        ctx.pending.store(true, Ordering::Relaxed);
        return None;
    }
    {
        let mut last = ctx
            .last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (text.to_string(), geom.rows, geom.cols, workload_margins);
        if !force && last.as_ref() == Some(&key) {
            ctx.pending.store(false, Ordering::Relaxed);
            return None;
        }
        *last = Some(key);
    }
    ctx.pending.store(false, Ordering::Relaxed);
    Some(status_bar_sequence(geom, text, workload_margins, &restore))
}

/// Whether a redraw should be held back because the workload is part-way
/// through a synchronized-output frame, bounded by
/// `STATUS_BAR_SYNC_DEFER_LIMIT` so an unclosed block cannot freeze the bar.
fn sync_defer(ctx: &StatusBarCtx, in_sync: bool) -> bool {
    let mut since = ctx
        .sync_deferred_since
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !in_sync {
        *since = None;
        return false;
    }
    match *since {
        Some(started) => started.elapsed() < STATUS_BAR_SYNC_DEFER_LIMIT,
        None => {
            *since = Some(Instant::now());
            true
        }
    }
}

/// The exact bytes a status-bar redraw writes. Split out from
/// `draw_status_bar` so a test can drive the real sequence through a real
/// `vt100` host terminal rather than assert on substrings of it.
///
/// There is deliberately **no `\x1b7`/`\x1b8` (DECSC/DECRC) bracket** here any
/// more, and none anywhere else in the client. A terminal has exactly one
/// save-cursor register. Saving into it from a stream we are only relaying
/// silently destroys whatever the workload put there, and the workload's own
/// later `\x1b8` then restores to *our* saved position -- text landing on the
/// wrong row, which is the superimposed-frames half of issue #5. Claude Code
/// opens with exactly that idiom (`\x1b7\x1b[r\x1b8`), opencode uses the same
/// register through `CSI s`/`CSI u`, and every `tput sc`-style progress line
/// run inside a session does too. The register is the workload's; the client
/// restores absolutely from its own model instead (`restore`, from
/// `ClientScreen::cursor_restore`), which also restores the workload's SGR pen
/// -- something DECRC only gives back on terminals whose DECSC saves
/// attributes, and `vt100` (the model aplexer itself runs) is not one.
///
/// `\x1b[?25l` first so the cursor does not visibly hop to the bar row and
/// back; `restore` ends with the workload's own cursor visibility, so the
/// hide is undone exactly as the workload wants it.
///
/// The scroll region re-asserted is the workload's own sub-range when it has
/// one, otherwise the bar's `1;{rows-1}` reservation. Re-asserting
/// `1;{rows-1}` unconditionally destroys a margin-using TUI's region --
/// including the one the attach snapshot just restored
/// (docs/terminal-state-design.md section 6.2 step 3) -- and makes the host
/// scroll the wrong rows. DECSTBM homes the cursor as a side effect on real
/// terminals, which is precisely why the absolute restore has to come after
/// it rather than being skipped when the region is unchanged.
fn status_bar_sequence(
    geom: TermGeom,
    text: &str,
    workload_margins: Option<(u16, u16)>,
    restore: &[u8],
) -> Vec<u8> {
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b[?25l");
    seq.extend_from_slice(
        match workload_margins {
            Some((top, bottom)) => format!("\x1b[{top};{bottom}r"),
            None => format!("\x1b[1;{}r", geom.rows - 1),
        }
        .as_bytes(),
    );
    seq.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
    seq.extend_from_slice(b"\x1b[2K\x1b[7m");
    seq.extend_from_slice(text.as_bytes());
    seq.extend_from_slice(b"\x1b[0m");
    seq.extend_from_slice(restore);
    seq
}

// ---------------------------------------------------------------------------
// Scroll mode (`Ctrl-b [`, or the wheel) -- aplexer's copy-mode
// ---------------------------------------------------------------------------
//
// The problem it solves. `a attach` holds the host terminal on the alternate
// screen for the whole attach (`ATTACH_ALT_SCREEN_ENTER`) so the pre-attach
// `a` session list cannot bleed into the live view. The alternate screen has
// no scrollback, so from the host terminal there is nothing to scroll back
// *to* -- and worse, a terminal with xterm's `alternateScroll` answers a
// wheel event there by synthesizing cursor-up/down key presses and sending
// them to the workload, i.e. scrolling to read types into the user's agent.
// That translation is now off (`?1007l`), which stopped the harm and left the
// user with no way to read earlier output at all.
//
// The shape of the fix is tmux's, not a terminal's. A tmux pane's virtual
// terminal retains a scrollback grid above the visible screen, and copy-mode
// pages through that grid; tmux never asks the host for scrollback and never
// re-parses a byte log. aplexer's equivalent emulator is `ScreenTracker`,
// which the attach client already runs over every relayed byte -- it was just
// built with a scrollback length of zero. Giving the *client's* model a real
// scrollback length (`ClientScreen::try_new_with_scrollback`) makes the
// history accumulate as a side effect of the parse that was happening anyway,
// and `Screen::set_scrollback` pages it.
//
// Why the client's model and not the worker's. The worker is the tmux-faithful
// home for it -- one parse, survives detach -- but it would need a protocol
// addition to serve scrolled-back rows, and the worker parses every session
// whether or not anyone is attached, so the memory would be spent on sessions
// nobody is reading. The client pays only while attached, is already at the
// exact geometry the pager has to render at, and reaches the same "scroll
// back through what happened while I was away" outcome by priming its grid
// once from the worker's retained raw history at attach
// (`ClientScreen::seed_history`, over the `capture` RPC that already exists).
// Only the priming replay reads bytes; from then on the live model *is* the
// history.

/// Retained history depth, in lines, for an attach client's model --
/// `history-limit` in tmux, whose default this deliberately matches.
///
/// Overridable with `APLEXER_HISTORY_LIMIT`; `0` disables scroll mode's
/// history entirely (the pager then has only the current screen, and the
/// model costs exactly what it did before this feature). The value is
/// clamped against `MAX_SCROLLBACK_CELLS` at the terminal's width, so a
/// large number cannot turn into a large allocation.
fn history_limit() -> usize {
    match env::var("APLEXER_HISTORY_LIMIT") {
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .unwrap_or(aplexer::screen::DEFAULT_SCROLLBACK_LINES),
        Err(_) => aplexer::screen::DEFAULT_SCROLLBACK_LINES,
    }
}

/// How much of the worker's retained raw history is replayed into a fresh
/// client model to give it a past (`ClientScreen::seed_history`).
///
/// This used to be sized from the line limit at an assumed ~512 raw bytes per
/// rendered line, which put the default 2000-line grid at ~1 MiB. **A byte
/// budget is not a line budget**, and for the workload aplexer exists for the
/// two diverge in the direction that empties the pager: an agent CLI that has
/// been idle spends its bytes on animation, not on rows. Measured over the
/// retained history of thirteen live agent sessions, one had spent 500 KiB on
/// a spinner containing *zero* line feeds -- 18,000 absolute cursor addresses
/// and not one row of transcript. Any fixed per-line guess is one idle hour
/// away from being a budget of pure noise, so this is simply a flat budget
/// with its cost measured rather than a guess dressed as arithmetic.
///
/// A line-feed-counting budget was tried and refused by measurement: agent
/// CLIs emit many `\n` per *rendered* row (wrapped and redrawn rows), so
/// "the suffix holding 4000 line feeds" cut four sessions from ~2000 retained
/// lines to 51-245. Counting line feeds is no better a proxy for rows than
/// counting bytes is.
///
/// **Why 2 MiB.** Replaying real captures through the real seed path, at
/// 23x100 into a 2000-line grid, minimum of five runs -- retained lines, and
/// the parse those lines cost:
///
/// ```text
///                  worst session   parse (mean / worst)
///   1 MiB shipped      0 lines        16.1 / 20.3 ms
///   2 MiB             83 lines        28.7 / 38.5 ms
///   4 MiB            225 lines        55.8 / 86.0 ms
/// ```
///
/// The seed is synchronous on the attach path *and* on every `Ctrl-b Right`
/// switch, where the protocol round trip it sits beside is 0.3-6 ms at p50
/// and 13-22 ms at p95 (`attach_round_trip_latency`). 2 MiB buys every one of
/// those thirteen sessions a pager with real content in it for ~13 ms; 4 MiB
/// spends another ~27 ms on every switch anyone ever makes to take a single
/// pathological session from 83 rows of history to 225. That is not a trade
/// worth making, and 83 rows is already three and a half screens.
fn scrollback_seed_bytes() -> usize {
    (2 * 1024 * 1024).min(aplexer::DEFAULT_HISTORY_BYTES)
}

/// Whether the client may borrow mouse reporting from the host terminal.
///
/// It has to, to see a wheel event at all: the host reports the wheel only
/// while some mouse protocol is enabled, and with `?1007l` in force nothing
/// else turns a wheel roll into anything. The cost is tmux's cost with
/// `mouse on` -- while the client owns the mouse, drag-to-select needs the
/// terminal's usual Shift override -- so `APLEXER_MOUSE=off` turns the
/// borrowing off and leaves `Ctrl-b [` as the way in.
fn mouse_capture_enabled() -> bool {
    !matches!(
        env::var("APLEXER_MOUSE").as_deref(),
        Ok("off") | Ok("0") | Ok("no") | Ok("false")
    )
}

/// The client's own mouse reporting: every protocol and encoding this client
/// knows about turned off, then button press/release (`?1000h`) in SGR
/// encoding (`?1006h`).
///
/// `?1000h` rather than `?1002h`/`?1003h` deliberately: press/release is all
/// a wheel needs, and not asking for motion reports keeps the terminal from
/// streaming a report per cell of mouse movement across the socket.
const CLIENT_MOUSE_ENABLE: &[u8] =
    b"\x1b[?9l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1000h\x1b[?1006h";

/// `CAN` -- "abandon any control sequence in flight". Leads every write made
/// under `BoundaryPolicy::StreamSuspended`; see that variant's doc comment
/// for why that is what makes those writes safe without the boundary gate.
const SCROLL_CANCEL: &[u8] = b"\x18";

/// Lines a wheel notch moves, matching tmux's own three.
const WHEEL_LINES: usize = 3;

/// SGR mouse button numbers for the wheel (xterm: 64 + button index).
const MOUSE_WHEEL_UP: u32 = 64;
const MOUSE_WHEEL_DOWN: u32 = 65;

/// Shared scroll-mode state.
///
/// `active` is an atomic rather than part of the mutex because the relay
/// reads it on every chunk, under the stdout lock, purely to decide whether
/// to write. Lock order is `stdout` -> `view` -> `screen`, which extends the
/// existing `stdout` -> `term` -> `screen` order rather than crossing it:
/// nothing takes `stdout` while holding `view`.
struct ScrollMode {
    active: AtomicBool,
    /// Type-through (`i` while the pager is up): `active` stays set -- the
    /// pager keeps the reserved bar row and the scroll offset -- but the
    /// relay flows and stdin forwards to the workload, so typing has its
    /// echo and the reply is visible as it streams. Esc drops it.
    typing: AtomicBool,
    view: Mutex<ScrollView>,
}

impl ScrollMode {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            typing: AtomicBool::new(false),
            view: Mutex::new(ScrollView::default()),
        }
    }
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    fn is_typing(&self) -> bool {
        self.typing.load(Ordering::Relaxed)
    }
}

/// Where the pager is looking: `offset` lines above the live screen, out of
/// `available` retained.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollView {
    offset: usize,
    available: usize,
}

/// One navigation step, resolved against the viewport height by
/// `apply_scroll_command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollCommand {
    /// Enter the pager without moving (`Ctrl-b [`).
    Stay,
    Up(usize),
    Down(usize),
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
    /// `i`: hand the keyboard to the workload while staying in the pager
    /// (type-through; a lone `Esc` takes it back).
    TypeThrough,
    /// `q`, `Esc` or `Ctrl-C`: back to the live screen.
    Exit,
}

/// What `scroll_keys` made of the bytes at the front of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollKey {
    /// A navigation command, and how many bytes it consumed.
    Command(ScrollCommand, usize),
    /// Recognized and deliberately swallowed (a non-wheel mouse report, an
    /// unbound key). **Consumed, never forwarded** -- that is the whole
    /// point of the mode: while the pager is up, no keystroke reaches the
    /// workload (until `i` hands the keyboard over; see `TypeThrough`).
    Ignored(usize),
    /// A sequence that has begun but not finished in this buffer. The caller
    /// keeps the bytes and retries when more arrive.
    Incomplete,
}

/// Keyboard and mouse decoding for scroll mode. Pure, so the split-sequence
/// and modifier cases are unit-testable without a terminal.
///
/// Bindings follow tmux copy-mode where tmux has one and `less` elsewhere,
/// because those are the two muscle memories a user arrives with:
/// arrows/`j`/`k` by the line, PageUp/PageDown and Space/`b` by the screen,
/// `Ctrl-U`/`Ctrl-D`/`u`/`d` by the half screen, Home/`g` and End/`G` to the
/// ends, `q`/`Esc`/`Ctrl-C` back to live, and the wheel by
/// `WHEEL_LINES`.
///
/// A lone `ESC` that is the *entire* remaining buffer is read as the Escape
/// key, not as the start of a sequence that has not arrived yet. Real
/// terminals emit `ESC [ A` for an arrow key in one write, so the ambiguity
/// is only theoretically reachable, and resolving it the other way would
/// mean Escape did nothing until the user pressed another key -- much worse
/// than the rare case of a split arrow key exiting the pager.
fn scroll_keys(buf: &[u8]) -> ScrollKey {
    use ScrollCommand::*;
    let Some(&first) = buf.first() else {
        return ScrollKey::Incomplete;
    };
    if first != 0x1b {
        let command = match first {
            b'q' | b'Q' | 0x03 => Some(Exit),
            b'k' | b'y' => Some(Up(1)),
            b'j' | b'e' => Some(Down(1)),
            b' ' | b'f' | 0x06 => Some(PageDown),
            b'b' | 0x02 => Some(PageUp),
            b'u' | 0x15 => Some(HalfUp),
            b'd' | 0x04 => Some(HalfDown),
            b'g' => Some(Top),
            b'G' => Some(Bottom),
            b'i' => Some(TypeThrough),
            _ => None,
        };
        return match command {
            Some(c) => ScrollKey::Command(c, 1),
            None => ScrollKey::Ignored(1),
        };
    }
    if buf.len() == 1 {
        return ScrollKey::Command(Exit, 1);
    }
    match buf[1] {
        b'[' => {
            if buf.len() == 2 {
                return ScrollKey::Incomplete;
            }
            if buf[2] == b'<' {
                return match parse_sgr_mouse(buf) {
                    MouseParse::Complete(report, consumed) => {
                        // Wheel reports repeat on press only; the release
                        // report a terminal may pair with them is swallowed
                        // by the `Ignored` arm below, so one notch moves
                        // WHEEL_LINES exactly once.
                        match (report.button, report.press) {
                            (MOUSE_WHEEL_UP, true) => ScrollKey::Command(Up(WHEEL_LINES), consumed),
                            (MOUSE_WHEEL_DOWN, true) => {
                                ScrollKey::Command(Down(WHEEL_LINES), consumed)
                            }
                            _ => ScrollKey::Ignored(consumed),
                        }
                    }
                    MouseParse::Incomplete => ScrollKey::Incomplete,
                    MouseParse::NotMouse => ScrollKey::Ignored(1),
                };
            }
            // A generic CSI: scan to the final byte, so `\x1b[5;2~`
            // (shifted PageUp) resolves the same as `\x1b[5~`.
            let Some(end) = buf[2..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map(|i| i + 2)
            else {
                // Bounded, so a stray `ESC [` followed by a stream of digits
                // cannot buffer forever.
                return if buf.len() > 32 {
                    ScrollKey::Ignored(buf.len())
                } else {
                    ScrollKey::Incomplete
                };
            };
            let consumed = end + 1;
            let params = &buf[2..end];
            let leading: u32 = params
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(0u32, |acc, b| {
                    acc.saturating_mul(10).saturating_add(u32::from(b - b'0'))
                });
            let command = match (buf[end], leading) {
                (b'A', _) => Some(Up(1)),
                (b'B', _) => Some(Down(1)),
                (b'H', _) => Some(Top),
                (b'F', _) => Some(Bottom),
                (b'~', 1 | 7) => Some(Top),
                (b'~', 4 | 8) => Some(Bottom),
                (b'~', 5) => Some(PageUp),
                (b'~', 6) => Some(PageDown),
                _ => None,
            };
            match command {
                Some(c) => ScrollKey::Command(c, consumed),
                None => ScrollKey::Ignored(consumed),
            }
        }
        b'O' => {
            if buf.len() == 2 {
                return ScrollKey::Incomplete;
            }
            let command = match buf[2] {
                b'A' => Some(Up(1)),
                b'B' => Some(Down(1)),
                b'H' => Some(Top),
                b'F' => Some(Bottom),
                _ => None,
            };
            match command {
                Some(c) => ScrollKey::Command(c, 3),
                None => ScrollKey::Ignored(3),
            }
        }
        _ => ScrollKey::Ignored(2),
    }
}

/// The scroll-mode status bar: what mode the user is in, how far back they
/// are, and how to get out.
///
/// The position readout is tmux copy-mode's `[n/total]` in words rather than
/// brackets, because this bar is the *only* thing telling the user their
/// keystrokes are going to the pager instead of to their agent -- the single
/// question the mode has to answer at a glance. Trimmed from the right as
/// the terminal narrows, down to a minimum that keeps the word SCROLL and
/// the way out.
fn scroll_bar_text(view: ScrollView, cols: usize, alt_screen: bool) -> String {
    let position = format!("SCROLL {}/{}", view.offset, view.available);
    // An empty pager must say *why* it is empty, and must say it on an
    // ordinary 80-column terminal rather than only on a wide one. So the two
    // cases get different ladders: with history the row spends its width on
    // the navigation keys, and with none it spends the width on the reason
    // instead -- offering PgUp/PgDn for a pager that cannot move is the thing
    // that reads as a broken feature.
    let candidates: Vec<String> = if view.available == 0 {
        let why = if alt_screen {
            // A full-screen application owns the grid; `vt100` gives the
            // alternate screen no scrollback, as every real terminal does.
            "no history: the workload owns the screen"
        } else {
            // The primary-screen case, and the one the generic hint used to
            // hide: a TUI that repaints in place with absolute cursor
            // addressing never scrolls, so nothing has ever left the top of
            // the screen for the history to hold. Measured on a real opencode
            // session whose whole 4 MiB of retained history holds 81,584
            // cursor addresses and zero line feeds -- there is genuinely
            // nothing to page back to, and what the user wants is on screen.
            "no history: nothing has scrolled off this screen"
        };
        vec![
            format!("{position} · {why} · q live"),
            format!("{position} · {why}"),
            format!("{position} · no history · q live"),
            format!("{position} · no history"),
        ]
    } else {
        vec![
            format!(
                "{position} · PgUp/PgDn ↑↓ Home/End · q live · keys go here, not to the session"
            ),
            format!("{position} · PgUp/PgDn ↑↓ Home/End · q live"),
            format!("{position} · q live"),
        ]
    };
    for candidate in candidates.into_iter().chain([position.clone()]) {
        if terminal_display_width(&candidate) <= cols {
            return pad_or_truncate(&sanitize_terminal_text(&candidate), cols);
        }
    }
    pad_or_truncate(&sanitize_terminal_text("SCROLL · q"), cols)
}

/// The bar row, drawn for scroll mode: no workload cursor restore (the
/// workload's cursor is not on screen -- the pager is), and the cursor left
/// hidden.
fn scroll_bar_sequence(geom: TermGeom, text: &str) -> Vec<u8> {
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b[?25l");
    seq.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
    seq.extend_from_slice(b"\x1b[2K\x1b[7m");
    seq.extend_from_slice(text.as_bytes());
    seq.extend_from_slice(b"\x1b[0m\x1b[?25l");
    seq
}

/// Paint the pager: the model's screen as it looked `offset` lines back,
/// plus the scroll-mode bar.
///
/// Three things this does not do, each deliberate:
///
/// - It does not consult the escape boundary (`BoundaryPolicy::StreamSuspended`).
///   Nothing of the workload's is being written while the pager is up, so
///   there is no half-emitted sequence of the workload's to splice into; the
///   `SCROLL_CANCEL` prefix ends whatever the host had in flight at the
///   moment the relay was suspended.
/// - It does not leave the model scrolled. `ClientScreen::scrolled_frame`
///   applies the offset, renders, and puts the model back at the live
///   screen, so `relay`, `cursor_restore` and `snapshot` keep describing the
///   live session throughout.
/// - It re-asserts the client's own `1;{rows-1}` reservation rather than the
///   workload's margins. The pager owns the whole screen above the bar; a
///   workload sub-range is meaningless to it, and would let the frame's own
///   absolute row addressing fall outside the region.
fn paint_scroll_view(ctx: &StatusBarCtx) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let mut view = ctx
        .scroll
        .view
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (frame, alt_screen) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let (frame, offset, available) = screen.scrolled_frame(view.offset);
        view.offset = offset;
        view.available = available;
        let frame = screen.filter_host(&frame).unwrap_or(frame);
        (frame, screen.alternate_screen())
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    if geom.reserved {
        seq.extend_from_slice(format!("\x1b[1;{}r", geom.rows - 1).as_bytes());
    }
    seq.extend_from_slice(&frame);
    if geom.reserved {
        let text = if ctx.scroll.is_typing() {
            scroll_bar_typing_text(*view, geom.cols as usize)
        } else {
            scroll_bar_text(*view, geom.cols as usize, alt_screen)
        };
        seq.extend_from_slice(&scroll_bar_sequence(geom, &text));
        // Recorded against the same dirty-check `refresh_scroll_bar` reads,
        // so the status tick right after a navigation does not rewrite a row
        // this frame just drew.
        *ctx.last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((text, geom.rows, geom.cols, None));
    }
    seq.extend_from_slice(b"\x1b[?25l");
    drop(view);
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    )
}

/// Refresh just the pager's bar row, so the retained-line count stays honest
/// while the workload keeps producing output behind the pager, without
/// repainting (and flickering) a whole screen on a timer.
///
/// The write policy follows the mode. With the pager owning the screen the
/// relay is suspended, so `StreamSuspended` applies and the bytes go out
/// unconditionally. While **type-through** (`i`) has handed the keyboard back
/// the relay is streaming again, so the bar is client-originated output
/// spliced into a live stream like any other: it waits for an escape boundary
/// (`Defer`), and on a refusal arms `ctx.pending` so the frame loop retries at
/// the next chunk. `pending` doubles as the force flag here -- a deferred bar
/// must not be swallowed by the dirty check on the retry, because between the
/// deferral and the retry nothing else may have changed the text.
fn refresh_scroll_bar(ctx: &StatusBarCtx) -> bool {
    let typing = ctx.scroll.is_typing();
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    if !geom.reserved {
        return false;
    }
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let mut view = ctx
        .scroll
        .view
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (available, alt_screen) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (screen.scrollback_available(), screen.alternate_screen())
    };
    view.available = available;
    let text = if typing {
        scroll_bar_typing_text(*view, geom.cols as usize)
    } else {
        scroll_bar_text(*view, geom.cols as usize, alt_screen)
    };
    drop(view);
    // Dirty-checked against the same `last_drawn` the live bar uses, so this
    // tick is a no-op in the common case *and* still repairs the row when
    // something else wrote over it -- a `Ctrl-b ?` help flash, most
    // obviously, which would otherwise sit on the bar for the rest of the
    // time the user spends reading. A pending deferred write forces through
    // the check: the text is by assumption unchanged (that is why the check
    // would skip), and the row may have been erased in the meantime.
    let force = typing && ctx.pending.load(Ordering::Relaxed);
    {
        let mut last = ctx
            .last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (text.clone(), geom.rows, geom.cols, None);
        if !force && last.as_ref() == Some(&key) {
            return false;
        }
        *last = Some(key);
    }
    let policy = if typing {
        BoundaryPolicy::Defer
    } else {
        BoundaryPolicy::StreamSuspended
    };
    let wrote = write_client_locked(
        &mut *out,
        &ctx.screen,
        &scroll_bar_sequence(geom, &text),
        policy,
    );
    if wrote {
        ctx.pending.store(false, Ordering::Relaxed);
    } else {
        ctx.pending.store(true, Ordering::Relaxed);
    }
    wrote
}

/// The bar while type-through is active. The pager still owns the reserved
/// row and its position readout stays honest, but the mode word and the hint
/// change: the keyboard currently belongs to the session, and Esc is the way
/// back to paging.
fn scroll_bar_typing_text(view: ScrollView, cols: usize) -> String {
    let full = format!(
        "SCROLL {}/{} · TYPE — keys go to the session · Esc back to paging",
        view.offset, view.available
    );
    if full.chars().count() <= cols {
        full
    } else if cols >= 14 {
        format!("SCROLL {}/{} · TYPE", view.offset, view.available)
    } else {
        "TYPE".to_string()
    }
}

/// Enter scroll mode and run the gesture that asked for it.
///
/// `active` is flipped under the stdout lock, which is the same lock
/// `relay_to_terminal` checks it under -- so a chunk cannot be half-written
/// over the pager's first frame.
fn enter_scroll_mode(ctx: &StatusBarCtx, first: ScrollCommand) {
    {
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        if ctx.scroll.active.swap(true, Ordering::SeqCst) {
            return;
        }
        ctx.scroll.typing.store(false, Ordering::SeqCst);
        *ctx.scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ScrollView::default();
    }
    // The bar is about to say something completely different from whatever
    // the dirty-check last recorded, and will again on the way out.
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    // Before the first frame paints: for a workload that scrolls through its
    // own DECSTBM sub-ranges, the live model's history is missing exactly the
    // rows the user is opening the pager to find, and the worker's retained
    // raw tail still has them (`refresh_pager_history`). Runs with
    // `active` already set, so the relay is suspended while it works and the
    // pager's first frame goes onto a quiet host.
    refresh_pager_history(ctx);
    apply_scroll_command(ctx, first);
}

/// Leave scroll mode: repaint the host from the live model and let the relay
/// resume.
///
/// The repaint is the same snapshot `Ctrl-b r` writes -- the model has been
/// fed every byte that arrived while the pager was up, so it is current, and
/// the frames the relay declined to write are already accounted for in it.
/// The bar is rewritten in the same sequence because the snapshot's `ED2`
/// blanks the reserved row.
fn exit_scroll_mode(ctx: &StatusBarCtx) {
    if !ctx.scroll.is_active() {
        return;
    }
    paint_live_screen(ctx);
    ctx.scroll.typing.store(false, Ordering::SeqCst);
    ctx.scroll.active.store(false, Ordering::SeqCst);
}

/// Paint the host from the live model while staying in scroll mode: the
/// snapshot `Ctrl-b r` writes, plus the bar, with the relay still suspended
/// throughout. `exit_scroll_mode` runs this before dropping `active`, and
/// `enter_typing` runs it before raising `typing` -- both orderings mean a
/// relay chunk can only ever land on the view it belongs on.
fn paint_live_screen(ctx: &StatusBarCtx) {
    // Reads session records off disk; must not happen under the stdout lock.
    let bar = status_bar_render(ctx);
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let (snapshot, restore, margins) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = screen.snapshot();
        let snapshot = screen.filter_host(&snapshot).unwrap_or(snapshot);
        (snapshot, screen.cursor_restore(), screen.margins())
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    seq.extend_from_slice(&snapshot);
    if let Some((geom, text)) = bar {
        seq.extend_from_slice(&status_bar_sequence(geom, &text, margins, &restore));
    }
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    );
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

/// `i` in the pager: hand the keyboard to the workload without leaving the
/// pager -- the thing tmux copy-mode cannot do. The view repaints to the
/// live screen first (so typing has its echo and the reply is visible as it
/// streams), then `typing` rises under the stdout lock, which is the lock
/// `relay_to_terminal` checks the flag under -- the relay stays suspended
/// until the flip, so no chunk can land on the pager's view.
fn enter_typing(ctx: &StatusBarCtx) {
    if !ctx.scroll.is_active() || ctx.scroll.is_typing() {
        return;
    }
    paint_live_screen(ctx);
    {
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        ctx.scroll.typing.store(true, Ordering::SeqCst);
    }
    // The bar's wording changes with the mode; the dirty-check still holds
    // the live bar text, so rewrite the row now rather than on the next tick.
    refresh_scroll_bar(ctx);
}

/// Esc while typing: take the keyboard back for the pager at the same
/// offset. `typing` drops first, under the stdout lock, and the pager frame
/// repaints after -- a relay chunk in between can only land on the live view
/// it was headed for anyway, and is covered by the frame immediately.
fn exit_typing(ctx: &StatusBarCtx) {
    if !ctx.scroll.is_typing() {
        return;
    }
    {
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        ctx.scroll.typing.store(false, Ordering::SeqCst);
    }
    apply_scroll_command(ctx, ScrollCommand::Stay);
}

/// Resolve one navigation step against the current viewport and repaint.
///
/// `Down` past the live screen leaves scroll mode, the way tmux's copy-mode
/// does not but every pager the user has ever used does: scrolling back to
/// the bottom means "I am done reading", and having to also press `q` to get
/// the keyboard back is exactly the confusion this mode must not create.
fn apply_scroll_command(ctx: &StatusBarCtx, command: ScrollCommand) {
    let page = {
        let geom = ctx.term.lock().map(|g| *g).unwrap_or(TermGeom {
            rows: 0,
            cols: 0,
            reserved: false,
        });
        usize::from(reserved_rows(geom.rows)).max(1)
    };
    if command == ScrollCommand::Exit {
        exit_scroll_mode(ctx);
        return;
    }
    if command == ScrollCommand::TypeThrough {
        enter_typing(ctx);
        return;
    }
    let exit = {
        let mut view = ctx
            .scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (delta, downward): (isize, bool) = match command {
            ScrollCommand::Exit => unreachable!("handled above"),
            ScrollCommand::TypeThrough => unreachable!("handled above"),
            ScrollCommand::Stay => (0, false),
            ScrollCommand::Up(n) => (n as isize, false),
            ScrollCommand::Down(n) => (-(n as isize), true),
            ScrollCommand::PageUp => (page as isize, false),
            ScrollCommand::PageDown => (-(page as isize), true),
            ScrollCommand::HalfUp => ((page / 2).max(1) as isize, false),
            ScrollCommand::HalfDown => (-((page / 2).max(1) as isize), true),
            ScrollCommand::Top => (view.available as isize, false),
            ScrollCommand::Bottom => (-(view.offset as isize), true),
        };
        if downward && view.offset == 0 {
            true
        } else {
            view.offset = (view.offset as isize + delta).max(0) as usize;
            false
        }
    };
    if exit {
        exit_scroll_mode(ctx);
    } else {
        paint_scroll_view(ctx);
    }
}

/// Byte-level routing of stdin while the client owns the mouse, the pager is
/// up, or both -- the thing that guarantees a keystroke aimed at the pager
/// can never reach the workload.
///
/// Returns the bytes that may still be forwarded. In scroll mode that is
/// empty except while type-through has handed the keyboard to the workload
/// (`i`): every byte is otherwise consumed, navigation or not.
///
/// `pending` exists because a mouse report can be split across two `read()`s
/// exactly like the `Ctrl-b` prefix can. Outside scroll mode it is only ever
/// allowed to hold a buffer that has already produced the full three-byte
/// `\x1b[<` SGR introducer -- no keyboard emits that, so nothing a user
/// types can be delayed by it. A bare `ESC` or `ESC [` at the end of a chunk
/// is forwarded immediately rather than held, because holding it would make
/// the Escape key in the user's editor wait for the next keystroke.
#[derive(Default)]
struct ScrollInput {
    pending: Vec<u8>,
}

impl ScrollInput {
    fn route(&mut self, ctx: &StatusBarCtx, bytes: &[u8]) -> Vec<u8> {
        let client_mouse = matches!(
            *ctx.mouse_owned
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Some(true)
        );
        if !ctx.scroll.is_active() && !client_mouse {
            // Neither the pager nor the wheel is in play: the workload's
            // input path is exactly what it always was.
            let mut out = std::mem::take(&mut self.pending);
            out.extend_from_slice(bytes);
            return out;
        }
        self.pending.extend_from_slice(bytes);
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if ctx.scroll.is_active() {
                if ctx.scroll.is_typing() {
                    // Type-through (`i`): the keyboard belongs to the
                    // workload now, so bytes forward verbatim. Two things
                    // are still decoded, because neither is text the user
                    // could mean to type: an SGR mouse report (the client
                    // borrowed the mouse; the workload never asked for it),
                    // and a lone-ESC chunk, which takes the keyboard back
                    // for the pager. Anything else starting with ESC --
                    // arrows, Home, a sequence split across reads -- is
                    // somebody's key, not text, and forwards whole.
                    let rest = &self.pending[i..];
                    if rest[0] == 0x1b {
                        if rest.len() >= 3 && &rest[..3] == b"\x1b[<" {
                            match parse_sgr_mouse(rest) {
                                MouseParse::Complete(_, consumed) => {
                                    i += consumed;
                                    continue;
                                }
                                MouseParse::Incomplete => break,
                                MouseParse::NotMouse => {}
                            }
                        } else if rest.len() == 1 {
                            self.pending.drain(..i + 1);
                            exit_typing(ctx);
                            // Whatever else was typed into this same chunk
                            // was typed blind against a pager that is back
                            // in charge: discard it, exactly like the bytes
                            // that trail a pager Exit below.
                            self.pending.clear();
                            return out;
                        }
                    }
                    out.push(self.pending[i]);
                    i += 1;
                    continue;
                }
                match scroll_keys(&self.pending[i..]) {
                    ScrollKey::Command(command, n) => {
                        i += n;
                        // The buffer is advanced *before* the command runs,
                        // so an Exit landing mid-buffer leaves the bytes
                        // after it to be forwarded normally on the next turn
                        // of this loop rather than being swallowed with it.
                        self.pending.drain(..i);
                        i = 0;
                        apply_scroll_command(ctx, command);
                        if !ctx.scroll.is_active() {
                            // The command closed the pager. Everything left
                            // in this buffer was typed while the pager still
                            // had the keyboard, so it is discarded rather
                            // than forwarded: the user meant it for the
                            // pager, and "the rest of the keystroke you were
                            // reading with lands in your agent's prompt" is
                            // the exact failure this mode exists to prevent.
                            // Bytes from the next read() go to the workload
                            // normally.
                            self.pending.clear();
                            return out;
                        }
                    }
                    ScrollKey::Ignored(n) => i += n,
                    ScrollKey::Incomplete => break,
                }
                continue;
            }
            // Live, with the client holding the mouse: swallow mouse
            // reports (the workload never asked for them, so forwarding
            // would type escape sequences into it) and let a wheel roll up
            // open the pager, which is the gesture the user already has in
            // their fingers from tmux.
            let rest = &self.pending[i..];
            if rest.len() >= 3 && &rest[..3] == b"\x1b[<" {
                match parse_sgr_mouse(rest) {
                    MouseParse::Complete(report, consumed) => {
                        i += consumed;
                        if report.button == MOUSE_WHEEL_UP && report.press {
                            self.pending.drain(..i);
                            i = 0;
                            enter_scroll_mode(ctx, ScrollCommand::Up(WHEEL_LINES));
                        }
                    }
                    MouseParse::Incomplete => break,
                    MouseParse::NotMouse => {
                        out.push(self.pending[i]);
                        i += 1;
                    }
                }
                continue;
            }
            out.push(self.pending[i]);
            i += 1;
        }
        self.pending.drain(..i);
        out
    }
}

/// Borrow the host's mouse reporting for the client, or hand it back to the
/// workload -- whichever the workload's own state currently calls for.
///
/// **Precedence, stated deliberately: the workload wins.** A TUI that has
/// asked for mouse reporting is a TUI with panes of its own to scroll, and
/// tmux's answer -- the pane's application gets the mouse when it requested
/// it -- is the right one. So the client borrows the mouse only while the
/// workload wants none, and gives it back the moment the workload asks,
/// re-asserting the workload's exact modes with
/// `ClientScreen::workload_mouse_sequence` so the handover cannot leave the
/// terminal in a state neither side chose. While the workload holds the
/// mouse the wheel goes to it and `Ctrl-b [` is the way into the pager.
///
/// Boundary-gated like every other client injection: this is a live splice
/// into a relayed stream (the workload can flip mouse modes at any byte),
/// so it waits for a real boundary and simply tries again on the next status
/// tick if it does not get one.
fn sync_client_mouse(ctx: &StatusBarCtx) -> bool {
    if !ctx.mouse_capture {
        return false;
    }
    let (want, seq) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        if screen.workload_wants_mouse() {
            (false, screen.workload_mouse_sequence())
        } else {
            (true, CLIENT_MOUSE_ENABLE.to_vec())
        }
    };
    {
        let owned = ctx
            .mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *owned == Some(want) {
            return false;
        }
    }
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let policy = if ctx.scroll.is_active() {
        BoundaryPolicy::StreamSuspended
    } else {
        BoundaryPolicy::Defer
    };
    if write_client_locked(&mut *out, &ctx.screen, &seq, policy) {
        *ctx.mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(want);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// The key overlay -- which-key for `Ctrl-b`
// ---------------------------------------------------------------------------
//
// `Ctrl-b` on its own is a mode with nothing on screen to say so. The overlay
// makes it visible the way which-key does in an editor: hesitate on the
// prefix and the keymap appears, press a key and it is gone. It renders from
// `ATTACH_BINDINGS`, so it is a third *view* of the one keymap the scanner
// implements rather than a third copy of it -- there is no string here that
// can drift from what `Ctrl-b <key>` actually does.
//
// Four properties it has to hold, each of which decided a design point:
//
// - **Muscle memory must never see it.** It is armed on a delay
//   (`KEY_OVERLAY_DELAY`) instead of being drawn by the prefix key, so a
//   `Ctrl-b Right` typed at speed draws nothing at all -- not a frame of it.
// - **The screen underneath must come back exactly.** Dismissal does not
//   restore a saved rectangle of cells; it repaints from
//   `ClientScreen::snapshot`, the same full-model repaint `Ctrl-b r` and the
//   pager's exit already use. The model was fed every byte that arrived while
//   the box was up, so the repaint is the *live* screen -- grid, cursor, SGR
//   pen, margins, input modes -- not a photograph of the one the box covered.
// - **Nothing may paint over it while it is up.** Like the pager, the overlay
//   suspends the relay: `relay_to_terminal` reads `KeyOverlay::is_active`
//   under the stdout lock and keeps feeding the model while writing nothing.
//   That is also what makes the previous point true.
// - **It must degrade honestly.** A terminal the box cannot fit into gets the
//   one-line `Ctrl-b ?` reference flashed on the status bar instead, and
//   nothing is suspended in that case.

/// How long a lone `Ctrl-b` waits for its second key before the keymap is
/// drawn for it.
///
/// which-key's whole trick is being invisible to anyone who already knows the
/// chord, so the delay has to sit above a typed two-key sequence and below
/// the point where hesitation stops feeling answered. 350ms is comfortably
/// both: a `Ctrl-b Right` from muscle memory lands in well under 200ms and
/// never draws anything.
///
/// **Its relationship with `CHORD_ESCAPE_TIMEOUT` is exclusion, not
/// ordering.** The two deadlines are armed in different scanner states and
/// can never be armed at the same moment: this one only while a *lone*
/// `Ctrl-b` is held (`InputScanner::awaiting_key`), that one only once an
/// `ESC` has arrived after the prefix and a partial arrow chord is being
/// withheld (`InputScanner::awaiting_escape`). Two consequences worth
/// spelling out, because they are what "they cannot fight each other" means
/// here: a half-typed `Ctrl-b Left` can never pop the overlay -- by the time
/// the chord deadline exists, the overlay's own is gone -- and a bare
/// `Ctrl-b ESC` still reaches the workload after exactly
/// `CHORD_ESCAPE_TIMEOUT`, not after that plus this.
const KEY_OVERLAY_DELAY: Duration = Duration::from_millis(350);

/// Rows the box spends on things that are not bindings: its two borders and
/// the footer line.
const KEY_OVERLAY_CHROME_ROWS: usize = 3;

/// Fewer binding rows than this and the box has stopped being a reference;
/// the one-line status-bar flash says more in less space.
const KEY_OVERLAY_MIN_BINDINGS: usize = 3;

/// The narrowest description column worth drawing. Below it the rows stop
/// being sentences and become ellipses, which is again worse than the flash.
const KEY_OVERLAY_MIN_DESC: usize = 14;

/// Whether the key overlay currently owns the host terminal.
///
/// An atomic for exactly the reason `ScrollMode::active` is one: the relay
/// reads it on every chunk, under the stdout lock, purely to decide whether
/// to write.
#[derive(Default)]
struct KeyOverlay {
    active: AtomicBool,
}

impl KeyOverlay {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

/// Rows the overlay may draw into: the physical terminal less the status
/// bar's reserved row, which the box must never write over.
fn key_overlay_rows(geom: TermGeom) -> u16 {
    if geom.reserved {
        geom.rows.saturating_sub(1)
    } else {
        geom.rows
    }
}

/// Fit `text` into exactly `width` display cells, ellipsing rather than
/// silently amputating when it is too long.
///
/// `pad_or_truncate` is the right tool for the status bar, where a cut line
/// is obviously cut because it runs to the edge of the terminal. Inside a box
/// with a border on the right there is no such cue, so an over-long
/// description would read as a complete sentence that happens to be wrong.
fn fit_overlay_cell(text: &str, width: usize) -> String {
    let text = sanitize_terminal_text(text);
    if terminal_display_width(&text) <= width || width < 2 {
        return pad_or_truncate(&text, width);
    }
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let cells = terminal_display_width(grapheme);
        if used + cells > width - 1 {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out.push('\u{2026}');
    used += 1;
    out.push_str(&" ".repeat(width - used));
    out
}

/// The box, as text rows already padded to a uniform display width -- or
/// `None` when this terminal cannot hold one worth drawing, which is the
/// caller's cue to fall back to the status-bar flash.
///
/// `rows` is `key_overlay_rows`, i.e. the status row is already excluded, so
/// the box can never be laid out over the bar. Bindings come from
/// `ATTACH_BINDINGS` in table order, and that order is already "most worth
/// seeing first" (it is the order the one-line flash truncates from the right
/// of), so a short terminal trims from the end and the footer says how many
/// went.
fn key_overlay_lines(rows: usize, cols: usize) -> Option<Vec<String>> {
    let capacity = rows.checked_sub(KEY_OVERLAY_CHROME_ROWS)?;
    if capacity < KEY_OVERLAY_MIN_BINDINGS {
        return None;
    }
    let shown = ATTACH_BINDINGS.len().min(capacity);
    let hidden = ATTACH_BINDINGS.len() - shown;
    let bindings = &ATTACH_BINDINGS[..shown];

    let keys_width = bindings
        .iter()
        .map(|b| terminal_display_width(b.keys))
        .max()
        .unwrap_or(0);
    // "|" + " " + keys + "  " + description + " " + "|"
    let chrome = keys_width + 6;
    if cols < chrome + KEY_OVERLAY_MIN_DESC {
        return None;
    }
    let widest = bindings
        .iter()
        .map(|b| terminal_display_width(b.description))
        .max()
        .unwrap_or(0);
    let desc_width = widest.max(KEY_OVERLAY_MIN_DESC).min(cols - chrome);
    let width = chrome + desc_width;
    let inner = width - 2;

    let mut lines = Vec::with_capacity(shown + KEY_OVERLAY_CHROME_ROWS);
    // The title doubles as the answer to "what is this box": it names the key
    // the user just pressed and is waiting on.
    let title = " Ctrl-b ";
    let title_cells = terminal_display_width(title) + 1;
    lines.push(format!(
        "\u{250c}\u{2500}{title}{}\u{2510}",
        "\u{2500}".repeat(inner - title_cells)
    ));
    for binding in bindings {
        lines.push(format!(
            "\u{2502} {}  {} \u{2502}",
            pad_or_truncate(binding.keys, keys_width),
            fit_overlay_cell(binding.description, desc_width)
        ));
    }
    let footer = if hidden > 0 {
        format!("{hidden} more \u{b7} ? for all \u{b7} Esc dismiss")
    } else {
        "Esc dismiss \u{b7} any other key passes through".to_string()
    };
    lines.push(format!(
        "\u{2502} {} \u{2502}",
        fit_overlay_cell(&footer, inner - 2)
    ));
    lines.push(format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner)));
    Some(lines)
}

/// Position the box on the host: bottom-left, its last row immediately above
/// the status bar, which is where which-key puts it and where it covers the
/// least of what a user is usually reading.
///
/// Absolute row addressing, so the caller must have the client's own
/// full-height reservation in force -- see `paint_key_overlay`, which
/// re-asserts it for exactly this reason.
fn key_overlay_sequence(geom: TermGeom, lines: &[String]) -> Vec<u8> {
    let mut seq = Vec::new();
    let top = key_overlay_rows(geom).saturating_sub(lines.len() as u16) + 1;
    seq.extend_from_slice(b"\x1b[?25l");
    for (offset, line) in lines.iter().enumerate() {
        seq.extend_from_slice(format!("\x1b[{};1H", top + offset as u16).as_bytes());
        seq.extend_from_slice(b"\x1b[0m\x1b[7m");
        seq.extend_from_slice(line.as_bytes());
        seq.extend_from_slice(b"\x1b[0m");
    }
    seq.extend_from_slice(b"\x1b[?25l");
    seq
}

/// Paint the overlay: the live screen from the model, then the box on top of
/// it, then the ordinary status bar.
///
/// Repainting the whole screen first rather than only the box's rows is what
/// makes this idempotent, which is what lets the resize thread simply call it
/// again at the new geometry instead of having to know what the old box
/// covered.
///
/// The DECSTBM re-assertion after the snapshot mirrors `paint_scroll_view`'s
/// and is there for the same reason: the box addresses rows absolutely, and
/// the snapshot may have just restored a workload sub-range (and, with it,
/// an origin mode that would make those rows relative to it). The dismissal
/// repaint puts the workload's own region back.
fn paint_key_overlay(ctx: &StatusBarCtx) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    let Some(lines) = key_overlay_lines(key_overlay_rows(geom) as usize, geom.cols as usize) else {
        return false;
    };
    // Reads session records off disk; must not happen under the stdout lock.
    let bar = status_bar_render(ctx);
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let snapshot = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = screen.snapshot();
        screen.filter_host(&snapshot).unwrap_or(snapshot)
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    seq.extend_from_slice(&snapshot);
    if geom.reserved {
        seq.extend_from_slice(format!("\x1b[1;{}r", geom.rows - 1).as_bytes());
    }
    seq.extend_from_slice(&key_overlay_sequence(geom, &lines));
    if let Some((bar_geom, text)) = bar {
        // The pager's flavour of the bar row: no workload cursor restore, and
        // the cursor left hidden. The workload's cursor is not what the user
        // is looking at while a modal is up, and parking it inside the box
        // would just make the box look broken.
        seq.extend_from_slice(&scroll_bar_sequence(bar_geom, &text));
        *ctx.last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some((text, bar_geom.rows, bar_geom.cols, None));
    }
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    )
}

/// Put the keymap on screen for a `Ctrl-b` the user is still thinking about,
/// and suspend the relay behind it.
///
/// Returns whether the box actually went up. `false` means this terminal is
/// too small for one and the one-line reference was flashed on the status bar
/// instead -- the honest degradation, decided *before* anything is suspended
/// so the fallback path never suspends the relay at all.
fn show_key_overlay(ctx: &StatusBarCtx) -> bool {
    if ctx.scroll.is_active() {
        // The pager has its own key routing (`ScrollInput::route`) and its own
        // full-screen view; a second modal on top of it would describe keys
        // that are not the ones in force.
        return false;
    }
    let fits = match ctx.term.lock() {
        Ok(geom) => {
            key_overlay_lines(key_overlay_rows(*geom) as usize, geom.cols as usize).is_some()
        }
        Err(_) => false,
    };
    if !fits {
        flash_status(ctx, attach_key_help());
        return false;
    }
    {
        // Flipped under the stdout lock -- the same lock `relay_to_terminal`
        // reads it under -- so a workload chunk cannot be half-written across
        // the overlay's first frame. Exactly `enter_scroll_mode`'s reasoning.
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        if ctx.overlay.active.swap(true, Ordering::SeqCst) {
            return true;
        }
    }
    // The bar is about to be drawn by a writer that is not the dirty-check's
    // usual one, and again on the way out.
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    if paint_key_overlay(ctx) {
        true
    } else {
        // Never leave the relay suspended behind a box that did not get
        // drawn: the user would be looking at a frozen terminal.
        dismiss_key_overlay(ctx);
        false
    }
}

/// Take the overlay down, put the screen back, and let the relay resume.
/// Returns whether there was an overlay to take down.
///
/// The restore is `ClientScreen::snapshot` -- the same repaint `Ctrl-b r` and
/// `exit_scroll_mode` write, and deliberately not a saved rectangle of cells.
/// A rectangle would be a photograph of the screen as it was when the box
/// went up; the model has been fed every byte that arrived since, so the
/// snapshot is the screen as it *is*. The bar is rewritten in the same
/// sequence because the snapshot's `ED2` blanks its reserved row.
fn dismiss_key_overlay(ctx: &StatusBarCtx) -> bool {
    if !ctx.overlay.is_active() {
        return false;
    }
    // Reads session records off disk; must not happen under the stdout lock.
    let bar = status_bar_render(ctx);
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let (snapshot, restore, margins) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = screen.snapshot();
        let snapshot = screen.filter_host(&snapshot).unwrap_or(snapshot);
        (snapshot, screen.cursor_restore(), screen.margins())
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    seq.extend_from_slice(&snapshot);
    if let Some((geom, text)) = bar {
        seq.extend_from_slice(&status_bar_sequence(geom, &text, margins, &restore));
    }
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    );
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    ctx.overlay.active.store(false, Ordering::SeqCst);
    true
}

/// Prime a freshly built client model with a tail of the worker's retained
/// raw history, so `Ctrl-b [` has a past to page through from the first
/// second of the attach rather than only what arrives afterwards.
///
/// Failure is not an error the user should see: a worker too old to answer,
/// a session that just exited, or a history file that has been rotated all
/// mean "no history to seed", and the attach continues with an empty grid
/// that fills from the live stream.
fn seed_client_scrollback(
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    record: &SessionRecord,
) {
    if history_limit() == 0 {
        return;
    }
    let Ok(tail) = rpc_capture(record, Some(scrollback_seed_bytes())) else {
        return;
    };
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .seed_history(&tail);
}

/// Rebuild the pager's history from the worker's *current* retained tail
/// before `Ctrl-b [` (or the wheel) opens it.
///
/// The attach-time seed (`seed_client_scrollback`) only helps a client that
/// attached after the output happened. A session attached from before it --
/// `a new`, the default flow -- accumulates history through the live relay,
/// where `vt100` is right to drop every row scrolled out of a DECSTBM
/// sub-range; the codex TUI holds a sub-range almost constantly, so its pager
/// opened on `SCROLL 0/0` no matter how long it had been running. Re-running
/// the seed at entry gives the live attach the same past a fresh attach gets
/// (`ClientScreen::refresh_scrollback` for the model-side mechanics).
///
/// The gate keeps every other workload exactly as it is. `subregion_seen` is
/// false for anything that never sent a sub-range -- shells, logs, tail -f --
/// and those sessions' live-maintained history is complete, so they pay
/// neither the two bounded RPCs (capture + screen, ~13-40 ms of replay
/// measured at `scrollback_seed_bytes`) nor the risk of a byte-capped replay
/// holding fewer rows than their live scrollback already does. The alternate
/// screen skips too: that grid has no scrollback anywhere, so there is
/// nothing to rebuild for a full-screen application, only RPCs to spend.
///
/// Failure is invisible, exactly like the attach seed: a worker that has gone
/// away or stopped answering means "page through whatever the live model
/// has", which is the pre-refresh behavior.
fn refresh_pager_history(ctx: &StatusBarCtx) {
    if history_limit() == 0 {
        return;
    }
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        if screen.alternate_screen() || !screen.subregion_seen() {
            return;
        }
    }
    let Ok(tail) = rpc_capture(&record, Some(scrollback_seed_bytes())) else {
        return;
    };
    if tail.is_empty() {
        return;
    }
    let Ok(snapshot) = rpc_capture_screen(&record, false) else {
        return;
    };
    ctx.screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .refresh_scrollback(&tail, &snapshot);
}

/// Which session a `Ctrl-b` switch chord asks for
/// (docs/fast-session-switching-design.md section 3).
#[derive(Clone, Copy, Debug, PartialEq)]
enum SwitchTarget {
    /// `Ctrl-b Right`: next session in the current workspace.
    Next,
    /// `Ctrl-b Left`: previous session in the current workspace.
    Prev,
    /// `Ctrl-b Down`: the next workspace in `a list` order, entered at its
    /// most recently accessed session.
    NextWorkspace,
    /// `Ctrl-b Up`: the previous workspace, likewise.
    PrevWorkspace,
    /// `Ctrl-b N`: next session across all workspaces (`a list` order).
    NextGlobal,
    /// `Ctrl-b P`: previous session across all workspaces.
    PrevGlobal,
    /// `Ctrl-b l`: toggle back to whatever was attached before this one.
    Last,
    /// `Ctrl-b 1`..`9`: the Nth session
    /// (1-based) of the current workspace, no skipping -- must mean exactly
    /// what the status bar shows.
    Index(usize),
    /// `Ctrl-b n`: create a brand-new session in the attached session's
    /// workspace and switch to it. (Session navigation moved to the arrow
    /// keys, which is what freed `n` to mean "new".) The odd one out -- every
    /// other variant *selects* an existing session, this one *makes* the
    /// session it then selects -- which is why it is resolved by
    /// `create_sibling_session` in `perform_switch` rather than by
    /// `pick_switch_target`. Everything after resolution (establish, swap,
    /// `last` bookkeeping, failure containment) is the ordinary switch path.
    New,
}

/// True iff `check_attachable` would pass; used to skip dead sessions when
/// cycling with n/p/N/P (never for explicit `Index`/`Last` addressing,
/// which report the real error instead of silently hopping past it).
fn is_attachable(r: &SessionRecord) -> bool {
    check_attachable(r).is_ok()
}

/// Walks `group` from `current_id`'s position (or position 0 if the current
/// session isn't in this group -- e.g. it was killed underneath us) by +1
/// (`prev = false`) or -1 (`prev = true`) with wraparound, skipping
/// `current_id` itself and any candidate that fails `is_attachable`.
/// Returns `None` once every other candidate has been tried and rejected.
fn walk_group(group: &[SessionRecord], current_id: Uuid, prev: bool) -> Option<SessionRecord> {
    let len = group.len();
    if len == 0 {
        return None;
    }
    let start = group.iter().position(|r| r.id == current_id).unwrap_or(0);
    for step in 1..=len {
        let idx = if prev {
            (start + len - step) % len
        } else {
            (start + step) % len
        };
        let candidate = &group[idx];
        if candidate.id != current_id && is_attachable(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

/// The session a workspace is *entered* at by `Ctrl-b Down`/`Up`: the one
/// used most recently (`last_accessed_ms`, stamped whenever a client
/// attaches), which is the session a returning user means by "that
/// workspace". Ties and a group where nothing has ever been attached fall
/// back to `a list` order -- the first row, i.e. what the status bar
/// numbers `1`. Unattachable sessions are skipped, so a workspace whose
/// most recent session has since died is entered at its next-best one
/// rather than erroring; `None` means the whole group is dead, and the
/// caller moves on to the next workspace.
fn workspace_entry_session(group: &[SessionRecord]) -> Option<SessionRecord> {
    let mut best: Option<&SessionRecord> = None;
    for candidate in group.iter().filter(|r| is_attachable(r)) {
        let better = match best {
            // Strictly greater: on a tie the earlier (higher in `a list`)
            // row wins, so "never attached" groups enter at row 1.
            Some(current) => {
                candidate.last_accessed_ms.unwrap_or(0) > current.last_accessed_ms.unwrap_or(0)
            }
            None => true,
        };
        if better {
            best = Some(candidate);
        }
    }
    best.cloned()
}

/// Pure candidate selection over the same groups `a list` prints (see
/// `group_by_workspace`). Split from `resolve_switch_target` (the
/// paths-touching wrapper) so it is unit-testable without a filesystem.
/// Semantics are docs/fast-session-switching-design.md section 3.2:
///
/// - `Next`/`Prev`: candidates are the current session's own workspace
///   group; skips dead sessions; wraps; errors if nothing else is
///   attachable there.
/// - `NextWorkspace`/`PrevWorkspace`: candidates are whole *groups*, in that
///   same `a list` order, skipping the current one and any group with
///   nothing attachable in it; the chosen group is entered at
///   `workspace_entry_session`.
/// - `NextGlobal`/`PrevGlobal`: candidates are every group flattened in
///   `a list` workspace order (the remembered `--sort`), then list order
///   inside each group -- exactly the top-to-bottom order of `a list`.
/// - `Index(n)`: 1-based, into the current workspace group only, **no**
///   skipping of dead sessions -- the number must mean exactly what the
///   status bar shows (`workspace_summary`); an unattachable target is
///   still returned here and rejected later by `perform_switch`'s
///   `check_attachable` call, so the error names the actual session.
/// - `Last`: resolved by UUID against every group (survives renames, works
///   across workspaces).
fn pick_switch_target(
    groups: &[(PathBuf, Vec<SessionRecord>)],
    current_workspace: &Path,
    current_id: Uuid,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let current_group = || -> Result<&[SessionRecord]> {
        groups
            .iter()
            .find(|(ws, _)| ws == current_workspace)
            .map(|(_, g)| g.as_slice())
            .ok_or_else(|| anyhow!("current workspace has no sessions"))
    };
    match target {
        SwitchTarget::Next | SwitchTarget::Prev => {
            let group = current_group()?;
            walk_group(group, current_id, target == SwitchTarget::Prev)
                .ok_or_else(|| anyhow!("no other running session in this workspace"))
        }
        SwitchTarget::NextWorkspace | SwitchTarget::PrevWorkspace => {
            // Workspace-level cycling, over the same top-level order `a list`
            // prints (the remembered `--sort`). The current workspace is
            // skipped, so this is always a real move; a workspace with
            // nothing attachable left in it is stepped over rather than
            // becoming an error the user has to press through.
            let len = groups.len();
            if len == 0 {
                bail!("no sessions to switch to");
            }
            let backwards = target == SwitchTarget::PrevWorkspace;
            let start = groups
                .iter()
                .position(|(ws, _)| ws == current_workspace)
                .unwrap_or(0);
            for step in 1..=len {
                let index = if backwards {
                    (start + len - step) % len
                } else {
                    (start + step) % len
                };
                let (workspace, group) = &groups[index];
                // Comparing the path (not the index) is what makes the
                // "current workspace not in the list" case -- it was just
                // killed underneath us -- consider every group, including
                // index 0, instead of silently skipping one.
                if workspace == current_workspace {
                    continue;
                }
                if let Some(entry) = workspace_entry_session(group) {
                    return Ok(entry);
                }
            }
            bail!("no other workspace has a running session")
        }
        SwitchTarget::NextGlobal | SwitchTarget::PrevGlobal => {
            let flat: Vec<SessionRecord> = groups.iter().flat_map(|(_, g)| g.clone()).collect();
            walk_group(&flat, current_id, target == SwitchTarget::PrevGlobal)
                .ok_or_else(|| anyhow!("no other running session"))
        }
        SwitchTarget::Index(n) => {
            let group = current_group()?;
            if n < 1 || n > group.len() {
                bail!(
                    "no session {n} here: this workspace has {} session(s)",
                    group.len()
                );
            }
            Ok(group[n - 1].clone())
        }
        // Not reachable through `perform_switch`, which resolves `New` by
        // *creating* the session before it ever gets here (see the variant's
        // doc comment). Spelled out rather than folded into another arm so a
        // future caller that forgets gets a named error instead of silently
        // switching somewhere arbitrary.
        SwitchTarget::New => bail!("new-session target is created, not selected"),
        SwitchTarget::Last => {
            let id = last.ok_or_else(|| anyhow!("no previous session"))?;
            groups
                .iter()
                .flat_map(|(_, g)| g.iter())
                .find(|r| r.id == id)
                .cloned()
                .ok_or_else(|| anyhow!("previous session is gone"))
        }
    }
}

fn resolve_switch_target(
    paths: &Paths,
    current: &SessionRecord,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let groups = group_by_workspace(list_records(paths)?, load_list_sort(paths));
    pick_switch_target(&groups, &current.workspace, current.id, target, last)
}

/// How long `Ctrl-b c` waits for the new session's workload to come up before
/// giving up -- `a start`'s own `--startup-timeout-ms` default, because this
/// chord is `a new` with the CLI trip removed and must not be quietly less
/// patient than typing it.
const NEW_SESSION_STARTUP_TIMEOUT_MS: u64 = 10_000;

/// `Ctrl-b c`'s half of the chord: create another session in the attached
/// session's workspace, the way `a new` (i.e. `a start --fresh --attach`)
/// would if the user had detached to run it.
///
/// Deliberately *not* a clone of `current`: the promise is "what `a start`
/// gives me in this workspace", so engine/profile are left `None` for
/// `Config::resolve` to fill from the configured default engine (and its
/// default profile), the tag base is `DEFAULT_HUMAN_TAG` -- the same base
/// `a start`/`a new`/`a here` use -- and cwd defaults to the workspace.
/// Inheriting the attached session's engine instead would make the chord mean
/// "another one of these", which is a different (and unrequested) feature, and
/// would be surprising the moment the user is attached to a `--` command
/// session that was never meant to be spawned twice.
///
/// Tag allocation is `--fresh`'s, not a reimplementation: `start_session`
/// picks the first free `<tag>`/`<tag>-2`/`<tag>-3` … *under the registry
/// lock*, so two clients pressing `Ctrl-b c` at the same instant cannot claim
/// the same suffix, and a dead holder is still reclaimed under its own name
/// rather than skipped (tests/fresh_start.rs).
///
/// `geometry` is the host's workload-sized `(rows, cols)` (already
/// reserved-rows-adjusted, exactly what the following `establish` sends), so
/// the new worker's PTY is born at the right size and its first snapshot needs
/// no SIGWINCH repaint -- the same thing `a start --attach` does with the
/// terminal it was typed into.
///
/// Errors propagate to `perform_switch`'s caller untouched: nothing here has
/// touched the live attachment, so a bad config or an exhausted tag space is a
/// status-bar flash and the user stays exactly where they were.
fn create_sibling_session(
    paths: &Paths,
    current: &SessionRecord,
    geometry: Option<(u16, u16)>,
) -> Result<SessionRecord> {
    let request = aplexer::api::StartRequest {
        workspace: current.workspace.clone(),
        tag: DEFAULT_HUMAN_TAG.to_string(),
        engine: None,
        profile: None,
        cwd: None,
        env: BTreeMap::new(),
        command: Vec::new(),
        memory: None,
        pids: None,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: None,
        no_skip_permissions: false,
        startup_timeout_ms: NEW_SESSION_STARTUP_TIMEOUT_MS,
        worker_rows: geometry.map(|(rows, _)| rows),
        worker_cols: geometry.map(|(_, cols)| cols),
        python: None,
        // The whole point: never fail because this workspace already has a
        // `main`, take `main-2` instead.
        fresh: true,
    };
    aplexer::api::start_session(paths, &request).context("create a session in this workspace")
}

/// Result of `establish()`: the connected/subscribed stream, its initial
/// payload (either a live-screen snapshot or a raw-tail replay -- see
/// `screen`), and enough of the response to know which one it got.
struct AttachHandshake {
    reader: UnixStream,
    initial: Vec<u8>,
    /// The response's `"screen"` field: `Some(true)`/`Some(false)` from a
    /// worker new enough to report it, `None` from an old worker whose
    /// response predates the field entirely (docs/terminal-state-design.md
    /// section 6.1's compatibility matrix) -- used to decide whether the
    /// explicit post-connect Resize control send is still needed (section
    /// 6.3 step 7).
    screen: Option<bool>,
}

/// Extracted attach handshake (connect + `Operation::Attach` request +
/// response check + initial payload frame), used by both the initial
/// attach and every in-process switch (docs/fast-session-switching-design.md
/// section 3.1).
///
/// `want_screen` requests the live-screen snapshot (docs/terminal-state-design.md
/// section 6.1); `geometry`, when known (a real tty), is `(rows, cols)`
/// already reserved-rows-adjusted by the caller -- sent so the worker can
/// resize the PTY and its screen model *before* rendering the snapshot, so
/// there is no wrong-size frame followed by a SIGWINCH repaint (section
/// 6.3 step 1). An old worker's serde simply ignores these unknown request
/// fields and falls back to today's raw-tail replay -- no worse than
/// before.
fn establish(
    record: &SessionRecord,
    replay_bytes: Option<usize>,
    want_screen: bool,
    geometry: Option<(u16, u16)>,
) -> Result<AttachHandshake> {
    let mut reader = connect(record)?;
    let (rows, cols) = match geometry {
        Some((rows, cols)) => (Some(rows), Some(cols)),
        None => (None, None),
    };
    let request = Request::new(
        record.id,
        Operation::Attach {
            history_bytes: replay_bytes,
            want_screen,
            rows,
            cols,
        },
    );
    let id = request.request_id.clone();
    write_json(&mut reader, &request)?;
    let response: Response =
        frame_json(read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing attach response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    let result = response.into_result()?;
    let screen = result.get("screen").and_then(|v| v.as_bool());
    let initial = read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing history frame"))?;
    if initial.kind != FrameKind::Data {
        bail!("expected history data");
    }
    // Only the handshake is an RPC. Once subscribed, silence is a normal
    // state for an interactive terminal and must not detach the client.
    clear_streaming_deadlines(&reader)?;
    Ok(AttachHandshake {
        reader,
        initial: initial.payload,
        screen,
    })
}

/// Default amount of history replayed on an in-process switch
/// (`Ctrl-b n/p/N/P/l/1-9`), separate from `DEFAULT_ATTACH_REPLAY_BYTES`
/// used by a fresh `a attach`.
///
/// Deviation from docs/fast-session-switching-design.md section 3.1, which
/// specifies reusing the exact same replay budget as a fresh attach: a
/// switch target is a session the user was just attached to, or explicitly
/// picked off the status bar's own sibling list -- not a cold, unfamiliar
/// session -- so the "show what's currently on its screen" justification
/// for a full 32KB tail is considerably weaker than on first attach, and
/// every byte here sits on the hot path the user actually experiences as
/// switch latency (it's written to the real terminal, and the terminal
/// emulator parsing/painting it dominates the whole switch -- see the
/// design doc's own section 8 budget). 4KB is still ~2-4 screens of typical
/// tail -- comfortably enough for a shell prompt or an agent CLI's last few
/// lines -- at 1/8th the bytes and, per informal measurement during
/// implementation, a visibly snappier repaint than 32KB on a small
/// terminal. An explicit `--history-bytes` from the CLI is still honored
/// (see `switch_replay_bytes` in `attach()`) -- this only changes the
/// *default*, the same way `DEFAULT_ATTACH_REPLAY_BYTES` is only a default.
const SWITCH_REPLAY_BYTES: usize = 4 * 1024;

/// A fully established connection to the new session, handed from the
/// input thread (which runs `perform_switch`) to the main frame loop
/// (which installs it -- see the `'session` loop in `attach()`).
struct SwitchOutcome {
    record: SessionRecord,
    /// Attach handshake already completed on this socket.
    reader: UnixStream,
    /// The replay tail read during that handshake.
    history: Vec<u8>,
}

/// One button press/release reported by xterm's SGR extended mouse mode
/// (`CSI ?1006h`, paired with `CSI ?1000h` click tracking) --
/// docs/clickable-status-bar-design.md section 2. `col`/`row` are 1-based,
/// matching the wire format, so callers subtract 1 to index into
/// `BarRegion` column ranges or compare against `TermGeom.rows`.
///
/// **Not yet wired into `InputScanner`/`attach()`** -- see the design doc
/// section 7 for why this is landing as a standalone, unit-tested primitive
/// ahead of the riskier live-input-thread integration.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseReport {
    button: u32,
    press: bool,
    col: u16,
    row: u16,
}

/// Result of attempting to parse an SGR mouse report off the front of a
/// buffer: a real hit (with the byte length consumed), "not this at all"
/// (any other byte sequence, including ordinary CSI sequences like arrow
/// keys -- `ESC [ <` is not a prefix any keyboard-generated input or other
/// terminal report uses, so this is an unambiguous, fast rejection), or
/// "looks like the start of one but the buffer ends before `M`/`m`" -- the
/// signal a live scanner needs to keep buffering across `read()` calls, the
/// same role `pending_ctrl_b` plays for the one-byte `Ctrl-b` prefix.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseParse {
    NotMouse,
    Incomplete,
    Complete(MouseReport, usize),
}

/// Pure parser for `ESC [ < Cb ; Cx ; Cy [Mm]` at the start of `buf`
/// (docs/clickable-status-bar-design.md section 2/4.4). Never panics on
/// malformed input; malformed-but-prefix-matching input that can't
/// possibly resolve (non-digit where a number is expected, once the `<`
/// has been seen) is reported `NotMouse` rather than `Incomplete`, so a
/// caller doesn't buffer forever waiting for a `M`/`m` that will never
/// come.
#[allow(dead_code)]
fn parse_sgr_mouse(buf: &[u8]) -> MouseParse {
    const PREFIX: &[u8] = b"\x1b[<";
    if buf.len() < PREFIX.len() {
        if PREFIX.starts_with(buf) {
            return MouseParse::Incomplete;
        }
        return MouseParse::NotMouse;
    }
    if &buf[..PREFIX.len()] != PREFIX {
        return MouseParse::NotMouse;
    }
    // Three ';'-separated decimal fields, terminated by 'M' (press) or 'm'
    // (release). Parse by scanning for the terminator rather than
    // pre-splitting, so a genuinely truncated buffer (no terminator yet)
    // is correctly reported Incomplete instead of NotMouse.
    let rest = &buf[PREFIX.len()..];
    let mut fields: [u32; 3] = [0; 3];
    let mut field_idx = 0;
    let mut cur: u32 = 0;
    let mut have_digit = false;
    for (i, &b) in rest.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                have_digit = true;
                cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
            }
            b';' => {
                if !have_digit || field_idx >= 2 {
                    return MouseParse::NotMouse;
                }
                fields[field_idx] = cur;
                field_idx += 1;
                cur = 0;
                have_digit = false;
            }
            b'M' | b'm' => {
                if !have_digit || field_idx != 2 {
                    return MouseParse::NotMouse;
                }
                fields[2] = cur;
                let consumed = PREFIX.len() + i + 1;
                let row = u16::try_from(fields[2]).unwrap_or(u16::MAX);
                let col = u16::try_from(fields[1]).unwrap_or(u16::MAX);
                return MouseParse::Complete(
                    MouseReport {
                        button: fields[0],
                        press: b == b'M',
                        col,
                        row,
                    },
                    consumed,
                );
            }
            _ => return MouseParse::NotMouse,
        }
    }
    // Ran out of buffer with no terminator yet, but every byte seen so far
    // was a valid digit/`;` -- genuinely incomplete, keep buffering.
    MouseParse::Incomplete
}

/// A clickable span of the rendered status-bar line
/// (docs/clickable-status-bar-design.md section 4.1), in 0-based display-cell
/// columns `[start, end)` -- the same units `pad_or_truncate` counts in, so
/// a click's 1-based `Cx` maps in with a single `- 1`.
///
/// **Not yet wired into `draw_status_bar`** -- see the design doc section 7.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct BarRegion {
    cols: std::ops::Range<usize>,
    action: BarClick,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum BarClick {
    /// Click a sibling's `{i}:{tag}` token: switch to it (`i` is exactly
    /// the digit `Ctrl-b <i>` would send, `SwitchTarget::Index`).
    Sibling(usize),
    /// Click the cross-workspace picker indicator (design doc section 5.2).
    WorkspacePicker,
    /// Click a session while browsing another workspace's sibling list
    /// (design doc section 5.2/5.3): jump straight to it by identity.
    RemoteSession(Uuid),
}

/// Pure builder mirroring `workspace_summary`'s rendering
/// (`{i}:{tag}[*][(state)]`, space-joined, `list_records` order) but also
/// returning the column range each token occupies, for the status-bar
/// click map (docs/clickable-status-bar-design.md section 4.2). `siblings`
/// must already be filtered to one workspace and ordered the way
/// `workspace_summary` expects; kept pure (no `Paths`/filesystem access)
/// so it's testable without touching disk, the same split
/// `pick_switch_target`/`resolve_switch_target` already use.
///
/// **Not yet called from `draw_status_bar`** -- see the design doc
/// section 7; `workspace_summary` (the currently-live renderer) is
/// untouched by this addition.
#[allow(dead_code)]
fn workspace_summary_regions(
    siblings: &[SessionRecord],
    current_id: Uuid,
) -> (String, Vec<BarRegion>) {
    let mut text = String::new();
    let mut regions = Vec::new();
    for (i, r) in siblings.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let start = terminal_display_width(&text);
        let (state, _) = session_ui_state(r, now_ms());
        text.push_str(&format!("{}:{}", i + 1, sanitize_terminal_text(&r.tag)));
        if r.id == current_id {
            text.push('*');
        }
        if !matches!(state, "running" | "working" | "active" | "quiet") {
            text.push_str(&format!("({state})"));
        }
        let end = terminal_display_width(&text);
        regions.push(BarRegion {
            cols: start..end,
            action: BarClick::Sibling(i + 1),
        });
    }
    (text, regions)
}

/// What one `InputScanner::scan` call decided to do with a chunk of raw
/// stdin bytes; several may result from a single `read()` (e.g.
/// `"a\x02n"` -> `Forward([b'a'])`, `Switch(Next)`).
enum InputAction {
    /// Ordinary input for the currently attached session.
    Forward(Vec<u8>),
    /// `Ctrl-b d`.
    Detach,
    /// `Ctrl-b n/p/N/P/l/1-9`, and `Ctrl-b c` (which creates the session it
    /// then switches to -- see `SwitchTarget::New`).
    Switch(SwitchTarget),
    /// `Ctrl-b ?`: a purely local help flash on the status bar. Consumed
    /// like every other chord -- no byte reaches the workload, so asking
    /// for help can never type `?` into a prompt.
    Help,
    /// `Ctrl-b r`: repaint the host from the client's live screen model.
    /// Local, like Help -- the garbled cells are on this terminal, not in
    /// the session.
    Redraw,
    /// `Ctrl-b [`: open the scrollback pager (tmux's copy-mode chord). Local
    /// and, crucially, *consumed*: from here until the user leaves the mode,
    /// no keystroke reaches the workload.
    Scroll,
}

/// Byte-scanning state for the `Ctrl-b` prefix state machine, split out of
/// the input thread body so the split-across-`read()` cases are
/// unit-testable independent of any real socket/thread (see the `#[cfg(test)]`
/// module below). `pending_ctrl_b` has to survive across `scan()` calls, not
/// just within one buffer: `Ctrl-b` can legitimately arrive as the very
/// last byte of one `read()` and the following key as the first byte of the
/// next.
/// Whether `fd` has input waiting, waiting up to `timeout` for it. Used only
/// to bound the scanner's wait for the rest of an arrow chord, so an error
/// (or a signal) answers "yes": the caller falls through to its ordinary
/// blocking `read`, which is where read errors are already handled.
fn readable(fd: libc::c_int, timeout: Duration) -> bool {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().clamp(0, i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
    ready != 0
}

/// How long a `Ctrl-b ESC` may wait for the rest of an arrow-key sequence
/// before the scanner concludes there is no arrow coming and forwards the
/// withheld bytes to the workload.
///
/// The arrow chords (`Ctrl-b Left/Right/Up/Down`) are multi-byte -- `ESC [ C`
/// in normal cursor mode, `ESC O C` in application cursor mode, which plenty
/// of TUIs enable -- and a PTY read can split them anywhere, so the scanner
/// has to be able to hold a half-typed one across `read()` calls. But a bare
/// `Ctrl-b ESC` (a user reaching for the workload's own Escape, having
/// touched the prefix key by accident) is indistinguishable from the first
/// byte of an arrow until either the rest arrives or enough time passes, and
/// holding it indefinitely would leave an editor sitting in insert mode with
/// no idea why. So the wait is bounded: the input thread polls for this long
/// while the scanner holds a partial chord and, on silence, flushes it
/// through (`InputScanner::flush_pending`).
///
/// 100ms is far longer than the gap a terminal can put between the bytes of
/// one escape sequence (they are written in a single `write`; a split is a
/// buffer boundary, not a pause) and short enough to read as instant for the
/// Escape case. It is only ever paid after a literal `Ctrl-b ESC`, never on
/// ordinary input.
const CHORD_ESCAPE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Default)]
struct InputScanner {
    pending_ctrl_b: bool,
    /// Bytes withheld *after* a `Ctrl-b` because they could still complete an
    /// arrow chord: `ESC`, `ESC [`, or `ESC O`, and nothing else. Empty at
    /// every other moment, which is what `awaiting_escape` reports. The
    /// withheld `Ctrl-b` itself is implied (it is not stored here) and is
    /// re-emitted ahead of these bytes whenever the sequence turns out not to
    /// be a chord.
    pending_escape: Vec<u8>,
}

impl InputScanner {
    /// True while a partial arrow chord is held. The input thread uses this
    /// to bound its wait for the rest (see `CHORD_ESCAPE_TIMEOUT`).
    fn awaiting_escape(&self) -> bool {
        !self.pending_escape.is_empty()
    }

    /// True while a *lone* `Ctrl-b` is held with nothing after it yet -- the
    /// hesitation the key overlay is armed on (`KEY_OVERLAY_DELAY`).
    ///
    /// Deliberately spelled as "prefix pending **and** nothing withheld after
    /// it", even though `scan` already clears `pending_ctrl_b` before it ever
    /// fills `pending_escape`: this is the predicate that makes the overlay's
    /// deadline and `CHORD_ESCAPE_TIMEOUT` mutually exclusive, so it states
    /// that exclusion rather than relying on a reader knowing the other
    /// invariant.
    fn awaiting_key(&self) -> bool {
        self.pending_ctrl_b && self.pending_escape.is_empty()
    }

    /// True when nothing at all is withheld: no prefix, no partial chord.
    /// The input thread reads this as "whatever the user was in the middle of
    /// is resolved", which is when the overlay comes down.
    fn settled(&self) -> bool {
        !self.pending_ctrl_b && self.pending_escape.is_empty()
    }

    /// Give up on a partial arrow chord and release what was withheld -- the
    /// `Ctrl-b` and the escape bytes -- as ordinary input, exactly as the
    /// "not a bound chord" fall-through in `scan` does.
    ///
    /// Deliberately does *not* flush a lone pending `Ctrl-b`: waiting
    /// indefinitely for its second key is this keymap's documented behavior
    /// (a chord is only a chord once its key arrives), and only the
    /// multi-byte arrows introduced ambiguity worth timing out.
    fn flush_pending(&mut self) -> Vec<InputAction> {
        if self.pending_escape.is_empty() {
            return Vec::new();
        }
        let mut out = vec![0x02];
        out.append(&mut self.pending_escape);
        vec![InputAction::Forward(out)]
    }

    /// Scan rules (docs/fast-session-switching-design.md section 5.1):
    /// `Ctrl-b d` detaches; `?` flashes the key reference; `r` redraws the
    /// live screen; `[` opens the scrollback pager; `n` creates another
    /// session in this workspace and switches to it; `Right`/`Left` move
    /// between the sessions of this workspace and `Down`/`Up` between
    /// workspaces; `N P l 1-9` switch. Anything else pending is "not a
    /// real prefix" -- the withheld `Ctrl-b` byte is forwarded and the
    /// current byte is reprocessed normally, so unbound `Ctrl-b` sequences
    /// still pass through to the workload untouched.
    fn scan(&mut self, buffer: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < buffer.len() {
            let byte = buffer[i];
            // Mid-arrow: `pending_escape` is only ever non-empty between the
            // `ESC` of a possible `Ctrl-b <arrow>` and its final byte, and
            // `pending_ctrl_b` is cleared before we get here, so the two
            // states cannot both be live.
            if !self.pending_escape.is_empty() {
                let complete = match (self.pending_escape.len(), byte) {
                    // Both encodings: CSI (`ESC [`, normal cursor mode) and
                    // SS3 (`ESC O`, application cursor mode). A TUI can flip
                    // the terminal into either, so binding only one of them
                    // would make the arrows work until the workload changed
                    // its mind.
                    (1, b'[') | (1, b'O') => {
                        self.pending_escape.push(byte);
                        i += 1;
                        continue;
                    }
                    (2, b'A') => Some(SwitchTarget::PrevWorkspace),
                    (2, b'B') => Some(SwitchTarget::NextWorkspace),
                    (2, b'C') => Some(SwitchTarget::Next),
                    (2, b'D') => Some(SwitchTarget::Prev),
                    _ => None,
                };
                match complete {
                    Some(target) => {
                        self.pending_escape.clear();
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::Switch(target));
                        i += 1;
                        continue;
                    }
                    None => {
                        // Not an arrow after all (`Ctrl-b ESC`, `Ctrl-b ESC [ H`,
                        // ...): release the withheld `Ctrl-b` and escape bytes
                        // and reprocess this byte normally -- it may itself be
                        // a fresh `Ctrl-b`, so `i` does not advance.
                        out.push(0x02);
                        out.append(&mut self.pending_escape);
                        continue;
                    }
                }
            }
            if self.pending_ctrl_b {
                self.pending_ctrl_b = false;
                // `ESC` after the prefix is the start of a possible arrow
                // chord; withhold it until the following bytes say which.
                if byte == 0x1b {
                    self.pending_escape.push(byte);
                    i += 1;
                    continue;
                }
                let action = match byte {
                    b'd' => {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::Detach);
                        return actions;
                    }
                    b'?' => Some(InputAction::Help),
                    b'r' => Some(InputAction::Redraw),
                    b'[' => Some(InputAction::Scroll),
                    // The product ask, on the key the user asked for. Session
                    // navigation lives on the arrows (above), so `n` is free
                    // to mean "new" the way it reads. `p` is deliberately
                    // *unbound*: it was only ever the other half of `n`/`p`,
                    // and leaving it as a lone "previous" next to an `n` that
                    // creates would be a trap. It falls through untouched.
                    b'n' => Some(InputAction::Switch(SwitchTarget::New)),
                    b'N' => Some(InputAction::Switch(SwitchTarget::NextGlobal)),
                    b'P' => Some(InputAction::Switch(SwitchTarget::PrevGlobal)),
                    b'l' => Some(InputAction::Switch(SwitchTarget::Last)),
                    b'1'..=b'9' => Some(InputAction::Switch(SwitchTarget::Index(
                        (byte - b'0') as usize,
                    ))),
                    _ => None,
                };
                if let Some(action) = action {
                    if !out.is_empty() {
                        actions.push(InputAction::Forward(std::mem::take(&mut out)));
                    }
                    actions.push(action);
                    i += 1;
                    continue;
                }
                // Not a bound chord: forward the withheld Ctrl-b and
                // reprocess this byte normally (it might itself be a fresh
                // Ctrl-b) -- do not advance `i`.
                out.push(0x02);
                continue;
            }
            if byte == 0x02 {
                self.pending_ctrl_b = true;
                i += 1;
                continue;
            }
            out.push(byte);
            i += 1;
        }
        if !out.is_empty() {
            actions.push(InputAction::Forward(out));
        }
        actions
    }
}

/// Atomic switch-or-stay, run on the input thread
/// (docs/fast-session-switching-design.md section 3.3). The critical
/// ordering property: the new connection is fully established *before* the
/// old one is touched, so any failure (resolution, `check_attachable`, or
/// `establish` itself) leaves the attachment to the current session
/// completely undisturbed.
#[allow(clippy::too_many_arguments)]
fn perform_switch(
    paths: &Paths,
    target: SwitchTarget,
    replay_bytes: Option<usize>,
    want_screen: bool,
    term: &Arc<Mutex<TermGeom>>,
    shared_record: &Arc<Mutex<SessionRecord>>,
    last_session: &Arc<Mutex<Option<Uuid>>>,
    writer: &Arc<Mutex<UnixStream>>,
    pending_switch: &Arc<Mutex<Option<SwitchOutcome>>>,
    switch_in_progress: &Arc<AtomicBool>,
) -> Result<()> {
    let current = shared_record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let last = *last_session.lock().unwrap_or_else(PoisonError::into_inner);
    // Current terminal geometry (docs/fast-session-switching-design.md
    // section 5.2 / docs/terminal-state-design.md section 10.2): passed
    // into the Attach so the new session's snapshot renders at the right
    // size immediately, rather than relying solely on the post-switch
    // explicit Resize send below. Read before resolution because
    // `SwitchTarget::New` also hands it to the worker it starts.
    let geometry = term
        .lock()
        .ok()
        .map(|g| *g)
        .filter(|g| g.rows > 0)
        .map(|g| (reserved_rows(g.rows), g.cols));
    // `New` makes its target instead of picking one; everything below is the
    // same for both, which is what keeps "which key creates a session" a
    // one-line change in `InputScanner::scan`. Creation happens here, still
    // before the current attachment has been touched, so a failed create is
    // indistinguishable from a failed resolve: an Err, and the user stays put.
    let next = match target {
        SwitchTarget::New => create_sibling_session(paths, &current, geometry)?,
        _ => resolve_switch_target(paths, &current, target, last)?,
    };
    if next.id == current.id {
        return Ok(()); // switching to yourself: silent no-op
    }
    check_attachable(&next)?;
    switch_in_progress.store(true, Ordering::Relaxed);
    let result = (|| -> Result<()> {
        let handshake = establish(&next, replay_bytes, want_screen, geometry)?;
        let reader = handshake.reader;
        let history = handshake.initial;
        let writer_clone = reader.try_clone()?; // before mutating anything
                                                // Repoint every forwarding thread (input, resize) at B, then retire
                                                // A's stream. From this instant keystrokes land in B.
        let old = {
            let mut w = writer.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *w, writer_clone)
        };
        *pending_switch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(SwitchOutcome {
            record: next.clone(),
            reader,
            history,
        });
        *last_session.lock().unwrap_or_else(PoisonError::into_inner) = Some(current.id);
        // Polite detach from A, then shutdown so the main loop's blocked
        // read_frame on A's socket returns immediately. shutdown() is
        // socket-wide, so it also unblocks the reader fd cloned from this
        // stream -- the same mechanism the existing detach path relies on.
        let mut old = old;
        let _ = write_json(&mut old, &AttachControl::Detach);
        let _ = old.shutdown(std::net::Shutdown::Both);
        Ok(())
    })();
    switch_in_progress.store(false, Ordering::Relaxed);
    result
}

/// Closes the race where the frame loop breaks because A's worker died at
/// the same moment the user pressed a switch chord, *before* the input
/// thread finished storing the outcome: if `pending_switch` is `None` but
/// `switch_in_progress` is true, poll briefly for the outcome before giving
/// up and treating it as a normal exit (docs/fast-session-switching-design.md
/// section 5.2).
fn take_pending_switch(
    pending: &Arc<Mutex<Option<SwitchOutcome>>>,
    in_progress: &Arc<AtomicBool>,
) -> Option<SwitchOutcome> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(o) = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Some(o);
        }
        if !in_progress.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ends the current attach from the input side. Sending the polite control
/// frame lets the worker release its subscriber immediately; shutting down
/// our socket locally guarantees the main frame loop wakes even if the peer
/// cannot read the control frame. This is shared by explicit detach, stdin
/// EOF, and terminal read/write failure so none can strand `attach()` in its
/// blocking socket read.
fn detach_attached_client(writer: &Arc<Mutex<UnixStream>>, active: &Arc<AtomicBool>) {
    let _ = send_control(writer, &AttachControl::Detach);
    active.store(false, Ordering::Relaxed);
    let stream = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Why the attach frame loop stopped. The goodbye line is the user's
/// diagnosis of which layer to look at, so "Detached" is reserved for a
/// client that left on purpose -- a worker-side error or a dropped socket
/// must not borrow that word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachStop {
    /// Ctrl-b d, stdin EOF, or a terminal read/write failure that made this
    /// client tear its own attach down.
    ClientDetached,
    /// The worker reported the workload gone (End frame we did not cause, or
    /// `ServerEvent::Exit`).
    SessionEnded,
    /// The worker sent `ServerEvent::Error`: a PTY/waiter failure, a
    /// containment cleanup that could not be proven, or a raw-tail
    /// (`--history-bytes`) subscriber evicted for falling behind. Live-screen
    /// subscribers coalesce instead of being evicted (issue #16), so for a
    /// plain `a attach` this now means a genuine worker-side failure.
    WorkerError,
    /// The socket ended under us with no explanation: EOF, ConnectionReset,
    /// or UnexpectedEof. The worker is unreachable; the session may well
    /// still be fine.
    SocketLost,
}

/// Client intent wins over everything but a session that actually ended:
/// Ctrl-b d shuts our own stream down, so the frame loop very often then
/// observes a reset that must not be reported as a connection loss.
fn classify_attach_stop(
    session_ended: bool,
    detached_by_client: bool,
    worker_error: bool,
) -> AttachStop {
    if session_ended {
        AttachStop::SessionEnded
    } else if detached_by_client {
        AttachStop::ClientDetached
    } else if worker_error {
        AttachStop::WorkerError
    } else {
        AttachStop::SocketLost
    }
}

/// `inspect_id` is the short session id when a record survived the session's
/// end (Failed/OOM leftovers); a clean exit removes the record and leaves
/// nothing to point the user at.
fn attach_goodbye_line(stop: AttachStop, selector: &str, inspect_id: Option<&str>) -> String {
    match stop {
        AttachStop::ClientDetached => format!("Detached from {selector}."),
        AttachStop::SessionEnded => match inspect_id {
            Some(id) => format!(
                "Session ended: {selector}. Inspect output with `a capture {id} --screen --plain`."
            ),
            None => format!("Session ended: {selector}."),
        },
        AttachStop::WorkerError => format!("Attach dropped: {selector}."),
        AttachStop::SocketLost => format!("Connection to {selector} lost."),
    }
}

fn attach(
    paths: &Paths,
    record: &SessionRecord,
    history_bytes: Option<usize>,
    no_status: bool,
) -> Result<()> {
    check_attachable(record)?;
    let explicit_history = history_bytes.is_some();
    let replay_bytes = Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES));
    let input_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let display_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    // PocketShell owns the session chrome. In this mode the host terminal is
    // a full-screen relay: no reserved status row, redraw thread, flash hint,
    // or Ctrl-b scanner is installed.
    let status_enabled = display_tty && !no_status;
    // Geometry read up front (docs/terminal-state-design.md section 6.3
    // step 1), not after the handshake: sent in the Attach request itself
    // so the worker can resize the PTY and its screen model *before*
    // rendering the snapshot below -- no wrong-size frame followed by a
    // SIGWINCH repaint. `--history-bytes` is the explicit escape hatch back
    // to the old raw-tail semantics (section 6.1); `want_screen` follows
    // its absence.
    let initial_geometry = if display_tty {
        terminal_size(libc::STDOUT_FILENO)
    } else if input_tty {
        terminal_size(libc::STDIN_FILENO)
    } else {
        None
    };
    let worker_geometry = initial_geometry.map(|(rows, cols)| {
        (
            if status_enabled {
                reserved_rows(rows)
            } else {
                rows
            },
            cols,
        )
    });
    let handshake = establish(record, replay_bytes, !explicit_history, worker_geometry)?;
    let mut reader = handshake.reader;
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let _raw = if input_tty {
        Some(RawMode::enter(libc::STDIN_FILENO)?)
    } else {
        None
    };
    // Display cleanup is independent of where input comes from. In
    // particular, `a attach </dev/null` still writes the snapshot to a tty
    // stdout before stdin EOF detaches, so it must undo that snapshot's
    // alternate-screen and input modes even though stdin was never raw.
    let _ui_guard = if display_tty {
        Some(TerminalUiGuard {
            stdout: stdout.clone(),
        })
    } else {
        None
    };
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let active = Arc::new(AtomicBool::new(true));
    let mut signal_bridge = if input_tty || display_tty {
        Some(AttachSignalBridge::install(writer.clone(), active.clone())?)
    } else {
        None
    };
    let term = Arc::new(Mutex::new(TermGeom {
        rows: 0,
        cols: 0,
        reserved: false,
    }));
    // Last time PTY output was written to the real terminal -- read by the
    // status-bar thread to decide when it's a good moment to redraw (see
    // STATUS_BAR_IDLE_GAP's doc comment above).
    let last_activity = Arc::new(Mutex::new(Instant::now()));

    // -- Fast in-process session switching state (survives across
    //    switches, unlike `reader`/`writer`'s inner stream/`record`; see
    //    docs/fast-session-switching-design.md sections 2-3) --
    let shared_record = Arc::new(Mutex::new(record.clone()));
    let pending_switch: Arc<Mutex<Option<SwitchOutcome>>> = Arc::new(Mutex::new(None));
    let switch_in_progress = Arc::new(AtomicBool::new(false));
    let last_session: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let switch_replay_bytes = Some(history_bytes.unwrap_or(SWITCH_REPLAY_BYTES));
    // Sized to the *workload's* geometry (the terminal minus the reserved bar
    // row), so the model's coordinates are the host's coordinates for every
    // row the workload can reach -- see `StatusBarCtx::screen`.
    let (screen_rows, screen_cols) = worker_geometry.unwrap_or((
        aplexer::screen::DEFAULT_TERMINAL_ROWS,
        aplexer::screen::DEFAULT_TERMINAL_COLS,
    ));
    // The client's model, unlike the worker's, retains scrollback: this is
    // the grid `Ctrl-b [` pages through, and giving it a real depth is what
    // makes an attached session's history readable at all (see the scroll
    // mode section above `history_limit`). The requested line count is
    // clamped against `MAX_SCROLLBACK_CELLS` at this terminal's width.
    let scrollback_lines = aplexer::screen::scrollback_lines_for(
        screen_cols,
        if status_enabled { history_limit() } else { 0 },
    );
    let workload_screen = Arc::new(Mutex::new(
        aplexer::screen::ClientScreen::try_new_with_scrollback(
            screen_rows,
            screen_cols,
            scrollback_lines,
        )?,
    ));
    let scroll_mode = Arc::new(ScrollMode::new());
    let key_overlay = Arc::new(KeyOverlay::default());
    let status_ctx = StatusBarCtx {
        stdout: stdout.clone(),
        term: term.clone(),
        paths: paths.clone(),
        record: shared_record.clone(),
        flash: Arc::new(Mutex::new(None)),
        last_drawn: Arc::new(Mutex::new(None)),
        screen: workload_screen.clone(),
        pending: Arc::new(AtomicBool::new(false)),
        pending_refresh: Arc::new(AtomicBool::new(false)),
        pending_layout: Arc::new(Mutex::new(None)),
        sync_deferred_since: Arc::new(Mutex::new(None)),
        scroll: scroll_mode.clone(),
        overlay: key_overlay.clone(),
        mouse_owned: Arc::new(Mutex::new(None)),
        mouse_capture: status_enabled && input_tty && mouse_capture_enabled(),
    };

    // Hold the host on the alternate screen for the whole attach, *before*
    // DECSTBM or the snapshot write anything. The primary screen -- and the
    // `a` list sitting on it -- stays frozen underneath, so host scrollback
    // cannot mix those rows into the live view. Workload 1049h/1049l still
    // update the model; `filter_host` keeps them off the wire.
    if display_tty {
        workload_screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .hold_host_on_alt_screen();
        let _ = write_locked(&stdout, ATTACH_ALT_SCREEN_ENTER);
        if status_enabled {
            if let Some((rows, cols)) = initial_geometry {
                // Nothing has been relayed yet, so the model is at a boundary by
                // construction and this always writes -- the gate is free here
                // and costs nothing to keep uniform.
                apply_terminal_layout(&status_ctx, rows, cols);
            }
        }
    }
    // Scanned before the bar is drawn: the snapshot re-emits the workload's
    // DECSTBM sub-range as its last bytes (design doc section 6.2 step 3), so
    // scanning it here is what lets the immediately-following `draw_status_bar`
    // re-assert that region instead of overwriting it with the bar's own.
    // Prime the retained history *before* the reattach snapshot, so its rows
    // sit above the screen the snapshot paints rather than after it. Model
    // only: not one byte of the seed reaches the terminal.
    if scrollback_lines > 0 {
        seed_client_scrollback(&workload_screen, record);
    }
    feed_and_write(&stdout, &workload_screen, b"", &handshake.initial, None)?;
    // After the snapshot, because the snapshot is what tells the model which
    // mouse modes the workload itself wants -- and the workload's wishes
    // decide whether the client may borrow the mouse at all.
    if status_enabled {
        sync_client_mouse(&status_ctx);
        // The attach hint goes through the status-bar flash channel, not an
        // eprintln'd banner: a banner written before/around the snapshot is
        // what once corrupted a live TUI's input box (docs/terminal-state-
        // design.md section 6.3 step 6 / section 10.1 item c). The flash is
        // drawn after the snapshot (whose ED2 blanked the bar row) and
        // disappears on its own after FLASH_DURATION.
        flash_status(
            &status_ctx,
            format!(
                "attached to {} · Ctrl-b ? help · Ctrl-b d detach",
                record.tag
            ),
        );
    }
    // The explicit post-connect Resize control send is unnecessary when the
    // Attach already carried geometry and a new-enough worker honored it
    // (the "screen" key is present in the response either way, true or
    // false); kept only for the old-worker fallback path, whose response
    // predates the field (section 6.3 step 7).
    if handshake.screen.is_none() {
        if let Some((rows, cols)) = worker_geometry {
            send_control(&writer, &AttachControl::Resize { rows, cols })?;
        }
    }

    let input_writer = writer.clone();
    let input_active = active.clone();
    // Set when THIS client ends its attach on purpose (Ctrl-b d, or its
    // stdin hit EOF) as opposed to the session ending under it. Combined
    // with `session_ended` / `worker_error` after the frame loop, this is
    // what keeps "Detached from ..." off the connection-loss and
    // worker-error paths -- see `classify_attach_stop`.
    let detached_by_client = Arc::new(AtomicBool::new(false));
    let input_detached = detached_by_client.clone();
    let input_paths = paths.clone();
    let input_term = term.clone();
    let input_shared_record = shared_record.clone();
    let input_last_session = last_session.clone();
    let input_pending_switch = pending_switch.clone();
    let input_switch_in_progress = switch_in_progress.clone();
    let input_status_ctx = status_ctx.clone();
    let input_want_screen = !explicit_history;
    let input_status_enabled = status_enabled;
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buffer = [0u8; 8192];
        // Ctrl-b (0x02) prefix state machine -- Ctrl-b d detaches,
        // Ctrl-b ? flashes the key reference, Ctrl-b r redraws the live
        // screen, Ctrl-b c creates another session here, Ctrl-b n/p/N/P/l/1-9
        // switch sessions, anything else
        // pending is not a real prefix (both bytes forward to the
        // workload). See
        // `InputScanner` for the byte-level rules and why this needs to
        // survive across separate read() calls, not just within one
        // buffer.
        //
        // Design choice: real tmux turns Ctrl-b into a standing "prefix"
        // that consumes the next keystroke as a command (or no-ops/bells if
        // unrecognized), never forwarding Ctrl-b itself to the pane. aplexer
        // has no such command-prefix system and isn't growing one just for
        // this, so the simplest reasonable behavior is used instead: a
        // *bound* Ctrl-b sequence (d/?/r/c/n/p/N/P/l/1-9) is consumed; anything
        // else is not a prefix at all -- both bytes are forwarded through as
        // ordinary input, so a program that wants a literal Ctrl-b (some
        // editors and REPLs use it) isn't broken by this feature.
        let mut scanner = InputScanner::default();
        // Sits between the chord scanner and the socket: while the pager is
        // up it consumes every byte (no keystroke reaches the workload --
        // the whole point of the mode), and while the client holds the
        // mouse it consumes mouse reports the workload never asked for and
        // turns a wheel roll up into the pager.
        let mut scroll_input = ScrollInput::default();
        // Whether `KEY_OVERLAY_DELAY` has already fired for the `Ctrl-b`
        // currently being held. Not "is the box up" -- that lives in
        // `KeyOverlay::active`, which the resize thread can also clear -- but
        // "this prefix has had its one chance to raise it", which is what
        // keeps a terminal too small for the box from flashing on a loop.
        let mut overlay_armed = false;
        'outer: while input_active.load(Ordering::Relaxed) {
            // The only two places this loop does not simply block on stdin,
            // and they are mutually exclusive by construction (see
            // `KEY_OVERLAY_DELAY`, which spells out why that matters):
            //
            // - a half-typed arrow chord has `CHORD_ESCAPE_TIMEOUT` to
            //   complete, after which the withheld bytes are released to the
            //   workload as ordinary input;
            // - a lone `Ctrl-b` has `KEY_OVERLAY_DELAY` before the keymap is
            //   drawn for it. The short-circuit is what keeps this free: with
            //   nothing pending, neither `readable` call is even reached.
            let chord_expired =
                scanner.awaiting_escape() && !readable(libc::STDIN_FILENO, CHORD_ESCAPE_TIMEOUT);
            if !chord_expired
                && !overlay_armed
                && scanner.awaiting_key()
                && !input_status_ctx.scroll.is_active()
                && !readable(libc::STDIN_FILENO, KEY_OVERLAY_DELAY)
            {
                // Hesitation on the prefix rather than a chord typed from
                // muscle memory. Draw the keymap and go straight back to
                // waiting for the key it explains -- the prefix is still
                // pending, so that key runs its binding exactly as it would
                // have. `overlay_armed` latches the deadline for *this*
                // prefix so a terminal too small for the box flashes the
                // one-line reference once instead of every 350ms.
                overlay_armed = true;
                show_key_overlay(&input_status_ctx);
                continue;
            }
            let actions = if chord_expired {
                let flushed = scanner.flush_pending();
                overlay_armed = false;
                if dismiss_key_overlay(&input_status_ctx) {
                    // `Esc` with the box up means "never mind", and taking
                    // the box down is the whole of it: the withheld
                    // `Ctrl-b ESC` is consumed rather than typed into the
                    // workload, which is what every menu of this shape does.
                    // Without the box this is unchanged -- both bytes go
                    // through, so an editor still gets its Escape.
                    Vec::new()
                } else {
                    flushed
                }
            } else {
                let n = match input.read(&mut buffer) {
                    Ok(0) => {
                        input_detached.store(true, Ordering::Relaxed);
                        detach_attached_client(&input_writer, &input_active);
                        break;
                    }
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        input_detached.store(true, Ordering::Relaxed);
                        detach_attached_client(&input_writer, &input_active);
                        break;
                    }
                };
                if !input_tty || !input_status_enabled {
                    if send_data(&input_writer, &buffer[..n]).is_err() {
                        detach_attached_client(&input_writer, &input_active);
                        break;
                    }
                    continue;
                }
                let actions = scanner.scan(&buffer[..n]);
                if scanner.settled() {
                    overlay_armed = false;
                    // A key arrived, so the overlay is over *before* its
                    // action runs: everything below -- a switch's replayed
                    // screen, a `?` flash, the pager's first frame -- draws
                    // onto the restored screen instead of onto the box. A
                    // key that turned out not to be a chord has already been
                    // put back into `actions` as ordinary input by `scan`,
                    // so the fall-through contract is untouched.
                    dismiss_key_overlay(&input_status_ctx);
                }
                actions
            };
            for action in actions {
                match action {
                    InputAction::Forward(bytes) => {
                        // Ordering matters: a Forward before a Switch goes
                        // to the old session (keystrokes typed before the
                        // chord); a Forward after it goes to the new one,
                        // automatically, because perform_switch swapped the
                        // stream inside input_writer's mutex.
                        let bytes = scroll_input.route(&input_status_ctx, &bytes);
                        if bytes.is_empty() {
                            continue;
                        }
                        if send_data(&input_writer, &bytes).is_err() {
                            detach_attached_client(&input_writer, &input_active);
                            break 'outer;
                        }
                    }
                    InputAction::Detach => {
                        // Leave the pager first: detach restores the host
                        // from the *live* model, and the reset sequence it
                        // writes assumes the relay owns the screen again.
                        exit_scroll_mode(&input_status_ctx);
                        input_detached.store(true, Ordering::Relaxed);
                        detach_attached_client(&input_writer, &input_active);
                        break 'outer;
                    }
                    InputAction::Help => {
                        flash_status(&input_status_ctx, attach_key_help());
                    }
                    InputAction::Redraw => {
                        // `Ctrl-b r` means "this display is garbled, redraw
                        // what I am looking at". While the pager is up that
                        // is the pager, not the live screen -- painting the
                        // live screen here would silently swap the user's
                        // view without giving them the keyboard back.
                        if input_status_ctx.scroll.is_active()
                            && !input_status_ctx.scroll.is_typing()
                        {
                            paint_scroll_view(&input_status_ctx);
                        } else {
                            redraw_live_screen(&input_status_ctx);
                        }
                    }
                    InputAction::Scroll => {
                        enter_scroll_mode(&input_status_ctx, ScrollCommand::Stay);
                    }
                    InputAction::Switch(target) => {
                        // A switch replaces the model wholesale; the pager
                        // is looking at the outgoing session's history, so
                        // it has to close before the swap. `SwitchTarget::New`
                        // (Ctrl-b c) rides this same arm: it starts a session
                        // first and then switches to it, so a create that
                        // fails lands in the same Err below -- a flash on the
                        // bar, the attach to the current session untouched.
                        exit_scroll_mode(&input_status_ctx);
                        let result = perform_switch(
                            &input_paths,
                            target,
                            switch_replay_bytes,
                            input_want_screen,
                            &input_term,
                            &input_shared_record,
                            &input_last_session,
                            &input_writer,
                            &input_pending_switch,
                            &input_switch_in_progress,
                        );
                        // On Err nothing was sent to A and nothing swapped
                        // (perform_switch's ordering guarantee) -- the user
                        // just stays where they were with an explanation on
                        // the bar. The consumed chord bytes are never
                        // forwarded either way.
                        if let Err(e) = result {
                            flash_status(&input_status_ctx, format!("{e:#}"));
                        }
                    }
                }
            }
        }
        // Whatever ended this thread -- stdin EOF, a read error, a dead
        // socket, `Ctrl-b d` -- must not leave the relay suspended behind a
        // box nobody can dismiss any more: this thread is the only one that
        // takes keys. A no-op in the ordinary case, because a key arriving is
        // what dismisses the overlay and `Ctrl-b d` is a key.
        dismiss_key_overlay(&input_status_ctx);
    });
    if display_tty {
        let resize_writer = writer.clone();
        let resize_active = active.clone();
        let resize_ctx = status_ctx.clone();
        let resize_initial = initial_geometry;
        let resize_status_enabled = status_enabled;
        thread::spawn(move || {
            // Seeded with the geometry `attach()` already applied and already
            // sent in the Attach request, so this thread reacts to *changes*
            // only. Starting from `None` made its very first poll look like a
            // resize and re-run `apply_terminal_layout` unconditionally --
            // harmless-looking, but it re-asserted `\x1b[1;{rows-1}r` and
            // dropped the workload scroll region the attach snapshot had just
            // restored (docs/terminal-state-design.md section 6.2 step 3),
            // roughly 200 ms after every attach.
            let mut last = resize_initial;
            while resize_active.load(Ordering::Relaxed) {
                let size = terminal_size(libc::STDOUT_FILENO);
                if size != last {
                    if let Some((rows, cols)) = size {
                        // Keep the client's tracker in step with the
                        // worker-side model across the same resize: both
                        // re-clamp the region to the new row count rather
                        // than dropping it (see `MarginTracker::set_rows`
                        // and design doc section 5.3's correction).
                        let worker_rows = if resize_status_enabled {
                            reserved_rows(rows)
                        } else {
                            rows
                        };
                        if let Ok(mut m) = resize_ctx.screen.lock() {
                            m.set_size(worker_rows, cols);
                        }
                        // May defer the DECSTBM write when the relayed
                        // stream is mid-escape-sequence (issue #14). The
                        // *workload* is never made to wait on that: the
                        // Resize control below goes out unconditionally, so
                        // the PTY is resized and SIGWINCH delivered on time
                        // whatever the host-side reservation is doing. Only
                        // the client's own row reservation waits, and only
                        // for as long as `LAYOUT_DEFER_LIMIT`.
                        if resize_status_enabled {
                            apply_terminal_layout(&resize_ctx, rows, cols);
                            // The pager renders at the reserved geometry, so a
                            // resize has to redraw it -- nothing else will,
                            // because the relay is suspended.
                            if resize_ctx.scroll.is_active() {
                                paint_scroll_view(&resize_ctx);
                            }
                            // Same for the key overlay, with one extra case: the
                            // new geometry may be one the box does not fit into
                            // at all, and a repaint that cannot happen must take
                            // the overlay down rather than leave a stale box over
                            // a suspended relay.
                            if resize_ctx.overlay.is_active() && !paint_key_overlay(&resize_ctx) {
                                dismiss_key_overlay(&resize_ctx);
                            }
                        }
                        // A switch deliberately shuts down the old socket to
                        // unblock the main frame loop's read (see
                        // perform_switch); if a real terminal resize races
                        // that exact window this send_control can fail on
                        // the about-to-die stream even though nothing is
                        // actually wrong going forward. `continue` (not
                        // `break`) so this thread keeps polling across a
                        // switch instead of leaving resizes dead for the
                        // rest of the attach -- the post-switch explicit
                        // Resize the main loop sends covers any update lost
                        // in that exact window.
                        if send_control(
                            &resize_writer,
                            &AttachControl::Resize {
                                rows: worker_rows,
                                cols,
                            },
                        )
                        .is_err()
                        {
                            last = size;
                            continue;
                        }
                    }
                    last = size;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
    }
    if status_enabled {
        let status_active = active.clone();
        let status_last_activity = last_activity.clone();
        let thread_status_ctx = status_ctx.clone();
        thread::spawn(move || {
            let mut last_draw = Instant::now();
            // Edge-triggered, not level-triggered: once the PTY has been
            // idle for STATUS_BAR_IDLE_GAP, redraw exactly once and then
            // stay quiet -- not on every poll tick for as long as it
            // remains idle. Tracking "have we already redrawn for the
            // current idle stretch" (reset the moment new PTY activity is
            // observed) is what makes this an actual debounce instead of a
            // redraw storm during any sufficiently long idle period.
            let mut last_seen_activity = status_last_activity
                .lock()
                .map(|t| *t)
                .unwrap_or_else(|_| Instant::now());
            let mut drawn_for_current_idle = false;
            // Whether the previous tick found the session agent-busy, so the
            // spinner's stop gets one final freeze-frame draw (see the
            // falling-edge note in the loop body).
            let mut was_animating = false;
            while status_active.load(Ordering::Relaxed) {
                thread::sleep(STATUS_BAR_POLL_INTERVAL);
                let activity = match status_last_activity.lock() {
                    Ok(t) => *t,
                    Err(_) => continue,
                };
                if activity != last_seen_activity {
                    last_seen_activity = activity;
                    drawn_for_current_idle = false;
                }
                // A resize the boundary gate deferred is delivered here even
                // when the workload has gone completely silent, which the
                // frame loop's flush cannot cover: it only runs when a chunk
                // arrives. This tick is also what lets `LAYOUT_DEFER_LIMIT`
                // expire on a workload that stopped mid-escape-sequence.
                // Cheap: one uncontended mutex peek that returns immediately
                // when nothing is deferred, which is the overwhelming case.
                if thread_status_ctx.overlay.is_active() {
                    // The overlay owns the terminal for the moment it is up.
                    // Nothing about the bar, the mouse or a deferred resize is
                    // worth painting into it: each keeps waiting, and the
                    // repaint `dismiss_key_overlay` writes delivers them.
                    continue;
                }
                // Cheap and idempotent: returns immediately unless the
                // workload's own mouse wishes changed since the last tick.
                sync_client_mouse(&thread_status_ctx);
                if thread_status_ctx.scroll.is_active() {
                    // The pager owns the terminal. Its position readout is
                    // the only thing that needs to keep moving while the
                    // workload produces output behind it; a deferred resize
                    // or bar redraw stays deferred until the user comes back
                    // to the live screen.
                    refresh_scroll_bar(&thread_status_ctx);
                    continue;
                }
                flush_pending_layout(&thread_status_ctx);
                let idle_for = activity.elapsed();
                let overdue = last_draw.elapsed() >= STATUS_BAR_MAX_INTERVAL;
                // While the attached session is agent-busy, the bar's state
                // glyph is a spinner (`spinner_frame`) whose frame changes
                // every SPINNER_FRAME_MS -- so "the text changed" becomes a
                // redraw trigger of its own, independent of the PTY-activity
                // debounce above. That is the point of the whole feature: the
                // long silent stretch of a compute-bound tool call streams
                // nothing, and without this branch the only remaining trigger
                // would be the STATUS_BAR_MAX_INTERVAL forced tick, leaving
                // the spinner visibly stuttering once every 3s. The
                // dirty-check still guards the write itself: only the glyph
                // segment differs frame to frame, and a tick whose frame was
                // already flushed (a deferred write the frame loop performed)
                // renders byte-identical text and costs nothing.
                let animating = {
                    let record = thread_status_ctx
                        .record
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    spinner_frame(session_ui_state(&record, now_ms()).0, now_ms()).is_some()
                };
                // On the falling edge (work just stopped) draw once more even
                // though the spinner no longer animates, so the glyph freezes
                // back to its static form and the new state word lands within
                // one poll tick instead of waiting out the 3s overdue bound.
                // Unconditional on purpose: the last animation write was at
                // most one frame ago, so any elapsed-time gate here would
                // skip exactly the tick this exists for. The dirty-check
                // still makes it a no-op when nothing actually changed.
                let anim_due = animating || was_animating;
                was_animating = animating;
                if anim_due
                    || (idle_for >= STATUS_BAR_IDLE_GAP && !drawn_for_current_idle)
                    || overdue
                {
                    // `overdue` forces the write even if the text is
                    // unchanged -- see draw_status_bar's doc comment on why
                    // the margin-defense guarantee needs that. It does *not*
                    // force the write to happen here: if the relayed stream is
                    // mid-escape-sequence (which, for a continuously-streaming
                    // agent CLI, it is about half the time), `draw_status_bar`
                    // marks `ctx.pending` and the main frame loop performs the
                    // write at the next safe boundary instead. `wrote` is
                    // false in that case, so this timer correctly keeps
                    // considering the bar overdue until a real write lands.
                    //
                    // Only reset `last_draw` when a real write happened:
                    // resetting it on every tick regardless -- even ticks
                    // the dirty-check turned into a no-op -- would let the
                    // idle-gap branch's frequent no-op "redraws" keep
                    // `overdue` perpetually false, starving the very
                    // self-heal guarantee this timer exists to provide (see
                    // draw_status_bar's doc comment).
                    let wrote = draw_status_bar(&thread_status_ctx, overdue);
                    if wrote {
                        last_draw = Instant::now();
                    }
                    drawn_for_current_idle = true;
                }
            }
        });
    }

    // Whether the session ended while we were attached to it (worker sent
    // End/Exit) as opposed to the client leaving first -- drives the
    // honest goodbye line after terminal restoration. `worker_error` is the
    // `ServerEvent::Error` arm: neither a detach nor a clean workload exit.
    let mut session_ended = false;
    let mut worker_error = false;
    'session: loop {
        loop {
            let frame = match read_frame(&mut reader) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .map(|x| {
                            matches!(
                                x.kind(),
                                io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                            )
                        })
                        .unwrap_or(false) =>
                {
                    break
                }
                Err(e) => return Err(e),
            };
            match frame.kind {
                FrameKind::Data => {
                    relay_to_terminal(
                        &workload_screen,
                        &stdout,
                        &scroll_mode,
                        &key_overlay,
                        &frame.payload,
                    )?;
                    if let Ok(mut t) = last_activity.lock() {
                        *t = Instant::now();
                    }
                    if !status_enabled {
                        continue;
                    }
                    // While a client modal is up nothing may paint over it:
                    // the deferred resize, the deferred bar and the deferred
                    // `Ctrl-b r` all keep waiting, and are delivered by the
                    // repaint `exit_scroll_mode`/`dismiss_key_overlay`
                    // performs (or by the first chunk after it).
                    //
                    // Type-through is the exception to "nothing may paint":
                    // the workload's bytes are being relayed to the host, so
                    // this IS a live view and the typing bar needs the same
                    // per-chunk maintenance the live bar gets -- a deferred
                    // bar write flushes at this chunk boundary, and the row
                    // the bar lives on is repaired if the workload's own
                    // erase sequences took it out (the `Layout` arm
                    // invalidates the dirty check for exactly that case).
                    if scroll_mode.is_active() || key_overlay.is_active() {
                        if scroll_mode.is_typing() && !key_overlay.is_active() {
                            flush_pending_layout(&status_ctx);
                            refresh_scroll_bar(&status_ctx);
                        }
                        continue;
                    }
                    // A redraw the status thread wanted while the stream was
                    // mid-sequence (or mid-frame) waits here rather than being
                    // written at an unsafe offset. This is the only place a
                    // continuously-streaming workload's bar gets refreshed at
                    // all, and it is by construction a chunk boundary that the
                    // model has just confirmed is also an escape boundary --
                    // see `draw_status_bar`'s boundary gate.
                    //
                    // The deferred *resize* goes first: a bar redraw is laid
                    // out against `TermGeom`, so reasserting the new scroll
                    // region before drawing keeps the two consistent within
                    // the same chunk instead of one chunk apart.
                    flush_pending_layout(&status_ctx);
                    if status_ctx.pending_refresh.load(Ordering::Relaxed) {
                        redraw_live_screen(&status_ctx);
                    } else if status_ctx.pending.load(Ordering::Relaxed) {
                        draw_status_bar(&status_ctx, true);
                    }
                }
                FrameKind::End => {
                    session_ended = !detached_by_client.load(Ordering::Relaxed);
                    break;
                }
                FrameKind::Json => {
                    let event: ServerEvent = serde_json::from_slice(&frame.payload)?;
                    match event {
                        ServerEvent::Exit { .. } => {
                            session_ended = true;
                            break;
                        }
                        ServerEvent::Error { message } => {
                            eprintln!("[aplexer: {message}]");
                            worker_error = true;
                            break;
                        }
                        // The workload reset margins or flipped alt-screen
                        // state (docs/terminal-state-design.md section 7):
                        // re-assert the status-bar reservation and redraw
                        // within one socket round-trip of the bytes that
                        // caused it, instead of waiting on the idle-gap
                        // timer. `draw_status_bar`'s own margin re-assert
                        // (see its doc comment) is the reservation half of
                        // this; the redraw is the other half. Only ever
                        // received when this attach opted in via
                        // `want_screen` (the worker gates it -- see
                        // `handle_attach`), so this arm is unreachable on
                        // the `--history-bytes` raw-tail path, but handling
                        // it unconditionally keeps this match exhaustive and
                        // correct if that ever changes.
                        ServerEvent::Layout { .. } => {
                            if status_enabled {
                                if status_ctx.scroll.is_typing() {
                                    // Type-through streams workload bytes to the
                                    // host, and Erase in Display ignores scroll
                                    // margins: the workload's own `CSI ... J` --
                                    // which Ink-style TUIs emit on nearly every
                                    // frame -- erases past the scroll region and
                                    // takes the reserved row, the typing bar
                                    // included. Nothing else repairs it: the
                                    // dirty check sees unchanged text and skips,
                                    // and the erased row would stay blank until
                                    // the offset or the count next changes.
                                    // Invalidate the check so this refresh
                                    // actually rewrites the row.
                                    *status_ctx
                                        .last_drawn
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner) = None;
                                    refresh_scroll_bar(&status_ctx);
                                } else if !status_ctx.scroll.is_active() {
                                    // Live view: re-assert the reservation and
                                    // redraw within one socket round-trip of the
                                    // bytes that caused it, instead of waiting
                                    // on the idle-gap timer. `draw_status_bar`'s
                                    // own margin re-assert (see its doc comment)
                                    // is the reservation half of this; the
                                    // redraw is the other half.
                                    draw_status_bar(&status_ctx, true);
                                }
                                // Pager without type-through: nothing is being
                                // written to the host, so no erase can have
                                // reached the bar row and the pager's own tick
                                // already maintains it.
                            }
                        }
                    }
                }
            }
        }
        // The frame loop broke: either the session ended/we detached, or
        // the input thread killed the old stream to hand us a switch.
        let outcome = take_pending_switch(&pending_switch, &switch_in_progress);
        let Some(outcome) = outcome else { break };

        let switched_to = outcome.record.clone();
        *shared_record.lock().unwrap_or_else(PoisonError::into_inner) = outcome.record;
        reader = outcome.reader; // old stream dropped (closed) here

        // A and B have independent terminal state. Neutralize every buffer
        // and input mode A's snapshot/live stream may have enabled before
        // replaying B's snapshot: a default-mode B deliberately emits no
        // mouse-off or primary-screen transition of its own. B's snapshot
        // follows immediately and re-enables exactly the modes it owns.
        // Raw termios belongs to this client rather than either session, so
        // it remains in force across the switch.
        //
        // Geometry is read before `stdout` is taken, keeping the
        // `stdout` -> `term` -> `screen` order `write_locked` documents.
        let geom = term.lock().map(|g| *g).unwrap_or(TermGeom {
            rows: 0,
            cols: 0,
            reserved: false,
        });
        // The new session's screen is its own: drop the previous one's model
        // (its margins, its half-parsed sequences, its cursor) and learn the
        // new one's from its snapshot payload, under the same lock as the
        // write so the status thread can never redraw against a model that
        // disagrees with what the terminal has been sent.
        let (screen_rows, screen_cols) = if geom.rows > 0 {
            (reserved_rows(geom.rows), geom.cols)
        } else {
            (
                aplexer::screen::DEFAULT_TERMINAL_ROWS,
                aplexer::screen::DEFAULT_TERMINAL_COLS,
            )
        };
        // The reset and the seed are done here rather than through
        // `feed_and_write`'s `reset_to`, because the retained history has to
        // be primed *between* them: reset (drop A's model and its history),
        // seed (give B's model B's past), then paint B's screen on top. Done
        // the other way round the seed's rows would land above nothing, or
        // below B's current screen.
        {
            let mut screen = workload_screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            screen.reset(screen_rows, screen_cols);
        }
        if scrollback_lines > 0 {
            seed_client_scrollback(&workload_screen, &switched_to);
        }
        let _ = feed_and_write(
            &stdout,
            &workload_screen,
            TERMINAL_RESET_SEQUENCE,
            &outcome.history,
            None,
        );
        // TERMINAL_RESET_SEQUENCE turned every mouse mode off, so whoever
        // owned the mouse before the switch owns nothing now.
        *status_ctx
            .mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        sync_client_mouse(&status_ctx);
        if let Ok(mut t) = last_activity.lock() {
            *t = Instant::now();
        }

        // B's PTY may still be sized for its previous client (or the
        // 24x80 default). The resize thread won't resend an unchanged
        // terminal size (its `last` cache), so push the current geometry
        // explicitly.
        if geom.rows > 0 {
            let _ = send_control(
                &writer,
                &AttachControl::Resize {
                    rows: reserved_rows(geom.rows),
                    cols: geom.cols,
                },
            );
        }
        if status_enabled {
            draw_status_bar(&status_ctx, true); // clear wiped the reserved row; redraw now
        }
        continue 'session;
    }
    active.store(false, Ordering::Relaxed);
    if let Ok(stream) = writer.lock() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    // Restore display state and cooked termios before restoring the prior
    // signal disposition and re-raising. A default TERM/HUP/QUIT action can
    // terminate the process immediately, so RAII alone cannot run after it.
    drop(_ui_guard);
    drop(_raw);
    if let Some(signal) = signal_bridge.take().and_then(AttachSignalBridge::finish) {
        unsafe {
            libc::raise(signal);
        }
    }
    if display_tty {
        // After restoration, so the message lands on a clean cooked
        // terminal: what happened to the session, not just that the client
        // came back. "Detached" means the client left and the session is
        // still running; "Session ended" means the workload is gone; the
        // worker-error and connection-loss lines say which layer failed
        // instead of blaming the user's own detach. A clean exit (including
        // Ctrl-D) removes the record; only Failed/OOM leftovers remain to
        // inspect.
        let current_record = shared_record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let short_id = current_record.id.to_string();
        let inspect_id = paths
            .record(current_record.id)
            .exists()
            .then(|| &short_id[..8]);
        let stop = classify_attach_stop(
            session_ended,
            detached_by_client.load(Ordering::Relaxed),
            worker_error,
        );
        eprintln!(
            "{}",
            attach_goodbye_line(stop, &current_record.selector(), inspect_id)
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod switching_tests {
    use super::*;

    #[test]
    fn attach_goodbye_distinguishes_detach_error_and_socket_loss() {
        let selector = "/ws:tag";
        assert_eq!(
            attach_goodbye_line(classify_attach_stop(false, true, false), selector, None),
            "Detached from /ws:tag."
        );
        assert_eq!(
            attach_goodbye_line(classify_attach_stop(false, false, true), selector, None),
            "Attach dropped: /ws:tag."
        );
        assert_eq!(
            attach_goodbye_line(classify_attach_stop(false, false, false), selector, None),
            "Connection to /ws:tag lost."
        );
        // Ctrl-b d shuts our own stream down, so the frame loop that follows
        // it usually sees a reset too. Client intent must still win, or every
        // deliberate detach would report a connection loss.
        assert_eq!(
            classify_attach_stop(false, true, true),
            AttachStop::ClientDetached
        );
        // Session end is unchanged, including the two-way split on whether a
        // record survived to be inspected.
        assert_eq!(
            attach_goodbye_line(classify_attach_stop(true, false, false), selector, None),
            "Session ended: /ws:tag."
        );
        assert_eq!(
            attach_goodbye_line(
                classify_attach_stop(true, true, true),
                selector,
                Some("0a1b2c3d")
            ),
            "Session ended: /ws:tag. Inspect output with `a capture 0a1b2c3d --screen --plain`."
        );
    }

    #[test]
    fn control_deadline_bounds_a_silent_worker_and_streaming_can_clear_it() {
        let (mut client, _silent_worker) = UnixStream::pair().unwrap();
        set_control_deadlines(&client).unwrap();
        let started = Instant::now();
        let error = client.read(&mut [0u8; 1]).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_secs(1));

        clear_streaming_deadlines(&client).unwrap();
        assert_eq!(client.read_timeout().unwrap(), None);
        assert_eq!(client.write_timeout().unwrap(), None);
    }

    #[test]
    fn connect_deadline_bounds_a_saturated_unix_listener_backlog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("saturated.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);

        // Linux permits one queued connection for backlog zero. Leave it
        // unaccepted so the next real AF_UNIX connect hits the saturated
        // backlog rather than a synthetic silent-response fixture.
        let _queued = connect_with_timeout(&path, Duration::from_millis(100)).unwrap();
        let started = Instant::now();
        let error = connect_with_timeout(&path, Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(started.elapsed() >= Duration::from_millis(75));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn persisted_history_tail_is_seeked_and_frame_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut file = fs::File::create(&path).unwrap();
        file.set_len(1024 * 1024 * 1024).unwrap();
        std::io::Seek::seek(&mut file, std::io::SeekFrom::End(-4)).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);

        assert_eq!(
            read_persisted_history_tail(&path, Some(4)).unwrap(),
            b"tail"
        );

        let bounded = read_persisted_history_tail(&path, Some(usize::MAX)).unwrap();
        assert_eq!(bounded.len(), MAX_FRAME_BYTES);
        assert_eq!(&bounded[bounded.len() - 4..], b"tail");
    }

    #[test]
    fn parse_hex_rejects_non_ascii_without_panicking() {
        assert!(parse_hex("aéa".as_bytes()).is_err());
    }

    #[test]
    fn ctrl_b_question_mark_flashes_help_without_forwarding() {
        let mut scanner = InputScanner::default();
        let actions = scanner.scan(&[0x02, b'?']);
        assert!(matches!(actions.as_slice(), [InputAction::Help]));
        assert!(bytes(&actions).is_empty());
        // Help is consumed wherever it appears in the stream, and the
        // withheld Ctrl-b of an unbound chord still forwards.
        let mut scanner = InputScanner::default();
        let actions = scanner.scan(&[b'x', 0x02, b'?', b'y']);
        assert_eq!(bytes(&actions), b"xy");
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[1], InputAction::Help));
    }

    #[test]
    fn ctrl_b_r_redraws_without_forwarding() {
        let mut scanner = InputScanner::default();
        let actions = scanner.scan(&[0x02, b'r']);
        assert!(matches!(actions.as_slice(), [InputAction::Redraw]));
        assert!(bytes(&actions).is_empty());
        let mut scanner = InputScanner::default();
        let actions = scanner.scan(&[b'x', 0x02, b'r', b'y']);
        assert_eq!(bytes(&actions), b"xy");
        assert_eq!(actions.len(), 3);
        assert!(matches!(actions[1], InputAction::Redraw));
        // Capital R is not the chord -- tmux's refresh-client is lowercase.
        let mut scanner = InputScanner::default();
        let actions = scanner.scan(&[0x02, b'R']);
        assert_eq!(bytes(&actions), &[0x02, b'R']);
    }

    fn bytes(actions: &[InputAction]) -> Vec<u8> {
        let mut out = Vec::new();
        for a in actions {
            if let InputAction::Forward(b) = a {
                out.extend_from_slice(b);
            }
        }
        out
    }

    #[test]
    fn human_commands_parse_as_real_clap_commands() {
        // The terminal-first vocabulary must be real Clap commands and
        // visible aliases -- not argv rewriting -- so generated completions
        // and `a help` know every name.
        let args = args_of(&["here", "codex", "review"]);
        match Cli::try_parse_from(args).unwrap().command {
            Some(Commands::Here(quick)) => {
                assert_eq!(quick.rest, vec!["codex".to_string(), "review".to_string()])
            }
            _ => panic!("expected `here` command"),
        }

        let args = args_of(&["open", "review"]);
        match Cli::try_parse_from(args).unwrap().command {
            Some(Commands::Attach(attach)) => {
                assert_eq!(attach.target.selector.as_deref(), Some("review"))
            }
            _ => panic!("expected `open` alias of attach"),
        }

        let args = args_of(&["attach", "review", "--no-status"]);
        match Cli::try_parse_from(args).unwrap().command {
            Some(Commands::Attach(attach)) => assert!(attach.no_status),
            _ => panic!("expected `attach --no-status` command"),
        }

        let args = args_of(&["new", "--engine", "shell"]);
        match Cli::try_parse_from(args).unwrap().command {
            Some(Commands::New(start)) => assert_eq!(start.engine.as_deref(), Some("shell")),
            _ => panic!("expected `new` command"),
        }

        for (argv, expected) in [
            (vec!["ps"], "list"),
            (vec!["show", "x"], "status"),
            (vec!["current"], "whoami"),
            (vec!["keys"], "hotkeys"),
            (vec!["check"], "doctor"),
        ] {
            let parsed = Cli::try_parse_from(args_of(&argv)).unwrap();
            let name = parsed
                .command
                .map(|command| command_name(&command).to_string())
                .unwrap_or_else(|| "<none>".to_string());
            assert_eq!(name, expected, "argv {argv:?}");
        }

        // `start`'s terminal-first default tag, shared with `a here`/`a -`.
        let args = args_of(&["start"]);
        match Cli::try_parse_from(args).unwrap().command {
            Some(Commands::Start(start)) => assert_eq!(start.tag, DEFAULT_HUMAN_TAG),
            _ => panic!("expected start command"),
        }

        let args = args_of(&["list", "--sort", "activity"]);
        match Cli::try_parse_from(args).unwrap().command {
            Some(Commands::List(list)) => assert_eq!(list.sort, Some(ListSort::Activity)),
            _ => panic!("expected list --sort activity"),
        }
    }

    fn args_of(argv: &[&str]) -> Vec<String> {
        std::iter::once("a")
            .chain(argv.iter().copied())
            .map(str::to_string)
            .collect()
    }

    fn command_name(command: &Commands) -> &'static str {
        match command {
            Commands::Start(_) => "start",
            Commands::New(_) => "new",
            Commands::Here(_) => "here",
            Commands::List(_) => "list",
            Commands::Snapshot(_) => "snapshot",
            Commands::Attach(_) => "attach",
            Commands::Send(_) => "send",
            Commands::Capture(_) => "capture",
            Commands::Status(_) => "status",
            Commands::Kill(_) => "kill",
            Commands::Forget(_) => "forget",
            Commands::Prune => "prune",
            Commands::Rename(_) => "rename",
            Commands::Engines => "engines",
            Commands::Profiles => "profiles",
            Commands::LaunchSpec(_) => "launch-spec",
            Commands::LaunchExec(_) => "launch-exec",
            Commands::Doctor => "doctor",
            Commands::Init(_) => "init",
            Commands::Whoami => "whoami",
            Commands::StateReport(_) => "state-report",
            Commands::Message(_) => "message",
            Commands::Watch(_) => "watch",
            Commands::Transcript(_) => "transcript",
            Commands::Completions(_) => "completions",
            Commands::Hotkeys => "hotkeys",
            Commands::QuickAttach(_) => "quick-attach",
            Commands::QuickLaunch(_) => "quick-launch",
        }
    }

    #[test]
    fn uuid_like_selector_detection_does_not_consume_normal_tags() {
        assert!(looks_like_uuid_selector("01234567"));
        assert!(looks_like_uuid_selector(
            "01234567-89ab-cdef-0123-456789abcdef"
        ));
        assert!(!looks_like_uuid_selector("review"));
        assert!(!looks_like_uuid_selector("deadbee"));
        // Dashes alone carry no digits; a 7-digit quick index is not a UUID
        // prefix and stays with the quick-attach resolver.
        assert!(!looks_like_uuid_selector("-------"));
        assert!(!looks_like_uuid_selector("1234567"));
    }

    #[test]
    fn compact_elapsed_and_fit_column_render_for_dense_terminals() {
        assert_eq!(compact_elapsed(0), "now");
        assert_eq!(compact_elapsed(4_000), "now");
        assert_eq!(compact_elapsed(59_000), "59s");
        assert_eq!(compact_elapsed(120_000), "2m");
        assert_eq!(compact_elapsed(7_200_000), "2h");
        assert_eq!(compact_elapsed(7_260_000), "2h 1m");
        assert_eq!(compact_elapsed(172_800_000), "2d");
        assert_eq!(compact_elapsed(190_800_000), "2d 5h");
        assert_eq!(human_age_phrase(7_260_000), "2h 1m ago");
        assert_eq!(human_age_phrase(0), "just now");

        assert_eq!(fit_column("abcdefgh", 5), "abcd…");
        // Wide glyphs count display cells, not chars: two CJK glyphs fit a
        // 5-cell column exactly with padding, three must truncate.
        assert_eq!(terminal_display_width(&fit_column("界界界", 5)), 5);
        assert_eq!(fit_column("界界", 5), "界界 ");
    }

    #[test]
    fn group_by_workspace_sorts_by_name_created_accessed_and_activity() {
        let mut zebra = mk_record("/ws/zebra", "main", Phase::Running);
        zebra.created_at_ms = 10;
        zebra.last_accessed_ms = Some(100);
        zebra.last_activity_ms = Some(1);

        let mut apple = mk_record("/ws/apple", "main", Phase::Running);
        apple.created_at_ms = 30;
        apple.last_accessed_ms = Some(50);
        apple.last_activity_ms = Some(200);

        let mut mango = mk_record("/ws/mango", "review", Phase::Running);
        mango.created_at_ms = 20;
        mango.last_accessed_ms = None;
        mango.last_activity_ms = None;

        let records = vec![zebra, apple, mango];
        let names = |sort: ListSort, records: &[SessionRecord]| -> Vec<PathBuf> {
            group_by_workspace(records.to_vec(), sort)
                .into_iter()
                .map(|(ws, _)| ws)
                .collect()
        };

        assert_eq!(
            names(ListSort::Name, &records),
            vec![
                PathBuf::from("/ws/apple"),
                PathBuf::from("/ws/mango"),
                PathBuf::from("/ws/zebra")
            ]
        );
        assert_eq!(
            names(ListSort::Created, &records),
            vec![
                PathBuf::from("/ws/apple"),
                PathBuf::from("/ws/mango"),
                PathBuf::from("/ws/zebra")
            ]
        );
        assert_eq!(
            names(ListSort::Accessed, &records),
            vec![
                PathBuf::from("/ws/zebra"),
                PathBuf::from("/ws/apple"),
                PathBuf::from("/ws/mango")
            ]
        );
        assert_eq!(
            names(ListSort::Activity, &records),
            vec![
                PathBuf::from("/ws/apple"),
                PathBuf::from("/ws/zebra"),
                PathBuf::from("/ws/mango")
            ]
        );
    }

    #[test]
    fn ui_state_is_semantic_when_reported_and_honest_when_inferred() {
        let now: u64 = 20_000;
        let mut record = mk_record("/ws/state", "agent", Phase::Running);
        record.engine = "codex".to_string();

        // A fresh state-report push is semantic fact.
        record.reported_state = Some("waiting".to_string());
        record.reported_state_at_ms = Some(now - 500);
        assert_eq!(session_ui_state(&record, now), ("waiting", "reported"));
        assert!(ui_state_needs_attention("waiting"));
        assert!(ui_state_is_active("waiting"));

        record.reported_state = Some("working".to_string());
        assert_eq!(session_ui_state(&record, now), ("working", "reported"));
        record.reported_state = Some("idle".to_string());
        assert_eq!(session_ui_state(&record, now), ("idle", "reported"));
        assert!(!ui_state_needs_attention("idle"));

        // Stale push: only PTY-recency evidence, so only activity words.
        record.reported_state = Some("waiting".to_string());
        record.reported_state_at_ms = Some(now.saturating_sub(60_000));
        record.last_activity_ms = Some(now - 500);
        assert_eq!(session_ui_state(&record, now), ("active", "activity"));
        record.last_activity_ms = Some(now - 5_000);
        assert_eq!(session_ui_state(&record, now), ("quiet", "activity"));
        // Quiet is deliberately not attention: silence is not a reported wait.
        assert!(!ui_state_needs_attention("quiet"));
    }

    #[test]
    fn ui_state_does_not_guess_agent_semantics_for_shells_or_corpses() {
        let now: u64 = 20_000;
        // A plain shell (no agent-state push ever) stays `running` however
        // quiet its PTY is.
        let mut record = mk_record("/ws/state", "shell", Phase::Running);
        record.last_activity_ms = Some(now.saturating_sub(60_000));
        assert_eq!(session_ui_state(&record, now), ("running", "lifecycle"));

        // But a fresh hook push inside a shell session is fact, not a
        // guess: the agent was started by hand, `APLEXER_SESSION_ID` is
        // still injected, and without this an idle agent in a shell
        // session would read `running` forever.
        record.reported_state = Some("idle".to_string());
        record.reported_state_at_ms = Some(now);
        assert_eq!(session_ui_state(&record, now), ("idle", "reported"));
        record.reported_state = Some("waiting".to_string());
        assert_eq!(session_ui_state(&record, now), ("waiting", "reported"));
        record.reported_state = Some("working".to_string());
        assert_eq!(session_ui_state(&record, now), ("working", "reported"));

        // A stale push no longer falls back to plain `running`: a shell an
        // agent has lived in gets the same activity words as a first-class
        // engine once nothing is fresh. Its long-quiet PTY is an agent
        // resting at a prompt (or thinking), not a shell doing work.
        record.reported_state_at_ms = Some(now.saturating_sub(60_000));
        assert_eq!(session_ui_state(&record, now), ("quiet", "activity"));

        // And an idle push with no PTY output since it landed stays
        // authoritative however old it gets -- a rest has no follow-up
        // push to refresh it, so expiring it on the clock is what made
        // resting agents read RUNNING.
        record.reported_state = Some("idle".to_string());
        assert_eq!(session_ui_state(&record, now), ("idle", "reported"));

        // Non-terminal phase + dead worker = broken, regardless of what the
        // record still claims or what was last reported.
        let mut corpse = mk_record("/ws/state", "agent", Phase::Running);
        corpse.engine = "codex".to_string();
        corpse.worker_pid = None;
        corpse.reported_state = Some("working".to_string());
        corpse.reported_state_at_ms = Some(now);
        assert_eq!(session_ui_state(&corpse, now), ("broken", "lifecycle"));
        assert!(ui_state_needs_attention("broken"));
        // ... but a Starting record with no worker pid yet is the shape
        // every healthy `a start` persists first, and the TTY UI must not
        // paint that as a corpse (issue #9). Age is the only thing that
        // turns it into one.
        let mut creating = mk_record("/ws/state", "creating", Phase::Starting);
        creating.worker_pid = None;
        creating.created_at_ms = now;
        assert_eq!(
            session_ui_state(&creating, now + 1),
            ("starting", "lifecycle")
        );
        assert_eq!(
            session_ui_state(&creating, now + DEFAULT_STARTUP_TIMEOUT_MS - 1),
            ("starting", "lifecycle")
        );
        assert_eq!(
            session_ui_state(&creating, now + DEFAULT_STARTUP_TIMEOUT_MS),
            ("broken", "lifecycle")
        );
    }

    /// The default list's corpse filter: `exited` is the one state that is
    /// neither active nor needs-attention, so it is the only one hidden.
    /// The failure states a human may need to see (oom, failed, broken) and
    /// a healthy Starting session that has not registered its worker pid
    /// yet (issue #9's startup window) all stay listed.
    #[test]
    fn only_exited_sessions_drop_out_of_the_default_list() {
        let now = now_ms();
        let mut exited = mk_record("/ws/f", "done", Phase::Exited);
        exited.worker_pid = None;
        assert!(!session_is_listed(&exited, now));

        let mut oom = mk_record("/ws/f", "oomed", Phase::Exited);
        oom.worker_pid = None;
        oom.exit = Some(aplexer::ExitInfo {
            code: None,
            signal: None,
            oom_killed: true,
            exited_at_ms: now,
        });
        assert!(
            session_is_listed(&oom, now),
            "oom needs attention, not a corpse"
        );

        let failed = mk_record("/ws/f", "failed", Phase::Failed);
        assert!(session_is_listed(&failed, now));

        let mut broken = mk_record("/ws/f", "broken", Phase::Running);
        broken.worker_pid = None;
        assert!(session_is_listed(&broken, now));

        let mut creating = mk_record("/ws/f", "creating", Phase::Starting);
        creating.worker_pid = None;
        creating.created_at_ms = now;
        assert!(
            session_is_listed(&creating, now + 1),
            "the startup window must not read as a corpse (issue #9)"
        );
    }

    #[test]
    fn overlay_reported_state_takes_the_live_activity_stamp_too() {
        let now: u64 = 200_000;
        let mut record = mk_record("/ws/state", "shell", Phase::Running);
        // Attach-time snapshot: output predates the attach; nothing has
        // been reported since.
        record.last_activity_ms = Some(now - 30_000);

        // The worker's live answer: the agent has since said `idle`, and
        // the PTY has been silent since the push. The rest is authoritative
        // even though the snapshot itself knows nothing of it.
        let raw = json!({
            "reported_state": "idle",
            "reported_state_at_ms": now - 1_000,
            "last_activity_ms": now - 2_000,
        });
        let overlaid = overlay_reported_state(&record, Some(&raw));
        assert_eq!(
            session_ui_state(&overlaid, now),
            ("idle", "reported"),
            "a rest with no output since the push is idle, however stale the attach snapshot"
        );

        // The same rest, but the live activity stamp says output arrived
        // after it: the snapshot's old stamp must not keep the idle claim
        // alive once the agent (or the user at the prompt) produced output.
        let raw = json!({
            "reported_state": "idle",
            "reported_state_at_ms": now - 5_000,
            "last_activity_ms": now - 500,
        });
        let overlaid = overlay_reported_state(&record, Some(&raw));
        assert_eq!(
            session_ui_state(&overlaid, now),
            ("active", "activity"),
            "newer live output retracts the rest even though the snapshot predates it"
        );

        // A failed Status RPC leaves the snapshot untouched...
        let untouched = overlay_reported_state(&record, None);
        assert_eq!(untouched.reported_state, None);
        assert_eq!(untouched.last_activity_ms, Some(now - 30_000));

        // ...and so does a worker too old to send the activity field.
        let old_worker = json!({ "reported_state": "idle" });
        let degraded = overlay_reported_state(&record, Some(&old_worker));
        assert_eq!(degraded.reported_state.as_deref(), Some("idle"));
        assert_eq!(degraded.last_activity_ms, Some(now - 30_000));
    }

    /// An accepted kill persists `phase: exiting` before teardown (issue
    /// #18), so the whole finalization window must render as a dying
    /// session, not as healthy -- and once the worker dies mid-finalization
    /// it must fall through to `broken`, the shape prune reaps, never back
    /// to a live-looking word.
    #[test]
    fn ui_state_shows_a_killed_session_as_stopping_while_it_dies() {
        let now: u64 = 20_000;
        // Worker still alive mid-finalization (the kill window, however long
        // finalization takes): "stopping", not "running".
        let dying = mk_record("/ws/state", "dying", Phase::Exiting);
        assert_eq!(session_ui_state(&dying, now), ("stopping", "lifecycle"));

        // Worker gone before finalization wrote a terminal phase: the
        // contradicted-phase rule owns the row now.
        let mut corpse = mk_record("/ws/state", "dying", Phase::Exiting);
        corpse.worker_pid = None;
        assert_eq!(session_ui_state(&corpse, now), ("broken", "lifecycle"));
    }

    /// Issue #9's second half. The worker persists the record, then its pid,
    /// then binds the control socket -- so a client racing a healthy start
    /// finds either no pid or no socket. Both used to be reported as
    /// destroyed state with `a kill` as the remedy.
    #[test]
    fn check_attachable_does_not_advise_killing_a_still_starting_session() {
        let now = now_ms();

        // Before the worker registers a pid.
        let mut pre_pid = mk_record("/ws/a", "main", Phase::Starting);
        pre_pid.worker_pid = None;
        pre_pid.created_at_ms = now;
        let err = check_attachable(&pre_pid).unwrap_err().to_string();
        assert!(err.contains("still starting"), "{err}");
        assert!(!err.contains("a kill"), "{err}");

        // Pid registered, socket not bound yet.
        let mut pre_socket = mk_record("/ws/a", "main", Phase::Starting);
        pre_socket.created_at_ms = now;
        pre_socket.socket_path = PathBuf::from("/definitely/does/not/exist/control.sock");
        let err = check_attachable(&pre_socket).unwrap_err().to_string();
        assert!(err.contains("still starting"), "{err}");
        assert!(!err.contains("removed out from under it"), "{err}");
        assert!(!err.contains("a kill"), "{err}");

        // Past the startup budget these really are wreckage, and the
        // original advice is the right advice again.
        let mut expired_pre_pid = pre_pid.clone();
        expired_pre_pid.created_at_ms = now.saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS);
        let err = check_attachable(&expired_pre_pid).unwrap_err().to_string();
        assert!(err.contains("worker is not running"), "{err}");
        assert!(err.contains("state: broken"), "{err}");

        let mut expired_pre_socket = pre_socket.clone();
        expired_pre_socket.created_at_ms = now.saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS);
        let err = check_attachable(&expired_pre_socket)
            .unwrap_err()
            .to_string();
        assert!(err.contains("control socket is gone"), "{err}");
        assert!(
            err.contains(&format!("a kill {}", expired_pre_socket.id)),
            "{err}"
        );

        // The guard must not stand in the way of the ordinary path: a
        // Starting session whose worker is up and listening is attachable.
        let mut ready = mk_record("/ws/a", "main", Phase::Starting);
        ready.created_at_ms = now;
        assert!(check_attachable(&ready).is_ok());
    }

    #[test]
    fn scan_ctrl_b_n_asks_for_a_new_session() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, b'n']);
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Switch(SwitchTarget::New)]
        ));
        // Split across reads, like every other chord: the prefix state has to
        // survive the read() boundary.
        let mut split = InputScanner::default();
        assert!(split.scan(&[0x02]).is_empty());
        assert!(matches!(
            split.scan(b"n").as_slice(),
            [InputAction::Switch(SwitchTarget::New)]
        ));
        // And the chord bytes never reach the workload -- pressing it must
        // not type an `n` into whatever has the prompt.
        let mut mixed = InputScanner::default();
        let actions = mixed.scan(&[b'a', 0x02, b'n', b'z']);
        assert_eq!(actions.len(), 3);
        match &actions[0] {
            InputAction::Forward(b) => assert_eq!(b, b"a"),
            _ => panic!("expected the pre-chord byte forwarded"),
        }
        assert!(matches!(actions[1], InputAction::Switch(SwitchTarget::New)));
        match &actions[2] {
            InputAction::Forward(b) => assert_eq!(b, b"z"),
            _ => panic!("expected the post-chord byte forwarded"),
        }
    }

    /// Session navigation lives on Right/Left and workspace navigation on
    /// Down/Up, in *both* encodings a terminal can send them in: CSI
    /// (`ESC [ C`) in normal cursor mode and SS3 (`ESC O C`) in application
    /// cursor mode, which a TUI in the session can turn on at any moment.
    #[test]
    fn scan_ctrl_b_arrows_navigate_in_both_cursor_modes() {
        for introducer in [b'[', b'O'] {
            for (final_byte, expected) in [
                (b'C', SwitchTarget::Next),
                (b'D', SwitchTarget::Prev),
                (b'B', SwitchTarget::NextWorkspace),
                (b'A', SwitchTarget::PrevWorkspace),
            ] {
                let mut s = InputScanner::default();
                let actions = s.scan(&[0x02, 0x1b, introducer, final_byte]);
                match actions.as_slice() {
                    [InputAction::Switch(target)] => assert_eq!(
                        *target, expected,
                        "ESC {} {} should mean {expected:?}",
                        introducer as char, final_byte as char
                    ),
                    other => panic!(
                        "ESC {} {} was not consumed as a chord ({} action(s))",
                        introducer as char,
                        final_byte as char,
                        other.len()
                    ),
                }
            }
        }
    }

    /// A terminal writes an escape sequence in one `write`, but a PTY read can
    /// still split it anywhere -- every prefix has to survive the boundary,
    /// including `Ctrl-b` and `ESC` landing in different reads.
    #[test]
    fn scan_ctrl_b_arrow_split_across_every_read_boundary() {
        let chord: &[u8] = &[0x02, 0x1b, b'[', b'C'];
        for split in 1..chord.len() {
            let mut s = InputScanner::default();
            let first = s.scan(&chord[..split]);
            assert!(
                first.is_empty(),
                "a partial chord split at {split} must emit nothing yet"
            );
            assert!(
                s.awaiting_escape() || split == 1,
                "split at {split} should leave the scanner holding escape bytes"
            );
            assert!(
                matches!(
                    s.scan(&chord[split..]).as_slice(),
                    [InputAction::Switch(SwitchTarget::Next)]
                ),
                "a chord split at {split} did not resolve"
            );
        }
    }

    /// `Ctrl-b` then a bare `ESC` is not a chord: once the input thread stops
    /// waiting for the rest of an arrow (`CHORD_ESCAPE_TIMEOUT`), both
    /// withheld bytes go to the workload, so an editor still gets its Escape.
    #[test]
    fn scan_ctrl_b_escape_flushes_when_no_arrow_follows() {
        let mut s = InputScanner::default();
        assert!(s.scan(&[0x02, 0x1b]).is_empty());
        assert!(s.awaiting_escape());
        assert_eq!(bytes(&s.flush_pending()), vec![0x02, 0x1b]);
        assert!(!s.awaiting_escape());
        // Nothing is left behind: the next key is scanned from scratch.
        assert!(matches!(
            s.scan(&[0x02, b'n']).as_slice(),
            [InputAction::Switch(SwitchTarget::New)]
        ));

        // A lone `Ctrl-b` is *not* flushed: waiting for its second key is the
        // keymap's contract, and only the multi-byte arrows are ambiguous.
        let mut lone = InputScanner::default();
        assert!(lone.scan(&[0x02]).is_empty());
        assert!(!lone.awaiting_escape());
        assert!(lone.flush_pending().is_empty());
    }

    /// An escape sequence after the prefix that is *not* an arrow (Home, F1,
    /// ...) forwards every withheld byte in order rather than swallowing any.
    #[test]
    fn scan_ctrl_b_non_arrow_escape_forwards_every_byte() {
        let mut s = InputScanner::default();
        assert_eq!(bytes(&s.scan(&[0x02, 0x1b, b'[', b'H'])), {
            let mut expected = vec![0x02, 0x1b];
            expected.extend_from_slice(b"[H");
            expected
        });
        let mut alt = InputScanner::default();
        assert_eq!(
            bytes(&alt.scan(&[0x02, 0x1b, b'x'])),
            vec![0x02, 0x1b, b'x']
        );
    }

    /// `p` was only ever the other half of `n`/`p`. With `n` now meaning
    /// "new", a lone "previous" on `p` would be a trap, so it is unbound and
    /// forwards -- and `N`/`P` are untouched.
    #[test]
    fn scan_ctrl_b_p_is_unbound_but_capital_p_still_switches() {
        let mut s = InputScanner::default();
        assert_eq!(bytes(&s.scan(&[0x02, b'p'])), vec![0x02, b'p']);
        assert!(matches!(
            s.scan(&[0x02, b'N']).as_slice(),
            [InputAction::Switch(SwitchTarget::NextGlobal)]
        ));
        assert!(matches!(
            s.scan(&[0x02, b'P']).as_slice(),
            [InputAction::Switch(SwitchTarget::PrevGlobal)]
        ));
    }

    /// The arrow chords must not have eaten the bracket-ish keys next to
    /// them: `[` is still the pager and the digits still jump.
    #[test]
    fn scan_ctrl_b_bracket_and_digits_survive_the_arrow_chords() {
        let mut s = InputScanner::default();
        assert!(matches!(
            s.scan(&[0x02, b'[']).as_slice(),
            [InputAction::Scroll]
        ));
        assert!(matches!(
            s.scan(&[0x02, b'4']).as_slice(),
            [InputAction::Switch(SwitchTarget::Index(4))]
        ));
        assert!(matches!(
            s.scan(&[0x02, b'l']).as_slice(),
            [InputAction::Switch(SwitchTarget::Last)]
        ));
        assert!(matches!(
            s.scan(&[0x02, b'r']).as_slice(),
            [InputAction::Redraw]
        ));
        assert!(matches!(
            s.scan(&[0x02, b'?']).as_slice(),
            [InputAction::Help]
        ));
    }

    /// The keymap has exactly one definition; the three renderings are views
    /// of it. This is the guard on that: each must mention every bound key,
    /// and the table must still spell the bindings the scanner implements.
    #[test]
    fn the_key_reference_is_generated_from_the_binding_table() {
        let help = attach_key_help();
        assert!(help.starts_with("Ctrl-b: "));
        // The overlay is the third view. Rendered at a size with room for
        // everything, it has to carry every key the table does -- a binding
        // that only reaches two of the three renderings is exactly the drift
        // this table exists to make impossible.
        let overlay = key_overlay_lines(40, 120).expect("40x120 fits the whole keymap");
        for binding in ATTACH_BINDINGS {
            assert!(
                overlay.iter().any(|line| line.contains(binding.keys)),
                "the key overlay dropped {:?}:\n{}",
                binding.keys,
                overlay.join("\n")
            );
            assert!(
                overlay
                    .iter()
                    .any(|line| line.contains(binding.description)),
                "the key overlay dropped {:?}:\n{}",
                binding.description,
                overlay.join("\n")
            );
        }
        for binding in ATTACH_BINDINGS {
            if let Some(brief) = binding.brief {
                assert!(
                    help.contains(brief),
                    "the status-bar reference dropped {brief:?}: {help}"
                );
            }
        }
        // The bindings the scanner actually implements, spelled as the table
        // spells them -- a binding added to the scanner and forgotten here (or
        // the reverse) fails this.
        let keys: Vec<&str> = ATTACH_BINDINGS.iter().map(|b| b.keys).collect();
        assert_eq!(
            keys,
            vec![
                "Right / Left",
                "Down / Up",
                "n",
                "d",
                "[",
                "N / P",
                "1-9",
                "l",
                "r",
                "?"
            ]
        );
    }

    /// The overlay is the third *view* of `ATTACH_BINDINGS`, not a third
    /// copy of it: on a terminal with room for the whole table, every key and
    /// every description the table holds is on screen verbatim. A binding
    /// added to the scanner and the table shows up here for free; one written
    /// out by hand could not.
    #[test]
    fn key_overlay_renders_every_binding_from_the_table() {
        let lines = key_overlay_lines(40, 100).expect("40x100 has room for the whole keymap");
        assert_eq!(
            lines.len(),
            ATTACH_BINDINGS.len() + KEY_OVERLAY_CHROME_ROWS,
            "every binding gets a row, plus two borders and the footer: {lines:#?}"
        );
        for (binding, line) in ATTACH_BINDINGS.iter().zip(&lines[1..]) {
            assert!(
                line.contains(binding.keys),
                "the overlay dropped the keys {:?}: {line}",
                binding.keys
            );
            assert!(
                line.contains(binding.description),
                "the overlay truncated {:?} on a terminal with room for it: {line}",
                binding.description
            );
        }
        assert!(
            lines[0].contains("Ctrl-b"),
            "the box has to name the key the user is waiting on: {}",
            lines[0]
        );
    }

    /// Every row is one uniform width that fits inside the terminal, and the
    /// box never claims more rows than it was given. This is the "do not draw
    /// outside the screen" guarantee stated over the layout rather than left
    /// to the sequence writer, because the sequence addresses rows absolutely
    /// and a box one row too tall would land on the status bar.
    #[test]
    fn key_overlay_lines_are_uniform_and_stay_inside_the_terminal() {
        for rows in [6usize, 9, 13, 23, 40, 200] {
            for cols in [32usize, 40, 46, 80, 100, 200] {
                let Some(lines) = key_overlay_lines(rows, cols) else {
                    continue;
                };
                assert!(
                    lines.len() <= rows,
                    "a {rows}x{cols} box claimed {} rows",
                    lines.len()
                );
                let width = terminal_display_width(&lines[0]);
                assert!(width <= cols, "a {rows}x{cols} box is {width} cells wide");
                for line in &lines {
                    assert_eq!(
                        terminal_display_width(line),
                        width,
                        "ragged row in a {rows}x{cols} box: {line}"
                    );
                }
            }
        }
    }

    /// Honest degradation, part one: a terminal with room for some of the
    /// keymap gets some of it -- trimmed from the end, because the table is
    /// ordered most-useful-first -- and is told how much it is not seeing.
    #[test]
    fn key_overlay_trims_from_the_end_and_says_how_much_it_dropped() {
        let rows = KEY_OVERLAY_CHROME_ROWS + 4;
        let lines = key_overlay_lines(rows, 80).expect("four bindings still fit");
        assert_eq!(lines.len(), rows);
        let dropped = ATTACH_BINDINGS.len() - 4;
        let footer = &lines[lines.len() - 2];
        assert!(
            footer.contains(&format!("{dropped} more")),
            "a trimmed box must say how many bindings it dropped: {footer}"
        );
        for binding in &ATTACH_BINDINGS[..4] {
            assert!(
                lines[1..5].iter().any(|l| l.contains(binding.keys)),
                "the first four table entries are the ones kept, missing {:?}",
                binding.keys
            );
        }
        let full = key_overlay_lines(40, 80).expect("40 rows fit everything");
        assert!(
            full[full.len() - 2].contains("Esc dismiss"),
            "an untrimmed box says how to get out, not how much is missing: {}",
            full[full.len() - 2]
        );
    }

    /// Honest degradation, part two: below a box worth drawing there is no
    /// box. `show_key_overlay` reads this `None` as "flash the one-line
    /// reference instead", which is the whole of the small-terminal story --
    /// no half-drawn border, no writing past the last column.
    #[test]
    fn key_overlay_refuses_a_terminal_it_cannot_fit() {
        // Too short: chrome plus fewer than KEY_OVERLAY_MIN_BINDINGS rows.
        for rows in 0..KEY_OVERLAY_CHROME_ROWS + KEY_OVERLAY_MIN_BINDINGS {
            assert!(
                key_overlay_lines(rows, 200).is_none(),
                "{rows} rows is not enough for a box worth reading"
            );
        }
        assert!(
            key_overlay_lines(KEY_OVERLAY_CHROME_ROWS + KEY_OVERLAY_MIN_BINDINGS, 200).is_some()
        );
        // Too narrow: the description column would stop being sentences.
        let keys_width = ATTACH_BINDINGS[..KEY_OVERLAY_MIN_BINDINGS]
            .iter()
            .map(|b| terminal_display_width(b.keys))
            .max()
            .unwrap();
        let narrowest = keys_width + 6 + KEY_OVERLAY_MIN_DESC;
        assert!(key_overlay_lines(40, narrowest - 1).is_none());
        assert!(key_overlay_lines(40, narrowest).is_some());
        assert!(key_overlay_lines(40, 0).is_none());
    }

    /// The sequence addresses rows absolutely, so the rows it addresses are
    /// the guarantee: never row 0, never the reserved status row, never past
    /// the bottom of the terminal.
    #[test]
    fn key_overlay_sequence_never_addresses_the_status_bar_row() {
        for (rows, reserved) in [(24u16, true), (24, false), (13, true), (40, true)] {
            let geom = TermGeom {
                rows,
                cols: 80,
                reserved,
            };
            let usable = key_overlay_rows(geom);
            let lines = key_overlay_lines(usable as usize, 80).expect("80 columns fit a box");
            let seq = key_overlay_sequence(geom, &lines);
            let text = String::from_utf8(seq).expect("the sequence is utf-8");
            let mut addressed: Vec<u16> = Vec::new();
            let mut rest = text.as_str();
            while let Some(at) = rest.find("\x1b[") {
                rest = &rest[at + 2..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() && rest[digits.len()..].starts_with(";1H") {
                    addressed.push(digits.parse().expect("a row number"));
                }
            }
            assert_eq!(
                addressed.len(),
                lines.len(),
                "one absolute address per row, got {addressed:?}"
            );
            assert_eq!(*addressed.first().unwrap(), usable - lines.len() as u16 + 1);
            assert_eq!(
                *addressed.last().unwrap(),
                usable,
                "the box sits directly above the status bar"
            );
            for row in addressed {
                assert!(
                    (1..=usable).contains(&row),
                    "the box addressed row {row} on a {rows}-row terminal (usable {usable})"
                );
            }
        }
    }

    /// The two deadlines a held prefix can be under -- `KEY_OVERLAY_DELAY`
    /// and `CHORD_ESCAPE_TIMEOUT` -- are armed by mutually exclusive scanner
    /// states, which is what stops them fighting: a partial arrow chord can
    /// never pop the overlay, and a bare `Ctrl-b ESC` never waits for one
    /// deadline plus the other.
    #[test]
    fn the_overlay_deadline_and_the_chord_deadline_are_never_armed_together() {
        let mut s = InputScanner::default();
        assert!(s.settled(), "an idle scanner is under neither deadline");
        assert!(!s.awaiting_key() && !s.awaiting_escape());

        assert!(s.scan(&[0x02]).is_empty());
        assert!(
            s.awaiting_key(),
            "a lone Ctrl-b arms the overlay's deadline"
        );
        assert!(!s.awaiting_escape());
        assert!(!s.settled());

        // The moment the arrow's ESC arrives the overlay's deadline is gone
        // and the chord's is the only one left.
        assert!(s.scan(&[0x1b]).is_empty());
        assert!(s.awaiting_escape());
        assert!(!s.awaiting_key());
        assert!(!s.settled());

        // ... and completing the chord leaves neither armed.
        assert!(matches!(
            s.scan(b"[C").as_slice(),
            [InputAction::Switch(SwitchTarget::Next)]
        ));
        assert!(s.settled());
        assert!(!s.awaiting_key() && !s.awaiting_escape());

        // A bound key resolves the prefix in one step, which is what makes
        // the input thread take the overlay down before running its action.
        let mut fast = InputScanner::default();
        assert!(matches!(
            fast.scan(&[0x02, b'd']).as_slice(),
            [InputAction::Detach]
        ));
        assert!(fast.settled());
        // So does an unbound one, which also still falls through.
        let mut through = InputScanner::default();
        assert_eq!(bytes(&through.scan(&[0x02, b'p'])), vec![0x02, b'p']);
        assert!(through.settled());
    }

    /// The delay has to be long enough that a chord typed from muscle memory
    /// resolves first, and its whole point is that it is a *different* wait
    /// from the arrow chord's -- long enough to read as hesitation where 100ms
    /// reads as a split escape sequence.
    #[test]
    fn the_overlay_delay_is_a_hesitation_not_a_chord_gap() {
        assert!(
            KEY_OVERLAY_DELAY > CHORD_ESCAPE_TIMEOUT,
            "an overlay that can fire inside the arrow chord's own deadline \
             would flicker on every Ctrl-b Left"
        );
        assert!(
            KEY_OVERLAY_DELAY < FLASH_DURATION,
            "hesitation has to be answered faster than a message is read"
        );
    }

    /// `SwitchTarget::New` is created, never selected: `pick_switch_target`
    /// must say so rather than quietly resolving somewhere.
    #[test]
    fn pick_switch_target_refuses_to_select_a_new_session() {
        let ws = PathBuf::from("/ws/new");
        let a = mk_record("/ws/new", "a", Phase::Running);
        let groups = vec![(ws.clone(), vec![a.clone()])];
        let error = pick_switch_target(&groups, &ws, a.id, SwitchTarget::New, None)
            .expect_err("New must not be selectable");
        assert!(
            format!("{error:#}").contains("created, not selected"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn scan_ctrl_q_through_ctrl_y_are_forwarded() {
        let mut s = InputScanner::default();
        for byte in 0x11u8..=0x19 {
            let actions = s.scan(&[byte]);
            assert_eq!(bytes(&actions), vec![byte]);
        }
    }

    #[test]
    fn scan_control_bytes_preserve_input_order() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[b'a', 0x13, b'z']);
        assert_eq!(bytes(&actions), vec![b'a', 0x13, b'z']);
    }

    #[test]
    fn scan_non_shortcut_control_bytes_still_forward() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x10, 0x1a, b'1', b'9']);
        assert_eq!(bytes(&actions), vec![0x10, 0x1a, b'1', b'9']);
        assert!(actions
            .iter()
            .all(|action| matches!(action, InputAction::Forward(_))));
    }

    #[test]
    fn scan_split_across_reads() {
        let mut s = InputScanner::default();
        assert!(s.scan(&[0x02]).is_empty());
        let actions = s.scan(b"N");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Switch(SwitchTarget::NextGlobal)]
        ));
    }

    #[test]
    fn scan_unbound_ctrl_b_forwards_both_bytes() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, b'x']);
        match actions.as_slice() {
            [InputAction::Forward(b)] => assert_eq!(b, &[0x02, b'x']),
            other => panic!("unexpected: {}", other.len()),
        }
    }

    #[test]
    fn scan_forward_switch_forward() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[b'a', 0x02, b'3', b'z']);
        assert_eq!(actions.len(), 3);
        match &actions[0] {
            InputAction::Forward(b) => assert_eq!(b, b"a"),
            _ => panic!("expected Forward"),
        }
        assert!(matches!(
            &actions[1],
            InputAction::Switch(SwitchTarget::Index(3))
        ));
        match &actions[2] {
            InputAction::Forward(b) => assert_eq!(b, b"z"),
            _ => panic!("expected Forward"),
        }
    }

    #[test]
    fn scan_ctrl_b_d_detaches() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, b'd']);
        assert!(matches!(actions.as_slice(), [InputAction::Detach]));
    }

    #[test]
    fn scan_ctrl_bracket_forwards_rest() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[b'a', 0x1d, b'b', b'c']);
        match &actions[0] {
            InputAction::Forward(b) => assert_eq!(b, &[b'a', 0x1d, b'b', b'c']),
            _ => panic!("expected Forward"),
        }
    }

    #[test]
    fn scan_double_ctrl_b_then_d() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, 0x02, b'd']);
        assert_eq!(actions.len(), 2);
        match &actions[0] {
            InputAction::Forward(b) => assert_eq!(b, &[0x02]),
            _ => panic!("expected Forward"),
        }
        assert!(matches!(&actions[1], InputAction::Detach));
    }

    #[test]
    fn scan_ctrl_b_zero_forwards_both() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, b'0']);
        assert_eq!(bytes(&actions), vec![0x02, b'0']);
    }

    // -- scroll mode: key decoding and offset arithmetic -------------------

    /// The property the whole mode rests on: in scroll mode `scroll_keys`
    /// classifies every byte as either a command or `Ignored`, and both are
    /// *consumed*. There is no third answer that could let a keystroke reach
    /// the workload.
    #[test]
    fn scroll_mode_consumes_every_byte_it_is_given() {
        // Ordinary typing, control characters, a paste, an unknown CSI, a
        // non-wheel mouse report, UTF-8.
        for chunk in [
            b"hello world".as_slice(),
            b"\x7f\x0d\x0a\x09".as_slice(),
            b"\x1b[200~pasted text\x1b[201~".as_slice(),
            b"\x1b[1;2R".as_slice(),
            b"\x1b[<0;10;5M".as_slice(),
            "naïve — ünïcode".as_bytes(),
            b"\x1b[Z\x1bOP\x1b[15~".as_slice(),
        ] {
            let mut i = 0;
            let mut guard = 0;
            while i < chunk.len() {
                guard += 1;
                assert!(guard < 1000, "scroll_keys made no progress on {chunk:?}");
                match scroll_keys(&chunk[i..]) {
                    ScrollKey::Command(_, n) | ScrollKey::Ignored(n) => {
                        assert!(n > 0, "a zero-length consume would spin forever");
                        i += n;
                    }
                    ScrollKey::Incomplete => panic!(
                        "a complete chunk must never be Incomplete: {:?} at {i}",
                        String::from_utf8_lossy(chunk)
                    ),
                }
            }
            assert_eq!(i, chunk.len(), "consumed past the end of {chunk:?}");
        }
    }

    #[test]
    fn scroll_keys_navigation_bindings() {
        use ScrollCommand::*;
        for (bytes, expected) in [
            (b"\x1b[5~".as_slice(), PageUp),
            (b"\x1b[6~".as_slice(), PageDown),
            (b"\x1b[5;2~".as_slice(), PageUp), // shifted PageUp
            (b"\x1b[A".as_slice(), Up(1)),
            (b"\x1b[B".as_slice(), Down(1)),
            (b"\x1bOA".as_slice(), Up(1)),
            (b"\x1bOB".as_slice(), Down(1)),
            (b"\x1b[H".as_slice(), Top),
            (b"\x1b[F".as_slice(), Bottom),
            (b"\x1b[1~".as_slice(), Top),
            (b"\x1b[4~".as_slice(), Bottom),
            (b"k".as_slice(), Up(1)),
            (b"j".as_slice(), Down(1)),
            (b" ".as_slice(), PageDown),
            (b"b".as_slice(), PageUp),
            (b"u".as_slice(), HalfUp),
            (b"d".as_slice(), HalfDown),
            (b"g".as_slice(), Top),
            (b"G".as_slice(), Bottom),
            (b"q".as_slice(), Exit),
            (b"\x03".as_slice(), Exit),
            (b"\x1b".as_slice(), Exit),
            (b"\x1b[<64;10;5M".as_slice(), Up(WHEEL_LINES)),
            (b"\x1b[<65;10;5M".as_slice(), Down(WHEEL_LINES)),
        ] {
            match scroll_keys(bytes) {
                ScrollKey::Command(command, consumed) => {
                    assert_eq!(
                        command,
                        expected,
                        "for {:?}",
                        String::from_utf8_lossy(bytes)
                    );
                    assert_eq!(
                        consumed,
                        bytes.len(),
                        "for {:?}",
                        String::from_utf8_lossy(bytes)
                    );
                }
                other => panic!("{:?} gave {other:?}", String::from_utf8_lossy(bytes)),
            }
        }
    }

    /// A wheel *release* report (`m`) must not move the view a second time,
    /// or one notch would scroll twice as far as tmux's.
    #[test]
    fn scroll_keys_wheel_release_is_swallowed_not_repeated() {
        assert_eq!(
            scroll_keys(b"\x1b[<64;10;5m"),
            ScrollKey::Ignored(b"\x1b[<64;10;5m".len())
        );
    }

    /// An arrow key or mouse report arriving in two `read()`s is held, not
    /// mistaken for something else.
    #[test]
    fn scroll_keys_split_sequences_are_incomplete_not_misread() {
        assert_eq!(scroll_keys(b"\x1b["), ScrollKey::Incomplete);
        assert_eq!(scroll_keys(b"\x1bO"), ScrollKey::Incomplete);
        assert_eq!(scroll_keys(b"\x1b[<64;10"), ScrollKey::Incomplete);
        // ...but not forever: a stray `ESC [` followed by junk is bounded.
        let long = [b"\x1b[".as_slice(), &[b'1'; 40]].concat();
        assert_eq!(scroll_keys(&long), ScrollKey::Ignored(long.len()));
    }

    /// The bar has to name the mode and the position at every width, because
    /// it is the only thing telling the user where their keystrokes go.
    #[test]
    fn scroll_bar_text_keeps_the_mode_and_position_at_every_width() {
        let view = ScrollView {
            offset: 12,
            available: 2000,
        };
        for cols in [10usize, 20, 40, 80, 200] {
            let text = scroll_bar_text(view, cols, false);
            assert_eq!(
                terminal_display_width(&text),
                cols,
                "the bar must fill exactly its row at {cols} columns"
            );
            assert!(
                text.contains("SCROLL"),
                "the mode must be named at {cols} columns, got {text:?}"
            );
            if cols >= 20 {
                assert!(
                    text.contains("12/2000"),
                    "the position must survive at {cols} columns, got {text:?}"
                );
            }
        }
    }

    /// An empty pager must say *why* it is empty, in both of the two ways a
    /// pager can be empty. The primary-screen case used to show the generic
    /// "PgUp/PgDn" hint over a pager that could not move, which reads as a
    /// broken feature rather than an answer -- and it is the case a
    /// full-screen TUI that repaints in place produces, which is most of what
    /// runs under aplexer.
    #[test]
    fn an_empty_pager_says_why_it_is_empty() {
        let empty = ScrollView {
            offset: 0,
            available: 0,
        };
        let alt = scroll_bar_text(empty, 120, true);
        assert!(
            alt.contains("no history: the workload owns the screen"),
            "an alt-screen workload's empty pager must name the reason: {alt:?}"
        );
        let primary = scroll_bar_text(empty, 120, false);
        assert!(
            primary.contains("no history"),
            "an empty pager on the primary screen must say so too, not offer \
             navigation keys that cannot do anything: {primary:?}"
        );
        assert!(
            !primary.contains("the workload owns the screen"),
            "...and must not blame the alternate screen when it is not in use: {primary:?}"
        );
        // The reason has to survive an ordinary terminal, not just a wide one.
        // It used to be the first thing the width ladder dropped, which left
        // exactly the row the user reported: `SCROLL 0/0 · q live`, with no
        // hint that the emptiness was the answer rather than a failure.
        for cols in [80usize, 100, 200] {
            for alt in [true, false] {
                let text = scroll_bar_text(empty, cols, alt);
                assert!(
                    text.contains("no history"),
                    "the reason must fit a {cols}-column terminal (alt={alt}): {text:?}"
                );
                assert!(
                    !text.contains("PgUp"),
                    "a pager that cannot move must not offer keys to move it: {text:?}"
                );
                assert_eq!(terminal_display_width(&text), cols);
            }
        }
        // A pager with history says nothing of the sort, at any width.
        for cols in [40usize, 80, 200] {
            let full = scroll_bar_text(
                ScrollView {
                    offset: 0,
                    available: 900,
                },
                cols,
                false,
            );
            assert!(
                !full.contains("no history"),
                "a pager with 900 lines behind it must not apologise: {full:?}"
            );
        }
    }

    /// A pager whose "back to live" gesture needs a second keystroke to
    /// actually hand the keyboard back is the confusion this mode exists to
    /// avoid, so scrolling down past the live screen leaves the mode.
    #[test]
    fn scrolling_down_past_the_live_screen_leaves_scroll_mode() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        ctx.scroll.active.store(true, Ordering::SeqCst);
        *ctx.scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ScrollView {
            offset: 2,
            available: 100,
        };
        apply_scroll_command(&ctx, ScrollCommand::Down(1));
        assert!(
            ctx.scroll.is_active(),
            "one line up from the bottom is still the pager"
        );
        apply_scroll_command(&ctx, ScrollCommand::Down(5));
        assert!(
            !ctx.scroll.is_active(),
            "hitting the bottom hands the keyboard back to the session"
        );
    }

    /// `Ctrl-b [` opens the pager without moving it, and without immediately
    /// closing it again -- the `Stay` command exists for exactly that.
    #[test]
    fn ctrl_b_bracket_opens_the_pager_at_the_live_screen() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        enter_scroll_mode(&ctx, ScrollCommand::Stay);
        assert!(ctx.scroll.is_active());
        assert_eq!(
            ctx.scroll
                .view
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .offset,
            0
        );
        apply_scroll_command(&ctx, ScrollCommand::Exit);
        assert!(!ctx.scroll.is_active());
    }

    #[test]
    fn scan_ctrl_b_bracket_opens_scroll_mode() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, b'[']);
        assert!(matches!(actions.as_slice(), [InputAction::Scroll]));
    }

    /// The routing layer, not just the decoder: with the pager up, a chunk
    /// of ordinary typing comes back empty -- nothing to send to the
    /// workload.
    #[test]
    fn scroll_input_forwards_nothing_while_the_pager_is_up() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        let mut input = ScrollInput::default();
        assert_eq!(
            input.route(&ctx, b"ls -la\r"),
            b"ls -la\r".to_vec(),
            "with the pager down and no mouse borrowed, input is untouched"
        );
        enter_scroll_mode(&ctx, ScrollCommand::Stay);
        for chunk in [
            b"ls -la\r".as_slice(),
            b"\x1b[A".as_slice(),
            b"XXNOTINPUTXX".as_slice(),
            b"\x1b[<0;3;4M".as_slice(),
            b"rm -rf /\r".as_slice(),
        ] {
            assert!(
                input.route(&ctx, chunk).is_empty(),
                "{:?} must not reach the workload",
                String::from_utf8_lossy(chunk)
            );
            if !ctx.scroll.is_active() {
                // A chunk containing a downward move at the live screen
                // (Space is PageDown) legitimately closes the pager -- and
                // the assertion above is the important half: the *rest* of
                // that chunk is discarded rather than typed into the
                // session. Reopen for the next case.
                enter_scroll_mode(&ctx, ScrollCommand::Stay);
            }
        }
    }

    /// A failed history refresh must not take the pager down with it. The
    /// model here has seen a DECSTBM sub-range (the gate fires) and the test
    /// record's socket path is a regular file (every RPC fails fast), which
    /// is exactly the "worker went away between the keystroke and the
    /// rebuild" case: `Ctrl-b [` still opens the pager on whatever the live
    /// model has, which is the pre-refresh behavior.
    #[test]
    fn pager_entry_survives_a_failed_history_refresh() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        feed_test_screen(&ctx.screen, b"\x1b[3;23r");
        assert!(
            ctx.screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .subregion_seen(),
            "precondition: the gate has to fire, or this tests nothing"
        );
        enter_scroll_mode(&ctx, ScrollCommand::Stay);
        assert!(ctx.scroll.is_active());
        apply_scroll_command(&ctx, ScrollCommand::Exit);
        assert!(!ctx.scroll.is_active());
    }

    /// `i` in the pager hands the keyboard to the workload -- the thing
    /// tmux copy-mode cannot do: text forwards verbatim, mouse reports stay
    /// swallowed (the client borrowed the mouse; the workload never asked
    /// for it), and a lone Esc takes the keyboard back with the pager still
    /// up, paging again.
    #[test]
    fn type_through_forwards_text_until_esc_returns_to_paging() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        enter_scroll_mode(&ctx, ScrollCommand::Stay);
        let mut input = ScrollInput::default();
        assert!(
            input.route(&ctx, b"i").is_empty(),
            "the i that opens type-through is consumed, not sent"
        );
        assert!(ctx.scroll.is_typing(), "i must enter type-through");
        assert!(ctx.scroll.is_active(), "typing must not close the pager");
        assert_eq!(
            input.route(&ctx, b"hi there\r"),
            b"hi there\r".to_vec(),
            "while typing, text goes to the workload"
        );
        assert!(
            input.route(&ctx, b"\x1b[<0;3;4M").is_empty(),
            "mouse reports stay swallowed during type-through"
        );
        assert!(
            input.route(&ctx, b"\x1b").is_empty(),
            "the Esc that ends type-through is consumed"
        );
        assert!(!ctx.scroll.is_typing());
        assert!(
            ctx.scroll.is_active(),
            "Esc returns to paging, not to the live screen"
        );
        assert!(
            input.route(&ctx, b"xx").is_empty(),
            "once back in the pager, keys are swallowed again"
        );
        apply_scroll_command(&ctx, ScrollCommand::Exit);
        assert!(!ctx.scroll.is_active());
    }

    /// The type-through bar keeps the pager's position readout (the offset is
    /// still where the user left it) and names the mode; narrow widths
    /// degrade without ever exceeding the row.
    #[test]
    fn typing_bar_names_the_mode_and_keeps_the_position() {
        let view = ScrollView {
            offset: 12,
            available: 240,
        };
        let text = scroll_bar_typing_text(view, 120);
        assert!(text.contains("SCROLL 12/240"), "{text:?}");
        assert!(text.contains("TYPE"), "{text:?}");
        assert!(text.chars().count() <= 120);
        let medium = scroll_bar_typing_text(view, 20);
        assert!(
            medium.contains("TYPE") && medium.chars().count() <= 20,
            "{medium:?}"
        );
        assert_eq!(scroll_bar_typing_text(view, 4), "TYPE");
    }

    #[test]
    fn scroll_keys_binds_i_to_type_through() {
        assert_eq!(
            scroll_keys(b"i"),
            ScrollKey::Command(ScrollCommand::TypeThrough, 1)
        );
    }

    /// Pane delivery appends the return by default (the tmuxctl behavior) in
    /// both framed and raw form, and `--no-enter` drops it in both.
    #[test]
    fn pane_delivery_appends_enter_by_default_and_no_enter_drops_it() {
        assert_eq!(
            pane_input_bytes("ship it", Some("review"), false, false),
            b"[aplexer message from review] ship it\r"
        );
        assert_eq!(
            pane_input_bytes("ship it", Some("review"), true, false),
            b"ship it\r"
        );
        assert_eq!(
            pane_input_bytes("hold", Some("review"), false, true),
            b"[aplexer message from review] hold"
        );
        assert_eq!(pane_input_bytes("hold", None, true, true), b"hold");
    }

    /// While the client holds the mouse and the pager is *down*, mouse
    /// reports are swallowed (the workload never asked for them) and a wheel
    /// roll up opens the pager -- with no `Ctrl-b` first, which is the
    /// gesture the user actually reported as broken. Ordinary typing in the
    /// same chunk still gets through.
    #[test]
    fn wheel_up_opens_the_pager_with_no_prefix_and_typing_still_passes() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        *ctx.mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(true);
        let mut input = ScrollInput::default();
        assert_eq!(input.route(&ctx, b"ab"), b"ab".to_vec());
        assert!(!ctx.scroll.is_active());
        // A left click: swallowed, never typed into the workload.
        assert!(input.route(&ctx, b"\x1b[<0;5;5M").is_empty());
        assert!(!ctx.scroll.is_active());
        // The wheel: straight into the pager.
        assert!(input.route(&ctx, b"\x1b[<64;5;5M").is_empty());
        assert!(ctx.scroll.is_active());
    }

    /// A mouse report split across two reads is reassembled rather than
    /// leaking its tail into the workload as text.
    #[test]
    fn a_split_mouse_report_is_buffered_not_leaked_to_the_workload() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        *ctx.mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(true);
        let mut input = ScrollInput::default();
        assert!(input.route(&ctx, b"\x1b[<64;5").is_empty());
        assert!(!ctx.scroll.is_active());
        assert!(input.route(&ctx, b";5M").is_empty());
        assert!(ctx.scroll.is_active());
    }

    /// The counterpart guarantee: a bare `ESC` at the end of a chunk is
    /// forwarded immediately while the pager is down, so pressing Escape in
    /// an editor inside the session does not wait for the next keystroke.
    #[test]
    fn a_bare_escape_is_never_held_back_from_the_workload() {
        let ctx = status_ctx_for_test(true);
        let _null = StdoutToDevNull::new();
        *ctx.mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(true);
        let mut input = ScrollInput::default();
        assert_eq!(input.route(&ctx, b"\x1b"), b"\x1b".to_vec());
        assert_eq!(input.route(&ctx, b"\x1b["), b"\x1b[".to_vec());
    }

    // -- parse_sgr_mouse (docs/clickable-status-bar-design.md section 2) --

    #[test]
    fn parse_sgr_mouse_left_click_press() {
        let buf = b"\x1b[<0;10;5M";
        match parse_sgr_mouse(buf) {
            MouseParse::Complete(report, consumed) => {
                assert_eq!(
                    report,
                    MouseReport {
                        button: 0,
                        press: true,
                        col: 10,
                        row: 5,
                    }
                );
                assert_eq!(consumed, buf.len());
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn parse_sgr_mouse_release() {
        let buf = b"\x1b[<0;10;5m";
        match parse_sgr_mouse(buf) {
            MouseParse::Complete(report, consumed) => {
                assert!(!report.press);
                assert_eq!(consumed, buf.len());
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn parse_sgr_mouse_large_coordinates_no_1006_overflow() {
        // The entire point of SGR (?1006h) over legacy (?1000h alone) mode:
        // no 223-column/row ceiling.
        let buf = b"\x1b[<2;9999;500M";
        match parse_sgr_mouse(buf) {
            MouseParse::Complete(report, _) => {
                assert_eq!(report.col, 9999);
                assert_eq!(report.row, 500);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn parse_sgr_mouse_trailing_bytes_only_consumes_the_report() {
        let buf = b"\x1b[<0;10;5Mrest-of-buffer";
        match parse_sgr_mouse(buf) {
            MouseParse::Complete(_, consumed) => assert_eq!(consumed, 10),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn parse_sgr_mouse_incomplete_at_every_prefix_length() {
        let full = b"\x1b[<0;10;5M";
        for split in 1..full.len() {
            let partial = &full[..split];
            assert_eq!(
                parse_sgr_mouse(partial),
                MouseParse::Incomplete,
                "prefix of length {split} should be Incomplete"
            );
        }
    }

    #[test]
    fn parse_sgr_mouse_rejects_ordinary_csi_sequences() {
        // Arrow keys, cursor reports, colors, etc. -- none start with the
        // `ESC [ <` mouse prefix, so these must be an immediate NotMouse,
        // never treated as "keep buffering".
        assert_eq!(parse_sgr_mouse(b"\x1b[A"), MouseParse::NotMouse); // up arrow
        assert_eq!(parse_sgr_mouse(b"\x1b[31m"), MouseParse::NotMouse); // SGR color
        assert_eq!(parse_sgr_mouse(b"hello"), MouseParse::NotMouse);
    }

    #[test]
    fn parse_sgr_mouse_empty_buffer_is_incomplete_not_rejected() {
        // Zero bytes seen yet can't be ruled out as the start of a mouse
        // report -- a caller with nothing buffered should keep reading,
        // not treat an empty read as "definitely not a mouse sequence".
        assert_eq!(parse_sgr_mouse(b""), MouseParse::Incomplete);
    }

    #[test]
    fn parse_sgr_mouse_malformed_after_prefix_is_not_mouse_not_incomplete() {
        // A non-digit, non-';' byte right where a field is expected can
        // never resolve into a valid report -- must not be reported
        // Incomplete (that would make a caller buffer forever).
        assert_eq!(parse_sgr_mouse(b"\x1b[<x;10;5M"), MouseParse::NotMouse);
        assert_eq!(parse_sgr_mouse(b"\x1b[<0;;5M"), MouseParse::NotMouse);
        assert_eq!(parse_sgr_mouse(b"\x1b[<0;10;5X"), MouseParse::NotMouse);
    }

    #[test]
    fn parse_sgr_mouse_split_across_two_reads_reassembles() {
        // Mirrors the Ctrl-b split-read tests above: a caller buffering
        // bytes across scan() calls must see Incomplete on the first half
        // and Complete once the second half is appended.
        let full: &[u8] = b"\x1b[<0;10;5M";
        let split = 5;
        assert_eq!(parse_sgr_mouse(&full[..split]), MouseParse::Incomplete);
        let mut buffered = full[..split].to_vec();
        buffered.extend_from_slice(&full[split..]);
        match parse_sgr_mouse(&buffered) {
            MouseParse::Complete(_, consumed) => assert_eq!(consumed, full.len()),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn mk_record(workspace: &str, tag: &str, phase: Phase) -> SessionRecord {
        let id = Uuid::new_v4();
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: PathBuf::from(workspace),
            tag: tag.to_string(),
            engine: "shell".to_string(),
            profile: None,
            command: vec![],
            cwd: PathBuf::from(workspace),
            env: Default::default(),
            env_unset: Default::default(),
            limits: Default::default(),
            history_bytes: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase,
            worker_pid: Some(std::process::id()), // our own pid: always "alive"
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            // Must exist on disk: check_attachable now checks socket_path
            // (this test binary's own executable is a convenient stand-in
            // for "some file that's there"; only .exists() is probed, never
            // actually connected to).
            socket_path: std::env::current_exe().unwrap(),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn terminal_text_sanitizer_replaces_c0_del_and_c1_controls() {
        let unsafe_text = "plain\x00\x07\x1b\n\r\x7f\u{0085}\u{009b}tail";
        let safe = sanitize_terminal_text(unsafe_text);
        assert_eq!(safe, "plain????????tail");
        assert!(!safe.chars().any(char::is_control));
    }

    #[test]
    fn cgroup_capability_requires_delegation_but_is_optional_when_missing() {
        let warning = cgroup_limits_check(CgroupLimitProbe {
            cgroup_v2: true,
            controllers: vec!["cpu".into(), "memory".into(), "pids".into()],
            delegated_scope: false,
            detail: "user manager unavailable".into(),
        });
        assert_eq!(warning["available"], false);
        assert_eq!(warning["ok"], false);
        assert_eq!(warning["severity"], "warning");
        assert_eq!(warning["required"], false);
        assert!(warning["detail"]
            .as_str()
            .unwrap()
            .contains("unlimited sessions still work"));
        assert!(doctor_checks_ok(&[warning]));

        let no_cgroup_v2 = cgroup_limits_check(CgroupLimitProbe {
            cgroup_v2: false,
            controllers: Vec::new(),
            delegated_scope: false,
            detail: "cgroup v2 unavailable".into(),
        });
        assert_eq!(no_cgroup_v2["severity"], "warning");
        assert!(doctor_checks_ok(&[no_cgroup_v2]));

        let supported = cgroup_limits_check(CgroupLimitProbe {
            cgroup_v2: true,
            controllers: vec!["cpu".into(), "memory".into(), "pids".into()],
            delegated_scope: true,
            detail: "verified".into(),
        });
        assert_eq!(supported["available"], true);
        assert_eq!(supported["ok"], true);
        assert_eq!(supported["severity"], "ok");
    }

    #[test]
    fn status_bar_sanitizes_record_fields_and_flash_messages() {
        let ctx = status_ctx_for_test(true);
        {
            let mut record = ctx.record.lock().unwrap();
            record.workspace = PathBuf::from("/ws/\x1b[31mred\nline");
            record.tag = "tag\x07bell".to_string();
            record.engine = "engine\rreturn".to_string();
            record.profile = Some("profile\u{009b}2J".to_string());
        }

        let rendered = status_bar_text(&ctx, 256);
        assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
        assert!(!rendered.contains("\x1b[31m"), "{rendered:?}");

        *ctx.flash.lock().unwrap() = Some(("failed\x1b[2J\x07\nnext".to_string(), Instant::now()));
        let flashed = status_bar_text(&ctx, 80);
        assert!(!flashed.chars().any(char::is_control), "{flashed:?}");
        assert!(!flashed.contains("\x1b[2J"), "{flashed:?}");
    }

    #[test]
    fn agent_annotation_appears_only_when_it_adds_information() {
        let mut record = mk_record("/ws", "t", Phase::Running);
        assert_eq!(extra_agent_label(&record, None), None);

        // A claude-engine session running claude already says claude ...
        record.engine = "claude".to_string();
        assert_eq!(
            extra_agent_label(&record, Some(agent_kind::AgentKind::Claude)),
            None
        );
        // ... but the same session running codex does not.
        assert_eq!(
            extra_agent_label(&record, Some(agent_kind::AgentKind::Codex)),
            Some("codex")
        );

        // The engine cell shows the declared engine/profile when there is
        // nothing detected ...
        record.engine = "shell".to_string();
        record.profile = Some("default".to_string());
        assert_eq!(engine_label(&record, None), "shell/default");
        // ... but a shell workload running an agent is labeled by the agent
        // alone: "shell" is the absence of a choice, not a fact worth a
        // column.
        assert_eq!(
            engine_label(&record, Some(agent_kind::AgentKind::Claude)),
            "claude"
        );

        // A declared engine stays the base: there the annotation is a real
        // override, not noise.
        record.engine = "claude".to_string();
        record.profile = None;
        assert_eq!(
            engine_label(&record, Some(agent_kind::AgentKind::Codex)),
            "claude -> codex"
        );
    }

    /// A fake `claude` whose cmdline detection classifies, run as
    /// `/bin/sh <script>` -- same script shape and same ETXTBSY reasoning as
    /// `write_fake_claude` in tests/agent_detection.rs. The loop also
    /// self-expires, so a test that panics before cleanup cannot leak an
    /// immortal polling process.
    fn spawn_fake_claude(dir: &Path) -> (PathBuf, std::process::Child) {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let script = bin.join("claude");
        fs::write(
            &script,
            "#!/bin/sh\n\
             echo running > \"$1\"\n\
             i=0\n\
             while [ -e \"$2\" ] && [ $i -lt 300 ]; do /bin/sleep 0.05; i=$((i+1)); done\n",
        )
        .unwrap();
        fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let ready = dir.join("claude.ready");
        let sentinel = dir.join("claude.keep-running");
        fs::write(&sentinel, b"run").unwrap();
        let child = Command::new("/bin/sh")
            .arg(&script)
            .arg(&ready)
            .arg(&sentinel)
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "fake claude never started");
            thread::sleep(Duration::from_millis(10));
        }
        (sentinel, child)
    }

    #[test]
    fn status_bar_names_the_agent_running_in_a_shell_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let (sentinel, mut child) = spawn_fake_claude(dir.path());
        let ctx = status_ctx_for_test(true);
        ctx.record.lock().unwrap().workload_pid = Some(child.id());

        // Full layout: the agent sits between the state and the engine cell.
        let full = status_bar_text(&ctx, 256);
        assert!(full.contains("\u{25cf} RUNNING  claude  shell"), "{full:?}");
        // Narrow layout: the agent outlives the engine cell, same as the tag.
        let compact = status_bar_text(&ctx, 32);
        assert!(compact.contains("claude"), "{compact:?}");

        fs::remove_file(&sentinel).unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn status_bar_does_not_repeat_an_agent_the_engine_already_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let (sentinel, mut child) = spawn_fake_claude(dir.path());
        let ctx = status_ctx_for_test(true);
        {
            let mut record = ctx.record.lock().unwrap();
            record.engine = "claude".to_string();
            record.workload_pid = Some(child.id());
        }

        let full = status_bar_text(&ctx, 256);
        assert!(full.contains("  claude  |  ^b ?"), "{full:?}");
        assert!(!full.contains("claude  claude"), "{full:?}");

        fs::remove_file(&sentinel).unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn spinner_frame_animates_only_the_reported_working_state() {
        // `working` is the one state that means "the agent said it is
        // running right now" (a fresh state-report push) -- the only one the
        // spinner may run for. `active` is deliberately absent: it is a
        // PTY-recency guess that also fires while the user types at a
        // prompt, and for records with no activity sample at all.
        for t in [0, 123_456_789] {
            assert!(
                spinner_frame("working", t).is_some(),
                "working should animate"
            );
        }
        // Everything else stays on `state_glyph`'s static glyph -- the bar
        // must be motionless for an idle, waiting, or dead session, which is
        // the "only when it's running" half of the feature.
        for state in [
            "active", "running", "waiting", "idle", "quiet", "starting", "stopping", "broken",
            "exited", "oom", "failed",
        ] {
            assert_eq!(spinner_frame(state, 0), None, "{state} must not animate");
            assert_eq!(
                spinner_frame(state, 123_456_789),
                None,
                "{state} must not animate"
            );
        }
    }

    #[test]
    fn spinner_frame_is_a_pure_function_of_the_wall_clock() {
        // The status thread, the frame loop's pending flush, and the input
        // thread's flash redraw all render the bar independently; they must
        // agree on the frame within one SPINNER_FRAME_MS window without any
        // shared counter state.
        let base = 1_000_100; // not a multiple of SPINNER_FRAME_MS
        let window = base / SPINNER_FRAME_MS;
        assert_eq!(
            spinner_frame("working", base),
            Some(SPINNER_FRAMES[(window as usize) % SPINNER_FRAMES.len()])
        );
        // Both ends of the same window land on the same frame...
        assert_eq!(
            spinner_frame("working", window * SPINNER_FRAME_MS),
            spinner_frame("working", window * SPINNER_FRAME_MS + SPINNER_FRAME_MS - 1)
        );
        // ...and one full revolution later the frame wraps back around.
        assert_eq!(
            spinner_frame("working", base),
            spinner_frame(
                "working",
                base + SPINNER_FRAME_MS * SPINNER_FRAMES.len() as u64
            )
        );
    }

    #[test]
    fn state_derivation_sees_the_worker_s_fresh_push_not_the_attach_snapshot() {
        let ctx = status_ctx_for_test(true);
        let now = now_ms();
        let record = {
            let mut record = ctx.record.lock().unwrap().clone();
            record.reported_state = Some("working".to_string());
            // A push from a minute before attach: stale now, so the
            // snapshot alone must not read as working.
            record.reported_state_at_ms = Some(now.saturating_sub(60_000));
            record
        };
        // No Status answer (worker briefly unreachable): snapshot stands.
        // The push is stale, so the word is only ever an activity guess --
        // for this occupied shell (no PTY sample at all) the heuristic's
        // just-started arm says `active`, never a semantic `working`.
        assert_eq!(
            session_ui_state(&overlay_reported_state(&record, None), now).0,
            "active"
        );
        // The worker's live copy says the agent started working *after*
        // attach -- the case the spinner exists for.
        let raw = serde_json::json!({"reported_state": "working", "reported_state_at_ms": now});
        assert_eq!(
            session_ui_state(&overlay_reported_state(&record, Some(&raw)), now).0,
            "working"
        );
        // An older worker that omits the fields leaves the snapshot alone.
        let overlay = overlay_reported_state(&record, Some(&serde_json::json!({"cgroup": {}})));
        assert_eq!(overlay.reported_state.as_deref(), Some("working"));
        assert_eq!(
            overlay.reported_state_at_ms,
            Some(now.saturating_sub(60_000))
        );
    }

    #[test]
    fn status_bar_spins_only_while_the_agent_is_working() {
        let ctx = status_ctx_for_test(true);
        let now = now_ms();
        {
            let mut record = ctx.record.lock().unwrap();
            record.reported_state = Some("working".to_string());
            record.reported_state_at_ms = Some(now);
        }
        let full = status_bar_text(&ctx, 256);
        assert!(full.contains(" WORKING"), "{full:?}");
        assert!(
            full.chars().any(|c| SPINNER_FRAMES.contains(&c)),
            "a working session's bar should carry a spinner frame: {full:?}"
        );
        assert!(
            !full.contains('\u{25cf}'),
            "the static dot must yield to the spinner: {full:?}"
        );

        // Once the push goes stale the occupied shell falls back to the
        // honest activity word (`active`: no PTY sample at all, so the
        // heuristic's just-started arm) -- the bar freezes back to the
        // static dot, no motion. Only a *reported* working push may spin.
        {
            let mut record = ctx.record.lock().unwrap();
            record.reported_state_at_ms = Some(now.saturating_sub(8_001));
        }
        let full = status_bar_text(&ctx, 256);
        assert!(
            full.contains("\u{25cf} ACTIVE"),
            "a stale push falls back to the static glyph: {full:?}"
        );
        assert!(
            !full.chars().any(|c| SPINNER_FRAMES.contains(&c)),
            "no spinner may survive the stale push: {full:?}"
        );
    }

    #[test]
    fn status_padding_uses_display_cells_and_preserves_graphemes() {
        let combining = "e\u{301}";
        let emoji = "👩‍💻";

        assert_eq!(pad_or_truncate("界x", 1), " ");
        assert_eq!(pad_or_truncate("界x", 2), "界");
        assert_eq!(pad_or_truncate("界x", 3), "界x");
        assert_eq!(pad_or_truncate(&format!("{combining}x"), 1), combining);
        assert_eq!(pad_or_truncate(&format!("{emoji}x"), 2), emoji);

        for (text, cols) in [("界x", 4), (combining, 3), (emoji, 5)] {
            let rendered = pad_or_truncate(text, cols);
            assert_eq!(terminal_display_width(&rendered), cols, "{rendered:?}");
        }
    }

    #[test]
    fn terminal_reset_disables_every_snapshot_input_mode_variant() {
        let mouse_modes: &[&[u8]] = &[b"\x1b[?9h", b"\x1b[?1000h", b"\x1b[?1002h", b"\x1b[?1003h"];
        let mouse_encodings: &[&[u8]] = &[b"\x1b[?1005h", b"\x1b[?1006h"];

        for mode in mouse_modes {
            for encoding in mouse_encodings {
                let mut parser = vt100::Parser::new(24, 80, 0);
                parser.process(b"\x1b[?1049h\x1b=\x1b[?1h\x1b[?2004h\x1b[?25l");
                parser.process(mode);
                parser.process(encoding);
                parser.process(TERMINAL_RESET_SEQUENCE);

                let screen = parser.screen();
                assert!(!screen.alternate_screen());
                assert!(!screen.application_keypad());
                assert!(!screen.application_cursor());
                assert!(!screen.bracketed_paste());
                assert_eq!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
                assert_eq!(
                    screen.mouse_protocol_encoding(),
                    vt100::MouseProtocolEncoding::Default
                );
                assert!(!screen.hide_cursor());
            }
        }
    }

    fn sample_groups() -> Vec<(PathBuf, Vec<SessionRecord>)> {
        let ws_a = "/ws/a";
        let ws_b = "/ws/b";
        let mut a1 = mk_record(ws_a, "main", Phase::Running);
        let mut a2 = mk_record(ws_a, "review", Phase::Running);
        let mut a3 = mk_record(ws_a, "dead", Phase::Exited);
        a1.worker_pid = Some(std::process::id());
        a2.worker_pid = Some(std::process::id());
        a3.worker_pid = None; // exited, unattachable regardless
        let b1 = mk_record(ws_b, "only", Phase::Running);
        vec![
            (PathBuf::from(ws_a), vec![a1, a2, a3]),
            (PathBuf::from(ws_b), vec![b1]),
        ]
    }

    // -- workspace_summary_regions (docs/clickable-status-bar-design.md
    // section 4.2) --

    #[test]
    fn summary_regions_matches_workspace_summary_text() {
        let groups = sample_groups();
        let siblings = &groups[0].1; // main, review, dead(Exited)
        let current = siblings[0].id;
        let (text, regions) = workspace_summary_regions(siblings, current);
        assert_eq!(text, "1:main* 2:review 3:dead(exited)");
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0].action, BarClick::Sibling(1));
        assert_eq!(regions[1].action, BarClick::Sibling(2));
        assert_eq!(regions[2].action, BarClick::Sibling(3));
    }

    #[test]
    fn summary_regions_column_ranges_slice_out_the_right_token() {
        let groups = sample_groups();
        let siblings = &groups[0].1;
        let current = siblings[0].id;
        let (text, regions) = workspace_summary_regions(siblings, current);
        let chars: Vec<char> = text.chars().collect();
        for region in &regions {
            let slice: String = chars[region.cols.clone()].iter().collect();
            match region.action {
                BarClick::Sibling(1) => assert_eq!(slice, "1:main*"),
                BarClick::Sibling(2) => assert_eq!(slice, "2:review"),
                BarClick::Sibling(3) => assert_eq!(slice, "3:dead(exited)"),
                _ => panic!("unexpected region {region:?}"),
            }
        }
    }

    #[test]
    fn summary_regions_unicode_tag_uses_display_cells_not_byte_offsets() {
        // A multi-byte tag must not desync the column map -- offsets are
        // display cells (matching pad_or_truncate), not byte counts.
        let a = mk_record("/ws/u", "café", Phase::Running);
        let b = mk_record("/ws/u", "b", Phase::Running);
        let current = a.id;
        let siblings = vec![a, b];
        let (text, regions) = workspace_summary_regions(&siblings, current);
        assert_eq!(text, "1:café* 2:b");
        let chars: Vec<char> = text.chars().collect();
        let second: String = chars[regions[1].cols.clone()].iter().collect();
        assert_eq!(second, "2:b");
    }

    #[test]
    fn summary_regions_count_wide_tags_in_terminal_cells() {
        let a = mk_record("/ws/u", "界", Phase::Running);
        let b = mk_record("/ws/u", "b", Phase::Running);
        let current = a.id;
        let (text, regions) = workspace_summary_regions(&[a, b], current);
        assert_eq!(text, "1:界* 2:b");
        assert_eq!(regions[0].cols, 0..5);
        assert_eq!(regions[1].cols, 6..9);
    }

    #[test]
    fn summary_regions_single_session_still_renders_one_region() {
        // Unlike workspace_summary (which returns "" for a lone session,
        // since there's nothing to switch *to*), the pure builder here
        // doesn't special-case count -- callers decide whether to show the
        // segment at all, same as workspace_summary's caller does today.
        let a = mk_record("/ws/solo", "only", Phase::Running);
        let current = a.id;
        let siblings = vec![a];
        let (text, regions) = workspace_summary_regions(&siblings, current);
        assert_eq!(text, "1:only*");
        assert_eq!(regions.len(), 1);
    }

    #[test]
    fn next_prev_wrap_and_skip_dead() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let a2 = groups[0].1[1].id;
        let next =
            pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Next, None).unwrap();
        assert_eq!(next.id, a2); // dead a3 skipped
        let prev =
            pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Prev, None).unwrap();
        assert_eq!(prev.id, a2); // wraps backward past dead a3 too
    }

    /// `Ctrl-b Down`/`Up` move a *workspace* at a time and enter the one they
    /// land on at its most recently accessed session -- the session a
    /// returning user means by "that workspace".
    #[test]
    fn workspace_hop_enters_at_the_most_recently_accessed_session() {
        let ws_a = "/ws/a";
        let ws_b = "/ws/b";
        let mut a1 = mk_record(ws_a, "main", Phase::Running);
        a1.worker_pid = Some(std::process::id());
        let mut b1 = mk_record(ws_b, "first", Phase::Running);
        let mut b2 = mk_record(ws_b, "second", Phase::Running);
        b1.worker_pid = Some(std::process::id());
        b2.worker_pid = Some(std::process::id());
        // `b2` is listed second but was attached to more recently.
        b1.last_accessed_ms = Some(1_000);
        b2.last_accessed_ms = Some(2_000);
        let (b1_id, b2_id) = (b1.id, b2.id);
        let groups = vec![
            (PathBuf::from(ws_a), vec![a1.clone()]),
            (PathBuf::from(ws_b), vec![b1, b2]),
        ];
        for target in [SwitchTarget::NextWorkspace, SwitchTarget::PrevWorkspace] {
            // Two workspaces, so next and previous are the same one; both
            // must enter it at b2, not at list position 1.
            let picked = pick_switch_target(&groups, Path::new(ws_a), a1.id, target, None).unwrap();
            assert_eq!(picked.id, b2_id, "{target:?} entered the wrong session");
        }

        // Never attached: fall back to `a list` order, i.e. the session the
        // status bar numbers 1.
        let mut c1 = mk_record(ws_b, "first", Phase::Running);
        let mut c2 = mk_record(ws_b, "second", Phase::Running);
        c1.worker_pid = Some(std::process::id());
        c2.worker_pid = Some(std::process::id());
        c1.id = b1_id;
        let fresh = vec![
            (PathBuf::from(ws_a), vec![a1.clone()]),
            (PathBuf::from(ws_b), vec![c1, c2]),
        ];
        let picked = pick_switch_target(
            &fresh,
            Path::new(ws_a),
            a1.id,
            SwitchTarget::NextWorkspace,
            None,
        )
        .unwrap();
        assert_eq!(picked.id, b1_id);
    }

    /// A workspace with nothing attachable left in it is stepped over, not
    /// turned into an error the user has to press through; when every other
    /// workspace is like that, the error says so and (via `perform_switch`)
    /// the attach is left alone.
    #[test]
    fn workspace_hop_skips_dead_workspaces_and_reports_when_none_remain() {
        let ws_a = "/ws/a";
        let ws_dead = "/ws/dead";
        let ws_c = "/ws/c";
        let mut a1 = mk_record(ws_a, "main", Phase::Running);
        a1.worker_pid = Some(std::process::id());
        let mut corpse = mk_record(ws_dead, "gone", Phase::Exited);
        corpse.worker_pid = None;
        let mut c1 = mk_record(ws_c, "live", Phase::Running);
        c1.worker_pid = Some(std::process::id());
        let c1_id = c1.id;
        let groups = vec![
            (PathBuf::from(ws_a), vec![a1.clone()]),
            (PathBuf::from(ws_dead), vec![corpse.clone()]),
            (PathBuf::from(ws_c), vec![c1]),
        ];
        let picked = pick_switch_target(
            &groups,
            Path::new(ws_a),
            a1.id,
            SwitchTarget::NextWorkspace,
            None,
        )
        .unwrap();
        assert_eq!(picked.id, c1_id, "the dead workspace was not skipped");

        let alone = vec![
            (PathBuf::from(ws_a), vec![a1.clone()]),
            (PathBuf::from(ws_dead), vec![corpse]),
        ];
        let error = pick_switch_target(
            &alone,
            Path::new(ws_a),
            a1.id,
            SwitchTarget::PrevWorkspace,
            None,
        )
        .expect_err("nowhere to go");
        assert!(
            format!("{error:#}").contains("no other workspace"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn index_returns_dead_session_without_skipping() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let dead = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            a1,
            SwitchTarget::Index(3),
            None,
        )
        .unwrap();
        assert_eq!(dead.phase, Phase::Exited);
    }

    #[test]
    fn index_out_of_range_errors() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let err = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            a1,
            SwitchTarget::Index(9),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no session 9"));
    }

    #[test]
    fn index_nine_selects_the_ninth_session_and_bounds_are_one_based() {
        let sessions: Vec<SessionRecord> = (1..=9)
            .map(|i| mk_record("/ws/a", &format!("session-{i}"), Phase::Running))
            .collect();
        let current = sessions[0].id;
        let groups = vec![(PathBuf::from("/ws/a"), sessions)];

        let ninth = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            current,
            SwitchTarget::Index(9),
            None,
        )
        .unwrap();
        assert_eq!(ninth.tag, "session-9");

        let zero = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            current,
            SwitchTarget::Index(0),
            None,
        )
        .unwrap_err();
        assert!(zero.to_string().contains("no session 0"));

        let tenth = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            current,
            SwitchTarget::Index(10),
            None,
        )
        .unwrap_err();
        assert!(tenth.to_string().contains("no session 10"));
    }

    #[test]
    fn next_global_crosses_workspace_boundary() {
        let groups = sample_groups();
        let a2 = groups[0].1[1].id; // last live session in ws/a
        let next = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            a2,
            SwitchTarget::NextGlobal,
            None,
        )
        .unwrap();
        assert_eq!(next.workspace, PathBuf::from("/ws/b"));
    }

    #[test]
    fn last_resolves_by_id() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let a2 = groups[0].1[1].id;
        let found = pick_switch_target(
            &groups,
            Path::new("/ws/a"),
            a1,
            SwitchTarget::Last,
            Some(a2),
        )
        .unwrap();
        assert_eq!(found.id, a2);
    }

    #[test]
    fn single_live_session_workspace_errors_on_next() {
        let groups = sample_groups();
        let b1 = groups[1].1[0].id;
        let err = pick_switch_target(&groups, Path::new("/ws/b"), b1, SwitchTarget::Next, None)
            .unwrap_err();
        assert!(err.to_string().contains("no other running session"));
    }

    /// The orphaned-session bug this test guards against: `phase: Running`
    /// plus an alive `worker_pid` used to sail straight through
    /// `check_attachable` and hit a raw `UnixStream::connect` OS error deep
    /// inside `rpc_simple`/`attach` if the socket file was gone. Now it's
    /// caught up front with a clear diagnostic.
    #[test]
    fn check_attachable_reports_missing_socket() {
        let mut r = mk_record("/ws/a", "main", Phase::Running);
        r.socket_path = PathBuf::from("/definitely/does/not/exist/control.sock");
        let err = check_attachable(&r).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("control socket is gone"), "{msg}");
        assert!(msg.contains(&format!("a kill {}", r.id)), "{msg}");
    }

    /// `check_attachable`'s other two cases (terminal phase, dead
    /// worker_pid) must be unaffected by the new socket check -- they bail
    /// before ever looking at `socket_path`.
    #[test]
    fn check_attachable_unchanged_for_terminal_and_dead_worker() {
        let mut exited = mk_record("/ws/a", "main", Phase::Exited);
        exited.socket_path = PathBuf::from("/does/not/matter");
        let err = check_attachable(&exited).unwrap_err().to_string();
        assert!(err.contains("has already exited"), "{err}");

        let mut dead_worker = mk_record("/ws/a", "main", Phase::Running);
        dead_worker.worker_pid = None;
        dead_worker.socket_path = PathBuf::from("/does/not/matter");
        let err = check_attachable(&dead_worker).unwrap_err().to_string();
        assert!(err.contains("worker is not running"), "{err}");
    }

    /// A registry containing exactly one record, with its paths wired to
    /// the throwaway state/runtime roots so `read_session_record`'s identity
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

    /// The default list sweeps before it renders: a corpse `a prune` would
    /// take (worker gone, proven-empty containment) is removed from the
    /// registry by one bare `a list`, not parked behind the hide filter.
    /// `--all` skips the sweep -- it exists to show post-mortems.
    #[test]
    fn default_list_sweeps_what_prune_would_take() {
        let now = now_ms();
        let mut corpse = mk_record("/ws/sweep", "gone", Phase::Exited);
        corpse.worker_pid = None;
        corpse.workload_pid = None;
        corpse.containment_empty = Some(true);
        corpse.exit = Some(aplexer::ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: now,
        });
        let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut corpse);

        cmd_list_tty(
            &paths,
            ListArgs {
                running: false,
                all: true,
                sort: None,
            },
        )
        .unwrap();
        assert!(
            paths.state_session(corpse.id).exists(),
            "--all must not sweep; it exists to show post-mortems"
        );

        cmd_list_tty(
            &paths,
            ListArgs {
                running: false,
                all: false,
                sort: None,
            },
        )
        .unwrap();
        assert!(
            !paths.state_session(corpse.id).exists(),
            "the default list left a prunable corpse on disk"
        );
    }

    /// resolve_quick_index shares session_is_listed with the default list,
    /// so the numbers `a 1` understands stay the numbers `a list` prints: a
    /// corpse cannot take a row number even when it is the newest session
    /// in its workspace, and a workspace whose only session has exited is
    /// simply not there (the list drops it whole).
    #[test]
    fn quick_index_skips_exited_corpses_like_the_default_list() {
        let now = now_ms();
        // Newest-created-first row order (registry.rs), so unfiltered this
        // corpse would be row 1 of the workspace.
        let mut corpse = mk_record("/ws/only", "gone", Phase::Exited);
        corpse.worker_pid = None;
        corpse.created_at_ms = now + 5_000;
        let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut corpse);

        let err =
            resolve_quick_index(&paths, 1, None).expect_err("a corpse-only workspace has no rows");
        assert!(
            format!("{err:#}").contains("no sessions found"),
            "unexpected error: {err:#}"
        );

        // A live session behind the corpse: with the filter, index 1
        // resolves to the live session, never to the newer corpse.
        let mut live = mk_record("/ws/only", "main", Phase::Running);
        live.created_at_ms = now;
        live.socket_path = paths.socket(live.id);
        live.history_path = paths.history(live.id);
        fs::create_dir_all(paths.state_session(live.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(live.id)).unwrap();
        atomic_write_json(&paths.record(live.id), &live).unwrap();

        let picked = resolve_quick_index(&paths, 1, None).unwrap();
        assert_eq!(picked.id, live.id, "the corpse took the live session's row");
    }

    /// `worker_alive()` deliberately falls back to the bare pid check when
    /// the worker identity sidecar cannot be read, so an unreadable sidecar
    /// can never let prune delete a live worker's session. Prune must
    /// inherit that conservatism instead of re-deriving liveness: while the
    /// recorded pid exists and the sidecar is garbage, the record stays --
    /// even though every other reap condition (non-terminal phase, dead
    /// workload, no containment proof) is satisfied.
    #[test]
    fn prune_retains_a_record_whose_worker_identity_is_unreadable() {
        let mut record = mk_record("/ws/uncertain", "main", Phase::Running);
        // A real throwaway process standing in for the recorded worker,
        // never this test process's own pid.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        record.worker_pid = Some(child.id());
        record.workload_pid = None;
        record.containment_empty = Some(false);
        let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut record);
        // Unparseable, so read_worker_identity errors rather than returning
        // None: the "we cannot tell" case, not the "legacy record" case.
        fs::write(
            paths.state_session(record.id).join("worker.identity.json"),
            b"{not json",
        )
        .unwrap();

        let outcome = prune_dead_sessions(&paths).unwrap();
        assert!(
            outcome.removed.is_empty(),
            "prune reaped a session whose worker liveness was unknown"
        );
        assert_eq!(outcome.retained_count, 1);
        assert!(
            paths.state_session(record.id).exists(),
            "durable state was removed under an unreadable identity"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "prune must never signal anything"
        );

        // Same unreadable sidecar, worker genuinely gone: now it is reapable.
        // Proves the retention above came from the liveness fallback and not
        // from some blanket refusal to touch this record.
        child.kill().unwrap();
        child.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(record.worker_pid.unwrap()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let outcome = prune_dead_sessions(&paths).unwrap();
        assert_eq!(outcome.removed, vec![record.id]);
        assert_eq!(outcome.removed_without_containment_proof, vec![record.id]);
        assert!(!paths.state_session(record.id).exists());
    }

    /// Prune no longer requires a terminal phase, so it can now see a
    /// `Starting` record with no worker pid -- which is exactly what
    /// `start_session` writes before its worker registers itself. That
    /// worker holds the session's worker lock, so prune must fence on it
    /// the same way `a forget` does rather than deleting a session that is
    /// coming up.
    #[test]
    fn prune_fences_a_pre_pid_starting_record_against_its_worker_lock() {
        let mut record = mk_record("/ws/starting", "main", Phase::Starting);
        record.worker_pid = None;
        record.workload_pid = None;
        record.containment_empty = Some(false);
        let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut record);

        let held = FileLock::exclusive(&paths.worker_lock(record.id), true).unwrap();
        let outcome = prune_dead_sessions(&paths).unwrap();
        assert!(
            outcome.removed.is_empty(),
            "prune removed a session whose worker still holds its lock"
        );
        assert_eq!(outcome.retained_count, 1);
        assert!(paths.state_session(record.id).exists());

        drop(held);
        let outcome = prune_dead_sessions(&paths).unwrap();
        assert_eq!(
            outcome.removed,
            vec![record.id],
            "an unfenced pre-PID stub must still be reapable"
        );
        assert!(!paths.state_session(record.id).exists());
    }

    /// `a kill` against a record whose worker_pid is alive but whose control
    /// socket is gone may stop that verified worker, but without a cgroup it
    /// cannot prove that a setsid descendant did not escape. It must preserve
    /// the worker and record, keeping the only remaining subreaper boundary.
    /// Uses a real throwaway child process as the stand-in worker_pid
    /// (never the test process's own pid, which `mk_record` defaults to --
    /// this test also proves the containment preflight happens before any
    /// signal is sent.
    #[test]
    fn cmd_kill_preserves_stale_socket_record_without_containment_proof() {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let mut record = mk_record("/ws/stale", "main", Phase::Running);
        // A real, throwaway process standing in for the orphaned worker --
        // long-lived enough that if `a kill` did nothing, it would still be
        // alive when we check.
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        record.worker_pid = Some(child.id());
        record.history_path = paths.history(record.id);
        // Preserve runtime evidence while leaving the control socket absent.
        record.socket_path = paths.socket(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), &record).unwrap();
        let start_time = process_start_time_ticks(child.id()).unwrap();
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let boot_id = boot_id.trim();
        fs::write(
            paths.state_session(record.id).join("worker.identity.json"),
            format!(
                "{{\"pid\":{},\"start_time_ticks\":{start_time},\"boot_id\":\"{boot_id}\"}}\n",
                child.id(),
            ),
        )
        .unwrap();

        let args = KillArgs {
            target: TargetArgs {
                selector: Some(record.id.to_string()),
                workspace: None,
                tag: None,
            },
            signal: "TERM".to_string(),
            grace_ms: 50,
        };
        let error = cmd_kill(&paths, args, false)
            .expect_err("missing containment proof must prevent cleanup success");
        assert!(
            format!("{error:#}").contains("no authoritative containment locator"),
            "{error:#}"
        );
        assert!(
            paths.state_session(record.id).exists(),
            "ambiguous stale record must be preserved"
        );
        assert!(
            paths.runtime_session(record.id).exists(),
            "ambiguous runtime evidence must be preserved"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "unreachable worker was destroyed before recovery was proven"
        );
        assert!(
            process_alive(record.worker_pid.unwrap()),
            "unreachable worker must remain the subreaper boundary"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn force_kill_stale_worker_refuses_legacy_record_without_identity() {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let mut record = mk_record("/ws/legacy", "main", Phase::Running);
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        record.worker_pid = Some(child.id());
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), &record).unwrap();

        let error = force_kill_stale_worker(&record).unwrap_err();
        assert!(
            format!("{error:#}").contains("no trustworthy recorded worker identity"),
            "{error:#}"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "legacy pid was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn force_kill_stale_worker_refuses_start_time_mismatch() {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let mut record = mk_record("/ws/reused", "main", Phase::Running);
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        record.worker_pid = Some(pid);
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), &record).unwrap();
        let wrong_start = process_start_time_ticks(pid).unwrap().saturating_add(1);
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let boot_id = boot_id.trim();
        fs::write(
            paths.state_session(record.id).join("worker.identity.json"),
            format!(
                "{{\"pid\":{pid},\"start_time_ticks\":{wrong_start},\"boot_id\":\"{boot_id}\"}}\n"
            ),
        )
        .unwrap();

        let error = force_kill_stale_worker(&record).unwrap_err();
        assert!(
            format!("{error:#}").contains("has been reused"),
            "{error:#}"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "mismatched pid identity was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The safety property this whole feature must preserve: a session
    /// whose worker is genuinely alive AND reachable (real socket on disk)
    /// must still be refused, exactly as before -- only the narrower
    /// socket-missing case gets the new force-kill behavior.
    #[test]
    fn cmd_kill_still_refuses_live_reachable_worker() {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let record = mk_record("/ws/live", "main", Phase::Running);
        // worker_pid defaults (via mk_record) to this test process's own
        // pid, i.e. genuinely alive. socket_path defaults to this test
        // binary's own executable path, i.e. genuinely exists on disk --
        // so `socket_missing` must be false and the RPC-failure path below
        // must hit the unchanged `return Err(error)` refusal, never the
        // force-kill/remove branch.
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), &record).unwrap();

        let args = KillArgs {
            target: TargetArgs {
                selector: Some(record.id.to_string()),
                workspace: None,
                tag: None,
            },
            signal: "TERM".to_string(),
            grace_ms: 50,
        };
        let err = cmd_kill(&paths, args, false)
            .expect_err("a live, reachable worker must not be force-killed by `a kill`");
        // The error is the raw RPC/connect failure (there's no real worker
        // listening on that socket path), not a "removed" success -- and
        // the record and the still-alive pid must both be untouched.
        drop(err);
        assert!(
            paths.state_session(record.id).exists(),
            "a live/reachable session's record must not be removed"
        );
        assert!(
            process_alive(record.worker_pid.unwrap()),
            "a live/reachable session's worker must not be signalled"
        );
    }

    // -- draw_status_bar's "did it actually write" contract, which the
    // status thread's STATUS_BAR_MAX_INTERVAL overdue-timer depends on to
    // avoid the timer-starvation bug: resetting `last_draw` on every tick
    // regardless of whether draw_status_bar performed a real write would
    // let a frequent-but-unchanging redraw (a spinner, streamed tokens with
    // pauses) keep the overdue timer perpetually "recently fired" without
    // ever actually re-writing a margin/row a full-screen erase clobbered.
    // See draw_status_bar's and the status thread's doc comments.

    /// `draw_status_bar` writes straight to the real `io::Stdout` (no
    /// injectable writer to swap in for a test), so exercising its
    /// real-write path here would otherwise leak raw DECSTBM/reverse-video
    /// escape sequences into whatever terminal happens to be running
    /// `cargo test` interactively -- and leave that terminal's scroll
    /// region permanently narrowed, since nothing in this test ever runs
    /// the reset-on-detach path that would restore it. Redirecting the
    /// process's real fd 1 to `/dev/null` for the guard's lifetime (and
    /// restoring the original fd on drop) makes the write land somewhere
    /// harmless instead.
    struct StdoutToDevNull {
        saved_fd: i32,
    }
    impl StdoutToDevNull {
        fn new() -> Self {
            let saved_fd = unsafe { libc::dup(1) };
            assert!(saved_fd >= 0, "dup(1) failed");
            let devnull = std::ffi::CString::new("/dev/null").unwrap();
            let devnull_fd = unsafe { libc::open(devnull.as_ptr(), libc::O_WRONLY) };
            assert!(devnull_fd >= 0, "open /dev/null failed");
            let rc = unsafe { libc::dup2(devnull_fd, 1) };
            unsafe { libc::close(devnull_fd) };
            assert!(rc >= 0, "dup2 to /dev/null failed");
            Self { saved_fd }
        }
    }
    impl Drop for StdoutToDevNull {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.saved_fd, 1);
                libc::close(self.saved_fd);
            }
        }
    }

    /// Feeds bytes into a test context's model the way the client's relay
    /// path does, without a terminal to write them to.
    fn feed_test_screen(screen: &Arc<Mutex<aplexer::screen::ClientScreen>>, data: &[u8]) {
        screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .feed(data);
    }

    fn status_ctx_for_test(reserved: bool) -> StatusBarCtx {
        StatusBarCtx {
            stdout: Arc::new(Mutex::new(io::stdout())),
            term: Arc::new(Mutex::new(TermGeom {
                rows: 24,
                cols: 80,
                reserved,
            })),
            paths: Paths {
                runtime_root: PathBuf::from("/nonexistent-aplexer-test-runtime"),
                state_root: PathBuf::from("/nonexistent-aplexer-test-state"),
                config_file: PathBuf::from("/nonexistent-aplexer-test-state/config.toml"),
            },
            record: Arc::new(Mutex::new(mk_record(
                "/ws/status-bar-test",
                "t",
                Phase::Running,
            ))),
            flash: Arc::new(Mutex::new(None)),
            last_drawn: Arc::new(Mutex::new(None)),
            screen: Arc::new(Mutex::new(
                aplexer::screen::ClientScreen::try_new(23, 80).unwrap(),
            )),
            pending: Arc::new(AtomicBool::new(false)),
            pending_refresh: Arc::new(AtomicBool::new(false)),
            pending_layout: Arc::new(Mutex::new(None)),
            sync_deferred_since: Arc::new(Mutex::new(None)),
            scroll: Arc::new(ScrollMode::new()),
            overlay: Arc::new(KeyOverlay::default()),
            mouse_owned: Arc::new(Mutex::new(None)),
            mouse_capture: false,
        }
    }

    /// Serializes every test that redirects the process-wide fd 1. Without
    /// it the default multi-threaded test harness lets one such test's
    /// `dup(1)` capture another's pipe write end and hold it open, so the
    /// reader blocks forever waiting for an EOF that never comes.
    static FD1_GUARD: Mutex<()> = Mutex::new(());

    /// Like `StdoutToDevNull`, but keeps the bytes: redirects fd 1 to a pipe
    /// so a test can assert on the exact escape sequences `draw_status_bar`
    /// emitted, rather than only on its `bool` return.
    struct StdoutToPipe {
        saved_fd: i32,
        read_fd: i32,
    }
    impl StdoutToPipe {
        fn new() -> Self {
            let saved_fd = unsafe { libc::dup(1) };
            assert!(saved_fd >= 0, "dup stdout failed");
            let mut fds = [0i32; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
            // Non-blocking read end: everything of interest is flushed by
            // `write_locked` before we read, so "no more data" must surface
            // as EAGAIN rather than an indefinite block.
            assert_eq!(
                unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) },
                0,
                "set O_NONBLOCK failed"
            );
            assert!(unsafe { libc::dup2(fds[1], 1) } >= 0, "dup2 to pipe failed");
            unsafe { libc::close(fds[1]) };
            Self {
                saved_fd,
                read_fd: fds[0],
            }
        }
        /// Restores stdout and returns everything written while redirected.
        fn take(self) -> Vec<u8> {
            // Restore first so the write end is fully closed before reading,
            // otherwise the read below blocks on a still-open pipe.
            unsafe {
                libc::dup2(self.saved_fd, 1);
                libc::close(self.saved_fd);
            }
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe {
                    libc::read(
                        self.read_fd,
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
            unsafe { libc::close(self.read_fd) };
            out
        }
    }

    /// Regression test for the DECSTBM clobber described on
    /// `StatusBarCtx::screen`: the bar's defensive scroll-region
    /// re-assert used to write `\x1b[1;{rows-1}r` unconditionally, which
    /// destroyed a workload's own sub-range -- including the one the attach
    /// snapshot had just restored (docs/terminal-state-design.md section 6.2
    /// step 3) -- and left the host terminal scrolling the wrong rows.
    #[test]
    fn draw_status_bar_reasserts_the_workload_scroll_region_not_its_own() {
        let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        let ctx = status_ctx_for_test(true);
        // No workload region: the bar reserves the bottom row for itself, as
        // it always has.
        let pipe = StdoutToPipe::new();
        draw_status_bar(&ctx, true);
        let default_margins = String::from_utf8_lossy(&pipe.take()).into_owned();
        assert!(
            default_margins.contains("\x1b[1;23r"),
            "with a full-screen workload the bar must reserve row 24 for itself, got {default_margins:?}"
        );

        // Workload sets a DECSTBM sub-range (as an attach snapshot's trailing
        // bytes do, and as a margin-using TUI does live).
        feed_test_screen(&ctx.screen, b"\x1b[5;15r");
        let pipe = StdoutToPipe::new();
        draw_status_bar(&ctx, true);
        let sub_range = String::from_utf8_lossy(&pipe.take()).into_owned();
        assert!(
            sub_range.contains("\x1b[5;15r"),
            "the workload's own scroll region must be the one re-asserted, got {sub_range:?}"
        );
        assert!(
            !sub_range.contains("\x1b[1;23r"),
            "the bar must not clobber the workload's sub-range, got {sub_range:?}"
        );

        // Workload releases its region (`\x1b[r`): the bar's own reservation
        // must come straight back, or the reserved row stops being protected.
        feed_test_screen(&ctx.screen, b"\x1b[r");
        let pipe = StdoutToPipe::new();
        draw_status_bar(&ctx, true);
        let released = String::from_utf8_lossy(&pipe.take()).into_owned();
        assert!(
            released.contains("\x1b[1;23r"),
            "releasing the workload region must restore the bar's reservation, got {released:?}"
        );
    }

    /// A workload margin change with otherwise-identical bar text must not be
    /// swallowed by the dirty-check -- that would leave the wrong scroll
    /// region in force on the host until some unrelated text change happened.
    #[test]
    fn draw_status_bar_dirty_check_notices_a_workload_margin_change() {
        let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        let _guard = StdoutToDevNull::new();
        let ctx = status_ctx_for_test(true);
        assert!(
            draw_status_bar(&ctx, false),
            "first draw must be a real write"
        );
        assert!(
            !draw_status_bar(&ctx, false),
            "unchanged state must be a skip"
        );
        feed_test_screen(&ctx.screen, b"\x1b[5;15r");
        assert!(
            draw_status_bar(&ctx, false),
            "a workload margin change must defeat the dirty-check even when the text is unchanged"
        );
    }

    // ---------------------------------------------------------------------
    // Issue #5: the status bar must not be able to corrupt a workload frame.
    //
    // Everything below asserts on the *rendered screen* of a real
    // `vt100::Parser` standing in for the user's terminal, compared against a
    // second parser standing in for what the workload believes it drew. A
    // byte-level assertion would pass while the screen was still wrong, which
    // is exactly the trap this class of bug sets.
    // ---------------------------------------------------------------------

    /// The user's real terminal (`host`, full physical geometry) beside the
    /// workload's own screen (`workload`, one row shorter -- its PTY is
    /// resized to leave the status row free). The client is a byte relay
    /// between them, so for every row the workload can reach the two must
    /// render identically, character for character, and agree on the cursor.
    struct Harness {
        ctx: StatusBarCtx,
        host: vt100::Parser,
        workload: vt100::Parser,
        rows: u16,
        cols: u16,
        /// Every status redraw that actually reached the terminal, with the
        /// stream state it was written at.
        redraws: Vec<Redraw>,
    }

    struct Redraw {
        at_escape_boundary: bool,
        in_synchronized_update: bool,
    }

    impl Harness {
        fn new() -> Self {
            let (rows, cols) = (24u16, 80u16);
            let ctx = status_ctx_for_test(true);
            let mut host = vt100::Parser::new(rows, cols, 0);
            // What `apply_terminal_layout` puts on the wire at attach time.
            host.process(format!("\x1b[1;{}r", rows - 1).as_bytes());
            Self {
                ctx,
                host,
                workload: vt100::Parser::new(rows - 1, cols, 0),
                rows,
                cols,
                redraws: Vec::new(),
            }
        }

        /// One PTY chunk: the workload's own screen sees it, and so does the
        /// client's model on its way to the host terminal.
        fn workload_emits(&mut self, data: &[u8]) {
            self.workload.process(data);
            let rewritten = {
                let mut screen = self
                    .ctx
                    .screen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                screen.relay(data)
            };
            self.host.process(rewritten.as_deref().unwrap_or(data));
            // What the main frame loop does after every Data frame.
            if self.ctx.pending.load(Ordering::Relaxed) {
                self.status_redraw();
            }
        }

        /// What the status thread's timer does. Returns whether the redraw
        /// actually reached the terminal (false = deferred to `ctx.pending`).
        fn status_redraw(&mut self) -> bool {
            let (at_escape_boundary, in_synchronized_update) = {
                let screen = self
                    .ctx
                    .screen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                (screen.at_escape_boundary(), screen.in_synchronized_update())
            };
            match status_bar_redraw(&self.ctx, true) {
                Some(bytes) => {
                    assert!(!bytes.is_empty(), "a reported write must emit bytes");
                    self.host.process(&bytes);
                    self.redraws.push(Redraw {
                        at_escape_boundary,
                        in_synchronized_update,
                    });
                    true
                }
                None => false,
            }
        }

        /// Simulates `STATUS_BAR_SYNC_DEFER_LIMIT` having elapsed, so the
        /// synchronized-output deferral stops holding the redraw back and the
        /// escape-boundary gate is the only thing standing between the
        /// injection and the workload's half-emitted sequence. That is the
        /// production worst case (a frame longer than the limit, or a block
        /// the workload never closes) and the case issue #5 was reported
        /// from, so the tests drive it directly rather than hiding behind the
        /// softer gate.
        fn expire_sync_deferral(&self) {
            let mut since = self
                .ctx
                .sync_deferred_since
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *since = Instant::now().checked_sub(STATUS_BAR_SYNC_DEFER_LIMIT * 2);
        }

        fn row(parser: &vt100::Parser, row: u16, cols: u16) -> String {
            parser.screen().contents_between(row, 0, row, cols)
        }

        /// The load-bearing assertion: every row the workload can reach must
        /// render on the host exactly as the workload drew it, and the cursor
        /// must agree.
        fn assert_screens_agree(&self, label: &str) {
            for row in 0..self.rows - 1 {
                assert_eq!(
                    Self::row(&self.host, row, self.cols),
                    Self::row(&self.workload, row, self.cols),
                    "{label}: host row {} diverged from the workload's screen",
                    row + 1
                );
            }
            assert_eq!(
                self.host.screen().cursor_position(),
                self.workload.screen().cursor_position(),
                "{label}: host and workload disagree on the cursor position"
            );
        }

        fn assert_bar_drawn(&self, label: &str) {
            let bar = Self::row(&self.host, self.rows - 1, self.cols);
            assert!(
                bar.contains("status-bar-test"),
                "{label}: the reserved row must still carry the status bar, got {bar:?}"
            );
        }
    }

    /// One opencode/opentui-shaped frame: a synchronized-output block wrapping
    /// a per-cell diff repaint, every run absolutely positioned with its own
    /// SGR. Captured verbatim from a real `opencode` session for issue #5:
    /// `\x1b[?2026h\x1b[?25l\x1b[15;64H\x1b[38;5;237m\x1b[48;5;234m\xc2\xb7...`
    fn opencode_shaped_frame(generation: usize) -> Vec<u8> {
        let words = [
            "Replace",
            "with",
            "ValueError",
            "capture-warnings.html",
            "docs.pytest.org",
            "session",
            "passed",
        ];
        let mut frame = b"\x1b[?2026h\x1b[?25l".to_vec();
        for row in 1..=23usize {
            let mut col = 1usize;
            let mut k = 0usize;
            while col < 66 {
                let word = words[(generation + row + k) % words.len()];
                frame.extend_from_slice(
                    format!(
                        "\x1b[{row};{col}H\x1b[38;5;{}m\x1b[48;5;234m{word}\x1b[0m",
                        16 + ((generation + row + k) % 200)
                    )
                    .as_bytes(),
                );
                col += word.len() + 1;
                k += 1;
            }
        }
        frame.extend_from_slice(b"\x1b[13;17H\x1b[?25h\x1b[?2026l");
        frame
    }

    /// **The issue #5 reproduction.** A status redraw requested while the
    /// relayed stream sits inside a half-emitted CSI sequence -- which is
    /// where a PTY read boundary lands about half the time under a
    /// continuously-streaming TUI (measured: 5 of 10 redraws on a real
    /// `a attach`) -- used to be written there anyway. The host terminal
    /// abandons the workload's partial sequence when our `ESC` arrives and
    /// prints its remaining parameter bytes as literal text into the frame:
    /// `\x1b[38;5;` + our redraw + `91m...` renders a stray `91m` welded into
    /// the row and shifts everything after it along, which is exactly the
    /// reported `Rep69ce with Val` / `Doos` / `hetps` corruption.
    ///
    /// The split point here is not hand-picked to be convenient: the test
    /// walks *every* byte offset inside the frame, so it covers splits inside
    /// CSI parameters, inside intermediate bytes, between an `ESC` and its
    /// `[`, and inside the multi-byte characters the frame draws with.
    #[test]
    fn status_redraw_never_splices_into_a_workload_frame() {
        let frame = opencode_shaped_frame(0);
        // Every offset would be ~3500 harnesses; step through it densely
        // enough to hit every sequence position class many times over while
        // keeping the test fast.
        let mut deferred = 0usize;
        let mut written = 0usize;
        for split in (1..frame.len()).step_by(7) {
            let mut h = Harness::new();
            h.workload_emits(&frame[..split]);
            // The whole frame is inside a `?2026` block, so without this the
            // softer synchronized-output gate would defer every redraw and the
            // escape-boundary gate -- the one that actually prevents the
            // corruption -- would never be exercised.
            h.expire_sync_deferral();
            if h.status_redraw() {
                written += 1;
            } else {
                deferred += 1;
            }
            h.workload_emits(&frame[split..]);
            h.assert_screens_agree(&format!("split at byte {split}"));
            h.assert_bar_drawn(&format!("split at byte {split}"));
            for r in &h.redraws {
                assert!(
                    r.at_escape_boundary,
                    "split at byte {split}: a redraw was written mid-escape-sequence"
                );
            }
        }
        assert!(
            deferred > 0,
            "the frame must contain unsafe split points for this test to mean anything"
        );
        assert!(
            written > 0,
            "and safe ones, so the bar is not simply never drawn"
        );
    }

    /// One Claude-Code-shaped frame: **no `?2026` anywhere**. Ink-based TUIs
    /// (Claude Code, and codex before it adopted synchronized output) repaint
    /// with a full-screen erase followed by absolutely-positioned SGR runs,
    /// and Claude Code opens with the DEC save/restore-cursor idiom
    /// (`\x1b7\x1b[r\x1b8`) that the status bar used to clobber. Measured from
    /// a real 24x100 capture: 1 `ESC 7`, 1 `ESC 8`, 0 `CSI ?2026h`.
    fn claude_code_shaped_frame(generation: usize) -> Vec<u8> {
        let words = [
            "Removed",
            "InDjango70Warning",
            "category",
            "capture-warnings.html",
            "docs.pytest.org",
            "8 passed, 3 warnings",
        ];
        // The exact opening idiom, plus a save the workload restores later.
        let mut frame = b"\x1b7\x1b[r\x1b8\x1b[2J\x1b[H".to_vec();
        for row in 1..=23usize {
            frame.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
            let mut k = 0usize;
            let mut col = 1usize;
            while col < 70 {
                let word = words[(generation + row + k) % words.len()];
                frame.extend_from_slice(
                    format!(
                        "\x1b[38;5;{}m\x1b[1m{word}\x1b[0m ",
                        16 + ((generation + row + k) % 200)
                    )
                    .as_bytes(),
                );
                col += word.len() + 1;
                k += 1;
            }
        }
        frame.extend_from_slice(b"\x1b[9;5H\x1b7\x1b[23;1H\x1b[2Kfooter\x1b8ANCHORED");
        frame
    }

    /// `flash_status` is a *new* forced-redraw caller (the terminal-first CLI
    /// merge routed the attach hint, `Ctrl-b ?` help and switch failures
    /// through it, replacing an `eprintln!` banner and two direct
    /// `draw_status_bar` calls). It must not be a hole in the boundary gate.
    ///
    /// It is not, by construction rather than by discipline: the gate lives
    /// inside `draw_status_bar`, which is the single funnel every bar write
    /// goes through, so a caller cannot opt out of it -- `flash_status` passes
    /// `force: true` and `force` deliberately does not bypass the boundary
    /// check. This pins that: a flash raised while the workload is
    /// mid-escape-sequence must be deferred rather than spliced, and must
    /// still reach the terminal at the next boundary with its message intact.
    ///
    /// This matters more since the maintainer's "let's not hide status"
    /// decision (aplexer#12 closed won't-do): there is no suppression flag, so
    /// the injected path has to be correct on its own for every caller.
    #[test]
    fn flash_status_cannot_bypass_the_boundary_gate() {
        let frame = opencode_shaped_frame(3);
        // A split inside a CSI parameter list -- the shape a real capture put
        // 11 of 54 status writes into.
        let needle = b"\x1b[38;5;";
        let split = frame
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap()
            + needle.len();
        let mut h = Harness::new();
        h.workload_emits(&frame[..split]);
        h.expire_sync_deferral();
        assert!(
            !h.ctx
                .screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .at_escape_boundary(),
            "the harness must actually be mid-sequence for this test to mean anything"
        );

        flash_status(&h.ctx, "FLASHED-MESSAGE");
        assert!(
            h.ctx.pending.load(Ordering::Relaxed),
            "a flash raised mid-sequence must be deferred, not written"
        );
        assert!(
            !Harness::row(&h.host, h.rows - 1, h.cols).contains("FLASHED-MESSAGE"),
            "nothing may reach the terminal while the stream is mid-sequence"
        );

        // The rest of the frame arrives; the frame loop flushes the deferral.
        h.workload_emits(&frame[split..]);
        h.assert_screens_agree("flash deferred across a mid-sequence split");
        assert!(
            Harness::row(&h.host, h.rows - 1, h.cols).contains("FLASHED-MESSAGE"),
            "the deferred flash must still be delivered, got {:?}",
            Harness::row(&h.host, h.rows - 1, h.cols)
        );
        for r in &h.redraws {
            assert!(
                r.at_escape_boundary,
                "a flash was written mid-escape-sequence"
            );
        }
    }

    /// **The fix must not depend on synchronized-output mode.** opencode and
    /// codex bracket every frame in `CSI ?2026 h/l`, but Claude Code -- the
    /// most-used agent here -- emits none at all, so if the boundary detection
    /// leaned on `?2026` it would be a strictly weaker code path for exactly
    /// the workload that matters most.
    ///
    /// It does not. `?2026` is a *soft* preference layered on top: it keeps a
    /// redraw out of a frame the workload declared, and is bounded by
    /// `STATUS_BAR_SYNC_DEFER_LIMIT` precisely so nothing can depend on it.
    /// The protection is `ClientScreen::at_escape_boundary()`, which is
    /// derived from the stream's own parser state and knows nothing about
    /// `?2026`.
    ///
    /// This test is the same all-offsets split walk as
    /// `status_redraw_never_splices_into_a_workload_frame`, over a frame that
    /// provably contains no synchronized-output markers, with the additional
    /// assertion that the synchronized-update gate never once fired -- so a
    /// green result here can only come from the escape-boundary gate.
    #[test]
    fn escape_boundary_gate_protects_a_workload_that_never_uses_synchronized_output() {
        let frame = claude_code_shaped_frame(0);
        assert!(
            !frame
                .windows(8)
                .any(|w| w == b"\x1b[?2026h" || w == b"\x1b[?2026l"),
            "this test is only meaningful on a frame with no ?2026 markers"
        );
        let mut deferred = 0usize;
        let mut written = 0usize;
        let mut redraws = 0usize;
        for split in (1..frame.len()).step_by(7) {
            let mut h = Harness::new();
            h.workload_emits(&frame[..split]);
            if h.status_redraw() {
                written += 1;
            } else {
                deferred += 1;
            }
            h.workload_emits(&frame[split..]);
            h.assert_screens_agree(&format!("no-sync split at byte {split}"));
            h.assert_bar_drawn(&format!("no-sync split at byte {split}"));
            // The workload's own `\x1b7`/`\x1b8` pair must have survived every
            // redraw: `ANCHORED` belongs at row 9 col 5, not wherever the bar
            // last left the cursor.
            assert!(
                Harness::row(&h.host, 8, h.cols).contains("ANCHORED"),
                "no-sync split at byte {split}: the workload's DECRC must land where it saved, row 9 was {:?}",
                Harness::row(&h.host, 8, h.cols)
            );
            for r in &h.redraws {
                assert!(
                    r.at_escape_boundary,
                    "no-sync split at byte {split}: a redraw was written mid-escape-sequence"
                );
                assert!(
                    !r.in_synchronized_update,
                    "no-sync split at byte {split}: this workload has no synchronized-output \
                     blocks, so the sync gate must never be what protected it"
                );
                redraws += 1;
            }
        }
        assert!(
            deferred > 0,
            "the frame must contain unsafe split points for this test to mean anything"
        );
        assert!(
            written > 0 && redraws > 0,
            "and the bar must still be drawn"
        );
    }

    /// A workload that uses the DEC save/restore-cursor register itself --
    /// Claude Code opens with exactly `\x1b7\x1b[r\x1b8`, opencode uses the
    /// same register via `CSI s`/`CSI u`, and every `tput sc`-style progress
    /// line inside a session does too. A terminal has one such register, so
    /// the status bar's old `\x1b7 ... \x1b8` bracket overwrote the workload's
    /// saved position and its own later restore jumped to *ours*.
    #[test]
    fn workload_saved_cursor_survives_a_status_redraw() {
        let mut h = Harness::new();
        h.workload_emits(b"\x1b[2J\x1b[5;1HHEADER");
        h.workload_emits(b"\x1b7"); // workload saves its cursor at row 5
        h.workload_emits(b"\x1b[20;1Hfooter drawn elsewhere");
        assert!(h.status_redraw(), "a ground-state redraw must be written");
        h.workload_emits(b"\x1b8TAIL"); // workload restores -- must be row 5
        h.assert_screens_agree("workload DECSC/DECRC");
        assert!(
            Harness::row(&h.host, 4, h.cols).contains("HEADERTAIL"),
            "the workload's own restore must land where it saved, got {:?}",
            Harness::row(&h.host, 4, h.cols)
        );
        h.assert_bar_drawn("workload DECSC/DECRC");
    }

    /// A workload that never goes idle -- an agent CLI mid-generation, the
    /// workload aplexer exists for -- never opens `STATUS_BAR_IDLE_GAP`, so
    /// every redraw it ever gets is the `STATUS_BAR_MAX_INTERVAL` forced one.
    /// This drives that worst case directly: a redraw requested after *every*
    /// chunk of a continuous multi-frame stream chopped at pseudo-random
    /// offsets. The bar must stay fresh, and no redraw may be written at an
    /// unsafe point or inside a declared frame.
    #[test]
    fn continuously_streaming_workload_redraws_only_at_frame_boundaries() {
        let mut stream = Vec::new();
        for generation in 0..6 {
            stream.extend_from_slice(&opencode_shaped_frame(generation));
        }
        // Phase A: the synchronized-output deferral in force. Every redraw
        // that reaches the terminal must be both at an escape boundary and
        // outside a declared frame.
        let mut h = Harness::new();
        // A deterministic LCG stands in for PTY read boundaries, which fall at
        // arbitrary byte offsets rather than on sequence boundaries.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut at = 0usize;
        while at < stream.len() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (((seed >> 33) % 900) + 100) as usize;
            let end = (at + len).min(stream.len());
            h.workload_emits(&stream[at..end]);
            h.status_redraw();
            at = end;
        }
        h.assert_screens_agree("continuous stream, sync respected");
        h.assert_bar_drawn("continuous stream, sync respected");
        assert!(
            !h.redraws.is_empty(),
            "a never-idle workload must still get its bar refreshed"
        );
        for (i, r) in h.redraws.iter().enumerate() {
            assert!(
                r.at_escape_boundary,
                "redraw {i} was written mid-escape-sequence"
            );
            assert!(
                !r.in_synchronized_update,
                "redraw {i} was written inside a synchronized-output frame"
            );
        }

        // Phase B: the deferral bounded out (a frame longer than
        // `STATUS_BAR_SYNC_DEFER_LIMIT`, or a block the workload never
        // closes). Redraws are now allowed inside a frame, so the
        // escape-boundary gate is the only protection left -- and it has to be
        // enough, which is the whole claim of this fix.
        let mut h = Harness::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut at = 0usize;
        while at < stream.len() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (((seed >> 33) % 900) + 100) as usize;
            let end = (at + len).min(stream.len());
            h.workload_emits(&stream[at..end]);
            h.expire_sync_deferral();
            h.status_redraw();
            at = end;
        }
        h.assert_screens_agree("continuous stream, deferral bounded out");
        h.assert_bar_drawn("continuous stream, deferral bounded out");
        assert!(
            h.redraws.len() >= 10,
            "with the deferral bounded out the bar must refresh often; got {} redraws",
            h.redraws.len()
        );
        assert!(
            h.redraws.iter().any(|r| r.in_synchronized_update),
            "this phase must actually exercise redraws inside a frame"
        );
        for (i, r) in h.redraws.iter().enumerate() {
            assert!(
                r.at_escape_boundary,
                "redraw {i} was written mid-escape-sequence"
            );
        }
    }

    /// docs/terminal-state-design.md section 7.1's reserved-row walk, which
    /// this replaces the old characterization test for. While the client
    /// re-asserts a workload's own DECSTBM sub-range, the host's bottom row is
    /// the screen bottom rather than a margin boundary, so a line feed on the
    /// workload's last row walked the host cursor onto the reserved row and
    /// left it there -- the workload's screen model and the host permanently
    /// one row apart. The client's model now detects that at the byte that
    /// causes it and splices in an absolute reposition.
    #[test]
    fn workload_line_feed_no_longer_reaches_the_reserved_row_under_a_sub_range() {
        let mut h = Harness::new();
        h.workload_emits(b"\x1b[5;15r");
        assert!(
            h.status_redraw(),
            "the bar re-asserts the workload sub-range"
        );
        h.workload_emits(b"\x1b[23;1HWORKLOAD-LAST-ROW\nWALKED");
        h.assert_screens_agree("sub-range line feed");
        assert_eq!(
            h.host.screen().cursor_position().0 + 1,
            23,
            "the cursor must stay on the workload's last row"
        );
        h.assert_bar_drawn("sub-range line feed");
    }

    /// The client must never write DECSC/DECRC into a stream it is only
    /// relaying -- not from the status bar and not from the layout code. This
    /// is a hard cut, not a preference: there is one save-cursor register and
    /// it belongs to the workload.
    #[test]
    fn client_never_writes_the_shared_save_cursor_register() {
        let ctx = status_ctx_for_test(true);
        let restore = ctx
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursor_restore();
        let mut bytes = status_bar_redraw(&ctx, true).expect("a ground-state redraw writes");
        bytes.extend_from_slice(&terminal_layout_sequence(24, &restore));
        bytes.extend_from_slice(TERMINAL_RESET_SEQUENCE);
        assert!(!bytes.is_empty());
        for pair in [&b"\x1b7"[..], b"\x1b8"] {
            assert!(
                !bytes.windows(2).any(|w| w == pair),
                "client emitted {pair:?}: {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }

    #[test]
    fn draw_status_bar_not_reserved_never_writes() {
        let ctx = status_ctx_for_test(false);
        assert!(!draw_status_bar(&ctx, false));
        assert!(!draw_status_bar(&ctx, true));
    }

    #[test]
    fn live_screen_refresh_repaints_model_contents_and_the_bar() {
        let ctx = status_ctx_for_test(true);
        feed_test_screen(&ctx.screen, b"recover-me\r\n");
        let bytes = live_screen_refresh_locked(&ctx).expect("ground-state refresh writes");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            bytes.windows(4).any(|w| w == b"\x1b[2J") || bytes.windows(3).any(|w| w == b"\x1b[J"),
            "refresh must clear before repainting: {text:?}"
        );
        assert!(
            text.contains("recover-me"),
            "refresh must include the live screen: {text:?}"
        );
        assert!(
            bytes.windows(4).any(|w| w == b"\x1b[7m"),
            "refresh must restore the status bar the snapshot's clear wiped: {text:?}"
        );
        assert!(!ctx.pending_refresh.load(Ordering::Relaxed));
    }

    #[test]
    fn live_screen_refresh_defers_mid_escape_sequence() {
        let ctx = status_ctx_for_test(true);
        feed_test_screen(&ctx.screen, b"\x1b[38;5;");
        assert!(live_screen_refresh_locked(&ctx).is_none());
        assert!(
            ctx.pending_refresh.load(Ordering::Relaxed),
            "a deferred refresh must be retried at the next safe boundary"
        );
    }

    #[test]
    fn draw_status_bar_dirty_check_reports_skip_vs_real_write() {
        let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        let _guard = StdoutToDevNull::new();
        let ctx = status_ctx_for_test(true);
        // Nothing drawn yet: even a non-forced call must actually write
        // (there's no `last_drawn` to compare against).
        assert!(
            draw_status_bar(&ctx, false),
            "first draw must be a real write"
        );
        // Same record/geometry, so the rendered text is unchanged: a
        // non-forced call must be a dirty-check no-op, not a real write --
        // this is exactly the case the timer-starvation bug got wrong by
        // treating a no-op the same as a real write for timer-reset
        // purposes.
        assert!(
            !draw_status_bar(&ctx, false),
            "unchanged text must be a dirty-check skip, not a real write"
        );
        // `force: true` must bypass the dirty-check unconditionally, since
        // that's the self-heal guarantee the overdue timer and every
        // switch/flash redraw rely on.
        assert!(
            draw_status_bar(&ctx, true),
            "force=true must always be a real write, even with unchanged text"
        );
    }

    #[test]
    fn real_zero_sized_pty_uses_conventional_geometry() {
        use std::os::fd::AsRawFd;

        let (_master, slave) = aplexer::open_pty(0, 0).unwrap();
        assert_eq!(
            terminal_size(slave.as_raw_fd()),
            Some((
                aplexer::screen::DEFAULT_TERMINAL_ROWS,
                aplexer::screen::DEFAULT_TERMINAL_COLS,
            ))
        );
    }

    // -- The typing bar is a live-stream writer, not a suspended one -------
    //
    // `i` (type-through) hands the keyboard back while the pager stays up,
    // and the relay streams workload bytes to the host again. The typing bar
    // is then client-originated output spliced into a live stream, with the
    // same two obligations as the live bar: never splice mid-sequence, and
    // repair the row when the workload's Erase-in-Display -- which ignores
    // scroll margins -- takes it out. Reproduced against a real zcodex
    // session: after wheel-up + `i`, the workload's first `CSI ... J` wiped
    // the bar and nothing ever rewrote it, because the frame loop `continue`d
    // past every bar path while scroll mode was active and the status tick's
    // dirty check saw unchanged text.

    fn ctx_in_typing_mode() -> StatusBarCtx {
        let ctx = status_ctx_for_test(true);
        ctx.scroll.active.store(true, Ordering::SeqCst);
        ctx.scroll.typing.store(true, Ordering::SeqCst);
        ctx
    }

    #[test]
    fn typing_bar_waits_for_an_escape_boundary_and_parks_until_one() {
        let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        let ctx = ctx_in_typing_mode();
        // Half a CSI sequence: the relayed stream is mid-escape, so the bar
        // write must be refused, parked for the frame loop, and nothing may
        // reach the terminal.
        feed_test_screen(&ctx.screen, b"\x1b[38;5;");
        let pipe = StdoutToPipe::new();
        assert!(
            !refresh_scroll_bar(&ctx),
            "a mid-sequence typing-bar write must be deferred"
        );
        assert!(
            pipe.take().is_empty(),
            "a deferred typing-bar write must not reach the terminal"
        );
        assert!(
            ctx.pending.load(Ordering::Relaxed),
            "a deferred typing-bar write must be parked for the frame loop"
        );
        // Complete the sequence: the parked write goes out at the boundary,
        // and `pending` (which forced the write past the dirty check) is
        // cleared so the tick's dirty check is honest again.
        feed_test_screen(&ctx.screen, b"m");
        let pipe = StdoutToPipe::new();
        assert!(refresh_scroll_bar(&ctx), "the parked write flushes");
        let text = String::from_utf8_lossy(&pipe.take()).into_owned();
        assert!(
            text.contains("TYPE"),
            "the typing wording must reach the bar row: {text:?}"
        );
        assert!(
            !ctx.pending.load(Ordering::Relaxed),
            "a delivered write must clear the parking flag"
        );
    }

    #[test]
    fn layout_erase_while_typing_repairs_an_unchanged_bar_row() {
        let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        let ctx = ctx_in_typing_mode();
        let pipe = StdoutToPipe::new();
        assert!(refresh_scroll_bar(&ctx), "first draw writes the bar");
        assert!(!pipe.take().is_empty());
        // The dirty check must skip when nothing changed -- this is what
        // kept the erased row blank forever before the fix: the text was
        // unchanged, so every later refresh saw "already drawn" and stopped.
        assert!(
            !refresh_scroll_bar(&ctx),
            "unchanged text must be a dirty-check skip"
        );
        // What the `Layout` arm does when the workload erased the screen
        // while typing: invalidate `last_drawn`, then refresh. The text is
        // byte-identical; the write must happen anyway, because the row the
        // text lives on no longer holds it.
        *ctx.last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let pipe = StdoutToPipe::new();
        assert!(
            refresh_scroll_bar(&ctx),
            "an invalidated dirty check must rewrite the bar row"
        );
        assert!(
            !pipe.take().is_empty(),
            "the repair must be a real write, not a bookkeeping update"
        );
    }

    #[test]
    fn pager_bar_without_typing_still_writes_unconditionally() {
        // Deliberate asymmetry, pinned so it reads as decided rather than
        // forgotten: with the pager up but NOT typing, the relay is
        // suspended, so there is no live stream to splice into and the bar
        // does not wait for a boundary -- a workload stopped mid-sequence
        // must not freeze the bar the user is actively reading against.
        let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
        let ctx = status_ctx_for_test(true);
        ctx.scroll.active.store(true, Ordering::SeqCst);
        feed_test_screen(&ctx.screen, b"\x1b[38;5;");
        let pipe = StdoutToPipe::new();
        assert!(
            refresh_scroll_bar(&ctx),
            "a stream-suspended write goes out at once"
        );
        assert!(!pipe.take().is_empty());
        assert!(
            !ctx.pending.load(Ordering::Relaxed),
            "a stream-suspended write never parks"
        );
    }

    // -- Issue #14: the resize path is a client-originated writer too ------
    //
    // `2db19d0` put the escape-boundary gate inside `draw_status_bar`, which
    // covered its eight callers and silently did not cover
    // `apply_terminal_layout` -- a ninth writer, driven by the resize
    // poller's wall clock, that wrote DECSTBM straight to stdout. Nothing
    // failed; a reviewer found it by reading. These tests are what fails
    // instead, and the last one is what fails for writer eleven.

    /// Runs the real resize path into a buffer instead of fd 1. Nothing here
    /// re-implements production's decision -- `apply_terminal_layout_to` is
    /// what `apply_terminal_layout` calls under the stdout lock -- and
    /// keeping the process's fd 1 out of it means these tests neither
    /// serialize on `FD1_GUARD` nor can catch another thread's stray write.
    fn resize_capturing(ctx: &StatusBarCtx, rows: u16, cols: u16) -> (bool, Vec<u8>) {
        let mut sink = Vec::new();
        let wrote = apply_terminal_layout_to(&mut sink, ctx, rows, cols);
        assert_eq!(
            wrote,
            !sink.is_empty(),
            "the resize path's return value must agree with what it actually wrote"
        );
        (wrote, sink)
    }

    fn flush_capturing(ctx: &StatusBarCtx) -> (bool, Vec<u8>) {
        let mut sink = Vec::new();
        let wrote = flush_pending_layout_to(&mut sink, ctx);
        (wrote, sink)
    }

    /// Backdates the parked resize's deadline, standing in for
    /// `LAYOUT_DEFER_LIMIT` having elapsed without the stream ever reaching
    /// a boundary -- a workload that stopped mid-escape-sequence.
    fn expire_layout_deferral(ctx: &StatusBarCtx) {
        let mut pending = ctx
            .pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = pending.as_mut() {
            p.since = Instant::now()
                .checked_sub(LAYOUT_DEFER_LIMIT * 2)
                .expect("backdate the layout deadline");
        }
    }

    /// A resize raised while the workload is mid-escape-sequence must put
    /// nothing on the wire. This is the splice the issue describes: the host
    /// terminal would abandon the workload's half-emitted CSI and print its
    /// remaining parameter bytes as literal text.
    ///
    /// The geometry is still recorded on the spot, deliberately: the
    /// physical terminal has already changed size, and `TermGeom` is
    /// internal state rather than output, so the status bar must start
    /// targeting the real last row immediately.
    #[test]
    fn resize_mid_escape_sequence_defers_decstbm_instead_of_splicing() {
        let ctx = status_ctx_for_test(true);

        // Control: at a boundary the same call writes, so a later "nothing
        // was written" assertion means the gate, not a broken fixture.
        let (wrote, bytes) = resize_capturing(&ctx, 24, 80);
        assert!(wrote, "a resize at an escape boundary must be written");
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[1;23r"),
            "expected the row reservation for a 24-row terminal, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(
            ctx.pending_layout
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "a resize that was written must leave nothing parked"
        );

        // Now mid-CSI, exactly as a PTY read boundary leaves the stream
        // about half the time under a streaming TUI.
        feed_test_screen(&ctx.screen, b"\x1b[38;5;");
        assert!(
            !ctx.screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .at_escape_boundary(),
            "the fixture must actually be mid-sequence for this test to mean anything"
        );
        let (wrote, bytes) = resize_capturing(&ctx, 30, 100);
        assert!(!wrote, "a resize raised mid-sequence must not be written");
        assert!(
            bytes.is_empty(),
            "nothing may reach the terminal mid-sequence, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
        let parked = *ctx
            .pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let parked = parked.expect("a deferred resize must be parked, not dropped");
        assert_eq!((parked.rows, parked.cols), (30, 100));
        let geom = *ctx.term.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            (geom.rows, geom.cols),
            (30, 100),
            "the new physical geometry must be recorded even while the bytes wait"
        );
    }

    /// Deferral is not dropping. The parked resize must reach the terminal
    /// at the next boundary -- and must carry the *latest* geometry, since a
    /// superseded size would leave the workload rendering at the wrong
    /// geometry just as surely as dropping it would.
    #[test]
    fn deferred_resize_is_delivered_at_the_next_boundary_with_the_latest_geometry() {
        let ctx = status_ctx_for_test(true);
        feed_test_screen(&ctx.screen, b"\x1b[38;5;");

        assert!(!resize_capturing(&ctx, 28, 80).0);
        // A second resize while the first is still parked: the user kept
        // dragging the window edge.
        assert!(!resize_capturing(&ctx, 32, 100).0);
        let (wrote, bytes) = flush_capturing(&ctx);
        assert!(
            !wrote && bytes.is_empty(),
            "a flush while still mid-sequence must stay silent, got {:?}",
            String::from_utf8_lossy(&bytes)
        );

        // The workload completes its sequence: the stream is at a boundary
        // again, which is exactly what the frame loop's flush waits for.
        feed_test_screen(&ctx.screen, b"91m");
        let (wrote, bytes) = flush_capturing(&ctx);
        assert!(wrote, "the deferred resize must be delivered, not dropped");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("\x1b[1;31r"),
            "the latest geometry (32 rows -> DECSTBM 1;31) must be the one delivered, got {text:?}"
        );
        assert!(
            !text.contains("\x1b[1;27r"),
            "a superseded deferred resize must not be the one delivered, got {text:?}"
        );
        assert!(
            ctx.pending_layout
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "a delivered resize must clear the parking slot"
        );
        // Idempotent: nothing parked, nothing written.
        let (wrote, bytes) = flush_capturing(&ctx);
        assert!(
            !wrote && bytes.is_empty(),
            "flushing with nothing parked must be a no-op, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    /// The one case the boundary gate cannot wait out: a workload that stops
    /// emitting part-way through an escape sequence. There is no next
    /// boundary, so `LAYOUT_DEFER_LIMIT` writes anyway -- one spliced frame
    /// beats a host terminal left scrolling the old geometry for the rest of
    /// the attach, which is the failure the issue calls worse than the
    /// splice. This asserts the exemption rather than describing it.
    #[test]
    fn deferred_resize_is_written_once_the_defer_limit_expires() {
        let ctx = status_ctx_for_test(true);
        feed_test_screen(&ctx.screen, b"\x1b[38;5;");
        assert!(!resize_capturing(&ctx, 20, 80).0);

        expire_layout_deferral(&ctx);
        assert!(
            !ctx.screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .at_escape_boundary(),
            "the stream must still be mid-sequence: the point is that the deadline, \
             not a recovered boundary, is what delivers this"
        );
        let (wrote, bytes) = flush_capturing(&ctx);
        assert!(
            wrote,
            "past LAYOUT_DEFER_LIMIT the resize must go out rather than be stranded"
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[1;19r"),
            "expected the row reservation for a 20-row terminal, got {:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(
            ctx.pending_layout
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "the deadline write must also clear the parking slot"
        );
    }

    /// The screen-level statement of the same thing, through a real `vt100`
    /// host terminal: after a resize raised in the middle of a workload's
    /// absolute-positioning sequence, every row the workload can reach must
    /// still render exactly what the workload drew.
    ///
    /// The second half is the control. It replays the identical resize the
    /// *ungated* code would have written at the same offset, and asserts the
    /// host screen is then wrong -- so this test fails if the gate is
    /// removed, rather than passing because the splice happened to be
    /// harmless.
    #[test]
    fn resize_across_a_mid_sequence_split_leaves_the_host_screen_intact() {
        let ctx = status_ctx_for_test(true);
        let (rows, cols) = (24u16, 80u16);
        let mut host = vt100::Parser::new(rows, cols, 0);
        let mut ungated = vt100::Parser::new(rows, cols, 0);
        let mut workload = vt100::Parser::new(rows - 1, cols, 0);
        for p in [&mut host, &mut ungated] {
            p.process(format!("\x1b[1;{}r", rows - 1).as_bytes());
        }

        // Ink-shaped output: words painted at absolute columns, which is
        // what makes a misaligned injection weld two frames onto one row.
        let frame = b"\x1b[2;1H\x1b[0m\x1b[2GQuick\x1b[8Gsafety\x1b[16Gcheck";
        // Split inside `\x1b[16G` -- a CSI with its parameters half emitted,
        // which is what a PTY read boundary looks like about half the time
        // under a streaming TUI.
        let split = frame.len() - 7;
        for p in [&mut host, &mut ungated] {
            p.process(&frame[..split]);
        }
        workload.process(&frame[..split]);
        feed_test_screen(&ctx.screen, &frame[..split]);

        // What the resize poller does at this instant.
        let (wrote, _) = resize_capturing(&ctx, 20, cols);
        assert!(!wrote, "the resize must be deferred here, not written");
        // What it used to do: DECSTBM straight onto the wire, mid-CSI.
        let restore = ctx
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursor_restore();
        ungated.process(&terminal_layout_sequence(20, &restore));

        for p in [&mut host, &mut ungated] {
            p.process(&frame[split..]);
        }
        workload.process(&frame[split..]);
        feed_test_screen(&ctx.screen, &frame[split..]);
        let (wrote, bytes) = flush_capturing(&ctx);
        assert!(wrote, "the deferred resize must still be delivered");
        host.process(&bytes);

        let row_of = |p: &vt100::Parser| p.screen().contents_between(1, 0, 1, cols);
        assert_eq!(
            row_of(&host),
            row_of(&workload),
            "the gated resize must leave the host row exactly as the workload drew it"
        );
        assert_ne!(
            row_of(&ungated),
            row_of(&workload),
            "control: the ungated resize must actually corrupt this row, otherwise \
             this test would pass with the gate removed"
        );
    }

    // -- The structural guard --------------------------------------------
    //
    // Issue #14 was a missed *caller*, not a subtle race, and the next one
    // will be too: someone adds a writer, does not know the gate exists,
    // and no test notices. So the rule is enforced over the source text --
    // every function in this file that can put bytes on the host terminal
    // is enumerated here, with how it is allowed to do so.

    /// Every function in src/bin/a.rs, outside this test module, that calls
    /// `write_all` -- i.e. that reaches the terminal without going through
    /// the funnel -- and the reason it is allowed to. A new one fails
    /// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`.
    const RAW_TERMINAL_WRITERS: &[(&str, &str)] = &[
        (
            "write_client_locked",
            "the gate itself: it performs the boundary check it is named for",
        ),
        (
            "write_locked",
            "attach start and detach only, pinned by WRITE_LOCKED_CALLERS below -- there is \
             no relayed stream to splice before the first workload byte or after the last, \
             and neither may be deferrable",
        ),
        (
            "relay_to_terminal",
            "relays the workload's own bytes; it *is* the stream, not an injection",
        ),
        (
            "feed_and_write",
            "the attach snapshot and a switch's replayed screen: a full repaint that replaces \
             the stream rather than splicing into it, fed to the model under the same lock",
        ),
        (
            "cmd_capture",
            "`a capture` on a plain stdout; no attach, no relayed stream",
        ),
    ];

    /// Every client-originated injection into a live relayed stream. Each
    /// goes through `write_client_locked`, so each is boundary-gated by
    /// construction rather than by remembering. Listed so the census is
    /// visible: `apply_terminal_layout` was the ninth writer that nobody had
    /// written down (issue #14).
    const FUNNELLED_WRITERS: &[&str] = &[
        "apply_terminal_layout_to",
        "draw_status_bar",
        "redraw_live_screen",
        "paint_scroll_view",
        "refresh_scroll_bar",
        "paint_live_screen",
        "sync_client_mouse",
        "paint_key_overlay",
        "dismiss_key_overlay",
    ];

    /// `write_locked` writes unconditionally, so it is a second route to the
    /// terminal and would be a hole in the funnel if it could be called from
    /// anywhere. These are the only two places allowed to.
    const WRITE_LOCKED_CALLERS: &[&str] = &["attach", "reset_terminal"];

    /// This file's source with the test module cut out. Compiled in, so it
    /// is the same text the rest of the binary was built from.
    fn production_source_lines() -> Vec<&'static str> {
        let src = include_str!("a.rs");
        let lines: Vec<&str> = src.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.starts_with("mod switching_tests {"))
            .expect("src/bin/a.rs must contain the switching_tests module");
        let end = start
            + 1
            + lines[start + 1..]
                .iter()
                .position(|l| *l == "}")
                .expect("the test module must close at column 0");
        let mut production = lines[..start].to_vec();
        production.extend_from_slice(&lines[end + 1..]);
        production
    }

    /// Maps each line matching `needle` to the name of the nearest
    /// preceding `fn` declaration. Comment lines are skipped so a doc
    /// comment mentioning a call is not mistaken for one.
    fn enclosing_fns_of(lines: &[&str], needle: &str) -> Vec<(String, String)> {
        let mut current = String::new();
        let mut hits = Vec::new();
        for line in lines {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for prefix in ["fn ", "pub fn ", "pub(crate) fn ", "unsafe fn "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    current = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    break;
                }
            }
            if trimmed.contains(needle) {
                hits.push((current.clone(), trimmed.to_string()));
            }
        }
        hits
    }

    /// The body of a top-level `fn`, from its declaration to the `}` that
    /// closes it at column 0.
    fn top_level_fn_body(lines: &[&str], name: &str) -> String {
        let decl = format!("fn {name}(");
        let start = lines
            .iter()
            .position(|l| l.starts_with(&decl))
            .unwrap_or_else(|| panic!("no top-level `fn {name}` in src/bin/a.rs"));
        let end = start
            + 1
            + lines[start + 1..]
                .iter()
                .position(|l| *l == "}")
                .unwrap_or_else(|| panic!("`fn {name}` is not closed at column 0"));
        lines[start..=end].join("\n")
    }

    /// The criterion the issue calls the most valuable one: a *new* ungated
    /// writer fails here, instead of shipping and being found by a reviewer
    /// reading the file (which is how #14 was found, after #5 declared the
    /// class fixed).
    ///
    /// Four things are pinned:
    ///
    /// 1. the exact set of functions that can write to the terminal;
    /// 2. that each one either goes through `write_client_locked` or carries
    ///    a written reason why it is not an injection;
    /// 3. that `write_client_locked` really is the boundary check, and that
    ///    `write_locked` -- the unconditional second route -- is reachable
    ///    only from attach start and detach;
    /// 4. that the deadline exemption stays a single call site.
    #[test]
    fn every_client_terminal_write_site_is_gated_or_explicitly_exempt() {
        use std::collections::BTreeSet;

        let lines = production_source_lines();

        // 1. Nobody new may reach `write_all` directly.
        let raw: BTreeSet<String> = enclosing_fns_of(&lines, "write_all")
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        let declared_raw: BTreeSet<String> = RAW_TERMINAL_WRITERS
            .iter()
            .map(|(n, _)| (*n).to_string())
            .collect();
        let undeclared: Vec<&String> = raw.difference(&declared_raw).collect();
        assert!(
            undeclared.is_empty(),
            "new raw write(s) to the host terminal, not listed in RAW_TERMINAL_WRITERS: \
             {undeclared:?}. Client-originated bytes must go through `write_client_locked` \
             (the escape-boundary gate) instead; if the write genuinely cannot splice a \
             relayed stream, add it to RAW_TERMINAL_WRITERS with that reason. This is issue \
             #14: an ungated writer corrupts a workload's half-emitted escape sequences."
        );
        let stale: Vec<&String> = declared_raw.difference(&raw).collect();
        assert!(
            stale.is_empty(),
            "RAW_TERMINAL_WRITERS lists function(s) that no longer call write_all: {stale:?}. \
             Drop them, so the list stays an accurate census rather than folklore."
        );

        // 2. The injection census: gated by construction, but written down,
        //    because #14 was a writer nobody had written down.
        let funnelled: BTreeSet<String> = enclosing_fns_of(&lines, "write_client_locked(")
            .into_iter()
            .map(|(f, _)| f)
            .filter(|f| f != "write_client_locked")
            .collect();
        let declared_funnelled: BTreeSet<String> =
            FUNNELLED_WRITERS.iter().map(|n| (*n).to_string()).collect();
        assert_eq!(
            funnelled, declared_funnelled,
            "the set of client-originated injections changed. A new one is already \
             boundary-gated (that is what `write_client_locked` is for) -- add its name to \
             FUNNELLED_WRITERS so the census stays true, and check that it parks a refused \
             write for a later boundary instead of dropping it."
        );
        for name in FUNNELLED_WRITERS {
            let body = top_level_fn_body(&lines, name);
            assert!(
                body.contains("write_client_locked("),
                "`{name}` no longer goes through `write_client_locked`, so its bytes are not \
                 boundary-gated (issue #14)"
            );
        }

        let gate = top_level_fn_body(&lines, "write_client_locked");
        assert!(
            gate.contains("at_escape_boundary()"),
            "`write_client_locked` must be the escape-boundary check; every Funnelled writer \
             above relies on it being one"
        );

        let raw_callers: Vec<String> = enclosing_fns_of(&lines, "write_locked(")
            .into_iter()
            .map(|(f, _)| f)
            .filter(|f| f != "write_locked" && f != "write_client_locked")
            .collect();
        for caller in &raw_callers {
            assert!(
                WRITE_LOCKED_CALLERS.contains(&caller.as_str()),
                "`{caller}` calls `write_locked`, which writes without consulting the escape \
                 boundary. Only attach start and detach may (there is no relayed stream to \
                 splice at either); a live injection must use `write_client_locked`."
            );
        }

        let past_deadline: Vec<(String, String)> =
            enclosing_fns_of(&lines, "BoundaryPolicy::PastDeadline")
                .into_iter()
                .filter(|(f, _)| f != "write_client_locked")
                .collect();
        assert_eq!(
            past_deadline.len(),
            1,
            "`BoundaryPolicy::PastDeadline` is the client's only exemption from the boundary \
             gate and must stay one narrow call site (the resize deadline), found: {past_deadline:?}"
        );
        assert_eq!(
            past_deadline[0].0, "apply_terminal_layout_to",
            "the deadline exemption belongs to the resize path and nothing else"
        );
    }

    /// `BoundaryPolicy::StreamSuspended` says "the relay is not writing to
    /// the host at all right now", which is true of exactly one class of
    /// thing: a *client modal* that has taken the host terminal away from the
    /// relay entirely -- the scroll-mode pager, and the `Ctrl-b` key overlay,
    /// which suspends the relay the same way and for the same reason (see
    /// `KeyOverlay`). Pinned the same way the deadline exemption is, so it
    /// cannot quietly become a general-purpose way around the gate.
    ///
    /// Two properties, both of which the variant's correctness rests on:
    /// only a modal's writers may pass it, and every write that is the *first*
    /// one after the suspension must lead with `SCROLL_CANCEL` -- the `CAN`
    /// that ends whatever sequence the host was part-way through when the
    /// relay was suspended.
    #[test]
    fn scroll_mode_writes_are_the_only_stream_suspended_ones() {
        use std::collections::BTreeSet;

        /// Every function allowed to write while the relay is suspended, and
        /// where its `SCROLL_CANCEL` prefix comes from.
        const SUSPENDED_WRITERS: &[(&str, bool)] = &[
            // (name, must build its own SCROLL_CANCEL-led sequence)
            ("paint_scroll_view", true),
            ("paint_live_screen", true),
            // The overlay's two frames. Either can be the first write after
            // the relay was suspended -- `paint_key_overlay` always is, and
            // `dismiss_key_overlay` is whenever nothing was repainted in
            // between (a resize, say) -- so both build their own.
            ("paint_key_overlay", true),
            ("dismiss_key_overlay", true),
            // The bar row is drawn *into* a screen the pager already owns
            // and already cancelled; it is not the first write after the
            // suspension, so it needs no CAN of its own.
            ("refresh_scroll_bar", false),
            // Hands the mouse over while the pager is up; same reasoning.
            ("sync_client_mouse", false),
        ];

        let lines = production_source_lines();
        let found: BTreeSet<String> = enclosing_fns_of(&lines, "BoundaryPolicy::StreamSuspended")
            .into_iter()
            .map(|(f, _)| f)
            .filter(|f| f != "write_client_locked")
            .collect();
        let declared: BTreeSet<String> = SUSPENDED_WRITERS
            .iter()
            .map(|(n, _)| (*n).to_string())
            .collect();
        assert_eq!(
            found, declared,
            "`BoundaryPolicy::StreamSuspended` bypasses the escape-boundary gate on the \
             grounds that scroll mode has suspended the relay entirely. Only scroll mode may \
             claim that. If a new writer genuinely runs with the relay suspended, add it here \
             with whether it must lead with SCROLL_CANCEL; otherwise use BoundaryPolicy::Defer."
        );
        for (name, needs_cancel) in SUSPENDED_WRITERS {
            if !needs_cancel {
                continue;
            }
            let body = top_level_fn_body(&lines, name);
            assert!(
                body.contains("SCROLL_CANCEL"),
                "`{name}` writes the first bytes after the relay is suspended, so it must lead \
                 with SCROLL_CANCEL (CAN) to end whatever escape sequence the host was \
                 part-way through -- that prefix is what makes skipping the boundary gate safe"
            );
        }
    }
}

fn send_data(writer: &Arc<Mutex<UnixStream>>, data: &[u8]) -> Result<()> {
    let mut stream = writer.lock().map_err(|_| anyhow!("socket lock poisoned"))?;
    write_frame(&mut *stream, FrameKind::Data, data)
}
fn send_control(writer: &Arc<Mutex<UnixStream>>, control: &AttachControl) -> Result<()> {
    let mut stream = writer.lock().map_err(|_| anyhow!("socket lock poisoned"))?;
    write_json(&mut *stream, control)
}

const ATTACH_CLEANUP_SIGNALS: [i32; 4] = [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGINT];
static ATTACH_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Async-signal-safe half of attach cleanup. The handler deliberately does
/// nothing except write one byte to a nonblocking self-pipe. Terminal I/O,
/// socket locking, termios restoration, and allocation all remain on normal
/// Rust threads.
extern "C" fn attach_cleanup_signal(signal: i32) {
    let fd = ATTACH_SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        unsafe {
            libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
        }
    }
}

struct AttachSignalBridge {
    read_fd: i32,
    write_fd: i32,
    previous: Vec<(i32, libc::sigaction)>,
    watcher: Option<thread::JoinHandle<()>>,
    caught: Arc<AtomicI32>,
}

impl AttachSignalBridge {
    fn install(writer: Arc<Mutex<UnixStream>>, active: Arc<AtomicBool>) -> Result<Self> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error()).context("create attach signal pipe");
        }
        let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(error).context("make attach signal pipe nonblocking");
        }

        ATTACH_SIGNAL_WRITE_FD.store(fds[1], Ordering::Release);
        let mut previous = Vec::with_capacity(ATTACH_CLEANUP_SIGNALS.len());
        for signal in ATTACH_CLEANUP_SIGNALS {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = attach_cleanup_signal as *const () as usize;
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            let mut old = unsafe { std::mem::zeroed::<libc::sigaction>() };
            if unsafe { libc::sigaction(signal, &action, &mut old) } != 0 {
                let error = io::Error::last_os_error();
                for (installed, prior) in previous.iter().rev() {
                    unsafe {
                        libc::sigaction(*installed, prior, std::ptr::null_mut());
                    }
                }
                ATTACH_SIGNAL_WRITE_FD.store(-1, Ordering::Release);
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(error).with_context(|| format!("install attach signal {signal}"));
            }
            previous.push((signal, old));
        }

        let caught = Arc::new(AtomicI32::new(0));
        let watcher_caught = caught.clone();
        let read_fd = fds[0];
        let watcher = thread::spawn(move || {
            let mut byte = 0u8;
            loop {
                let read = unsafe { libc::read(read_fd, std::ptr::from_mut(&mut byte).cast(), 1) };
                if read == 1 {
                    if byte == 0 {
                        return;
                    }
                    watcher_caught
                        .compare_exchange(0, i32::from(byte), Ordering::SeqCst, Ordering::Relaxed)
                        .ok();
                    active.store(false, Ordering::Relaxed);
                    if let Ok(stream) = writer.lock() {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                    return;
                }
                if read < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
        });
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
            previous,
            watcher: Some(watcher),
            caught,
        })
    }

    fn finish(mut self) -> Option<i32> {
        self.stop_and_restore();
        match self.caught.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    fn stop_and_restore(&mut self) {
        if self.write_fd < 0 {
            return;
        }
        let stop = 0u8;
        unsafe {
            libc::write(self.write_fd, std::ptr::from_ref(&stop).cast(), 1);
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        ATTACH_SIGNAL_WRITE_FD.store(-1, Ordering::Release);
        for (signal, prior) in self.previous.iter().rev() {
            unsafe {
                libc::sigaction(*signal, prior, std::ptr::null_mut());
            }
        }
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
        self.read_fd = -1;
        self.write_fd = -1;
    }
}

impl Drop for AttachSignalBridge {
    fn drop(&mut self) {
        self.stop_and_restore();
    }
}

struct RawMode {
    fd: i32,
    old: libc::termios,
}
impl RawMode {
    fn enter(fd: i32) -> Result<Self> {
        let mut old = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, old.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error()).context("tcgetattr");
        }
        let old = unsafe { old.assume_init() };
        let mut raw = unsafe { std::ptr::read(&old) };
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr");
        }
        Ok(Self { fd, old })
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.old);
        }
    }
}
fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    let mut ws = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, ws.as_mut_ptr()) } < 0 {
        return None;
    }
    let ws = unsafe { ws.assume_init() };
    // A newly-created or deliberately-unsized PTY reports 0x0. Treat it as
    // "geometry unknown", using the same conventional fallback as the
    // worker's screen model; 1x1 is a degenerate vt100 grid and is not a
    // useful representation of any interactive terminal.
    Some((
        if ws.ws_row == 0 {
            aplexer::screen::DEFAULT_TERMINAL_ROWS
        } else {
            ws.ws_row
        },
        if ws.ws_col == 0 {
            aplexer::screen::DEFAULT_TERMINAL_COLS
        } else {
            ws.ws_col
        },
    ))
}

fn parse_signal(raw: &str) -> Result<i32> {
    let upper = raw.trim().trim_start_matches("SIG").to_ascii_uppercase();
    let value = match upper.as_str() {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => upper.parse::<i32>().context("unknown signal")?,
    };
    if !(1..=64).contains(&value) {
        bail!("signal out of range");
    }
    Ok(value)
}
fn parse_hex(input: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(input)?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    if !text.is_ascii() {
        bail!("hex input must contain only ASCII hexadecimal digits");
    }
    if text.len() % 2 != 0 {
        bail!("hex input must contain an even number of digits");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(Into::into))
        .collect()
}
