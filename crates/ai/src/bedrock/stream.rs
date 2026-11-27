//! Event-stream decoding, retry/HTTP1 selection, and `stream` / `stream_simple` entry points for Amazon Bedrock.

use super::BedrockOptions;
use super::error::{
    BedrockError, abort_error, diagnostic_value, failure_diagnostic, format_error, http_error, request_id, stream_error,
};
use super::payload::{REQUEST_MEMBERS, command_input, is_anthropic_claude_model, supports_adaptive_thinking};
use super::target::{Auth, Target, build_request, chain_credentials, resolve_target};
use crate::aws::{EventMessage, EventStreamDecoder, backoff_delay_ms};
use crate::env::provider_env_value;
use crate::http::{AbortSignal, HttpRequest, HttpResponse, Transport, cancelled, is_aborted};
use crate::models::calculate_cost;
use crate::options::{
    SimpleStreamOptions, adjust_max_tokens_for_thinking, build_base_options, clamp_max_tokens_to_context,
};
use crate::provider::{Run, next_chunk};
use crate::transcript::collapse_system_messages;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, EventSink, Model, StopReason, TextContent,
    ThinkingContent, ToolCall, TranscriptContext,
};
use jsrt::json_parse::{parse_json, parse_streaming_json};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::Arc;

const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

fn str_field(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn cache_write_1h(usage: &Value) -> Option<u64> {
    usage.get("cacheDetails").and_then(Value::as_array).map(|details| {
        details
            .iter()
            .filter(|d| d.get("ttl").and_then(Value::as_str) == Some("1h"))
            .map(|d| u64_field(d, "inputTokens"))
            .sum()
    })
}

fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("end_turn" | "stop_sequence") => (StopReason::Stop, None),
        Some("max_tokens" | "model_context_window_exceeded") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some(other) if !other.is_empty() => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
        _ => (StopReason::Error, None),
    }
}

/// A content block being streamed, keyed by Bedrock's `contentBlockIndex`.
struct Block {
    index: Option<i64>,
    partial_json: Option<String>,
    redacted_chunks: Option<Vec<u8>>,
}

struct ConverseStream {
    blocks: Vec<Block>,
}

impl Run<'_, '_, ConverseStream> {
    fn position(&self, index: i64) -> Option<usize> {
        self.state.blocks.iter().position(|b| b.index == Some(index))
    }

    fn finalize(&mut self, position: usize) {
        let block = &mut self.state.blocks[position];
        block.index = None;
        block.partial_json = None;
        if let Some(bytes) = block.redacted_chunks.take()
            && let AssistantContent::Thinking(thinking) = &mut self.output.content[position]
        {
            thinking.thinking_signature = Some(jsrt::base64::encode(&bytes));
        }
    }

    fn finalize_all(&mut self) {
        for position in 0..self.state.blocks.len() {
            self.finalize(position);
        }
    }

    fn push(&mut self, content: AssistantContent, block: Block) -> usize {
        self.output.content.push(content);
        self.state.blocks.push(block);
        self.output.content.len() - 1
    }

    fn handle_message_start(&mut self, event: &Value) -> Result<(), BedrockError> {
        if event.get("role").and_then(Value::as_str) != Some("assistant") {
            return Err(BedrockError::plain(
                "Unexpected assistant message start but got user message start instead",
            ));
        }
        self.emit(|partial| AssistantMessageEvent::Start { partial });
        Ok(())
    }

    fn handle_content_block_start(&mut self, event: &Value) {
        let index = event
            .get("contentBlockIndex")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(tool) = event.pointer("/start/toolUse") else {
            return;
        };
        let position = self.push(
            AssistantContent::ToolCall(ToolCall {
                id: str_field(tool, "toolUseId"),
                name: str_field(tool, "name"),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            }),
            Block {
                index: Some(index),
                partial_json: Some(String::new()),
                redacted_chunks: None,
            },
        );
        self.emit(|partial| AssistantMessageEvent::ToolCallStart {
            content_index: position,
            partial,
        });
    }

