//! Claude Code CLI transport (`api/claude-code-cli.ts`): one model call is one headless `claude -p` process speaking
//! stream-json. The CLI is feature-detected (`--version`, `--help`, `auth status --json`) once per binary and config
//! dir; every call runs isolated (no tools, MCP servers, settings sources, slash commands or session persistence, an
//! empty working directory, a replaced system prompt); active calls are capped; after a call, one idle process with
//! the same arguments is started ahead of time (bounded, expiring) for the next call of that shape.
//!
//! A request whose transcript declares tools fails before any process starts (no tool runner is registered here).

mod probe;
mod process;
mod request;
mod stream;

use crate::options::{SimpleStreamOptions, StreamOptions};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

pub use probe::{ClaudeCliCapabilities, empty_work_dir, ensure_claude_cli, parse_help_flags, resolve_claude_cli};
pub use process::{CliProcess, claude_code_cli_stats};
pub use request::render_claude_code_prompt;
pub(crate) use stream::emit_text_or_thinking_end;
pub use stream::{RateLimitInfo, rate_limit_info, reset_text, stream, stream_simple, usage_from_result};

pub const CLAUDE_CODE_BIN_ENV: &str = "ULTRON_CLAUDE_CODE_BIN";
pub const CLAUDE_CODE_CONCURRENCY_ENV: &str = "ULTRON_CLAUDE_CODE_CONCURRENCY";
pub const CLAUDE_CODE_TIMEOUT_ENV: &str = "ULTRON_CLAUDE_CODE_TIMEOUT_MS";
pub const CLAUDE_CODE_WARM_ENV: &str = "ULTRON_CLAUDE_CODE_WARM";
pub const CLAUDE_CODE_WARM_TTL_ENV: &str = "ULTRON_CLAUDE_CODE_WARM_TTL_MS";
pub const CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV: &str = "ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY";
pub const DEFAULT_CONCURRENCY: u64 = 4;
pub const DEFAULT_TIMEOUT_MS: u64 = 10 * 60 * 1000;
pub const DEFAULT_WARM_SPARES: u64 = 2;
pub const DEFAULT_WARM_TTL_MS: u64 = 30_000;
pub const USAGE_LIMIT_DIAGNOSTIC: &str = "provider_usage_limit";
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_FAILURE_TTL: Duration = Duration::from_secs(10);
const EXIT_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);
const STDERR_TAIL_CHARS: usize = 4_000;
const MAX_SYSTEM_PROMPT_ARG_CHARS: usize = 100_000;
const NEUTRAL_SYSTEM_PROMPT: &str = "You are a helpful assistant. Follow the user's instructions exactly.";
/// Why a request with tools fails on `claude -p`. Through `dispatch` (session lanes, frames with depth) such a request
/// runs on the Claude subscription instead (`claude_subscription::claude_code_lane`), so this is the text of an id
/// the subscription does not serve, or of a direct call. TypeScript's text recommended frames; this one does not.
pub const TOOLS_UNSUPPORTED: &str = "does not support tool calling here: `claude -p` returns completions, not raw tool calls, and lanes with tools run Claude Code models on the Claude subscription (claude-subscription), which does not serve this model id. Use a Claude model id the subscription serves (claude-subscription/<id>), or run Claude Code as the root agent with `ultron --claude` or `ultron claude`.";

/// Flags every request relies on; each must appear in `claude --help`.
pub const REQUIRED_FLAGS: &[&str] = &[
    "--print",
    "--output-format",
    "--input-format",
    "--verbose",
    "--include-partial-messages",
    "--tools",
    "--strict-mcp-config",
    "--mcp-config",
    "--setting-sources",
    "--permission-prompts",
    "--disable-slash-commands",
    "--no-session-persistence",
    "--model",
    "--system-prompt",
    "--settings",
];

/// Session variables a parent Claude Code sets for its children; a frame is not one of them.
pub const PARENT_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
];

