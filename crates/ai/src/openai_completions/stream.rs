use super::compat::Compat;
use super::headers::client_headers;
use super::params::build_params;
use super::reasoning::{append_reasoning_detail, is_reasoning_detail};
use super::{OpenAICompletionsOptions, client_api_key};
use crate::constrained::{GrammarInputBuffer, grammar_tool_input_properties};
use crate::http::{
    AbortSignal, Failure, HttpRequest, Sdk, Transport, format_provider_error, is_aborted, join_url, send_with_retry,
};
use crate::models::calculate_cost;
use crate::options::{SimpleStreamOptions, build_base_options};
use crate::provider::{REQUEST_WAS_ABORTED, Run};
use crate::sse::read_openai_stream;
use crate::transcript::{declared_tools, resolve_transcript};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, EventSink, Model, StopReason,
    TextContent, ThinkingContent, ToolCall, TranscriptContext, Usage,
};
use indexmap::IndexMap;
use jsrt::json_parse::parse_streaming_json;
use serde_json::{Value, json};
use std::collections::HashMap;

fn usage_number<'a>(raw: &'a Value, path: &str) -> Option<&'a Value> {
    raw.pointer(path).filter(|val| !val.is_null())
}

fn chunk_usage(raw: &Value, model: &Model) -> Usage {
    let prompt = usage_number(raw, "/prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as i64;
    let cache_read = usage_number(raw, "/prompt_tokens_details/cached_tokens")
        .or_else(|| usage_number(raw, "/prompt_cache_hit_tokens"))
        .or_else(|| usage_number(raw, "/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = usage_number(raw, "/prompt_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input = (prompt - cache_read as i64 - cache_write as i64).max(0) as u64;
    let output = usage_number(raw, "/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(
            usage_number(raw, "/completion_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    calculate_cost(model, &mut usage);
    usage
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "function_call" | "tool_calls" => (StopReason::ToolUse, None),
        other => (StopReason::Error, Some(format!("Provider finish_reason: {other}"))),
    }
}

/// Per tool-call scratch state upstream keeps on the block.
#[derive(Default)]
struct ToolScratch {
    partial_args: Option<String>,
    custom: Option<(String, GrammarInputBuffer)>,
    stream_index: Option<i64>,
}

struct CompletionsStream {
    scratch: Vec<ToolScratch>,
    text_block: Option<usize>,
    thinking_block: Option<usize>,
    has_finish_reason: bool,
    by_index: HashMap<i64, usize>,
    by_id: HashMap<String, usize>,
    reasoning_details: Option<Vec<Value>>,
}

struct ToolDeltaMeta {
    stream_index: Option<i64>,
    id: Option<String>,
    name: String,
    is_custom: bool,
}

fn truthy_field<'a>(delta: &'a Value, key: &str) -> Option<&'a Value> {
    delta.get(key).filter(|val| jsrt::value::truthy(Some(val)))
}

impl ToolDeltaMeta {
    fn from_delta(delta: &Value) -> Self {
        let stream_index = delta.get("index").and_then(Value::as_i64);
        let function = truthy_field(delta, "function");
        let custom = truthy_field(delta, "custom");
        let name = delta
            .pointer("/function/name")
            .filter(|val| !val.is_null())
            .or_else(|| delta.pointer("/custom/name").filter(|val| !val.is_null()))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = truthy_field(delta, "id").and_then(Value::as_str).map(str::to_string);
        Self {
            stream_index,
            id,
            name,
            is_custom: custom.is_some() && function.is_none(),
        }
    }
}

impl Run<'_, '_, CompletionsStream> {
    fn push(&mut self, block: AssistantContent, scratch: ToolScratch) -> usize {
        self.output.content.push(block);
        self.state.scratch.push(scratch);
        self.output.content.len() - 1
    }

    fn call(&mut self, position: usize) -> &mut ToolCall {
        match &mut self.output.content[position] {
            AssistantContent::ToolCall(call) => call,
            _ => unreachable!("tool scratch always indexes a tool call"),
        }
    }

    fn custom_input(&mut self, position: usize) -> String {
        let Some((property, _)) = &self.state.scratch[position].custom else {
            return String::new();
        };
        let property = property.clone();
        self.call(position)
            .arguments
            .get(&property)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn append_custom(&mut self, position: usize, next: &str, close: bool) -> Result<Option<String>, String> {
        let Some((property, buffer)) = &mut self.state.scratch[position].custom else {
            return Ok(None);
        };
        let delta = buffer.append(property, next, close)?;
        let property = property.clone();
        self.call(position).arguments = json!({ property: next });
        Ok(delta)
    }

    fn apply_reasoning_details(&mut self, position: usize) {
        if let (Some(details), AssistantContent::Thinking(block)) =
            (&self.state.reasoning_details, &mut self.output.content[position])
        {
            block.thinking_signature = Some(jsrt::json::stringify(&Value::Array(details.clone())));
        }
    }

    fn ensure_text(&mut self) -> usize {
        if let Some(position) = self.state.text_block {
            return position;
        }
        let position = self.push(
            AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }),
            ToolScratch::default(),
        );
        self.state.text_block = Some(position);
        self.emit(|partial| AssistantMessageEvent::TextStart {
            content_index: position,
            partial,
        });
        position
    }

    fn ensure_thinking(&mut self, signature: &str) -> usize {
        if let Some(position) = self.state.thinking_block {
            return position;
        }
        let block = ThinkingContent {
            thinking: String::new(),
            thinking_signature: Some(signature.to_string()),
            redacted: None,
        };
        let position = self.push(AssistantContent::Thinking(block), ToolScratch::default());
        self.state.thinking_block = Some(position);
        self.emit(|partial| AssistantMessageEvent::ThinkingStart {
            content_index: position,
            partial,
        });
        position
    }

    fn lookup_tool_call_position(&self, stream_index: Option<i64>, id: Option<&str>) -> Option<usize> {
        if let Some(position) = stream_index.and_then(|idx| self.state.by_index.get(&idx).copied()) {
            return Some(position);
        }
        id.and_then(|key| self.state.by_id.get(key).copied())
    }

    fn start_tool_call(&mut self, meta: &ToolDeltaMeta, grammar: &IndexMap<String, String>) -> usize {
        let custom_property = meta
            .is_custom
            .then(|| grammar.get(&meta.name).cloned().unwrap_or_else(|| "input".into()));
        let call = ToolCall {
            id: meta.id.clone().unwrap_or_default(),
            name: meta.name.clone(),
            arguments: match &custom_property {
                Some(property) => json!({ property: "" }),
                None => json!({}),
            },
            thought_signature: None,
            namespace: None,
        };
        let scratch = ToolScratch {
            partial_args: custom_property.is_none().then(String::new),
            custom: custom_property.map(|prop| (prop, GrammarInputBuffer::default())),
            stream_index: meta.stream_index,
        };
        let position = self.push(AssistantContent::ToolCall(call), scratch);
        if let Some(index) = meta.stream_index {
            self.state.by_index.insert(index, position);
        }
        if let Some(id) = &meta.id {
            self.state.by_id.insert(id.clone(), position);
        }
        self.emit(|partial| AssistantMessageEvent::ToolCallStart {
            content_index: position,
            partial,
        });
        position
    }

    fn backfill_tool_call(&mut self, position: usize, meta: &ToolDeltaMeta, grammar: &IndexMap<String, String>) {
        if let (Some(index), None) = (meta.stream_index, self.state.scratch[position].stream_index) {
            self.state.scratch[position].stream_index = Some(index);
            self.state.by_index.insert(index, position);
        }
        if let Some(id) = &meta.id {
            self.state.by_id.insert(id.clone(), position);
        }
        let call = self.call(position);
        if call.name.is_empty() && !meta.name.is_empty() {
            call.name = meta.name.clone();
        }
        if meta.is_custom && self.state.scratch[position].custom.is_none() {
            let call_name = self.call(position).name.clone();
            let property = grammar.get(&call_name).cloned().unwrap_or_else(|| "input".into());
            self.call(position).arguments = json!({ property.clone(): "" });
            self.state.scratch[position].custom = Some((property, GrammarInputBuffer::default()));
            self.state.scratch[position].partial_args = None;
        }
    }

    fn ensure_tool_call(&mut self, delta: &Value, grammar: &IndexMap<String, String>) -> usize {
        let meta = ToolDeltaMeta::from_delta(delta);
        let position = match self.lookup_tool_call_position(meta.stream_index, meta.id.as_deref()) {
            Some(position) => position,
            None => self.start_tool_call(&meta, grammar),
        };
        self.backfill_tool_call(position, &meta, grammar);
        position
    }

    fn apply_chunk_metadata(&mut self, chunk: &Value, model: &Model) -> bool {
        if self.output.response_id.as_deref().is_none_or(str::is_empty) {
            self.output.response_id = chunk.get("id").and_then(Value::as_str).map(str::to_string);
        }
        if let Some(response_model) = chunk.get("model").and_then(Value::as_str)
            && !response_model.is_empty()
            && response_model != model.id
            && self.output.response_model.as_deref().is_none_or(str::is_empty)
        {
            self.output.response_model = Some(response_model.to_string());
        }
        let chunk_usage_present = chunk.get("usage").is_some_and(|usage| jsrt::value::truthy(Some(usage)));
        if chunk_usage_present {
            self.output.usage = chunk_usage(&chunk["usage"], model);
        }
        chunk_usage_present
    }

    fn apply_choice_header(&mut self, choice: &Value, model: &Model, chunk_usage_present: bool) {
        if !chunk_usage_present
            && let Some(usage) = choice.get("usage").filter(|usage| jsrt::value::truthy(Some(usage)))
        {
            self.output.usage = chunk_usage(usage, model);
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            self.output.raw_stop_reason = Some(reason.to_string());
            let (stop, message) = map_stop_reason(reason);
            self.output.stop_reason = stop;
            if message.is_some() {
                self.output.error_message = message;
            }
            self.state.has_finish_reason = true;
        }
    }

    fn handle_text_delta(&mut self, delta: &Value) {
        let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let position = self.ensure_text();
        if let AssistantContent::Text(block) = &mut self.output.content[position] {
            block.text.push_str(content);
        }
        let text = content.to_string();
        self.emit(|partial| AssistantMessageEvent::TextDelta {
            content_index: position,
            delta: text,
            partial,
        });
    }

    fn handle_reasoning_delta(&mut self, delta: &Value, model: &Model) {
        let found = ["reasoning_content", "reasoning", "reasoning_text"]
            .into_iter()
            .find_map(|field| {
                delta
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|val| !val.is_empty())
                    .map(|val| (field, val))
            });
        let Some((field, text)) = found else {
            return;
        };
        let signature = if model.provider == "opencode-go" && field == "reasoning" {
            "reasoning_content"
        } else {
            field
        };
        let position = self.ensure_thinking(signature);
        if let AssistantContent::Thinking(block) = &mut self.output.content[position] {
            block.thinking.push_str(text);
        }
        let delta_text = text.to_string();
        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
            content_index: position,
            delta: delta_text,
            partial,
        });
    }

    fn apply_tool_call_args_delta(&mut self, position: usize, call_delta: &Value) -> Result<String, Failure> {
        if let Some(arguments) = call_delta
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .filter(|args| !args.is_empty())
        {
            let partial = self.state.scratch[position]
                .partial_args
                .get_or_insert_with(String::new);
            partial.push_str(arguments);
            let parsed = parse_streaming_json(partial);
            self.call(position).arguments = parsed;
            return Ok(arguments.to_string());
        }
        if let Some(input) = call_delta
            .pointer("/custom/input")
            .and_then(Value::as_str)
            .filter(|val| !val.is_empty())
        {
            let next = format!("{}{input}", self.custom_input(position));
            return Ok(self
                .append_custom(position, &next, false)
                .map_err(Failure::plain)?
                .unwrap_or_default());
        }
        Ok(String::new())
    }

    fn handle_tool_call_delta(
        &mut self,
        call_delta: &Value,
        grammar: &IndexMap<String, String>,
    ) -> Result<(), Failure> {
        let position = self.ensure_tool_call(call_delta, grammar);
        if let Some(id) = call_delta
            .get("id")
            .and_then(Value::as_str)
            .filter(|val| !val.is_empty())
            && self.call(position).id.is_empty()
        {
            self.call(position).id = id.to_string();
            self.state.by_id.insert(id.to_string(), position);
        }
        let name = call_delta
            .pointer("/function/name")
            .filter(|val| !val.is_null())
            .or_else(|| call_delta.pointer("/custom/name"))
            .and_then(Value::as_str);
        if let Some(name) = name.filter(|val| !val.is_empty())
            && self.call(position).name.is_empty()
        {
            self.call(position).name = name.to_string();
        }
        let text = self.apply_tool_call_args_delta(position, call_delta)?;
        self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
            content_index: position,
            delta: text,
            partial,
        });
        Ok(())
    }

    fn handle_reasoning_details_delta(&mut self, delta: &Value) {
        let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) else {
            return;
        };
        for detail in details.iter().filter(|detail| is_reasoning_detail(detail)) {
            self.ensure_thinking("");
            append_reasoning_detail(self.state.reasoning_details.get_or_insert_with(Vec::new), detail);
        }
    }

    fn handle_chunk(
        &mut self,
        chunk: &Value,
        model: &Model,
        grammar: &IndexMap<String, String>,
    ) -> Result<(), Failure> {
        if !chunk.is_object() {
            return Ok(());
        }
        let chunk_usage_present = self.apply_chunk_metadata(chunk, model);
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|list| list.first())
        else {
            return Ok(());
        };
        self.apply_choice_header(choice, model, chunk_usage_present);
        let Some(delta) = choice.get("delta").filter(|delta| jsrt::value::truthy(Some(delta))) else {
            return Ok(());
        };
        self.handle_text_delta(delta);
        self.handle_reasoning_delta(delta, model);
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call_delta in calls {
                self.handle_tool_call_delta(call_delta, grammar)?;
            }
        }
        self.handle_reasoning_details_delta(delta);
        Ok(())
    }

    fn finish_tool_call_block(&mut self, position: usize) -> Result<(), Failure> {
        if self.state.scratch[position].custom.is_some() {
            let input = self.custom_input(position);
            if let Some(delta) = self.append_custom(position, &input, true).map_err(Failure::plain)? {
                self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                    content_index: position,
                    delta,
                    partial,
                });
            }
        } else {
            let parsed = parse_streaming_json(self.state.scratch[position].partial_args.as_deref().unwrap_or_default());
            self.call(position).arguments = parsed;
        }
        let tool_call = self.call(position).clone();
        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
            content_index: position,
            tool_call,
            partial,
        });
        Ok(())
    }

    fn finish_blocks(&mut self) -> Result<(), Failure> {
        for position in 0..self.output.content.len() {
            match &self.output.content[position] {
                AssistantContent::Text(block) => {
                    let content = block.text.clone();
                    self.emit(|partial| AssistantMessageEvent::TextEnd {
                        content_index: position,
                        content,
                        partial,
                    });
                }
                AssistantContent::Thinking(_) => {
                    self.apply_reasoning_details(position);
                    let AssistantContent::Thinking(block) = &self.output.content[position] else {
                        continue;
                    };
                    let content = block.thinking.clone();
                    self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                        content_index: position,
                        content,
                        partial,
                    });
                }
                AssistantContent::ToolCall(_) => self.finish_tool_call_block(position)?,
            }
        }
        Ok(())
    }
}

