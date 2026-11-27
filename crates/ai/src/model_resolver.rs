//! Model references as the TypeScript product resolves them (`coding-agent/src/core/model-resolver.ts`):
//! `findExactModelReferenceMatch`, `parseModelPattern` (exact, then partial id/name matching preferring aliases, with
//! `:<thinking>` suffixes peeled off the end), `resolveModelScopeFromModels` (`--models` / `enabledModels`: globs
//! through minimatch, fuzzy patterns otherwise), `resolveCliModel` (`--provider` / `--model`: provider inference,
//! ambiguity across providers, the custom-id fallback) and `findInitialModel`.
//!
//! Everything works over a model list and an "has configured auth" predicate, as the TypeScript does over
//! `ModelRuntime`; callers pass the registry's models and their availability.
//!
//! Evidence: `fixtures/generate-model-resolver.mjs` runs the TypeScript resolver; `tests/model_resolver.rs` replays it.

use crate::types::Model;

use model_scope::{ScopeDiagnostic, is_valid_thinking_level};

/// `defaultModelPerProvider`: the model a fallback prefers for each known provider, in declaration order.
pub const DEFAULT_MODEL_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", "claude-opus-4-8"),
    ("openai", "gpt-5.5"),
    ("azure-openai-responses", "gpt-5.4"),
    ("openai-codex", "gpt-5.5"),
    ("radius", "balanced"),
    ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.7"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "gpt-oss-120b"),
    ("zai", "glm-5.3"),
    ("zai-coding-cn", "glm-5.3"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k2p6"),
    ("together", "moonshotai/Kimi-K2.6"),
    ("baseten", "zai-org/GLM-5.2"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k2.6"),
    ("kimi-coding", "kimi-for-coding"),
    ("meta", "muse-spark-1.3"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    ("cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("qwen-token-plan-individual", "qwen3.8-max"),
    ("xiaomi", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
];

fn default_model_id(provider: &str) -> Option<&'static str> {
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, id)| *id)
}

/// `isClaudeCodeModel`: Claude Code CLI models answer only to their full `claude-code/<id>` reference.
pub fn is_claude_code_model(model: &Model) -> bool {
    model.api == crate::claude_code::API
}

impl model_scope::ScopeModel for Model {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn full_reference_only(&self) -> bool {
        config::providers::full_reference_only(&self.provider, &self.api)
    }
}

/// A model chosen by a pattern, with the thinking level the pattern named.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedModel {
    pub model: Model,
    pub thinking_level: Option<String>,
}

/// `ParsedModelResult`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedModel {
    pub model: Option<Model>,
    pub thinking_level: Option<String>,
    pub warning: Option<String>,
}

/// `parseModelPattern` ([`model_scope::parse_model_pattern`]) with the model owned.
pub fn parse_model_pattern(pattern: &str, models: &[Model], allow_invalid_fallback: bool) -> ParsedModel {
    let parsed = model_scope::parse_model_pattern(pattern, models, allow_invalid_fallback);
    ParsedModel {
        model: parsed.model.cloned(),
        thinking_level: parsed.thinking_level,
        warning: parsed.warning,
    }
}

/// `resolveModelScopeFromModels` ([`model_scope::resolve_model_scope`]) with the models owned.
pub fn resolve_model_scope(patterns: &[String], models: &[Model]) -> (Vec<ScopedModel>, Vec<ScopeDiagnostic>) {
    let (scoped, diagnostics) = model_scope::resolve_model_scope(patterns, models);
    let scoped = scoped
        .into_iter()
        .map(|s| ScopedModel {
            model: s.model.clone(),
            thinking_level: s.thinking_level,
        })
        .collect();
    (scoped, diagnostics)
}

/// `ResolveCliModelResult`: `error` set means no model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CliModel {
    pub model: Option<Model>,
    pub thinking_level: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

impl CliModel {
    fn found(model: &Model) -> Self {
        CliModel {
            model: Some(model.clone()),
            ..CliModel::default()
        }
    }
}

/// `buildFallbackModel`: the provider's default model (else its first) under a custom id.
fn build_fallback_model(provider: &str, id: &str, models: &[Model]) -> Option<Model> {
    let ours: Vec<&Model> = models.iter().filter(|m| m.provider == provider).collect();
    let first = *ours.first()?;
    let base = default_model_id(provider)
        .and_then(|default| ours.iter().copied().find(|m| m.id == default))
        .unwrap_or(first);
    let mut model = base.clone();
    model.id = id.to_string();
    model.name = id.to_string();
    Some(model)
}

fn canonical_provider(models: &[Model], name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    models
        .iter()
        .rev()
        .find(|m| m.provider.to_lowercase() == lower)
        .map(|m| m.provider.clone())
}

fn is_raw_exact(model: &Model, lower: &str) -> bool {
    model.id.to_lowercase() == lower || model_scope::reference(model).to_lowercase() == lower
}

