//! Anthropic Messages provider (`api/anthropic-messages.ts`): request construction, the SSE event loop, usage and stop
//! reasons, retries and cancellation. HTTP goes through [`Transport`]; headers and URL follow the pinned
//! `@anthropic-ai/sdk` (0.124.0) minus its `x-stainless-*` telemetry headers.

mod headers;
mod messages;
mod params;
mod stream;

use crate::http::{Transport, has_header};
use crate::options::StreamOptions;
use crate::provider::{compat_flag, is_openrouter};
use crate::types::{Message, Model, UserContent, UserMessageContent};
use serde::Deserialize;
use serde_json::Value;

pub(crate) use headers::copilot_headers;
pub use stream::{stream, stream_simple};

/// Anchor reference for module docs (`[`Transport`]`).
const _TRANSPORT_DOC: Option<&dyn Transport> = None;

/// The error message of a refusal stop without one of its own.
pub const REFUSAL_TEXT: &str = "The model refused to complete the request";

/// Whether an Anthropic key is a Claude subscription OAuth token (`sk-ant-oat…`), not an API key.
pub fn is_oauth_token(key: &str) -> bool {
    key.contains("sk-ant-oat")
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget_tokens: Option<u64>,
    /// `"low" | "medium" | "high" | "xhigh" | "max"`.
    pub effort: Option<String>,
    /// `"summarized" | "omitted"`.
    pub thinking_display: Option<String>,
    pub interleaved_thinking: Option<bool>,
    /// `"auto" | "any" | "none"` or `{type: "tool", name}`.
    pub tool_choice: Option<Value>,
}

struct Compat {
    eager_tool_input_streaming: bool,
    long_cache_retention: bool,
    session_affinity_headers: bool,
    openrouter_affinity: bool,
    cache_control_on_tools: bool,
    temperature: bool,
    allow_empty_signature: bool,
    strict_tools: bool,
    mid_convo_system_messages: bool,
    mid_convo_tool_changes: bool,
    managed_effort: bool,
    force_adaptive: bool,
}

impl Compat {
    fn of(model: &Model) -> Self {
        let openrouter = is_openrouter(model);
        Compat {
            eager_tool_input_streaming: compat_flag(model, "supportsEagerToolInputStreaming", true),
            long_cache_retention: compat_flag(model, "supportsLongCacheRetention", true),
            session_affinity_headers: compat_flag(model, "sendSessionAffinityHeaders", openrouter),
            openrouter_affinity: match model.compat_str("sessionAffinityFormat") {
                Some(format) => format == "openrouter",
                None => openrouter,
            },
            cache_control_on_tools: compat_flag(model, "supportsCacheControlOnTools", true),
            temperature: compat_flag(model, "supportsTemperature", true),
            allow_empty_signature: compat_flag(model, "allowEmptySignature", false),
            strict_tools: compat_flag(model, "supportsStrictTools", false),
            mid_convo_system_messages: compat_flag(model, "supportsMidConvoSystemMessages", false),
            mid_convo_tool_changes: compat_flag(model, "supportsMidConvoToolChanges", false),
            managed_effort: model.compat_bool("supportsMidConvoEffort") == Some(true),
            force_adaptive: model.compat_bool("forceAdaptiveThinking") == Some(true),
        }
    }
}

pub(super) fn fallback_models(model: &Model) -> Vec<Value> {
    model
        .compat_value("allowedFallbackModels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

pub(super) fn assert_request_auth(model: &Model, options: &StreamOptions) -> Result<(), String> {
    let headers = options.headers.as_ref();
    if options.api_key.as_ref().is_some_and(|k| !k.is_empty())
        || has_header(headers, "authorization")
        || has_header(headers, "x-api-key")
        || has_header(headers, "cf-aig-authorization")
    {
        return Ok(());
    }
    Err(format!("No API key for provider: {}", model.provider))
}

pub(crate) fn has_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::User(u) => {
            matches!(&u.content, UserMessageContent::Blocks(b) if b.iter().any(|c| matches!(c, UserContent::Image(_))))
        }
        Message::ToolResult(r) => r.content.iter().any(|c| matches!(c, UserContent::Image(_))),
        _ => false,
    })
}
