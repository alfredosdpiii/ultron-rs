//! The streaming assistant message, folded from `message_update` frames (Rust-only). A `message_update` event
//! carries the provider frame and the usage so far, not the message: sending the whole message with every delta made
//! streaming quadratic in the answer's length (serialized, sent and diffed once per token). A watcher starts from
//! the `message_start` message and folds each frame into it, as the provider built its partial:
//!
//! - `text_start` / `thinking_start` open an empty block at the frame's index; `*_delta` appends; `*_end` sets the
//!   block's final text.
//! - `toolcall_start` opens a call (`id`, `toolName` from the frame) with empty arguments; `toolcall_delta` appends to
//!   its streamed argument text, kept beside the call as `partialJson`, and parses it as the providers do
//!   (`ai::json::parse_streaming_json`); `toolcall_end` puts the finished call in its place.
//!
//! A `message_update` from an older worker still carries `message`: it replaces the fold ([`message_of`]).

use serde_json::{Map, Value, json};

/// The streamed argument text of a tool call still being written (dropped when the call ends).
pub const PARTIAL_JSON: &str = "partialJson";

/// Fold one `message_update` event into `message` (the streaming message so far): its frame, and its usage.
pub fn fold_update(message: &mut Value, event: &Value) {
    if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
        message["usage"] = usage.clone();
    }
    fold_frame(message, &event["frame"]);
}

/// The message an update gives a watcher that had none: an older worker's whole message, else nothing.
pub fn message_of(event: &Value) -> Option<&Value> {
    event.get("message").filter(|message| message.is_object())
}

/// Fold one provider frame into `message`.
pub fn fold_frame(message: &mut Value, frame: &Value) {
    let Some(kind) = frame["type"].as_str() else {
        return;
    };
    let Some(index) = frame["contentIndex"].as_u64().map(|index| index as usize) else {
        return;
    };
    if !message["content"].is_array() {
        message["content"] = json!([]);
    }
    let Some(content) = message["content"].as_array_mut() else {
        return;
    };
    let block = match kind {
        "text_start" => Some(json!({"type": "text", "text": ""})),
        "thinking_start" => Some(json!({"type": "thinking", "thinking": ""})),
        "toolcall_start" => Some(json!({
            "type": "toolCall", "id": frame["id"], "name": frame["toolName"], "arguments": {},
        })),
        "toolcall_end" => frame.get("toolCall").filter(|call| call.is_object()).cloned(),
        _ => None,
    };
    if let Some(block) = block {
        if index < content.len() {
            content[index] = block;
        } else {
            // A frame for a later index than the next: the blocks between are unknown here.
            content.resize(index, Value::Null);
            content.push(block);
        }
        return;
    }
    let Some(block) = content.get_mut(index).and_then(Value::as_object_mut) else {
        return;
    };
    match kind {
        "text_delta" | "thinking_delta" => {
            let key = if kind == "text_delta" { "text" } else { "thinking" };
            append(block, key, frame["delta"].as_str().unwrap_or_default());
        }
        "text_end" | "thinking_end" => {
            let key = if kind == "text_end" { "text" } else { "thinking" };
            if let Some(text) = frame["content"].as_str() {
                block.insert(key.into(), text.into());
            }
        }
        "toolcall_delta" => {
            append(block, PARTIAL_JSON, frame["delta"].as_str().unwrap_or_default());
            let arguments = ai::json::parse_streaming_json(block[PARTIAL_JSON].as_str().unwrap_or_default());
            block.insert("arguments".into(), arguments);
        }
        _ => {}
    }
}

fn append(block: &mut Map<String, Value>, key: &str, delta: &str) {
    match block.get_mut(key) {
        Some(Value::String(text)) => text.push_str(delta),
        _ => {
            block.insert(key.into(), delta.into());
        }
    }
}
