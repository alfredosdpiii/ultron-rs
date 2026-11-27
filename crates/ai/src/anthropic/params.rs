//! Request parameter and tool schema construction for Anthropic Messages requests.

use super::headers::beta_features;
use super::messages::{convert_messages, to_claude_code_name};
use super::{AnthropicOptions, Compat, fallback_models};
use crate::constrained::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::options::StreamOptions;
use crate::transcript::{
    current_tools, declared_tools, has_tool_redefinitions, initial_system_message, system_message_text,
    transform_messages,
};
use crate::types::{AssistantMessage, CacheRetention, Mapped, Message, Model, Tool, TranscriptContext};
use serde_json::{Map, Value, json};

fn cache_control(model: &Model, options: &StreamOptions) -> Option<Value> {
    match options.cache_retention() {
        CacheRetention::None => None,
        CacheRetention::Long if Compat::of(model).long_cache_retention => {
            Some(json!({"type": "ephemeral", "ttl": "1h"}))
        }
        _ => Some(json!({"type": "ephemeral"})),
    }
}

fn empty_json_object() -> Value {
    Value::Object(Map::new())
}

fn empty_json_array() -> Value {
    Value::Array(Vec::new())
}

fn tool_input_schema(parameters: &Value, strict: Option<bool>) -> Map<String, Value> {
    let mut legacy = Map::new();
    legacy.insert("type".into(), json!("object"));
    legacy.insert(
        "properties".into(),
        parameters
            .get("properties")
            .filter(|p| !p.is_null())
            .cloned()
            .unwrap_or_else(empty_json_object),
    );
    legacy.insert(
        "required".into(),
        parameters
            .get("required")
            .filter(|p| !p.is_null())
            .cloned()
            .unwrap_or_else(empty_json_array),
    );
    if strict == Some(true) {
        let mut merged = parameters.as_object().cloned().unwrap_or_default();
        merged.extend(legacy);
        merged
    } else {
        legacy
    }
}

fn convert_tool(
    tool: &Tool,
    oauth: bool,
    eager: bool,
    strict_tools: bool,
    cache_control: Option<&Value>,
) -> Result<Value, String> {
    let strict = resolve_json_schema_strict_sampling(tool, strict_tools)?;
    let parameters = json_schema_tool_parameters(tool, strict)?;
    let input_schema = tool_input_schema(&parameters, strict);
    let mut converted = Map::new();
    converted.insert(
        "name".into(),
        json!(if oauth {
            to_claude_code_name(&tool.name)
        } else {
            tool.name.clone()
        }),
    );
    converted.insert("description".into(), json!(tool.description));
    if eager {
        converted.insert("eager_input_streaming".into(), json!(true));
    }
    if strict == Some(true) {
        converted.insert("strict".into(), json!(true));
    }
    converted.insert("input_schema".into(), Value::Object(input_schema));
    if let Some(cache_control) = cache_control {
        converted.insert("cache_control".into(), cache_control.clone());
    }
    Ok(Value::Object(converted))
}

fn convert_tools(
    tools: &[Tool],
    oauth: bool,
    eager: bool,
    strict_tools: bool,
    cache_control: Option<&Value>,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let tool_cache = cache_control.filter(|_| index + 1 == tools.len());
        out.push(convert_tool(tool, oauth, eager, strict_tools, tool_cache)?);
    }
    Ok(out)
}

fn normalize_tool_call_id(id: &str) -> String {
    let replaced: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    replaced.chars().take(64).collect()
}

fn normalize_assistant_tool_call_id(id: &str, _: &AssistantMessage) -> String {
    normalize_tool_call_id(id)
}

fn deferred_tool_placeholder() -> Value {
    json!({
        "name": "__pi_deferred_placeholder__",
        "description": "Reserved placeholder. Never available. Never call this.",
        "input_schema": {"type": "object", "properties": {}, "required": []},
        "defer_loading": true,
    })
}

fn has_native_tool_changes(compat: &Compat, initial_tools: &[Tool], messages: &[Message]) -> bool {
    compat.mid_convo_system_messages
        && compat.mid_convo_tool_changes
        && !initial_tools.is_empty()
        && !has_tool_redefinitions(messages)
}

