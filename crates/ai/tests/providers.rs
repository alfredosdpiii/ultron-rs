//! Replays `fixtures/providers.json` (recorded from the TypeScript providers by `fixtures/generate.mjs`): the same canned
//! HTTP/SSE responses go through the Rust providers, which must send the same requests (URL, method, headers minus SDK
//! telemetry, JSON body including key order), emit the same events, and return the same final message.

use crate::provider_replay::replay;
use serde_json::Value;
use std::sync::LazyLock;

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/providers.json")).expect("fixture parses"));

const ANTHROPIC_STREAM: &[&str] = &[
    "anthropic/thinking-text-tool-history",
    "anthropic/split-bytes-crlf",
    "anthropic/redacted-length-long-cache",
    "anthropic/repair-control-chars",
];
const ANTHROPIC_REQUESTS: &[&str] = &[
    "anthropic/env-long-retention",
    "anthropic/oauth-claude-code",
    "anthropic/managed-effort-mid-convo",
    "anthropic/beta-header-configured-and-suppressed",
    "anthropic/beta-null-suppresses",
    "anthropic/copilot-and-header-auth",
    "anthropic/openrouter-session-affinity-header-auth",
    "anthropic/on-payload",
    "anthropic/fallback-model-and-transformations",
];
const ANTHROPIC_ERRORS: &[&str] = &[
    "anthropic/http-400",
    "anthropic/sse-error-event",
    "anthropic/ended-before-stop",
    "anthropic/refusal",
    "anthropic/unknown-stop-reason",
    "anthropic/sensitive-and-pause",
    "anthropic/missing-key",
    "anthropic/missing-key-simple-throws",
];
const ANTHROPIC_RETRY: &[&str] = &[
    "anthropic/retry-429-then-ok",
    "anthropic/retry-delay-too-long",
    "anthropic/no-retry-should-retry-false",
];
const ANTHROPIC_ABORT: &[&str] = &["anthropic/abort-after-first-delta"];
const ANTHROPIC_SIMPLE: &[&str] = &[
    "anthropic/simple-reasoning-budget",
    "anthropic/simple-adaptive-catalog-model",
    "anthropic/simple-no-reasoning",
];
const COMPLETIONS_STREAM: &[&str] = &[
    "completions/reasoning-text-parallel-tools",
    "completions/openrouter-anthropic-cache-reasoning-details",
    "completions/grammar-custom-tool",
    "completions/moonshot-choice-usage",
    "completions/finish-inferred-tool-use",
];
const COMPLETIONS_REQUESTS: &[&str] = &[
    "completions/deepseek-detection",
    "completions/zai-tool-stream",
    "completions/requires-flags-and-tool-images",
    "completions/header-auth-and-on-payload",
    "completions/long-session-id-cache-key",
];
const COMPLETIONS_ERRORS: &[&str] = &[
    "completions/http-400-json",
    "completions/http-502-text",
    "completions/http-403-json-without-error",
    "completions/in-stream-error",
    "completions/content-filter",
    "completions/no-finish-reason",
    "completions/missing-key-throws-simple",
];
const COMPLETIONS_RETRY: &[&str] = &["completions/retry-500-then-ok"];
const COMPLETIONS_ABORT: &[&str] = &["completions/abort"];
const COMPLETIONS_SIMPLE: &[&str] = &[
    "completions/simple-chat-template-clamped",
    "completions/simple-off-qwen",
];
const RESPONSES_STREAM: &[&str] = &[
    "responses/reasoning-text-tool-history",
    "responses/grammar-custom-tool-and-priority-tier",
    "responses/refusal-and-final-answer-phase",
    "responses/backfill-encrypted-reasoning",
];
const RESPONSES_REQUESTS: &[&str] = &["responses/xai-copilot-no-max-output", "responses/simple-long-cache"];
const RESPONSES_ERRORS: &[&str] = &[
    "responses/incomplete-max-output",
    "responses/incomplete-content-filter",
    "responses/failed",
    "responses/error-event",
    "responses/no-terminal-event",
    "responses/http-401",
    "responses/http-401-other-provider",
];
const RESPONSES_ABORT: &[&str] = &["responses/abort"];

#[test]
fn ai_anthropic_streams_thinking_text_and_tool_calls() {
    replay(&FIXTURE, ANTHROPIC_STREAM);
}

#[test]
fn ai_anthropic_builds_requests_like_upstream() {
    replay(&FIXTURE, ANTHROPIC_REQUESTS);
}

#[test]
fn ai_anthropic_errors_and_stop_reasons() {
    replay(&FIXTURE, ANTHROPIC_ERRORS);
}

#[test]
fn ai_anthropic_retries_like_upstream() {
    replay(&FIXTURE, ANTHROPIC_RETRY);
}

#[test]
fn ai_anthropic_abort_mid_stream() {
    replay(&FIXTURE, ANTHROPIC_ABORT);
}

#[test]
fn ai_anthropic_simple_options() {
    replay(&FIXTURE, ANTHROPIC_SIMPLE);
}

#[test]
fn ai_completions_streams_reasoning_text_and_tool_calls() {
    replay(&FIXTURE, COMPLETIONS_STREAM);
}

#[test]
fn ai_completions_builds_requests_with_detected_compat() {
    replay(&FIXTURE, COMPLETIONS_REQUESTS);
}

#[test]
fn ai_completions_errors_and_stop_reasons() {
    replay(&FIXTURE, COMPLETIONS_ERRORS);
}

#[test]
fn ai_completions_retries_like_upstream() {
    replay(&FIXTURE, COMPLETIONS_RETRY);
}

#[test]
fn ai_completions_abort_mid_stream() {
    replay(&FIXTURE, COMPLETIONS_ABORT);
}

#[test]
fn ai_completions_simple_options() {
    replay(&FIXTURE, COMPLETIONS_SIMPLE);
}

#[test]
fn ai_responses_streams_reasoning_text_and_tool_calls() {
    replay(&FIXTURE, RESPONSES_STREAM);
}

#[test]
fn ai_responses_builds_requests_with_compat() {
    replay(&FIXTURE, RESPONSES_REQUESTS);
}

#[test]
fn ai_responses_errors_and_stop_reasons() {
    replay(&FIXTURE, RESPONSES_ERRORS);
}

#[test]
fn ai_responses_abort_mid_stream() {
    replay(&FIXTURE, RESPONSES_ABORT);
}

#[test]
fn ai_every_provider_fixture_scenario_is_replayed() {
    let groups = [
        ANTHROPIC_STREAM,
        ANTHROPIC_REQUESTS,
        ANTHROPIC_ERRORS,
        ANTHROPIC_RETRY,
        ANTHROPIC_ABORT,
        ANTHROPIC_SIMPLE,
        COMPLETIONS_STREAM,
        COMPLETIONS_REQUESTS,
        COMPLETIONS_ERRORS,
        COMPLETIONS_RETRY,
        COMPLETIONS_ABORT,
        COMPLETIONS_SIMPLE,
        RESPONSES_STREAM,
        RESPONSES_REQUESTS,
        RESPONSES_ERRORS,
        RESPONSES_ABORT,
    ];
    let covered: Vec<&str> = groups.iter().flat_map(|g| g.iter().copied()).collect();
    let missing: Vec<&str> = FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .filter(|n| !covered.contains(n))
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
}
