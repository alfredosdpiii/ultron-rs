//! Prompt rendering, payload construction, and CLI argument/environment planning for `claude -p`.

use super::probe::ClaudeCliCapabilities;
use super::{ClaudeCodeOptions, Env, MAX_SYSTEM_PROMPT_ARG_CHARS, NEUTRAL_SYSTEM_PROMPT};
use crate::transcript::{current_system_message, system_message_text};
use crate::types::{AssistantContent, Mapped, Message, Model, TranscriptContext, UserContent, UserMessageContent};
use serde_json::{Map, Value, json};

fn push_text(blocks: &mut Vec<Value>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = blocks.last_mut()
        && last["type"] == "text"
    {
        let joined = format!("{}{text}", last["text"].as_str().unwrap_or_default());
        last["text"] = json!(joined);
        return;
    }
    blocks.push(json!({"type": "text", "text": text}));
}

fn push_content(blocks: &mut Vec<Value>, content: &[UserContent], images: bool) {
    for part in content {
        match part {
            UserContent::Text(text) => push_text(blocks, &text.text),
            UserContent::Image(image) if images => blocks.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": image.mime_type, "data": image.data},
            })),
            UserContent::Image(_) => push_text(blocks, "[image omitted: this model takes text only]"),
        }
    }
}

fn push_user(blocks: &mut Vec<Value>, content: &UserMessageContent, images: bool) {
    match content {
        UserMessageContent::Text(text) => push_text(blocks, text),
        UserMessageContent::Blocks(parts) => push_content(blocks, parts, images),
    }
}

/// `renderClaudeCodePrompt`: one user turn as is, a longer conversation as a text transcript.
pub fn render_claude_code_prompt(messages: &[Message], images: bool) -> Vec<Value> {
    let turns: Vec<&Message> = messages.iter().filter(|m| !matches!(m, Message::System(_))).collect();
    let mut blocks = Vec::new();
    if let [Message::User(user)] = turns.as_slice() {
        push_user(&mut blocks, &user.content, images);
        if blocks.is_empty() {
            push_text(&mut blocks, "(empty message)");
        }
        return blocks;
    }
    push_text(&mut blocks, "<conversation>\n");
    for message in turns {
        match message {
            Message::User(user) => {
                push_text(&mut blocks, "<user>\n");
                push_user(&mut blocks, &user.content, images);
                push_text(&mut blocks, "\n</user>\n");
            }
            Message::Assistant(assistant) => {
                let parts: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(text) => Some(text.text.clone()),
                        AssistantContent::ToolCall(call) => Some(format!(
                            "[tool call {} (id {}): {}]",
                            call.name,
                            call.id,
                            jsrt::json::stringify(&call.arguments)
                        )),
                        AssistantContent::Thinking(_) => None,
                    })
                    .collect();
                push_text(
                    &mut blocks,
                    &format!("<assistant>\n{}\n</assistant>\n", parts.join("\n")),
                );
            }
            Message::ToolResult(result) => {
                push_text(
                    &mut blocks,
                    &format!(
                        "<tool_result name=\"{}\" id=\"{}\"{}>\n",
                        result.tool_name,
                        result.tool_call_id,
                        if result.is_error { " error=\"true\"" } else { "" }
                    ),
                );
                push_content(&mut blocks, &result.content, images);
                push_text(&mut blocks, "\n</tool_result>\n");
            }
            Message::System(_) => {}
        }
    }
    push_text(
        &mut blocks,
        "</conversation>\n\nYou are the assistant in the conversation above. Write the assistant's next reply to the latest user message; reply with that text only.",
    );
    blocks
}

