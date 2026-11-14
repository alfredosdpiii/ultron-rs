//! compaction::compact when the summarizer refuses a request as too long (Rust-only). The summary goes to the
//! lane's current model, and after a model switch that model may count the same history as more tokens than its
//! window holds. The history is then summarized in parts, each part updating the summary of the parts before it.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use ai::{AssistantMessage, Model};
use compaction::{CompactOptions, CompactionPreparation, CompactionSettings, FileOperations, SummaryRequest, compact};
use serde_json::{Value, json};

/// What the Claude Code CLI answered a 1,073,472-token summary request on a 1,000,000-token model.
const TOO_LONG: &str = "Claude Code CLI error (400): Prompt is too long · the request is ~1073472 tokens (limit \
                        1000000) but this conversation is only ~461329 tokens";

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "sum-1", "name": "Summarizer", "api": "anthropic-messages", "provider": "anthropic",
        "baseUrl": "https://example.invalid", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 3000, "maxTokens": 6000,
    }))
    .unwrap()
}

fn answer(stop: &str, text: &str, error: Option<&str>) -> AssistantMessage {
    serde_json::from_value(json!({
        "role": "assistant", "content": [{"type": "text", "text": text}], "api": "anthropic-messages",
        "provider": "anthropic", "model": "sum-1", "stopReason": stop, "errorMessage": error, "timestamp": 2,
        "usage": {"input": 10, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 11,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
    }))
    .unwrap()
}

/// `count` user messages `<name>-<i>.`, each about 400 characters.
fn messages(name: &str, count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| {
            let text = format!("{name}-{i}. {}", "x".repeat(400));
            json!({"role": "user", "content": [{"type": "text", "text": text}], "timestamp": i})
        })
        .collect()
}

fn preparation(history: Vec<Value>, prefix: Vec<Value>) -> CompactionPreparation {
    CompactionPreparation {
        first_kept_entry_id: "kept".into(),
        messages_to_summarize: history,
        is_split_turn: !prefix.is_empty(),
        turn_prefix_messages: prefix,
        tokens_before: 0,
        previous_summary: None,
        file_ops: FileOperations::default(),
        settings: CompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
        },
    }
}

/// A summarizer that refuses a request longer than `limit` characters as `refusal`, and answers the n-th request
/// it accepts `SUMMARY n`. `sent` records each request's text and whether it was accepted.
fn summarizer<'a>(
    limit: usize,
    refusal: &'a str,
    sent: &'a mut Vec<(String, bool)>,
) -> impl AsyncFnMut(SummaryRequest) -> AssistantMessage + 'a {
    async move |request: SummaryRequest| {
        let text = serde_json::to_string(&request.context).unwrap();
        let accepted = text.len() <= limit;
        sent.push((text, accepted));
        if accepted {
            let n = sent.iter().filter(|(_, accepted)| *accepted).count();
            answer("stop", &format!("SUMMARY {n}"), None)
        } else {
            answer("error", "", Some(refusal))
        }
    }
}

fn run(prep: &CompactionPreparation, limit: usize, refusal: &str) -> (Result<String, String>, Vec<(String, bool)>) {
    let model = model();
    let options = CompactOptions {
        model: &model,
        custom_instructions: None,
        thinking_level: None,
        session_id: Some("s"),
        now_ms: 1,
    };
    let mut sent = Vec::new();
    let result = block_on(compact(prep, &options, summarizer(limit, refusal, &mut sent)));
    (result.map(|r| r.summary), sent)
}

/// Each of `count` messages `<name>-<i>.` is in exactly one accepted request.
fn assert_each_summarized_once(sent: &[(String, bool)], name: &str, count: usize) {
    for i in 0..count {
        let marker = format!("{name}-{i}.");
        let holders = sent.iter().filter(|(text, ok)| *ok && text.contains(&marker)).count();
        assert_eq!(holders, 1, "{marker} is in {holders} accepted requests");
    }
}

#[test]
fn compaction_summarizes_a_history_the_summarizer_refuses_as_too_long_in_parts() {
    // Eight messages are about 3,400 characters with the prompt around them; half of them fit in 4,500.
    let prep = preparation(messages("history", 8), Vec::new());
    let (summary, sent) = run(&prep, 4_500, TOO_LONG);
    let accepted: Vec<&String> = sent.iter().filter(|(_, ok)| *ok).map(|(text, _)| text).collect();
    assert!(!sent[0].1, "the whole history is refused first");
    assert!(accepted.len() >= 2, "{} accepted requests", accepted.len());
    assert_each_summarized_once(&sent, "history", 8);
    assert!(
        !accepted[0].contains("<previous-summary>"),
        "the first part starts the summary"
    );
    for (n, text) in accepted.iter().enumerate().skip(1) {
        assert!(
            text.contains(&format!("<previous-summary>\\nSUMMARY {n}\\n</previous-summary>")),
            "part {} updates the summary of the parts before it",
            n + 1
        );
    }
    assert_eq!(summary, Ok(format!("SUMMARY {}", accepted.len())));
}

#[test]
fn compaction_summarizes_a_split_turn_prefix_the_summarizer_refuses_as_too_long_in_parts() {
    let prep = preparation(messages("history", 2), messages("prefix", 8));
    let (summary, sent) = run(&prep, 4_500, TOO_LONG);
    let accepted = sent.iter().filter(|(_, ok)| *ok).count();
    assert_each_summarized_once(&sent, "history", 2);
    assert_each_summarized_once(&sent, "prefix", 8);
    assert_eq!(
        summary,
        Ok(format!("SUMMARY {accepted}")),
        "the last part's answer holds the whole summary"
    );
}

#[test]
fn compaction_still_fails_when_one_message_is_too_long_or_the_error_is_not_about_length() {
    let prep = preparation(messages("history", 1), Vec::new());
    let (summary, sent) = run(&prep, 100, TOO_LONG);
    assert_eq!(summary, Err(format!("Summarization failed: {TOO_LONG}")));
    assert_eq!(sent.len(), 1, "one message cannot be split");

    let prep = preparation(messages("history", 8), Vec::new());
    let (summary, sent) = run(&prep, 100, "Overloaded");
    assert_eq!(summary, Err("Summarization failed: Overloaded".into()));
    assert_eq!(
        sent.len(),
        1,
        "an error that is not about length is not answered by splitting"
    );
}
