use anyhow::{anyhow, bail, Context, Result};
use aplexer::messaging::*;
use aplexer::*;
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::ffi::CString;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
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
    about = "Daemonless durable PTY sessions",
    disable_help_subcommand = true
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
    Start(StartArgs),
    /// List sessions grouped by workspace.
    #[command(alias = "ls")]
    List(ListArgs),
    /// Same as `list`, but always machine-readable (see also `list --json`).
    Snapshot(ListArgs),
    /// Attach to a session's live PTY (Ctrl-b d to detach).
    Attach(AttachArgs),
    /// Send input to a session without attaching.
    Send(SendArgs),
    /// Print a session's captured output/scrollback.
    Capture(CaptureArgs),
    /// Show a session's phase, exit info, and liveness.
    Status(TargetArgs),
    /// Signal a session's workload and clean up its records.
    Kill(KillArgs),
    /// Forget a dead session's records without claiming its workloads stopped.
    Forget(ForgetArgs),
    /// Remove dead, unreclaimable session records and their durable history.
    Prune,
    /// Change a session's tag.
    Rename(RenameArgs),
    /// List configured/discovered engines.
    Engines,
    /// List configured profiles.
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
    Doctor,
    /// Print the current session's identity (workspace/tag/engine/profile).
    Whoami,
    /// Send or read messages between sibling agent sessions.
    Message(MessageArgs),
    /// Stream session lifecycle events.
    Watch(WatchArgs),
    /// Read/follow a session's conversation transcript.
    Transcript(TranscriptArgs),
    /// Print a shell completion script for `a` to stdout.
    Completions(CompletionsArgs),
    /// Print the attach-mode keyboard shortcuts (Ctrl-b prefix, detach).
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
struct QuickAttachArgs {
    /// 1-based index into the workspaces as shown by `a list` (alphabetical).
    workspace_index: usize,
    /// 1-based index into that workspace's sessions (list order), or a
    /// literal tag. Defaults to that workspace's first session.
    session: Option<String>,
}

#[derive(Args)]
struct QuickLaunchArgs {
    rest: Vec<String>,
}

#[derive(Args)]
struct CompletionsArgs {
    /// Shell to generate a completion script for.
    shell: Shell,
}

#[derive(Args)]
struct StartArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long, default_value = "default")]
    tag: String,
    #[arg(long)]
    engine: Option<String>,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    #[arg(long)]
    memory: Option<String>,
    #[arg(long)]
    pids: Option<u64>,
    #[arg(long)]
    cpu_quota_us: Option<u64>,
    #[arg(long, default_value_t = 100_000)]
    cpu_period_us: u64,
    #[arg(long)]
    history_bytes: Option<usize>,
    #[arg(long)]
    attach: bool,
    #[arg(long, default_value_t = 10_000)]
    startup_timeout_ms: u64,
    /// Keep the engine's confirmation/sandbox prompts. Default is to append
    /// the engine's skip-permissions argv (`--dangerously-bypass-approvals-
    /// and-sandbox` / `--dangerously-skip-permissions` / `--always-approve`).
    #[arg(long)]
    no_skip_permissions: bool,
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
        help = "Full UUID or an unambiguous UUID prefix"
    )]
    selector: Option<String>,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    running: bool,
}
#[derive(Args)]
struct AttachArgs {
    #[command(flatten)]
    target: TargetArgs,
    #[arg(long)]
    history_bytes: Option<usize>,
}
#[derive(Args)]
struct SendArgs {
    #[command(flatten)]
    target: TargetArgs,
    #[arg(value_name = "TEXT")]
    text: Option<String>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    hex: bool,
    #[arg(long)]
    enter: bool,
}
#[derive(Args)]
struct CaptureArgs {
    #[command(flatten)]
    target: TargetArgs,
    #[arg(long)]
    bytes: Option<usize>,
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
    #[arg(long, default_value = "TERM")]
    signal: String,
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
    #[arg(value_name = "SESSION")]
    selector: String,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
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
    Send(MessageSendArgs),
    /// Reply to a received message (threads via reply_to).
    Reply(MessageReplyArgs),
    /// List unread messages addressed to the calling session.
    Inbox(MessageInboxArgs),
    /// Show the whole workspace conversation, in id (time) order.
    Log(MessageLogArgs),
    /// Show one message by id.
    Show(MessageShowArgs),
    /// Acknowledge messages so they stop appearing in `inbox`.
    Ack(MessageAckArgs),
    /// Prune expired/over-cap messages from a workspace mailbox.
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
        help = "With --pane: suppress the '[aplexer message from ...]' frame and trailing return"
    )]
    raw: bool,
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
    #[arg(value_name = "TEXT")]
    text: String,
}

#[derive(Args)]
struct MessageReplyArgs {
    #[arg(value_name = "MESSAGE_ID")]
    message_id: Uuid,
    #[command(flatten)]
    pane_delivery: PaneDeliveryArgs,
    #[arg(long, value_name = "TAG")]
    from: Option<String>,
    #[arg(long, value_name = "JSON")]
    data: Option<String>,
    #[arg(long, value_name = "KIND", help = "Defaults to \"reply\"")]
    kind: Option<String>,
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
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
}

#[derive(Args)]
struct MessageShowArgs {
    #[arg(value_name = "MESSAGE_ID")]
    message_id: Uuid,
}

#[derive(Args)]
struct MessageAckArgs {
    #[arg(value_name = "MESSAGE_ID")]
    message_ids: Vec<Uuid>,
    #[arg(
        long,
        help = "Ack every currently-unread message addressed to this consumer"
    )]
    all: bool,
    #[arg(long, value_name = "TAG")]
    from: Option<String>,
}