fn interleave_effort_messages(
    converted: Vec<Value>,
    levels: &[(usize, String)],
    active_effort: &str,
    managed_effort: bool,
) -> Vec<Value> {
    if !managed_effort {
        return converted;
    }
    let mut with_levels = Vec::with_capacity(converted.len() + levels.len() + 1);
    for (index, message) in converted.into_iter().enumerate() {
        if let Some((_, effort)) = levels.iter().find(|(i, _)| *i == index) {
            with_levels.push(json!({"role": "system", "content": [], "output_config": {"effort": effort}}));
        }
        with_levels.push(message);
    }
    with_levels.push(json!({"role": "system", "content": [], "output_config": {"effort": active_effort}}));
    with_levels
}

fn system_text_block(text: &str, cache_control: Option<&Value>) -> Value {
    let mut block = Map::new();
    block.insert("type".into(), json!("text"));
    block.insert("text".into(), json!(text));
    if let Some(cache_control) = cache_control {
        block.insert("cache_control".into(), cache_control.clone());
    }
    Value::Object(block)
}

fn build_system_prompt(oauth: bool, initial_text: &str, cache_control: Option<&Value>) -> Option<Value> {
    if oauth {
        let mut system = vec![system_text_block(
            "You are Claude Code, Anthropic's official CLI for Claude.",
            cache_control,
        )];
        if !initial_text.is_empty() {
            system.push(system_text_block(initial_text, cache_control));
        }
        Some(Value::Array(system))
    } else if !initial_text.is_empty() {
        Some(json!([system_text_block(initial_text, cache_control)]))
    } else {
        None
    }
}

fn insert_temperature_param(params: &mut Map<String, Value>, compat: &Compat, options: &AnthropicOptions) {
    if let Some(temperature) = options.base.temperature
        && options.thinking_enabled != Some(true)
        && !compat.managed_effort
        && compat.temperature
    {
        params.insert("temperature".into(), json!(temperature));
    }
}

fn build_tools_param(
    messages: &[Message],
    initial_tools: &[Tool],
    oauth: bool,
    compat: &Compat,
    native_tool_changes: bool,
    tool_cache_control: Option<&Value>,
) -> Result<Option<Value>, String> {
    if native_tool_changes {
        let initial_names: Vec<&str> = initial_tools.iter().map(|t| t.name.as_str()).collect();
        let later: Vec<Tool> = declared_tools(messages)
            .into_iter()
            .filter(|t| !initial_names.contains(&t.name.as_str()))
            .collect();
        let mut tools = convert_tools(
            initial_tools,
            oauth,
            compat.eager_tool_input_streaming,
            compat.strict_tools,
            tool_cache_control,
        )?;
        tools.push(deferred_tool_placeholder());
        for mut tool in convert_tools(
            &later,
            oauth,
            compat.eager_tool_input_streaming,
            compat.strict_tools,
            None,
        )? {
            tool["defer_loading"] = json!(true);
            tools.push(tool);
        }
        return Ok(Some(Value::Array(tools)));
    }
    let tools = current_tools(messages);
    if tools.is_empty() {
        return Ok(None);
    }
    let converted = convert_tools(
        &tools,
        oauth,
        compat.eager_tool_input_streaming,
        compat.strict_tools,
        tool_cache_control,
    )?;
    Ok(Some(Value::Array(converted)))
}

fn insert_thinking_params(params: &mut Map<String, Value>, model: &Model, compat: &Compat, options: &AnthropicOptions) {
    let display = options.thinking_display.as_deref().unwrap_or("summarized");
    if compat.managed_effort {
        params.insert(
            "thinking".into(),
            json!({"type": "adaptive", "display": display, "block_binding": {"prefix_mismatch_behavior": "drop_block"}}),
        );
        params.insert("output_config".into(), json!({"effort": "high"}));
    } else if model.reasoning {
        insert_model_thinking_params(params, model, compat, options, display);
    }
}

