use super::events::{ResponsesRun, ResponsesStream, TierPricing};
use super::params::build_params;
use super::{Compat, OpenAIResponsesOptions};
use crate::constrained::grammar_tool_input_properties;
use crate::http::{Failure, HeaderBuilder, Sdk, Transport, format_provider_error, join_url, send_with_retry};
use crate::openai_completions::client_api_key;
use crate::options::{SimpleStreamOptions, StreamOptions, build_base_options};
use crate::provider::{Run, build_json_post_request, openai_client_headers};
use crate::sse::read_openai_stream;
use crate::transcript::{declared_tools, resolve_transcript};
use crate::types::{AssistantMessage, CacheRetention, EventSink, Model, TranscriptContext};
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Stream one Responses API response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAIResponsesOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let compat = Compat::of(model);
    let context = resolve_transcript(context, compat.supports_mid_convo_system_messages);
    let now = jsrt::date::now_ms();
    let mut run = Run::new(AssistantMessage::pending(model, now), sink, ResponsesStream::default());
    let result = drive(&mut run, model, &context, options, &compat, transport, now).await;
    let prefix = format!(
        "{} API error",
        if model.provider == "openai" {
            "OpenAI"
        } else {
            model.provider.as_str()
        }
    );
    run.end(
        result.map_err(|failure| format_provider_error(&failure, Some(&prefix))),
        options.base.signal(),
    )
}

async fn drive(
    run: &mut ResponsesRun<'_, '_>,
    model: &Model,
    context: &TranscriptContext,
    options: &OpenAIResponsesOptions,
    compat: &Compat,
    transport: &dyn Transport,
    now: i64,
) -> Result<(), Failure> {
    let base = &options.base;
    let api_key = client_api_key(model, base).map_err(Failure::plain)?;
    let session_id = if base.cache_retention() == CacheRetention::None {
        None
    } else {
        base.session_id.as_deref()
    };
    let grammar =
        grammar_tool_input_properties(&declared_tools(&context.messages), compat.supports_openai_grammar_tools)
            .map_err(Failure::plain)?;
    let headers = openai_client_headers(
        model,
        context,
        base,
        &api_key,
        session_id,
        Some(compat.session_affinity_format.as_str()),
        false,
    );
    let params = build_params(model, context, options, compat, &grammar, now).map_err(Failure::plain)?;
    let tier = TierPricing::Response(options.service_tier.as_deref());
    send_and_read_responses_stream(
        run,
        model,
        base,
        transport,
        join_url(&model.base_url, "/responses"),
        headers,
        params,
        &grammar,
        tier,
        "OpenAI Responses stream ended without a stop reason",
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared Responses-family HTTP dispatch and SSE consumption helper"
)]
pub(crate) async fn send_and_read_responses_stream(
    run: &mut ResponsesRun<'_, '_>,
    model: &Model,
    base: &StreamOptions,
    transport: &dyn Transport,
    url: String,
    headers: HeaderBuilder,
    params: Map<String, Value>,
    grammar: &IndexMap<String, String>,
    tier: TierPricing<'_>,
    eof_message: &str,
) -> Result<(), Failure> {
    let signal = base.signal();
    let request = build_json_post_request(url, headers, params, model, base);
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, base.retry(), signal).await?;
    run.start();
    read_openai_stream(&mut response, signal, |event| {
        run.handle(event, model, grammar, tier).map_err(Failure::plain)
    })
    .await?;
    run.terminated().map_err(Failure::plain)?;
    run.stopped(signal, eof_message).map_err(Failure::plain)
}

/// `streamSimple`: reasoning clamped to the model's supported levels and sent as `reasoningEffort`.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    client_api_key(model, &options.base)?;
    let responses = OpenAIResponsesOptions {
        base: build_base_options(model, context, options),
        reasoning_effort: crate::options::simple_reasoning_effort(model, options),
        tool_choice: options.tool_choice.clone().map(Value::String),
        ..Default::default()
    };
    Ok(stream(model, context, &responses, transport, sink).await)
}
