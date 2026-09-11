//! Which coding agent is running inside a session, detected at query time.
//!
//! Every pocketshell-created aplexer session is `engine: "shell"` with the
//! agent launched by hand inside it, so `engine` cannot name the agent. What
//! aplexer does own is the workload process (`SessionRecord::workload_pid`)
//! and, through `/proc/<pid>/task/*/children`, its whole descendant tree --
//! the same walk `worker::descendant_pids` uses for containment. This module
//! reuses that walker to answer "which agent is live in this session right
//! now" from the process tree, and -- through `DetectedAgent` -- which of
//! the agent's configured variations (profiles, spec.md 9) that agent runs
//! as. Everything the resolution needs comes from the configuration rather
//! than from this module: the profile env vars from `config::discovery`'s
//! rule table (`PROFILE_DISCOVERY_RULES`, the same table that
//! auto-discovers profile dirs), and the variation tokens (`zcodex`,
//! `zodex`, a user profile's executable, ...) from [`profile_variants`]
//! over the loaded `Config` -- so a variation defined only in some other
//! installation's config is detected there without a code change.
//!
//! Two deliberate properties:
//!
//! * **Nothing is persisted.** Detection runs only when a caller asks for
//!   `a list --json` / `a snapshot` / `a status --json`. A record on disk
//!   never carries an `agent` field, so it cannot go stale, and the worker's
//!   hot path never pays for this.
//! * **Every read is defensive.** A pid that exits between the `children`
//!   read and the `comm` read, an unreadable `/proc` entry, a permission
//!   error -- all are skipped. Detection degrades to "no agent found"
//!   (`None`), never to an error that would fail the whole listing.

use std::fmt;

use serde::Serialize;

mod detect;
mod profile;
mod rules;
#[cfg(test)]
mod tests;

pub use detect::{detect_agent, detect_agent_detailed};
pub use profile::{profile_variants, ProfileVariants};
pub use rules::classify_token;

/// The `/proc` root detection reads. Injectable so the unit tests classify a
/// synthetic tree with zero live processes.
pub const DEFAULT_PROC_ROOT: &str = "/proc";

/// An agent aplexer can recognise from a workload's process tree. The serde
/// representation is the lowercase name that appears on the wire, identical
/// to the kinds pocketshell's own classifier returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
    Grok,
}

impl AgentKind {
    /// The wire/display name, identical to this enum's serde representation.
    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
            AgentKind::Grok => "grok",
        }
    }

    /// The profile-config environment variable this agent honours and the
    /// basename of its default config dir, taken from `config::discovery`'s
    /// rule table -- the single source both auto-discovery and detection
    /// read, so they can never disagree about where a variation lives.
    /// `None` for the agents with no rule -- opencode has no profile env
    /// var and grok is not known to have one, so neither can run as
    /// anything but the default profile.
    pub(super) fn profile_env(self) -> Option<(&'static str, &'static str)> {
        crate::config::PROFILE_DISCOVERY_RULES
            .iter()
            .find(|rule| rule.engine == self.name())
            .map(|rule| (rule.env_var, rule.default_dirname))
    }
}

/// Which agent is live in a session, plus which of the agent's configured
/// variations ("profiles", spec.md 9) it is running as. Detection names the
/// variation the same way `config::discovery` names profiles -- the config
/// dir's stem minus its leading dot (`~/.zodex` -> `zodex`) -- so a detected
/// stem is always the id that profile is (or would be) registered under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAgent {
    pub kind: AgentKind,
    /// The variation's stem when the agent runs as a named profile, `None`
    /// for the engine's own default config. Never a display label, always
    /// a profile id: `profile_label` is where the "default" spelling comes
    /// from.
    pub profile: Option<String>,
}

impl DetectedAgent {
    /// The wire/display name of the variation: the profile id, or
    /// `"default"` when the agent runs the engine's own config untouched.
    pub fn profile_label(&self) -> &str {
        self.profile.as_deref().unwrap_or("default")
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}
