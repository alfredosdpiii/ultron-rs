//! Model patterns as the TypeScript resolver reads them (`coding-agent/src/core/model-resolver.ts`):
//! `findExactModelReferenceMatch`, `parseModelPattern` (exact, then partial id/name matching preferring aliases,
//! `:<thinking>` suffixes peeled off the end) and `resolveModelScopeFromModels` (`--models` / `enabledModels`: globs
//! through minimatch, fuzzy patterns otherwise), over any model record ([`ScopeModel`]). The provider layer
//! (`ai::model_resolver`) and the TUI's Ctrl+P cycling share them.
//!
//! Evidence: `crates/ai/fixtures/generate-model-resolver.mjs` runs the TypeScript resolver;
//! `crates/ai/tests/model_resolver.rs` and `crates/interactive/tests/next_model.rs` replay it.

pub mod glob;

use glob::minimatch;
use std::cmp::Ordering;

/// What the resolver reads of a model.
pub trait ScopeModel {
    fn provider(&self) -> &str;
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    /// Answers only to its full `provider/id` reference, never to a bare id, partial match or id glob (Claude Code
    /// CLI models, `isClaudeCodeModel`; the model's owner says which).
    fn full_reference_only(&self) -> bool;
}

/// `isValidThinkingLevel` (cli/args.ts).
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// `DEFAULT_THINKING_LEVEL` (core/defaults.ts): the level without a setting.
pub const DEFAULT_THINKING_LEVEL: &str = "medium";

pub fn is_valid_thinking_level(level: &str) -> bool {
    THINKING_LEVELS.contains(&level)
}

/// A request payload's `thinking` value: one of [`THINKING_LEVELS`], else "thinking must be one of ...".
pub fn thinking_level(value: &serde_json::Value) -> Result<String, String> {
    match value.as_str().filter(|level| is_valid_thinking_level(level)) {
        Some(level) => Ok(level.to_string()),
        None => Err(format!("thinking must be one of {}", THINKING_LEVELS.join(", "))),
    }
}

/// A request payload's `model` value: a [`is_model_ref`] string, else "model must be provider/model".
pub fn model_ref(value: &serde_json::Value) -> Result<String, String> {
    match value.as_str().filter(|model| is_model_ref(model)) {
        Some(model) => Ok(model.to_string()),
        None => Err("model must be provider/model".into()),
    }
}

/// `MODEL_REF_PATTERN` (`^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$`): `provider/model[/more]`, no part empty or with
/// whitespace.
pub fn is_model_ref(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && !part.chars().any(jsrt::string::is_space))
}

use jsrt::collate::locale_compare;

/// `modelsAreEqual`.
pub fn same<M: ScopeModel>(a: &M, b: &M) -> bool {
    a.id() == b.id() && a.provider() == b.provider()
}

pub fn reference<M: ScopeModel>(model: &M) -> String {
    format!("{}/{}", model.provider(), model.id())
}

enum ExactLookup<'a, M> {
    Found(&'a M),
    Ambiguous,
    None,
}

fn unique_model<'a, M>(models: &'a [M], pred: impl Fn(&M) -> bool) -> ExactLookup<'a, M> {
    let mut found = None;
    for model in models.iter().filter(|m| pred(m)) {
        if found.is_some() {
            return ExactLookup::Ambiguous;
        }
        found = Some(model);
    }
    found.map_or(ExactLookup::None, ExactLookup::Found)
}

fn find_split_reference<'a, M: ScopeModel>(trimmed: &str, models: &'a [M]) -> ExactLookup<'a, M> {
    let Some((provider, id)) = trimmed.split_once('/') else {
        return ExactLookup::None;
    };
    let (provider, id) = (jsrt::string::trim(provider), jsrt::string::trim(id));
    if provider.is_empty() || id.is_empty() {
        return ExactLookup::None;
    }
    let (provider, id) = (provider.to_lowercase(), id.to_lowercase());
    unique_model(models, |m| {
        m.provider().to_lowercase() == provider && m.id().to_lowercase() == id
    })
}

/// `findExactModelReferenceMatch`: `provider/id` (case-insensitive) or a bare id that only one model has.
pub fn find_exact_model_reference_match<'a, M: ScopeModel>(reference_: &str, models: &'a [M]) -> Option<&'a M> {
    let trimmed = jsrt::string::trim(reference_);
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_lowercase();
    match unique_model(models, |m| reference(m).to_lowercase() == normalized) {
        ExactLookup::Found(m) => return Some(m),
        ExactLookup::Ambiguous => return None,
        ExactLookup::None => {}
    }
    match find_split_reference(trimmed, models) {
        ExactLookup::Found(m) => return Some(m),
        ExactLookup::Ambiguous => return None,
        ExactLookup::None => {}
    }
    match unique_model(models, |m| {
        m.id().to_lowercase() == normalized && !m.full_reference_only()
    }) {
        ExactLookup::Found(m) => Some(m),
        _ => None,
    }
}

/// `isAlias`: no `-YYYYMMDD` date suffix (or a `-latest` one).
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    let dated =
        bytes.len() >= 9 && bytes[bytes.len() - 9] == b'-' && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit);
    !dated
}

/// The first model of a stable descending sort by id (`localeCompare`).
fn highest<'a, M: ScopeModel>(models: impl Iterator<Item = &'a M>) -> Option<&'a M> {
    let mut best: Option<&M> = None;
    for model in models {
        if best.is_none_or(|b| locale_compare(model.id(), b.id()) == Ordering::Greater) {
            best = Some(model);
        }
    }
    best
}

