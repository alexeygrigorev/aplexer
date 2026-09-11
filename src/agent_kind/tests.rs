//! Unit tests for detection, backed by synthetic `/proc` trees so no live
//! processes are needed.

use super::rules::classify_token_detailed;
use super::*;
use crate::config::{Config, EngineConfig, ProfileConfig};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Canonical-only detection: no configured variation tokens.
fn no_variants() -> ProfileVariants {
    ProfileVariants::new()
}

/// Variation tokens for a config whose profiles are exactly these
/// `(id, engine)` pairs -- the shape a user's `[profiles.<id>]` table
/// or `config::discovery`'s output produces.
fn variants_from(profiles: &[(&str, &str)]) -> ProfileVariants {
    let mut config = Config::default();
    for (id, engine) in profiles {
        config.profiles.insert(
            (*id).to_owned(),
            ProfileConfig {
                engine: Some((*engine).to_owned()),
                ..ProfileConfig::default()
            },
        );
    }
    profile_variants(&config)
}

/// Materialise one pid in a synthetic `/proc`: `comm`, `cmdline`, and the
/// `task/<pid>/children` file the descendant walker reads.
fn write_proc(root: &Path, pid: u32, comm: &str, cmdline: &[&str], children: &[u32]) {
    let dir = root.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
    let mut raw = Vec::new();
    for arg in cmdline {
        raw.extend_from_slice(arg.as_bytes());
        raw.push(0);
    }
    fs::write(dir.join("cmdline"), raw).unwrap();
    let task = dir.join("task").join(pid.to_string());
    fs::create_dir_all(&task).unwrap();
    let listed = children
        .iter()
        .map(|child| child.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    fs::write(task.join("children"), format!("{listed}\n")).unwrap();
}

/// `write_proc` plus an `environ` file, the `/proc` evidence profile
/// resolution reads.
fn write_proc_env(root: &Path, pid: u32, comm: &str, cmdline: &[&str], env: &[(&str, &str)]) {
    write_proc(root, pid, comm, cmdline, &[]);
    let mut raw = Vec::new();
    for (key, value) in env {
        raw.extend_from_slice(format!("{key}={value}").as_bytes());
        raw.push(0);
    }
    fs::write(root.join(pid.to_string()).join("environ"), raw).unwrap();
}

fn proc_tree() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    (dir, root)
}

#[test]
fn claude_under_bash_is_detected() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 100, "bash", &["/bin/bash", "-l"], &[101]);
    write_proc(&root, 101, "claude", &["claude"], &[]);

    assert_eq!(
        detect_agent(&root, 100, &no_variants()),
        Some(AgentKind::Claude)
    );
}

#[test]
fn node_wrapped_codex_is_detected() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 200, "bash", &["/bin/bash", "-l"], &[201]);
    write_proc(
        &root,
        201,
        "node",
        &["node", "/home/alexey/.local/bin/codex", "--model", "gpt"],
        &[],
    );

    assert_eq!(
        detect_agent(&root, 200, &no_variants()),
        Some(AgentKind::Codex)
    );
}

/// A config carrying the built-in variant engine: `engine_family`
/// maps `zcodex` onto codex, so the token derives from the config --
/// no hardcoded rule.
fn zcodex_engine_variants() -> ProfileVariants {
    let mut config = Config::default();
    config.engines.insert(
        "zcodex".to_owned(),
        EngineConfig {
            command: vec!["zcodex".to_owned()],
            ..EngineConfig::default()
        },
    );
    profile_variants(&config)
}

