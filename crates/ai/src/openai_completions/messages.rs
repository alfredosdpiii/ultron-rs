use super::compat::Compat;
use super::reasoning::{REASONING_FIELDS, parse_legacy_encrypted_detail, parse_reasoning_details};
use crate::constrained::{
    grammar_tool_input, json_schema_tool_parameters, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
use crate::json::short_hash;
use crate::transcript::{
    render_system_message_update, resolve_transcript, resolve_transcript_tools, system_message_text, transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, Message, Model, SystemMessage, ThinkingContent, Tool, ToolCall,
    ToolResultMessage, TranscriptContext, UserContent, UserMessage, UserMessageContent,
};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

pub(super) fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_))),
        _ => false,
    })
}

fn sanitize_tool_id_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn normalize_tool_call_id(model: &Model, id: &str) -> String {
    if let Some(separator) = id.find('|') {
        let call_id = sanitize_tool_id_segment(&id[..separator]);
        let item_id = sanitize_tool_id_segment(&id[separator + 1..]);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if jsrt::utf16::len(&combined) <= 40 {
            return combined;
        }
        let hash: String = short_hash(id).chars().take(8).collect();
        let prefix = jsrt::utf16::head(&call_id, 1.max(40 - jsrt::utf16::len(&hash) - 1));
        return format!("{prefix}_{hash}");
    }
    if model.provider == "openai" && jsrt::utf16::len(id) > 40 {
        return jsrt::utf16::head(id, 40).to_string();
    }
    id.to_string()
}

fn image_url(data: &str, mime_type: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}})
}

fn convert_single_tool(tool: &Tool, compat: &Compat) -> Result<Value, String> {
    if let Some(grammar) = resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)? {
        return Ok(json!({"type": "custom", "custom": {
            "name": tool.name,
            "description": tool.description,
            "format": {"type": "grammar", "grammar": {"syntax": grammar.format, "definition": grammar.definition}},
        }}));
    }
    let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode)?;
    let mut function = Map::new();
    function.insert("name".into(), json!(tool.name));
    function.insert("description".into(), json!(tool.description));
    function.insert("parameters".into(), json_schema_tool_parameters(tool, strict)?);
    if compat.supports_strict_mode {
        function.insert("strict".into(), json!(strict.unwrap_or(false)));
    }
    Ok(json!({"type": "function", "function": function}))
}

pub(super) fn convert_tools(tools: &[Tool], compat: &Compat) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        out.push(convert_single_tool(tool, compat)?);
    }
    Ok(out)
}

fn assistant_bridge_message() -> Value {
    json!({"role": "assistant", "content": "I have processed the tool results."})
}

fn instruction_role(model: &Model, compat: &Compat) -> &'static str {
    if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    }
}

fn should_insert_bridge(compat: &Compat, last_role: Option<&str>, message: &Message) -> bool {
    compat.requires_assistant_after_tool_result
        && last_role == Some("toolResult")
        && matches!(message, Message::User(_))
}

fn push_system_message(
    params: &mut Vec<Value>,
    system: &SystemMessage,
    is_initial: bool,
    anchors_additions: bool,
    role: &str,
    compat: &Compat,
) -> Result<(), String> {
    let added = if !is_initial && anchors_additions {
        system.tools_added.clone().unwrap_or_default()
    } else {
        vec![]
    };
    if !added.is_empty() {
        params.push(json!({"role": "system", "tools": convert_tools(&added, compat)?}));
    }
    let text = if is_initial {
        system_message_text(system)
    } else {
        render_system_message_update(system)
    };
    if !text.is_empty() {
        params.push(json!({"role": role, "content": text}));
    }
    Ok(())
}

fn convert_user_message(user: &UserMessage) -> Option<Value> {
    match &user.content {
        UserMessageContent::Text(text) => Some(json!({"role": "user", "content": text})),
        UserMessageContent::Blocks(blocks) => {
            let content: Vec<Value> = blocks
                .iter()
                .filter(|block| !matches!(block, UserContent::Text(text) if text.text.is_empty()))
                .map(|block| match block {
                    UserContent::Text(text) => json!({"type": "text", "text": text.text}),
                    UserContent::Image(image) => image_url(&image.data, &image.mime_type),
                })
                .collect();
            (!content.is_empty()).then(|| json!({"role": "user", "content": content}))
        }
    }
}

struct AssistantParts<'a> {
    text_parts: Vec<&'a str>,
    assistant_text: String,
    thinking: Vec<&'a ThinkingContent>,
    calls: Vec<&'a ToolCall>,
}

