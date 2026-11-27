//! OpenAI Chat Completions provider (`api/openai-completions.ts`), with upstream's URL/provider-based compat detection.
//! HTTP goes through [`Transport`]; headers and URL follow the pinned `openai` SDK (6.40.0) minus `x-stainless-*`.

mod compat;
mod headers;
mod messages;
mod params;
mod reasoning;
mod stream;

use crate::http::{Transport, has_header};
use crate::options::{StreamOptions, ThinkingBudgets};
use crate::types::Model;
use serde::Deserialize;
use serde_json::Value;

pub use stream::{stream, stream_simple};

/// Anchor reference for module docs (`[`Transport`]`).
const _TRANSPORT_DOC: Option<&dyn Transport> = None;

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompletionsOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    /// `ChatCompletionToolChoiceOption`: a string or an object.
    pub tool_choice: Option<Value>,
    /// `"minimal" | "low" | "medium" | "high" | "xhigh" | "max"`.
    pub reasoning_effort: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// The OpenAI SDK client's `apiKey`: the caller's, else a placeholder when an authorization header carries the
/// credential; no key and no header fails as the providers throw.
pub(crate) fn client_api_key(model: &Model, options: &StreamOptions) -> Result<String, String> {
    if let Some(key) = options.api_key.as_ref().filter(|key| !key.is_empty()) {
        return Ok(key.clone());
    }
    let headers = options.headers.as_ref();
    if has_header(headers, "authorization") || has_header(headers, "cf-aig-authorization") {
        return Ok("unused".into());
    }
    Err(format!("No API key for provider: {}", model.provider))
}
