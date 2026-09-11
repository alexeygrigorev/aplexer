//! The command-token classifier: the whole-word rules a comm or cmdline
//! string is matched against, canonical and config-derived.
//!
//! The token rules mirror pocketshell's server-side classifier
//! (`tools/pocketshell/src/pocketshell/cgroup_agents.py`, itself mirroring
//! `AgentDetector.namesAgent`): a comm/cmdline names an agent when the
//! agent's command token appears as a whole word -- bounded by the start/end
//! of the string or by shell/path delimiters. That is what lets a bare
//! `codex` comm and a node-wrapped `node /…/bin/codex` cmdline both classify
//! as codex while `codex-helper` buried in an unrelated path does not.

use std::path::Path;

use super::{AgentKind, ProfileVariants};

/// Characters that may precede an agent's command token. Mirrors
/// `cgroup_agents.py`'s `_BOUNDARY_LEAD`: start-of-string, whitespace, or a
/// shell/path delimiter. Note `-` is deliberately absent, so `my-claude`
/// does not name claude.
const LEAD_DELIMITERS: &[u8] = b" \t\n\r\x0b\x0c/|;&('\"`";
/// Characters that may follow an agent's command token. Mirrors
/// `_BOUNDARY_TAIL`, which additionally allows `)` and `:`.
const TAIL_DELIMITERS: &[u8] = b" \t\n\r\x0b\x0c/|;&):'\"`";

/// One agent's command-token rule, expressed as the literal alternatives the
/// Python/Kotlin regexes accept. These are the *canonical* rules only: the
/// engine commands every installation shares. Variation tokens (this
/// installation's `zcodex`, a user profile's `acme`, ...) come from the
/// config via [`super::profile_variants`], never from this table.
struct TokenRule {
    kind: AgentKind,
    /// Literal stems the token may start with (regex alternation).
    stems: &'static [&'static str],
    /// Optional literal continuations directly after a stem (`claude` also
    /// matches `claudecode` and `claude-code`, per `claude(?:-?code)?`).
    literal_suffixes: &'static [&'static str],
    /// Whether a `[-_][a-z0-9]+` continuation is accepted after the stem,
    /// the tail of `open[-_]?code(?:[-_][a-z0-9]+)?`.
    alnum_suffix: bool,
}

/// Rules in the same order `cgroup_agents.py` applies them. Only agents
/// every installation shares appear here; a box-specific variant command
/// (`zcodex`) is not hardcoded -- it is derived from the configured
/// `zcodex` engine and profile by [`super::profile_variants`], exactly like
/// any user-defined variation. Order matters only within the canonical set:
/// no string matches two of these rules, since no agent name sits at a lead
/// boundary inside another.
const TOKEN_RULES: &[TokenRule] = &[
    TokenRule {
        kind: AgentKind::Claude,
        stems: &["claude"],
        literal_suffixes: &["code", "-code"],
        alnum_suffix: false,
    },
    TokenRule {
        kind: AgentKind::Codex,
        stems: &["codex"],
        literal_suffixes: &[],
        alnum_suffix: false,
    },
    TokenRule {
        kind: AgentKind::Opencode,
        stems: &["opencode", "open-code", "open_code"],
        literal_suffixes: &[],
        alnum_suffix: true,
    },
    TokenRule {
        kind: AgentKind::Grok,
        stems: &["grok"],
        literal_suffixes: &[],
        alnum_suffix: false,
    },
];

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| offset + from)
}

fn is_lead_boundary(bytes: &[u8], start: usize) -> bool {
    start == 0 || LEAD_DELIMITERS.contains(&bytes[start - 1])
}

fn is_tail_boundary(bytes: &[u8], end: usize) -> bool {
    end == bytes.len() || TAIL_DELIMITERS.contains(&bytes[end])
}