    fn stop_thinking_block(&mut self, position: usize) {
        self.finalize(position);
        if let AssistantContent::Thinking(thinking) = &self.output.content[position] {
            let content = thinking.thinking.clone();
            self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                content_index: position,
                content,
                partial,
            });
        }
    }

    fn stop_tool_call_block(&mut self, position: usize) {
        let json_text = self.state.blocks[position].partial_json.take().unwrap_or_default();
        let arguments = parse_streaming_json(&json_text);
        if let AssistantContent::ToolCall(call) = &mut self.output.content[position] {
            call.arguments = arguments;
            let tool_call = call.clone();
            self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                content_index: position,
                tool_call,
                partial,
            });
        }
    }

    fn handle_content_block_stop(&mut self, event: &Value) {
        let index = event
            .get("contentBlockIndex")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(position) = self.position(index) else {
            return;
        };
        self.state.blocks[position].index = None;
        match &self.output.content[position] {
            AssistantContent::Text(text) => {
                let content = text.text.clone();
                self.emit(|partial| AssistantMessageEvent::TextEnd {
                    content_index: position,
                    content,
                    partial,
                });
            }
            AssistantContent::Thinking(_) => self.stop_thinking_block(position),
            AssistantContent::ToolCall(_) => self.stop_tool_call_block(position),
        }
    }

    fn handle_message_stop(&mut self, event: &Value) {
        let reason = event.get("stopReason").and_then(Value::as_str);
        self.output.raw_stop_reason = reason.map(str::to_string);
        let (stop, message) = map_stop_reason(reason);
        self.output.stop_reason = stop;
        if message.is_some() {
            self.output.error_message = message;
        }
    }

    fn handle_metadata(&mut self, event: &Value, model: &Model) {
        let Some(usage) = event.get("usage").filter(|u| u.is_object()) else {
            return;
        };
        let out = &mut self.output.usage;
        out.input = u64_field(usage, "inputTokens");
        out.output = u64_field(usage, "outputTokens");
        out.cache_read = u64_field(usage, "cacheReadInputTokens");
        out.cache_write = u64_field(usage, "cacheWriteInputTokens");
        out.cache_write_1h = cache_write_1h(usage);
        let total = u64_field(usage, "totalTokens");
        out.total_tokens = if total > 0 { total } else { out.input + out.output };
        calculate_cost(model, out);
    }

    fn handle(&mut self, kind: &str, event: &Value, model: &Model) -> Result<(), BedrockError> {
        match kind {
            "messageStart" => return self.handle_message_start(event),
            "contentBlockStart" => self.handle_content_block_start(event),
            "contentBlockDelta" => return self.delta(event),
            "contentBlockStop" => self.handle_content_block_stop(event),
            "messageStop" => self.handle_message_stop(event),
            "metadata" => self.handle_metadata(event, model),
            _ => {}
        }
        Ok(())
    }

    fn ensure_text_block(&mut self, index: i64, position: Option<usize>) -> usize {
        if let Some(p) = position {
            return p;
        }
        let p = self.push(
            AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }),
            Block {
                index: Some(index),
                partial_json: None,
                redacted_chunks: None,
            },
        );
        self.emit(|partial| AssistantMessageEvent::TextStart {
            content_index: p,
            partial,
        });
        p
    }

    fn delta_text(&mut self, index: i64, position: Option<usize>, text: &str) {
        let position = self.ensure_text_block(index, position);
        if let AssistantContent::Text(block) = &mut self.output.content[position] {
            block.text.push_str(text);
            let delta = text.to_string();
            self.emit(|partial| AssistantMessageEvent::TextDelta {
                content_index: position,
                delta,
                partial,
            });
        }
    }

    fn delta_tool_use(&mut self, position: usize, tool: &Value) {
        if !matches!(self.output.content[position], AssistantContent::ToolCall(_)) {
            return;
        }
        let input = str_field(tool, "input");
        let buffer = self.state.blocks[position].partial_json.get_or_insert_with(String::new);
        buffer.push_str(&input);
        let arguments = parse_streaming_json(buffer);
        if let AssistantContent::ToolCall(call) = &mut self.output.content[position] {
            call.arguments = arguments;
        }
        self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
            content_index: position,
            delta: input,
            partial,
        });
    }

    fn ensure_thinking_block(&mut self, index: i64, position: Option<usize>) -> usize {
        if let Some(p) = position {
            return p;
        }
        let p = self.push(
            AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some(String::new()),
                redacted: None,
            }),
            Block {
                index: Some(index),
                partial_json: None,
                redacted_chunks: None,
            },
        );
        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
            content_index: p,
            partial,
        });
        p
    }

    fn apply_reasoning_text(&mut self, position: usize, reasoning: &Value) {
        let Some(text) = reasoning.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
            return;
        };
        if let AssistantContent::Thinking(block) = &mut self.output.content[position] {
            block.thinking.push_str(text);
        }
        let delta = text.to_string();
        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
            content_index: position,
            delta,
            partial,
        });
    }

    fn apply_reasoning_signature(&mut self, position: usize, reasoning: &Value) {
        if let Some(signature) = reasoning
            .get("signature")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            && let AssistantContent::Thinking(block) = &mut self.output.content[position]
            && block.redacted != Some(true)
        {
            block
                .thinking_signature
                .get_or_insert_with(String::new)
                .push_str(signature);
        }
    }

    fn apply_redacted_reasoning(&mut self, position: usize, reasoning: &Value) {
        let Some(redacted) = reasoning.get("redactedContent").and_then(Value::as_str) else {
            return;
        };
        let bytes = jsrt::base64::atob(redacted).unwrap_or_default();
        if bytes.is_empty() {
            return;
        }
        let first = matches!(
            &self.output.content[position],
            AssistantContent::Thinking(b) if b.redacted != Some(true)
        );
        if first {
            if let AssistantContent::Thinking(block) = &mut self.output.content[position] {
                block.redacted = Some(true);
                block.thinking_signature = Some(String::new());
                block.thinking.push_str(REDACTED_THINKING_PLACEHOLDER);
            }
            self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                content_index: position,
                delta: REDACTED_THINKING_PLACEHOLDER.into(),
                partial,
            });
        }
        self.state.blocks[position]
            .redacted_chunks
            .get_or_insert_with(Vec::new)
            .extend(bytes);
    }

    fn delta_reasoning(&mut self, index: i64, position: Option<usize>, reasoning: &Value) {
        let p = self.ensure_thinking_block(index, position);
        if !matches!(self.output.content[p], AssistantContent::Thinking(_)) {
            return;
        }
        self.apply_reasoning_text(p, reasoning);
        self.apply_reasoning_signature(p, reasoning);
        self.apply_redacted_reasoning(p, reasoning);
    }

    fn delta(&mut self, event: &Value) -> Result<(), BedrockError> {
        let index = event
            .get("contentBlockIndex")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(delta) = event.get("delta") else {
            return Ok(());
        };
        let position = self.position(index);
        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            self.delta_text(index, position, text);
        } else if let (Some(tool), Some(p)) = (delta.get("toolUse"), position) {
            self.delta_tool_use(p, tool);
        } else if let Some(reasoning) = delta.get("reasoningContent").filter(|r| !r.is_null()) {
            self.delta_reasoning(index, position, reasoning);
        }
        Ok(())
    }
}

