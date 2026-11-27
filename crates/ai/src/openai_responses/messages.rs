use super::str_field;
use crate::constrained::{
    GrammarConstrainedSampling, grammar_tool_input, json_schema_tool_parameters, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
use crate::json::short_hash;
use crate::transcript::{
    render_system_message_update, resolve_transcript, resolve_transcript_tools, system_message_text, transform_messages,
};
use crate::types::{
    AssistantContent, AssistantMessage, Message, Model, SystemMessage, TextContent, Tool, ToolCall, ToolResultMessage,
    TranscriptContext, UserContent, UserMessage, UserMessageContent,
};
use indexmap::IndexMap;
use jsrt::json_parse::parse_json;
use serde_json::{Map, Value, json};

pub(crate) const OPENAI_TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode"];

/// Options for converting tools (`ConvertResponsesToolsOptions`).
#[derive(Clone, Copy, Default)]
pub struct ToolOptions {
    /// `strict: null` (Codex): tools without a constrained-sampling choice send `"strict": null`, not `false`.
    pub null_strict: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub tool_search_result: bool,
}

fn convert_custom_tool(tool: &Tool, grammar: &GrammarConstrainedSampling, options: ToolOptions) -> Value {
    let mut converted = Map::new();
    converted.insert("type".into(), json!("custom"));
    converted.insert("name".into(), json!(tool.name));
    converted.insert("description".into(), json!(tool.description));
    converted.insert(
        "format".into(),
        json!({"type": "grammar", "syntax": grammar.format, "definition": grammar.definition}),
    );
    if options.tool_search_result {
        converted.insert("defer_loading".into(), json!(true));
    }
    Value::Object(converted)
}

fn convert_function_tool(tool: &Tool, options: ToolOptions) -> Result<Value, String> {
    let mut converted = Map::new();
    let constrained = resolve_json_schema_strict_sampling(tool, options.supports_strict_mode)?;
    let strict = constrained.or(if options.null_strict { None } else { Some(false) });
    converted.insert("type".into(), json!("function"));
    converted.insert("name".into(), json!(tool.name));
    converted.insert("description".into(), json!(tool.description));
    converted.insert(
        "parameters".into(),
        json_schema_tool_parameters(tool, Some(strict == Some(true)))?,
    );
    if options.tool_search_result {
        converted.insert("defer_loading".into(), json!(true));
    }
    if options.supports_strict_mode {
        converted.insert("strict".into(), json!(strict));
    }
    Ok(Value::Object(converted))
}

pub fn convert_responses_tools(tools: &[Tool], options: ToolOptions) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, options.supports_openai_grammar_tools)? {
            out.push(convert_custom_tool(tool, &grammar, options));
        } else {
            out.push(convert_function_tool(tool, options)?);
        }
    }
    Ok(out)
}

fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature.filter(|sig| !sig.is_empty())?;
    if signature.starts_with('{')
        && let Ok(parsed) = parse_json(signature)
        && parsed.get("v").and_then(Value::as_i64) == Some(1)
        && let Some(id) = str_field(&parsed, "id")
    {
        let phase = str_field(&parsed, "phase")
            .filter(|phase| *phase == "commentary" || *phase == "final_answer")
            .map(str::to_string);
        return Some((id.to_string(), phase));
    }
    Some((signature.to_string(), None))
}

fn input_image(data: &str, mime_type: &str) -> Value {
    json!({"type": "input_image", "detail": "auto", "image_url": format!("data:{mime_type};base64,{data}")})
}

fn tool_result_parts(content: &[UserContent]) -> (String, Vec<Value>) {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for item in content {
        match item {
            UserContent::Text(text) => text_parts.push(text.text.as_str()),
            UserContent::Image(image) => images.push(input_image(&image.data, &image.mime_type)),
        }
    }
    (text_parts.join("\n"), images)
}

fn tool_result_output(model: &Model, content: &[UserContent]) -> Value {
    let (text, images) = tool_result_parts(content);
    if images.is_empty() || !model.supports_images() {
        return json!(if !text.is_empty() {
            text
        } else if !images.is_empty() {
            "(see attached image)".into()
        } else {
            "(no tool output)".to_string()
        });
    }
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({"type": "input_text", "text": text}));
    }
    output.extend(images);
    Value::Array(output)
}

fn sanitize(part: &str) -> String {
    part.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = sanitize(part).chars().take(64).collect();
    sanitized.trim_end_matches('_').to_string()
}

fn normalize_tool_call_id(id: &str, source: &AssistantMessage, model: &Model, allowed: bool) -> String {
    if !allowed || !id.contains('|') {
        return normalize_id_part(id);
    }
    let mut parts = id.split('|');
    let call_id = normalize_id_part(parts.next().unwrap_or_default());
    let item_id = parts.next().unwrap_or_default();
    let foreign = source.provider != model.provider || source.api != model.api;
    let mut normalized_item = if foreign {
        let hashed = format!("fc_{}", short_hash(item_id));
        hashed.chars().take(64).collect()
    } else {
        normalize_id_part(item_id)
    };
    if !normalized_item.starts_with("fc_") {
        normalized_item = normalize_id_part(&format!("fc_{normalized_item}"));
    }
    format!("{call_id}|{normalized_item}")
}

