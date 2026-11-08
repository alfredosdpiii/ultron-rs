//! Typed reads of the settings keys the session, the worker, the host and the TUI apply (settings-manager.ts
//! `getShellPath`, `getShellCommandPrefix`, `getThinkingBudgets`, `getWebSocketConnectTimeoutMs`,
//! `getTerminalCapabilityOverrides`, `getCodeBlockIndent`, `getWarnings`, `getRlmModelSettings`, `getReviewModel`,
//! `getWorktreeSettings`, `getHttpIdleTimeoutMs`, `getProviderRetrySettings`, `getShowHardwareCursor`,
//! `getClearOnShrink`, session-worker.ts `providerRequestOptions`), over a settings object so a composed session
//! (which keeps its settings as JSON) reads them as a [`crate::settings::SettingsManager`] does; plus the value sets
//! of the enumerated settings, which `/settings` offers and validates.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::paths::expand_tilde;
use crate::settings::{Settings, model_ref, token_cap};

/// `steeringMode` and `followUpMode`: deliver every queued message at once, or one per turn (the default).
pub const QUEUE_ALL: &str = "all";
pub const QUEUE_ONE_AT_A_TIME: &str = "one-at-a-time";
pub const QUEUE_MODES: [&str; 2] = [QUEUE_ALL, QUEUE_ONE_AT_A_TIME];
/// `transport`: the preferred transport where a provider has several.
pub const TRANSPORTS: [&str; 4] = ["sse", "websocket", "websocket-cached", "auto"];
pub const CACHE_WARMING_MODES: [&str; 3] = ["off", "streaming", "idle"];
pub const TREE_FILTER_MODES: [&str; 5] = ["default", "no-tools", "user-only", "labeled-only", "all"];
pub const DOUBLE_ESCAPE_ACTIONS: [&str; 3] = ["fork", "tree", "none"];
/// `markdown.mermaid`.
pub const MERMAID_MODES: [&str; 3] = ["off", "final", "streaming"];
pub const TUI_MODES: [&str; 2] = ["regular", "fullscreen"];
pub const FULLSCREEN_EXIT_OUTPUTS: [&str; 2] = ["transcript", "resume-hint"];
pub const FULLSCREEN_SCROLLBARS: [&str; 3] = ["hidden", "auto", "always"];
/// `defaultProjectTrust`.
pub const PROJECT_TRUST_DEFAULTS: [&str; 3] = ["ask", "always", "never"];

/// `parseHttpIdleTimeoutMs` (core/http-dispatcher.ts), which `parseTimeoutSetting` uses for every timeout setting:
/// a non-negative number of milliseconds (floored), a numeric string (`Number(text)`), or `"disabled"` (0). `None`
/// for anything else.
pub fn parse_timeout_ms(value: &Value) -> Option<f64> {
    let n = match value {
        Value::String(text) => {
            let trimmed = jsrt::string::trim(text);
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0.0);
            }
            if trimmed.is_empty() {
                return None;
            }
            jsrt::number::parse(trimmed)
        }
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (n.is_finite() && n >= 0.0).then(|| n.floor())
}

/// `getTerminalCapabilityOverrides`: what the settings force over the detected terminal capabilities. `images` is
/// `Some(None)` for `false` (no inline images), `Some(Some(protocol))` for `"kitty"`/`"iterm2"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub images: Option<Option<&'static str>>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
}

/// `shellPath` with `~` expanded (`normalizePath`); an empty string stays empty (it selects the default shell).
pub fn shell_path(settings: &Settings, home: &Path) -> Option<String> {
    match settings.get("shellPath")?.as_str()? {
        "" => Some(String::new()),
        path => Some(expand_tilde(path, home).to_string_lossy().into_owned()),
    }
}

/// `shellCommandPrefix`: prepended (with a newline) to a user `!` command.
pub fn shell_command_prefix(settings: &Settings) -> Option<String> {
    settings.get("shellCommandPrefix")?.as_str().map(str::to_string)
}

/// `thinkingBudgets` as configured.
pub fn thinking_budgets(settings: &Settings) -> Option<&Value> {
    settings.get("thinkingBudgets").filter(|v| !v.is_null())
}

