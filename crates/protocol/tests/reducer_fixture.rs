//! Replays crates/protocol/fixtures/reducer.json, recorded from the TypeScript lane snapshot reducer
//! (packages/agent/src/harness/runtime/reducer.ts) by fixtures/generate-reducer.mjs: event streams of real lanes
//! (faux provider) must fold to the authoritative resnapshot, and synthetic sequences must match TypeScript's
//! reduction and snapshot after every event.

use protocol::watch::reducer::{Reduction, reduce_lane_snapshot};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/reducer.json")).unwrap()
}

fn reduction(value: Option<Reduction>) -> Value {
    match value {
        Some(Reduction::Rebase) => "rebase".into(),
        None => Value::Null,
    }
}

#[test]
fn protocol_reducer_folds_lane_events_to_resnapshot() {
    let fixture = fixture();
    let lanes = fixture["lane"].as_object().unwrap();
    assert!(lanes.len() >= 5);
    for (name, phases) in lanes {
        for (n, phase) in phases.as_array().unwrap().iter().enumerate() {
            let mut snapshot = phase["start"].clone();
            for (event, expected) in phase["events"]
                .as_array()
                .unwrap()
                .iter()
                .zip(phase["reductions"].as_array().unwrap())
            {
                assert_eq!(
                    reduction(reduce_lane_snapshot(&mut snapshot, event)),
                    *expected,
                    "{name} phase {n} {}",
                    event["type"]
                );
            }
            assert_eq!(snapshot, phase["resnapshot"], "{name} phase {n} folded vs resnapshot");
            assert_eq!(snapshot, phase["folded"], "{name} phase {n} folded vs TypeScript fold");
        }
    }
}

#[test]
fn protocol_reducer_matches_typescript_on_synthetic_events() {
    let fixture = fixture();
    let cases = fixture["synthetic"].as_object().unwrap();
    assert!(cases.len() >= 6);
    for (name, case) in cases {
        let mut snapshot = case["snapshot"].clone();
        for (n, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let got = reduction(reduce_lane_snapshot(&mut snapshot, &step["event"]));
            assert_eq!(got, step["reduction"], "{name} step {n} reduction");
            assert_eq!(
                snapshot, step["snapshot"],
                "{name} step {n} ({}) snapshot",
                step["event"]["type"]
            );
        }
    }
}

/// A `message_update` folds its frame and usage into the streaming message; an older worker's whole message replaces
/// the fold.
#[test]
fn protocol_stream_fold_takes_an_older_workers_whole_message() {
    let mut snapshot = json!({"lane": "main", "operation": {"id": "r1", "kind": "run",
        "streamingMessage": {"role": "assistant", "content": [{"type": "text", "text": "Hel"}]}}});
    let update = |fields: Value| {
        let mut event = json!({"type": "message_update", "runId": "r1", "lane": "main"});
        for (key, value) in fields.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    };
    reduce_lane_snapshot(
        &mut snapshot,
        &update(
            json!({"frame": {"type": "text_delta", "contentIndex": 0, "delta": "lo"}, "usage": json!({"output": 2})}),
        ),
    );
    assert_eq!(snapshot["operation"]["streamingMessage"]["content"][0]["text"], "Hello");
    assert_eq!(snapshot["operation"]["streamingMessage"]["usage"], json!({"output": 2}));
    let whole = json!({"role": "assistant", "content": [{"type": "text", "text": "Hello, world"}]});
    reduce_lane_snapshot(
        &mut snapshot,
        &update(json!({"message": whole, "frame": {"type": "text_delta", "contentIndex": 0, "delta": ", world"}})),
    );
    assert_eq!(
        snapshot["operation"]["streamingMessage"], whole,
        "a whole message replaces the fold"
    );
}
