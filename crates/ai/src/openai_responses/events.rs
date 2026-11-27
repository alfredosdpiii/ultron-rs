use super::{js_str, str_field};
use crate::constrained::GrammarInputBuffer;
use crate::models::calculate_cost;
use crate::provider::Run;
use crate::types::{
    AssistantContent, AssistantMessageEvent, Model, StopReason, TextContent, ThinkingContent, ToolCall, Usage,
};
use indexmap::IndexMap;
use jsrt::json_parse::{parse_json, parse_streaming_json};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// How `processResponsesStream` prices a service tier (`resolveServiceTier`/`applyServiceTierPricing`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum TierPricing<'a> {
    /// No tier pricing (Azure).
    None,
    /// The response's tier, else the requested one (OpenAI Responses).
    Response(Option<&'a str>),
    /// Codex: a `default` response tier keeps a requested `flex`/`priority`.
    Codex(Option<&'a str>),
}

fn service_tier_multiplier(model: &Model, tier: Option<&str>) -> f64 {
    match tier {
        Some("flex") => 0.5,
        Some("priority") if model.id == "gpt-5.5" => 2.5,
        Some("priority") => 2.0,
        _ => 1.0,
    }
}

fn map_stop_reason(
    status: Option<&str>,
    incomplete_reason: Option<&str>,
) -> Result<(StopReason, Option<String>), String> {
    let Some(status) = status else {
        return Ok((StopReason::Stop, None));
    };
    Ok(match status {
        "completed" | "in_progress" | "queued" => (StopReason::Stop, None),
        "incomplete" if incomplete_reason == Some("max_output_tokens") => (StopReason::Length, None),
        "incomplete" => (
            StopReason::Error,
            Some(match incomplete_reason {
                Some(reason) => format!("Response incomplete: {reason}"),
                None => "Response incomplete without a provider reason".into(),
            }),
        ),
        "failed" | "cancelled" => (StopReason::Error, None),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Thinking,
    Text,
    ToolCall,
}

#[derive(Default)]
struct ToolScratch {
    partial_json: Option<String>,
    custom: Option<(String, GrammarInputBuffer)>,
}

/// The Responses API event stream the OpenAI, Azure and Codex providers read.
pub(crate) type ResponsesRun<'s, 'e> = Run<'s, 'e, ResponsesStream>;

#[derive(Default)]
pub(crate) struct ResponsesStream {
    slots: HashMap<i64, (SlotKind, usize)>,
    scratch: HashMap<usize, ToolScratch>,
    reasoning_by_id: HashMap<String, usize>,
    saw_terminal: bool,
}

fn delta_str(event: &Value) -> String {
    str_field(event, "delta").unwrap_or_default().to_string()
}

fn usage_count(usage: &Value, path: &str) -> u64 {
    usage.pointer(path).and_then(Value::as_u64).unwrap_or(0)
}

fn item_texts(item: &Value, key: &str, sep: &str) -> String {
    item.get(key).and_then(Value::as_array).map_or_default(|parts| {
        parts
            .iter()
            .map(|part| js_str(part.get("text")))
            .collect::<Vec<_>>()
            .join(sep)
    })
}

fn error_field(error: &Value, key: &str, fallback: &str) -> String {
    error
        .get(key)
        .filter(|val| val.as_str().is_none_or(|text| !text.is_empty()) && !val.is_null())
        .map_or_else(|| fallback.to_string(), |val| js_str(Some(val)))
}

impl ResponsesRun<'_, '_> {
    fn slot(&self, output_index: Option<i64>, kind: SlotKind) -> Option<usize> {
        let (slot_kind, position) = *self.state.slots.get(&output_index?)?;
        (slot_kind == kind).then_some(position)
    }

    fn call(&mut self, position: usize) -> &mut ToolCall {
        match &mut self.output.content[position] {
            AssistantContent::ToolCall(call) => call,
            _ => unreachable!("tool slots index tool calls"),
        }
    }

    fn apply_phase(&mut self, item: &Value) {
        if str_field(item, "type") == Some("message") && str_field(item, "phase") == Some("final_answer") {
            self.output.stop_reason = StopReason::Stop;
        }
    }

    fn build_slot_entry(
        &mut self,
        item: &Value,
        grammar: &IndexMap<String, String>,
    ) -> Option<(SlotKind, AssistantContent, Option<ToolScratch>)> {
        let namespace = item
            .get("namespace")
            .filter(|ns| !ns.is_null())
            .map(|ns| js_str(Some(ns)));
        match str_field(item, "type") {
            Some("reasoning") => Some((
                SlotKind::Thinking,
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                }),
                None,
            )),
            Some("message") => {
                self.apply_phase(item);
                Some((
                    SlotKind::Text,
                    AssistantContent::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }),
                    None,
                ))
            }
            Some("function_call") => Some((
                SlotKind::ToolCall,
                AssistantContent::ToolCall(ToolCall {
                    id: format!("{}|{}", js_str(item.get("call_id")), js_str(item.get("id"))),
                    name: js_str(item.get("name")),
                    arguments: json!({}),
                    thought_signature: None,
                    namespace,
                }),
                Some(ToolScratch {
                    partial_json: Some(str_field(item, "arguments").unwrap_or_default().to_string()),
                    custom: None,
                }),
            )),
            Some("custom_tool_call") => {
                let name = js_str(item.get("name"));
                let property = grammar.get(&name).cloned().unwrap_or_else(|| "input".into());
                let input = str_field(item, "input").unwrap_or_default();
                Some((
                    SlotKind::ToolCall,
                    AssistantContent::ToolCall(ToolCall {
                        id: format!("{}|{}", js_str(item.get("call_id")), js_str(item.get("id"))),
                        name,
                        arguments: json!({ property.clone(): input }),
                        thought_signature: None,
                        namespace,
                    }),
                    Some(ToolScratch {
                        partial_json: None,
                        custom: Some((property, GrammarInputBuffer::default())),
                    }),
                ))
            }
            _ => None,
        }
    }

    fn emit_slot_start(&mut self, kind: SlotKind, position: usize) {
        self.emit(|partial| match kind {
            SlotKind::Thinking => AssistantMessageEvent::ThinkingStart {
                content_index: position,
                partial,
            },
            SlotKind::Text => AssistantMessageEvent::TextStart {
                content_index: position,
                partial,
            },
            SlotKind::ToolCall => AssistantMessageEvent::ToolCallStart {
                content_index: position,
                partial,
            },
        });
    }

    fn create_slot(
        &mut self,
        output_index: Option<i64>,
        item: &Value,
        grammar: &IndexMap<String, String>,
    ) -> Option<(SlotKind, usize)> {
        let (kind, block, scratch) = self.build_slot_entry(item, grammar)?;
        self.output.content.push(block);
        let position = self.output.content.len() - 1;
        if let Some(scratch) = scratch {
            self.state.scratch.insert(position, scratch);
        }
        if let Some(index) = output_index {
            self.state.slots.insert(index, (kind, position));
        }
        self.emit_slot_start(kind, position);
        Some((kind, position))
    }

    fn custom_input(&mut self, position: usize) -> String {
        let Some((property, _)) = self.state.scratch.get(&position).and_then(|slot| slot.custom.as_ref()) else {
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
        let Some((property, buffer)) = self
            .state
            .scratch
            .get_mut(&position)
            .and_then(|slot| slot.custom.as_mut())
        else {
            return Ok(None);
        };
        let delta = buffer.append(property, next, close)?;
        let property = property.clone();
        self.call(position).arguments = json!({ property: next });
        Ok(delta)
    }

    fn push_tool_delta(&mut self, position: usize, delta: Option<String>) {
        if let Some(delta) = delta {
            self.emit(|partial| AssistantMessageEvent::ToolCallDelta {
                content_index: position,
                delta,
                partial,
            });
        }
    }

    fn has_partial_json(&self, position: usize) -> bool {
        self.state
            .scratch
            .get(&position)
            .is_some_and(|slot| slot.partial_json.is_some())
    }

    fn has_custom(&self, position: usize) -> bool {
        self.state
            .scratch
            .get(&position)
            .is_some_and(|slot| slot.custom.is_some())
    }

    fn append_text(&mut self, position: usize, delta: &str) {
        match &mut self.output.content[position] {
            AssistantContent::Text(block) => block.text.push_str(delta),
            AssistantContent::Thinking(block) => block.thinking.push_str(delta),
            AssistantContent::ToolCall(_) => {}
        }
    }

    fn backfill_encrypted_reasoning(&mut self, response: &Value) -> Result<(), String> {
        for item in response.get("output").and_then(Value::as_array).into_iter().flatten() {
            let Some(encrypted) = item
                .get("encrypted_content")
                .filter(|val| val.as_str().is_some_and(|text| !text.is_empty()))
            else {
                continue;
            };
            if str_field(item, "type") != Some("reasoning") {
                continue;
            }
            let Some(&position) = self.state.reasoning_by_id.get(&js_str(item.get("id"))) else {
                continue;
            };
            let AssistantContent::Thinking(block) = &mut self.output.content[position] else {
                continue;
            };
            let Some(signature) = block.thinking_signature.as_deref().filter(|sig| !sig.is_empty()) else {
                continue;
            };
            let mut stored = parse_json(signature).map_err(|err| err.to_string())?;
            if stored
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                continue;
            }
            stored["encrypted_content"] = encrypted.clone();
            block.thinking_signature = Some(jsrt::json::stringify(&stored));
        }
        Ok(())
    }

    fn apply_usage(&mut self, response: &Value, model: &Model) {
        if let Some(usage) = response.get("usage").filter(|val| val.is_object()) {
            let cached = usage_count(usage, "/input_tokens_details/cached_tokens");
            let write = usage_count(usage, "/input_tokens_details/cache_write_tokens");
            self.output.usage = Usage {
                input: (usage_count(usage, "/input_tokens") as i64 - cached as i64 - write as i64).max(0) as u64,
                output: usage_count(usage, "/output_tokens"),
                cache_read: cached,
                cache_write: write,
                reasoning: Some(usage_count(usage, "/output_tokens_details/reasoning_tokens")),
                total_tokens: usage_count(usage, "/total_tokens"),
                ..Usage::default()
            };
        }
        calculate_cost(model, &mut self.output.usage);
    }

    fn apply_tier_pricing(&mut self, response: &Value, model: &Model, pricing: TierPricing<'_>) {
        let response_tier = str_field(response, "service_tier");
        let multiplier = match pricing {
            TierPricing::None => 1.0,
            TierPricing::Response(request) => service_tier_multiplier(model, response_tier.or(request)),
            TierPricing::Codex(request) => {
                let tier = match (response_tier, request) {
                    (Some("default"), Some(request @ ("flex" | "priority"))) => Some(request),
                    (response, request) => response.or(request),
                };
                service_tier_multiplier(model, tier)
            }
        };
        if multiplier != 1.0 {
            let cost = &mut self.output.usage.cost;
            cost.input *= multiplier;
            cost.output *= multiplier;
            cost.cache_read *= multiplier;
            cost.cache_write *= multiplier;
            cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
        }
    }

    fn apply_stop_reason(&mut self, response: &Value) -> Result<(), String> {
        let status = str_field(response, "status");
        let reason = response.pointer("/incomplete_details/reason").and_then(Value::as_str);
        self.output.raw_stop_reason = match (status, reason) {
            (status, Some(reason)) => Some(format!("{}.{reason}", status.unwrap_or("undefined"))),
            (status, None) => status.map(str::to_string),
        };
        let (stop, message) = map_stop_reason(status, reason)?;
        self.output.stop_reason = stop;
        self.output.error_message = message;
        if stop == StopReason::Stop
            && self
                .output
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            self.output.stop_reason = StopReason::ToolUse;
        }
        Ok(())
    }

    fn finalize(&mut self, response: &Value, model: &Model, pricing: TierPricing<'_>) -> Result<(), String> {
        self.state.saw_terminal = true;
        self.backfill_encrypted_reasoning(response)?;
        if let Some(id) = str_field(response, "id").filter(|val| !val.is_empty()) {
            self.output.response_id = Some(id.to_string());
        }
        self.apply_usage(response, model);
        self.apply_tier_pricing(response, model, pricing);
        self.apply_stop_reason(response)
    }

    fn handle_thinking_delta(&mut self, output_index: Option<i64>, kind: &str, event: &Value) {
        let Some(position) = self.slot(output_index, SlotKind::Thinking) else {
            return;
        };
        let text = if kind == "response.reasoning_summary_part.done" {
            "\n\n".to_string()
        } else {
            delta_str(event)
        };
        self.append_text(position, &text);
        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
            content_index: position,
            delta: text,
            partial,
        });
    }

    fn handle_text_delta(&mut self, output_index: Option<i64>, event: &Value) {
        let Some(position) = self.slot(output_index, SlotKind::Text) else {
            return;
        };
        let text = delta_str(event);
        self.append_text(position, &text);
        self.emit(|partial| AssistantMessageEvent::TextDelta {
            content_index: position,
            delta: text,
            partial,
        });
    }

    fn handle_function_call_delta(&mut self, output_index: Option<i64>, event: &Value) {
        let Some(position) = self.slot(output_index, SlotKind::ToolCall) else {
            return;
        };
        let Some(partial) = self
            .state
            .scratch
            .get_mut(&position)
            .and_then(|slot| slot.partial_json.as_mut())
        else {
            return;
        };
        let text = delta_str(event);
        partial.push_str(&text);
        let parsed = parse_streaming_json(partial);
        self.call(position).arguments = parsed;
        self.push_tool_delta(position, Some(text));
    }

    fn handle_function_call_done(&mut self, output_index: Option<i64>, event: &Value) {
        let Some(position) = self.slot(output_index, SlotKind::ToolCall) else {
            return;
        };
        let Some(slot) = self
            .state
            .scratch
            .get_mut(&position)
            .and_then(|entry| entry.partial_json.as_mut())
        else {
            return;
        };
        let arguments = str_field(event, "arguments").unwrap_or_default().to_string();
        let previous = std::mem::replace(slot, arguments.clone());
        self.call(position).arguments = parse_streaming_json(&arguments);
        if let Some(rest) = arguments
            .strip_prefix(previous.as_str())
            .filter(|tail| !tail.is_empty())
        {
            self.push_tool_delta(position, Some(rest.to_string()));
        }
    }

    fn handle_custom_tool_delta(&mut self, output_index: Option<i64>, event: &Value) -> Result<(), String> {
        let Some(position) = self
            .slot(output_index, SlotKind::ToolCall)
            .filter(|pos| self.has_custom(*pos))
        else {
            return Ok(());
        };
        let next = format!("{}{}", self.custom_input(position), delta_str(event));
        let appended = self.append_custom(position, &next, false)?;
        self.push_tool_delta(position, appended);
        Ok(())
    }

    fn handle_custom_tool_done(&mut self, output_index: Option<i64>, event: &Value) -> Result<(), String> {
        let Some(position) = self
            .slot(output_index, SlotKind::ToolCall)
            .filter(|pos| self.has_custom(*pos))
        else {
            return Ok(());
        };
        let input = str_field(event, "input").unwrap_or_default().to_string();
        let appended = self.append_custom(position, &input, true)?;
        self.push_tool_delta(position, appended);
        Ok(())
    }

    fn try_handle_stream_delta(
        &mut self,
        kind: &str,
        output_index: Option<i64>,
        event: &Value,
    ) -> Result<bool, String> {
        match kind {
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_part.done" => self.handle_thinking_delta(output_index, kind, event),
            "response.output_text.delta" | "response.refusal.delta" => self.handle_text_delta(output_index, event),
            "response.function_call_arguments.delta" => self.handle_function_call_delta(output_index, event),
            "response.function_call_arguments.done" => self.handle_function_call_done(output_index, event),
            "response.custom_tool_call_input.delta" => self.handle_custom_tool_delta(output_index, event)?,
            "response.custom_tool_call_input.done" => self.handle_custom_tool_done(output_index, event)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish_reasoning_item(&mut self, position: usize, item: &Value) {
        let summary = item_texts(item, "summary", "\n\n");
        let content = item_texts(item, "content", "\n\n");
        if let AssistantContent::Thinking(block) = &mut self.output.content[position] {
            if !summary.is_empty() {
                block.thinking = summary;
            } else if !content.is_empty() {
                block.thinking = content;
            }
            block.thinking_signature = Some(jsrt::json::stringify(item));
        }
        self.state.reasoning_by_id.insert(js_str(item.get("id")), position);
        let AssistantContent::Thinking(block) = &self.output.content[position] else {
            return;
        };
        let content = block.thinking.clone();
        self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
            content_index: position,
            content,
            partial,
        });
    }

    fn finish_message_item(&mut self, position: usize, item: &Value) {
        let text = item.get("content").and_then(Value::as_array).map_or_default(|parts| {
            parts
                .iter()
                .map(|part| {
                    if str_field(part, "type") == Some("output_text") {
                        js_str(part.get("text"))
                    } else {
                        js_str(part.get("refusal"))
                    }
                })
                .collect::<String>()
        });
        let mut signature = Map::new();
        signature.insert("v".into(), json!(1));
        if let Some(id) = item.get("id").filter(|val| !val.is_null()) {
            signature.insert("id".into(), id.clone());
        }
        if let Some(phase) = str_field(item, "phase").filter(|val| !val.is_empty()) {
            signature.insert("phase".into(), json!(phase));
        }
        if let AssistantContent::Text(block) = &mut self.output.content[position] {
            block.text = text.clone();
            block.text_signature = Some(jsrt::json::stringify(&Value::Object(signature)));
        }
        self.emit(|partial| AssistantMessageEvent::TextEnd {
            content_index: position,
            content: text,
            partial,
        });
    }

    fn finish_function_call_item(&mut self, position: usize, item: &Value) {
        let partial = self
            .state
            .scratch
            .remove(&position)
            .and_then(|slot| slot.partial_json)
            .unwrap_or_default();
        let source = str_field(item, "arguments")
            .filter(|args| !args.is_empty())
            .map(str::to_string);
        let source = source.unwrap_or(if partial.is_empty() { "{}".into() } else { partial });
        let call = self.call(position);
        call.arguments = parse_streaming_json(&source);
        if let Some(namespace) = item.get("namespace").filter(|ns| !ns.is_null()) {
            call.namespace = Some(js_str(Some(namespace)));
        }
        let tool_call = call.clone();
        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
            content_index: position,
            tool_call,
            partial,
        });
    }

    fn finish_custom_tool_item(&mut self, position: usize, item: &Value) -> Result<(), String> {
        let input = match item.get("input").filter(|val| !val.is_null()) {
            Some(input) => js_str(Some(input)),
            None => self.custom_input(position),
        };
        let appended = self.append_custom(position, &input, true)?;
        self.push_tool_delta(position, appended);
        self.state.scratch.remove(&position);
        let call = self.call(position);
        if let Some(namespace) = item.get("namespace").filter(|ns| !ns.is_null()) {
            call.namespace = Some(js_str(Some(namespace)));
        }
        let tool_call = call.clone();
        self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
            content_index: position,
            tool_call,
            partial,
        });
        Ok(())
    }

    fn handle_output_item_done(
        &mut self,
        output_index: Option<i64>,
        item: &Value,
        grammar: &IndexMap<String, String>,
    ) -> Result<(), String> {
        self.apply_phase(item);
        let slot = match output_index.and_then(|idx| self.state.slots.get(&idx).copied()) {
            Some(slot) => Some(slot),
            None => self.create_slot(output_index, item, grammar),
        };
        let Some((kind, position)) = slot else {
            return Ok(());
        };
        let item_type = str_field(item, "type").unwrap_or_default();
        match (item_type, kind) {
            ("reasoning", SlotKind::Thinking) => self.finish_reasoning_item(position, item),
            ("message", SlotKind::Text) => self.finish_message_item(position, item),
            ("function_call", SlotKind::ToolCall) if self.has_partial_json(position) => {
                self.finish_function_call_item(position, item);
            }
            ("custom_tool_call", SlotKind::ToolCall) if self.has_custom(position) => {
                self.finish_custom_tool_item(position, item)?;
            }
            _ => return Ok(()),
        }
        if let Some(index) = output_index {
            self.state.slots.remove(&index);
        }
        Ok(())
    }

    fn try_handle_item_event(
        &mut self,
        kind: &str,
        output_index: Option<i64>,
        event: &Value,
        grammar: &IndexMap<String, String>,
    ) -> Result<bool, String> {
        match kind {
            "response.created" => {
                self.output.response_id = event
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "response.output_item.added" => {
                self.create_slot(output_index, &event["item"], grammar);
            }
            "response.output_item.done" => {
                self.handle_output_item_done(output_index, &event["item"], grammar)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn format_response_failed(&mut self, response: &Value) -> String {
        self.state.saw_terminal = true;
        self.output.raw_stop_reason = str_field(response, "status").map(str::to_string);
        let error = response.get("error").filter(|val| !val.is_null());
        let reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .filter(|val| !val.is_empty());
        match (error, reason) {
            (Some(error), _) => format!(
                "{}: {}",
                error_field(error, "code", "unknown"),
                error_field(error, "message", "no message")
            ),
            (None, Some(reason)) => format!("incomplete: {reason}"),
            (None, None) => "Unknown error (no error details in response)".into(),
        }
    }

    fn handle_response_lifecycle(
        &mut self,
        kind: &str,
        event: &Value,
        model: &Model,
        pricing: TierPricing<'_>,
    ) -> Result<(), String> {
        match kind {
            "response.completed" | "response.incomplete" => self.finalize(&event["response"], model, pricing),
            "error" => Err(format!(
                "Error Code {}: {}",
                js_str(event.get("code")),
                js_str(event.get("message"))
            )),
            "response.failed" => Err(self.format_response_failed(&event["response"])),
            _ => Ok(()),
        }
    }

    /// One Responses stream event.
    pub fn handle(
        &mut self,
        event: &Value,
        model: &Model,
        grammar: &IndexMap<String, String>,
        pricing: TierPricing<'_>,
    ) -> Result<(), String> {
        let output_index = event.get("output_index").and_then(Value::as_i64);
        let kind = str_field(event, "type").unwrap_or_default();
        if self.try_handle_item_event(kind, output_index, event, grammar)? {
            return Ok(());
        }
        if self.try_handle_stream_delta(kind, output_index, event)? {
            return Ok(());
        }
        self.handle_response_lifecycle(kind, event, model, pricing)
    }

    /// Whether the stream reached its terminal response event.
    pub fn terminated(&self) -> Result<(), String> {
        if self.state.saw_terminal {
            Ok(())
        } else {
            Err("OpenAI Responses stream ended before a terminal response event".into())
        }
    }
}
