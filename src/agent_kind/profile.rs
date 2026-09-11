//! The variation-token table derived from the loaded config: which command
//! tokens mean "this agent running as that profile".

use std::collections::BTreeMap;

use crate::config::Config;

use super::rules::{basename, canonical_kind};
use super::AgentKind;

/// The variation tokens detection consults for one loaded [`Config`],
/// mapping a command token to the agent kind it implies and the profile id
/// to report. Derived entirely from the configuration, so a variation
/// defined in some installation's config is detected there and nowhere
/// hardcoded:
///
/// * A non-canonical *engine* id whose family is a known agent
///   (`engine_family("zcodex") == "codex"`) contributes its id and its
///   command's basename: running that engine's binary is running that
///   variation, env override or not.
/// * Every *profile* contributes its id, and the basenames of its
///   `executable`/`command` when set, keyed to the kind its engine
///   resolves to (the same engine resolution `Config::resolve` applies:
///   the profile's engine, else the configured default). A session
///   running binary `acme` is the `acme` profile of acme's engine,
///   whatever the profile is named on this installation.
///
/// Tokens that already classify canonically (`codex`, `claude-code`) are
/// skipped, so a variant entry can never shadow the canonical rules, and
/// profile entries win over engine entries on a shared token (user intent
/// beats the built-in derivation).
pub type ProfileVariants = BTreeMap<String, (AgentKind, String)>;

pub fn profile_variants(config: &Config) -> ProfileVariants {
    fn add(map: &mut ProfileVariants, token: &str, kind: AgentKind, label: &str) {
        if !token.is_empty() && canonical_kind(token).is_none() {
            map.insert(token.to_owned(), (kind, label.to_owned()));
        }
    }
    let mut variants = ProfileVariants::new();
    for (id, engine) in &config.engines {
        let Some(kind) = canonical_kind(crate::engine_family(id)) else {
            continue;
        };
        add(&mut variants, id, kind, id);
        if let Some(command) = engine.command.first() {
            add(&mut variants, basename(command), kind, id);
        }
    }
    for (id, profile) in &config.profiles {
        let engine = profile
            .engine
            .as_deref()
            .or(config.default_engine.as_deref());
        let Some(kind) = engine.and_then(|engine| canonical_kind(crate::engine_family(engine)))
        else {
            continue;
        };
        add(&mut variants, id, kind, id);
        if let Some(executable) = &profile.executable {
            add(&mut variants, basename(executable), kind, id);
        }
        if let Some(command) = &profile.command {
            if let Some(first) = command.first() {
                add(&mut variants, basename(first), kind, id);
            }
        }
    }
    variants
}
