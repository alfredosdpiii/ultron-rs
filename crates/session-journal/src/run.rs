//! The vocabulary of the harness operation journal of a run on lane `main` (the TypeScript drive's
//! `harness/runtime/lane.ts`, `drive/{checkpoint,generation,response,tools,terminal}.ts`): its session value
//! namespaces, the lane configuration ([`ensure_lane`]) and default run settings, and the assistant frame encoder.
//! The journal itself is written by `session_core::op_journal::Run`.

use crate::session::{BRANCH_TIP, LANE_CONFIG, LANE_STATE, Session};
use crate::state::Write;
use serde_json::{Map, Value, json};

pub const OP_META: &str = "ultron.op.meta";
pub const OP_STATE: &str = "ultron.op.state";
pub const OP_RESULT: &str = "ultron.result";
pub const OP_TOOL_ARGS: &str = "ultron.op.tool_args";
pub const PENDING_ENTRY: &str = "ultron.pending.entry";
pub const PENDING_TOOL_OUTPUT: &str = "ultron.pending.tool_output";
pub const PENDING_FRAMES: &str = "ultron.pending.assistant_frame";
pub const ULTRON_USAGE: &str = "ultron.usage";

/// `LaneConfiguration`.
pub fn lane_config(provider: &str, model_id: &str, thinking_level: &str, tools: &[&str]) -> Value {
    json!({"model": {"provider": provider, "modelId": model_id}, "thinkingLevel": thinking_level, "activeToolNames": tools})
}

/// The default `RunSettings` (compaction on, reserve 16384, keep 20000; queues "all"; parallel tools).
pub fn default_settings() -> Value {
    json!({
        "compaction": {"enabled": true, "reserveTokens": 16384, "keepRecentTokens": 20000},
        "steeringMode": "all", "followUpMode": "all", "toolExecution": "parallel",
    })
}

fn idle_lane() -> Value {
    json!({"currentOperationId": null, "lastOperationId": null, "inbox": []})
}

/// Prepare lane `main` for a run and return the configuration it runs with. A new session gets its tip, the
/// configuration and an idle state in one transaction. A resumed session keeps its stored model and thinking
/// level; only the active tool names follow this run (rewritten when they differ).
pub fn ensure_lane(session: &mut Session, wanted: &Value) -> Result<Value, String> {
    let stored = session.value(LANE_CONFIG, "main").cloned();
    let has_tip = session.branch_tip("main").is_some();
    match stored {
        None => {
            let mut writes = Vec::new();
            if !has_tip {
                writes.push(Write::set(BRANCH_TIP, "main", Value::Null));
            }
            writes.push(Write::set(LANE_CONFIG, "main", wanted.clone()));
            writes.push(Write::set(LANE_STATE, "main", idle_lane()));
            session.commit(&writes)?;
            Ok(wanted.clone())
        }
        Some(stored) => {
            let mut config = stored.clone();
            config["activeToolNames"] = wanted["activeToolNames"].clone();
            if config != stored {
                session.commit(&[Write::set(LANE_CONFIG, "main", config.clone())])?;
            }
            Ok(config)
        }
    }
}

/// `AssistantMessageFrameEncoder` for snapshot events: each block's start frame carries the block as of its start
/// event and every later delta becomes a delta frame (the TypeScript encoder skips deltas a live start snapshot
/// already covered; both reduce to the same message).
#[derive(Default)]
pub struct FrameEncoder {
    started: bool,
}

fn clone_start(partial: &Value) -> Value {
    let mut out = Map::new();
    out.insert("role".into(), "assistant".into());
    out.insert("content".into(), json!([]));
    for key in [
        "api",
        "provider",
        "model",
        "responseModel",
        "responseId",
        "providerThinkingLevel",
        "diagnostics",
    ] {
        if let Some(v) = partial.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    out.insert("usage".into(), partial["usage"].clone());
    out.insert("stopReason".into(), "pending".into());
    out.insert("timestamp".into(), partial["timestamp"].clone());
    Value::Object(out)
}

fn pick(source: &Value, keys: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for key in keys {
        if let Some(v) = source.get(*key) {
            out.insert((*key).into(), v.clone());
        }
    }
    out
}

impl FrameEncoder {
    pub fn encode(&mut self, event: &Value) -> Option<Value> {
        let kind = event["type"].as_str()?;
        let index = event.get("contentIndex").cloned().unwrap_or(Value::Null);
        let block = || event["partial"]["content"][index.as_u64().unwrap_or(0) as usize].clone();
        let frame = |kind: &str, mut rest: Map<String, Value>| {
            let mut out = Map::new();
            out.insert("type".into(), kind.into());
            out.insert("contentIndex".into(), index.clone());
            out.append(&mut rest);
            Some(Value::Object(out))
        };
        match kind {
            "start" => {
                self.started = true;
                Some(json!({"type": "start", "partial": clone_start(&event["partial"])}))
            }
            _ if !self.started => None,
            "text_start" => frame(
                "text_start",
                [(
                    "content".to_string(),
                    Value::Object(pick(&block(), &["type", "text", "textSignature"])),
                )]
                .into_iter()
                .collect(),
            ),
            "thinking_start" => frame(
                "thinking_start",
                [(
                    "content".to_string(),
                    Value::Object(pick(&block(), &["type", "thinking", "thinkingSignature", "redacted"])),
                )]
                .into_iter()
                .collect(),
            ),
            "text_delta" | "thinking_delta" | "toolcall_delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if delta.is_empty() && kind == "toolcall_delta" {
                    return None;
                }
                frame(kind, [("delta".to_string(), delta.into())].into_iter().collect())
            }
            "text_end" => {
                let mut rest = Map::new();
                rest.insert("content".into(), event["content"].clone());
                rest.append(&mut pick(&block(), &["textSignature"]));
                frame("text_end", rest)
            }
            "thinking_end" => {
                let mut rest = Map::new();
                rest.insert("content".into(), event["content"].clone());
                rest.append(&mut pick(&block(), &["thinkingSignature", "redacted"]));
                frame("thinking_end", rest)
            }
            "toolcall_start" => frame(
                "toolcall_start",
                [(
                    "toolCall".to_string(),
                    Value::Object(pick(
                        &block(),
                        &["type", "id", "name", "arguments", "thoughtSignature", "namespace"],
                    )),
                )]
                .into_iter()
                .collect(),
            ),
            "toolcall_end" => frame(
                "toolcall_end",
                pick(
                    &event["toolCall"],
                    &["id", "name", "arguments", "thoughtSignature", "namespace"],
                ),
            ),
            _ => None,
        }
    }
}
