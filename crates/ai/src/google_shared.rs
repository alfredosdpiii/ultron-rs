//! `api/google-shared.ts`: message, tool, tool-choice, thinking and stop-reason conversion shared by the Google Generative
//! AI and Vertex providers. Values are built in the key order the `@google/genai` SDK serializes them, so request bodies
//! equal the recorded wire JSON.

use crate::constrained::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::models::clamp_thinking_level;
use crate::transcript::{collapse_system_messages, transform_messages};
use crate::types::{
    AssistantContent, AssistantMessage, Message, Model, StopReason, Tool, ToolCall, ToolResultMessage,
    TranscriptContext, UserContent, UserMessage, UserMessageContent,
};
use serde_json::{Map, Value, json};

/// Resolve a pi level (or the model's mapping of it) to `minimal | low | medium | high`.
pub fn resolve_google_thinking_level(model: &Model, level: &str) -> Result<&'static str, String> {
    let resolved = model.mapped_or(level).to_lowercase();
    match resolved.as_str() {
        "minimal" => Ok("minimal"),
        "low" => Ok("low"),
        "medium" => Ok("medium"),
        "high" => Ok("high"),
        _ => {
            let mapped = match model.thinking_level(level) {
                crate::types::Mapped::Missing => "undefined".to_string(),
                crate::types::Mapped::Unsupported => "null".to_string(),
                crate::types::Mapped::Value(v) => v.to_string(),
            };
            Err(format!(
                "Unsupported Google thinking level mapping for {}/{}: {level} -> {mapped}",
                model.provider, model.id
            ))
        }
    }
}

/// Whether the model takes Gemini's discrete `thinkingLevel` instead of a token `thinkingBudget`.
pub fn uses_google_thinking_level(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    let gemini3 = id.find("gemini-3").is_some_and(|start| {
        let rest = &id[start + "gemini-3".len()..];
        let rest = match rest.strip_prefix('.') {
            Some(minor) if minor.starts_with(|c: char| c.is_ascii_digit()) => {
                minor.trim_start_matches(|c: char| c.is_ascii_digit())
            }
            _ => rest,
        };
        rest.starts_with("-pro") || rest.starts_with("-flash")
    });
    gemini3
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || id.contains("gemma-4")
        || id.contains("gemma4")
}

pub fn to_google_thinking_level(level: &str) -> &'static str {
    match level {
        "minimal" => "MINIMAL",
        "low" => "LOW",
        "medium" => "MEDIUM",
        _ => "HIGH",
    }
}

/// `thinkingConfig` for a reasoning model with thinking turned off.
pub fn disabled_google_thinking_config(model: &Model) -> Result<Value, String> {
    if !uses_google_thinking_level(&model.id) {
        return Ok(json!({ "thinkingBudget": 0 }));
    }
    let fallback = clamp_thinking_level(model, "off");
    if fallback == "off" {
        return Ok(json!({ "thinkingBudget": 0 }));
    }
    let level = resolve_google_thinking_level(model, fallback)?;
    Ok(json!({ "thinkingLevel": to_google_thinking_level(level) }))
}

/// Keep the last non-empty signature seen for a streamed block.
pub fn retain_thought_signature(existing: Option<String>, incoming: Option<&str>) -> Option<String> {
    match incoming {
        Some(sig) if !sig.is_empty() => Some(sig.to_string()),
        _ => existing,
    }
}

