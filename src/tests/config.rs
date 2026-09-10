//! Unit tests for engine/profile/shortcut configuration.

use super::*;

fn load_config_text(text: &str) -> Result<Config> {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    fs::write(&paths.config_file, text).unwrap();
    Config::load(&paths)
}

#[test]
fn zcodex_is_a_built_in_codex_variant() {
    let config = load_config_text("").unwrap();
    let zcodex = config
        .engines
        .get("zcodex")
        .expect("built-in zcodex engine");
    assert_eq!(
        zcodex.command,
        vec![
            "zcodex".to_string(),
            "-c".to_string(),
            "check_for_update_on_startup=false".to_string(),
        ]
    );
    assert_eq!(
        zcodex.skip_permissions_argv,
        vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
    );
    assert_eq!(engine_family("zcodex"), "codex");
    assert_eq!(engine_family("codex"), "codex");
    assert_eq!(engine_family("claude"), "claude");
}

/// `config_keep_exited` is a second reader of the same setting, chosen
/// so the worker's exit path does not depend on the whole config file
/// validating. It must agree with `Config::load` on every shape that
/// matters, or the escape hatch would silently mean different things to
/// `a` and to the worker that acts on it.
#[test]
fn config_keep_exited_matches_full_config_load() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };

    // No config file at all: the documented default.
    assert!(!config_keep_exited(&paths));

    for (text, expected) in [
        ("version = 1\n", false),
        ("version = 1\nkeep_exited = false\n", false),
        ("version = 1\nkeep_exited = true\n", true),
    ] {
        fs::write(&paths.config_file, text).unwrap();
        assert_eq!(
            Config::load(&paths).unwrap().keep_exited,
            expected,
            "Config::load disagreed for {text:?}"
        );
        assert_eq!(
            config_keep_exited(&paths),
            expected,
            "config_keep_exited disagreed for {text:?}"
        );
    }

    // An unrelated invalid entry fails `Config::load` outright. The
    // worker's reader must not treat that as "keep records": a typo in
    // an engine definition is not a retention decision.
    fs::write(
        &paths.config_file,
        "version = 1\nkeep_exited = true\ndefault_engine = \"nope\"\n",
    )
    .unwrap();
    assert!(Config::load(&paths).is_err());
    assert!(config_keep_exited(&paths));

    // Unparsable or unreadable config: default, never a panic.
    fs::write(&paths.config_file, "this is not toml {{{").unwrap();
    assert!(!config_keep_exited(&paths));

    // An unsupported version is refused by both readers: the field may
    // not mean the same thing there, so the worker falls back to the
    // default rather than trusting it.
    fs::write(&paths.config_file, "version = 2\nkeep_exited = true\n").unwrap();
    assert!(Config::load(&paths).is_err());
    assert!(!config_keep_exited(&paths));
}

/// A future-version file is reported by its version, not by whichever of
/// its fields the strict current schema happens to reject first.
#[test]
fn unsupported_config_version_is_reported_before_unknown_fields() {
    let message = format!(
        "{:#}",
        load_config_text("version = 2\nbrand_new_setting = true\n").unwrap_err()
    );
    assert!(
        message.contains("unsupported config version 2"),
        "{message}"
    );
    assert!(!message.contains("unknown field"), "{message}");
}

/// A config file that exists but cannot be read is an error, never the
/// built-in defaults. The old `exists()` gate was false on EACCES too, so
/// an unreadable file loaded silently as "no config". ENOTDIR (a regular
/// file where the parent directory should be) is the same failure class,
/// reproducible without root.
#[test]
fn unreadable_config_file_is_an_error_not_defaults() {
    let root = tempfile::tempdir().unwrap();
    let blocker = root.path().join("not-a-dir");
    fs::write(&blocker, "").unwrap();
    let mut paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: blocker.join("config.toml"),
    };
    let message = format!("{:#}", Config::load(&paths).unwrap_err());
    assert!(message.contains("read"), "{message}");
    assert!(message.contains("config.toml"), "{message}");

    // A genuinely absent file still means defaults.
    paths.config_file = root.path().join("missing.toml");
    assert!(Config::load(&paths).is_ok());
}

#[test]
fn config_rejects_oversized_profile_history() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    fs::write(
        &paths.config_file,
        format!(
            "version = 1\n[profiles.too_large]\nhistory_bytes = {}\n",
            MAX_HISTORY_BYTES + 1
        ),
    )
    .unwrap();

    let error = Config::load(&paths).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("profile \"too_large\" history_bytes"),
        "{message}"
    );
    assert!(
        message.contains(&MAX_HISTORY_BYTES.to_string()),
        "{message}"
    );
}

