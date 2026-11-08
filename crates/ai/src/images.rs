//! Image generation: the built-in image model catalog (`image-models.ts`, shipped as `data/image-models.json` by
//! `data/generate-image-models.mjs`), API dispatch (`images.ts`) and the OpenRouter images API
//! (`api/openrouter-images.ts`).
//!
//! Evidence: `fixtures/generate-images.mjs` drives the TypeScript `generateImages` through a stubbed `fetch`;
//! `tests/images.rs` replays the recorded responses here and compares requests and results.

use crate::env::ProviderEnv;
use crate::http::{
    AbortSignal, Failure, HeaderBuilder, HttpRequest, Sdk, Transport, format_provider_error, is_aborted, join_url,
    send_with_retry,
};
use crate::options::ProviderHeaders;
use crate::types::{Cost, ImageContent, ModelCost, TextContent, Usage};
use indexmap::IndexMap;
use jsrt::json_parse::parse_json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock};

/// `ImagesModel`: a model without the chat-only fields, plus its output modalities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesModel {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub cost: ModelCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
    /// Model fields the images layer does not read (`inputLimits`, `promptCache`, ...), kept for round trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Text or image: image-generation input and output content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ImagesContent {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ImagesContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

/// `AssistantImages`: the result of one generation; failures are carried in `stop_reason`/`error_message`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<ImagesContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
}

impl AssistantImages {
    fn empty(model: &ImagesModel, now: i64) -> Self {
        AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Stop,
            error_message: None,
            timestamp: now,
        }
    }
}

/// Inspect or replace the images payload before it is sent; `None` keeps it.
pub type ImagesPayloadHook = Arc<dyn Fn(&Value, &ImagesModel) -> Option<Value> + Send + Sync>;

#[derive(Clone, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub env: Option<ProviderEnv>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<f64>,
    /// `timeoutMs`: how long each attempt may wait for the response headers (the SDK's request `timeout`).
    pub timeout_ms: Option<u64>,
    pub metadata: Option<Map<String, Value>>,
    pub signal: Option<AbortSignal>,
    pub on_payload: Option<ImagesPayloadHook>,
    /// `Date.now()` for the result timestamp; `None` reads the clock.
    pub now_ms: Option<i64>,
}

impl ImagesOptions {
    fn now(&self) -> i64 {
        self.now_ms.unwrap_or_else(jsrt::date::now_ms)
    }
}

type Catalog = IndexMap<String, IndexMap<String, ImagesModel>>;

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../data/image-models.json")).expect("built-in image model catalog parses")
});

/// `getImageProviders`: built-in image providers in catalog order.
pub fn image_providers() -> impl Iterator<Item = &'static str> {
    CATALOG.keys().map(String::as_str)
}

/// `getImageModels`: built-in image models of `provider`; empty for an unknown provider.
pub fn image_models(provider: &str) -> impl Iterator<Item = &'static ImagesModel> {
    CATALOG.get(provider).into_iter().flat_map(IndexMap::values)
}

pub fn image_model(provider: &str, id: &str) -> Option<&'static ImagesModel> {
    CATALOG.get(provider)?.get(id)
}

/// `generateImages` of `images.ts`: dispatch by `model.api`. `Err` for an api without an implementation.
pub async fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    transport: &dyn Transport,
) -> Result<AssistantImages, String> {
    match model.api.as_str() {
        "openrouter-images" => Ok(openrouter_generate_images(model, context, options, transport).await),
        api => Err(format!("No API provider registered for api: {api}")),
    }
}

/// The pinned OpenAI SDK's `User-Agent`; upstream sends it unchanged for images requests.
pub const OPENAI_SDK_USER_AGENT: &str = "OpenAI/JS 6.40.0";

/// `buildParams`: one user message with the input parts; `modalities` include text when the model outputs text.
pub fn openrouter_images_params(model: &ImagesModel, context: &ImagesContext) -> Value {
    let content: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            // sanitizeSurrogates is a no-op: a Rust string cannot hold a lone surrogate.
            ImagesContent::Text(text) => json!({"type": "text", "text": text.text}),
            ImagesContent::Image(image) => json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", image.mime_type, image.data)},
            }),
        })
        .collect();
    let modalities = if model.output.iter().any(|o| o == "text") {
        json!(["image", "text"])
    } else {
        json!(["image"])
    };
    json!({
        "model": model.id,
        "messages": [{"role": "user", "content": content}],
        "stream": false,
        "modalities": modalities,
    })
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).filter(|n| *n != 0.0).unwrap_or(0.0)
}

