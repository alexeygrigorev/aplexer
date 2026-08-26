use anyhow::{anyhow, bail, Context, Result};
use aplexer::*;
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    Doctor,
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
struct RenameArgs {
    #[arg(value_name = "SESSION")]
    selector: String,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
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
        Commands::Doctor => cmd_doctor(&paths, cli.json),
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
        limits: launch.limits,
        history_bytes: launch.history_bytes,
        created_at_ms: now,
        updated_at_ms: now,
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
        Err(_) => {
            let bytes = fs::read(&record.history_path)
                .context("worker unavailable and persisted history cannot be read")?;
            let n = args.bytes.unwrap_or(bytes.len()).min(bytes.len());
            bytes[bytes.len() - n..].to_vec()
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
    let values=config.engines.iter().map(|(name,e)|json!({"name":name,"command":e.command,"available":command_exists(&e.command)})).collect::<Vec<_>>();
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

/// `tag(state)` for every other session in the same workspace, mirroring how
/// `a list`'s tree groups sessions by workspace (see `group_by_workspace`) --
/// a live glance at what else is running here without detaching.
fn sibling_summary(paths: &Paths, record: &SessionRecord) -> String {
    let records = match list_records(paths) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let parts: Vec<String> = records
        .into_iter()
        .filter(|r| r.workspace == record.workspace && r.id != record.id)
        .map(|r| {
            let alive = r.worker_pid.map(process_alive).unwrap_or(false);
            format!("{}({})", r.tag, display_state(&r.phase, alive))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("siblings: {}", parts.join(" "))
    }
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

/// Compact, agent-first status-bar line: workspace, tag, engine/profile,
/// live memory (if the session has a cgroup), and sibling sessions in the
/// same workspace -- the aplexer analogue of tmux's window list, adapted for
/// a per-session/per-tag model rather than tmux's single-server window set.
fn status_bar_text(paths: &Paths, record: &SessionRecord, cols: usize) -> String {
    let home = env::var_os("HOME").map(PathBuf::from);
    let ws = display_workspace(&record.workspace, home.as_deref());
    let ep = match &record.profile {
        Some(p) => format!("{}/{}", record.engine, p),
        None => record.engine.clone(),
    };
    let mut text = format!("{ws}:{} [{ep}]", record.tag);
    if let Some(mem) = memory_indicator(record) {
        text.push_str(&format!("  mem {mem}"));
    }
    let siblings = sibling_summary(paths, record);
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
fn draw_status_bar(stdout: &Arc<Mutex<io::Stdout>>, term: &Arc<Mutex<TermGeom>>, paths: &Paths, record: &SessionRecord) {
    let geom = match term.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    if !geom.reserved {
        return;
    }
    let text = status_bar_text(paths, record, geom.cols as usize);
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b7");
    seq.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
    seq.extend_from_slice(b"\x1b[2K\x1b[7m");
    seq.extend_from_slice(text.as_bytes());
    seq.extend_from_slice(b"\x1b[0m\x1b8");
    let _ = write_locked(stdout, &seq);
}

fn attach(paths: &Paths, record: &SessionRecord, history_bytes: Option<usize>) -> Result<()> {
    let mut reader = connect(record)?;
    let replay_bytes = Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES));
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
    let stdout = Arc::new(Mutex::new(io::stdout()));
    write_locked(&stdout, &initial.payload)?;
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
        eprintln!("[aplexer attached; Ctrl-] or Ctrl-b d detaches]");
    }
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let active = Arc::new(AtomicBool::new(true));
    let term = Arc::new(Mutex::new(TermGeom {
        rows: 0,
        cols: 0,
        reserved: false,
    }));
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
    let input_writer = writer.clone();
    let input_active = active.clone();
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buffer = [0u8; 8192];
        // Ctrl-b (0x02) immediately followed by `d` (0x64) detaches, same as
        // Ctrl-] -- tmux's actual default prefix-key detach binding, added
        // so tmux muscle memory works here too. This state has to survive
        // across separate read() calls (not just within one buffer): Ctrl-b
        // can legitimately arrive as the very last byte of one read and `d`
        // as the first byte of the next.
        //
        // Design choice: real tmux turns Ctrl-b into a standing "prefix"
        // that consumes the next keystroke as a command (or no-ops/bells if
        // unrecognized), never forwarding Ctrl-b itself to the pane. aplexer
        // has no such command-prefix system and isn't growing one just for
        // this, so the simplest reasonable behavior is used instead: Ctrl-b
        // followed by `d` detaches; Ctrl-b followed by anything else is not
        // a prefix at all -- both bytes are forwarded through as ordinary
        // input, so a program that wants a literal Ctrl-b (some editors and
        // REPLs use it) isn't broken by this feature when the user doesn't
        // follow it with `d`.
        let mut pending_ctrl_b = false;
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
            let mut out = Vec::with_capacity(n);
            let mut i = 0;
            while i < n {
                let byte = buffer[i];
                if pending_ctrl_b {
                    pending_ctrl_b = false;
                    if byte == b'd' {
                        if !out.is_empty() {
                            let _ = send_data(&input_writer, &out);
                        }
                        let _ = send_control(&input_writer, &AttachControl::Detach);
                        input_active.store(false, Ordering::Relaxed);
                        break 'outer;
                    }
                    // Not a detach: forward the withheld Ctrl-b and then
                    // reprocess this byte normally (it might itself be
                    // Ctrl-] or a fresh Ctrl-b).
                    out.push(0x02);
                    continue;
                }
                if byte == 0x1d {
                    if !out.is_empty() {
                        let _ = send_data(&input_writer, &out);
                    }
                    let _ = send_control(&input_writer, &AttachControl::Detach);
                    input_active.store(false, Ordering::Relaxed);
                    break 'outer;
                }
                if byte == 0x02 {
                    pending_ctrl_b = true;
                    i += 1;
                    continue;
                }
                out.push(byte);
                i += 1;
            }
            if !out.is_empty() && send_data(&input_writer, &out).is_err() {
                break;
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
                        if send_control(
                            &resize_writer,
                            &AttachControl::Resize {
                                rows: reserved_rows(rows),
                                cols,
                            },
                        )
                        .is_err()
                        {
                            break;
                        }
                    }
                    last = size;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
        let status_stdout = stdout.clone();
        let status_term = term.clone();
        let status_active = active.clone();
        let status_paths = paths.clone();
        let status_record = record.clone();
        thread::spawn(move || {
            while status_active.load(Ordering::Relaxed) {
                draw_status_bar(&status_stdout, &status_term, &status_paths, &status_record);
                thread::sleep(Duration::from_millis(1500));
            }
        });
    }
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
    active.store(false, Ordering::Relaxed);
    if let Ok(stream) = writer.lock() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(())
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