#[test]
fn zcodex_under_bash_is_detected_as_codex() {
    // The shape every zcodex session has: a login shell whose child is
    // the codex-rs dev build, `comm` = `zcodex`. It is a codex variant,
    // so it reports the codex kind -- via the configured variant
    // engine, not a hardcoded rule.
    let (_dir, root) = proc_tree();
    write_proc(&root, 210, "bash", &["/bin/bash", "-l"], &[211]);
    write_proc(
        &root,
        211,
        "zcodex",
        &[
            "/home/alexey/git/codex-zcode/codex-rs/target/dev-small/zcodex",
            "--dangerously-bypass-approvals-and-sandbox",
        ],
        &[],
    );

    assert_eq!(
        detect_agent(&root, 210, &zcodex_engine_variants()),
        Some(AgentKind::Codex)
    );
    // Without a configured variation the same binary degrades to
    // plain shell: detection only knows what the config tells it.
    assert_eq!(detect_agent(&root, 210, &no_variants()), None);
}

#[test]
fn zcodex_binary_names_the_zcodex_profile_without_any_env() {
    // The usual hand-launched shape: the variant engine's binary and
    // no profile env at all. The config-derived token names the
    // profile the binary implies.
    let (_dir, root) = proc_tree();
    write_proc(&root, 220, "bash", &["/bin/bash", "-l"], &[221]);
    write_proc(&root, 221, "zcodex", &["/opt/dev-small/zcodex"], &[]);

    assert_eq!(
        detect_agent_detailed(&root, 220, &zcodex_engine_variants()),
        Some(DetectedAgent {
            kind: AgentKind::Codex,
            profile: Some("zcodex".into()),
        })
    );
}

#[test]
fn sibling_codex_home_env_names_that_profile() {
    let (_dir, root) = proc_tree();
    write_proc_env(
        &root,
        230,
        "codex",
        &["codex"],
        &[("CODEX_HOME", "/home/alexey/.godex")],
    );

    assert_eq!(
        detect_agent_detailed(&root, 230, &no_variants()),
        Some(DetectedAgent {
            kind: AgentKind::Codex,
            profile: Some("godex".into()),
        })
    );
}

#[test]
fn default_codex_home_env_is_the_default_profile() {
    let (_dir, root) = proc_tree();
    write_proc_env(
        &root,
        240,
        "codex",
        &["codex"],
        &[("CODEX_HOME", "/home/alexey/.codex")],
    );

    assert_eq!(
        detect_agent_detailed(&root, 240, &no_variants()),
        Some(DetectedAgent {
            kind: AgentKind::Codex,
            profile: None,
        })
    );
}

#[test]
fn a_non_default_env_wins_over_the_variant_binary_name() {
    // A zcodex binary pointed at a different sibling home: the home
    // decides which profile is live, the binary is only the fallback.
    let (_dir, root) = proc_tree();
    write_proc_env(
        &root,
        250,
        "zcodex",
        &["/opt/dev-small/zcodex"],
        &[("CODEX_HOME", "/home/alexey/.godex/")],
    );

    assert_eq!(
        detect_agent_detailed(&root, 250, &zcodex_engine_variants()),
        Some(DetectedAgent {
            kind: AgentKind::Codex,
            profile: Some("godex".into()),
        })
    );
}

#[test]
fn sibling_claude_config_dir_names_that_profile() {
    let (_dir, root) = proc_tree();
    write_proc_env(
        &root,
        260,
        "claude",
        &["claude"],
        &[("CLAUDE_CONFIG_DIR", "/home/alexey/.zlaude")],
    );

    assert_eq!(
        detect_agent_detailed(&root, 260, &no_variants()),
        Some(DetectedAgent {
            kind: AgentKind::Claude,
            profile: Some("zlaude".into()),
        })
    );
}

#[test]
fn agents_without_a_profile_env_can_only_be_the_default_profile() {
    // Grok has no known profile env, so whatever else is in the
    // environment cannot name a variation.
    let (_dir, root) = proc_tree();
    write_proc_env(
        &root,
        270,
        "grok",
        &["grok"],
        &[("GROK_HOME", "/home/alexey/.zgrok")],
    );

    assert_eq!(
        detect_agent_detailed(&root, 270, &no_variants()),
        Some(DetectedAgent {
            kind: AgentKind::Grok,
            profile: None,
        })
    );
}