/// Google requires base64 (`TYPE_BYTES`) signatures.
fn is_valid_thought_signature(signature: Option<&str>) -> bool {
    let Some(sig) = signature.filter(|s| !s.is_empty()) else {
        return false;
    };
    if sig.len() % 4 != 0 {
        return false;
    }
    let body = sig.trim_end_matches('=');
    sig.len() - body.len() <= 2
        && !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

fn resolve_thought_signature(same_model: bool, signature: Option<&str>) -> Option<String> {
    (same_model && is_valid_thought_signature(signature)).then(|| signature.unwrap_or_default().to_string())
}

fn gemini_major_version(model_id: &str) -> Option<u64> {
    let id = model_id.to_lowercase();
    let rest = id.strip_prefix("gemini-")?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Models behind Google APIs that need explicit ids on function calls and responses.
pub fn requires_tool_call_id(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("gpt-oss-")
        || gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

fn supports_multimodal_function_response(model_id: &str) -> bool {
    gemini_major_version(model_id).is_none_or(|v| v >= 3)
}

/// Gemini 3+ enforces required function parameters in validated tool-calling modes.
pub fn supports_google_strict_tool_sampling(model_id: &str) -> bool {
    gemini_major_version(model_id).is_some_and(|v| v >= 3)
}

fn inline_data(data: &str, mime_type: &str) -> Value {
    json!({ "inlineData": { "mimeType": mime_type, "data": data } })
}

/// A text part with optional `thought` and `thoughtSignature`, keys in upstream's construction order.
fn text_part(text: &str, thought: bool, signature: Option<String>) -> Value {
    let mut part = Map::new();
    if thought {
        part.insert("thought".into(), true.into());
    }
    part.insert("text".into(), text.into());
    if let Some(sig) = signature {
        part.insert("thoughtSignature".into(), sig.into());
    }
    Value::Object(part)
}

fn reorder(object: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|key| object.get(*key).map(|v| ((*key).to_string(), v.clone())))
        .collect()
}

const PART_KEYS: [&str; 6] = [
    "functionCall",
    "functionResponse",
    "inlineData",
    "text",
    "thought",
    "thoughtSignature",
];

/// The SDK's `contentToMldev` / `contentToVertex` over upstream-shaped `contents`: part keys in SDK order; the Gemini
/// API transform also reorders `functionCall` (`args`, `id`, `name`) and `inlineData` (`data`, `mimeType`), which the
/// Vertex transform passes through.
pub fn contents_to_wire(contents: &[Value], vertex: bool) -> Vec<Value> {
    contents
        .iter()
        .map(|content| {
            let parts: Vec<Value> = content
                .get("parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|part| {
                    let Some(part) = part.as_object() else {
                        return part.clone();
                    };
                    let mut wire = reorder(part, &PART_KEYS);
                    if !vertex {
                        if let Some(Value::Object(call)) = wire.get_mut("functionCall") {
                            *call = reorder(call, &["args", "id", "name"]);
                        }
                        if let Some(Value::Object(blob)) = wire.get_mut("inlineData") {
                            *blob = reorder(blob, &["data", "mimeType"]);
                        }
                    }
                    Value::Object(wire)
                })
                .collect();
            json!({ "parts": parts, "role": content.get("role").cloned().unwrap_or(Value::Null) })
        })
        .collect()
}

fn normalize_google_tool_id(id: &str, needs_id: bool) -> String {
    if !needs_id {
        return id.to_string();
    }
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn convert_user_message(user: &UserMessage) -> Option<Value> {
    let parts: Vec<Value> = match &user.content {
        UserMessageContent::Text(text) => vec![text_part(text, false, None)],
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContent::Text(t) => text_part(&t.text, false, None),
                UserContent::Image(i) => inline_data(&i.data, &i.mime_type),
            })
            .collect(),
    };
    (!parts.is_empty()).then(|| json!({ "parts": parts, "role": "user" }))
}

fn convert_tool_call_block(call: &ToolCall, same: bool, needs_id: bool) -> Value {
    let sig = resolve_thought_signature(same, call.thought_signature.as_deref());
    let mut function_call = Map::new();
    let args = if call.arguments.is_null() {
        json!({})
    } else {
        call.arguments.clone()
    };
    function_call.insert("name".into(), call.name.clone().into());
    function_call.insert("args".into(), args);
    if needs_id {
        function_call.insert("id".into(), call.id.clone().into());
    }
    let mut part = Map::new();
    part.insert("functionCall".into(), Value::Object(function_call));
    if let Some(sig) = sig {
        part.insert("thoughtSignature".into(), sig.into());
    }
    Value::Object(part)
}

fn convert_assistant_block(block: &AssistantContent, same: bool, needs_id: bool) -> Option<Value> {
    match block {
        AssistantContent::Text(t) => {
            let sig = resolve_thought_signature(same, t.text_signature.as_deref());
            (!(t.text.trim().is_empty() && sig.is_none())).then(|| text_part(&t.text, false, sig))
        }
        AssistantContent::Thinking(t) if same => {
            let sig = resolve_thought_signature(same, t.thinking_signature.as_deref());
            (!(t.thinking.trim().is_empty() && sig.is_none())).then(|| text_part(&t.thinking, true, sig))
        }
        AssistantContent::Thinking(t) => (!t.thinking.trim().is_empty()).then(|| text_part(&t.thinking, false, None)),
        AssistantContent::ToolCall(call) => Some(convert_tool_call_block(call, same, needs_id)),
    }
}

fn convert_assistant_message(assistant: &AssistantMessage, model: &Model, needs_id: bool) -> Option<Value> {
    let same = assistant.provider == model.provider && assistant.model == model.id;
    let parts: Vec<Value> = assistant
        .content
        .iter()
        .filter_map(|block| convert_assistant_block(block, same, needs_id))
        .collect();
    (!parts.is_empty()).then(|| json!({ "parts": parts, "role": "model" }))
}

fn collect_tool_result_images(result: &ToolResultMessage, model: &Model) -> Vec<Value> {
    if !model.supports_images() {
        return Vec::new();
    }
    result
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Image(i) => Some(inline_data(&i.data, &i.mime_type)),
            UserContent::Text(_) => None,
        })
        .collect()
}

