//! Request payload construction, message/tool conversion, and model capability queries for Amazon Bedrock.

use super::BedrockOptions;
use super::target::configured_region;
use crate::constrained::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::env::{ProviderEnv, provider_env_value};
use crate::options::StreamOptions;
use crate::transcript::{current_tools, initial_system_message, system_message_text, transform_messages};
use crate::types::{
    AssistantContent, AssistantMessage, CacheRetention, Mapped, Message, Model, ThinkingContent, Tool,
    TranscriptContext, UserContent, UserMessage, UserMessageContent,
};
use serde_json::{Map, Value, json};

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

/// The JSON body in `ConverseStreamRequest` member order.
pub(super) const REQUEST_MEMBERS: &[&str] = &[
    "messages",
    "system",
    "inferenceConfig",
    "toolConfig",
    "guardrailConfig",
    "additionalModelRequestFields",
    "promptVariables",
    "additionalModelResponseFieldPaths",
    "requestMetadata",
    "performanceConfig",
    "serviceTier",
    "outputConfig",
];

fn match_candidates(model: &Model) -> Vec<String> {
    let mut values = vec![model.id.as_str()];
    if !model.name.is_empty() {
        values.push(&model.name);
    }
    values
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let mut dashed = String::new();
            let mut in_run = false;
            for c in lower.chars() {
                if c.is_whitespace() || matches!(c, '_' | '.' | ':') {
                    if !in_run {
                        dashed.push('-');
                    }
                    in_run = true;
                } else {
                    dashed.push(c);
                    in_run = false;
                }
            }
            [lower, dashed]
        })
        .collect()
}

fn any_candidate(model: &Model, needles: &[&str]) -> bool {
    match_candidates(model)
        .iter()
        .any(|c| needles.iter().any(|n| c.contains(n)))
}

pub fn supports_adaptive_thinking(model: &Model) -> bool {
    any_candidate(
        model,
        &[
            "opus-4-6",
            "opus-4-7",
            "opus-4-8",
            "opus-5",
            "sonnet-4-6",
            "sonnet-5",
            "haiku-5",
            "fable-5",
        ],
    )
}

fn supports_native_xhigh_effort(model: &Model) -> bool {
    any_candidate(
        model,
        &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5"],
    )
}

pub fn is_anthropic_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

pub fn supports_prompt_caching(model: &Model, env: Option<&ProviderEnv>) -> bool {
    let candidates = match_candidates(model);
    if !candidates.iter().any(|c| c.contains("claude")) {
        return provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    candidates.iter().any(|c| {
        c.contains("fable-5")
            || c.contains("opus-5")
            || c.contains("sonnet-5")
            || c.contains("haiku-5")
            || c.contains("-4-")
            || c.contains("claude-3-7-sonnet")
            || c.contains("claude-3-5-haiku")
    })
}

fn effort_for(model: &Model, level: &str) -> String {
    if level == "xhigh" && supports_native_xhigh_effort(model) {
        return "xhigh".into();
    }
    if let Mapped::Value(mapped) = model.thinking_level(level) {
        return mapped.to_string();
    }
    match level {
        "minimal" | "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
    .into()
}

fn cache_point(retention: CacheRetention) -> Value {
    if retention == CacheRetention::Long {
        json!({"cachePoint": {"type": "default", "ttl": "1h"}})
    } else {
        json!({"cachePoint": {"type": "default"}})
    }
}

fn normalize_tool_call_id(id: &str, _source: &AssistantMessage) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let units: Vec<u16> = sanitized.encode_utf16().collect();
    if units.len() > 64 {
        String::from_utf16_lossy(&units[..64])
    } else {
        sanitized
    }
}

fn non_blank_text(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"text": text}))
}

fn required_text(text: &str) -> Value {
    match non_blank_text(text) {
        Some(value) => value,
        None => json!({"text": EMPTY_TEXT_PLACEHOLDER}),
    }
}

/// `atob` failure (`DOMException: The string to be decoded is not correctly encoded.`).
fn base64_bytes(data: &str) -> Result<Vec<u8>, String> {
    jsrt::base64::atob(data).ok_or_else(|| "The string to be decoded is not correctly encoded.".to_string())
}

fn image_block(mime_type: &str, data: &str) -> Result<Value, String> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unknown image type: {other}")),
    };
    Ok(json!({"format": format, "source": {"bytes": jsrt::base64::encode(&base64_bytes(data)?)}}))
}

fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.clone(), sanitize_document(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn tool_result_content(content: &[UserContent]) -> Result<Vec<Value>, String> {
    let mut result = Vec::new();
    for block in content {
        match block {
            UserContent::Image(image) => result.push(json!({"image": image_block(&image.mime_type, &image.data)?})),
            UserContent::Text(text) => result.extend(non_blank_text(&text.text)),
        }
    }
    if result.is_empty() {
        result.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    Ok(result)
}

fn convert_user_blocks(blocks: &[UserContent]) -> Result<Vec<Value>, String> {
    let mut content = Vec::new();
    for block in blocks {
        match block {
            UserContent::Text(text) => content.extend(non_blank_text(&text.text)),
            UserContent::Image(image) => {
                content.push(json!({"image": image_block(&image.mime_type, &image.data)?}));
            }
        }
    }
    if content.is_empty() {
        content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    Ok(content)
}

fn convert_user_message(message: &UserMessage) -> Result<Value, String> {
    let content = match &message.content {
        UserMessageContent::Text(text) => vec![required_text(text)],
        UserMessageContent::Blocks(blocks) => convert_user_blocks(blocks)?,
    };
    Ok(json!({"role": "user", "content": content}))
}

fn convert_redacted_thinking(thinking: &ThinkingContent) -> Option<Value> {
    let bytes = thinking
        .thinking_signature
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(jsrt::base64::atob)
        .filter(|b| !b.is_empty())?;
    Some(json!({"reasoningContent": {"redactedContent": jsrt::base64::encode(&bytes)}}))
}

fn convert_thinking_block(thinking: &ThinkingContent, claude: bool) -> Option<Value> {
    if thinking.redacted == Some(true) {
        return convert_redacted_thinking(thinking);
    }
    if thinking.thinking.trim().is_empty() {
        return None;
    }
    if !claude {
        return Some(json!({"reasoningContent": {"reasoningText": {"text": thinking.thinking}}}));
    }
    match thinking.thinking_signature.as_deref().filter(|s| !s.trim().is_empty()) {
        None => Some(json!({"text": thinking.thinking})),
        Some(signature) => Some(json!({"reasoningContent": {
            "reasoningText": {"text": thinking.thinking, "signature": signature}
        }})),
    }
}

fn convert_assistant_message(message: &AssistantMessage, claude: bool) -> Option<Value> {
    let mut blocks = Vec::new();
    for block in &message.content {
        match block {
            AssistantContent::Text(text) => blocks.extend(non_blank_text(&text.text)),
            AssistantContent::ToolCall(call) => blocks.push(json!({"toolUse": {
                "toolUseId": call.id,
                "name": call.name,
                "input": sanitize_document(&call.arguments),
            }})),
            AssistantContent::Thinking(thinking) => {
                if let Some(converted) = convert_thinking_block(thinking, claude) {
                    blocks.push(converted);
                }
            }
        }
    }
    if blocks.is_empty() {
        None
    } else {
        Some(json!({"role": "assistant", "content": blocks}))
    }
}

fn collect_tool_results(transformed: &[Message], start: usize) -> Result<(Value, usize), String> {
    let mut i = start;
    let mut results = Vec::new();
    while let Some(Message::ToolResult(next)) = transformed.get(i) {
        results.push(json!({"toolResult": {
            "toolUseId": next.tool_call_id,
            "content": tool_result_content(&next.content)?,
            "status": if next.is_error { "error" } else { "success" },
        }}));
        i += 1;
    }
    Ok((json!({"role": "user", "content": results}), i))
}

fn append_trailing_cache_point(
    result: &mut [Value],
    model: &Model,
    retention: CacheRetention,
    env: Option<&ProviderEnv>,
) {
    if retention == CacheRetention::None || !supports_prompt_caching(model, env) {
        return;
    }
    let Some(last) = result.last_mut() else {
        return;
    };
    if last["role"] != "user" {
        return;
    }
    if let Some(content) = last["content"].as_array_mut() {
        content.push(cache_point(retention));
    }
}

fn convert_messages(
    context: &TranscriptContext,
    model: &Model,
    retention: CacheRetention,
    options: &StreamOptions,
) -> Result<Vec<Value>, String> {
    let messages: &[Message] = match context.messages.first() {
        Some(Message::System(_)) => &context.messages[1..],
        _ => &context.messages,
    };
    let transformed = transform_messages(messages, model, Some(&normalize_tool_call_id), jsrt::date::now_ms());
    let claude = is_anthropic_claude_model(model);
    let mut result: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(message) => result.push(convert_user_message(message)?),
            Message::Assistant(message) => {
                if let Some(converted) = convert_assistant_message(message, claude) {
                    result.push(converted);
                }
            }
            Message::ToolResult(_) => {
                let (converted, next_i) = collect_tool_results(&transformed, i)?;
                result.push(converted);
                i = next_i;
                continue;
            }
            Message::System(_) => {}
        }
        i += 1;
    }
    append_trailing_cache_point(&mut result, model, retention, options.env.as_ref());
    Ok(result)
}

fn system_prompt(
    context: &TranscriptContext,
    model: &Model,
    retention: CacheRetention,
    options: &StreamOptions,
) -> Option<Value> {
    let text = initial_system_message(&context.messages).map(system_message_text)?;
    if text.is_empty() {
        return None;
    }
    let mut blocks = vec![json!({"text": text})];
    if retention != CacheRetention::None && supports_prompt_caching(model, options.env.as_ref()) {
        blocks.push(cache_point(retention));
    }
    Some(Value::Array(blocks))
}

fn build_tool_spec(tool: &Tool, supports_strict_mode: bool) -> Result<Value, String> {
    let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
    let mut spec = Map::new();
    spec.insert("name".into(), json!(tool.name));
    spec.insert(
        "inputSchema".into(),
        json!({"json": json_schema_tool_parameters(tool, strict)?}),
    );
    spec.insert("description".into(), json!(tool.description));
    if strict == Some(true) {
        spec.insert("strict".into(), json!(true));
    }
    Ok(json!({"toolSpec": spec}))
}

fn convert_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    if let Some(kind) = choice.as_str() {
        return match kind {
            "auto" => Some(json!({"auto": {}})),
            "any" => Some(json!({"any": {}})),
            _ => None,
        };
    }
    if let Some(obj) = choice.as_object()
        && obj.get("type").and_then(Value::as_str) == Some("tool")
    {
        return Some(json!({"tool": {"name": obj.get("name").cloned().unwrap_or(Value::Null)}}));
    }
    None
}

