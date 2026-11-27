//! OpenAI Responses provider (`api/openai-responses.ts` with `api/openai-responses-shared.ts`): request construction,
//! response-event processing, usage with service-tier pricing, and stop reasons.

mod events;
mod messages;
mod params;
mod stream;

use crate::options::StreamOptions;
use crate::provider::{compat_flag, is_openrouter};
use crate::transcript::resolve_transcript;
use crate::types::{AssistantMessage, EventSink, Model, TranscriptContext};
use serde::Deserialize;
use serde_json::Value;

pub(crate) use events::{ResponsesRun, ResponsesStream, TierPricing};
pub(crate) use messages::OPENAI_TOOL_CALL_PROVIDERS;
pub use messages::{ConvertOptions, ToolOptions, convert_responses_messages, convert_responses_tools};
pub(crate) use params::{apply_reasoning_params, clamp_prompt_cache_key, reasoning_off_effort};
pub(crate) use stream::send_and_read_responses_stream;
pub use stream::{stream, stream_simple};

/// Resolve the transcript and initialize a pending [`ResponsesRun`] for a Responses-family provider.
pub(crate) fn prepare_responses_run<'s, 'e>(
    model: &Model,
    context: &TranscriptContext,
    api: &str,
    sink: &'s mut EventSink<'e>,
) -> (TranscriptContext, i64, ResponsesRun<'s, 'e>) {
    let context = resolve_transcript(context, compat_flag(model, "supportsMidConvoSystemMessages", false));
    let now = jsrt::date::now_ms();
    let mut output = AssistantMessage::pending(model, now);
    output.api = api.into();
    let run = ResponsesRun::new(output, sink, ResponsesStream::default());
    (context, now, run)
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAIResponsesOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub reasoning_effort: Option<String>,
    /// `"auto" | "detailed" | "concise"`.
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub tool_choice: Option<Value>,
}

struct Compat {
    supports_mid_convo_system_messages: bool,
    session_affinity_format: String,
    supports_long_cache_retention: bool,
    supports_strict_mode: bool,
    supports_openai_grammar_tools: bool,
    supports_additional_tools: bool,
    supports_tool_search: bool,
    supports_explicit_prompt_cache_mode: bool,
    supports_max_output_tokens: bool,
}

impl Compat {
    fn of(model: &Model) -> Self {
        let openrouter = is_openrouter(model);
        let default_affinity = if openrouter { "openrouter" } else { "openai" };
        Compat {
            supports_mid_convo_system_messages: compat_flag(model, "supportsMidConvoSystemMessages", false),
            session_affinity_format: model
                .compat_str("sessionAffinityFormat")
                .unwrap_or(default_affinity)
                .to_string(),
            supports_long_cache_retention: compat_flag(model, "supportsLongCacheRetention", true),
            supports_strict_mode: compat_flag(model, "supportsStrictMode", false),
            supports_openai_grammar_tools: compat_flag(model, "supportsOpenAIGrammarTools", false),
            supports_additional_tools: compat_flag(model, "supportsAdditionalTools", false),
            supports_tool_search: compat_flag(model, "supportsToolSearch", false),
            supports_explicit_prompt_cache_mode: compat_flag(model, "supportsExplicitPromptCacheMode", false),
            supports_max_output_tokens: compat_flag(model, "supportsMaxOutputTokens", true),
        }
    }
}

/// `String(value)` for an optional JSON field (`undefined` when absent).
pub(super) fn js_str(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => jsrt::json::stringify(other),
    }
}

pub(super) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