/// Stream one Chat Completions response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let compat = Compat::of(model);
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let now = jsrt::date::now_ms();
    let state = CompletionsStream {
        scratch: Vec::new(),
        text_block: None,
        thinking_block: None,
        has_finish_reason: false,
        by_index: HashMap::new(),
        by_id: HashMap::new(),
        reasoning_details: None,
    };
    let mut run = Run::new(AssistantMessage::pending(model, now), sink, state);
    let result = drive(&mut run, model, &context, options, &compat, transport, now).await;
    if result.is_err() {
        for position in 0..run.output.content.len() {
            run.apply_reasoning_details(position);
        }
    }
    let result = result.map_err(|failure| {
        let mut message = format_provider_error(&failure, None);
        if let Some(raw) = failure
            .raw_metadata
            .as_ref()
            .filter(|raw| !message.contains(raw.as_str()))
        {
            message.push('\n');
            message.push_str(raw);
        }
        message
    });
    run.end(result, options.base.signal())
}

fn build_http_request(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    compat: &Compat,
    now: i64,
) -> Result<(HttpRequest, IndexMap<String, String>), Failure> {
    let base = &options.base;
    let api_key = client_api_key(model, base).map_err(Failure::plain)?;
    let grammar =
        grammar_tool_input_properties(&declared_tools(&context.messages), compat.supports_openai_grammar_tools)
            .map_err(Failure::plain)?;
    let retention = base.cache_retention();
    let session_id = if retention == CacheRetention::None {
        None
    } else {
        base.session_id.as_deref()
    };
    let headers = client_headers(model, context, base, &api_key, session_id, compat);
    let params = build_params(model, context, options, compat, retention, &grammar, now).map_err(Failure::plain)?;
    let request = crate::provider::build_json_post_request(
        join_url(&model.base_url, "/chat/completions"),
        headers,
        params,
        model,
        base,
    );
    Ok((request, grammar))
}