#[derive(Args)]
struct MessageGcArgs {
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
    let command = cli
        .command
        .unwrap_or(Commands::List(ListArgs { running: false }));
    match command {
        Commands::Start(args) => cmd_start(&paths, args, cli.json),
        Commands::List(args) => cmd_list(&paths, args, cli.json),
        Commands::Snapshot(args) => cmd_list(&paths, args, true),
        Commands::Attach(args) => {
            let record = resolve(&paths, &args.target)?;
            attach(&paths, &record, args.history_bytes)
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
        Commands::Doctor => cmd_doctor(&paths, cli.json),
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
    resolve_record(
        paths,
        target.selector.as_deref(),
        target.workspace.as_deref(),
        target.tag.as_deref(),
    )
}

fn cmd_start(paths: &Paths, args: StartArgs, json_output: bool) -> Result<()> {
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
        attach(paths, &ready, None)?;
    }
    Ok(())
}

fn cmd_list(paths: &Paths, args: ListArgs, json_output: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&aplexer::api::snapshot_json(paths, args.running)?)?
        );
        return Ok(());
    }
    let mut records = list_records(paths)?;
    if args.running {
        records.retain(|r| r.worker_phase_active() && r.worker_alive());
    }
    // Group by workspace as a compact tree -- spec.md's own presentation of
    // the model (sections 2 and 22.1) is a workspace tree with tags
    // underneath, not a flat table repeating the workspace on every row.
    // `a <N>` quick-attach (see cmd_quick_attach/resolve_quick_index) numbers
    // workspaces and sessions using this exact same grouping, so the
    // `[N]`/session-index prefixes printed below are not decoration -- they
    // are the literal numbers `a <N>` and `a <N> <M>` resolve against.
    let by_workspace = group_by_workspace(records);
    let home = env::var_os("HOME").map(PathBuf::from);
    let color = color_enabled();
    for (workspace_index, (workspace, group)) in by_workspace.iter().enumerate() {
        if workspace_index > 0 {
            println!();
        }
        let (running, total) = running_count(group);
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
            &format!("{dot} {}", running_summary(group)),
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
            let state = display_state(&r.phase, r.worker_alive());
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
        "running" => ("\u{25CF}", ANSI_GREEN),
        "starting" | "exiting" => ("\u{25D0}", ANSI_YELLOW),
        "failed" | "broken" => ("\u{2717}", ANSI_RED),
        _ => ("\u{25CB}", ANSI_GRAY), // "exited"
    }
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

/// A persisted `phase` of Starting/Running/Exiting only means the worker
/// *said* it was in that phase the last time it wrote its record -- if the
/// worker process has since died (e.g. SIGKILL, which gives it no chance to
/// update the record), that phase is stale. `phase` and worker liveness are
/// different facts (spec.md 20: "session worker alive" vs "workload alive"
/// vs "agent semantic state" are different facts) so this only affects
/// display, not the persisted phase itself.
fn display_state(phase: &Phase, worker_alive: bool) -> &'static str {
    if matches!(phase, Phase::Starting | Phase::Running | Phase::Exiting) && !worker_alive {
        "broken"
    } else {
        phase_name(phase)
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

fn running_count(group: &[SessionRecord]) -> (usize, usize) {
    let running = group
        .iter()
        .filter(|r| display_state(&r.phase, r.worker_alive()) == "running")
        .count();
    (running, group.len())
}

fn running_summary(group: &[SessionRecord]) -> String {
    let (running, total) = running_count(group);
    if running == total {
        format!("running {running}")
    } else if running == 0 {
        format!("stopped {total}")
    } else {
        format!("running {running}/{total}")
    }
}

fn group_by_workspace(records: Vec<SessionRecord>) -> Vec<(PathBuf, Vec<SessionRecord>)> {
    let mut groups: Vec<(PathBuf, Vec<SessionRecord>)> = Vec::new();
    for r in records {
        match groups.iter_mut().find(|(ws, _)| *ws == r.workspace) {
            Some((_, group)) => group.push(r),
            None => groups.push((r.workspace.clone(), vec![r])),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    groups
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
        if existing.worker_phase_active() && existing.worker_alive() {
            return attach(paths, &existing, None);
        }
        // A finished session falls through to cmd_start, which reclaims a
        // workspace+tag held by a terminal-phase, worker-dead session. A
        // "broken" one (non-terminal phase, dead worker) keeps its claim
        // there too -- it needs an explicit `a kill` first.
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
            startup_timeout_ms: 10_000,
            no_skip_permissions: false,
            command,
        },
        false,
    )
}

fn cmd_quick_attach(paths: &Paths, args: QuickAttachArgs) -> Result<()> {
    let record = resolve_quick_index(paths, args.workspace_index, args.session.as_deref())?;
    attach(paths, &record, None)
}

fn cmd_prune(paths: &Paths, json_output: bool) -> Result<()> {
    let records = list_records(paths)?;
    let mut removed = Vec::new();
    let mut retained_count = 0usize;
    for record in records {
        let workload_alive = record.workload_pid.map(process_alive).unwrap_or(false);
        if workload_alive || !record.worker_finished() || !record.containment_proven_empty() {
            retained_count += 1;
            continue;
        }
        remove_session_state(paths, record.id)
            .with_context(|| format!("remove stale session {}", record.id))?;
        removed.push(record.id);
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": removed,
                "retained_count": retained_count,
            }))?
        );
    } else if removed.is_empty() {
        println!("no dead sessions to prune");
    } else {
        for id in &removed {
            println!("removed {id}");
        }
        println!("removed {} session(s)", removed.len());
    }
    Ok(())
}

fn cmd_forget(paths: &Paths, args: ForgetArgs, json_output: bool) -> Result<()> {
    if !args.force {
        bail!("forget requires --force");
    }
    let selected = resolve(paths, &args.target)?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    // Resolve happened before taking the registry lock. Re-read under the
    // lock so a concurrent rename or lifecycle update cannot make a stale
    // liveness decision destructive.
    let current = read_record(&paths.record(selected.id))
        .with_context(|| format!("re-read session {} before forgetting", selected.id))?;
    if current.worker_alive() {
        bail!(
            "session {} still has a live worker; refusing to forget it",
            current.id
        );
    }
    let _startup_absence_lock = if current.worker_phase_active() && current.worker_pid.is_none() {
        let lock_path = paths.worker_lock(current.id);
        // This lock is also a fence against the spawn-to-worker-lock gap. If
        // a worker was spawned but has not reached its first required lock
        // yet, it will fail that acquisition and cannot proceed after we
        // remove the record. Keep our lock through both directory removals.
        match FileLock::exclusive(&lock_path, true) {
            Ok(lock) => Some(lock),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .and_then(io::Error::raw_os_error)
                    .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK) =>
            {
                bail!(
                    "session {} still has a worker holding {}; refusing to forget it",
                    current.id,
                    lock_path.display()
                )
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot fence session {}'s pre-PID worker; refusing to forget it",
                        current.id
                    )
                })
            }
        }
    } else {
        None
    };

    let containment_proven_empty = current.containment_proven_empty();
    match fs::remove_dir_all(paths.runtime_session(current.id)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove forgotten session runtime state"),
    }
    fs::remove_dir_all(paths.state_session(current.id))
        .with_context(|| format!("remove forgotten session {} durable state", current.id))?;

    let workload_may_survive = !containment_proven_empty;
    if workload_may_survive {
        eprintln!(
            "a: forgot session {} without signalling any process; containment was not proven empty, so workload processes may survive",
            current.id
        );
    } else {
        eprintln!(
            "a: forgot session {} without signalling any process (containment was proven empty)",
            current.id
        );
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": current.id,
                "forgotten": true,
                "signalled": false,
                "containment_proven_empty": containment_proven_empty,
                "workload_may_survive": workload_may_survive,
            }))?
        );
    } else {
        println!("forgotten {}", current.id);
    }
    Ok(())
}

