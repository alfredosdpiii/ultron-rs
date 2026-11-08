//! `experimental/rpc-events.ts`: lane events become the Pi RPC event stream. The lane's own turn boundaries are not
//! forwarded, so turns are reconstructed from message boundaries, and stream events are reshaped into Pi's
//! partial-free `assistantMessageEvent` form.

use protocol::watch::{Ended, QueueKind, WatchEvent};
use serde_json::{Map, Value, json};

/// Translates lane events into Pi RPC wire events.
#[derive(Default)]
pub struct RpcEventTranslator {
    run_messages: Vec<Value>,
    turn_open: bool,
    turn_tool_results: Vec<Value>,
    tool_args: Vec<(String, Value)>,
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

impl RpcEventTranslator {
    /// The Pi RPC events a lane-watch event is (none for the ones the RPC stream does not show).
    pub fn translate(&mut self, event: WatchEvent) -> Vec<Value> {
        match event {
            WatchEvent::RunStart { .. } => {
                self.run_messages.clear();
                self.turn_open = false;
                self.turn_tool_results.clear();
                vec![json!({"type": "agent_start"})]
            }
            WatchEvent::RunEnd {
                status,
                error,
                ended_at,
                ..
            } => {
                let mut events = self.close_turn();
                // A run that failed before any response has no assistant error message; Pi clients learn of a
                // failed run from one, so it is shown as one.
                let answered_with_error = self
                    .run_messages
                    .last()
                    .is_some_and(|m| role(m) == Some("assistant") && m["stopReason"] == "error");
                if let Some(error) = error.filter(|_| status == Ended::Failed)
                    && !answered_with_error
                {
                    let message = run_failure_message(&error.message, ended_at);
                    self.run_messages.push(message.clone());
                    events.push(json!({"type": "message_start", "message": message}));
                    events.push(json!({"type": "message_end", "message": message}));
                }
                let messages = std::mem::take(&mut self.run_messages);
                events.push(json!({"type": "agent_end", "messages": messages, "willRetry": false}));
                events.push(json!({"type": "agent_settled"}));
                events
            }
            WatchEvent::MessageStart { message, .. } => {
                let mut events = Vec::new();
                if role(&message) == Some("assistant") {
                    events.extend(self.close_turn());
                    self.turn_open = true;
                    events.push(json!({"type": "turn_start"}));
                }
                events.push(json!({"type": "message_start", "message": message}));
                events
            }
            // The usage rides on the event (protocol::watch::stream_fold); an older worker's update carries its message.
            WatchEvent::MessageUpdate {
                frame, usage, message, ..
            } => {
                let message = message.filter(Value::is_object);
                if message.as_ref().is_some_and(|m| m["role"] != "assistant") {
                    return Vec::new();
                }
                let usage = message.map_or(usage, |m| m["usage"].clone());
                match frame_to_assistant_message_event(&frame) {
                    Some(update) => {
                        vec![json!({"type": "message_update", "usage": usage, "assistantMessageEvent": update})]
                    }
                    None => Vec::new(),
                }
            }
            WatchEvent::MessageEnd { run_id, message, .. } => {
                if run_id.is_some() {
                    self.run_messages.push(message.clone());
                }
                if role(&message) == Some("toolResult") {
                    self.turn_tool_results.push(message.clone());
                }
                vec![json!({"type": "message_end", "message": message})]
            }
            WatchEvent::ToolStart {
                tool_call_id: id,
                tool_name: name,
                args,
                ..
            } => {
                self.tool_args.push((id.clone(), args.clone()));
                vec![json!({"type": "tool_execution_start", "toolCallId": id, "toolName": name, "args": args})]
            }
            WatchEvent::ToolUpdate {
                tool_call_id: id,
                tool_name: name,
                partial_result,
                ..
            } => {
                let mut event = json!({"type": "tool_execution_update", "toolCallId": id, "toolName": name});
                if let Some((_, args)) = self.tool_args.iter().find(|(known, _)| *known == id) {
                    event["args"] = args.clone();
                }
                event["partialResult"] = partial_result;
                vec![event]
            }
            WatchEvent::ToolEnd {
                tool_call_id: id,
                tool_name: name,
                result,
                is_error,
                ..
            } => {
                self.tool_args.retain(|(known, _)| *known != id);
                vec![json!({
                    "type": "tool_execution_end", "toolCallId": id, "toolName": name, "result": result, "isError": is_error,
                })]
            }
            WatchEvent::QueueUpdate { queues, .. } => {
                let queues: Vec<(QueueKind, String)> = queues
                    .iter()
                    .filter(|item| item.item == "message")
                    .map(|item| (item.kind, item.message.as_ref().map(message_text).unwrap_or_default()))
                    .collect();
                vec![queue_update(&queues)]
            }
            WatchEvent::CompactionStart { reason, .. } => vec![json!({"type": "compaction_start", "reason": reason})],
            WatchEvent::CompactionEnd {
                reason, status, error, ..
            } => {
                let mut event = json!({"type": "compaction_end", "reason": reason, "aborted": status == Ended::Aborted,
                    "willRetry": false});
                if status == Ended::Failed {
                    event["errorMessage"] = error.map(|e| e.message).unwrap_or_default().into();
                }
                vec![event]
            }
            WatchEvent::EntryAdded { entry, .. } => vec![json!({"type": "entry_appended", "entry": entry})],
            WatchEvent::ConfigUpdate { property, value, .. } if property == "thinkingLevel" => {
                vec![json!({"type": "thinking_level_changed", "level": value.as_str().unwrap_or_default()})]
            }
            // AgentSession's `summarization_retry_scheduled` / `_attempt_start` / `_finished` (a compaction's or
            // branch summary's retry), as the RPC stream carries them.
            event @ (WatchEvent::SummarizationRetryScheduled { .. }
            | WatchEvent::SummarizationRetryAttemptStart { .. }
            | WatchEvent::SummarizationRetryFinished { .. }) => {
                let mut shown = event.to_value();
                if let Some(shown) = shown.as_object_mut() {
                    shown.remove("lane");
                }
                vec![shown]
            }
            // The run's generation failed and is retried (rpc-events.ts `auto_retry_start`).
            WatchEvent::RetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
                ..
            } => vec![
                json!({"type": "auto_retry_start", "attempt": attempt, "maxAttempts": max_attempts,
                "delayMs": delay_ms, "errorMessage": error_message}),
            ],
            // The run's retries ended (rpc-events.ts `auto_retry_end`).
            WatchEvent::RetryEnd {
                success,
                attempt,
                final_error,
                ..
            } => {
                let mut event = json!({"type": "auto_retry_end", "success": success, "attempt": attempt});
                if let Some(error) = final_error {
                    event["finalError"] = error.into();
                }
                vec![event]
            }
            // A chunk of a user command's output (AgentSession `bash_execution_update`), under the command's id.
            WatchEvent::BashExecutionUpdate { id, delta, .. } => {
                let mut event = json!({"type": "bash_execution_update"});
                if let Some(id) = id {
                    event["id"] = json!(id);
                }
                event["delta"] = delta.into();
                vec![event]
            }
            // The session was renamed (AgentSession `session_info_changed`).
            WatchEvent::SessionInfoChanged { name, .. } => {
                let mut event = json!({"type": "session_info_changed"});
                if let Some(name) = name {
                    event["name"] = json!(name);
                }
                vec![event]
            }
            _ => Vec::new(),
        }
    }

