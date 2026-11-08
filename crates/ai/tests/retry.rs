use ai::retry::{is_retryable, is_retryable_text};
use ai::{AssistantMessage, Usage};
use serde_json::json;

fn ended(stop_reason: &str, text: &str) -> AssistantMessage {
    serde_json::from_value(json!({
        "content": [], "api": "a", "provider": "p", "model": "m", "usage": Usage::default(),
        "stopReason": stop_reason, "errorMessage": text, "timestamp": 0,
    }))
    .unwrap()
}

fn failed(text: &str) -> AssistantMessage {
    ended("error", text)
}

/// pi-ai's `isRetryableAssistantError` lists, the one classifier of session turns, summaries and review frames.
#[test]
fn ai_retry_classifies_failures_as_pi_ai_does() {
    assert!(is_retryable(&failed("503 Service Unavailable")));
    assert!(is_retryable(&failed("fetch failed")));
    // Account limits are never retried, whatever status they carry.
    assert!(!is_retryable(&failed("429 insufficient_quota")));
    assert!(!is_retryable_text(
        "Claude Code usage limit reached (five_hour window; resets at 2026-10-01T00:00:00.000Z): 429. The subscription's capacity is exhausted for now; this is not retried."
    ));
    // Text TS's list does not name is not retried.
    assert!(!is_retryable_text("server_busy"));
    assert!(!is_retryable_text("model is at capacity"));
    // Only an error turn.
    assert!(!is_retryable(&ended("stop", "overloaded")));
}