/// Shared by the bare `a <N>` shortcut and by `resolve()` (so `a attach 1`,
/// `a status 1`, `a kill 1`, etc. all understand the same numbers `a list`
/// prints, not just the no-subcommand form).
fn resolve_quick_index(
    paths: &Paths,
    workspace_index: usize,
    session: Option<&str>,
) -> Result<SessionRecord> {
    let groups = group_by_workspace(list_records(paths)?);
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
        value["worker_reachable"] = json!(worker_reachable);
        if let Some(error) = &rpc_error {
            value["rpc_error"] = json!(error);
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("id: {}", current.id);
        println!("selector: {}", current.selector());
        println!("state: {}", display_state(&current.phase, worker_alive));
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
                match read_history_tail(&record.history_path, args.bytes) {
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
        println!(
            "{}",
            json!({"id":record.id,"bytes":data.len(),"utf8":String::from_utf8_lossy(&data)})
        );
    } else {
        io::stdout().write_all(&data)?;
    }
    Ok(())
}

/// Read only the tail that can fit in one protocol frame. Persisted history
/// may come from an older configuration with a much larger cap, so loading
/// the whole file before slicing would let a dead-session capture exhaust the
/// CLI process even though the equivalent live RPC is frame-bounded.
fn read_history_tail(path: &Path, requested: Option<usize>) -> Result<Vec<u8>> {
    let limit = requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let length = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?
        .len();
    let count = length.min(limit as u64);
    file.seek(SeekFrom::Start(length - count))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut bytes = Vec::with_capacity(count as usize);
    file.take(count)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read tail of {}", path.display()))?;
    Ok(bytes)
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
        // Only a missing socket file proves an alive worker is unreachable.
        // A mere RPC failure can be transient, so it still returns below.
        let socket_missing = worker_alive && !record.socket_path.exists();
        if worker_alive && !socket_missing {
            return Err(error);
        }
        if socket_missing {
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
        } else {
            if !record.containment_proven_empty() {
                recover_broken_containment(&record, signal, args.grace_ms)?;
            }
            remove_session_state(paths, record.id)
                .with_context(|| format!("remove finished session {}", record.id))?;
            eprintln!(
                "a: removed {} session {}",
                phase_name(&record.phase),
                record.id
            );
        }
    }
    if json_output {
        println!("{}", json!({"id":record.id,"signal":signal}));
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
        println!("state: {}", phase_name(&record.phase));
    }
    Ok(())
}