#[test]
fn profile_label_defaults_to_default() {
    let default = DetectedAgent {
        kind: AgentKind::Codex,
        profile: None,
    };
    assert_eq!(default.profile_label(), "default");
    let zcodex = DetectedAgent {
        kind: AgentKind::Codex,
        profile: Some("zcodex".into()),
    };
    assert_eq!(zcodex.profile_label(), "zcodex");
}

#[test]
fn codex_helper_path_alone_does_not_match() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 300, "bash", &["/bin/bash", "-l"], &[301]);
    write_proc(
        &root,
        301,
        "node",
        &["node", "/opt/tools/codex-helper/index.js"],
        &[],
    );

    assert_eq!(detect_agent(&root, 300, &no_variants()), None);
}

#[test]
fn workload_without_descendants_has_no_agent() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 400, "bash", &["/bin/bash", "-l"], &[]);

    assert_eq!(detect_agent(&root, 400, &no_variants()), None);
}

#[test]
fn workload_leader_itself_can_be_the_agent() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 500, "claude", &["claude"], &[]);

    assert_eq!(
        detect_agent(&root, 500, &no_variants()),
        Some(AgentKind::Claude)
    );
}

#[test]
fn agent_nested_several_levels_below_the_workload_is_found() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 600, "bash", &["/bin/bash", "-l"], &[601]);
    write_proc(&root, 601, "sh", &["/bin/sh", "-c", "run"], &[602]);
    write_proc(&root, 602, "tmux", &["tmux", "attach"], &[603]);
    write_proc(&root, 603, "grok", &["grok", "--always-approve"], &[]);

    assert_eq!(
        detect_agent(&root, 600, &no_variants()),
        Some(AgentKind::Grok)
    );
}

#[test]
fn vanished_pid_mid_walk_is_skipped_and_the_live_sibling_still_matches() {
    let (_dir, root) = proc_tree();
    // 701 is listed as a child but its /proc entry is gone -- exactly what
    // a process exiting between the children read and the comm read
    // looks like.
    write_proc(&root, 700, "bash", &["/bin/bash", "-l"], &[701, 702]);
    write_proc(&root, 702, "claude", &["claude"], &[]);

    assert_eq!(
        detect_agent(&root, 700, &no_variants()),
        Some(AgentKind::Claude)
    );
}

#[test]
fn every_descendant_vanishing_yields_none_without_error() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 800, "bash", &["/bin/bash", "-l"], &[801, 802]);

    assert_eq!(detect_agent(&root, 800, &no_variants()), None);
}

#[test]
fn missing_workload_pid_yields_none_without_error() {
    let (_dir, root) = proc_tree();

    assert_eq!(detect_agent(&root, 999_999, &no_variants()), None);
}

#[test]
fn a_child_cycle_cannot_loop_forever() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 900, "bash", &["/bin/bash"], &[901]);
    write_proc(&root, 901, "sh", &["/bin/sh"], &[900, 901]);

    assert_eq!(detect_agent(&root, 900, &no_variants()), None);
}

#[test]
fn shallower_match_wins_over_a_deeper_one() {
    let (_dir, root) = proc_tree();
    write_proc(&root, 1000, "bash", &["/bin/bash"], &[1001, 1002]);
    write_proc(&root, 1001, "codex", &["codex"], &[1003]);
    write_proc(&root, 1002, "sh", &["/bin/sh"], &[]);
    write_proc(&root, 1003, "claude", &["claude"], &[]);

    assert_eq!(
        detect_agent(&root, 1000, &no_variants()),
        Some(AgentKind::Codex)
    );
}

