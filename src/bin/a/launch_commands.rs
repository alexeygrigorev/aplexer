use super::*;

/// Resolution result shared by `a launch-spec` and `a launch-exec` -- both
/// wrap the exact same `Config::resolve` that `a start` uses
/// (pocketshell-integration-plan.md 0.3/0.4); they differ only in what they
/// do with it (print JSON vs execvpe). Neither creates a session or spawns
/// a worker -- pure resolution/preview.

pub(crate) struct LaunchPreview {
    pub(crate) engine: String,
    pub(crate) profile: Option<String>,
    pub(crate) argv: Vec<String>,
    pub(crate) env_set: BTreeMap<String, String>,
    pub(crate) env_unset: Vec<String>,
    pub(crate) cwd: PathBuf,
}

pub(crate) fn build_launch_preview(paths: &Paths, args: &LaunchArgs) -> Result<LaunchPreview> {
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
pub(crate) fn cmd_launch_spec(paths: &Paths, args: LaunchArgs, json_output: bool) -> Result<()> {
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
pub(crate) fn cmd_launch_exec(paths: &Paths, args: LaunchArgs) -> Result<()> {
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
