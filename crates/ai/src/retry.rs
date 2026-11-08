//! pi-ai's retry (`utils/retry.ts`): which failed assistant turns are transient provider or transport errors, and
//! the policy that bounds how often and how late they are tried again (`settings.retry`).

use crate::{AssistantMessage, StopReason};
use regex::Regex;
use std::sync::LazyLock;

/// `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN`: account and subscription limits, not transient throttles.
static NON_RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|\
         out of budget|quota exceeded|billing|Claude Code usage limit reached",
    )
    .expect("valid pattern")
});

/// `RETRYABLE_PROVIDER_ERROR_PATTERN`.
static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "(?i)overloaded|currently experiencing high demand|rate.?limit|too many requests|429|500|502|503|504|520|524|\
         service.?unavailable|server.?error|internal.?error|provider.?returned.?error|\
         exceeded request buffer limit while retrying upstream|network.?error|connection.?error|connection.?refused|\
         connection.?lost|other side closed|fetch failed|getaddrinfo|ENOTFOUND|EAI_AGAIN|upstream.?connect|\
         reset before headers|socket hang up|socket connection was closed|timed? out|timeout|terminated|\
         websocket.?closed|websocket.?error|ended without|stream ended before message_stop|\
         stream ended before a terminal response event|http2 request did not get a response|retry delay|\
         you can retry your request|try your request again|please retry your request|ResourceExhausted",
    )
    .expect("valid pattern")
});

/// The classifier of `isRetryableAssistantError` over a failure's text (a provider's message, or a response's
/// status and body).
pub fn is_retryable_text(text: &str) -> bool {
    !text.is_empty() && !NON_RETRYABLE.is_match(text) && RETRYABLE.is_match(text)
}

/// `isRetryableAssistantError`: a turn that failed with a transient provider or transport error.
pub fn is_retryable(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Error && message.error_message.as_deref().is_some_and(is_retryable_text)
}

/// `RetryPolicy` (`settings.retry`): bounded attempts with exponential backoff.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Retries after the first attempt.
    pub max_retries: u64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_delay_ms: 60_000,
        }
    }
}

impl RetryPolicy {
    /// `normalizedRetryPolicy` of settings.json's `retry` object (missing members take their defaults):
    /// `retry.enabled ? retry.maxRetries + 1 : 1` attempts; `enabled` false turns retries off for this session.
    pub fn from_settings(retry: &serde_json::Value, enabled: bool) -> RetryPolicy {
        let defaults = RetryPolicy::default();
        let number = |key: &str, default: u64| retry[key].as_u64().unwrap_or(default);
        let max_retries = if enabled && retry["enabled"] != false {
            number("maxRetries", defaults.max_retries)
        } else {
            0
        };
        RetryPolicy {
            enabled: max_retries > 0,
            max_retries,
            base_delay_ms: number("baseDelayMs", defaults.base_delay_ms),
            max_delay_ms: number("maxAgentDelayMs", defaults.max_delay_ms),
        }
    }

    /// Attempts in all, the first included.
    pub fn max_attempts(&self) -> u64 {
        if self.enabled { self.max_retries + 1 } else { 1 }
    }

    /// The policy as an operation journal records it (`{maxAttempts, baseDelayMs, maxAgentDelayMs}`).
    pub fn journal_value(&self) -> serde_json::Value {
        serde_json::json!({
            "maxAttempts": self.max_attempts(),
            "baseDelayMs": self.base_delay_ms,
            "maxAgentDelayMs": self.max_delay_ms,
        })
    }

    /// `retryDelayMs`: `baseDelayMs * 2^(attempt-1)`, capped.
    pub fn delay_ms(&self, attempt: u64) -> u64 {
        let factor = 1u64.checked_shl(attempt.saturating_sub(1) as u32).unwrap_or(u64::MAX);
        self.base_delay_ms.saturating_mul(factor).min(self.max_delay_ms)
    }
}
