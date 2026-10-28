//! A `message_update` carries its frame and usage, not the message (session_core::journal::update_fields), and a
//! watcher folds the frames into the `message_start` message (harness::stream_fold). Played over a provider's stream
//! of text, thinking and a code cell streamed as partial JSON: after every event the fold equals the provider's own
//! partial message, the streamed argument text aside.

use ai::{AssistantMessage, AssistantMessageEvent, Message};
use harness::stream_fold::{PARTIAL_JSON, fold_update};
use serde_json::{Value, json};
use session_core::journal::update_fields;

fn usage(output: u64) -> Value {
    json!({"input": 10, "output": output, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10 + output,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

/// The provider's partial message after `content`, with `output` tokens so far.
fn partial(content: Value, output: u64) -> AssistantMessage {
    serde_json::from_value(json!({
        "role": "assistant", "content": content, "api": "anthropic-messages", "provider": "anthropic",
        "model": "claude", "usage": usage(output), "stopReason": "stop", "timestamp": 1,
    }))
    .unwrap()
}

fn event(fields: Value, partial: &AssistantMessage) -> AssistantMessageEvent {
    let mut fields = fields;
    fields["partial"] = serde_json::to_value(partial).unwrap();
    serde_json::from_value(fields).unwrap()
}

/// `message` without the fold's streamed argument text.
fn shown(message: &Value) -> Value {
    let mut message = message.clone();
    for block in message["content"].as_array_mut().into_iter().flatten() {
        if let Some(block) = block.as_object_mut() {
            block.remove(PARTIAL_JSON);
        }
    }
    message
}

#[test]
fn session_core_stream_fold_rebuilds_the_partial_message_from_frames() {
    let mut text = String::new();
    let mut args = String::new();
    let call = |args: &str| json!({"type": "toolCall", "id": "c1", "name": "rlm", "arguments": ai::json::parse_streaming_json(args)});
    // (the event's own fields, the content after it): a provider's stream, step by step.
    let mut steps: Vec<(Value, Value)> = Vec::new();
    steps.push((
        json!({"type": "thinking_start", "contentIndex": 0}),
        json!([{"type": "thinking", "thinking": ""}]),
    ));
    steps.push((
        json!({"type": "thinking_delta", "contentIndex": 0, "delta": "Count first."}),
        json!([{"type": "thinking", "thinking": "Count first."}]),
    ));
    steps.push((
        json!({"type": "thinking_end", "contentIndex": 0, "content": "Count first."}),
        json!([{"type": "thinking", "thinking": "Count first."}]),
    ));
    let thinking = json!({"type": "thinking", "thinking": "Count first."});
    steps.push((
        json!({"type": "text_start", "contentIndex": 1}),
        json!([thinking, {"type": "text", "text": ""}]),
    ));
    for piece in ["I'll ", "count ", "the rows."] {
        text.push_str(piece);
        steps.push((
            json!({"type": "text_delta", "contentIndex": 1, "delta": piece}),
            json!([thinking, {"type": "text", "text": text}]),
        ));
    }
    steps.push((
        json!({"type": "text_end", "contentIndex": 1, "content": text}),
        json!([thinking, {"type": "text", "text": text}]),
    ));
    let said = json!({"type": "text", "text": text});
    steps.push((
        json!({"type": "toolcall_start", "contentIndex": 2}),
        json!([thinking, said, call("")]),
    ));
    for piece in ["{\"co", "de\": \"print(len(", "rows))\"}"] {
        args.push_str(piece);
        steps.push((
            json!({"type": "toolcall_delta", "contentIndex": 2, "delta": piece}),
            json!([thinking, said, call(&args)]),
        ));
    }
    let done = json!({"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "print(len(rows))"}});
    steps.push((
        json!({"type": "toolcall_end", "contentIndex": 2, "toolCall": done}),
        json!([thinking, said, done]),
    ));

    let start = partial(json!([]), 0);
    let mut folded = serde_json::to_value(&start).unwrap();
    for (number, (fields, content)) in steps.into_iter().enumerate() {
        let now = partial(content, number as u64 + 1);
        let provider_event = event(fields, &now);
        let (frame, usage) = update_fields(&Message::Assistant(now.clone()), &provider_event);
        assert!(
            frame.get("partial").is_none(),
            "an update never carries the response so far"
        );
        let update = json!({"type": "message_update", "runId": "r1", "lane": "main", "frame": frame, "usage": usage});
        fold_update(&mut folded, &update);
        assert_eq!(
            shown(&folded),
            serde_json::to_value(&now).unwrap(),
            "after {}",
            update["frame"]
        );
    }
    assert!(
        folded["content"][2].get(PARTIAL_JSON).is_none(),
        "a finished call drops its streamed text"
    );
}

#[test]
fn session_core_stream_fold_takes_an_older_workers_whole_message() {
    let mut snapshot = json!({"lane": "main", "operation": {"id": "r1", "kind": "run",
        "streamingMessage": {"role": "assistant", "content": [{"type": "text", "text": "Hel"}]}}});
    let update = |fields: Value| {
        let mut event = json!({"type": "message_update", "runId": "r1", "lane": "main"});
        for (key, value) in fields.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    };
    harness::reducer::reduce_lane_snapshot(
        &mut snapshot,
        &update(json!({"frame": {"type": "text_delta", "contentIndex": 0, "delta": "lo"}, "usage": usage(2)})),
    );
    assert_eq!(snapshot["operation"]["streamingMessage"]["content"][0]["text"], "Hello");
    assert_eq!(snapshot["operation"]["streamingMessage"]["usage"], usage(2));
    let whole = json!({"role": "assistant", "content": [{"type": "text", "text": "Hello, world"}]});
    harness::reducer::reduce_lane_snapshot(
        &mut snapshot,
        &update(json!({"message": whole, "frame": {"type": "text_delta", "contentIndex": 0, "delta": ", world"}})),
    );
    assert_eq!(
        snapshot["operation"]["streamingMessage"], whole,
        "a whole message replaces the fold"
    );
}
