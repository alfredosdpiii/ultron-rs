//! SSE streaming loop, event handling, and `stream` / `stream_simple` entry points for Anthropic Messages.

use super::headers::client_headers;
use super::messages::from_claude_code_name;
use super::params::build_params;
use super::{AnthropicOptions, Compat, REFUSAL_TEXT, assert_request_auth, fallback_models};
use crate::http::{AbortSignal, Body, Failure, HttpRequest, Sdk, Transport, is_aborted, join_url, send_with_retry};
use crate::models::calculate_cost;
use crate::options::{
    SimpleStreamOptions, adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::provider::{REQUEST_WAS_ABORTED, Run, next_chunk};
use crate::sse::{ServerSentEvent, SseDecoder};
use crate::transcript::{current_tools, resolve_transcript};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, CacheRetention, EventSink,
    Mapped, Model, ModelCost, StopReason, TextContent, ThinkingContent, Tool, ToolCall, TranscriptContext, Usage,
};
use jsrt::json_parse::{parse_json_with_repair, parse_streaming_json};
use serde_json::{Map, Value, json};

const MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

fn stop_reason(reason: &str, details: Option<&Value>) -> Result<(StopReason, Option<String>), String> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = details
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty());
            (StopReason::Error, Some(explanation.unwrap_or(REFUSAL_TEXT).to_string()))
        }
        "sensitive" => (StopReason::Error, Some("Provider stopped with: sensitive".into())),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