fn resolve_explicit_provider(cli_provider: Option<&str>, models: &[Model]) -> Result<Option<String>, String> {
    let Some(name) = cli_provider.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    canonical_provider(models, name)
        .map(Some)
        .ok_or_else(|| format!("Unknown provider \"{name}\". Use --list-models to see available providers/models."))
}

fn resolve_unscoped_exact(
    cli_model: &str,
    lower: &str,
    models: &[Model],
    has_auth: &dyn Fn(&str) -> bool,
) -> Option<CliModel> {
    let exact: Vec<&Model> = models.iter().filter(|m| is_raw_exact(m, lower)).collect();
    if exact.is_empty() {
        return None;
    }
    if exact.len() == 1 {
        return Some(CliModel::found(exact[0]));
    }
    let authenticated: Vec<&Model> = exact.iter().copied().filter(|m| has_auth(&m.provider)).collect();
    if authenticated.len() == 1 {
        return Some(CliModel::found(authenticated[0]));
    }
    let mut refs: Vec<String> = exact.iter().map(|m| model_scope::reference(*m)).collect();
    refs.sort_by(|a, b| jsrt::collate::locale_compare(a, b));
    let hint = if authenticated.is_empty() {
        "No matching provider is authenticated."
    } else {
        "More than one matching provider is authenticated."
    };
    Some(CliModel {
        error: Some(format!(
            "Model \"{cli_model}\" is ambiguous across providers: {}. {hint} Use --provider or provider/model.",
            refs.join(", ")
        )),
        ..CliModel::default()
    })
}

fn strip_provider_prefix(cli_model: &str, provider: &str) -> Option<String> {
    let prefix = format!("{provider}/");
    if cli_model.to_lowercase().starts_with(&prefix.to_lowercase()) {
        return cli_model.get(prefix.len()..).map(str::to_string);
    }
    None
}

fn resolve_parsed_match(
    parsed: ParsedModel,
    inferred: bool,
    lower: &str,
    models: &[Model],
    has_auth: &dyn Fn(&str) -> bool,
) -> CliModel {
    if inferred
        && let Some(model) = &parsed.model
        && !has_auth(&model.provider)
    {
        let authenticated: Vec<&Model> = models
            .iter()
            .filter(|m| m.id.to_lowercase() == lower && !model_scope::same(*m, model))
            .filter(|m| has_auth(&m.provider))
            .collect();
        if authenticated.len() == 1 {
            return CliModel::found(authenticated[0]);
        }
    }
    CliModel {
        model: parsed.model,
        thinking_level: parsed.thinking_level,
        warning: parsed.warning,
        error: None,
    }
}

fn resolve_inferred_fallback(cli_model: &str, lower: &str, models: &[Model]) -> Option<CliModel> {
    if let Some(exact) = models.iter().find(|m| is_raw_exact(m, lower)) {
        return Some(CliModel::found(exact));
    }
    let fallback = parse_model_pattern(cli_model, models, false);
    fallback.model.map(|model| CliModel {
        model: Some(model),
        thinking_level: fallback.thinking_level,
        warning: fallback.warning,
        error: None,
    })
}

fn resolve_custom_provider_model(
    provider: &str,
    pattern: &str,
    cli_thinking: Option<&str>,
    prior_warning: Option<&str>,
    models: &[Model],
) -> Option<CliModel> {
    let (mut id, mut thinking) = (pattern.to_string(), None);
    if cli_thinking.is_none()
        && let Some((head, suffix)) = pattern.rsplit_once(':')
        && is_valid_thinking_level(suffix)
    {
        id = head.to_string();
        thinking = Some(suffix.to_string());
    }
    let mut model = build_fallback_model(provider, &id, models)?;
    if cli_thinking.or(thinking.as_deref()).is_some_and(|t| t != "off") {
        model.reasoning = true;
    }
    let note = format!("Model \"{id}\" not found for provider \"{provider}\". Using custom model id.");
    let warning = match prior_warning {
        Some(w) => format!("{w} {note}"),
        None => note,
    };
    Some(CliModel {
        model: Some(model),
        thinking_level: thinking,
        warning: Some(warning),
        error: None,
    })
}

fn infer_provider_and_pattern(
    explicit: Option<String>,
    cli_provider: Option<&str>,
    cli_model: &str,
    models: &[Model],
) -> (Option<String>, String, bool) {
    if explicit.is_none()
        && let Some((head, rest)) = cli_model.split_once('/')
        && let Some(found) = canonical_provider(models, head)
    {
        return (Some(found), rest.to_string(), true);
    }
    let pattern = if cli_provider.is_some()
        && let Some(p) = explicit.as_deref()
        && let Some(stripped) = strip_provider_prefix(cli_model, p)
    {
        stripped
    } else {
        cli_model.to_string()
    };
    (explicit, pattern, false)
}

