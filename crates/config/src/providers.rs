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
pub const CLAUDE_CODE_DEFAULT_REF: &str = concat!(claude_code!(), "/", claude_code_default_model!());

/// The Claude Code CLI provider's API (`claude-code-cli`).
pub const CLAUDE_CODE_API: &str = "claude-code-cli";

/// `isClaudeCodeModel`, plus the Claude subscription's models (TypeScript has no `claude-subscription`, so a bare
/// Claude id there matches the `anthropic` model alone): models with Anthropic's own ids that answer only to their
/// full `<provider>/<id>` reference.
pub fn full_reference_only(provider: &str, api: &str) -> bool {
    api == CLAUDE_CODE_API || provider == CLAUDE_SUBSCRIPTION
}

/// Claude Code and the Claude subscription: responses that carry Claude Code's usage.
pub fn runs_claude_code(provider: &str) -> bool {
    provider == CLAUDE_CODE || provider == CLAUDE_SUBSCRIPTION
}