async fn read_body_text(response: &mut HttpResponse) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.body.next_chunk().await {
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// An open event stream: the decoder with the messages read so far.
struct OpenStream {
    response: HttpResponse,
    decoder: EventStreamDecoder,
    pending: VecDeque<EventMessage>,
    done: bool,
    status: u16,
    request_id: Option<String>,
}

impl OpenStream {
    async fn next(&mut self, signal: Option<&AbortSignal>) -> Result<Option<EventMessage>, BedrockError> {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return Ok(Some(message));
            }
            if self.done {
                return Ok(None);
            }
            let Some(bytes) = next_chunk::<BedrockError>(self.response.body.as_mut(), signal).await? else {
                self.done = true;
                self.decoder.finish().map_err(BedrockError::plain)?;
                continue;
            };
            self.pending
                .extend(self.decoder.push(&bytes).map_err(BedrockError::plain)?);
        }
    }
}

/// One attempt: send, turn an error status into the protocol's exception, and read the first stream message (the
/// SDK reads it before `send` resolves, so its exceptions are retried like HTTP errors).
async fn attempt(
    transport: &dyn Transport,
    request: &HttpRequest,
    signal: Option<&AbortSignal>,
) -> Result<OpenStream, BedrockError> {
    if is_aborted(signal) {
        return Err(abort_error());
    }
    let sent = tokio::select! {
        sent = transport.send(request.clone()) => sent,
        () = cancelled(signal) => return Err(abort_error()),
    };
    let mut response = sent.map_err(|message| BedrockError {
        transient: true,
        ..BedrockError::plain(message)
    })?;
    let request_id = request_id(&response.headers);
    if !(200..300).contains(&response.status) {
        let text = read_body_text(&mut response).await;
        return Err(http_error(response.status, &response.headers, &text));
    }
    let status = response.status;
    let mut stream = OpenStream {
        response,
        decoder: EventStreamDecoder::default(),
        pending: Default::default(),
        done: false,
        status,
        request_id,
    };
    if let Some(first) = stream.next(signal).await? {
        if matches!(
            first.headers.get(":message-type").map(String::as_str),
            Some("exception" | "error")
        ) {
            return Err(stream_error(&first, true, stream.status, stream.request_id.as_deref()));
        }
        stream.pending.push_front(first);
    }
    Ok(stream)
}

