//! The provider API-key environment stripped from agent-engine launches.

/// Provider API-key-style env vars unset for every agent-engine launch, so
/// the agent falls back to its subscription auth instead of a per-token env
/// key. Ported verbatim (same order) from PocketShell's
/// `tools/pocketshell/src/pocketshell/engines.py::PROVIDER_ENV_UNSET_VARS`
/// (maintainer decision, pocketshell issue #703 -- subscription billing
/// across the board for codex/claude/opencode). `Config::resolve` unions
/// this with each agent engine's own `EngineConfig.env_unset`; the union is
/// forced for every engine id except the literal `shell` engine (see that
/// function's doc comment) -- an agent config can only add to this list,
/// never remove from it.
pub(crate) const PROVIDER_ENV_UNSET_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORG_ID",
    "OPENAI_PROJECT_ID",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "GROQ_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_API_KEY",
    "VERTEX_LOCATION",
    "VERTEX_AI_PROJECT",
    "DEEPSEEK_API_KEY",
    "XAI_API_KEY",
    "FIREWORKS_API_KEY",
    "CEREBRAS_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "TOGETHER_AI_API_KEY",
    "AZURE_API_KEY",
    "AZURE_RESOURCE_NAME",
    "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_GATEWAY_ID",
    "CLOUDFLARE_API_KEY",
    "HUGGING_FACE_API_KEY",
    "HF_TOKEN",
    "HF_API_TOKEN",
    "MOONSHOT_API_KEY",
    "MOONSHOTAI_API_KEY",
    "MINIMAX_API_KEY",
    "NEBIUS_API_KEY",
    "DEEPINFRA_API_KEY",
    "BASETEN_API_KEY",
    "VENICE_API_KEY",
    "SCALEWAY_API_KEY",
    "OVH_API_KEY",
    "CORTECS_API_KEY",
    "IONET_API_KEY",
    "VERCEL_API_KEY",
    "ZENMUX_API_KEY",
    "ZAI_API_KEY",
    "HELICONE_API_KEY",
    "OPENCODE_API_KEY",
    "OPENCODE_ZEN_API_KEY",
    "GITLAB_TOKEN",
    "GITLAB_INSTANCE_URL",
    "GITLAB_AI_GATEWAY_URL",
    "GITLAB_OAUTH_CLIENT_ID",
    "AICORE_SERVICE_KEY",
    "AICORE_DEPLOYMENT_ID",
    "AICORE_RESOURCE_GROUP",
    "OPENAI_COMPATIBLE_API_KEY",
    "LMSTUDIO_API_KEY",
    "OLLAMA_API_KEY",
    "302AI_API_KEY",
    "FIRMWARE_API_KEY",
    "2AI_API_KEY",
    "GEMINI_API_KEY",
];

/// Preserve first-seen order and drop blanks/duplicates across the policy
/// prefix and configured additions.
pub(crate) fn ordered_unique_env_names<'a>(
    prefix: impl IntoIterator<Item = &'a str>,
    extra: &'a [String],
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for name in prefix.into_iter().chain(extra.iter().map(String::as_str)) {
        let trimmed = name.trim();
        if !trimmed.is_empty() && seen.insert(trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}