pub type Env = BTreeMap<String, String>;

/// Options of a Claude Code request: the simple options plus an optional JSON schema for structured output.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeOptions {
    #[serde(flatten)]
    pub base: SimpleStreamOptions,
    pub json_schema: Option<Value>,
}

fn positive_int(value: Option<&String>, fallback: u64, allow_zero: bool) -> u64 {
    let Some(text) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return fallback;
    };
    match text.parse::<f64>() {
        Ok(n) if jsrt::number::is_safe_integer(n) && n >= if allow_zero { 0.0 } else { 1.0 } => n as u64,
        _ => fallback,
    }
}

#[derive(Clone, Copy, Debug)]
struct RuntimeConfig {
    concurrency: u64,
    timeout_ms: u64,
    warm_spares: u64,
    warm_ttl_ms: u64,
}

fn runtime_config(env: &Env, options: &StreamOptions) -> RuntimeConfig {
    RuntimeConfig {
        concurrency: positive_int(env.get(CLAUDE_CODE_CONCURRENCY_ENV), DEFAULT_CONCURRENCY, false),
        timeout_ms: options
            .timeout_ms
            .unwrap_or_else(|| positive_int(env.get(CLAUDE_CODE_TIMEOUT_ENV), DEFAULT_TIMEOUT_MS, false)),
        warm_spares: positive_int(env.get(CLAUDE_CODE_WARM_ENV), DEFAULT_WARM_SPARES, true),
        warm_ttl_ms: positive_int(env.get(CLAUDE_CODE_WARM_TTL_ENV), DEFAULT_WARM_TTL_MS, false),
    }
}

/// `childEnv`: the process env plus the provider env, without the parent Claude Code session variables.
pub fn child_env(options: &StreamOptions) -> Env {
    let mut env: Env = std::env::vars().collect();
    for (name, value) in options.env.iter().flatten() {
        env.insert(name.clone(), value.clone());
    }
    for name in PARENT_SESSION_ENV {
        env.remove(*name);
    }
    env
}

/// Test hook: forget cached CLI paths, probes and warm spares.
pub fn reset_claude_code_cli_state() {
    probe::reset_probe_state();
    process::reset_process_state();
}

#[cfg(test)]
mod tests {
    use super::request::plan_request;
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn parse_help_flags_expands_optional_suffix() {
        let flags = parse_help_flags("via: --system-prompt[-file], --append-system-prompt[-file]");
        assert!(flags.iter().any(|f| f == "--system-prompt"));
        assert!(flags.iter().any(|f| f == "--system-prompt-file"));
        assert!(flags.iter().any(|f| f == "--append-system-prompt"));
        assert!(flags.iter().any(|f| f == "--append-system-prompt-file"));
    }

    #[test]
    fn large_system_prompt_uses_file_when_supported() {
        let model = crate::models::builtin_model("claude-code", "sonnet").expect("builtin sonnet");
        let big = "S".repeat(MAX_SYSTEM_PROMPT_ARG_CHARS + 50);
        let payload = json!({
            "model": "sonnet",
            "system": big,
            "content": [{"type": "text", "text": "hi"}],
        });
        let caps = ClaudeCliCapabilities {
            version: "2.0.0".into(),
            flags: HashSet::from(["--system-prompt".into(), "--system-prompt-file".into()]),
            warnings: Vec::new(),
        };
        let plan = plan_request(model, &payload, &Env::new(), 8192, &caps, false, None).expect("plan");
        let idx = plan
            .args
            .iter()
            .position(|a| a == "--system-prompt-file")
            .expect("flag");
        let path = std::path::PathBuf::from(&plan.args[idx + 1]);
        assert_eq!(std::fs::read_to_string(&path).expect("temp file"), big);
        assert_eq!(plan.message["message"]["content"].as_array().unwrap().len(), 1);
        drop(plan);
        assert!(!path.exists(), "temp system prompt file cleaned up on Drop");
    }
}