fn max_attempts(options: &BedrockOptions) -> u32 {
    provider_env_value("AWS_MAX_ATTEMPTS", options.base.env.as_ref())
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

async fn send_with_retry(
    transport: &dyn Transport,
    request: &HttpRequest,
    options: &BedrockOptions,
) -> Result<OpenStream, BedrockError> {
    let signal = options.base.signal();
    let attempts = max_attempts(options);
    crate::provider::retry(
        signal,
        abort_error,
        |_| attempt(transport, request, signal),
        |error, retries| {
            if retries + 1 < attempts && error.retryable() && !is_aborted(signal) {
                Ok(backoff_delay_ms(retries, error.throttling()) as f64)
            } else {
                Err(error)
            }
        },
    )
    .await
}

/// Stream one Converse response; failures end with an `error` event and a `bedrock_response_failure` diagnostic.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &BedrockOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let context = collapse_system_messages(context);
    let mut output = AssistantMessage::pending(model, jsrt::date::now_ms());
    output.api = "bedrock-converse-stream".into();
    let mut run = Run::new(output, sink, ConverseStream { blocks: Vec::new() });
    let mut response_id: Option<String> = None;
    let result = drive(&mut run, model, &context, options, transport, &mut response_id).await;
    let signal = options.base.signal();
    run.finalize_all();
    let result = result.map_err(|error| {
        if !is_aborted(signal)
            && let Some(diagnostic) = failure_diagnostic(&error, response_id.as_deref())
        {
            run.output.diagnostics.get_or_insert_with(Vec::new).push(diagnostic);
        }
        format_error(&error)
    });
    run.end(result, signal)
}

async fn resolve_chain_auth(
    mut target: Target,
    options: &BedrockOptions,
    transport: &dyn Transport,
) -> Result<Target, BedrockError> {
    if let Auth::Chain(profile) = &target.auth {
        let credentials = chain_credentials(&target, profile.as_deref(), options, transport).await?;
        target.auth = Auth::SigV4(credentials);
    }
    Ok(target)
}

