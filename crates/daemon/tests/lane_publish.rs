//! A lane publication sets the transcript aside for an event that leaves it alone (every streamed token): the lane
//! ends each event with the value a full copy-and-compare change gives, its transcript in place.

use daemon::chord::State;
use daemon::lane::Lane;
use serde_json::{Value, json};

fn snapshot() -> Value {
    json!({
        "lane": "main",
        "tipId": "b",
        "operation": null,
        "transcript": [
            {"id": "a", "type": "message", "message": {"role": "user", "content": "hi"}},
            {"id": "b", "type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}},
        ],
        "stats": {"messageCount": 2},
    })
}

fn streaming(text: &str) -> Value {
    json!({
        "type": "message_update", "runId": "r1", "lane": "main",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "pending"},
    })
}

#[test]
fn lane_publish_folds_like_a_full_change_and_keeps_the_transcript() {
    let lane = Lane::new(snapshot());
    let full = State::new(json!({"snapshot": snapshot(), "event": null}));
    let events = [
        json!({"type": "run_start", "runId": "r1", "startedAt": 1, "lane": "main"}),
        streaming("wor"),
        streaming("working"),
        json!({"type": "tool_start", "runId": "r1", "lane": "main", "toolCallId": "t1", "toolName": "rlm", "args": {}}),
        json!({"type": "tool_update", "runId": "r1", "lane": "main", "toolCallId": "t1", "partialResult": {"n": 1}}),
        json!({"type": "message_end", "runId": "r1", "lane": "main", "message": {"role": "assistant"}}),
        json!({"type": "entry_added", "lane": "main", "entry": {"id": "c", "type": "message", "message": {"role": "assistant"}}}),
        json!({"type": "compaction_start", "runId": "r1", "lane": "main"}),
        json!({"type": "entry_added", "lane": "main", "entry": {"id": "d", "type": "compaction", "summary": "s"}}),
        json!({"type": "run_end", "runId": "r1", "lane": "main", "status": "completed", "tipId": "d", "endedAt": 2}),
    ];
    for event in events {
        lane.publish(event.clone());
        full.change(|state| {
            harness::reducer::reduce_lane_snapshot(&mut state["snapshot"], &event);
            state["event"] = event.clone();
        });
        assert_eq!(lane.transcript.value(), full.value(), "after {}", event["type"]);
    }
    let ids: Vec<Value> = lane.snapshot()["transcript"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].clone())
        .collect();
    assert_eq!(ids, [json!("d")]);
}

/// A change that panics while the transcript is set aside leaves it in place: the lock is recovered after the panic,
/// so a transcript left out would stay null for every later reader.
#[test]
fn lane_publish_panic_keeps_the_transcript() {
    let state = State::new(json!({"snapshot": snapshot(), "event": null}));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.change_beside("/snapshot/transcript", |_| panic!("a malformed event"));
    }));
    assert!(panicked.is_err());
    assert_eq!(state.value(), json!({"snapshot": snapshot(), "event": null}));
    state.change_beside("/snapshot/transcript", |value| value["event"] = json!("next"));
    assert_eq!(
        state.read(|value| value["snapshot"]["transcript"].clone()),
        snapshot()["transcript"]
    );
    assert_eq!(state.read(|value| value["event"].clone()), json!("next"));
}