fn cmd_doctor(paths: &Paths, json_output: bool) -> Result<()> {
    let mut checks = Vec::<Value>::new();
    checks.push(json!({"name":"linux","ok":true,"detail":std::env::consts::OS}));
    checks.push(path_check("runtime_root", &paths.runtime_root));
    checks.push(path_check("state_root", &paths.state_root));
    let sample = paths.socket(Uuid::nil());
    checks.push(json!({"name":"unix_socket_path","ok":sample.as_os_str().len()<108,"detail":sample.display().to_string()}));
    let cgv2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    checks.push(json!({"name":"cgroup_v2","ok":cgv2,"detail":if cgv2{"mounted"}else{"not mounted; unlimited sessions still work"}}));
    match Config::load(paths){Ok(config)=>checks.push(json!({"name":"config","ok":true,"detail":format!("{} engines, {} profiles",config.engines.len(),config.profiles.len())})),Err(e)=>checks.push(json!({"name":"config","ok":false,"detail":format!("{e:#}")}))}
    match list_records(paths) {
        Ok(records) => {
            let record_count = records.len();
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
                    Some(json!({
                        "id": record.id,
                        "selector": record.selector(),
                        "phase": phase_name(&record.phase),
                        "worker_alive": worker_alive,
                        "worker_reachable": worker_reachable,
                        "rpc_error": rpc_error,
                        "recovery": {
                            "kill": format!("a kill {}", record.id),
                            "forget": format!("a forget {} --force", record.id),
                        },
                    }))
                })
                .collect();
            let detail = if broken.is_empty() {
                format!("{record_count} session record(s), none broken")
            } else {
                format!(
                    "{} broken/stale session(s); run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
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
    let ok = checks.iter().all(|v| v["ok"].as_bool().unwrap_or(false));
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok":ok,"checks":checks}))?
        );
    } else {
        for check in &checks {
            println!(
                "{:<5} {:<20} {}",
                if check["ok"].as_bool().unwrap() {
                    "OK"
                } else {
                    "FAIL"
                },
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

/// `a hotkeys` -- a lookup command for the attach-mode Ctrl-b chords, kept in
/// sync by hand with the startup banner (`"[aplexer attached; ...]"`, printed
/// where `attach()` enters raw mode) and the `SwitchTarget` match arms in the
/// attach loop's byte scanner -- there is one authoritative keymap, this just
/// prints it somewhere you can look it up without already being attached.
fn cmd_hotkeys() -> Result<()> {
    println!("Attach-mode hotkeys (press Ctrl-b, then one of these):");
    println!();
    println!("  n / p    next / previous session in this workspace");
    println!("  N / P    next / previous session, across all workspaces");
    println!("  Ctrl-b 1..9 jumps to that number from the status bar");
    println!("  1-9      jump to that number after Ctrl-b");
    println!("  l        toggle back to the last session you were on");
    println!("  d        detach");
    println!();
    println!("Any other key after Ctrl-b is forwarded through untouched.");
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
fn phase_name(phase: &Phase) -> &'static str {
    match phase {
        Phase::Starting => "starting",
        Phase::Running => "running",
        Phase::Exiting => "exiting",
        Phase::Exited => "exited",
        Phase::Failed => "failed",
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
/// it -- observed in practice as a race during worker startup under heavy
/// concurrent load. The worker process is technically still running, but
/// it's unreachable, so treating it as attachable would just trade the
/// clear checks above for the same bare `UnixStream::connect` OS error this
/// function exists to avoid. `a kill` again has a real action to take here
/// (see cmd_kill's socket-missing force-clean path), so it's not exempted
/// from this check the way the other two cases exempt it -- `a kill` relies
/// on `rpc_simple` failing and inspects the socket itself rather than going
/// through `check_attachable`.
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
    if !worker_alive {
        bail!(
            "session {}'s worker is not running (state: {}); run `a status` for details, `a kill` to reclaim it",
            record.id,
            display_state(&record.phase, worker_alive)
        );
    }
    if !record.socket_path.exists() {
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
    let framed = if raw {
        body.as_bytes().to_vec()
    } else {
        let sender = from_tag.unwrap_or("external");
        format!("[aplexer message from {sender}] {body}\r").into_bytes()
    };
    rpc_send(&record, &framed).with_context(|| format!("inject into session {tag:?}'s PTY"))
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
    let request = Request::new(operation);
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
    let request = Request::new(Operation::Capture { max_bytes: max });
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
    let request = Request::new(Operation::CaptureScreen { plain });
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

/// aplexer has no real terminal emulation (spec.md's v1 non-goal), so the
/// status-bar thread has no way to know what the workload's screen actually
/// looks like right now or whether interjecting a redraw would visually
/// collide with something the workload just drew -- unlike tmux, which can
/// place its status line safely because it tracks real per-pane terminal
/// state server-side. The save-cursor/jump/draw/restore-cursor sequence in
/// `draw_status_bar` is byte-safe (serialized under the shared stdout
/// mutex, so writes never tear or interleave), but a fast-redrawing
/// full-screen TUI (htop's ~1-2s full-screen cycle) can still visibly
/// reflect our redraw firing mid-cycle, or have its own cursor-position
/// bookkeeping thrown off by our jump-away-and-back. Building real terminal
/// state tracking to eliminate this completely is a much bigger project
/// than this fix -- so instead of a fixed independent timer, the status
/// thread redraws when the PTY has been quiet for `STATUS_BAR_IDLE_GAP`
/// (tending to land in the gaps between a TUI's own redraws rather than
/// racing them on an unrelated clock), falling back to a forced redraw
/// every `STATUS_BAR_MAX_INTERVAL` for workloads that stream continuously
/// (agent CLIs during generation, a chatty build) and so never go quiet.
const STATUS_BAR_IDLE_GAP: Duration = Duration::from_millis(450);
const STATUS_BAR_MAX_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_BAR_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
fn write_locked(stdout: &Arc<Mutex<io::Stdout>>, bytes: &[u8]) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.write_all(bytes)?;
    out.flush()
}

/// Sets (or, for a too-small terminal, clears) the DECSTBM scrolling region
/// and records the resulting geometry for the status-bar thread. Wrapped in
/// DEC save/restore cursor (`\x1b7`/`\x1b8`) because DECSTBM itself moves the
/// cursor to the region's home position as a side effect on real terminals;
/// saving immediately before and restoring immediately after -- with nothing
/// else written in between -- keeps that jump invisible and leaves the
/// shell's own cursor position undisturbed.
fn apply_terminal_layout(
    stdout: &Arc<Mutex<io::Stdout>>,
    term: &Arc<Mutex<TermGeom>>,
    rows: u16,
    cols: u16,
) {
    let reserved = rows > 2;
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b7");
    if reserved {
        seq.extend_from_slice(format!("\x1b[1;{}r", rows - 1).as_bytes());
    } else {
        seq.extend_from_slice(b"\x1b[r");
    }
    seq.extend_from_slice(b"\x1b8");
    let _ = write_locked(stdout, &seq);
    if let Ok(mut g) = term.lock() {
        *g = TermGeom {
            rows,
            cols,
            reserved,
        };
    }
}

/// Feeds PTY bytes on their way to the terminal through the client's copy of
/// the worker's `MarginTracker`, so `draw_status_bar` knows which scroll
/// region the workload currently holds -- see `StatusBarCtx::workload_margins`
/// for why the bar must not blindly re-assert its own.
///
/// A byte-level state machine with no allocation on the common path, run on
/// the same bytes that are about to be `write`n anyway; the worker already
/// pays the identical cost per chunk (docs/terminal-state-design.md section
/// 9's steady-state parse budget), and this is strictly cheaper than that
/// since it recognizes two sequences rather than emulating a terminal.
fn scan_workload_margins(margins: &Arc<Mutex<aplexer::screen::MarginTracker>>, data: &[u8]) {
    if let Ok(mut m) = margins.lock() {
        m.scan(data);
    }
}

/// Forgets the tracked workload scroll region on an in-process session
/// switch: the new session's margins are its own, and carrying the previous
/// session's over would apply that region to it (observed directly --
/// switching away from a session holding `\x1b[5;15r` left the bar
/// re-asserting `5;15` on the session switched *to*).
///
/// Note this must be `reset()`, not `set_rows()`: `set_rows` clamps the
/// region to a row count rather than clearing it, so on a switch that did
/// not also change the terminal size it is a no-op.
fn reset_workload_margins(
    margins: &Arc<Mutex<aplexer::screen::MarginTracker>>,
    term: &Arc<Mutex<TermGeom>>,
) {
    let rows = term.lock().map(|g| g.rows).unwrap_or(0);
    if let Ok(mut m) = margins.lock() {
        m.reset();
        if rows > 0 {
            m.set_rows(reserved_rows(rows));
        }
    }
}

/// Undoes `apply_terminal_layout` and clears the screen, exactly like tmux
/// does on detach (Ctrl-b d) -- otherwise whatever was last drawn (including
/// the status bar) just sits in the user's terminal after attach() returns.
/// `\x1b[2J\x1b[H` (full clear + cursor home) is used rather than a fuller
/// reset (`\x1bc`) because it doesn't disturb terminal scrollback history.
const TERMINAL_RESET_SEQUENCE: &[u8] = b"\
\x1b[?1049l\
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

fn reset_terminal(stdout: &Arc<Mutex<io::Stdout>>) {
    // `\x1b[?1049l` first (docs/terminal-state-design.md section 6.3): if
    // the session was on the alternate screen -- whether entered by a
    // reattach snapshot (section 6.2 step 1) or by live workload output
    // while attached -- detaching must return the *host* terminal to its
    // primary screen, otherwise the user's real terminal is left stuck on
    // the alt screen after detach. A no-op on a host already on the primary
    // screen. This does not conflict with docs/scrollback-design.md section
    // 4.1's "no alt-screen for aplexer's own UI" rule: aplexer still never
    // *enters* the alt screen for itself; this only ever *exits* one that
    // the workload's own live behavior put the host into.
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
/// forget the cleanup. Only constructed when stdin is a tty (mirrors
/// `RawMode`, which it's dropped alongside).
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
            let state = display_state(&r.phase, r.worker_alive());
            let mut part = format!("{}:{}", i + 1, r.tag);
            if r.id == record.id {
                part.push('*');
            }
            if state != "running" {
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
    /// The *workload's* current DECSTBM scroll region, recovered by running
    /// the same `MarginTracker` the worker uses over every PTY byte this
    /// client writes to the terminal (including the attach snapshot, which
    /// re-emits the region per docs/terminal-state-design.md section 6.2
    /// step 3). `None` means the workload is on full-screen margins, the
    /// common case.
    ///
    /// This exists so `draw_status_bar`'s defensive margin re-assert can
    /// re-assert *the right region*. Without it the bar unconditionally
    /// rewrote `\x1b[1;{rows-1}r`, which silently destroyed a workload's own
    /// sub-range within one redraw cycle -- including the one the attach
    /// snapshot had just restored, defeating section 6.3 step 3's "the
    /// workload's sub-range lands after and wins".
    workload_margins: Arc<Mutex<aplexer::screen::MarginTracker>>,
}

type LastDrawnStatus = Option<(String, u16, u16, Option<(u16, u16)>)>;

/// How long a switch-failure message stays on the status bar before the
/// normal text resumes (docs/fast-session-switching-design.md section 6.1).
const FLASH_DURATION: Duration = Duration::from_secs(2);

/// Compact, agent-first status-bar line: workspace, tag, engine/profile,
/// live memory (if the session has a cgroup), and the current workspace's
/// numbered session list -- the aplexer analogue of tmux's window list,
/// adapted for a per-session/per-tag model rather than tmux's single-server
/// window set. Renders a flashed error instead of the normal text while one
/// is active (section 6.1).
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
    if let Some(fg) = raw
        .as_ref()
        .and_then(|raw| foreground_override(&record, raw))
    {
        ep.push_str(&format!(" -> {fg}"));
    }
    let mut text = format!("{ws}:{} [{ep}]", record.tag);
    if let Some(mem) = raw.as_ref().and_then(|raw| memory_indicator(&record, raw)) {
        text.push_str(&format!("  mem {mem}"));
    }
    let siblings = workspace_summary(ctx, &record);
    if !siblings.is_empty() {
        text.push_str("  |  ");
        text.push_str(&siblings);
    }
    pad_or_truncate(&sanitize_terminal_text(&text), cols)
}

/// Redraws the reserved bottom row in place: save cursor, jump to the last
/// row, clear it, draw the (reverse-video, full-width) status line, restore
/// cursor -- so a redraw racing a keystroke never disturbs the shell's own
/// cursor position. No-ops when the current terminal is too small to have a
/// reserved row.
///
/// Two deliberate additions beyond the original version:
///
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
///   redraw cycle instead of being lost permanently. Cheap (a handful of
///   extra bytes) and wrapped in the same save/restore-cursor pair so it
///   can't disturb the workload's own cursor position.
///
///   What gets reasserted is `ctx.workload_margins`-aware, and that
///   distinction is load-bearing rather than cosmetic. Reasserting
///   `apply_terminal_layout`'s own `\x1b[1;{rows-1}r` *unconditionally*
///   clobbers a workload that set its own DECSTBM sub-range: the host
///   terminal then stops scrolling the workload's region, so the workload's
///   line feeds at the bottom of its region walk the cursor past it and
///   overwrite whatever the workload placed below instead. Reproduced
///   directly -- a workload holding `\x1b[5;15r` and scrolling inside it
///   rendered `SCROLLER-70M-ROW-16` on the host, a mangled overlay of its
///   scrolling text on the fixed row 16 the worker's own screen model
///   correctly still showed as `FIXED-BOTTOM-ROW-16`. It also silently
///   undid the sub-range the attach snapshot had just restored
///   (docs/terminal-state-design.md section 6.2 step 3), on the very first
///   bar redraw after attaching.
///
/// `force`: bypass the dirty-check and write unconditionally. The
/// dirty-check alone would let a *clobbered margin* go unrepaired
/// indefinitely during a long idle stretch where the bar's *text* never
/// changes (nothing to detect); callers that need the margin-defense
/// guarantee to actually bound in time -- the status thread's own
/// `STATUS_BAR_MAX_INTERVAL` forced tick, and every switch/flash redraw,
/// which are already low-frequency, user-triggered events where bandwidth
/// isn't the concern -- pass `true`.
///
/// Returns whether a real write to the terminal happened (`false` when the
/// reserved row doesn't exist, or the dirty-check skipped an unchanged
/// redraw). Callers that drive `STATUS_BAR_MAX_INTERVAL`'s overdue timer
/// must only reset it on `true` -- resetting on a dirty-check no-op would
/// let a workload with frequent-but-unchanging redraws (a spinner, streamed
/// tokens with pauses) keep the timer perpetually "recently fired" without
/// ever actually rewriting a margin a full-screen erase clobbered, breaking
/// the self-heal guarantee this constant exists for.
fn draw_status_bar(ctx: &StatusBarCtx, force: bool) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    if !geom.reserved {
        return false;
    }
    let workload_margins = ctx
        .workload_margins
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .margins();
    let text = status_bar_text(ctx, geom.cols as usize);
    {
        let mut last = ctx
            .last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (text.clone(), geom.rows, geom.cols, workload_margins);
        if !force && last.as_ref() == Some(&key) {
            return false;
        }
        *last = Some(key);
    }
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b7");
    // Re-assert whichever scroll region should currently be in force: the
    // workload's own sub-range when it has one, otherwise the bar's
    // `1;rows-1` reservation.
    //
    // Known limitation, deliberately accepted (see
    // `workload_line_feed_can_still_reach_the_reserved_row_under_a_sub_range`
    // and docs/terminal-state-design.md section 7): while a workload
    // sub-range is in force, the reserved row is *not* protected the way
    // `1;rows-1` protects it. DECSTBM constrains scrolling inside the region,
    // not cursor motion outside it, so a workload whose cursor sits on its
    // own last row (`rows-1` -- outside its sub-range, since its PTY is one
    // row shorter than the terminal) and emits a line feed walks onto the
    // reserved row and writes there, because for the host terminal that row
    // is just the screen bottom rather than a margin boundary. The bar text
    // is repainted on the next redraw, but the workload's cursor is left one
    // row lower than its own screen model believes.
    //
    // Not re-asserting the sub-range is not the alternative: that was the
    // bug this replaced, and it corrupts every frame a margin-using TUI
    // draws rather than one row on an uncommon cursor walk. Closing the gap
    // properly means the client emulating the workload's stream well enough
    // to clamp its cursor motion -- a different design (the client passes
    // PTY bytes straight through today), tracked as a follow-up rather than
    // papered over here.
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
    seq.extend_from_slice(b"\x1b[0m\x1b8");
    let _ = write_locked(&ctx.stdout, &seq);
    true
}

/// Which session a `Ctrl-b` switch chord asks for
/// (docs/fast-session-switching-design.md section 3).
#[derive(Clone, Copy, Debug, PartialEq)]
enum SwitchTarget {
    /// `Ctrl-b n`: next session in the current workspace.
    Next,
    /// `Ctrl-b p`: previous session in the current workspace.
    Prev,
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

/// Pure candidate selection over the same groups `a list` prints (see
/// `group_by_workspace`). Split from `resolve_switch_target` (the
/// paths-touching wrapper) so it is unit-testable without a filesystem.
/// Semantics are docs/fast-session-switching-design.md section 3.2:
///
/// - `Next`/`Prev`: candidates are the current session's own workspace
///   group; skips dead sessions; wraps; errors if nothing else is
///   attachable there.
/// - `NextGlobal`/`PrevGlobal`: candidates are every group flattened in
///   group order (alphabetical workspace, then list order) -- exactly the
///   top-to-bottom order of `a list`'s tree.
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
    let groups = group_by_workspace(list_records(paths)?);
    pick_switch_target(&groups, &current.workspace, current.id, target, last)
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
    let request = Request::new(Operation::Attach {
        history_bytes: replay_bytes,
        want_screen,
        rows,
        cols,
    });
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
        let state = display_state(&r.phase, r.worker_alive());
        text.push_str(&format!("{}:{}", i + 1, sanitize_terminal_text(&r.tag)));
        if r.id == current_id {
            text.push('*');
        }
        if state != "running" {
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
    /// `Ctrl-b n/p/N/P/l/1-9`.
    Switch(SwitchTarget),
}

/// Byte-scanning state for the `Ctrl-b` prefix state machine, split out of
/// the input thread body so the split-across-`read()` cases are
/// unit-testable independent of any real socket/thread (see the `#[cfg(test)]`
/// module below). `pending_ctrl_b` has to survive across `scan()` calls, not
/// just within one buffer: `Ctrl-b` can legitimately arrive as the very
/// last byte of one `read()` and the following key as the first byte of the
/// next.
#[derive(Default)]
struct InputScanner {
    pending_ctrl_b: bool,
}

impl InputScanner {
    /// Scan rules (docs/fast-session-switching-design.md section 5.1):
    /// `Ctrl-b d` detaches; `n p N P l 1-9`
    /// are consumed after a pending `Ctrl-b`. Anything else pending is "not a
    /// real prefix" -- the withheld `Ctrl-b` byte is forwarded and the
    /// current byte is reprocessed normally, so unbound `Ctrl-b` sequences
    /// still pass through to the workload untouched.
    fn scan(&mut self, buffer: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < buffer.len() {
            let byte = buffer[i];
            if self.pending_ctrl_b {
                self.pending_ctrl_b = false;
                let switch = match byte {
                    b'd' => {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::Detach);
                        return actions;
                    }
                    b'n' => Some(SwitchTarget::Next),
                    b'p' => Some(SwitchTarget::Prev),
                    b'N' => Some(SwitchTarget::NextGlobal),
                    b'P' => Some(SwitchTarget::PrevGlobal),
                    b'l' => Some(SwitchTarget::Last),
                    b'1'..=b'9' => Some(SwitchTarget::Index((byte - b'0') as usize)),
                    _ => None,
                };
                if let Some(target) = switch {
                    if !out.is_empty() {
                        actions.push(InputAction::Forward(std::mem::take(&mut out)));
                    }
                    actions.push(InputAction::Switch(target));
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
    let next = resolve_switch_target(paths, &current, target, last)?;
    if next.id == current.id {
        return Ok(()); // switching to yourself: silent no-op
    }
    check_attachable(&next)?;
    switch_in_progress.store(true, Ordering::Relaxed);
    // Current terminal geometry (docs/fast-session-switching-design.md
    // section 5.2 / docs/terminal-state-design.md section 10.2): passed
    // into the Attach so the new session's snapshot renders at the right
    // size immediately, rather than relying solely on the post-switch
    // explicit Resize send below.
    let geometry = term
        .lock()
        .ok()
        .map(|g| *g)
        .filter(|g| g.rows > 0)
        .map(|g| (reserved_rows(g.rows), g.cols));
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

fn attach(paths: &Paths, record: &SessionRecord, history_bytes: Option<usize>) -> Result<()> {
    check_attachable(record)?;
    let explicit_history = history_bytes.is_some();
    let replay_bytes = Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES));
    let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    // Geometry read up front (docs/terminal-state-design.md section 6.3
    // step 1), not after the handshake: sent in the Attach request itself
    // so the worker can resize the PTY and its screen model *before*
    // rendering the snapshot below -- no wrong-size frame followed by a
    // SIGWINCH repaint. `--history-bytes` is the explicit escape hatch back
    // to the old raw-tail semantics (section 6.1); `want_screen` follows
    // its absence.
    let initial_geometry = if tty {
        terminal_size(libc::STDIN_FILENO)
    } else {
        None
    };
    let handshake = establish(
        record,
        replay_bytes,
        !explicit_history,
        initial_geometry.map(|(rows, cols)| (reserved_rows(rows), cols)),
    )?;
    let mut reader = handshake.reader;
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let _raw = if tty {
        Some(RawMode::enter(libc::STDIN_FILENO)?)
    } else {
        None
    };
    // Constructed only for a tty, and dropped (LIFO, so before `_raw`
    // restores cooked termios) on every exit from this point on -- see
    // `TerminalUiGuard`'s doc comment for why this is the one cleanup path.
    let _ui_guard = if tty {
        Some(TerminalUiGuard {
            stdout: stdout.clone(),
        })
    } else {
        None
    };
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let active = Arc::new(AtomicBool::new(true));
    let mut signal_bridge = if tty {
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
    // Sized to the *workload's* row count (the terminal minus the reserved
    // bar row), which is what a DECSTBM the workload emits is validated
    // against -- see `StatusBarCtx::workload_margins`.
    let workload_margins = Arc::new(Mutex::new(aplexer::screen::MarginTracker::new(
        initial_geometry
            .map(|(rows, _)| reserved_rows(rows))
            .unwrap_or(24),
    )));
    let status_ctx = StatusBarCtx {
        stdout: stdout.clone(),
        term: term.clone(),
        paths: paths.clone(),
        record: shared_record.clone(),
        flash: Arc::new(Mutex::new(None)),
        last_drawn: Arc::new(Mutex::new(None)),
        workload_margins: workload_margins.clone(),
    };

    // Reservation asserted *first* (docs/terminal-state-design.md section
    // 6.3 step 3): a workload sub-range margin the snapshot itself
    // re-establishes below (numerically within rows 1..rows-1, since the
    // workload PTY is one row shorter) lands after and wins, while a
    // default-margin workload leaves this reservation standing.
    if tty {
        if let Some((rows, cols)) = initial_geometry {
            apply_terminal_layout(&stdout, &term, rows, cols);
        }
    }
    if tty {
        // No banner into the output stream (section 6.3 step 6 / section
        // 10.1 item c): printed here, before the snapshot write below, so
        // the snapshot's own ED2 clear repaints over it -- this ordering
        // (banner then snapshot, both after raw mode/layout are already in
        // effect) is exactly the fix for the original corruption, where the
        // banner landed inside a live TUI's input box. A real status-bar
        // flash slot for this is future work (section 6.3 step 6).
        eprintln!("[aplexer attached; Ctrl-b d detaches; Ctrl-b n/p/1-9/l switches]");
    }
    // Scanned before the bar is drawn: the snapshot re-emits the workload's
    // DECSTBM sub-range as its last bytes (design doc section 6.2 step 3), so
    // scanning it here is what lets the immediately-following `draw_status_bar`
    // re-assert that region instead of overwriting it with the bar's own.
    scan_workload_margins(&workload_margins, &handshake.initial);
    write_locked(&stdout, &handshake.initial)?;
    if tty {
        draw_status_bar(&status_ctx, true); // the snapshot's ED2 blanked the bar row
    }
    // The explicit post-connect Resize control send is unnecessary when the
    // Attach already carried geometry and a new-enough worker honored it
    // (the "screen" key is present in the response either way, true or
    // false); kept only for the old-worker fallback path, whose response
    // predates the field (section 6.3 step 7).
    if tty && handshake.screen.is_none() {
        if let Some((rows, cols)) = initial_geometry {
            send_control(
                &writer,
                &AttachControl::Resize {
                    rows: reserved_rows(rows),
                    cols,
                },
            )?;
        }
    }

    let input_writer = writer.clone();
    let input_active = active.clone();
    let input_paths = paths.clone();
    let input_term = term.clone();
    let input_shared_record = shared_record.clone();
    let input_last_session = last_session.clone();
    let input_pending_switch = pending_switch.clone();
    let input_switch_in_progress = switch_in_progress.clone();
    let input_status_ctx = status_ctx.clone();
    let input_want_screen = !explicit_history;
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buffer = [0u8; 8192];
        // Ctrl-b (0x02) prefix state machine -- Ctrl-b d detaches,
        // Ctrl-b n/p/N/P/l/1-9 switch sessions,
        // anything else pending is not a real prefix (both bytes forward to
        // the workload). See
        // `InputScanner` for the byte-level rules and why this needs to
        // survive across separate read() calls, not just within one
        // buffer.
        //
        // Design choice: real tmux turns Ctrl-b into a standing "prefix"
        // that consumes the next keystroke as a command (or no-ops/bells if
        // unrecognized), never forwarding Ctrl-b itself to the pane. aplexer
        // has no such command-prefix system and isn't growing one just for
        // this, so the simplest reasonable behavior is used instead: a
        // *bound* Ctrl-b sequence (d/n/p/N/P/l/1-9) is consumed; anything
        // else is not a prefix at all -- both bytes are forwarded through as
        // ordinary input, so a program that wants a literal Ctrl-b (some
        // editors and REPLs use it) isn't broken by this feature.
        let mut scanner = InputScanner::default();
        'outer: while input_active.load(Ordering::Relaxed) {
            let n = match input.read(&mut buffer) {
                Ok(0) => {
                    detach_attached_client(&input_writer, &input_active);
                    break;
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    detach_attached_client(&input_writer, &input_active);
                    break;
                }
            };
            if !tty {
                if send_data(&input_writer, &buffer[..n]).is_err() {
                    detach_attached_client(&input_writer, &input_active);
                    break;
                }
                continue;
            }
            for action in scanner.scan(&buffer[..n]) {
                match action {
                    InputAction::Forward(bytes) => {
                        // Ordering matters: a Forward before a Switch goes
                        // to the old session (keystrokes typed before the
                        // chord); a Forward after it goes to the new one,
                        // automatically, because perform_switch swapped the
                        // stream inside input_writer's mutex.
                        if send_data(&input_writer, &bytes).is_err() {
                            detach_attached_client(&input_writer, &input_active);
                            break 'outer;
                        }
                    }
                    InputAction::Detach => {
                        detach_attached_client(&input_writer, &input_active);
                        break 'outer;
                    }
                    InputAction::Switch(target) => {
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
                            if let Ok(mut flash) = input_status_ctx.flash.lock() {
                                *flash = Some((format!("{e:#}"), Instant::now()));
                            }
                            draw_status_bar(&input_status_ctx, true);
                        }
                    }
                }
            }
        }
    });
    if tty {
        let resize_writer = writer.clone();
        let resize_active = active.clone();
        let resize_stdout = stdout.clone();
        let resize_term = term.clone();
        let resize_margins = workload_margins.clone();
        let resize_initial = initial_geometry;
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
                let size = terminal_size(libc::STDIN_FILENO);
                if size != last {
                    if let Some((rows, cols)) = size {
                        // Keep the client's tracker in step with the
                        // worker-side model across the same resize: both
                        // re-clamp the region to the new row count rather
                        // than dropping it (see `MarginTracker::set_rows`
                        // and design doc section 5.3's correction).
                        if let Ok(mut m) = resize_margins.lock() {
                            m.set_rows(reserved_rows(rows));
                        }
                        apply_terminal_layout(&resize_stdout, &resize_term, rows, cols);
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
                                rows: reserved_rows(rows),
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
                let idle_for = activity.elapsed();
                let overdue = last_draw.elapsed() >= STATUS_BAR_MAX_INTERVAL;
                if (idle_for >= STATUS_BAR_IDLE_GAP && !drawn_for_current_idle) || overdue {
                    // `overdue` forces the write even if the text is
                    // unchanged -- see draw_status_bar's doc comment on why
                    // the margin-defense guarantee needs that. Only reset
                    // `last_draw` when a real write happened (`wrote`):
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
                    scan_workload_margins(&workload_margins, &frame.payload);
                    write_locked(&stdout, &frame.payload)?;
                    if let Ok(mut t) = last_activity.lock() {
                        *t = Instant::now();
                    }
                }
                FrameKind::End => break,
                FrameKind::Json => {
                    let event: ServerEvent = serde_json::from_slice(&frame.payload)?;
                    match event {
                        ServerEvent::Exit { .. } => break,
                        ServerEvent::Error { message } => {
                            eprintln!("[aplexer: {message}]");
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
                            draw_status_bar(&status_ctx, true);
                        }
                    }
                }
            }
        }
        // The frame loop broke: either the session ended/we detached, or
        // the input thread killed the old stream to hand us a switch.
        let outcome = take_pending_switch(&pending_switch, &switch_in_progress);
        let Some(outcome) = outcome else { break };

        *shared_record.lock().unwrap_or_else(PoisonError::into_inner) = outcome.record;
        reader = outcome.reader; // old stream dropped (closed) here

        // Light-variant reset: clear screen + home + show cursor, but keep
        // the DECSTBM scroll region and raw mode -- they are terminal
        // state, not session state. ?25h because A's TUI may have hidden
        // the cursor and B never knows to show it; same rationale as
        // reset_terminal's.
        let mut seq: Vec<u8> = b"\x1b[2J\x1b[H\x1b[?25h".to_vec();
        seq.extend_from_slice(&outcome.history);
        // The new session's margins are its own: drop whatever the previous
        // one had, then learn the new one's from its snapshot payload.
        reset_workload_margins(&workload_margins, &term);
        scan_workload_margins(&workload_margins, &outcome.history);
        let _ = write_locked(&stdout, &seq);
        if let Ok(mut t) = last_activity.lock() {
            *t = Instant::now();
        }

        // B's PTY may still be sized for its previous client (or the
        // 24x80 default). The resize thread won't resend an unchanged
        // terminal size (its `last` cache), so push the current geometry
        // explicitly.
        let geom = term.lock().map(|g| *g).unwrap_or(TermGeom {
            rows: 0,
            cols: 0,
            reserved: false,
        });
        if geom.rows > 0 {
            let _ = send_control(
                &writer,
                &AttachControl::Resize {
                    rows: reserved_rows(geom.rows),
                    cols: geom.cols,
                },
            );
        }
        draw_status_bar(&status_ctx, true); // clear wiped the reserved row; redraw now
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
    Ok(())
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod switching_tests {
    use super::*;

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
        file.seek(SeekFrom::End(-4)).unwrap();
        file.write_all(b"tail").unwrap();
        drop(file);

        assert_eq!(read_history_tail(&path, Some(4)).unwrap(), b"tail");

        let bounded = read_history_tail(&path, Some(usize::MAX)).unwrap();
        assert_eq!(bounded.len(), MAX_FRAME_BYTES);
        assert_eq!(&bounded[bounded.len() - 4..], b"tail");
    }

    #[test]
    fn parse_hex_rejects_non_ascii_without_panicking() {
        assert!(parse_hex("aéa".as_bytes()).is_err());
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
    fn scan_ctrl_b_n_is_switch_next() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[0x02, b'n']);
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Switch(SwitchTarget::Next)]
        ));
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
        let actions = s.scan(b"n");
        assert!(matches!(
            actions.as_slice(),
            [InputAction::Switch(SwitchTarget::Next)]
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
            phase,
            worker_pid: Some(std::process::id()), // our own pid: always "alive"
            workload_pid: None,
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
            workload_margins: Arc::new(Mutex::new(aplexer::screen::MarginTracker::new(23))),
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
    /// `StatusBarCtx::workload_margins`: the bar's defensive scroll-region
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
        scan_workload_margins(&ctx.workload_margins, b"\x1b[5;15r");
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
        scan_workload_margins(&ctx.workload_margins, b"\x1b[r");
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
        scan_workload_margins(&ctx.workload_margins, b"\x1b[5;15r");
        assert!(
            draw_status_bar(&ctx, false),
            "a workload margin change must defeat the dirty-check even when the text is unchanged"
        );
    }

    /// Characterization test for the documented limitation on
    /// `draw_status_bar`'s scroll-region re-assert: re-asserting a workload's
    /// own DECSTBM sub-range does **not** protect the client's reserved
    /// bottom row, because DECSTBM constrains scrolling *inside* the region,
    /// not cursor motion outside it.
    ///
    /// Both halves are measured against a real `vt100::Parser` standing in
    /// for the host terminal, at the client's own geometry (24 physical rows,
    /// row 24 reserved, the workload told it has 23):
    ///
    /// - under the bar's own `1;23`, a line feed on row 23 scrolls rows 1-23
    ///   and the cursor stays on row 23 -- row 24 is untouched;
    /// - under a workload sub-range (`5;15`), the same line feed walks the
    ///   cursor onto row 24 and writes there, because for the host terminal
    ///   that row is the screen bottom rather than a margin boundary.
    ///
    /// This is pinned rather than fixed (see the comment in
    /// `draw_status_bar`): the alternative -- not re-asserting the workload's
    /// region -- is the strictly worse bug this replaced. If the client ever
    /// grows enough emulation to clamp the workload's cursor, this test is
    /// where that shows up, and the comments it points at have to change with
    /// it.
    #[test]
    fn workload_line_feed_can_still_reach_the_reserved_row_under_a_sub_range() {
        // The workload parks on its own last row -- host row 23, which is
        // *outside* a 5;15 sub-range -- and line-feeds, as anything printing
        // a trailing newline at the bottom of its own screen does.
        let walk = b"\x1b[23;1HWORKLOAD-LAST-ROW\nWALKED";

        let mut protected = vt100::Parser::new(24, 80, 0);
        protected.process(b"\x1b[24;1HBAR-TEXT\x1b[1;23r");
        protected.process(walk);
        assert_eq!(
            protected.screen().cursor_position().0 + 1,
            23,
            "under the bar's own reservation the cursor must stay on row 23"
        );
        assert_eq!(
            protected.screen().contents().lines().nth(23),
            Some("BAR-TEXT"),
            "under the bar's own reservation row 24 must be untouched"
        );

        let mut exposed = vt100::Parser::new(24, 80, 0);
        exposed.process(b"\x1b[24;1HBAR-TEXT\x1b[5;15r");
        exposed.process(walk);
        assert_eq!(
            exposed.screen().cursor_position().0 + 1,
            24,
            "known limitation: a workload sub-range lets a line feed on row 23 reach row 24"
        );
        assert!(
            exposed
                .screen()
                .contents()
                .lines()
                .nth(23)
                .is_some_and(|row| row.contains("WALKED")),
            "known limitation: the reserved row is written over, not protected; row 24 was {:?}",
            exposed.screen().contents().lines().nth(23)
        );
    }

    #[test]
    fn draw_status_bar_not_reserved_never_writes() {
        let ctx = status_ctx_for_test(false);
        assert!(!draw_status_bar(&ctx, false));
        assert!(!draw_status_bar(&ctx, true));
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