/// What the provider sends (`ClaudeCodePayload`), exposed to `onPayload`.
pub(super) fn payload(model: &Model, context: &TranscriptContext, options: &ClaudeCodeOptions) -> Value {
    let system = current_system_message(&context.messages).map_or_default(|m| system_message_text(&m));
    let mut payload = Map::new();
    payload.insert("model".into(), json!(model.id));
    payload.insert("system".into(), json!(system));
    payload.insert(
        "content".into(),
        Value::Array(render_claude_code_prompt(&context.messages, model.supports_images())),
    );
    payload.insert(
        "max_tokens".into(),
        json!(
            options
                .base
                .base
                .max_tokens
                .unwrap_or(model.max_tokens)
                .min(model.max_tokens)
        ),
    );
    if let Some(schema) = options.json_schema.as_ref().filter(|s| s.is_object()) {
        payload.insert("json_schema".into(), schema.clone());
    }
    if let Some(effort) = effort_for(model, options.base.reasoning.as_deref()) {
        payload.insert("effort".into(), json!(effort));
    }
    Value::Object(payload)
}

fn effort_for(model: &Model, level: Option<&str>) -> Option<String> {
    let level = level?;
    if !model.reasoning {
        return None;
    }
    match model.thinking_level(level) {
        Mapped::Unsupported => None,
        Mapped::Value(value) => Some(value.to_string()),
        Mapped::Missing => Some(level.to_string()),
    }
}

pub(super) struct TempPromptFile(pub(super) std::path::PathBuf);

impl Drop for TempPromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_temp_system_prompt(system: &str) -> std::io::Result<TempPromptFile> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = jsrt::fs::temp_dir().join(format!("ultron-claude-sys-{}-{id}.txt", std::process::id()));
    std::fs::write(&path, system)?;
    Ok(TempPromptFile(path))
}

pub(super) struct PlannedRequest {
    pub(super) args: Vec<String>,
    pub(super) env: Env,
    pub(super) message: Value,
    pub(super) structured: bool,
    pub(super) _system_prompt_file: Option<TempPromptFile>,
}

fn resolve_system_prompt(
    mut system: String,
    content: &mut Vec<Value>,
    capabilities: &ClaudeCliCapabilities,
    is_resume: bool,
) -> (String, Option<TempPromptFile>) {
    if system.is_empty() {
        system = NEUTRAL_SYSTEM_PROMPT.into();
    }
    if is_resume || system.encode_utf16().count() <= MAX_SYSTEM_PROMPT_ARG_CHARS {
        return (system, None);
    }
    if capabilities.flags.contains("--system-prompt-file")
        && let Ok(file) = write_temp_system_prompt(&system)
    {
        return (system, Some(file));
    }
    content.insert(
        0,
        json!({"type": "text", "text": format!("<instructions>\n{system}\n</instructions>\n\n")}),
    );
    (NEUTRAL_SYSTEM_PROMPT.into(), None)
}

fn resolve_schema_mode<'a>(
    payload: &'a Value,
    content: &mut Vec<Value>,
    capabilities: &ClaudeCliCapabilities,
    use_schema_flag: bool,
) -> Result<(Option<&'a Value>, bool), String> {
    let schema = payload.get("json_schema").filter(|s| !s.is_null());
    let structured = schema.is_some_and(|s| s.is_object() && s["type"] == "object") && use_schema_flag;
    if structured && !capabilities.flags.contains("--json-schema") {
        return Err(format!(
            "claude-code provider: this request needs --json-schema, which the installed Claude Code CLI ({}) does not list in `claude --help`.",
            capabilities.version
        ));
    }
    if let Some(schema) = schema
        && !structured
    {
        push_text(
            content,
            &format!(
                "\n\nReply with only one JSON value (no prose, no code fences) that satisfies this JSON schema: {}",
                jsrt::json::stringify(schema)
            ),
        );
    }
    Ok((schema, structured))
}

struct CliArgSpec<'a> {
    model_id: &'a str,
    resume_id: Option<&'a str>,
    create_id: Option<&'a str>,
    system: &'a str,
    system_prompt_file: Option<&'a TempPromptFile>,
    effort: Option<&'a str>,
    structured_schema: Option<&'a Value>,
}

