//! pi-ai's `isContextOverflow` (`utils/overflow.ts`) without a context window: the error texts providers use for a
//! request that is too long for the model.

use crate::{AssistantMessage, StopReason};
use regex::Regex;
use std::sync::LazyLock;

fn patterns(sources: &[&str]) -> Vec<Regex> {
    sources
        .iter()
        .map(|source| Regex::new(&format!("(?i){source}")).expect("valid pattern"))
        .collect()
}

static OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"prompt (?:is )?too long",
        r"prompt exceeds max length",
        r"request_too_large",
        r"input is too long for requested model",
        r"exceeds the context window",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"input token count.*exceeds the maximum",
        r"maximum prompt length is \d+",
        r"reduce the length of the messages",
        r"maximum context length is \d+ tokens",
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"exceeds the limit of \d+",
        r"exceeds the available context size",
        r"greater than the context length",
        r"context window exceeds limit",
        r"exceeded model token limit",
        r"too large for model with \d+ maximum context length",
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"model_context_window_exceeded",
        r"prompt too long; exceeded (?:max )?context length",
        r"range of input length should be",
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ])
});

static NON_OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"^(Throttling error|Service unavailable):",
        r"rate limit",
        r"too many requests",
    ])
});

static CEREBRAS_BODYLESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)").expect("valid pattern"));

/// pi-ai `isContextOverflow` without a context window: an error message a provider uses for an oversized request.
pub fn is_context_overflow(message: &AssistantMessage) -> bool {
    let Some(text) = message
        .error_message
        .as_deref()
        .filter(|_| message.stop_reason == StopReason::Error)
    else {
        return false;
    };
    if NON_OVERFLOW.iter().any(|pattern| pattern.is_match(text)) {
        return false;
    }
    OVERFLOW.iter().any(|pattern| pattern.is_match(text))
        || (message.provider == "cerebras" && CEREBRAS_BODYLESS.is_match(text))
}