/// `x || 0` for a token count.
fn tokens(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn block_str(block: &Value, key: &str) -> String {
    block.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn empty_json_object() -> Value {
    Value::Object(Map::new())
}

fn fallback_model_cost(model: &Model, response_model: &str) -> Option<ModelCost> {
    fallback_models(model)
        .into_iter()
        .find(|f| {
            f.get("provider").and_then(Value::as_str) == Some(&model.provider)
                && f.get("model").and_then(Value::as_str) == Some(response_model)
        })
        .and_then(|f| serde_json::from_value::<ModelCost>(f["cost"].clone()).ok())
}

fn set_usage_field(usage: &Value, slot: &mut u64, key: &str) {
    if let Some(v) = usage.get(key).and_then(Value::as_u64) {
        *slot = v;
    }
}

fn apply_message_delta_usage(usage: &Value, target: &mut Usage) {
    set_usage_field(usage, &mut target.input, "input_tokens");
    set_usage_field(usage, &mut target.output, "output_tokens");
    set_usage_field(usage, &mut target.cache_read, "cache_read_input_tokens");
    set_usage_field(usage, &mut target.cache_write, "cache_creation_input_tokens");
    if let Some(thinking) = usage
        .pointer("/output_tokens_details/thinking_tokens")
        .and_then(Value::as_u64)
    {
        target.reasoning = Some(thinking);
    }
}

struct MessagesStream {
    /// Upstream's per-block `index` (Anthropic content index), cleared on `content_block_stop`.
    indices: Vec<Option<u64>>,
    /// Upstream's per-tool-call `partialJson` scratch buffer.
    partial_json: Vec<String>,
    usage_cost: ModelCost,
    usage_model_id: String,
    transformations: Option<Vec<Value>>,
    saw_start: bool,
    saw_stop: bool,
}

impl Run<'_, '_, MessagesStream> {
    fn cost(&mut self, model: &Model) {
        let mut usage_model = model.clone();
        usage_model.cost = self.state.usage_cost.clone();
        usage_model.id = self.state.usage_model_id.clone();
        calculate_cost(&usage_model, &mut self.output.usage);
    }

    fn block_position(&self, index: Option<u64>) -> Option<usize> {
        index.and_then(|index| self.state.indices.iter().position(|i| *i == Some(index)))
    }

    fn push_block(&mut self, block: AssistantContent, index: Option<u64>) -> usize {
        self.output.content.push(block);
        self.state.indices.push(index);
        self.state.partial_json.push(String::new());
        self.output.content.len() - 1
    }

    fn handle_sse(&mut self, sse: &ServerSentEvent, model: &Model, oauth: bool, tools: &[Tool]) -> Result<(), String> {
        if sse.event.as_deref() == Some("error") {
            return Err(sse.data.clone());
        }
        if !MESSAGE_EVENTS.contains(&sse.event.as_deref().unwrap_or("")) {
            return Ok(());
        }
        let event = parse_json_with_repair(&sse.data).map_err(|e| {
            format!(
                "Could not parse Anthropic SSE event {}: {e}; data={}; raw={}",
                sse.event.as_deref().unwrap_or("null"),
                sse.data,
                sse.raw.join("\\n")
            )
        })?;
        self.handle_event(&event, model, oauth, tools)
    }

    fn handle_message_start(&mut self, event: &Value, model: &Model) {
        self.state.saw_start = true;
        let message = &event["message"];
        self.output.response_id = message.get("id").and_then(Value::as_str).map(str::to_string);
        if let Some(Value::Array(t)) = message.get("input_transformations") {
            self.state.transformations = Some(t.clone());
        }
        let response_model = block_str(message, "model");
        let fallback_cost = if response_model == model.id {
            None
        } else {
            self.output.response_model = Some(response_model.clone());
            fallback_model_cost(model, &response_model)
        };
        (self.state.usage_cost, self.state.usage_model_id) = match fallback_cost {
            Some(cost) => (cost, response_model),
            None => (model.cost.clone(), model.id.clone()),
        };
        let usage = &message["usage"];
        let u = &mut self.output.usage;
        u.input = tokens(usage.get("input_tokens"));
        u.output = tokens(usage.get("output_tokens"));
        u.cache_read = tokens(usage.get("cache_read_input_tokens"));
        u.cache_write = tokens(usage.get("cache_creation_input_tokens"));
        u.cache_write_1h = Some(tokens(usage.pointer("/cache_creation/ephemeral_1h_input_tokens")));
        u.total_tokens = u.input + u.output + u.cache_read + u.cache_write;
        self.cost(model);
    }

    fn handle_content_block_start(
        &mut self,
        event: &Value,
        index: Option<u64>,
        oauth: bool,
        tools: &[Tool],
    ) -> Result<(), String> {
        let block = &event["content_block"];
        match block.get("type").and_then(Value::as_str).unwrap_or_default() {
            "fallback" => {
                if !self.output.content.is_empty() {
                    return Err("Anthropic performed an unsupported mid-output model fallback".into());
                }
            }
            "text" => {
                let position = self.push_block(
                    AssistantContent::Text(TextContent {
                        text: block_str(block, "text"),
                        text_signature: None,
                    }),
                    index,
                );
                self.emit(|partial| AssistantMessageEvent::TextStart {
                    content_index: position,
                    partial,
                });
            }
            "thinking" => {
                let thinking = ThinkingContent {
                    thinking: block_str(block, "thinking"),
                    thinking_signature: Some(block_str(block, "signature")),
                    redacted: None,
                };
                let position = self.push_block(AssistantContent::Thinking(thinking), index);
                self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                    content_index: position,
                    partial,
                });
            }
            "redacted_thinking" => {
                let thinking = ThinkingContent {
                    thinking: "[Reasoning redacted]".into(),
                    thinking_signature: block.get("data").and_then(Value::as_str).map(str::to_string),
                    redacted: Some(true),
                };
                let position = self.push_block(AssistantContent::Thinking(thinking), index);
                self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                    content_index: position,
                    partial,
                });
            }
            "tool_use" => {
                let name = block_str(block, "name");
                let call = ToolCall {
                    id: block_str(block, "id"),
                    name: if oauth {
                        from_claude_code_name(&name, tools)
                    } else {
                        name
                    },
                    arguments: block
                        .get("input")
                        .filter(|i| !i.is_null())
                        .cloned()
                        .unwrap_or_else(empty_json_object),
                    thought_signature: None,
                    namespace: None,
                };
                let position = self.push_block(AssistantContent::ToolCall(call), index);
                self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                    content_index: position,
                    partial,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_content_block_delta(&mut self, event: &Value, index: Option<u64>) {
        let Some(position) = self.block_position(index) else {
            return;
        };
        let delta = &event["delta"];
        match (
            delta.get("type").and_then(Value::as_str).unwrap_or_default(),
            &mut self.output.content[position],
        ) {
            ("text_delta", AssistantContent::Text(block)) => {
                let text = block_str(delta, "text");
                block.text.push_str(&text);
                self.emit(|partial| AssistantMessageEvent::TextDelta {
                    content_index: position,
                    delta: text,
                    partial,
                });
            }
            ("thinking_delta", AssistantContent::Thinking(block)) => {
                let thinking = block_str(delta, "thinking");
                block.thinking.push_str(&thinking);
                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                    content_index: position,
                    delta: thinking,
                    partial,
                });
            }
            ("input_json_delta", AssistantContent::ToolCall(call)) => {
                let json = block_str(delta, "partial_json");
                self.state.partial_json[position].push_str(&json);
                call.arguments = parse_streaming_json(&self.state.partial_json[position]);
                self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                    content_index: position,
                    delta: json,
                    partial,
                });
            }
            ("signature_delta", AssistantContent::Thinking(block)) => {
                block
                    .thinking_signature
                    .get_or_insert_with(String::new)
                    .push_str(&block_str(delta, "signature"));
            }
            _ => {}
        }
    }

    fn handle_content_block_stop(&mut self, index: Option<u64>) {
        let Some(position) = self.block_position(index) else {
            return;
        };
        self.state.indices[position] = None;
        if let AssistantContent::ToolCall(call) = &mut self.output.content[position] {
            call.arguments = parse_streaming_json(&self.state.partial_json[position]);
            let tool_call = call.clone();
            self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                content_index: position,
                tool_call,
                partial,
            });
            return;
        }
        crate::claude_code_cli::emit_text_or_thinking_end(self, position);
    }

    fn handle_message_delta(&mut self, event: &Value, model: &Model) -> Result<(), String> {
        if let Some(Value::Array(t)) = event.get("input_transformations") {
            self.state.transformations = Some(t.clone());
        }
        if let Some(reason) = event
            .pointer("/delta/stop_reason")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_string());
            let (stop, message) = stop_reason(reason, event.pointer("/delta/stop_details"))?;
            self.output.stop_reason = stop;
            if message.is_some() {
                self.output.error_message = message;
            }
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            apply_message_delta_usage(usage, &mut self.output.usage);
        }
        let u = &mut self.output.usage;
        u.total_tokens = u.input + u.output + u.cache_read + u.cache_write;
        self.cost(model);
        Ok(())
    }

    fn handle_event(&mut self, event: &Value, model: &Model, oauth: bool, tools: &[Tool]) -> Result<(), String> {
        let index = event.get("index").and_then(Value::as_u64);
        match event.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => self.handle_message_start(event, model),
            "message_stop" => self.state.saw_stop = true,
            "content_block_start" => return self.handle_content_block_start(event, index, oauth, tools),
            "content_block_delta" => self.handle_content_block_delta(event, index),
            "content_block_stop" => self.handle_content_block_stop(index),
            "message_delta" => return self.handle_message_delta(event, model),
            _ => {}
        }
        Ok(())
    }
}