#[test]
fn config_schema_rejects_unknown_fields_at_every_level() {
    for (label, text, unknown) in [
        (
            "root",
            "version = 1\ndefualt_engine = \"shell\"\n",
            "defualt_engine",
        ),
        (
            "engine",
            "version = 1\n[engines.custom]\ncommand = [\"true\"]\ncomand = [\"false\"]\n",
            "comand",
        ),
        (
            "profile",
            "version = 1\n[profiles.review]\nhistroy_bytes = 1024\n",
            "histroy_bytes",
        ),
        (
            "profile limits",
            "version = 1\n[profiles.review.limits]\nmemroy_bytes = 1024\n",
            "memroy_bytes",
        ),
        (
            "shortcut",
            "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofiel = \"review\"\n",
            "profiel",
        ),
    ] {
        let message = format!("{:#}", load_config_text(text).unwrap_err());
        assert!(message.contains("unknown field"), "{label}: {message}");
        assert!(message.contains(unknown), "{label}: {message}");
    }
}

#[test]
fn config_semantics_reject_dangling_references_and_invalid_commands() {
    for (label, text, expected) in [
        (
            "default engine",
            "version = 1\ndefault_engine = \"missing\"\n",
            "default_engine \"missing\"",
        ),
        (
            "default profile",
            "version = 1\ndefault_profile = \"missing\"\n",
            "default_profile \"missing\"",
        ),
        (
            "profile engine",
            "version = 1\n[profiles.review]\nengine = \"missing\"\n",
            "profile \"review\" engine \"missing\"",
        ),
        (
            "shortcut engine",
            "version = 1\n[shortcuts.review]\nengine = \"missing\"\n",
            "shortcut \"review\" engine \"missing\"",
        ),
        (
            "shortcut profile",
            "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofile = \"missing\"\n",
            "shortcut \"review\" profile \"missing\"",
        ),
        (
            "shortcut profile engine",
            "version = 1\n[profiles.review]\nengine = \"claude\"\n[shortcuts.review]\nengine = \"codex\"\nprofile = \"review\"\n",
            "selects engine \"codex\", but profile \"review\" selects engine \"claude\"",
        ),
        (
            "empty engine command",
            "version = 1\n[engines.shell]\ncommand = []\n",
            "engine \"shell\" command must not be empty",
        ),
        (
            "empty profile command",
            "version = 1\n[profiles.review]\ncommand = []\n",
            "profile \"review\" command must not be empty",
        ),
        (
            "empty profile executable",
            "version = 1\n[profiles.review]\nexecutable = \"\"\n",
            "profile \"review\" executable must not be empty",
        ),
        (
            "ignored profile executable",
            "version = 1\n[profiles.review]\ncommand = [\"true\"]\nexecutable = \"false\"\n",
            "cannot set both command and executable",
        ),
        (
            "ignored profile args",
            "version = 1\n[profiles.review]\ncommand = [\"true\"]\nargs = [\"--ignored\"]\n",
            "cannot set both command and args",
        ),
    ] {
        let message = format!("{:#}", load_config_text(text).unwrap_err());
        assert!(message.contains(expected), "{label}: {message}");
    }
}

#[test]
fn config_semantics_reject_invalid_numeric_limits() {
    for (label, field, expected) in [
        ("memory", "memory_bytes = 0", "memory_bytes must be greater"),
        ("pids", "pids = 0", "pids must be greater"),
        ("quota", "cpu_quota_us = 0", "cpu_quota_us must be greater"),
        (
            "period",
            "cpu_quota_us = 1\ncpu_period_us = 0",
            "cpu_period_us must be greater",
        ),
        (
            "orphan period",
            "cpu_period_us = 100000",
            "cpu_period_us requires cpu_quota_us",
        ),
    ] {
        let text = format!("version = 1\n[profiles.review.limits]\n{field}\n");
        let message = format!("{:#}", load_config_text(&text).unwrap_err());
        assert!(message.contains(expected), "{label}: {message}");
    }

    let config = load_config_text("version = 1\n").unwrap();
    let error = config
        .resolve(
            vec!["/bin/true".into()],
            Some("shell"),
            None,
            Path::new("/tmp"),
            None,
            &BTreeMap::new(),
            &Limits {
                pids: Some(0),
                ..Limits::default()
            },
            None,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("resolved launch limits pids"),
        "{error:#}"
    );
}