fn build_cli_args(spec: &CliArgSpec<'_>) -> Vec<String> {
    let mut args: Vec<String> = [
        "--print",
        "--output-format=stream-json",
        "--input-format=stream-json",
        "--verbose",
        "--include-partial-messages",
        "--tools=",
        "--strict-mcp-config",
        "--mcp-config={\"mcpServers\":{}}",
        "--setting-sources=",
        "--permission-prompts=none",
        "--disable-slash-commands",
    ]
    .iter()
    .map(std::string::ToString::to_string)
    .collect();
    if let Some(id) = spec.resume_id {
        args.push(format!("--resume={id}"));
    } else if let Some(id) = spec.create_id {
        args.push(format!("--session-id={id}"));
    } else {
        args.push("--no-session-persistence".into());
    }
    args.push(format!("--model={}", spec.model_id));
    if spec.resume_id.is_none() {
        if let Some(file) = spec.system_prompt_file {
            args.push("--system-prompt-file".into());
            args.push(file.0.to_string_lossy().into_owned());
        } else {
            args.push(format!("--system-prompt={}", spec.system));
        }
    }
    args.push(match spec.effort {
        Some(effort) => format!("--effort={effort}"),
        None => "--settings={\"alwaysThinkingEnabled\":false}".into(),
    });
    if let Some(schema) = spec.structured_schema {
        args.push(format!("--json-schema={}", jsrt::json::stringify(schema)));
    }
    args
}

fn build_request_env(base_env: &Env, max_tokens: Option<u64>, default_max_tokens: u64) -> Env {
    let mut env = base_env.clone();
    for (k, v) in crate::claude_subscription::process::ALWAYS_QUIET
        .iter()
        .chain(crate::claude_subscription::process::CALLER_IDENTITY)
        .chain(&[
            ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
            ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ])
    {
        env.insert((*k).into(), (*v).into());
    }
    crate::claude_subscription::process::reap_orphaned_refresh_lock(&env);
    if let Some(max_tokens) = max_tokens
        && max_tokens > 0
        && max_tokens != default_max_tokens
    {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), max_tokens.to_string());
    }
    env
}

pub(super) fn plan_request(
    model: &Model,
    payload: &Value,
    base_env: &Env,
    default_max_tokens: u64,
    capabilities: &ClaudeCliCapabilities,
    use_schema_flag: bool,
    session_id: Option<&str>,
) -> Result<PlannedRequest, String> {
    let mut content: Vec<Value> = payload["content"].as_array().cloned().unwrap_or_default();
    let resume_id = session_id
        .and_then(|s| s.strip_prefix("resume:"))
        .filter(|s| !s.is_empty());
    let create_id = session_id
        .and_then(|s| s.strip_prefix("create:"))
        .filter(|s| !s.is_empty());
    let raw_system = payload["system"].as_str().unwrap_or_default().trim().to_string();
    let (system, system_prompt_file) =
        resolve_system_prompt(raw_system, &mut content, capabilities, resume_id.is_some());
    let (schema, structured) = resolve_schema_mode(payload, &mut content, capabilities, use_schema_flag)?;
    let effort = payload.get("effort").and_then(Value::as_str).filter(|e| !e.is_empty());
    if effort.is_some() && !capabilities.flags.contains("--effort") {
        return Err(format!(
            "claude-code provider: a thinking level needs --effort, which the installed Claude Code CLI ({}) does not list in `claude --help`.",
            capabilities.version
        ));
    }
    let model_id = payload["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(&model.id);
    let args = build_cli_args(&CliArgSpec {
        model_id,
        resume_id,
        create_id,
        system: &system,
        system_prompt_file: system_prompt_file.as_ref(),
        effort,
        structured_schema: structured.then_some(schema).flatten(),
    });
    let env = build_request_env(base_env, payload["max_tokens"].as_u64(), default_max_tokens);
    Ok(PlannedRequest {
        args,
        env,
        message: json!({"type": "user", "message": {"role": "user", "content": content}}),
        structured,
        _system_prompt_file: system_prompt_file,
    })
}