/// Stream one Anthropic Messages response. Failures end the stream with an `error` event and an `error`/`aborted`
/// message, as upstream.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &AnthropicOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let compat = Compat::of(model);
    let context = resolve_transcript(context, compat.mid_convo_system_messages);
    let tools = current_tools(&context.messages);
    let now = jsrt::date::now_ms();
    let state = MessagesStream {
        indices: Vec::new(),
        partial_json: Vec::new(),
        usage_cost: model.cost.clone(),
        usage_model_id: model.id.clone(),
        transformations: None,
        saw_start: false,
        saw_stop: false,
    };
    let mut run = Run::new(AssistantMessage::pending(model, now), sink, state);
    if compat.managed_effort {
        run.output.provider_thinking_level = Some(options.effort.as_deref().unwrap_or("high").to_string());
    }
    let signal = options.base.signal();
    let result = drive(&mut run, model, &context, options, transport, &tools, now).await;
    run.end(result.map_err(|failure| failure.message), signal)
}

fn build_http_request(
    model: &Model,
    context: &TranscriptContext,
    options: &AnthropicOptions,
    now: i64,
) -> Result<(HttpRequest, bool), Failure> {
    let base = &options.base;
    let session_id = if base.cache_retention() == CacheRetention::None {
        None
    } else {
        base.session_id.as_deref()
    };
    let (mut headers, oauth) = client_headers(model, context, base, session_id);
    let mut params = Value::Object(build_params(model, context, oauth, options, now).map_err(Failure::plain)?);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
        params["stream"] = json!(true);
    }
    if let Some(betas) = params.as_object_mut().and_then(|p| p.shift_remove("betas")) {
        let list: Vec<String> = betas
            .as_array()
            .into_iter()
            .flatten()
            .map(|b| b.as_str().map_or_else(|| b.to_string(), str::to_string))
            .collect();
        headers.set("anthropic-beta", list.join(","));
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: join_url(&model.base_url, "/v1/messages?beta=true"),
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    Ok((request, oauth))
}