#[test]
fn valid_config_references_commands_and_limits_still_load() {
    let config = load_config_text(
        "version = 1\n\
         default_engine = \"custom\"\n\
         default_profile = \"review\"\n\
         [engines.custom]\n\
         command = [\"/bin/sh\", \"-l\"]\n\
         env_unset = [\"CUSTOM_SECRET\"]\n\
         [profiles.review]\n\
         engine = \"custom\"\n\
         args = [\"--review\"]\n\
         history_bytes = 0\n\
         [profiles.review.limits]\n\
         memory_bytes = 1048576\n\
         pids = 4\n\
         cpu_quota_us = 50000\n\
         cpu_period_us = 100000\n\
         [shortcuts.rev]\n\
         engine = \"custom\"\n\
         profile = \"review\"\n",
    )
    .unwrap();

    assert_eq!(config.default_engine.as_deref(), Some("custom"));
    assert_eq!(config.default_profile.as_deref(), Some("review"));
    for (name, shortcut) in &config.shortcuts {
        assert!(config.engines.contains_key(&shortcut.engine), "{name}");
        if let Some(profile) = &shortcut.profile {
            assert!(config.profiles.contains_key(profile), "{name}");
        }
    }
}

/// The load-bearing property from pocketshell-integration-plan.md 0.2: a
/// custom engine's own (smaller/different) `env_unset` can only ADD to
/// the forced provider-key union, never replace or shrink it.
#[test]
fn env_unset_union_is_forced() {
    let mut config = Config {
        default_engine: Some("custom".into()),
        ..Config::default()
    };
    config.engines.insert(
        "custom".into(),
        EngineConfig {
            command: vec!["true".into()],
            env: BTreeMap::new(),
            // deliberately includes a name already in the forced list
            // (to exercise dedup) plus one new name.
            env_unset: vec!["ANTHROPIC_API_KEY".into(), "MY_CUSTOM_VAR".into()],
            skip_permissions_argv: Vec::new(),
        },
    );
    let launch = config
        .resolve(
            Vec::new(),
            None,
            None,
            Path::new("/tmp"),
            None,
            &BTreeMap::new(),
            &Limits::default(),
            None,
        )
        .unwrap();
    for name in PROVIDER_ENV_UNSET_VARS {
        assert!(
            launch.env_unset.iter().any(|v| v == name),
            "forced provider var {name} missing from env_unset"
        );
    }
    assert!(launch.env_unset.iter().any(|v| v == "MY_CUSTOM_VAR"));
    let count = launch
        .env_unset
        .iter()
        .filter(|v| v.as_str() == "ANTHROPIC_API_KEY")
        .count();
    assert_eq!(count, 1, "ANTHROPIC_API_KEY must not be duplicated");
    assert_eq!(
        launch.env_unset.len(),
        PROVIDER_ENV_UNSET_VARS.len() + 1,
        "union must be exactly the forced list plus the one new custom name"
    );
}

#[test]
fn shell_env_unset_preserves_provider_overrides_and_configured_removals() {
    let config = load_config_text(
        "version = 1\n\
         [engines.shell]\n\
         command = [\"/bin/sh\", \"-l\"]\n\
         env_unset = [\"SHELL_SECRET\", \"SHELL_SECRET\"]\n",
    )
    .unwrap();
    let overrides = BTreeMap::from([
        ("OPENAI_API_KEY".into(), "literal-shell-value".into()),
        ("SHELL_SECRET".into(), "remove-me".into()),
    ]);
    let launch = config
        .resolve(
            vec!["/bin/true".into()],
            Some("shell"),
            None,
            Path::new("/tmp"),
            None,
            &overrides,
            &Limits::default(),
            None,
        )
        .unwrap();

    assert_eq!(
        launch.env.get("OPENAI_API_KEY").map(String::as_str),
        Some("literal-shell-value")
    );
    assert_eq!(launch.env_unset, vec!["SHELL_SECRET"]);
    assert!(!launch.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));

    let agent = config
        .resolve(
            vec!["/bin/true".into()],
            Some("codex"),
            None,
            Path::new("/tmp"),
            None,
            &overrides,
            &Limits::default(),
            None,
        )
        .unwrap();
    assert!(agent.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));
}

#[test]
fn skip_permissions_argv_ported_values() {
    let config = Config {
        engines: BTreeMap::from([(
            "claude".to_string(),
            EngineConfig {
                command: vec!["claude".into()],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                skip_permissions_argv: vec!["--dangerously-skip-permissions".into()],
            },
        )]),
        ..Config::default()
    };
    let launch = config
        .resolve(
            Vec::new(),
            Some("claude"),
            None,
            Path::new("/tmp"),
            None,
            &BTreeMap::new(),
            &Limits::default(),
            None,
        )
        .unwrap();
    assert_eq!(
        launch.skip_permissions_argv,
        vec!["--dangerously-skip-permissions".to_string()]
    );
}

#[test]
fn executable_available_requires_execute_permission() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("tool");
    fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(!executable_available(program.to_str().unwrap()));

    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(executable_available(program.to_str().unwrap()));
}
