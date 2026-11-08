//! Provider ids and defaults that settings, the provider layer (`ai`) and the TUI all name.

macro_rules! claude_code {
    () => {
        "claude-code"
    };
}
macro_rules! claude_code_default_model {
    () => {
        "claude-opus-5-5"
    };
}

/// The Claude Code CLI provider (`ai::claude_code`).
pub const CLAUDE_CODE: &str = claude_code!();
/// The Claude subscription provider (`ai::claude_subscription`): the Rust root under `--claude`; TypeScript has none.
pub const CLAUDE_SUBSCRIPTION: &str = "claude-subscription";
/// Kimi For Coding: the footer marks its cost `(sub)` whatever the runtime reports.
pub const KIMI_CODING: &str = "kimi-coding";
/// The Claude Code model of the root, of claude subagents and (as `claude-code/…`) of frames.
pub const CLAUDE_CODE_DEFAULT_MODEL: &str = claude_code_default_model!();
/// [`CLAUDE_CODE_DEFAULT_MODEL`] as a model reference.
pub const CLAUDE_CODE_DEFAULT_REF: &str = concat!(claude_code!(), "/", claude_code_default_model!());

/// Claude Code and the Claude subscription: models with Anthropic's own ids that answer only to their full
/// reference, and responses that carry Claude Code's usage.
pub fn runs_claude_code(provider: &str) -> bool {
    provider == CLAUDE_CODE || provider == CLAUDE_SUBSCRIPTION
}