fn infer_missing_finish_reason(run: &mut Run<'_, '_, CompletionsStream>, compat: &Compat) {
    if run.state.has_finish_reason || compat.supports_finish_reason {
        return;
    }
    let has_calls = run
        .output
        .content
        .iter()
        .any(|block| matches!(block, AssistantContent::ToolCall(_)));
    run.output.stop_reason = if has_calls {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    };
}

fn finalize_stop_reason(
    run: &mut Run<'_, '_, CompletionsStream>,
    compat: &Compat,
    signal: Option<&AbortSignal>,
) -> Result<(), Failure> {
    if is_aborted(signal) || run.output.stop_reason == StopReason::Aborted {
        return Err(Failure::plain(REQUEST_WAS_ABORTED));
    }
    infer_missing_finish_reason(run, compat);
    if run.output.stop_reason == StopReason::Error {
        let message = run.output.error_message.clone().filter(|msg| !msg.is_empty());
        return Err(Failure::plain(
            message.unwrap_or_else(|| "Provider returned an error stop reason".into()),
        ));
    }
    if (compat.supports_finish_reason && !run.state.has_finish_reason) || run.output.stop_reason == StopReason::Pending
    {
        return Err(Failure::plain("Stream ended without finish_reason"));
    }
    Ok(())
}

async fn drive(
    run: &mut Run<'_, '_, CompletionsStream>,
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAICompletionsOptions,
    compat: &Compat,
    transport: &dyn Transport,
    now: i64,
) -> Result<(), Failure> {
    let base = &options.base;
    let signal = base.signal();
    let (request, grammar) = build_http_request(model, context, options, compat, now)?;
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, base.retry(), signal).await?;
    run.start();
    read_openai_stream(&mut response, signal, |chunk| run.handle_chunk(chunk, model, &grammar)).await?;
    run.finish_blocks()?;
    finalize_stop_reason(run, compat, signal)
}

/// `streamSimple`: reasoning clamped to the model's supported levels and sent as `reasoningEffort`.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    client_api_key(model, &options.base)?;
    let completions = OpenAICompletionsOptions {
        base: build_base_options(model, context, options),
        tool_choice: options.tool_choice.clone().map(Value::String),
        reasoning_effort: crate::options::simple_reasoning_effort(model, options),
        thinking_budgets: options.thinking_budgets,
    };
    Ok(stream(model, context, &completions, transport, sink).await)
}