/// `resolveCliModel`: `--provider` / `--model` (fuzzy, `<pattern>:<thinking>`, `provider/pattern`) against every
/// model, `has_auth` telling which providers have configured credentials.
pub fn resolve_cli_model(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<&str>,
    models: &[Model],
    has_auth: &dyn Fn(&str) -> bool,
) -> CliModel {
    let Some(cli_model) = cli_model.filter(|m| !m.is_empty()) else {
        return CliModel::default();
    };
    if models.is_empty() {
        return CliModel {
            error: Some("No models available. Check your installation or add models to models.json.".into()),
            ..CliModel::default()
        };
    }
    let explicit = match resolve_explicit_provider(cli_provider, models) {
        Ok(p) => p,
        Err(error) => {
            return CliModel {
                error: Some(error),
                ..CliModel::default()
            };
        }
    };
    let (provider, pattern, inferred) = infer_provider_and_pattern(explicit, cli_provider, cli_model, models);
    let lower = cli_model.to_lowercase();
    if provider.is_none()
        && let Some(exact) = resolve_unscoped_exact(cli_model, &lower, models, has_auth)
    {
        return exact;
    }
    resolve_with_candidates(
        provider.as_deref(),
        &pattern,
        cli_model,
        &lower,
        inferred,
        cli_thinking,
        models,
        has_auth,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal helper forwarding parsed CLI model state"
)]
fn resolve_with_candidates(
    provider: Option<&str>,
    pattern: &str,
    cli_model: &str,
    lower: &str,
    inferred: bool,
    cli_thinking: Option<&str>,
    models: &[Model],
    has_auth: &dyn Fn(&str) -> bool,
) -> CliModel {
    let candidates: Vec<Model> = match provider {
        Some(p) => models.iter().filter(|m| m.provider == p).cloned().collect(),
        None => models.to_vec(),
    };
    let parsed = parse_model_pattern(pattern, &candidates, false);
    if parsed.model.is_some() {
        return resolve_parsed_match(parsed, inferred, lower, models, has_auth);
    }
    if inferred && let Some(fallback) = resolve_inferred_fallback(cli_model, lower, models) {
        return fallback;
    }
    if let Some(p) = provider
        && let Some(custom) = resolve_custom_provider_model(p, pattern, cli_thinking, parsed.warning.as_deref(), models)
    {
        return custom;
    }
    let display = match provider {
        Some(p) => format!("{p}/{pattern}"),
        None => cli_model.to_string(),
    };
    CliModel {
        warning: parsed.warning,
        error: Some(format!(
            "Model \"{display}\" not found. Use --list-models to see available models."
        )),
        ..CliModel::default()
    }
}

/// The saved settings `findInitialModel` reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InitialSettings {
    pub default_provider: Option<String>,
    pub default_model_id: Option<String>,
    pub default_thinking_level: Option<String>,
    /// `modelThinkingLevels`: `provider/id` to level.
    pub model_thinking_levels: Vec<(String, String)>,
}

impl InitialSettings {
    fn per_model(&self, provider: &str, id: &str) -> Option<String> {
        let key = format!("{provider}/{id}");
        self.model_thinking_levels
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    }
}

fn resolve_thinking_level(explicit: Option<&str>, settings: &InitialSettings, provider: &str, id: &str) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| settings.per_model(provider, id))
        .or_else(|| settings.default_thinking_level.clone())
        .unwrap_or_else(|| model_scope::DEFAULT_THINKING_LEVEL.into())
}

fn pick_fallback_model(available: &[Model]) -> Option<Model> {
    let eligible: Vec<&Model> = available
        .iter()
        .filter(|m| !is_claude_code_model(m) && m.provider != crate::models::ANTIGRAVITY_PROVIDER)
        .collect();
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find_map(|(provider, id)| {
            eligible
                .iter()
                .copied()
                .find(|m| m.provider == *provider && m.id == *id)
        })
        .or(eligible.first().copied())
        .cloned()
}

/// `findInitialModel` without CLI flags (steps 2-5): the first scoped model (not when continuing) with its level,
/// else the saved default when its provider has credentials, else an available model preferring a known provider's
/// default. `models` is every model (`getModel`); `available` the ones with credentials. Claude Code CLI models are
/// never a fallback.
pub fn find_initial_model(
    scoped: &[ScopedModel],
    is_continuing: bool,
    settings: &InitialSettings,
    models: &[Model],
    available: &[Model],
    has_auth: &dyn Fn(&str) -> bool,
) -> (Option<Model>, String) {
    if let Some(first) = scoped.first().filter(|_| !is_continuing) {
        let thinking = resolve_thinking_level(
            first.thinking_level.as_deref(),
            settings,
            &first.model.provider,
            &first.model.id,
        );
        return (Some(first.model.clone()), thinking);
    }
    if let (Some(provider), Some(id)) = (&settings.default_provider, &settings.default_model_id)
        && let Some(found) = models.iter().find(|m| &m.provider == provider && &m.id == id)
        && has_auth(&found.provider)
    {
        let thinking = resolve_thinking_level(None, settings, provider, id);
        return (Some(found.clone()), thinking);
    }
    (
        pick_fallback_model(available),
        model_scope::DEFAULT_THINKING_LEVEL.into(),
    )
}