fn insert_model_thinking_params(
    params: &mut Map<String, Value>,
    model: &Model,
    compat: &Compat,
    options: &AnthropicOptions,
    display: &str,
) {
    if options.thinking_enabled == Some(true) {
        if compat.force_adaptive {
            params.insert("thinking".into(), json!({"type": "adaptive", "display": display}));
            if let Some(effort) = options.effort.as_deref().filter(|e| !e.is_empty()) {
                params.insert("output_config".into(), json!({"effort": effort}));
            }
        } else {
            let budget = options.thinking_budget_tokens.filter(|b| *b > 0).unwrap_or(1024);
            params.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget, "display": display}),
            );
        }
    } else if options.thinking_enabled == Some(false) && model.thinking_level("off") != Mapped::Unsupported {
        params.insert("thinking".into(), json!({"type": "disabled"}));
    }
}

fn insert_optional_params(params: &mut Map<String, Value>, model: &Model, options: &AnthropicOptions) {
    if let Some(Value::String(user_id)) = options.base.metadata.as_ref().and_then(|m| m.get("user_id")) {
        params.insert("metadata".into(), json!({"user_id": user_id}));
    }
    match &options.tool_choice {
        Some(Value::String(choice)) if !choice.is_empty() => {
            params.insert("tool_choice".into(), json!({"type": choice}));
        }
        Some(choice @ Value::Object(_)) => {
            params.insert("tool_choice".into(), choice.clone());
        }
        _ => {}
    }
    let fallbacks = fallback_models(model);
    if !fallbacks.is_empty() {
        let models: Vec<Value> = fallbacks
            .iter()
            .map(|f| json!({"model": f.get("model").cloned().unwrap_or(Value::Null)}))
            .collect();
        params.insert("fallbacks".into(), Value::Array(models));
    }
}

/// The request parameters (`buildParams`), `betas` included as upstream passes them to the SDK.
pub(super) fn build_params(
    model: &Model,
    context: &TranscriptContext,
    oauth: bool,
    options: &AnthropicOptions,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let cache_control = cache_control(model, &options.base);
    let compat = Compat::of(model);
    let initial = initial_system_message(&context.messages);
    let initial_text = initial.map_or_default(system_message_text);
    let normalize = normalize_assistant_tool_call_id;
    let transformed = transform_messages(&context.messages, model, Some(&normalize), now);
    let conversation = if initial.is_some() {
        &transformed[1..]
    } else {
        &transformed[..]
    };
    let initial_tools = initial.and_then(|m| m.tools_added.clone()).unwrap_or_default();
    let native_tool_changes = has_native_tool_changes(&compat, &initial_tools, &context.messages);
    let (converted, levels) = convert_messages(
        conversation,
        oauth,
        cache_control.as_ref(),
        compat.allow_empty_signature,
        compat.managed_effort.then_some(model.provider.as_str()),
        native_tool_changes,
    );
    let active_effort = options.effort.as_deref().unwrap_or("high");
    let betas = beta_features(model, context, oauth, native_tool_changes, options);
    let messages = interleave_effort_messages(converted, &levels, active_effort, compat.managed_effort);

    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), Value::Array(messages));
    params.insert(
        "max_tokens".into(),
        json!(options.base.max_tokens.unwrap_or(model.max_tokens)),
    );
    params.insert("stream".into(), json!(true));
    if !betas.is_empty() {
        params.insert("betas".into(), json!(betas));
    }
    if let Some(system) = build_system_prompt(oauth, &initial_text, cache_control.as_ref()) {
        params.insert("system".into(), system);
    }
    insert_temperature_param(&mut params, &compat, options);
    let tool_cache_control = cache_control.as_ref().filter(|_| compat.cache_control_on_tools);
    if let Some(tools) = build_tools_param(
        &context.messages,
        &initial_tools,
        oauth,
        &compat,
        native_tool_changes,
        tool_cache_control,
    )? {
        params.insert("tools".into(), tools);
    }
    insert_thinking_params(&mut params, model, &compat, options);
    insert_optional_params(&mut params, model, options);
    Ok(params)
}