/// `tryMatchModel`: an exact reference, else the best partial id/name match (an alias over dated ids).
fn try_match_model<'a, M: ScopeModel>(pattern: &str, models: &'a [M]) -> Option<&'a M> {
    if let Some(exact) = find_exact_model_reference_match(pattern, models) {
        return Some(exact);
    }
    let needle = pattern.to_lowercase();
    let matches: Vec<&M> = models
        .iter()
        .filter(|m| {
            !m.full_reference_only()
                && (m.id().to_lowercase().contains(&needle) || m.name().to_lowercase().contains(&needle))
        })
        .collect();
    highest(matches.iter().copied().filter(|m| is_alias(m.id())))
        .or_else(|| highest(matches.iter().copied().filter(|m| !is_alias(m.id()))))
}

/// `ParsedModelResult`.
#[derive(Debug, PartialEq)]
pub struct Parsed<'a, M> {
    pub model: Option<&'a M>,
    pub thinking_level: Option<String>,
    pub warning: Option<String>,
}

/// `parseModelPattern`: the whole pattern as a model, else the part before the last `:` with that suffix as the
/// thinking level. An invalid suffix warns and is dropped when `allow_invalid_fallback` (scope mode), otherwise it
/// is part of the id and the pattern fails (strict CLI mode).
pub fn parse_model_pattern<'a, M: ScopeModel>(
    pattern: &str,
    models: &'a [M],
    allow_invalid_fallback: bool,
) -> Parsed<'a, M> {
    if let Some(model) = try_match_model(pattern, models) {
        return Parsed {
            model: Some(model),
            thinking_level: None,
            warning: None,
        };
    }
    let none = Parsed {
        model: None,
        thinking_level: None,
        warning: None,
    };
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return none;
    };
    if is_valid_thinking_level(suffix) {
        let result = parse_model_pattern(prefix, models, allow_invalid_fallback);
        if result.model.is_some() {
            let thinking_level = result.warning.is_none().then(|| suffix.to_string());
            return Parsed {
                thinking_level,
                ..result
            };
        }
        return result;
    }
    if !allow_invalid_fallback {
        return none;
    }
    let result = parse_model_pattern(prefix, models, allow_invalid_fallback);
    if result.model.is_some() {
        return Parsed {
            model: result.model,
            thinking_level: None,
            warning: Some(format!(
                "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
            )),
        };
    }
    result
}

/// A model a pattern selected, with the thinking level the pattern named.
#[derive(Debug, PartialEq)]
pub struct Scoped<'a, M> {
    pub model: &'a M,
    pub thinking_level: Option<String>,
}

/// `ModelScopeDiagnostic`: a warning about one pattern (`code` is `no-match` or `invalid-thinking-level`).
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub pattern: String,
}

fn push_scoped<'a, M: ScopeModel>(scoped: &mut Vec<Scoped<'a, M>>, model: &'a M, thinking: Option<String>) {
    if !scoped.iter().any(|s| same(s.model, model)) {
        scoped.push(Scoped {
            model,
            thinking_level: thinking,
        });
    }
}

fn no_match_diagnostic(pattern: &str) -> ScopeDiagnostic {
    ScopeDiagnostic {
        code: "no-match",
        message: format!("No models match pattern \"{pattern}\""),
        pattern: pattern.to_string(),
    }
}

fn resolve_glob_pattern<'a, M: ScopeModel>(
    pattern: &str,
    models: &'a [M],
    scoped: &mut Vec<Scoped<'a, M>>,
    diagnostics: &mut Vec<ScopeDiagnostic>,
) {
    let (glob, thinking) = match pattern.rsplit_once(':') {
        Some((head, suffix)) if is_valid_thinking_level(suffix) => (head, Some(suffix.to_string())),
        _ => (pattern, None),
    };
    if let Some(exact) = find_exact_model_reference_match(glob, models) {
        push_scoped(scoped, exact, thinking);
        return;
    }
    let matching: Vec<&M> = models
        .iter()
        .filter(|m| {
            minimatch(&reference(*m), glob, true) || (!m.full_reference_only() && minimatch(m.id(), glob, true))
        })
        .collect();
    if matching.is_empty() {
        diagnostics.push(no_match_diagnostic(pattern));
        return;
    }
    for model in matching {
        push_scoped(scoped, model, thinking.clone());
    }
}

fn resolve_fuzzy_pattern<'a, M: ScopeModel>(
    pattern: &str,
    models: &'a [M],
    scoped: &mut Vec<Scoped<'a, M>>,
    diagnostics: &mut Vec<ScopeDiagnostic>,
) {
    let parsed = parse_model_pattern(pattern, models, true);
    if let Some(warning) = parsed.warning {
        diagnostics.push(ScopeDiagnostic {
            code: "invalid-thinking-level",
            message: warning,
            pattern: pattern.to_string(),
        });
    }
    match parsed.model {
        Some(model) => push_scoped(scoped, model, parsed.thinking_level),
        None => diagnostics.push(no_match_diagnostic(pattern)),
    }
}

/// `resolveModelScopeFromModels`: the models `patterns` select, in pattern order without duplicates, and the
/// warnings for patterns that select nothing or name an invalid thinking level.
pub fn resolve_model_scope<'a, M: ScopeModel>(
    patterns: &[String],
    models: &'a [M],
) -> (Vec<Scoped<'a, M>>, Vec<ScopeDiagnostic>) {
    let mut scoped: Vec<Scoped<'a, M>> = Vec::new();
    let mut diagnostics = Vec::new();
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            resolve_glob_pattern(pattern, models, &mut scoped, &mut diagnostics);
        } else {
            resolve_fuzzy_pattern(pattern, models, &mut scoped, &mut diagnostics);
        }
    }
    (scoped, diagnostics)
}