#[test]
fn command_tokens_are_matched_as_whole_words() {
    // Canonical rules only; variation tokens have their own test below.
    for (text, expected) in [
        ("claude", Some(AgentKind::Claude)),
        ("claude-code", Some(AgentKind::Claude)),
        ("claudecode", Some(AgentKind::Claude)),
        ("/usr/local/bin/claude --resume", Some(AgentKind::Claude)),
        ("sh -c 'claude'", Some(AgentKind::Claude)),
        ("node /home/a/.bun/bin/codex", Some(AgentKind::Codex)),
        ("codex exec", Some(AgentKind::Codex)),
        ("opencode", Some(AgentKind::Opencode)),
        ("open-code", Some(AgentKind::Opencode)),
        ("open_code", Some(AgentKind::Opencode)),
        ("opencode-dev run", Some(AgentKind::Opencode)),
        ("grok --always-approve", Some(AgentKind::Grok)),
        ("(grok)", Some(AgentKind::Grok)),
        ("bash -l", None),
        ("claudette", None),
        ("my-claude", None),
        ("codex-helper", None),
        ("/opt/codex-helper/run", None),
        ("grokking", None),
        ("xcodex", None),
        ("", None),
    ] {
        assert_eq!(
            classify_token(text, &no_variants()),
            expected,
            "classifying {text:?}"
        );
    }
}

#[test]
fn variation_tokens_classify_through_the_config() {
    // Any config variation, not just this box's: a codex profile named
    // `acme` makes a binary `acme` classify as codex running the acme
    // profile.
    let variants = variants_from(&[("acme", "codex"), ("zteam", "claude")]);
    assert_eq!(
        classify_token_detailed("acme", &variants),
        Some((AgentKind::Codex, Some("acme".to_owned())))
    );
    assert_eq!(
        classify_token_detailed("/opt/tools/acme --serve", &variants),
        Some((AgentKind::Codex, Some("acme".to_owned())))
    );
    assert_eq!(
        classify_token_detailed("claude-team", &variants),
        None,
        "a variation token matches only itself"
    );
    assert_eq!(
        classify_token_detailed("zteam", &variants),
        Some((AgentKind::Claude, Some("zteam".to_owned())))
    );
    // Lowercasing applies to variation tokens the same as canonical.
    assert_eq!(
        classify_token_detailed("ACME", &variants),
        Some((AgentKind::Codex, Some("acme".to_owned())))
    );
}

#[test]
fn variation_tokens_respect_whole_word_boundaries() {
    let variants = variants_from(&[("acme", "codex")]);
    assert_eq!(classify_token("acme-helper", &variants), None);
    assert_eq!(classify_token("aacme", &variants), None);
    assert_eq!(classify_token("/opt/acme-dev/bin", &variants), None);
    assert_eq!(
        classify_token("sh -c 'acme'", &variants),
        Some(AgentKind::Codex)
    );
}

#[test]
fn canonical_rules_are_never_shadowed_by_variation_tokens() {
    // A sibling dir named `.odex` would discover a profile whose id
    // sits inside the canonical `codex` token; the canonical rule must
    // still own `codex`, with no variation attributed. And a profile
    // id that itself classifies canonically (`claude-code`) never
    // enters the variant table at all.
    let variants = variants_from(&[("odex", "codex"), ("claude-code", "codex")]);
    assert_eq!(
        classify_token_detailed("codex", &variants),
        Some((AgentKind::Codex, None))
    );
    assert_eq!(
        classify_token_detailed("claude-code", &variants),
        Some((AgentKind::Claude, None)),
        "canonical claude wins over the skipped variant entry"
    );
    assert!(!variants.contains_key("claude-code"));
}