/// `parseUsage`: cache writes are reported inside `cached_tokens`; input excludes both cache kinds.
fn openrouter_usage(raw: &Value, model: &ImagesModel) -> Usage {
    let prompt = number(raw.get("prompt_tokens"));
    let reported_cached = number(raw.pointer("/prompt_tokens_details/cached_tokens"));
    let cache_write = number(raw.pointer("/prompt_tokens_details/cache_write_tokens"));
    let cache_read = if cache_write > 0.0 {
        (reported_cached - cache_write).max(0.0)
    } else {
        reported_cached
    };
    let input = (prompt - cache_read - cache_write).max(0.0);
    let output = number(raw.get("completion_tokens"));
    let mut cost = Cost {
        input: (model.cost.input / 1_000_000.0) * input,
        output: (model.cost.output / 1_000_000.0) * output,
        cache_read: (model.cost.cache_read / 1_000_000.0) * cache_read,
        cache_write: (model.cost.cache_write / 1_000_000.0) * cache_write,
        total: 0.0,
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    Usage {
        input: input as u64,
        output: output as u64,
        cache_read: cache_read as u64,
        cache_write: cache_write as u64,
        total_tokens: (input + output + cache_read + cache_write) as u64,
        cost,
        ..Usage::default()
    }
}

/// `data:<mime>;base64,<data>` (`/^data:([^;]+);base64,(.+)$/`).
fn parse_data_url(url: &str) -> Option<ImageContent> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    if mime.is_empty() || mime.contains(';') || data.is_empty() || data.contains(['\n', '\r']) {
        return None;
    }
    Some(ImageContent {
        data: data.to_string(),
        mime_type: mime.to_string(),
    })
}

fn openrouter_output(response: &Value, out: &mut AssistantImages, model: &ImagesModel) {
    out.response_id = response.get("id").and_then(Value::as_str).map(str::to_owned);
    if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
        out.usage = Some(openrouter_usage(usage, model));
    }
    let Some(message) = response.pointer("/choices/0/message") else {
        return;
    };
    if let Some(text) = message.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        out.output.push(ImagesContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        }));
    }
    for image in message.get("images").and_then(Value::as_array).into_iter().flatten() {
        let url = match image.get("image_url") {
            Some(Value::String(url)) => Some(url.as_str()),
            Some(other) => other.get("url").and_then(Value::as_str),
            None => None,
        };
        if let Some(image) = url.and_then(parse_data_url) {
            out.output.push(ImagesContent::Image(image));
        }
    }
}

async fn openrouter_request(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    transport: &dyn Transport,
    out: &mut AssistantImages,
) -> Result<(), Failure> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| Failure::plain(format!("No API key for provider: {}", model.provider)))?;
    // `{ ...model.headers, ...options.headers }` (case-sensitive spread), nulls dropped, then the SDK's defaults.
    let mut custom: IndexMap<String, Option<String>> = model
        .headers
        .iter()
        .flatten()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect();
    custom.extend(options.headers.iter().flatten().map(|(k, v)| (k.clone(), v.clone())));
    let mut headers = HeaderBuilder::default();
    headers
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .set("User-Agent", OPENAI_SDK_USER_AGENT)
        .openai_sdk_identity()
        .set("Authorization", format!("Bearer {api_key}"));
    for (name, value) in &custom {
        if let Some(value) = value {
            headers.set(name, value.clone());
        }
    }
    let mut params = openrouter_images_params(model, context);
    if let Some(next) = options.on_payload.as_ref().and_then(|hook| hook(&params, model)) {
        params = next;
    }
    let request = HttpRequest {
        method: "POST".into(),
        url: join_url(&model.base_url, "/chat/completions"),
        headers: headers.build(),
        body: serde_json::to_string(&params).unwrap_or_default(),
    };
    let retry = crate::http::RetryOptions {
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        ..Default::default()
    };
    let signal = options.signal.as_ref();
    let timed;
    let transport = match options.timeout_ms {
        Some(ms) => {
            timed = crate::http::TimeoutTransport {
                inner: transport,
                timeout: std::time::Duration::from_millis(ms),
            };
            &timed as &dyn Transport
        }
        None => transport,
    };
    let mut response = send_with_retry(transport, &request, Sdk::OpenAi, retry, signal).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await.map_err(Failure::plain)? {
        bytes.extend(chunk);
    }
    let body = parse_json(&String::from_utf8_lossy(&bytes)).map_err(|e| Failure::plain(e.to_string()))?;
    openrouter_output(&body, out, model);
    Ok(())
}

/// `generateImages` of `api/openrouter-images.ts`. Never fails: errors are returned in the result.
pub async fn openrouter_generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    transport: &dyn Transport,
) -> AssistantImages {
    let mut out = AssistantImages::empty(model, options.now());
    if let Err(failure) = openrouter_request(model, context, options, transport, &mut out).await {
        out.stop_reason = if is_aborted(options.signal.as_ref()) {
            ImagesStopReason::Aborted
        } else {
            ImagesStopReason::Error
        };
        out.error_message = Some(format_provider_error(&failure, None));
    }
    out
}