fn prepare_request_payload(
    model: &Model,
    context: &TranscriptContext,
    options: &BedrockOptions,
) -> Result<(Map<String, Value>, Model), BedrockError> {
    let mut input = Value::Object(command_input(model, context, options).map_err(BedrockError::plain)?);
    if let Some(next) = options.base.on_payload.as_ref().and_then(|hook| hook(&input, model)) {
        input = next;
    }
    let mut body = Map::new();
    for member in REQUEST_MEMBERS {
        if let Some(value) = input.get(*member).filter(|v| !v.is_null()) {
            body.insert((*member).to_string(), value.clone());
        }
    }
    let model_id = input
        .get("modelId")
        .and_then(Value::as_str)
        .unwrap_or(&model.id)
        .to_string();
    let request_model = Model {
        id: model_id,
        ..model.clone()
    };
    Ok((body, request_model))
}

fn select_http1_transport(
    model: &Model,
    options: &BedrockOptions,
    transport: &dyn Transport,
) -> Result<Option<Arc<dyn Transport>>, BedrockError> {
    let proxied = crate::net::node_proxy_for_target(&model.base_url, options.base.env.as_ref())
        .map_err(BedrockError::plain)?
        .is_some();
    let force_http1 = provider_env_value("AWS_BEDROCK_FORCE_HTTP1", options.base.env.as_ref()).as_deref() == Some("1");
    if proxied || force_http1 {
        Ok(transport.http1_only())
    } else {
        Ok(None)
    }
}

fn process_stream_message(
    run: &mut Run<'_, '_, ConverseStream>,
    message: &EventMessage,
    stream_status: u16,
    stream_request_id: Option<&str>,
    model: &Model,
) -> Result<(), BedrockError> {
    match message.headers.get(":message-type").map(String::as_str) {
        Some("event") => {
            let kind = message
                .headers
                .get(":event-type")
                .map_or("", String::as_str)
                .to_string();
            let event = parse_json(&String::from_utf8_lossy(&message.body)).unwrap_or(Value::Null);
            run.handle(&kind, &event, model)
        }
        Some("exception" | "error") => Err(stream_error(message, false, stream_status, stream_request_id)),
        _ => Err(BedrockError::plain(format!(
            "Unrecognizable event type: {}",
            message.headers.get(":event-type").map_or("undefined", String::as_str)
        ))),
    }
}

async fn drive(
    run: &mut Run<'_, '_, ConverseStream>,
    model: &Model,
    context: &TranscriptContext,
    options: &BedrockOptions,
    transport: &dyn Transport,
    response_id: &mut Option<String>,
) -> Result<(), BedrockError> {
    let signal = options.base.signal();
    let target = resolve_target(model, options)?;
    let (body, request_model) = prepare_request_payload(model, context, options)?;
    let target = resolve_chain_auth(target, options, transport).await?;
    let request = build_request(
        &target,
        &request_model,
        &serde_json::to_string(&body).unwrap_or_default(),
        options,
    );
    let http1 = select_http1_transport(model, options, transport)?;
    let mut stream = send_with_retry(http1.as_deref().unwrap_or(transport), &request, options).await?;
    *response_id = diagnostic_value(stream.request_id.as_deref());
    while let Some(message) = stream.next(signal).await? {
        process_stream_message(run, &message, stream.status, stream.request_id.as_deref(), model)?;
    }
    run.stopped(signal, "Bedrock stream ended without a stop reason")
        .map_err(BedrockError::plain)
}