fn decode_sse_events(
    run: &mut Run<'_, '_, MessagesStream>,
    events: Vec<ServerSentEvent>,
    model: &Model,
    oauth: bool,
    tools: &[Tool],
) -> Result<(), Failure> {
    for sse in events {
        run.handle_sse(&sse, model, oauth, tools).map_err(Failure::plain)?;
    }
    Ok(())
}

async fn read_sse_stream(
    run: &mut Run<'_, '_, MessagesStream>,
    body: &mut dyn Body,
    signal: Option<&AbortSignal>,
    model: &Model,
    oauth: bool,
    tools: &[Tool],
) -> Result<(), Failure> {
    let mut decoder = SseDecoder::default();
    loop {
        if is_aborted(signal) {
            return Err(Failure::plain(REQUEST_WAS_ABORTED));
        }
        let Some(bytes) = next_chunk::<Failure>(body, signal).await? else {
            break;
        };
        decode_sse_events(run, decoder.push(&bytes), model, oauth, tools)?;
    }
    decode_sse_events(run, decoder.finish(true), model, oauth, tools)?;
    if run.state.saw_start && !run.state.saw_stop {
        return Err(Failure::plain("Anthropic stream ended before message_stop"));
    }
    Ok(())
}

fn format_transformation_entry(t: &Value) -> Value {
    let mut entry = Map::new();
    for key in ["type", "path", "reason"] {
        if let Some(value) = t.get(key).filter(|v| !v.is_null()) {
            entry.insert(key.into(), value.clone());
        }
    }
    Value::Object(entry)
}

fn record_input_transformations(run: &mut Run<'_, '_, MessagesStream>) {
    let Some(transformations) = run.state.transformations.take().filter(|t| !t.is_empty()) else {
        return;
    };
    let list: Vec<Value> = transformations.iter().map(format_transformation_entry).collect();
    let mut details = Map::new();
    details.insert("transformations".into(), Value::Array(list));
    run.output
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(AssistantMessageDiagnostic {
            kind: "anthropic_input_transformations".into(),
            timestamp: jsrt::date::now_ms(),
            error: None,
            details: Some(details),
        });
}

async fn drive(
    run: &mut Run<'_, '_, MessagesStream>,
    model: &Model,
    context: &TranscriptContext,
    options: &AnthropicOptions,
    transport: &dyn Transport,
    tools: &[Tool],
    now: i64,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    assert_request_auth(model, base).map_err(Failure::plain)?;
    let (request, oauth) = build_http_request(model, context, options, now)?;
    let mut response = send_with_retry(transport, &request, Sdk::Anthropic, base.retry(), signal).await?;
    run.start();
    read_sse_stream(run, response.body.as_mut(), signal, model, oauth, tools).await?;
    run.stopped(signal, "Anthropic stream ended without a stop reason")
        .map_err(Failure::plain)?;
    record_input_transformations(run);
    Ok(())
}

fn effort_for_level(model: &Model, level: &str) -> String {
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

/// `streamSimple`: provider-neutral reasoning mapped to adaptive effort or a thinking budget. `Err` is upstream's
/// synchronous throw (missing auth), before any event.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    assert_request_auth(model, &options.base)?;
    let base = build_base_options(model, context, options);
    let mut anthropic = AnthropicOptions {
        tool_choice: options.tool_choice.clone().map(Value::String),
        ..AnthropicOptions {
            base,
            ..Default::default()
        }
    };
    match options.reasoning.as_deref() {
        None => anthropic.thinking_enabled = Some(false),
        Some(level) if model.compat_bool("forceAdaptiveThinking") == Some(true) => {
            anthropic.thinking_enabled = Some(true);
            anthropic.effort = Some(effort_for_level(model, level));
        }
        Some(level) => {
            let (adjusted, budget) = adjust_max_tokens_for_thinking(
                anthropic.base.max_tokens,
                model.max_tokens,
                level,
                options.thinking_budgets.as_ref(),
            );
            let max_tokens = clamp_max_tokens_to_context(model, context, adjusted);
            anthropic.base.max_tokens = Some(max_tokens);
            anthropic.thinking_enabled = Some(true);
            anthropic.thinking_budget_tokens =
                Some(crate::options::clamp_thinking_budget_to_answer_room(budget, max_tokens));
        }
    }
    Ok(stream(model, context, &anthropic, transport, sink).await)
}
