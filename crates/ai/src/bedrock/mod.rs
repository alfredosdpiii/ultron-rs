//! Amazon Bedrock Converse stream provider (`api/bedrock-converse-stream.ts`) without the AWS SDK: the same request
//! (schema member order of `ConverseStreamRequest`, blobs as base64), SigV4 or bearer-token auth, region and endpoint
//! resolution, the SDK's standard retry for throttling and transient failures, event-stream decoding, and the SDK's
//! error names and messages, formatted as upstream formats them.
//!
//! Credentials: explicit env keys, else the SDK default chain (`aws_credentials.rs`: env, shared files, assume role,
//! web identity, `credential_process`, SSO, container and instance metadata).

mod error;
mod payload;
mod stream;
mod target;

use crate::options::StreamOptions;
use crate::options::ThinkingBudgets;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

pub use payload::{is_anthropic_claude_model, supports_adaptive_thinking, supports_prompt_caching};
pub use stream::{stream, stream_simple};
pub use target::{SecondLookup, configured_endpoint_url, second_lookup};

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub region: Option<String>,
    pub profile: Option<String>,
    /// `"auto" | "any" | "none" | { type: "tool", name }`.
    pub tool_choice: Option<Value>,
    pub reasoning: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub interleaved_thinking: Option<bool>,
    /// `"summarized" | "omitted"`.
    pub thinking_display: Option<String>,
    pub request_metadata: Option<IndexMap<String, String>>,
    pub bearer_token: Option<String>,
}