fn merge_function_response(contents: &mut Vec<Value>, part: Value) {
    let merge = contents.last_mut().filter(|last| {
        last["role"] == "user"
            && last["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|p| p.get("functionResponse").is_some()))
    });
    match merge {
        Some(last) => {
            if let Some(parts) = last["parts"].as_array_mut() {
                parts.push(part);
            }
        }
        None => contents.push(json!({ "parts": [part], "role": "user" })),
    }
}

fn push_tool_result(contents: &mut Vec<Value>, result: &ToolResultMessage, model: &Model, needs_id: bool) {
    let text = result.joined_text();
    let images = collect_tool_result_images(result, model);
    let multimodal = supports_multimodal_function_response(&model.id);
    let value = if !text.is_empty() {
        text
    } else if !images.is_empty() {
        "(see attached image)".to_string()
    } else {
        String::new()
    };
    let mut response = Map::new();
    response.insert("name".into(), result.tool_name.clone().into());
    let key = if result.is_error { "error" } else { "output" };
    response.insert("response".into(), json!({ key: value }));
    if !images.is_empty() && multimodal {
        response.insert("parts".into(), Value::Array(images.clone()));
    }
    if needs_id {
        response.insert("id".into(), result.tool_call_id.clone().into());
    }
    merge_function_response(contents, json!({ "functionResponse": response }));
    if !images.is_empty() && !multimodal {
        let mut parts = vec![text_part("Tool result image:", false, None)];
        parts.extend(images);
        contents.push(json!({ "parts": parts, "role": "user" }));
    }
}

/// Convert the transcript to Gemini `contents` (the leading system prompt travels separately), shaped as upstream
/// builds them; `contents_to_wire` gives the serialized form.
pub fn convert_messages(model: &Model, context: &TranscriptContext, now: i64) -> Vec<Value> {
    let collapsed = collapse_system_messages(context).messages;
    let conversation = match collapsed.first() {
        Some(Message::System(_)) => &collapsed[1..],
        _ => &collapsed[..],
    };
    let needs_id = requires_tool_call_id(&model.id);
    let normalize = |id: &str, _: &AssistantMessage| normalize_google_tool_id(id, needs_id);
    let transformed = transform_messages(conversation, model, Some(&normalize), now);
    let mut contents: Vec<Value> = Vec::new();
    for message in &transformed {
        match message {
            Message::User(user) => {
                if let Some(entry) = convert_user_message(user) {
                    contents.push(entry);
                }
            }
            Message::Assistant(assistant) => {
                if let Some(entry) = convert_assistant_message(assistant, model, needs_id) {
                    contents.push(entry);
                }
            }
            Message::ToolResult(result) => push_tool_result(&mut contents, result, model, needs_id),
            Message::System(_) => {}
        }
    }
    contents
}

const JSON_SCHEMA_META_DECLARATIONS: [&str; 8] = [
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "$defs",
    "definitions",
];

/// Strip JSON Schema meta declarations for the OpenAPI `parameters` field.
fn sanitize_for_open_api(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(k, _)| !JSON_SCHEMA_META_DECLARATIONS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), sanitize_for_open_api(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Gemini `tools` (one `functionDeclarations` entry), `None` when there are no tools.
pub fn convert_tools(
    tools: &[Tool],
    use_parameters: bool,
    supports_strict_mode: bool,
) -> Result<Option<Value>, String> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut declarations = Vec::new();
    for tool in tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict_mode)?;
        let parameters = json_schema_tool_parameters(tool, strict)?;
        let mut declaration = Map::new();
        declaration.insert("name".into(), tool.name.clone().into());
        declaration.insert("description".into(), tool.description.clone().into());
        if use_parameters {
            declaration.insert("parameters".into(), sanitize_for_open_api(&parameters));
        } else {
            declaration.insert("parametersJsonSchema".into(), parameters);
        }
        declarations.push(Value::Object(declaration));
    }
    Ok(Some(json!([{ "functionDeclarations": declarations }])))
}

fn map_tool_choice(choice: &str) -> &'static str {
    match choice {
        "none" => "NONE",
        "any" => "ANY",
        _ => "AUTO",
    }
}

/// `functionCallingConfig.mode`: explicit none/any win, then `VALIDATED` when any tool samples strictly.
pub fn resolve_google_function_calling_mode(
    tools: &[Tool],
    tool_choice: Option<&str>,
    supports_strict_mode: bool,
) -> Result<Option<&'static str>, String> {
    let mut strict = false;
    for tool in tools {
        strict |= resolve_json_schema_strict_sampling(tool, supports_strict_mode)? == Some(true);
    }
    if let Some(choice @ ("none" | "any")) = tool_choice {
        return Ok(Some(map_tool_choice(choice)));
    }
    if strict {
        return Ok(Some("VALIDATED"));
    }
    Ok(tool_choice.map(map_tool_choice))
}

/// `mapStopReason` over the SDK's `FinishReason` values; unknown values throw.
pub fn map_stop_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "STOP" => Ok(StopReason::Stop),
        "MAX_TOKENS" => Ok(StopReason::Length),
        "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "SAFETY"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "RECITATION"
        | "FINISH_REASON_UNSPECIFIED"
        | "OTHER"
        | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "TOO_MANY_TOOL_CALLS"
        | "NO_IMAGE" => Ok(StopReason::Error),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}