/// `websocketConnectTimeoutMs` (`parseTimeoutSetting`): a non-negative number (floored), a numeric string, or
/// `"disabled"` (0); absent is `None`; anything else fails as TypeScript throws.
pub fn websocket_connect_timeout_ms(settings: &Settings) -> Result<Option<f64>, String> {
    let Some(value) = settings.get("websocketConnectTimeoutMs") else {
        return Ok(None);
    };
    parse_timeout_ms(value).map(Some).ok_or_else(|| {
        format!(
            "Invalid websocketConnectTimeoutMs setting: {}",
            jsrt::value::string(Some(value))
        )
    })
}

/// `getTerminalCapabilityOverrides`.
pub fn terminal_capability_overrides(settings: &Settings) -> CapabilityOverrides {
    let terminal = settings.get("terminal");
    let field = |key: &str| terminal.and_then(|t| t.get(key));
    CapabilityOverrides {
        images: match field("images") {
            Some(Value::String(s)) if s == "kitty" => Some(Some("kitty")),
            Some(Value::String(s)) if s == "iterm2" => Some(Some("iterm2")),
            Some(Value::Bool(false)) => Some(None),
            _ => None,
        },
        true_color: field("trueColor").and_then(Value::as_bool),
        hyperlinks: field("hyperlinks").and_then(Value::as_bool),
    }
}

/// `markdown.codeBlockIndent`, default two spaces.
pub fn code_block_indent(settings: &Settings) -> String {
    settings
        .get("markdown")
        .and_then(|m| m.get("codeBlockIndent"))
        .and_then(Value::as_str)
        .unwrap_or("  ")
        .to_string()
}

/// `warnings.anthropicExtraUsage`: only an explicit `false` turns the subscription-auth warning off.
pub fn anthropic_extra_usage_warning(settings: &Settings) -> bool {
    settings.get("warnings").and_then(|w| w.get("anthropicExtraUsage")) != Some(&Value::Bool(false))
}

pub const CLEAR_ON_SHRINK_ENV: &str = "PI_CLEAR_ON_SHRINK";
pub const HARDWARE_CURSOR_ENV: &str = "PI_HARDWARE_CURSOR";