pub struct ConvertOptions<'a> {
    /// Providers whose `call|item` tool call ids keep their item part (`allowedToolCallProviders`).
    pub tool_call_providers: &'a [&'a str],
    pub include_system_prompt: bool,
    pub grammar: &'a IndexMap<String, String>,
    pub supports_mid_convo_system_messages: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub tool_options: ToolOptions,
}

struct MessagePass<'a> {
    options: &'a ConvertOptions<'a>,
    instruction_role: &'a str,
    anchors_additions: bool,
}

fn append_tool_additions(
    messages: &mut Vec<Value>,
    system: &SystemMessage,
    seed: &str,
    pass: &MessagePass<'_>,
) -> Result<(), String> {
    let added = if pass.anchors_additions {
        system.tools_added.clone().unwrap_or_default()
    } else {
        vec![]
    };
    if added.is_empty() {
        return Ok(());
    }
    if pass.options.supports_additional_tools {
        messages.push(json!({
            "type": "additional_tools",
            "role": "developer",
            "tools": convert_responses_tools(&added, pass.options.tool_options)?,
        }));
        return Ok(());
    }
    if !pass.options.supports_tool_search {
        return Ok(());
    }
    let names: Vec<&str> = added.iter().map(|tool| tool.name.as_str()).collect();
    let call_id = format!("pi_tool_load_{}", short_hash(&format!("{seed}:{}", names.join(","))));
    messages.push(json!({
        "type": "tool_search_call",
        "call_id": call_id,
        "execution": "client",
        "status": "completed",
        "arguments": {"query": names.join(" "), "limit": names.len()},
    }));
    let search_options = ToolOptions {
        tool_search_result: true,
        ..pass.options.tool_options
    };
    messages.push(json!({
        "type": "tool_search_output",
        "call_id": call_id,
        "execution": "client",
        "status": "completed",
        "tools": convert_responses_tools(&added, search_options)?,
    }));
    Ok(())
}

fn append_system_message(
    messages: &mut Vec<Value>,
    system: &SystemMessage,
    leading: bool,
    msg_index: usize,
    pass: &MessagePass<'_>,
) -> Result<(), String> {
    if !leading {
        append_tool_additions(messages, system, &format!("system:{msg_index}"), pass)?;
    }
    if !leading || pass.options.include_system_prompt {
        let text = if leading {
            system_message_text(system)
        } else {
            render_system_message_update(system)
        };
        if !text.is_empty() {
            messages.push(json!({"role": pass.instruction_role, "content": text}));
        }
    }
    Ok(())
}

fn convert_user_message(user: &UserMessage) -> Option<Value> {
    match &user.content {
        UserMessageContent::Text(text) => {
            Some(json!({"role": "user", "content": [{"type": "input_text", "text": text}]}))
        }
        UserMessageContent::Blocks(blocks) => {
            if blocks.is_empty() {
                return None;
            }
            let content: Vec<Value> = blocks
                .iter()
                .map(|block| match block {
                    UserContent::Text(text) => json!({"type": "input_text", "text": text.text}),
                    UserContent::Image(image) => input_image(&image.data, &image.mime_type),
                })
                .collect();
            Some(json!({"role": "user", "content": content}))
        }
    }
}

fn convert_assistant_text(text: &TextContent, msg_index: usize, text_index: usize) -> Value {
    let parsed = parse_text_signature(text.text_signature.as_deref());
    let fallback = if text_index == 0 {
        format!("msg_pi_{msg_index}")
    } else {
        format!("msg_pi_{msg_index}_{text_index}")
    };
    let id = match parsed.as_ref().map(|(id, _)| id.as_str()).filter(|id| !id.is_empty()) {
        None => fallback,
        Some(id) if jsrt::utf16::len(id) > 64 => format!("msg_{}", short_hash(id)),
        Some(id) => id.to_string(),
    };
    let mut item = Map::new();
    item.insert("type".into(), json!("message"));
    item.insert("role".into(), json!("assistant"));
    item.insert(
        "content".into(),
        json!([{"type": "output_text", "text": text.text, "annotations": []}]),
    );
    item.insert("status".into(), json!("completed"));
    item.insert("id".into(), json!(id));
    if let Some(phase) = parsed.and_then(|(_, phase)| phase) {
        item.insert("phase".into(), json!(phase));
    }
    Value::Object(item)
}

fn tool_call_item_id(raw_item_id: Option<&str>, is_custom: bool, different_model: bool) -> Option<&str> {
    let is_fc = raw_item_id.is_some_and(|id| id.starts_with("fc_"));
    if (different_model && is_fc) || (!is_custom && !is_fc) {
        return None;
    }
    raw_item_id
}

