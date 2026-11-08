//! Replays the fixtures recorded from the TypeScript providers by `fixtures/generate-google.mjs`: the same canned HTTP/SSE
//! responses go through the Rust providers, which must send the same requests (URL, method, headers minus SDK telemetry,
//! JSON body including key order), emit the same events, and return the same final message.

use crate::provider_replay::replay_all;
use ai::StopReason;
use ai::google_shared;
use ai::mistral;
use serde_json::{Value, json};
use std::sync::LazyLock;

static GOOGLE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/google.json")).expect("fixture parses"));
static VERTEX: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/google-vertex.json")).expect("fixture parses"));
static MISTRAL: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/mistral.json")).expect("fixture parses"));

#[test]
fn ai_google_replays_recorded_typescript_scenarios() {
    replay_all(&GOOGLE);
}

#[test]
fn ai_google_vertex_replays_recorded_typescript_scenarios() {
    replay_all(&VERTEX);
}

#[test]
fn ai_mistral_replays_recorded_typescript_scenarios() {
    replay_all(&MISTRAL);
}

#[test]
fn ai_mistral_tool_call_ids_are_nine_alphanumerics() {
    // Already nine alphanumerics after stripping: kept as is.
    assert_eq!(mistral::derive_mistral_tool_call_id("abc-DEF_123", 0), "abcDEF123");
    // The recorded TypeScript request maps this foreign id to "88lrg5dqg" (fixtures/mistral.json).
    assert_eq!(
        mistral::derive_mistral_tool_call_id("call|weird+id/with=chars", 0),
        "88lrg5dqg"
    );
    let retry = mistral::derive_mistral_tool_call_id("abc-DEF_123", 1);
    assert_eq!(retry.len(), 9);
    assert_ne!(retry, "abcDEF123");
}

#[test]
fn ai_google_shared_model_predicates_match_typescript() {
    let units = &GOOGLE["units"];
    for case in units["usesThinkingLevel"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        assert_eq!(
            json!(google_shared::uses_google_thinking_level(id)),
            case["output"],
            "usesGoogleThinkingLevel({id})"
        );
    }
    for case in units["requiresToolCallId"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        assert_eq!(
            json!(google_shared::requires_tool_call_id(id)),
            case["output"],
            "requiresToolCallId({id})"
        );
    }
}

#[test]
fn ai_google_unknown_finish_reason_is_an_error() {
    assert_eq!(google_shared::map_stop_reason("SAFETY"), Ok(StopReason::Error));
    assert_eq!(
        google_shared::map_stop_reason("BRAND_NEW"),
        Err("Unhandled stop reason: BRAND_NEW".to_string())
    );
}