/// End of a `[-_][a-z0-9]+` continuation starting at `start`, if present.
/// Only the longest run is considered: any shorter one would end on an
/// alphanumeric character, which is never a tail boundary.
fn alnum_suffix_end(bytes: &[u8], start: usize) -> Option<usize> {
    if !matches!(bytes.get(start), Some(b'-') | Some(b'_')) {
        return None;
    }
    let mut end = start + 1;
    while matches!(bytes.get(end), Some(byte) if byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        end += 1;
    }
    (end > start + 1).then_some(end)
}

impl TokenRule {
    /// Whether `lowered` (already lowercased) contains this rule's command
    /// token as a whole word.
    fn matches(&self, lowered: &str) -> bool {
        let bytes = lowered.as_bytes();
        for stem in self.stems {
            let stem = stem.as_bytes();
            let mut cursor = 0;
            while let Some(start) = find_from(bytes, stem, cursor) {
                cursor = start + 1;
                if !is_lead_boundary(bytes, start) {
                    continue;
                }
                let stem_end = start + stem.len();
                if is_tail_boundary(bytes, stem_end) {
                    return true;
                }
                for suffix in self.literal_suffixes {
                    let end = stem_end + suffix.len();
                    if bytes.len() >= end
                        && &bytes[stem_end..end] == suffix.as_bytes()
                        && is_tail_boundary(bytes, end)
                    {
                        return true;
                    }
                }
                if self.alnum_suffix {
                    if let Some(end) = alnum_suffix_end(bytes, stem_end) {
                        if is_tail_boundary(bytes, end) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

/// The agent kind a canonical token rule names, ignoring any variation
/// tokens. Also the guard `profile_variants` applies so its entries can
/// never shadow these strings.
pub(super) fn canonical_kind(text: &str) -> Option<AgentKind> {
    let lowered = text.to_lowercase();
    TOKEN_RULES
        .iter()
        .find(|rule| rule.matches(&lowered))
        .map(|rule| rule.kind)
}

/// Whether `lowered` contains `token` as a whole word -- the same lead/tail
/// boundary rules the canonical token rules apply, so `acme` matches
/// `/opt/bin/acme --serve` but not `acme-helper`.
fn whole_word_match(lowered: &str, token: &str) -> bool {
    let bytes = lowered.as_bytes();
    let needle = token.as_bytes();
    let mut cursor = 0;
    while let Some(start) = find_from(bytes, needle, cursor) {
        cursor = start + 1;
        if is_lead_boundary(bytes, start) && is_tail_boundary(bytes, start + needle.len()) {
            return true;
        }
    }
    false
}

/// The agent named by a single comm or cmdline string, if any.
///
/// Mirrors `cgroup_agents.py::classify_token`: the text is lowercased first
/// and the rules themselves are lowercase, so `CLAUDE` and `claude` classify
/// identically. `variants` carries this installation's variation tokens
/// (`profile_variants` over the loaded config); the canonical rules are
/// consulted first, so a variation token never reclassifies a string the
/// canonical rules already name.
pub fn classify_token(text: &str, variants: &ProfileVariants) -> Option<AgentKind> {
    classify_token_detailed(text, variants).map(|(kind, _)| kind)
}

/// `classify_token` plus the variation the token names: `None` for a
/// canonical match (the profile comes from the process's environment, if
/// any), `Some(profile id)` when a configured variation token matched.
pub(super) fn classify_token_detailed(
    text: &str,
    variants: &ProfileVariants,
) -> Option<(AgentKind, Option<String>)> {
    let lowered = text.to_lowercase();
    if let Some(rule) = TOKEN_RULES.iter().find(|rule| rule.matches(&lowered)) {
        return Some((rule.kind, None));
    }
    variants
        .iter()
        .find(|(token, _)| whole_word_match(&lowered, token))
        .map(|(_, (kind, label))| (*kind, Some(label.clone())))
}

/// Basename of a command/executable path, as a variation-token candidate.
pub(super) fn basename(arg: &str) -> &str {
    Path::new(arg)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(arg)
}