fn convert_assistant_tool_call(
    call: &ToolCall,
    same_model: bool,
    different_model: bool,
    grammar: &IndexMap<String, String>,
) -> Result<Value, String> {
    let mut parts = call.id.split('|');
    let call_id = parts.next().unwrap_or_default();
    let raw_item_id = parts.next();
    let custom = grammar.get(&call.name);
    let item_id = tool_call_item_id(raw_item_id, custom.is_some(), different_model);
    let mut item = Map::new();
    item.insert(
        "type".into(),
        json!(if custom.is_some() {
            "custom_tool_call"
        } else {
            "function_call"
        }),
    );
    if let Some(item_id) = item_id {
        item.insert("id".into(), json!(item_id));
    }
    item.insert("call_id".into(), json!(call_id));
    item.insert("name".into(), json!(call.name));
    match custom {
        Some(property) => item.insert(
            "input".into(),
            json!(grammar_tool_input(&call.name, &call.arguments, property)?),
        ),
        None => item.insert("arguments".into(), json!(jsrt::json::stringify(&call.arguments))),
    };
    if let (true, Some(namespace)) = (same_model, &call.namespace) {
        item.insert("namespace".into(), json!(namespace));
    }
    Ok(Value::Object(item))
}

fn convert_assistant_message(
    assistant: &AssistantMessage,
    model: &Model,
    msg_index: usize,
    grammar: &IndexMap<String, String>,
) -> Result<Vec<Value>, String> {
    let mut output = Vec::new();
    let same_provider = assistant.provider == model.provider && assistant.api == model.api;
    let same_model = same_provider && assistant.model == model.id;
    let different_model = same_provider && assistant.model != model.id;
    let mut text_index = 0;
    for block in &assistant.content {
        match block {
            AssistantContent::Thinking(thinking) => {
                if let Some(signature) = thinking.thinking_signature.as_deref().filter(|sig| !sig.is_empty()) {
                    output.push(parse_json(signature).map_err(|err| err.to_string())?);
                }
            }
            AssistantContent::Text(text) => {
                output.push(convert_assistant_text(text, msg_index, text_index));
                text_index += 1;
            }
            AssistantContent::ToolCall(call) => {
                output.push(convert_assistant_tool_call(call, same_model, different_model, grammar)?);
            }
        }
    }
    Ok(output)
}

fn convert_tool_result(result: &ToolResultMessage, model: &Model, grammar: &IndexMap<String, String>) -> Value {
    let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
    let kind = if grammar.contains_key(&result.tool_name) {
        "custom_tool_call_output"
    } else {
        "function_call_output"
    };
    json!({"type": kind, "call_id": call_id, "output": tool_result_output(model, &result.content)})
}

fn append_transformed_message(
    messages: &mut Vec<Value>,
    message: &Message,
    leading: bool,
    msg_index: usize,
    model: &Model,
    pass: &MessagePass<'_>,
) -> Result<bool, String> {
    match message {
        Message::System(system) => {
            append_system_message(messages, system, leading, msg_index, pass)?;
            Ok(!leading)
        }
        Message::User(user) => {
            let Some(converted) = convert_user_message(user) else {
                return Ok(false);
            };
            messages.push(converted);
            Ok(true)
        }
        Message::Assistant(assistant) => {
            let output = convert_assistant_message(assistant, model, msg_index, pass.options.grammar)?;
            if output.is_empty() {
                return Ok(false);
            }
            messages.extend(output);
            Ok(true)
        }
        Message::ToolResult(result) => {
            messages.push(convert_tool_result(result, model, pass.options.grammar));
            Ok(true)
        }
    }
}

/// `convertResponsesMessages`: the transcript as Responses input items.
pub fn convert_responses_messages(
    model: &Model,
    context: &TranscriptContext,
    options: &ConvertOptions<'_>,
    now: i64,
) -> Result<Vec<Value>, String> {
    let context = resolve_transcript(context, options.supports_mid_convo_system_messages);
    let allowed = options.tool_call_providers.contains(&model.provider.as_str());
    let normalize = |id: &str, source: &AssistantMessage| normalize_tool_call_id(id, source, model, allowed);
    let transformed = transform_messages(&context.messages, model, Some(&normalize), now);
    let tools = resolve_transcript_tools(
        &context.messages,
        options.supports_additional_tools || options.supports_tool_search,
    );
    let instruction_role = if model.reasoning && model.compat_bool("supportsDeveloperRole") != Some(false) {
        "developer"
    } else {
        "system"
    };
    let pass = MessagePass {
        options,
        instruction_role,
        anchors_additions: tools.anchors_additions,
    };
    let mut messages: Vec<Value> = Vec::new();
    let mut msg_index = 0;
    for (source_index, message) in transformed.iter().enumerate() {
        let leading = source_index == 0 && matches!(message, Message::System(_));
        if append_transformed_message(&mut messages, message, leading, msg_index, model, &pass)? {
            msg_index += 1;
        }
    }
    Ok(messages)
}