fn collect_assistant_parts(assistant: &AssistantMessage) -> AssistantParts<'_> {
    let mut text_parts = Vec::new();
    let mut thinking = Vec::new();
    let mut calls = Vec::new();
    for block in &assistant.content {
        match block {
            AssistantContent::Text(text) => {
                if !text.text.trim().is_empty() {
                    text_parts.push(text.text.as_str());
                }
            }
            AssistantContent::Thinking(thought) => thinking.push(thought),
            AssistantContent::ToolCall(call) => calls.push(call),
        }
    }
    let assistant_text = text_parts.concat();
    AssistantParts {
        text_parts,
        assistant_text,
        thinking,
        calls,
    }
}

fn resolve_preserved_reasoning(thinking: &[&ThinkingContent], calls: &[&ToolCall]) -> Option<Vec<Value>> {
    let signed = thinking
        .iter()
        .find_map(|thought| parse_reasoning_details(thought.thinking_signature.as_deref()));
    if signed.is_some() {
        return signed;
    }
    let legacy: Vec<Value> = calls
        .iter()
        .filter_map(|call| parse_legacy_encrypted_detail(call.thought_signature.as_deref()))
        .collect();
    (!legacy.is_empty()).then_some(legacy)
}

fn resolve_reasoning_field(model: &Model, thinking: &ThinkingContent) -> Option<String> {
    let mut signature = thinking.thinking_signature.clone();
    if model.provider == "opencode-go" && signature.as_deref() == Some("reasoning") {
        signature = Some("reasoning_content".into());
    }
    signature.filter(|field| REASONING_FIELDS.contains(&field.as_str()))
}

fn insert_assistant_reasoning(
    out: &mut Map<String, Value>,
    model: &Model,
    compat: &Compat,
    parts: &AssistantParts<'_>,
    has_preserved: bool,
) {
    let non_empty: Vec<&&ThinkingContent> = parts
        .thinking
        .iter()
        .filter(|thought| !thought.thinking.trim().is_empty())
        .collect();
    if non_empty.is_empty() {
        if !parts.assistant_text.is_empty() {
            out.insert("content".into(), json!(parts.assistant_text));
        }
        return;
    }
    if compat.requires_thinking_as_text {
        let thinking_text = non_empty
            .iter()
            .map(|thought| thought.thinking.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut content_parts = vec![json!({"type": "text", "text": thinking_text})];
        content_parts.extend(
            parts
                .text_parts
                .iter()
                .map(|text| json!({"type": "text", "text": text})),
        );
        out.insert("content".into(), Value::Array(content_parts));
        return;
    }
    if !parts.assistant_text.is_empty() {
        out.insert("content".into(), json!(parts.assistant_text));
    }
    if !has_preserved && let Some(field) = resolve_reasoning_field(model, non_empty[0]) {
        let joined = non_empty
            .iter()
            .map(|thought| thought.thinking.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        out.insert(field, json!(joined));
    }
}

fn convert_assistant_tool_calls(calls: &[&ToolCall], grammar: &IndexMap<String, String>) -> Result<Vec<Value>, String> {
    let mut tool_calls = Vec::with_capacity(calls.len());
    for call in calls {
        tool_calls.push(match grammar.get(&call.name) {
            Some(property) => json!({"id": call.id, "type": "custom", "custom": {
                "name": call.name,
                "input": grammar_tool_input(&call.name, &call.arguments, property)?,
            }}),
            None => json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": jsrt::json::stringify(&call.arguments)},
            }),
        });
    }
    Ok(tool_calls)
}

fn assistant_has_content(out: &Map<String, Value>) -> bool {
    match out.get("content") {
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        _ => false,
    }
}

fn convert_assistant_message(
    assistant: &AssistantMessage,
    model: &Model,
    compat: &Compat,
    grammar: &IndexMap<String, String>,
) -> Result<Option<Value>, String> {
    let mut out = Map::new();
    out.insert("role".into(), json!("assistant"));
    out.insert(
        "content".into(),
        if compat.requires_assistant_after_tool_result {
            json!("")
        } else {
            Value::Null
        },
    );
    let parts = collect_assistant_parts(assistant);
    let preserved = resolve_preserved_reasoning(&parts.thinking, &parts.calls);
    insert_assistant_reasoning(&mut out, model, compat, &parts, preserved.is_some());
    if !parts.calls.is_empty() {
        out.insert(
            "tool_calls".into(),
            Value::Array(convert_assistant_tool_calls(&parts.calls, grammar)?),
        );
    }
    if let Some(details) = preserved {
        out.insert("reasoning_details".into(), Value::Array(details));
    }
    if compat.requires_reasoning_content_on_assistant_messages
        && model.reasoning
        && !out.contains_key("reasoning_content")
    {
        out.insert("reasoning_content".into(), json!(""));
    }
    if !assistant_has_content(&out) && !out.contains_key("tool_calls") {
        return Ok(None);
    }
    Ok(Some(Value::Object(out)))
}

