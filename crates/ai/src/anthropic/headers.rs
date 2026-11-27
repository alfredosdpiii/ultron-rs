//! Request headers and `anthropic-beta` feature selection for Anthropic Messages requests.

use super::{AnthropicOptions, Compat, fallback_models, has_vision_input, is_oauth_token};
use crate::http::HeaderBuilder;
use crate::options::{StreamOptions, pi_user_agent};
use crate::transcript::current_tools;
use crate::types::{Message, Model, TranscriptContext};

const CLAUDE_CODE_VERSION: &str = "2.1.280";
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const MID_CONVERSATION_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";

fn copilot_dynamic_headers(messages: &[Message]) -> Vec<(String, String)> {
    let initiator = match messages.last() {
        Some(last) if !matches!(last, Message::User(_)) => "agent",
        _ => "user",
    };
    let mut headers = vec![
        ("X-Initiator".to_string(), initiator.to_string()),
        ("Openai-Intent".into(), "conversation-edits".into()),
    ];
    if has_vision_input(messages) {
        headers.push(("Copilot-Vision-Request".into(), "true".into()));
    }
    headers
}

pub(crate) fn copilot_headers(messages: &[Message]) -> Vec<(String, String)> {
    copilot_dynamic_headers(messages)
}

fn set_auth_headers(headers: &mut HeaderBuilder, model: &Model, api_key: Option<&str>, oauth: bool) {
    let Some(key) = api_key else {
        return;
    };
    if model.provider == "github-copilot" || oauth {
        headers.set("Authorization", format!("Bearer {key}"));
    } else {
        headers.set("X-Api-Key", key);
    }
}

fn set_provider_headers(
    headers: &mut HeaderBuilder,
    model: &Model,
    context: &TranscriptContext,
    session_id: Option<&str>,
    oauth: bool,
) {
    if model.provider == "github-copilot" {
        headers.extend_set(model.headers.iter().flatten());
        for (name, value) in copilot_dynamic_headers(&context.messages) {
            headers.set(&name, value);
        }
        return;
    }
    if oauth {
        headers
            .set("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
            .set("x-app", "cli");
        headers.extend_set(model.headers.iter().flatten());
        return;
    }
    let compat = Compat::of(model);
    if let (Some(session_id), true) = (session_id, compat.session_affinity_headers) {
        let header_name = if compat.openrouter_affinity {
            "x-session-id"
        } else {
            "x-session-affinity"
        };
        headers.set(header_name, session_id);
    }
    headers.extend_set(model.headers.iter().flatten());
}

/// Client default headers and whether the key is a Claude Code OAuth token.
pub(super) fn client_headers(
    model: &Model,
    context: &TranscriptContext,
    options: &StreamOptions,
    session_id: Option<&str>,
) -> (HeaderBuilder, bool) {
    let api_key = options.api_key.as_deref().filter(|k| !k.is_empty());
    let mut headers = HeaderBuilder::default();
    headers
        .set("Accept", "application/json")
        .set("anthropic-dangerous-direct-browser-access", "true")
        .set("anthropic-version", "2023-06-01");
    let oauth = model.provider != "github-copilot" && api_key.is_some_and(is_oauth_token);
    set_auth_headers(&mut headers, model, api_key, oauth);
    headers
        .set("User-Agent", pi_user_agent())
        .set("accept", "application/json")
        .set("anthropic-dangerous-direct-browser-access", "true");
    set_provider_headers(&mut headers, model, context, session_id, oauth);
    headers.extend(options.headers.iter().flatten());
    headers.set("content-type", "application/json");
    (headers, oauth)
}

fn configured_beta_header(model: &Model, options: &AnthropicOptions) -> Option<Option<String>> {
    let mut configured: Option<Option<String>> = None;
    for (name, value) in model.headers.iter().flatten() {
        if name.eq_ignore_ascii_case("anthropic-beta") {
            configured = Some(Some(value.clone()));
        }
    }
    for (name, value) in options.base.headers.iter().flatten() {
        if name.eq_ignore_ascii_case("anthropic-beta") {
            configured = Some(value.clone());
        }
    }
    configured
}

fn interleaved_thinking_enabled(model: &Model, compat: &Compat, options: &AnthropicOptions) -> bool {
    model.reasoning
        && options.thinking_enabled == Some(true)
        && options.interleaved_thinking.unwrap_or(true)
        && !compat.force_adaptive
}

fn default_beta_features(
    model: &Model,
    context: &TranscriptContext,
    oauth: bool,
    native_tool_changes: bool,
    options: &AnthropicOptions,
) -> Vec<String> {
    let mut features = Vec::new();
    let compat = Compat::of(model);
    if oauth {
        features.extend(["claude-code-20250219".to_string(), "oauth-2025-04-20".to_string()]);
    }
    if !current_tools(&context.messages).is_empty() && !compat.eager_tool_input_streaming {
        features.push(FINE_GRAINED_TOOL_STREAMING_BETA.into());
    }
    if interleaved_thinking_enabled(model, &compat, options) {
        features.push(INTERLEAVED_THINKING_BETA.into());
    }
    if !fallback_models(model).is_empty() {
        features.push(SERVER_SIDE_FALLBACK_BETA.into());
    }
    if compat.managed_effort {
        features.extend([
            MID_CONVERSATION_OUTPUT_CONFIG_BETA.to_string(),
            THINKING_BINDING_CONTROLS_BETA.to_string(),
        ]);
    }
    if native_tool_changes {
        features.push(MID_CONVERSATION_TOOL_CHANGES_BETA.into());
    }
    features
}

fn parse_beta_list(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect()
}

fn dedupe_features(features: Vec<String>) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for feature in features {
        if !unique.contains(&feature) {
            unique.push(feature);
        }
    }
    unique
}

pub(super) fn beta_features(
    model: &Model,
    context: &TranscriptContext,
    oauth: bool,
    native_tool_changes: bool,
    options: &AnthropicOptions,
) -> Vec<String> {
    let features = match configured_beta_header(model, options) {
        Some(None) => return Vec::new(),
        Some(Some(list)) => parse_beta_list(&list),
        None => default_beta_features(model, context, oauth, native_tool_changes, options),
    };
    dedupe_features(features)
}
