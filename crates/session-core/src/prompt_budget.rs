//! The prompt budget check before a request (from Mirror's react/agent.rs `validate_budget` and
//! compaction/compactor.rs `Compactor::new`): the system prompt and the tool schemas are sent with every request and
//! no compaction can shrink them. When they leave the conversation less than `reserveTokens` below the compaction
//! threshold (`contextWindow - reserveTokens`), every compaction would end above the threshold again, so the run
//! fails at once with the sizes instead of compacting in a loop or overflowing at the provider.
//!
//! Sizes use auto-compaction's own estimator ([`compaction::estimate_tokens`] of the system message, its `toolsAdded`
//! included). Small windows (under three times the reserve, where compaction cannot keep a reserve-sized
//! conversation anyway and the TypeScript product still runs) are not refused for that: the fixed part may always
//! take up to `reserveTokens` (or the window minus one, when smaller), so the limit is
//! `max(contextWindow - 2 * reserveTokens, min(contextWindow - 1, reserveTokens))` and grows continuously with the
//! window. Without compaction only the window limits: the fixed part must leave some room in it. An unknown window
//! (0) is not checked.

use compaction::{CompactionSettings, estimate_tokens};
use serde_json::{Value, json};

/// The fixed part of every request, in estimated tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptSize {
    /// The system prompt text (and its sections).
    pub system: u64,
    /// The tool schemas the system message declares.
    pub tools: u64,
}

impl PromptSize {
    /// The sizes of `system`, a system message as the transcript holds it (`{role: "system", content, sections?,
    /// toolsAdded?}`); `tools` is what `toolsAdded` adds to the estimate, so the two sum to the message's.
    pub fn of(system: &Value) -> PromptSize {
        let total = estimate_tokens(system);
        let mut text_only = json!({"role": "system", "content": system.get("content").cloned().unwrap_or(Value::Null)});
        if let Some(sections) = system.get("sections") {
            text_only["sections"] = sections.clone();
        }
        let text = estimate_tokens(&text_only).min(total);
        PromptSize {
            system: text,
            tools: total - text,
        }
    }

    pub fn fixed(&self) -> u64 {
        self.system + self.tools
    }
}

/// Whether a request whose fixed part is `size` fits `context_window` under `settings`; the error names the sizes
/// and what to change. `model` labels the message (`provider/id`).
pub fn check_prompt_budget(
    size: PromptSize,
    context_window: u64,
    settings: &CompactionSettings,
    model: &str,
) -> Result<(), String> {
    if context_window == 0 {
        return Ok(());
    }
    let (fixed, reserve) = (size.fixed(), settings.reserve_tokens);
    let fix = "Shorten the system prompt (AGENTS.md and other context files, --system-prompt or --append-system-prompt), lower compaction.reserveTokens, or switch to a model with a larger context window.";
    let (system, tools) = (size.system, size.tools);
    if settings.enabled {
        let reserved = context_window.saturating_sub(reserve.saturating_mul(2));
        let small = (context_window - 1).min(reserve);
        if fixed <= reserved.max(small) {
            return Ok(());
        }
        if reserved >= small {
            let threshold = context_window - reserve;
            return Err(format!(
                "Prompt budget exceeded for {model}: the system prompt ({system} tokens) and tool schemas ({tools} tokens) take {fixed} tokens, leaving {} below the compaction threshold of {threshold} tokens (context window {context_window} minus compaction.reserveTokens {reserve}); the conversation needs at least {reserve}. {fix}",
                threshold.saturating_sub(fixed),
            ));
        }
        return Err(format!(
            "Prompt budget exceeded for {model}: the system prompt ({system} tokens) and tool schemas ({tools} tokens) take {fixed} tokens; a {context_window}-token context window with compaction.reserveTokens {reserve} leaves room for at most {small}. {fix}"
        ));
    }
    if fixed < context_window {
        return Ok(());
    }
    Err(format!(
        "Prompt budget exceeded for {model}: the system prompt ({system} tokens) and tool schemas ({tools} tokens) take {fixed} tokens of the {context_window}-token context window, leaving no room for the conversation. {fix}"
    ))
}

/// Literal expectations: the sizes come from auto-compaction's estimator (characters / 4, rounded up, the
/// `toolsAdded` JSON included), and the run is refused exactly when the fixed part leaves the conversation less than
/// `reserveTokens` below the threshold, or (no compaction, or a window too small for the reserve) no room in the
/// window.
#[cfg(test)]
mod tests {
    use super::{PromptSize, check_prompt_budget};
    use compaction::CompactionSettings;
    use serde_json::json;