    fn close_turn(&mut self) -> Vec<Value> {
        if !self.turn_open {
            return Vec::new();
        }
        self.turn_open = false;
        let last_assistant = self.run_messages.iter().rev().find(|m| role(m) == Some("assistant"));
        let mut event = json!({"type": "turn_end"});
        if let Some(message) = last_assistant {
            event["message"] = message.clone();
        }
        event["toolResults"] = Value::Array(std::mem::take(&mut self.turn_tool_results));
        vec![event]
    }
}

/// `queueUpdate`: steering texts and follow-up (and next-run) texts.
pub fn queue_update(queues: &[(QueueKind, String)]) -> Value {
    let pick = |steer: bool| -> Vec<&str> {
        queues
            .iter()
            .filter(|(kind, _)| (*kind == QueueKind::Steer) == steer)
            .map(|(_, text)| text.as_str())
            .collect()
    };
    json!({"type": "queue_update", "steering": pick(true), "followUp": pick(false)})
}

/// `frameToAssistantMessageEvent` over the frame a provider event encodes to: the event without its `partial`;
/// start events of a tool call carry its id and name, end events the whole call. Terminal events have no frame.
pub fn frame_to_assistant_message_event(event: &Value) -> Option<Value> {
    let kind = event.get("type")?.as_str()?;
    let index = event.get("contentIndex").cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("type".into(), kind.into());
    match kind {
        "start" => {}
        "text_start" | "thinking_start" => {
            out.insert("contentIndex".into(), index);
        }
        "text_delta" | "thinking_delta" | "toolcall_delta" => {
            let delta = event.get("delta")?.clone();
            // The frame encoder drops empty tool-call deltas.
            if kind == "toolcall_delta" && delta.as_str() == Some("") {
                return None;
            }
            out.insert("contentIndex".into(), index);
            out.insert("delta".into(), delta);
        }
        "text_end" | "thinking_end" => {
            out.insert("contentIndex".into(), index);
            out.insert("content".into(), event.get("content")?.clone());
        }
        "toolcall_start" => {
            // A frame names the call; a provider event carries it in its partial message.
            let (id, name) = match (event.get("id"), event.get("toolName")) {
                (Some(id), Some(name)) => (id.clone(), name.clone()),
                _ => {
                    let call = &event["partial"]["content"][index.as_u64()? as usize];
                    (call.get("id")?.clone(), call.get("name")?.clone())
                }
            };
            out.insert("contentIndex".into(), index);
            out.insert("id".into(), id);
            out.insert("toolName".into(), name);
        }
        "toolcall_end" => {
            let call = event.get("toolCall")?;
            let mut tool_call = Map::new();
            tool_call.insert("type".into(), "toolCall".into());
            for key in ["id", "name", "arguments", "thoughtSignature", "namespace"] {
                if let Some(value) = call.get(key) {
                    tool_call.insert(key.into(), value.clone());
                }
            }
            out.insert("contentIndex".into(), index);
            out.insert("toolCall".into(), Value::Object(tool_call));
        }
        _ => return None,
    }
    Some(Value::Object(out))
}

use protocol::pi_message::text as message_text;

fn run_failure_message(error: &str, timestamp: i64) -> Value {
    json!({
        "role": "assistant", "content": [], "api": "openai-completions", "provider": "ultron", "model": "ultron",
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "error", "errorMessage": error, "timestamp": timestamp,
    })
}