fn collect_tool_result_images(result: &ToolResultMessage, model: &Model, images: &mut Vec<Value>) {
    if !model.supports_images() {
        return;
    }
    for block in &result.content {
        if let UserContent::Image(image) = block {
            images.push(image_url(&image.data, &image.mime_type));
        }
    }
}

fn convert_single_tool_result(
    result: &ToolResultMessage,
    model: &Model,
    compat: &Compat,
    images: &mut Vec<Value>,
) -> Value {
    let text = result.joined_text();
    let has_images = result.has_images();
    let content = if !text.is_empty() {
        text
    } else if has_images {
        "(see attached image)".into()
    } else {
        "(no tool output)".into()
    };
    let mut out = Map::new();
    out.insert("role".into(), json!("tool"));
    out.insert("content".into(), json!(content));
    out.insert("tool_call_id".into(), json!(result.tool_call_id));
    if compat.requires_tool_result_name && !result.tool_name.is_empty() {
        out.insert("name".into(), json!(result.tool_name));
    }
    if has_images {
        collect_tool_result_images(result, model, images);
    }
    Value::Object(out)
}

fn push_tool_result_run(
    params: &mut Vec<Value>,
    transformed: &[Message],
    start: usize,
    model: &Model,
    compat: &Compat,
) -> (usize, &'static str) {
    let mut idx = start;
    let mut images = Vec::new();
    while let Some(Message::ToolResult(result)) = transformed.get(idx) {
        params.push(convert_single_tool_result(result, model, compat, &mut images));
        idx += 1;
    }
    if images.is_empty() {
        return (idx, "toolResult");
    }
    if compat.requires_assistant_after_tool_result {
        params.push(assistant_bridge_message());
    }
    let mut content = vec![json!({"type": "text", "text": "Attached image(s) from tool result:"})];
    content.extend(images);
    params.push(json!({"role": "user", "content": content}));
    (idx, "user")
}

struct MessageEnv<'a> {
    model: &'a Model,
    compat: &'a Compat,
    grammar: &'a IndexMap<String, String>,
    anchors_additions: bool,
    instruction_role: &'static str,
}

fn dispatch_non_tool_message(
    params: &mut Vec<Value>,
    message: &Message,
    is_initial: bool,
    env: &MessageEnv<'_>,
) -> Result<Option<&'static str>, String> {
    match message {
        Message::System(system) => {
            push_system_message(
                params,
                system,
                is_initial,
                env.anchors_additions,
                env.instruction_role,
                env.compat,
            )?;
            Ok(Some("system"))
        }
        Message::User(user) => {
            let Some(converted) = convert_user_message(user) else {
                return Ok(None);
            };
            params.push(converted);
            Ok(Some("user"))
        }
        Message::Assistant(assistant) => {
            let Some(converted) = convert_assistant_message(assistant, env.model, env.compat, env.grammar)? else {
                return Ok(None);
            };
            params.push(converted);
            Ok(Some("assistant"))
        }
        Message::ToolResult(_) => Ok(None),
    }
}

/// `convertMessages`: the transcript as Chat Completions messages.
pub(super) fn convert_messages(
    model: &Model,
    context: &TranscriptContext,
    compat: &Compat,
    grammar: &IndexMap<String, String>,
    now: i64,
) -> Result<Vec<Value>, String> {
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let mut params: Vec<Value> = Vec::new();
    let normalize = |id: &str, _: &AssistantMessage| normalize_tool_call_id(model, id);
    let transformed = transform_messages(&context.messages, model, Some(&normalize), now);
    let tools = resolve_transcript_tools(
        &context.messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let env = MessageEnv {
        model,
        compat,
        grammar,
        anchors_additions: tools.anchors_additions,
        instruction_role: instruction_role(model, compat),
    };
    let mut last_role: Option<&str> = None;
    let mut idx = 0;
    while idx < transformed.len() {
        let message = &transformed[idx];
        if should_insert_bridge(compat, last_role, message) {
            params.push(assistant_bridge_message());
        }
        if matches!(message, Message::ToolResult(_)) {
            let (next_idx, role) = push_tool_result_run(&mut params, &transformed, idx, model, compat);
            last_role = Some(role);
            idx = next_idx;
            continue;
        }
        if let Some(role) = dispatch_non_tool_message(&mut params, message, idx == 0, &env)? {
            last_role = Some(role);
        }
        idx += 1;
    }
    Ok(params)
}