    fn settings(enabled: bool, reserve_tokens: u64) -> CompactionSettings {
        CompactionSettings {
            enabled,
            reserve_tokens,
            keep_recent_tokens: 20000,
        }
    }

    fn size(system: u64, tools: u64) -> PromptSize {
        PromptSize { system, tools }
    }

    const FIX: &str = "Shorten the system prompt (AGENTS.md and other context files, --system-prompt or --append-system-prompt), lower compaction.reserveTokens, or switch to a model with a larger context window.";

    #[test]
    fn session_core_prompt_budget_sizes_use_the_auto_compaction_estimator() {
        let tool = json!({"name": "rlm", "description": "Python REPL", "parameters": {"type": "object"}});
        let system =
            json!({"role": "system", "content": "a".repeat(4001), "toolsAdded": [tool.clone()], "timestamp": 0});
        let measured = PromptSize::of(&system);
        // 4001 characters: 1001 tokens; the whole message is one estimate, so the parts add up to it.
        assert_eq!(measured.system, 1001);
        let tools_chars = json!([tool]).to_string().len();
        assert_eq!(measured.fixed(), (4001 + tools_chars).div_ceil(4) as u64);
        assert_eq!(measured.tools, measured.fixed() - 1001);
        // Sections count as system text; a message without tools has none.
        let sectioned = json!({"role": "system", "content": "abcd", "sections": {"guide": "efgh"}, "timestamp": 0});
        assert_eq!(PromptSize::of(&sectioned), size(2, 0));
    }

    #[test]
    fn session_core_prompt_budget_leaves_the_reserve_below_the_threshold() {
        let on = settings(true, 16384);
        // Window 200000: threshold 183616; the conversation needs 16384 of it, so at most 167232 fixed.
        let fits = size(167000, 232);
        assert_eq!(check_prompt_budget(fits, 200000, &on, "anthropic/m"), Ok(()));
        let over = size(167001, 232);
        assert_eq!(
            check_prompt_budget(over, 200000, &on, "anthropic/m"),
            Err(format!(
                "Prompt budget exceeded for anthropic/m: the system prompt (167001 tokens) and tool schemas (232 tokens) take 167233 tokens, leaving 16383 below the compaction threshold of 183616 tokens (context window 200000 minus compaction.reserveTokens 16384); the conversation needs at least 16384. {FIX}"
            ))
        );
        // Past the threshold itself: nothing left.
        let huge = size(190000, 0);
        assert!(
            check_prompt_budget(huge, 200000, &on, "p/m")
                .unwrap_err()
                .contains("take 190000 tokens, leaving 0 below")
        );
    }

    #[test]
    fn session_core_prompt_budget_without_compaction_only_the_window_limits() {
        let off = settings(false, 16384);
        assert_eq!(check_prompt_budget(size(199999, 0), 200000, &off, "p/m"), Ok(()));
        assert_eq!(
            check_prompt_budget(size(199000, 1000), 200000, &off, "p/m"),
            Err(format!(
                "Prompt budget exceeded for p/m: the system prompt (199000 tokens) and tool schemas (1000 tokens) take 200000 tokens of the 200000-token context window, leaving no room for the conversation. {FIX}"
            ))
        );
        // Small windows (under three times the reserve): the fixed part may take up to the reserve.
        let on = settings(true, 16384);
        assert_eq!(check_prompt_budget(size(16384, 0), 20000, &on, "p/m"), Ok(()));
        assert_eq!(
            check_prompt_budget(size(16000, 385), 20000, &on, "p/m"),
            Err(format!(
                "Prompt budget exceeded for p/m: the system prompt (16000 tokens) and tool schemas (385 tokens) take 16385 tokens; a 20000-token context window with compaction.reserveTokens 16384 leaves room for at most 16384. {FIX}"
            ))
        );
        // At three times the reserve both limits meet (16384); above it the threshold rule grows with the window.
        assert_eq!(check_prompt_budget(size(16384, 0), 49152, &on, "p/m"), Ok(()));
        assert!(check_prompt_budget(size(16385, 0), 49152, &on, "p/m").is_err());
        assert_eq!(check_prompt_budget(size(16385, 0), 49153, &on, "p/m"), Ok(()));
        assert!(check_prompt_budget(size(16386, 0), 49153, &on, "p/m").is_err());
        // A window no larger than the reserve: the window minus one.
        assert_eq!(check_prompt_budget(size(8191, 0), 8192, &on, "p/m"), Ok(()));
        assert!(check_prompt_budget(size(8192, 0), 8192, &on, "p/m").is_err());
        // An unknown window is not checked.
        assert_eq!(check_prompt_budget(size(u64::MAX / 2, 0), 0, &on, "p/m"), Ok(()));
    }
}