/// `streamSimple`: Claude budget models get their thinking budget fitted into `maxTokens`; adaptive Claude and other
/// models pass the level through.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let base = build_base_options(model, context, options);
    let tool_choice = options.tool_choice.clone().map(Value::String);
    let Some(level) = options.reasoning.clone() else {
        let bedrock = BedrockOptions {
            base,
            tool_choice,
            ..Default::default()
        };
        return stream(model, context, &bedrock, transport, sink).await;
    };
    let mut bedrock = BedrockOptions {
        base,
        tool_choice,
        reasoning: Some(level.clone()),
        thinking_budgets: options.thinking_budgets,
        ..Default::default()
    };
    if is_anthropic_claude_model(model) && !supports_adaptive_thinking(model) {
        let (adjusted, budget) = adjust_max_tokens_for_thinking(
            bedrock.base.max_tokens,
            model.max_tokens,
            &level,
            options.thinking_budgets.as_ref(),
        );
        let max_tokens = clamp_max_tokens_to_context(model, context, adjusted);
        bedrock.base.max_tokens = Some(max_tokens);
        let budgets = options.thinking_budgets.unwrap_or_default();
        bedrock.thinking_budgets = Some(budgets.fitted(&level, budget, max_tokens));
    }
    stream(model, context, &bedrock, transport, sink).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::BoxFuture;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingTransport {
        calls: Arc<AtomicUsize>,
    }

    impl Transport for CountingTransport {
        fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("unexpected credential or stream request".to_string()) })
        }
    }

    fn test_model() -> Model {
        serde_json::from_value(json!({
            "id": "amazon.nova-lite-v1:0",
            "name": "Nova Lite",
            "api": "bedrock-converse-stream",
            "provider": "amazon-bedrock",
            "baseUrl": "https://bedrock-runtime.us-east-1.amazonaws.com",
            "reasoning": false,
            "input": ["text", "image"],
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 300000,
            "maxTokens": 5000
        }))
        .expect("valid test model")
    }

    fn chain_options() -> BedrockOptions {
        serde_json::from_value(json!({
            "region": "us-east-1",
            "maxRetries": 0,
            "env": {
                "AWS_CONTAINER_CREDENTIALS_FULL_URI": "http://127.0.0.1:9/creds",
                "AWS_EC2_METADATA_DISABLED": "true"
            }
        }))
        .expect("valid test options")
    }

    #[tokio::test]
    async fn payload_validation_runs_before_chain_credentials() {
        let model = test_model();
        let options = chain_options();
        let context: TranscriptContext = serde_json::from_value(json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "image", "data": "AAAA", "mimeType": "image/bmp"}],
                "timestamp": 0
            }]
        }))
        .expect("valid test context");
        let calls = Arc::new(AtomicUsize::new(0));
        let transport = CountingTransport { calls: calls.clone() };
        let mut sink = |_: &AssistantMessageEvent| {};

        let output = stream(&model, &context, &options, &transport, &mut sink).await;

        assert_eq!(output.stop_reason, StopReason::Error);
        assert_eq!(output.error_message.as_deref(), Some("Unknown image type: image/bmp"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn on_payload_runs_before_chain_credentials() {
        let model = test_model();
        let mut options = chain_options();
        let hook_ran = Arc::new(AtomicBool::new(false));
        let saw_hook_before_send = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_flag = hook_ran.clone();
        let hook_calls = calls.clone();
        let before_send = saw_hook_before_send.clone();
        options.base.on_payload = Some(Arc::new(move |_, _| {
            hook_flag.store(true, Ordering::SeqCst);
            if hook_calls.load(Ordering::SeqCst) == 0 {
                before_send.store(true, Ordering::SeqCst);
            }
            None
        }));
        let context: TranscriptContext = serde_json::from_value(json!({
            "messages": [{"role": "user", "content": "hello", "timestamp": 0}]
        }))
        .expect("valid test context");
        let transport = CountingTransport { calls: calls.clone() };
        let mut sink = |_: &AssistantMessageEvent| {};

        let output = stream(&model, &context, &options, &transport, &mut sink).await;

        assert_eq!(output.stop_reason, StopReason::Error);
        assert!(hook_ran.load(Ordering::SeqCst));
        assert!(saw_hook_before_send.load(Ordering::SeqCst));
        assert!(calls.load(Ordering::SeqCst) >= 1);
    }
}
