//! Message conversion and Claude Code tool-name mapping for Anthropic Messages requests.

use crate::transcript::render_system_message_update;
use crate::types::{
    AssistantContent, AssistantMessage, Message, SystemMessage, ThinkingContent, Tool, ToolCall, UserContent,
    UserMessage, UserMessageContent,
};
use serde_json::{Value, json};

const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

pub(super) fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_string(), std::string::ToString::to_string)
}

pub(super) fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    tools
        .iter()
        .find(|t| t.name.to_lowercase() == name.to_lowercase())
        .map_or_else(|| name.to_string(), |t| t.name.clone())
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}})
}

/// Tool-result content: one string when there are no images, else blocks (with a placeholder when text is absent).
fn convert_content_blocks(content: &[UserContent]) -> Value {
    if !content.iter().any(|c| matches!(c, UserContent::Image(_))) {
        let text: Vec<&str> = content
            .iter()
            .map(|c| match c {
                UserContent::Text(t) => t.text.as_str(),
                UserContent::Image(_) => "",
            })
            .collect();
        return Value::String(text.join("\n"));
    }
    let mut blocks: Vec<Value> = content
        .iter()
        .map(|c| match c {
            UserContent::Text(t) => text_block(&t.text),
            UserContent::Image(i) => image_block(&i.data, &i.mime_type),
        })
        .collect();
    if !content.iter().any(|c| matches!(c, UserContent::Text(_))) {
        blocks.insert(0, text_block("(see attached image)"));
    }
    Value::Array(blocks)
}

fn tool_reference(name: &str, oauth: bool) -> Value {
    let mapped = if oauth {
        to_claude_code_name(name)
    } else {
        name.to_string()
    };
    json!({"type": "tool_reference", "name": mapped})
}

fn convert_system_message(system: &SystemMessage, oauth: bool, native_tool_changes: bool) -> Option<Value> {
    let text = render_system_message_update(system);
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(text_block(&text));
    }
    if native_tool_changes {
        for tool in system.tools_removed.iter().flatten() {
            blocks.push(json!({"type": "tool_removal", "tool": tool_reference(&tool.name, oauth)}));
        }
        for tool in system.tools_added.iter().flatten() {
            blocks.push(json!({"type": "tool_addition", "tool": tool_reference(&tool.name, oauth)}));
        }
    }
    (!blocks.is_empty()).then(|| json!({"role": "system", "content": blocks}))
}

fn convert_user_message(user: &UserMessage) -> Option<Value> {
    match &user.content {
        UserMessageContent::Text(text) => (!text.trim().is_empty()).then(|| json!({"role": "user", "content": text})),
        UserMessageContent::Blocks(blocks) => {
            let blocks: Vec<Value> = blocks
                .iter()
                .filter(|b| !matches!(b, UserContent::Text(t) if t.text.trim().is_empty()))
                .map(|b| match b {
                    UserContent::Text(t) => text_block(&t.text),
                    UserContent::Image(img) => image_block(&img.data, &img.mime_type),
                })
                .collect();
            (!blocks.is_empty()).then(|| json!({"role": "user", "content": blocks}))
        }
    }
}

fn convert_thinking_block(thinking: &ThinkingContent, allow_empty_signature: bool) -> Option<Value> {
    if thinking.redacted == Some(true) {
        return Some(json!({
            "type": "redacted_thinking",
            "data": thinking.thinking_signature.clone().unwrap_or_default()
        }));
    }
    let signature = thinking.thinking_signature.as_deref().filter(|s| !s.trim().is_empty());
    if thinking.thinking.trim().is_empty() && signature.is_none() {
        return None;
    }
    Some(match signature {
        Some(signature) => json!({"type": "thinking", "thinking": thinking.thinking, "signature": signature}),
        None if allow_empty_signature => json!({"type": "thinking", "thinking": thinking.thinking, "signature": ""}),
        None => text_block(&thinking.thinking),
    })
}

fn convert_tool_use_block(call: &ToolCall, oauth: bool) -> Value {
    json!({
        "type": "tool_use",
        "id": call.id,
        "name": if oauth { to_claude_code_name(&call.name) } else { call.name.clone() },
        "input": if call.arguments.is_null() { json!({}) } else { call.arguments.clone() },
    })
}