/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS` (core/http-dispatcher.ts): five minutes.
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;
/// What a disabled (0) idle timeout becomes as a request timeout (`2147483647`, the largest Node timer).
pub const DISABLED_REQUEST_TIMEOUT_MS: u64 = 2_147_483_647;

/// `getRlmModelSettings`: the models of RLM frames and subagents (global only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RlmModelSettings {
    pub frame_model: Option<String>,
    /// A `claude-code/*` child model runs its tool lanes on the Claude subscription (ai::dispatch).
    pub child_model: Option<String>,
    pub frame_thinking: Option<String>,
    /// `rlm.mapTokens`: the token cap of a top-level `rlm.map` that sets none (Rust only); unset or `"off"` is none.
    pub map_tokens: Option<u64>,
    /// `rlm.raceModels`: the `provider/model` refs subagent races run on, one arm each (Rust only); invalid
    /// entries are dropped.
    pub race_models: Vec<String>,
}

/// `getWorktreeSettings`: the default setup of subagent worktrees (`worktrees.link`, `.copy`, `.setupCommand`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeSettings {
    pub link: Option<Vec<String>>,
    pub copy: Option<Vec<String>>,
    /// The trimmed `setupCommand`, when not empty.
    pub command: Option<String>,
}

/// `getProviderRetrySettings`: `retry.provider.{timeoutMs, maxRetries, maxRetryDelayMs}` as configured (any JSON
/// value; the cap defaults to 60 s).
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRetrySettings {
    pub timeout_ms: Option<Value>,
    pub max_retries: Option<Value>,
    pub max_retry_delay_ms: Value,
}

fn paths(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `isMergeableObject`.
fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object()
}

/// `getWorktreeSettings` over merged settings.
pub fn worktree_settings(settings: &Settings) -> WorktreeSettings {
    let Some(configured) = object(settings.get("worktrees")) else {
        return WorktreeSettings::default();
    };
    let command = configured
        .get("setupCommand")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    WorktreeSettings {
        link: paths(configured.get("link")),
        copy: paths(configured.get("copy")),
        command,
    }
}

/// `getHttpIdleTimeoutMs`: the setting, else five minutes; an invalid value fails as `parseTimeoutSetting` throws.
pub fn http_idle_timeout_ms(settings: &Settings) -> Result<u64, String> {
    match settings.get("httpIdleTimeoutMs") {
        None => Ok(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
        Some(value) => parse_timeout_ms(value).map(|ms| ms as u64).ok_or_else(|| {
            let shown = jsrt::value::string(Some(value));
            format!("Invalid httpIdleTimeoutMs setting: {shown}")
        }),
    }
}

/// `getProviderRetrySettings` over merged settings.
pub fn provider_retry_settings(settings: &Settings) -> ProviderRetrySettings {
    let provider = settings.get("retry").and_then(|r| r.get("provider"));
    // `?.` reads: a `null` value is kept (only `??` skips it).
    let field = |key: &str| provider.and_then(|p| p.get(key)).cloned();
    ProviderRetrySettings {
        timeout_ms: field("timeoutMs"),
        max_retries: field("maxRetries"),
        max_retry_delay_ms: field("maxRetryDelayMs")
            .filter(|v| !v.is_null())
            .unwrap_or(json!(60000)),
    }
}

/// `providerRequestOptions` (session-worker.ts): the stream options of every model request, as the harness keeps
/// them (`{timeoutMs, maxRetries?, maxRetryDelayMs}`): the request timeout is `retry.provider.timeoutMs`, else the
/// HTTP idle timeout (disabled: 2147483647). An invalid idle timeout fails as the TypeScript getter throws.
pub fn provider_request_options(settings: &Settings) -> Result<Value, String> {
    let provider = provider_retry_settings(settings);
    let timeout = match provider.timeout_ms.filter(|v| !v.is_null()) {
        Some(timeout) => timeout,
        None => match http_idle_timeout_ms(settings)? {
            0 => json!(DISABLED_REQUEST_TIMEOUT_MS),
            ms => json!(ms),
        },
    };
    let mut options = Map::new();
    options.insert("timeoutMs".into(), timeout);
    if let Some(retries) = provider.max_retries {
        options.insert("maxRetries".into(), retries);
    }
    options.insert("maxRetryDelayMs".into(), provider.max_retry_delay_ms);
    Ok(Value::Object(options))
}

/// `getShowHardwareCursor`: the setting, else `PI_HARDWARE_CURSOR=1`.
pub fn show_hardware_cursor(settings: &Settings, var: &dyn Fn(&str) -> Option<String>) -> bool {
    match settings.get("showHardwareCursor").and_then(Value::as_bool) {
        Some(on) => on,
        None => var(HARDWARE_CURSOR_ENV).as_deref() == Some("1"),
    }
}

/// `getClearOnShrink`: `terminal.clearOnShrink`, else `PI_CLEAR_ON_SHRINK=1`.
pub fn clear_on_shrink(settings: &Settings, var: &dyn Fn(&str) -> Option<String>) -> bool {
    match settings
        .get("terminal")
        .and_then(|t| t.get("clearOnShrink"))
        .and_then(Value::as_bool)
    {
        Some(on) => on,
        None => var(CLEAR_ON_SHRINK_ENV).as_deref() == Some("1"),
    }
}

/// `getRlmModelSettings` over the global settings (a repository cannot pick its models).
pub fn rlm_model_settings(global: &Settings) -> RlmModelSettings {
    let Some(configured) = object(global.get("rlm")) else {
        return RlmModelSettings::default();
    };
    let frame_thinking = configured
        .get("frameThinking")
        .and_then(Value::as_str)
        .filter(|level| model_scope::THINKING_LEVELS.contains(level))
        .map(str::to_string);
    RlmModelSettings {
        frame_model: model_ref(configured.get("frameModel")),
        child_model: model_ref(configured.get("childModel")),
        frame_thinking,
        map_tokens: token_cap(configured.get("mapTokens")),
        race_models: configured
            .get("raceModels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| model_ref(Some(m)))
            .collect(),
    }
}

/// `getReviewModel`: `review.model` of the global settings.
pub fn review_model(global: &Settings) -> Option<String> {
    model_ref(object(global.get("review"))?.get("model"))
}
