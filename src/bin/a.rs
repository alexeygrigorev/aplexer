use anyhow::{anyhow, bail, Context, Result};
use aplexer::messaging::*;
use aplexer::*;
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
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
    Start(StartArgs),
    #[command(alias = "ls")]
    List(ListArgs),
    Snapshot(ListArgs),
    Attach(AttachArgs),
    Send(SendArgs),
    Capture(CaptureArgs),
    Status(TargetArgs),
    Kill(KillArgs),
    Rename(RenameArgs),
    Engines,
    Profiles,
    LaunchSpec(LaunchArgs),
    LaunchExec(LaunchArgs),
    Doctor,
    Whoami,
    Message(MessageArgs),
    Watch(WatchArgs),
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
    #[arg(long, help = "Inject as terminal input into the target's PTY instead of the durable inbox")]
    pane: bool,
    #[arg(long = "or-inbox", help = "If --pane delivery fails, fall back to an inbox send instead of erroring")]
    or_inbox: bool,
    #[arg(long, help = "With --pane: suppress the '[aplexer message from ...]' frame and trailing return")]
    raw: bool,
}

#[derive(Args)]
struct MessageSendArgs {
    #[arg(long, value_name = "TAG", help = "Send to one session, addressed by tag")]
    to: Option<String>,
    #[arg(long, help = "Broadcast to every other session in the workspace")]
    all: bool,
    #[arg(long = "to-engine", value_name = "ENGINE", help = "Broadcast to sessions of one engine")]
    to_engine: Option<String>,
    #[arg(long, help = "Allow sending to a tag that has never existed in this workspace")]
    queue: bool,
    #[arg(long, default_value = "note", help = "note (default) | handoff | reply | any string")]
    kind: String,
    #[arg(long, value_name = "JSON", help = "Opaque structured payload")]
    data: Option<String>,
    #[command(flatten)]
    pane_delivery: PaneDeliveryArgs,
    #[arg(long, value_name = "TAG", help = "Sender identity override (default: APLEXER_TAG or anonymous)")]
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
    #[arg(long, help = "Unread messages only (this is also the default with no flag)")]
    new: bool,
    #[arg(long, value_name = "TAG", help = "Consumer identity override (default: APLEXER_SESSION_ID)")]
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
    #[arg(long, help = "Ack every currently-unread message addressed to this consumer")]
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
    let args = rewrite_quick_attach_args(std::env::args().collect());
    let cli = Cli::parse_from(args);
    let paths = Paths::discover()?;
    // Bare `a` with no subcommand defaults to `a list`, matching how tmux
    // and similar tools default to a listing rather than printing usage.
    let command = cli.command.unwrap_or(Commands::List(ListArgs { running: false }));
    match command {
        Commands::Start(args) => cmd_start(&paths, args, cli.json),
        Commands::List(args) | Commands::Snapshot(args) => cmd_list(&paths, args, cli.json),
        Commands::Attach(args) => {
            let record = resolve(&paths, &args.target)?;
            attach(&paths, &record, args.history_bytes)
        }
        Commands::Send(args) => cmd_send(&paths, args, cli.json),
        Commands::Capture(args) => cmd_capture(&paths, args, cli.json),
        Commands::Status(target) => cmd_status(&paths, target, cli.json),
        Commands::Kill(args) => cmd_kill(&paths, args, cli.json),
        Commands::Rename(args) => cmd_rename(&paths, args, cli.json),
        Commands::Engines => cmd_engines(&paths, cli.json),
        Commands::Profiles => cmd_profiles(&paths, cli.json),
        Commands::LaunchSpec(args) => cmd_launch_spec(&paths, args, cli.json),
        Commands::LaunchExec(args) => cmd_launch_exec(&paths, args),
        Commands::Whoami => cmd_whoami(&paths, cli.json),
        Commands::Doctor => cmd_doctor(&paths, cli.json),
        Commands::Message(args) => cmd_message(&paths, args, cli.json),
        Commands::Watch(args) => cmd_watch(&paths, args),
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
            let is_quick_index =
                !selector.is_empty() && selector.len() < 8 && selector.bytes().all(|b| b.is_ascii_digit());
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
    validate_tag(&args.tag)?;
    let workspace = canonical_workspace(&args.workspace)?;
    let env = parse_env(&args.env)?;
    let limits = Limits {
        memory_bytes: args.memory.as_deref().map(parse_byte_size).transpose()?,
        pids: args.pids,
        cpu_quota_us: args.cpu_quota_us,
        cpu_period_us: args.cpu_quota_us.map(|_| args.cpu_period_us),
    };
    let direct = args
        .command
        .iter()
        .map(|v| os_to_utf8(v, "command argument"))
        .collect::<Result<Vec<_>>>()?;
    let config = Config::load(paths)?;
    let launch = config.resolve(
        direct,
        args.engine.as_deref(),
        args.profile.as_deref(),
        &workspace,
        args.cwd.as_deref(),
        &env,
        &limits,
        args.history_bytes,
    )?;
    if !command_exists(&launch.command) {
        bail!(
            "command is not executable or was not found in PATH: {}",
            launch
                .command
                .first()
                .map(String::as_str)
                .unwrap_or("<empty>")
        );
    }
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    if let Some(existing) = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == args.tag)
    {
        // A session that reached a terminal phase and whose worker is gone
        // is finished: starting anew on the same workspace+tag supersedes
        // it (there is no other way to reclaim the tag -- the v1 CLI has no
        // remove command). A live session, or a "broken" one (non-terminal
        // phase, dead worker) whose workload may still be running, keeps
        // its claim and start still refuses.
        let worker_alive = existing.worker_pid.map(process_alive).unwrap_or(false);
        let finished =
            matches!(existing.phase, Phase::Exited | Phase::Failed) && !worker_alive;
        if !finished {
            bail!(
                "workspace+tag already belongs to session {}; rename it or choose a different tag",
                existing.id
            );
        }
        eprintln!(
            "a: superseding {} session {}",
            phase_name(&existing.phase),
            existing.id
        );
        fs::remove_dir_all(paths.state_session(existing.id))
            .with_context(|| format!("remove superseded session {}", existing.id))?;
        let _ = fs::remove_dir_all(paths.runtime_session(existing.id));
    }
    let id = Uuid::new_v4();
    ensure_private_dir(&paths.state_session(id))?;
    ensure_private_dir(&paths.runtime_session(id))?;
    let now = now_ms();
    let record = SessionRecord {
        schema_version: SCHEMA_VERSION,
        id,
        workspace: workspace.clone(),
        tag: args.tag,
        engine: launch.engine,
        profile: launch.profile,
        command: launch.command,
        cwd: canonical_workspace(&launch.cwd).unwrap_or(launch.cwd),
        env: launch.env,
        env_unset: launch.env_unset,
        limits: launch.limits,
        history_bytes: launch.history_bytes,
        created_at_ms: now,
        updated_at_ms: now,
        last_activity_ms: None,
        phase: Phase::Starting,
        worker_pid: None,
        workload_pid: None,
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    atomic_write_json(&paths.record(id), &record)?;
    let worker = worker_executable()?;
    let worker_log = File::create(paths.state_session(id).join("worker.log"))
        .context("create worker log")?;
    let mut command = Command::new(&worker);
    command
        .arg("worker")
        .arg("--id")
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(worker_log));
    if args.attach {
        // We're about to attach right after the worker comes up (`a start
        // --attach` / `a -`), so this client's terminal size is already
        // known -- pass it through so the worker opens the workload's PTY
        // at its real, final (reserved-row-adjusted) size from the start,
        // instead of the 24x80 default that would otherwise be corrected a
        // moment later by attach()'s own resize. See `run_worker`'s
        // `initial_size` doc comment in src/worker.rs for the race this
        // closes. A non-tty stdin (piped/scripted use) has no size to
        // offer, so the worker falls back to its own default in that case,
        // same as a detached start.
        let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if let Some((rows, cols)) = tty.then(|| terminal_size(libc::STDIN_FILENO)).flatten() {
            command
                .arg("--rows")
                .arg(reserved_rows(rows).to_string())
                .arg("--cols")
                .arg(cols.to_string());
        }
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn worker {}", worker.display()))?;
    let deadline = Instant::now() + Duration::from_millis(args.startup_timeout_ms);
    let ready = loop {
        if let Ok(current) = read_record(&paths.record(id)) {
            match current.phase {
                Phase::Running | Phase::Exiting | Phase::Exited if current.socket_path.exists() => {
                    break current
                }
                Phase::Failed => bail!(
                    "worker startup failed: {}",
                    current.error.unwrap_or_else(|| "unknown error".into())
                ),
                _ => {}
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!("worker exited during startup: {status}");
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            bail!(
                "worker did not become ready within {} ms",
                args.startup_timeout_ms
            );
        }
        thread::sleep(Duration::from_millis(25));
    };
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
    let mut records = list_records(paths)?;
    if args.running {
        records.retain(|r| {
            matches!(r.phase, Phase::Starting | Phase::Running | Phase::Exiting)
                && r.worker_pid.map(process_alive).unwrap_or(false)
        });
    }
    if json_output {
        // list/snapshot must stay cheap (spec.md 30: "milliseconds on tens
        // of sessions"), so liveness here is the pid-existence check only
        // (process_alive), never a socket round-trip per session -- that's
        // what `a status` is for.
        let enriched: Vec<Value> = records
            .iter()
            .map(|r| {
                let mut value = serde_json::to_value(r).unwrap_or(Value::Null);
                value["worker_alive"] = json!(r.worker_pid.map(process_alive).unwrap_or(false));
                value
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
        return Ok(());
    }
    // Group by workspace as a compact tree -- spec.md's own presentation of
    // the model (sections 2 and 22.1) is a workspace tree with tags
    // underneath, not a flat table repeating the workspace on every row.
    // `a <N>` quick-attach (see cmd_quick_attach) numbers workspaces and
    // sessions using this exact same grouping, so what a user sees here is
    // what those numbers mean.
    let by_workspace = group_by_workspace(records);
    let home = env::var_os("HOME").map(PathBuf::from);
    for (workspace, group) in &by_workspace {
        println!(
            "{} ({})",
            display_workspace(workspace, home.as_deref()),
            running_summary(group)
        );
        let last = group.len().saturating_sub(1);
        for (i, r) in group.iter().enumerate() {
            let connector = if i == last { "\u{2514}\u{2500}\u{2500}" } else { "\u{251c}\u{2500}\u{2500}" };
            let ep = match &r.profile {
                Some(p) => format!("{}/{}", r.engine, p),
                None => r.engine.clone(),
            };
            let alive = r.worker_pid.map(process_alive).unwrap_or(false);
            println!(
                "{connector} {:<14} {:<16} {}",
                r.tag,
                ep,
                display_state(&r.phase, alive),
            );
        }
    }
    Ok(())
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

fn running_summary(group: &[SessionRecord]) -> String {
    let running = group
        .iter()
        .filter(|r| {
            let alive = r.worker_pid.map(process_alive).unwrap_or(false);
            display_state(&r.phase, alive) == "running"
        })
        .count();
    let total = group.len();
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
        let alive = existing.worker_pid.map(process_alive).unwrap_or(false);
        let terminal = matches!(existing.phase, Phase::Exited | Phase::Failed);
        if alive && !terminal {
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
            command,
        },
        false,
    )
}

fn cmd_quick_attach(paths: &Paths, args: QuickAttachArgs) -> Result<()> {
    let record = resolve_quick_index(paths, args.workspace_index, args.session.as_deref())?;
    attach(paths, &record, None)
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
    let rpc_result = rpc_simple(&record, Operation::Status, None);
    // A successful round-trip to the worker's own socket is stronger
    // evidence of liveness than a pid-existence check (e.g. it also rules
    // out a hung/unresponsive worker); fall back to the pid check only if
    // the RPC itself failed, so `status` on a single session is worth the
    // extra round-trip that `list` deliberately skips.
    let rpc_reachable = rpc_result.is_ok();
    let raw = rpc_result.unwrap_or_else(|_| serde_json::to_value(&record).unwrap_or(Value::Null));
    let current: SessionRecord = serde_json::from_value(raw.clone()).unwrap_or(record);
    let cgroup_stats = raw.get("cgroup").cloned();
    let worker_alive =
        rpc_reachable || current.worker_pid.map(process_alive).unwrap_or(false);
    if json_output {
        let mut value = serde_json::to_value(&current)?;
        if let Some(stats) = cgroup_stats {
            value["cgroup"] = stats;
        }
        value["worker_alive"] = json!(worker_alive);
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("id: {}", current.id);
        println!("selector: {}", current.selector());
        println!("state: {}", display_state(&current.phase, worker_alive));
        println!(
            "worker_pid: {}",
            current
                .worker_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
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
    let data = match rpc_capture(&record, args.bytes) {
        Ok(data) => data,
        // No live worker to ask -- capture still has a sensible fallback
        // here (unlike attach/send), since a dead session's last-known
        // output is still sitting in its persisted history file. Only when
        // that ALSO comes up empty (e.g. the session never produced any
        // output before exiting) is there truly nothing to show; in that
        // case, give the same clear reason attach/send would rather than
        // the raw filesystem error from the failed read.
        Err(_) => match fs::read(&record.history_path) {
            Ok(bytes) => {
                let n = args.bytes.unwrap_or(bytes.len()).min(bytes.len());
                bytes[bytes.len() - n..].to_vec()
            }
            Err(read_error) => {
                check_attachable(&record)?;
                return Err(read_error)
                    .context("worker unavailable and persisted history cannot be read");
            }
        },
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

fn cmd_kill(paths: &Paths, args: KillArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let signal = parse_signal(&args.signal)?;
    let rpc = rpc_simple(
        &record,
        Operation::Kill {
            signal,
            grace_ms: args.grace_ms,
        },
        None,
    );
    if let Err(error) = rpc {
        let worker_alive = record.worker_pid.map(process_alive).unwrap_or(false);
        if worker_alive {
            return Err(error);
        }
        // The worker exits once its workload is gone, so a terminal-phase
        // session has no socket to talk to; killing something already dead
        // is success, not an error. A *broken* session (non-terminal phase,
        // dead worker) is the harder case: its workload may have survived
        // the worker (anything ignoring SIGHUP does), and the dead worker
        // can neither kill it nor record its exit -- without this fallback
        // such a workload is unkillable through the CLI and the session
        // unreclaimable forever. The client is the only actor left, so it
        // signals the workload's process group itself and retires the
        // record.
        if !matches!(record.phase, Phase::Exited | Phase::Failed) {
            kill_broken_workload(&record, signal, args.grace_ms)?;
            let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
            let mut current = read_record(&paths.record(record.id)).unwrap_or(record.clone());
            current.phase = Phase::Failed;
            current.error = Some(
                "worker died without recording workload exit; workload killed by `a kill`"
                    .into(),
            );
            current.updated_at_ms = now_ms();
            atomic_write_json(&paths.record(record.id), &current)?;
            let _ = fs::remove_dir_all(paths.runtime_session(record.id));
        } else {
            // Already terminal with a dead worker: there's nothing left to
            // signal. `a kill` is the only command a user would think to
            // reach for to clean up a finished session, so treat this as
            // "remove it" rather than a silent no-op -- otherwise it lingers
            // in `a list` forever with no way to get rid of it. Take the
            // registry lock the same way `cmd_start`'s superseding logic
            // does, to avoid racing a concurrent `a start` that might be
            // reclaiming the same workspace+tag at the same moment.
            let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
            fs::remove_dir_all(paths.state_session(record.id))
                .with_context(|| format!("remove finished session {}", record.id))?;
            let _ = fs::remove_dir_all(paths.runtime_session(record.id));
            eprintln!("a: removed {} session {}", phase_name(&record.phase), record.id);
        }
    }
    if json_output {
        println!("{}", json!({"id":record.id,"signal":signal}));
    }
    Ok(())
}

/// Signals a broken session's surviving workload process group directly.
/// The recorded workload pid could in principle have been recycled since
/// the worker died, so before signalling anything the pid's identity is
/// verified against the APLEXER_SESSION_ID environment variable the worker
/// stamped into the workload at spawn; on any mismatch this fails closed.
fn kill_broken_workload(record: &SessionRecord, signal: i32, grace_ms: u64) -> Result<()> {
    let Some(pid) = record.workload_pid else {
        return Ok(());
    };
    if !process_alive(pid) {
        return Ok(());
    }
    if !workload_identity_matches(pid, record.id) {
        bail!(
            "session {} is broken (worker dead) and pid {} no longer looks like its workload; \
             refusing to signal it",
            record.id,
            pid
        );
    }
    let pgid = pid as i32;
    if unsafe { libc::kill(-pgid, signal) } != 0 {
        return Err(io::Error::last_os_error()).context("signal workload process group");
    }
    if signal != libc::SIGKILL {
        let deadline = Instant::now() + Duration::from_millis(grace_ms);
        while process_alive(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if process_alive(pid) && workload_identity_matches(pid, record.id) {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    Ok(())
}

fn workload_identity_matches(pid: u32, id: Uuid) -> bool {
    let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let needle = format!("APLEXER_SESSION_ID={id}");
    environ
        .split(|b| *b == 0)
        .any(|entry| entry == needle.as_bytes())
}

fn cmd_rename(paths: &Paths, args: RenameArgs, json_output: bool) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let old = resolve_record(paths, Some(&args.selector), None, None)?;
    let workspace = canonical_workspace(args.workspace.as_deref().unwrap_or(&old.workspace))?;
    let tag = args.tag.unwrap_or_else(|| old.tag.clone());
    validate_tag(&tag)?;
    if let Some(conflict) = list_records(paths)?
        .into_iter()
        .find(|r| r.id != old.id && r.workspace == workspace && r.tag == tag)
    {
        bail!("workspace+tag already belongs to session {}", conflict.id);
    }
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
    let config = Config::load(paths)?;
    // Shape stays lean (name/command/available) rather than growing to match
    // pocketshell's fuller EngineManifest (label/family/provider_mark are
    // presentation concerns aplexer has no reason to own) -- the one
    // addition is env_unset, exposed as the actual resolved list (not just
    // a count) now that 0.2 makes it meaningful
    // (pocketshell-integration-plan.md 0.5).
    let values = config
        .engines
        .iter()
        .map(|(name, e)| {
            let env_unset = e.resolved_env_unset();
            json!({
                "name": name,
                "command": e.command,
                "available": command_exists(&e.command),
                "env_unset_count": env_unset.len(),
                "env_unset": env_unset,
            })
        })
        .collect::<Vec<_>>();
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
    let config = Config::load(paths)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&config.profiles)?);
    } else if config.profiles.is_empty() {
        println!("no configured profiles");
    } else {
        for (name, p) in config.profiles {
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
    let Some(id_text) = env::var_os("APLEXER_SESSION_ID").and_then(|v| v.into_string().ok())
    else {
        // Deliberately silent on stdout either way -- a script doing
        // `id=$(a whoami --json)` should see empty output and rely on the
        // exit code, not have to filter out a "not in a session" sentence.
        if !json_output {
            eprintln!("not inside an aplexer session");
        }
        std::process::exit(1);
    };
    let id: Uuid = id_text
        .parse()
        .with_context(|| format!("APLEXER_SESSION_ID {id_text:?} is not a valid UUID"))?;
    let record = read_record(&paths.record(id))
        .with_context(|| format!("session {id} (from APLEXER_SESSION_ID) has no persisted record"))?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&record)?);
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
/// handles the broken case itself via `kill_broken_workload`.
fn check_attachable(record: &SessionRecord) -> Result<()> {
    if matches!(record.phase, Phase::Exited | Phase::Failed) {
        bail!(
            "session {} has already exited (see `a status {}` for details); run `a kill {}` to remove it",
            record.id,
            record.id,
            record.id
        );
    }
    let worker_alive = record.worker_pid.map(process_alive).unwrap_or(false);
    if !worker_alive {
        bail!(
            "session {}'s worker is not running (state: {}); run `a status` for details, `a kill` to reclaim it",
            record.id,
            display_state(&record.phase, worker_alive)
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
fn deliver_pane(paths: &Paths, workspace: &Path, tag: &str, from_tag: Option<&str>, body: &str, raw: bool) -> Result<()> {
    if body.as_bytes().len() > MAX_BODY_BYTES {
        bail!("message body exceeds the {MAX_BODY_BYTES}-byte cap");
    }
    let record = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == tag)
        .ok_or_else(|| anyhow!("no session tagged {tag:?} in this workspace"))?;
    let alive = record.worker_pid.map(process_alive).unwrap_or(false);
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
        match deliver_pane(paths, workspace, &tag, envelope.from.tag.as_deref(), &envelope.body, pane.raw) {
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
    write_message(paths, &envelope)?;
    if envelope.delivery == Delivery::Pane {
        if let Recipient::Tag { session_id: Some(sid), .. } = &envelope.to {
            // Best-effort: a pane message is already delivered by
            // definition, so a failure to also pre-ack it here is a
            // cosmetic mailbox-record issue, not a delivery failure
            // (design doc section 6.2).
            let _ = ack_messages(paths, workspace, *sid, &[envelope.id]);
        }
    }
    let _ = maybe_gc(paths, workspace);
    Ok(envelope)
}

fn print_message_line(m: &MessageEnvelope) {
    let sender = m
        .from
        .tag
        .clone()
        .unwrap_or_else(|| if m.from.external { "external".into() } else { "?".into() });
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
    println!("{}  [{}] {sender} -> {to_desc}{delivery}  {first_line}", m.id, m.kind);
}

fn print_message_details(m: &MessageEnvelope) {
    println!("id: {}", m.id);
    println!("workspace: {}", m.workspace.display());
    println!("created_at: {}", m.created_at);
    let sender = m
        .from
        .tag
        .clone()
        .unwrap_or_else(|| if m.from.external { "(external)".into() } else { "(unknown)".into() });
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
    let to = build_recipient(paths, &workspace, args.to.as_deref(), args.all, args.to_engine.as_deref(), args.queue)?;
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
    let (consumer_id, consumer_tag, consumer_engine) = resolve_consumer(paths, &workspace, args.from.as_deref())?;
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
    let (consumer_id, consumer_tag, consumer_engine) = resolve_consumer(paths, &workspace, args.from.as_deref())?;
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
        println!("removed {} message(s), {} remaining", report.removed, report.remaining);
    }
    Ok(())
}

fn connect(record: &SessionRecord) -> Result<UnixStream> {
    UnixStream::connect(&record.socket_path)
        .with_context(|| format!("connect {}", record.socket_path.display()))
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
fn apply_terminal_layout(stdout: &Arc<Mutex<io::Stdout>>, term: &Arc<Mutex<TermGeom>>, rows: u16, cols: u16) {
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
        *g = TermGeom { rows, cols, reserved };
    }
}

/// Undoes `apply_terminal_layout` and clears the screen, exactly like tmux
/// does on detach (Ctrl-b d) -- otherwise whatever was last drawn (including
/// the status bar) just sits in the user's terminal after attach() returns.
/// `\x1b[2J\x1b[H` (full clear + cursor home) is used rather than a fuller
/// reset (`\x1bc`) because it doesn't disturb terminal scrollback history.
fn reset_terminal(stdout: &Arc<Mutex<io::Stdout>>) {
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
    let _ = write_locked(stdout, b"\x1b[r\x1b[2J\x1b[H\x1b[?25h");
}

/// RAII guard that runs `reset_terminal` on every exit path out of attach()
/// -- explicit Ctrl-] detach, the remote session exiting, a connection
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

/// Live memory indicator from the session's cgroup, if it has one -- a
/// small "useful for our application" touch given aplexer's whole reason
/// for existing is resource-isolated agent sessions. Best-effort: any RPC
/// failure (worker briefly unreachable, no cgroup configured) just omits
/// the indicator rather than disrupting the status bar.
fn memory_indicator(record: &SessionRecord) -> Option<String> {
    let raw = rpc_simple(record, Operation::Status, None).ok()?;
    let current = raw.get("cgroup")?.get("memory_current")?.as_u64()?;
    let used = format_bytes(current);
    Some(match record.limits.memory_bytes {
        Some(max) => format!("{used}/{}", format_bytes(max)),
        None => used,
    })
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
            let alive = r.worker_pid.map(process_alive).unwrap_or(false);
            let state = display_state(&r.phase, alive);
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

/// Pads or truncates (by character, not byte, so multi-byte UTF-8 in a tag
/// name can't split mid-codepoint) to exactly `cols` wide, so the reverse-
/// video status bar spans the full terminal width like tmux's own.
fn pad_or_truncate(text: &str, cols: usize) -> String {
    let cols = cols.max(1);
    let len = text.chars().count();
    if len >= cols {
        text.chars().take(cols).collect()
    } else {
        let mut s = text.to_string();
        s.push_str(&" ".repeat(cols - len));
        s
    }
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
    /// (text, rows, cols) last actually written, so an unchanged bar isn't
    /// rewritten every debounce tick -- see `draw_status_bar`'s doc comment
    /// and docs/low-bandwidth-remote-access-design.md section 2.1.
    last_drawn: Arc<Mutex<Option<(String, u16, u16)>>>,
}

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
                return pad_or_truncate(&format!("[{msg}]"), cols);
            }
            *flash = None;
        }
    }
    let record = ctx.record.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let home = env::var_os("HOME").map(PathBuf::from);
    let ws = display_workspace(&record.workspace, home.as_deref());
    let ep = match &record.profile {
        Some(p) => format!("{}/{}", record.engine, p),
        None => record.engine.clone(),
    };
    let mut text = format!("{ws}:{} [{ep}]", record.tag);
    if let Some(mem) = memory_indicator(&record) {
        text.push_str(&format!("  mem {mem}"));
    }
    let siblings = workspace_summary(ctx, &record);
    if !siblings.is_empty() {
        text.push_str("  |  ");
        text.push_str(&siblings);
    }
    pad_or_truncate(&text, cols)
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
/// - **Defensively reasserts the DECSTBM margin** (`apply_terminal_layout`'s
///   own `\x1b[1;{rows-1}r`) every time it actually writes. A full-screen
///   TUI switching to the alternate screen buffer, or resetting margins
///   itself before laying out its own UI, can silently undo the reservation
///   outside our control; the resize-poll thread only reapplies it when the
///   physical terminal *size* changes, so a clobbered margin would
///   otherwise stay clobbered for the rest of the attach. Reasserting it
///   here means the reservation self-heals within one redraw cycle instead
///   of being lost permanently. Cheap (a handful of extra bytes) and
///   wrapped in the same save/restore-cursor pair so it can't disturb the
///   workload's own cursor position.
///
/// `force`: bypass the dirty-check and write unconditionally. The
/// dirty-check alone would let a *clobbered margin* go unrepaired
/// indefinitely during a long idle stretch where the bar's *text* never
/// changes (nothing to detect); callers that need the margin-defense
/// guarantee to actually bound in time -- the status thread's own
/// `STATUS_BAR_MAX_INTERVAL` forced tick, and every switch/flash redraw,
/// which are already low-frequency, user-triggered events where bandwidth
/// isn't the concern -- pass `true`.
fn draw_status_bar(ctx: &StatusBarCtx, force: bool) {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    if !geom.reserved {
        return;
    }
    let text = status_bar_text(ctx, geom.cols as usize);
    {
        let mut last = ctx.last_drawn.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (text.clone(), geom.rows, geom.cols);
        if !force && last.as_ref() == Some(&key) {
            return;
        }
        *last = Some(key);
    }
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b7");
    seq.extend_from_slice(format!("\x1b[1;{}r", geom.rows - 1).as_bytes());
    seq.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
    seq.extend_from_slice(b"\x1b[2K\x1b[7m");
    seq.extend_from_slice(text.as_bytes());
    seq.extend_from_slice(b"\x1b[0m\x1b8");
    let _ = write_locked(&ctx.stdout, &seq);
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
    /// `Ctrl-b 1`..`9`: the Nth session (1-based) of the current workspace,
    /// no skipping -- must mean exactly what the status bar shows.
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

/// Extracted attach handshake (connect + `Operation::Attach` request +
/// response check + initial history frame), used by both the initial
/// attach and every in-process switch (docs/fast-session-switching-design.md
/// section 3.1).
fn establish(record: &SessionRecord, replay_bytes: Option<usize>) -> Result<(UnixStream, Vec<u8>)> {
    let mut reader = connect(record)?;
    let request = Request::new(Operation::Attach {
        history_bytes: replay_bytes,
    });
    let id = request.request_id.clone();
    write_json(&mut reader, &request)?;
    let response: Response =
        frame_json(read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing attach response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()?;
    let initial = read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing history frame"))?;
    if initial.kind != FrameKind::Data {
        bail!("expected history data");
    }
    Ok((reader, initial.payload))
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

/// What one `InputScanner::scan` call decided to do with a chunk of raw
/// stdin bytes; several may result from a single `read()` (e.g.
/// `"a\x02n"` -> `Forward([b'a'])`, `Switch(Next)`).
enum InputAction {
    /// Ordinary input for the currently attached session.
    Forward(Vec<u8>),
    /// `Ctrl-]` or `Ctrl-b d`.
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
    /// existing `Ctrl-]`/`Ctrl-b d` semantics preserved exactly; `n p N P l
    /// 1-9` newly consumed after a pending `Ctrl-b`; anything else pending
    /// is "not a real prefix" -- the withheld `Ctrl-b` byte is forwarded
    /// and the current byte is reprocessed normally, so unbound `Ctrl-b`
    /// sequences still pass through to the workload untouched.
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
                // reprocess this byte normally (it might itself be Ctrl-]
                // or a fresh Ctrl-b) -- do not advance `i`.
                out.push(0x02);
                continue;
            }
            if byte == 0x1d {
                if !out.is_empty() {
                    actions.push(InputAction::Forward(std::mem::take(&mut out)));
                }
                actions.push(InputAction::Detach);
                return actions;
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
fn perform_switch(
    paths: &Paths,
    target: SwitchTarget,
    replay_bytes: Option<usize>,
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
    let result = (|| -> Result<()> {
        let (reader, history) = establish(&next, replay_bytes)?;
        let writer_clone = reader.try_clone()?; // before mutating anything
        // Repoint every forwarding thread (input, resize) at B, then retire
        // A's stream. From this instant keystrokes land in B.
        let old = {
            let mut w = writer.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *w, writer_clone)
        };
        *pending_switch.lock().unwrap_or_else(PoisonError::into_inner) = Some(SwitchOutcome {
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
        if let Some(o) = pending.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Some(o);
        }
        if !in_progress.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn attach(paths: &Paths, record: &SessionRecord, history_bytes: Option<usize>) -> Result<()> {
    check_attachable(record)?;
    let replay_bytes = Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES));
    let (mut reader, initial_history) = establish(record, replay_bytes)?;
    let stdout = Arc::new(Mutex::new(io::stdout()));
    write_locked(&stdout, &initial_history)?;
    let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
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
    if tty {
        eprintln!("[aplexer attached; Ctrl-] or Ctrl-b d detaches; Ctrl-b n/p/1-9/l switches]");
    }
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let active = Arc::new(AtomicBool::new(true));
    let term = Arc::new(Mutex::new(TermGeom {
        rows: 0,
        cols: 0,
        reserved: false,
    }));
    // Last time PTY output was written to the real terminal -- read by the
    // status-bar thread to decide when it's a good moment to redraw (see
    // STATUS_BAR_IDLE_GAP's doc comment above).
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    if tty {
        if let Some((rows, cols)) = terminal_size(libc::STDIN_FILENO) {
            apply_terminal_layout(&stdout, &term, rows, cols);
            send_control(
                &writer,
                &AttachControl::Resize {
                    rows: reserved_rows(rows),
                    cols,
                },
            )?;
        }
    }

    // -- Fast in-process session switching state (survives across
    //    switches, unlike `reader`/`writer`'s inner stream/`record`; see
    //    docs/fast-session-switching-design.md sections 2-3) --
    let shared_record = Arc::new(Mutex::new(record.clone()));
    let pending_switch: Arc<Mutex<Option<SwitchOutcome>>> = Arc::new(Mutex::new(None));
    let switch_in_progress = Arc::new(AtomicBool::new(false));
    let last_session: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let switch_replay_bytes = Some(history_bytes.unwrap_or(SWITCH_REPLAY_BYTES));
    let status_ctx = StatusBarCtx {
        stdout: stdout.clone(),
        term: term.clone(),
        paths: paths.clone(),
        record: shared_record.clone(),
        flash: Arc::new(Mutex::new(None)),
        last_drawn: Arc::new(Mutex::new(None)),
    };

    let input_writer = writer.clone();
    let input_active = active.clone();
    let input_paths = paths.clone();
    let input_shared_record = shared_record.clone();
    let input_last_session = last_session.clone();
    let input_pending_switch = pending_switch.clone();
    let input_switch_in_progress = switch_in_progress.clone();
    let input_status_ctx = status_ctx.clone();
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buffer = [0u8; 8192];
        // Ctrl-b (0x02) prefix state machine -- Ctrl-] and Ctrl-b d detach,
        // Ctrl-b n/p/N/P/l/1-9 switch sessions, anything else pending is
        // not a real prefix (both bytes forward to the workload). See
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
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if !tty {
                if send_data(&input_writer, &buffer[..n]).is_err() {
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
                            break 'outer;
                        }
                    }
                    InputAction::Detach => {
                        let _ = send_control(&input_writer, &AttachControl::Detach);
                        input_active.store(false, Ordering::Relaxed);
                        break 'outer;
                    }
                    InputAction::Switch(target) => {
                        let result = perform_switch(
                            &input_paths,
                            target,
                            switch_replay_bytes,
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
        thread::spawn(move || {
            let mut last = None;
            while resize_active.load(Ordering::Relaxed) {
                let size = terminal_size(libc::STDIN_FILENO);
                if size != last {
                    if let Some((rows, cols)) = size {
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
                    // the margin-defense guarantee needs that.
                    draw_status_bar(&thread_status_ctx, overdue);
                    last_draw = Instant::now();
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
    Ok(())
}

#[cfg(test)]
mod switching_tests {
    use super::*;

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
    fn scan_split_across_reads() {
        let mut s = InputScanner::default();
        assert!(s.scan(&[0x02]).is_empty());
        let actions = s.scan(&[b'n']);
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
            InputAction::Forward(b) => assert_eq!(b, &[b'a']),
            _ => panic!("expected Forward"),
        }
        assert!(matches!(
            &actions[1],
            InputAction::Switch(SwitchTarget::Index(3))
        ));
        match &actions[2] {
            InputAction::Forward(b) => assert_eq!(b, &[b'z']),
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
    fn scan_ctrl_bracket_discards_rest() {
        let mut s = InputScanner::default();
        let actions = s.scan(&[b'a', 0x1d, b'b', b'c']);
        assert_eq!(actions.len(), 2);
        match &actions[0] {
            InputAction::Forward(b) => assert_eq!(b, &[b'a']),
            _ => panic!("expected Forward"),
        }
        assert!(matches!(&actions[1], InputAction::Detach));
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
            limits: Default::default(),
            history_bytes: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            phase,
            worker_pid: Some(std::process::id()), // our own pid: always "alive"
            workload_pid: None,
            socket_path: PathBuf::from("/nonexistent"),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
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

    #[test]
    fn next_prev_wrap_and_skip_dead() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let a2 = groups[0].1[1].id;
        let next = pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Next, None)
            .unwrap();
        assert_eq!(next.id, a2); // dead a3 skipped
        let prev = pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Prev, None)
            .unwrap();
        assert_eq!(prev.id, a2); // wraps backward past dead a3 too
    }

    #[test]
    fn index_returns_dead_session_without_skipping() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let dead = pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Index(3), None)
            .unwrap();
        assert_eq!(dead.phase, Phase::Exited);
    }

    #[test]
    fn index_out_of_range_errors() {
        let groups = sample_groups();
        let a1 = groups[0].1[0].id;
        let err = pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Index(9), None)
            .unwrap_err();
        assert!(err.to_string().contains("no session 9"));
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
        let found =
            pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Last, Some(a2))
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
}

fn send_data(writer: &Arc<Mutex<UnixStream>>, data: &[u8]) -> Result<()> {
    let mut stream = writer.lock().map_err(|_| anyhow!("socket lock poisoned"))?;
    write_frame(&mut *stream, FrameKind::Data, data)
}
fn send_control(writer: &Arc<Mutex<UnixStream>>, control: &AttachControl) -> Result<()> {
    let mut stream = writer.lock().map_err(|_| anyhow!("socket lock poisoned"))?;
    write_json(&mut *stream, control)
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
    Some((ws.ws_row.max(1), ws.ws_col.max(1)))
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
    if text.len() % 2 != 0 {
        bail!("hex input must contain an even number of digits");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(Into::into))
        .collect()
}