fn convert_assistant_block(block: &AssistantContent, oauth: bool, allow_empty_signature: bool) -> Option<Value> {
    match block {
        AssistantContent::Text(text) => (!text.text.trim().is_empty()).then(|| text_block(&text.text)),
        AssistantContent::Thinking(thinking) => convert_thinking_block(thinking, allow_empty_signature),
        AssistantContent::ToolCall(call) => Some(convert_tool_use_block(call, oauth)),
    }
}

fn convert_assistant_blocks(assistant: &AssistantMessage, oauth: bool, allow_empty_signature: bool) -> Vec<Value> {
    assistant
        .content
        .iter()
        .filter_map(|block| convert_assistant_block(block, oauth, allow_empty_signature))
        .collect()
}

fn record_managed_effort(
    assistant: &AssistantMessage,
    managed_provider: Option<&str>,
    index: usize,
    levels: &mut Vec<(usize, String)>,
) {
    if let (Some(provider), Some(level)) = (managed_provider, assistant.provider_thinking_level.as_deref())
        && assistant.api == "anthropic-messages"
        && assistant.provider == provider
        && EFFORTS.contains(&level)
    {
        levels.push((index, level.to_string()));
    }
}

fn collect_tool_results(messages: &[Message], start: usize) -> (Value, usize) {
    let mut i = start;
    let mut results = Vec::new();
    while let Some(Message::ToolResult(result)) = messages.get(i) {
        results.push(json!({
            "type": "tool_result",
            "tool_use_id": result.tool_call_id,
            "content": convert_content_blocks(&result.content),
            "is_error": result.is_error,
        }));
        i += 1;
    }
    (json!({"role": "user", "content": results}), i)
}

fn apply_block_cache_control(last: &mut Value, cache_control: &Value) {
    match last.get_mut("content") {
        Some(Value::Array(blocks)) => {
            let Some(block) = blocks.last_mut() else {
                return;
            };
            let kind = block.get("type").and_then(Value::as_str).unwrap_or_default();
            if matches!(
                kind,
                "text" | "image" | "tool_result" | "tool_addition" | "tool_removal"
            ) {
                block["cache_control"] = cache_control.clone();
            }
        }
        Some(content @ Value::String(_)) => {
            let text = content.as_str().unwrap_or_default().to_string();
            *content = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
        }
        _ => {}
    }
}

fn apply_last_message_cache_control(params: &mut [Value], cache_control: Option<&Value>) {
    let (Some(cache_control), Some(last)) = (cache_control, params.last_mut()) else {
        return;
    };
    let role = last.get("role").and_then(Value::as_str).unwrap_or_default();
    if role == "user" || role == "system" {
        apply_block_cache_control(last, cache_control);
    }
}

struct MessageConfig<'a> {
    oauth: bool,
    allow_empty_signature: bool,
    managed_provider: Option<&'a str>,
    native_tool_changes: bool,
}

fn push_assistant_message(
    assistant: &AssistantMessage,
    config: &MessageConfig<'_>,
    params: &mut Vec<Value>,
    levels: &mut Vec<(usize, String)>,
) {
    let blocks = convert_assistant_blocks(assistant, config.oauth, config.allow_empty_signature);
    if blocks.is_empty() {
        return;
    }
    let index = params.len();
    params.push(json!({"role": "assistant", "content": blocks}));
    record_managed_effort(assistant, config.managed_provider, index, levels);
}

/// Converted messages and the effort recorded on each managed assistant message, by output index.
pub(super) fn convert_messages(
    messages: &[Message],
    oauth: bool,
    cache_control: Option<&Value>,
    allow_empty_signature: bool,
    managed_provider: Option<&str>,
    native_tool_changes: bool,
) -> (Vec<Value>, Vec<(usize, String)>) {
    let config = MessageConfig {
        oauth,
        allow_empty_signature,
        managed_provider,
        native_tool_changes,
    };
    let mut params: Vec<Value> = Vec::new();
    let mut levels = Vec::new();
    let mut pending_system: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(system) => {
                pending_system.extend(convert_system_message(system, config.oauth, config.native_tool_changes));
            }
            Message::User(user) => {
                params.extend(convert_user_message(user));
            }
            Message::Assistant(assistant) => {
                params.append(&mut pending_system);
                push_assistant_message(assistant, &config, &mut params, &mut levels);
            }
            Message::ToolResult(_) => {
                let (msg, next_i) = collect_tool_results(messages, i);
                params.push(msg);
                i = next_i;
                continue;
            }
        }
        i += 1;
    }
    params.append(&mut pending_system);
    apply_last_message_cache_control(&mut params, cache_control);
    (params, levels)
}
