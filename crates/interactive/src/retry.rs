//! Retry of the last failed turn (omp `app.retry`, F5): whether the transcript tail is retryable (oh-my-pi
//! turn-recovery.ts `hasAbortedToolCallTail`). The retry itself is a session request the client sends.

use serde_json::Value;

/// The tail is a failed or aborted assistant turn, possibly followed by the placeholder tool results an aborted
/// tool call leaves (or by an errored tool result when Esc landed during execution). A settled successful
/// assistant message leaves nothing to retry.
pub fn failed_turn_tail(messages: &[Value]) -> bool {
    let mut i = messages.len();
    let mut errored_result = false;
    while i > 0 && messages[i - 1]["role"] == "toolResult" {
        errored_result |= messages[i - 1]["isError"] == true;
        i -= 1;
    }
    let Some(last) = i.checked_sub(1).map(|i| &messages[i]) else {
        return false;
    };
    if last["role"] != "assistant" {
        return false;
    }
    match last["stopReason"].as_str() {
        Some("error") | Some("aborted") => true,
        // A completed tool call whose result failed because the user aborted it mid-run.
        Some("toolUse") => errored_result && i < messages.len(),
        _ => false,
    }
}