#[test]
fn a_profiles_executable_and_command_basenames_become_tokens() {
    let mut config = Config::default();
    config.profiles.insert(
        "team".to_owned(),
        ProfileConfig {
            engine: Some("claude".to_owned()),
            executable: Some("claude-team".to_owned()),
            ..ProfileConfig::default()
        },
    );
    config.profiles.insert(
        "lab".to_owned(),
        ProfileConfig {
            engine: Some("codex".to_owned()),
            command: Some(vec!["/opt/lab/bin/codex-lab".to_owned()]),
            ..ProfileConfig::default()
        },
    );
    let variants = profile_variants(&config);
    assert_eq!(
        variants.get("team"),
        Some(&(AgentKind::Claude, "team".to_owned()))
    );
    assert_eq!(
        variants.get("claude-team"),
        Some(&(AgentKind::Claude, "team".to_owned()))
    );
    assert_eq!(
        variants.get("codex-lab"),
        Some(&(AgentKind::Codex, "lab".to_owned()))
    );
}

#[test]
fn non_agent_profiles_contribute_no_variation_tokens() {
    let mut config = Config::default();
    // No engine at all resolves to the default engine, which is not an
    // agent (`shell`), so the profile cannot name a variation...
    config
        .profiles
        .insert("misc".to_owned(), ProfileConfig::default());
    assert!(profile_variants(&config).is_empty());
    // ... and an explicit shell profile is the same story.
    config.profiles.insert(
        "sh".to_owned(),
        ProfileConfig {
            engine: Some("shell".to_owned()),
            ..ProfileConfig::default()
        },
    );
    assert!(profile_variants(&config).is_empty());
    // But a config whose default engine IS an agent makes the
    // engine-less profile a variation of it, mirroring how
    // `Config::resolve` picks the launch engine.
    config.default_engine = Some("codex".to_owned());
    let variants = profile_variants(&config);
    assert_eq!(
        variants.get("misc"),
        Some(&(AgentKind::Codex, "misc".to_owned()))
    );
    // The shell profile stays out regardless.
    assert!(!variants.contains_key("sh"));
}

#[test]
fn a_profile_entry_wins_over_an_engine_entry_on_a_shared_token() {
    let mut config = Config::default();
    config.engines.insert(
        "acme".to_owned(),
        EngineConfig {
            command: vec!["acme".to_owned()],
            ..EngineConfig::default()
        },
    );
    // A user profile rebinding the same token to another engine wins.
    config.profiles.insert(
        "acme".to_owned(),
        ProfileConfig {
            engine: Some("claude".to_owned()),
            ..ProfileConfig::default()
        },
    );
    let variants = profile_variants(&config);
    assert_eq!(
        variants.get("acme"),
        Some(&(AgentKind::Claude, "acme".to_owned()))
    );
}

#[test]
fn tokens_are_lowercase_rules_applied_to_lowercased_text() {
    // Mirrors cgroup_agents.py::classify_token, which lowercases the comm
    // /cmdline (`lowered = text.lower()`) before applying the same
    // lowercase token patterns -- so the rules are lowercase-only, and an
    // upper/mixed-case command still classifies.
    assert_eq!(
        classify_token("CLAUDE", &no_variants()),
        Some(AgentKind::Claude)
    );
    assert_eq!(
        classify_token("/usr/bin/Codex", &no_variants()),
        Some(AgentKind::Codex)
    );
}

#[test]
fn comm_is_classified_before_cmdline() {
    let (_dir, root) = proc_tree();
    // A wrapper whose comm already names the agent must not need its
    // cmdline read at all -- the cmdline here names nothing.
    write_proc(&root, 1100, "claude", &["-zsh"], &[]);

    assert_eq!(
        detect_agent(&root, 1100, &no_variants()),
        Some(AgentKind::Claude)
    );
}

#[test]
fn agent_kind_serialises_to_its_lowercase_name() {
    for kind in [
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Opencode,
        AgentKind::Grok,
    ] {
        assert_eq!(
            serde_json::to_value(kind).unwrap(),
            serde_json::Value::String(kind.name().to_owned()),
        );
    }
    assert_eq!(
        serde_json::to_value(Option::<AgentKind>::None).unwrap(),
        serde_json::Value::Null,
    );
}
