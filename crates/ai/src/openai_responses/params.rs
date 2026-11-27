use super::messages::{
    ConvertOptions, OPENAI_TOOL_CALL_PROVIDERS, ToolOptions, convert_responses_messages, convert_responses_tools,
};
use super::{Compat, OpenAIResponsesOptions};
use crate::options::StreamOptions;
use crate::transcript::resolve_transcript_tools;
use crate::types::{CacheRetention, Mapped, Model, TranscriptContext};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

const MIN_OUTPUT_TOKENS: u64 = 16;

/// `api/openai-prompt-cache.ts`: an OpenAI prompt cache key is at most 64 characters (code points).
pub(crate) fn clamp_prompt_cache_key(key: &str) -> String {
    key.chars().take(64).collect()
}

fn apply_prompt_cache_params(params: &mut Map<String, Value>, base: &StreamOptions, compat: &Compat) {
    let retention = base.cache_retention();
    if retention != CacheRetention::None
        && let Some(session) = &base.session_id
    {
        params.insert("prompt_cache_key".into(), json!(clamp_prompt_cache_key(session)));
    }
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    if long && !compat.supports_explicit_prompt_cache_mode {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_explicit_prompt_cache_mode {
        if retention == CacheRetention::None {
            params.insert("prompt_cache_options".into(), json!({"mode": "explicit"}));
        } else if long {
            params.insert("prompt_cache_options".into(), json!({"ttl": "30m"}));
        }
    }
}

pub(crate) fn reasoning_off_effort(model: &Model) -> &str {
    match model.thinking_level("off") {
        Mapped::Value(val) => val,
        _ => "none",
    }
}

pub(crate) fn apply_reasoning_params(
    params: &mut Map<String, Value>,
    model: &Model,
    reasoning_effort: Option<&str>,
    reasoning_summary: Option<&str>,
    allow_off_effort: bool,
) {
    if !model.reasoning {
        return;
    }
    let effort = reasoning_effort.filter(|level| !level.is_empty());
    let summary = reasoning_summary.filter(|text| !text.is_empty());
    if effort.is_some() || summary.is_some() {
        let effort = effort.map_or("medium", |level| model.mapped_or(level));
        params.insert(
            "reasoning".into(),
            json!({"effort": effort, "summary": summary.unwrap_or("auto")}),
        );
        params.insert("include".into(), json!(["reasoning.encrypted_content"]));
    } else if allow_off_effort && model.thinking_level("off") != Mapped::Unsupported {
        params.insert("reasoning".into(), json!({"effort": reasoning_off_effort(model)}));
    }
}

fn apply_optional_params(
    params: &mut Map<String, Value>,
    options: &OpenAIResponsesOptions,
    compat: &Compat,
    request_tools: &[crate::types::Tool],
    tool_options: ToolOptions,
) -> Result<(), String> {
    let base = &options.base;
    if let Some(max_tokens) = base.max_tokens.filter(|tokens| *tokens > 0)
        && compat.supports_max_output_tokens
    {
        params.insert("max_output_tokens".into(), json!(max_tokens.max(MIN_OUTPUT_TOKENS)));
    }
    if let Some(temperature) = base.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
    if let Some(tier) = &options.service_tier {
        params.insert("service_tier".into(), json!(tier));
    }
    if !request_tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_responses_tools(request_tools, tool_options)?),
        );
    }
    if let Some(choice) = &options.tool_choice {
        params.insert("tool_choice".into(), choice.clone());
    }
    Ok(())
}

pub(super) fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAIResponsesOptions,
    compat: &Compat,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let base = &options.base;
    let tools = resolve_transcript_tools(
        &context.messages,
        compat.supports_additional_tools || compat.supports_tool_search,
    );
    let tool_options = ToolOptions {
        null_strict: false,
        supports_strict_mode: compat.supports_strict_mode,
        supports_openai_grammar_tools: compat.supports_openai_grammar_tools,
        tool_search_result: false,
    };
    let input = convert_responses_messages(
        model,
        context,
        &ConvertOptions {
            tool_call_providers: OPENAI_TOOL_CALL_PROVIDERS,
            include_system_prompt: true,
            grammar,
            supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
            supports_additional_tools: compat.supports_additional_tools,
            supports_tool_search: compat.supports_tool_search,
            tool_options,
        },
        now,
    )?;
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("input".into(), Value::Array(input));
    params.insert("stream".into(), json!(true));
    apply_prompt_cache_params(&mut params, base, compat);
    params.insert("store".into(), json!(false));
    apply_optional_params(&mut params, options, compat, &tools.request_tools, tool_options)?;
    apply_reasoning_params(
        &mut params,
        model,
        options.reasoning_effort.as_deref(),
        options.reasoning_summary.as_deref(),
        model.provider != "github-copilot",
    );
    if model.reasoning && model.provider == "xai" {
        params.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    if let Some(sampling) = &base.sampling_params {
        params.extend(sampling.clone());
    }
    Ok(params)
}
