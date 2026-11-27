use super::OpenAICompletionsOptions;
use super::compat::Compat;
use super::messages::{convert_messages, convert_tools, has_tool_history};
use crate::options::{StreamOptions, clamp_thinking_budget_to_answer_room, thinking_budget_for_level};
use crate::transcript::resolve_transcript_tools;
use crate::types::{CacheRetention, Mapped, Message, Model, Tool, TranscriptContext};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

/// Mark the last text part (or a non-empty string) of `message` with `cache_control`.
fn add_cache_control_to_text(message: &mut Value, cache_control: &Value) -> bool {
    match message.get_mut("content") {
        Some(content @ Value::String(_)) => {
            let text = content.as_str().unwrap_or_default().to_string();
            if text.is_empty() {
                return false;
            }
            *content = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
            true
        }
        Some(Value::Array(parts)) => match parts
            .iter_mut()
            .rev()
            .find(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        {
            Some(part) => {
                part["cache_control"] = cache_control.clone();
                true
            }
            None => false,
        },
        _ => false,
    }
}

fn message_role(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or_default()
}

fn apply_anthropic_cache_control(messages: &mut [Value], tools: Option<&mut Value>, cache_control: &Value) {
    if let Some(system) = messages
        .iter_mut()
        .find(|message| matches!(message_role(message), "system" | "developer"))
    {
        add_cache_control_to_text(system, cache_control);
    }
    if let Some(Value::Array(tools)) = tools
        && let Some(last) = tools.last_mut()
    {
        last["cache_control"] = cache_control.clone();
    }
    for message in messages.iter_mut().rev() {
        if matches!(message_role(message), "user" | "assistant" | "tool")
            && add_cache_control_to_text(message, cache_control)
        {
            return;
        }
    }
}

fn resolve_template_entry(
    model: &Model,
    effort: Option<&str>,
    spec: &Map<String, Value>,
    budget: Option<u64>,
) -> Option<Value> {
    if effort.is_none() && spec.get("omitWhenOff").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match spec.get("$var").and_then(Value::as_str) {
        Some("thinking.enabled") => Some(json!(effort.is_some())),
        Some("thinking.budget") => budget.map(|val| json!(val)),
        _ => match model.thinking_level(effort.unwrap_or("off")) {
            Mapped::Missing => effort.map(|level| json!(level)),
            Mapped::Value(mapped) => Some(json!(mapped)),
            Mapped::Unsupported => None,
        },
    }
}

fn chat_template_values(
    model: &Model,
    effort: Option<&str>,
    values: &Map<String, Value>,
    budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let mut resolved = Map::new();
    for (key, value) in values {
        let Some(spec) = value.as_object() else {
            resolved.insert(key.clone(), value.clone());
            continue;
        };
        if let Some(resolved_value) = resolve_template_entry(model, effort, spec, budget) {
            resolved.insert(key.clone(), resolved_value);
        }
    }
    (!resolved.is_empty()).then_some(resolved)
}

fn build_cache_control(compat: &Compat, retention: CacheRetention) -> Option<Value> {
    if compat.cache_control_format.as_deref() != Some("anthropic") || retention == CacheRetention::None {
        return None;
    }
    if retention == CacheRetention::Long && compat.supports_long_cache_retention {
        Some(json!({"type": "ephemeral", "ttl": "1h"}))
    } else {
        Some(json!({"type": "ephemeral"}))
    }
}

fn resolve_tools_value(messages: &[Message], request_tools: &[Tool], compat: &Compat) -> Result<Option<Value>, String> {
    if !request_tools.is_empty() {
        return Ok(Some(Value::Array(convert_tools(request_tools, compat)?)));
    }
    if has_tool_history(messages) {
        return Ok(Some(json!([])));
    }
    Ok(None)
}

fn insert_prompt_cache_params(
    params: &mut Map<String, Value>,
    model: &Model,
    base: &StreamOptions,
    retention: CacheRetention,
    long: bool,
) {
    let openai_cache = model.base_url.contains("api.openai.com") && retention != CacheRetention::None;
    if (openai_cache || long)
        && let Some(session) = &base.session_id
    {
        params.insert(
            "prompt_cache_key".into(),
            json!(session.chars().take(64).collect::<String>()),
        );
    }
    if long {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }
}

fn insert_request_tuning_params(params: &mut Map<String, Value>, base: &StreamOptions, compat: &Compat) {
    if compat.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        params.insert("store".into(), json!(false));
    }
    if let Some(max_tokens) = base.max_tokens.filter(|tokens| *tokens > 0) {
        let field = if compat.max_tokens_field == "max_tokens" {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        params.insert(field.into(), json!(max_tokens));
    }
    if let Some(temperature) = base.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
}

fn insert_tools_and_priority(
    params: &mut Map<String, Value>,
    tools_value: Option<Value>,
    options: &OpenAICompletionsOptions,
    compat: &Compat,
) {
    if let Some(tools_value) = tools_value {
        let has_tools = tools_value.as_array().is_some_and(|tools| !tools.is_empty());
        params.insert("tools".into(), tools_value);
        if has_tools && compat.zai_tool_stream {
            params.insert("tool_stream".into(), json!(true));
        }
    }
    if let Some(choice) = options
        .tool_choice
        .as_ref()
        .filter(|choice| jsrt::value::truthy(Some(choice)))
    {
        params.insert("tool_choice".into(), choice.clone());
    }
    if let Some(priority) = &compat.vllm_priority {
        params.insert("priority".into(), priority.clone());
    }
}

fn compute_thinking_budget(
    params: &Map<String, Value>,
    model: &Model,
    options: &OpenAICompletionsOptions,
    effort: Option<&str>,
) -> Option<u64> {
    let effort = effort.filter(|_| model.reasoning)?;
    let ceiling = params
        .get("max_tokens")
        .or_else(|| params.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(model.max_tokens);
    let clamped = clamp_thinking_budget_to_answer_room(
        thinking_budget_for_level(effort, options.thinking_budgets.as_ref()),
        ceiling,
    );
    (clamped > 0).then_some(clamped)
}

fn off_or_none(model: &Model) -> String {
    match model.thinking_level("off") {
        Mapped::Value(val) => val.to_string(),
        _ => "none".to_string(),
    }
}

fn insert_effort_if_supported(params: &mut Map<String, Value>, model: &Model, compat: &Compat, effort: Option<&str>) {
    if let (Some(effort), true) = (effort, compat.supports_reasoning_effort) {
        params.insert("reasoning_effort".into(), json!(model.mapped_or(effort)));
    }
}

fn insert_zai_thinking(params: &mut Map<String, Value>, model: &Model, compat: &Compat, effort: Option<&str>) {
    params.insert(
        "thinking".into(),
        if effort.is_some() {
            json!({"type": "enabled", "clear_thinking": false})
        } else {
            json!({"type": "disabled"})
        },
    );
    if let (Some(effort), true) = (effort, compat.supports_reasoning_effort) {
        match model.thinking_level(effort) {
            Mapped::Missing => {
                params.insert("reasoning_effort".into(), json!(effort));
            }
            Mapped::Value(mapped) => {
                params.insert("reasoning_effort".into(), json!(mapped));
            }
            Mapped::Unsupported => {}
        }
    }
}

fn insert_baseten_thinking(
    params: &mut Map<String, Value>,
    model: &Model,
    compat: &Compat,
    effort: Option<&str>,
    budget: Option<u64>,
) {
    if let Some(values) = chat_template_values(model, effort, &compat.chat_template_args, budget) {
        params.insert("chat_template_args".into(), Value::Object(values));
    }
    if compat.supports_reasoning_effort {
        let value = match model.thinking_level(effort.unwrap_or("off")) {
            Mapped::Missing => effort.map(str::to_string),
            Mapped::Value(mapped) => Some(mapped.to_string()),
            Mapped::Unsupported => None,
        };
        if let Some(value) = value {
            params.insert("reasoning_effort".into(), json!(value));
        }
    }
}

fn insert_provider_thinking(
    params: &mut Map<String, Value>,
    model: &Model,
    compat: &Compat,
    effort: Option<&str>,
    budget: Option<u64>,
) -> bool {
    match compat.thinking_format.as_str() {
        "zai" => insert_zai_thinking(params, model, compat, effort),
        "qwen" => {
            params.insert("enable_thinking".into(), json!(effort.is_some()));
            insert_effort_if_supported(params, model, compat, effort);
        }
        "qwen-chat-template" => {
            params.insert(
                "chat_template_kwargs".into(),
                json!({"enable_thinking": effort.is_some(), "preserve_thinking": true}),
            );
        }
        "chat-template" => {
            if let Some(values) = chat_template_values(model, effort, &compat.chat_template_kwargs, budget) {
                params.insert("chat_template_kwargs".into(), Value::Object(values));
            }
        }
        "baseten" => insert_baseten_thinking(params, model, compat, effort, budget),
        _ => return false,
    }
    true
}

fn insert_deepseek_thinking(params: &mut Map<String, Value>, model: &Model, compat: &Compat, effort: Option<&str>) {
    if effort.is_some() {
        params.insert("thinking".into(), json!({"type": "enabled"}));
    } else if model.thinking_level("off") != Mapped::Unsupported {
        params.insert("thinking".into(), json!({"type": "disabled"}));
    }
    insert_effort_if_supported(params, model, compat, effort);
}

fn insert_default_thinking(params: &mut Map<String, Value>, model: &Model, compat: &Compat, effort: Option<&str>) {
    if !compat.supports_reasoning_effort {
        return;
    }
    if let Some(effort) = effort {
        params.insert("reasoning_effort".into(), json!(model.mapped_or(effort)));
    } else if let Mapped::Value(off) = model.thinking_level("off") {
        params.insert("reasoning_effort".into(), json!(off));
    }
}

fn insert_openrouter_thinking(params: &mut Map<String, Value>, model: &Model, effort: Option<&str>) {
    if let Some(effort) = effort {
        params.insert("reasoning".into(), json!({"effort": model.mapped_or(effort)}));
    } else if model.thinking_level("off") != Mapped::Unsupported {
        params.insert("reasoning".into(), json!({"effort": off_or_none(model)}));
    }
}

fn insert_ant_ling_thinking(params: &mut Map<String, Value>, model: &Model, effort: Option<&str>) {
    if let Some(effort) = effort
        && let Mapped::Value(mapped) = model.thinking_level(effort)
    {
        params.insert("reasoning".into(), json!({"effort": mapped}));
    }
}

fn insert_string_thinking(params: &mut Map<String, Value>, model: &Model, effort: Option<&str>) {
    if let Some(effort) = effort {
        params.insert("thinking".into(), json!(model.mapped_or(effort)));
    } else if model.thinking_level("off") != Mapped::Unsupported {
        params.insert("thinking".into(), json!(off_or_none(model)));
    }
}

fn insert_standard_thinking(params: &mut Map<String, Value>, model: &Model, compat: &Compat, effort: Option<&str>) {
    match compat.thinking_format.as_str() {
        "deepseek" => insert_deepseek_thinking(params, model, compat, effort),
        "openrouter" => insert_openrouter_thinking(params, model, effort),
        "ant-ling" => insert_ant_ling_thinking(params, model, effort),
        "together" => {
            params.insert("reasoning".into(), json!({"enabled": effort.is_some()}));
            insert_effort_if_supported(params, model, compat, effort);
        }
        "string-thinking" => insert_string_thinking(params, model, effort),
        _ => insert_default_thinking(params, model, compat, effort),
    }
}

fn insert_thinking_params(
    params: &mut Map<String, Value>,
    model: &Model,
    compat: &Compat,
    effort: Option<&str>,
    budget: Option<u64>,
) {
    if !model.reasoning {
        return;
    }
    if !insert_provider_thinking(params, model, compat, effort, budget) {
        insert_standard_thinking(params, model, compat, effort);
    }
}

fn insert_routing_and_sampling(params: &mut Map<String, Value>, model: &Model, base: &StreamOptions) {
    if let Some(routing) = model.compat_value("openRouterRouting") {
        params.insert("provider".into(), routing.clone());
    }
    if let Some(routing) = model.compat_value("vercelGatewayRouting") {
        let mut gateway = Map::new();
        for key in ["only", "order"] {
            if let Some(list) = routing.get(key).filter(|val| !val.is_null()) {
                gateway.insert(key.into(), list.clone());
            }
        }
        if !gateway.is_empty() {
            params.insert("providerOptions".into(), json!({"gateway": gateway}));
        }
    }
    if let Some(sampling) = &base.sampling_params {
        params.extend(sampling.clone());
    }
}

pub(super) fn build_params(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    compat: &Compat,
    retention: CacheRetention,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Map<String, Value>, String> {
    let base = &options.base;
    let tools = resolve_transcript_tools(
        &context.messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let mut messages = convert_messages(model, context, compat, grammar, now)?;
    let cache_control = build_cache_control(compat, retention);
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    let mut params = Map::new();
    params.insert("model".into(), json!(model.id));
    params.insert("messages".into(), Value::Null);
    params.insert("stream".into(), json!(true));
    insert_prompt_cache_params(&mut params, model, base, retention, long);
    insert_request_tuning_params(&mut params, base, compat);
    let mut tools_value = resolve_tools_value(&context.messages, &tools.request_tools, compat)?;
    if let Some(cache_control) = &cache_control {
        apply_anthropic_cache_control(&mut messages, tools_value.as_mut(), cache_control);
    }
    params.insert("messages".into(), Value::Array(messages));
    insert_tools_and_priority(&mut params, tools_value, options, compat);
    let budget_field = compat.thinking_token_budget_field.clone().or_else(|| {
        compat
            .supports_thinking_token_budget
            .then(|| "thinking_token_budget".to_string())
    });
    let effort = options.reasoning_effort.as_deref().filter(|level| !level.is_empty());
    let budget = compute_thinking_budget(&params, model, options, effort);
    insert_thinking_params(&mut params, model, compat, effort, budget);
    if let (Some(field), Some(budget)) = (budget_field, budget) {
        params.insert(field, json!(budget));
    }
    insert_routing_and_sampling(&mut params, model, base);
    Ok(params)
}