fn tool_config(tools: &[Tool], choice: Option<&Value>, supports_strict_mode: bool) -> Result<Option<Value>, String> {
    if tools.is_empty() || choice.and_then(Value::as_str) == Some("none") {
        return Ok(None);
    }
    let mut specs = Vec::new();
    for tool in tools {
        specs.push(build_tool_spec(tool, supports_strict_mode)?);
    }
    let mut config = Map::new();
    config.insert("tools".into(), Value::Array(specs));
    if let Some(choice) = convert_tool_choice(choice) {
        config.insert("toolChoice".into(), choice);
    }
    Ok(Some(Value::Object(config)))
}

fn is_govcloud(model: &Model, options: &BedrockOptions) -> bool {
    if configured_region(options).is_some_and(|r| r.to_lowercase().starts_with("us-gov-")) {
        return true;
    }
    let id = model.id.to_lowercase();
    id.starts_with("us-gov.") || id.starts_with("arn:aws-us-gov:")
}

fn adaptive_thinking_fields(model: &Model, level: &str, display: Option<&str>) -> Map<String, Value> {
    let mut thinking = Map::new();
    thinking.insert("type".into(), json!("adaptive"));
    if let Some(display) = display {
        thinking.insert("display".into(), json!(display));
    }
    let mut result = Map::new();
    result.insert("thinking".into(), Value::Object(thinking));
    result.insert("output_config".into(), json!({"effort": effort_for(model, level)}));
    result
}

fn budget_thinking_fields(options: &BedrockOptions, level: &str, display: Option<&str>) -> Map<String, Value> {
    let budget = crate::options::thinking_budget_for_level(level, options.thinking_budgets.as_ref());
    let mut thinking = Map::new();
    thinking.insert("type".into(), json!("enabled"));
    thinking.insert("budget_tokens".into(), json!(budget));
    if let Some(display) = display {
        thinking.insert("display".into(), json!(display));
    }
    let mut result = Map::new();
    result.insert("thinking".into(), Value::Object(thinking));
    if options.interleaved_thinking.unwrap_or(true) {
        result.insert("anthropic_beta".into(), json!(["interleaved-thinking-2025-05-14"]));
    }
    result
}

fn additional_fields(model: &Model, options: &BedrockOptions) -> Option<Value> {
    let level = options.reasoning.as_deref()?;
    if !model.reasoning || !is_anthropic_claude_model(model) {
        return None;
    }
    let display = if is_govcloud(model, options) {
        None
    } else {
        Some(options.thinking_display.as_deref().unwrap_or("summarized"))
    };
    let result = if supports_adaptive_thinking(model) {
        adaptive_thinking_fields(model, level, display)
    } else {
        budget_thinking_fields(options, level, display)
    };
    Some(Value::Object(result))
}

/// The `ConverseStreamCommand` input; the hook sees it with `modelId` first.
pub(super) fn command_input(
    model: &Model,
    context: &TranscriptContext,
    options: &BedrockOptions,
) -> Result<Map<String, Value>, String> {
    let base = &options.base;
    let retention = base.cache_retention();
    let max_tokens = base
        .max_tokens
        .or(is_anthropic_claude_model(model).then_some(model.max_tokens));
    let mut inference = Map::new();
    if let Some(max_tokens) = max_tokens {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = base.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    let mut input = Map::new();
    input.insert("modelId".into(), json!(model.id));
    input.insert(
        "messages".into(),
        Value::Array(convert_messages(context, model, retention, base)?),
    );
    if let Some(system) = system_prompt(context, model, retention, base) {
        input.insert("system".into(), system);
    }
    input.insert("inferenceConfig".into(), Value::Object(inference));
    if let Some(config) = tool_config(
        &current_tools(&context.messages),
        options.tool_choice.as_ref(),
        model.compat_bool("supportsStrictMode").unwrap_or(false),
    )? {
        input.insert("toolConfig".into(), config);
    }
    if let Some(fields) = additional_fields(model, options) {
        input.insert("additionalModelRequestFields".into(), fields);
    }
    if let Some(metadata) = &options.request_metadata {
        input.insert("requestMetadata".into(), json!(metadata));
    }
    Ok(input)
}
