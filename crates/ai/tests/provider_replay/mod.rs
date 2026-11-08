//! The replay harness of the recorded provider fixtures (`fixtures/generate*.mjs`): canned HTTP/SSE responses go
//! through the Rust providers, which must send the same requests (URL, method, headers minus SDK telemetry, JSON body
//! including key order), emit the same events, and return the same final message. `streamSimple` scenarios go through
//! [`ai::dispatch::stream_simple`]; `stream` scenarios through the API's own `stream` with its typed options.

use ai::anthropic::AnthropicOptions;
use ai::azure_openai_responses::AzureOpenAIResponsesOptions;
use ai::bedrock::BedrockOptions;
use ai::google::GoogleOptions;
use ai::google_vertex::GoogleVertexOptions;
use ai::http::{BoxFuture, HeaderMap};
use ai::mistral::MistralOptions;
use ai::openai_codex_responses::OpenAICodexResponsesOptions;
use ai::openai_completions::OpenAICompletionsOptions;
use ai::openai_responses::OpenAIResponsesOptions;
use ai::options::{PayloadHook, pi_user_agent};
use ai::pi_messages::PiMessagesOptions;
use ai::types::EventSink;
use ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, Body, HttpRequest, HttpResponse, Model, SimpleStreamOptions,
    TranscriptContext, Transport,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

pub struct Canned {
    network_error: bool,
    status: u16,
    headers: HeaderMap,
    chunks: Vec<Vec<u8>>,
}

struct FixtureBody(std::vec::IntoIter<Vec<u8>>);

impl Body for FixtureBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// Answers each request with the next canned response (`networkError`: a connection failure); records requests.
#[derive(Clone, Default)]
pub struct FixtureTransport {
    pub queue: Arc<Mutex<Vec<Canned>>>,
    pub requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl FixtureTransport {
    pub fn answering(responses: &Value) -> Self {
        let transport = FixtureTransport::default();
        *transport.queue.lock().unwrap() = responses.as_array().unwrap().iter().map(canned).collect();
        transport
    }
}

impl Transport for FixtureTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let canned = {
            let mut queue = self.queue.lock().unwrap();
            if queue.is_empty() { None } else { Some(queue.remove(0)) }
        };
        Box::pin(async move {
            let canned = canned.ok_or("unexpected request")?;
            if canned.network_error {
                return Err("fetch failed".to_string());
            }
            Ok(HttpResponse {
                status: canned.status,
                headers: canned.headers,
                body: Box::new(FixtureBody(canned.chunks.into_iter())),
            })
        })
    }
}

fn canned(response: &Value) -> Canned {
    let headers = match response.get("headers").and_then(Value::as_object) {
        Some(headers) => headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap().to_string()))
            .collect(),
        None => [("content-type".to_string(), "text/event-stream".to_string())].into(),
    };
    let chunks = response
        .get("chunks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|chunk| match chunk {
            Value::String(s) => s.as_bytes().to_vec(),
            Value::Array(bytes) => bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect(),
            other => panic!("bad chunk {other}"),
        })
        .collect();
    Canned {
        network_error: response.get("networkError") == Some(&json!(true)),
        status: response.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        headers,
        chunks,
    }
}

const SCRATCH_KEYS: &[&str] = &[
    "index",
    "partialJson",
    "partialArgs",
    "customInput",
    "streamIndex",
    "redactedChunks",
];

/// Upstream's streaming scratch fields leak into `partial` snapshots; the Rust partials do not carry them.
fn strip_scratch(event: &mut Value) {
    for key in ["partial", "message", "error"] {
        if let Some(Value::Array(content)) = event.get_mut(key).and_then(|m| m.get_mut("content")) {
            for block in content {
                if let Some(block) = block.as_object_mut() {
                    for scratch in SCRATCH_KEYS {
                        block.shift_remove(*scratch);
                    }
                }
            }
        }
    }
}

/// Wall-clock fields: the generator pins `Date.now()`; Rust reads the clock.
pub fn pin_clock(message: &mut Value, now: &Value) {
    if let Some(object) = message.as_object_mut() {
        object.insert("timestamp".into(), now.clone());
        if let Some(Value::Array(diagnostics)) = object.get_mut("diagnostics") {
            for diagnostic in diagnostics {
                diagnostic["timestamp"] = now.clone();
                if let Some(details) = diagnostic.get_mut("details").and_then(Value::as_object_mut)
                    && details.contains_key("timestampMs")
                {
                    details.insert("timestampMs".into(), now.clone());
                }
            }
        }
    }
}

pub fn pin_event_clock(event: &mut Value, now: &Value) {
    for key in ["partial", "message", "error"] {
        if let Some(message) = event.get_mut(key) {
            pin_clock(message, now);
        }
    }
}

/// `stream` of `model.api` with `options` parsed as that API's options, plus the signal and payload hook.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &Value,
    signal: &AbortSignal,
    on_payload: Option<PayloadHook>,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    macro_rules! call {
        ($module:ident, $ty:ty) => {{
            let mut parsed: $ty = serde_json::from_value(options.clone()).expect("options");
            parsed.base.signal = Some(signal.clone());
            parsed.base.on_payload = on_payload;
            ai::$module::stream(model, context, &parsed, transport, sink).await
        }};
    }
    match model.api.as_str() {
        "anthropic-messages" => call!(anthropic, AnthropicOptions),
        "openai-completions" => call!(openai_completions, OpenAICompletionsOptions),
        "openai-responses" => call!(openai_responses, OpenAIResponsesOptions),
        "azure-openai-responses" => call!(azure_openai_responses, AzureOpenAIResponsesOptions),
        "openai-codex-responses" => call!(openai_codex_responses, OpenAICodexResponsesOptions),
        "google-generative-ai" => call!(google, GoogleOptions),
        "google-vertex" => call!(google_vertex, GoogleVertexOptions),
        "mistral-conversations" => call!(mistral, MistralOptions),
        "bedrock-converse-stream" => call!(bedrock, BedrockOptions),
        "pi-messages" => call!(pi_messages, PiMessagesOptions),
        api => panic!("unknown api {api}"),
    }
}

struct Outcome {
    requests: Vec<Value>,
    events: Vec<Value>,
    result: Value,
    thrown: Value,
}

fn run(scenario: &Value) -> Outcome {
    let simple = scenario["fn"] == "streamSimple";
    let mut model: Model = serde_json::from_value(scenario["model"].clone()).expect("model");
    if scenario["wrapper"] == "cloudflare" && !simple {
        let env: Option<ai::env::ProviderEnv> =
            serde_json::from_value(scenario["options"]["env"].clone()).expect("env");
        model = ai::cloudflare::resolve_cloudflare_model(&model, env.as_ref());
    }
    let context: TranscriptContext = serde_json::from_value(scenario["context"].clone()).expect("context");
    let transport = FixtureTransport::answering(&scenario["responses"]);
    let signal = AbortSignal::new();
    let abort_on = scenario
        .get("abortOn")
        .filter(|a| !a.is_null())
        .map(|a| (a["type"].as_str().unwrap().to_string(), a["nth"].as_u64().unwrap()));
    let on_payload = scenario
        .get("payloadPatch")
        .and_then(Value::as_object)
        .cloned()
        .map(|patch| {
            Arc::new(move |payload: &Value, _: &Model| {
                let mut merged = payload.as_object().cloned().unwrap_or_default();
                merged.extend(patch.clone());
                Some(Value::Object(merged))
            }) as PayloadHook
        });
    let mut events = Vec::new();
    let mut seen = 0;
    let trigger = signal.clone();
    let mut sink = |event: &AssistantMessageEvent| {
        let value = serde_json::to_value(event).unwrap();
        if let Some((kind, nth)) = &abort_on
            && value["type"] == kind.as_str()
        {
            seen += 1;
            if seen == *nth {
                trigger.abort();
            }
        }
        events.push(value);
    };
    let fixtures_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
    let mut options: Value =
        serde_json::from_str(&scenario["options"].to_string().replace("<fixtures>", fixtures_dir)).unwrap();
    // The generator's process env disables instance metadata for the SDK chain; scope the same to these runs.
    // Every Bedrock run gets it, so no replay can reach a real instance metadata endpoint.
    if model.api == "bedrock-converse-stream" {
        if !options["env"].is_object() {
            options["env"] = json!({});
        }
        options["env"]["AWS_EC2_METADATA_DISABLED"] = json!("true");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    let outcome: Result<AssistantMessage, String> = runtime.block_on(async {
        if simple {
            let mut parsed: SimpleStreamOptions = serde_json::from_value(options).expect("options");
            parsed.base.signal = Some(signal.clone());
            parsed.base.on_payload = on_payload;
            ai::dispatch::stream_simple(&model, &context, &parsed, &transport, &mut sink).await
        } else {
            Ok(stream(&model, &context, &options, &signal, on_payload, &transport, &mut sink).await)
        }
    });
    let user_agent = pi_user_agent();
    let requests = transport
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| {
            let unrecorded: &[&str] = if model.api == "bedrock-converse-stream" {
                // The fixture leaves out what carries the SDK version, invocation ids or the wall clock.
                if scenario["recordAuthorization"] == true {
                    &["host", "x-amz-date", "x-amz-content-sha256"]
                } else {
                    &["authorization", "host", "x-amz-date", "x-amz-content-sha256"]
                }
            } else {
                &[]
            };
            let headers: Map<String, Value> = r
                .headers
                .iter()
                .filter(|(k, _)| !unrecorded.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), json!(v.replace(&user_agent, "<pi-user-agent>"))))
                .collect();
            // reqwest sends the WHATWG-normalized URL (`{` becomes `%7B`), as `fetch` does.
            let url = reqwest::Url::parse(&r.url).map_or(r.url.clone(), |u| u.to_string());
            json!({"url": url, "method": r.method, "headers": headers, "body": serde_json::from_str::<Value>(&r.body).unwrap()})
        })
        .collect();
    let (result, thrown) = match outcome {
        Ok(message) => (serde_json::to_value(message).unwrap(), Value::Null),
        Err(message) => (Value::Null, json!(message)),
    };
    Outcome {
        requests,
        events,
        result,
        thrown,
    }
}

/// Generated tool call ids embed `Date.now()` (pinned upstream, the clock here): `name_<13 digits>_n` -> `name_<now>_n`.
fn pin_generated_ids(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_'
            && i + 14 < bytes.len()
            && bytes[i + 1..i + 14].iter().all(u8::is_ascii_digit)
            && bytes[i + 14] == b'_'
        {
            out.push_str("_<now>_");
            i += 15;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn compact(value: &Value) -> String {
    pin_generated_ids(&serde_json::to_string(&jsrt::json::normalize_numbers(value.clone())).unwrap())
}

/// Every difference between the recorded and the Rust run of `scenario`, empty when they match exactly.
fn differences(fixture: &Value, scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let now = fixture["now"].clone();
    let rust = run(scenario);
    let mut diffs = Vec::new();
    // Request parts compare as text (key order included); messages compare as JSON values, because upstream adds
    // optional message fields in whatever order the stream sets them.
    let mut check = |what: &str, expected: &Value, actual: &Value| {
        let ordered = what.starts_with("request") && !(what.ends_with("body") && scenario["unorderedBody"] == true);
        let (e, a) = if ordered {
            (json!(compact(expected)), json!(compact(actual)))
        } else {
            (
                serde_json::from_str(&compact(expected)).unwrap(),
                serde_json::from_str::<Value>(&compact(actual)).unwrap(),
            )
        };
        if e != a {
            diffs.push(format!(
                "{name}: {what}\n  expected {}\n  actual   {}",
                compact(expected),
                compact(actual)
            ));
        }
    };
    let expected_requests = scenario["requests"].as_array().unwrap();
    check(
        "request count",
        &json!(expected_requests.len()),
        &json!(rust.requests.len()),
    );
    for (i, (expected, actual)) in expected_requests.iter().zip(&rust.requests).enumerate() {
        for part in ["url", "method", "headers", "body"] {
            check(&format!("request {i} {part}"), &expected[part], &actual[part]);
        }
    }
    let expected_events = scenario["events"].as_array().unwrap();
    check(
        "event types",
        &json!(expected_events.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()),
        &json!(rust.events.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()),
    );
    for (i, (expected, actual)) in expected_events.iter().zip(&rust.events).enumerate() {
        let mut expected = expected.clone();
        strip_scratch(&mut expected);
        if scenario["unorderedBody"] == true
            && let Some(event) = expected.as_object_mut()
        {
            // pi-messages spreads the wire event into the stream event; the Rust event type carries only its own fields.
            event.shift_remove("contentSignature");
            event.shift_remove("redacted");
            if event.get("type") == Some(&json!("toolcall_start")) {
                event.shift_remove("id");
                event.shift_remove("toolName");
            }
        }
        let mut actual = actual.clone();
        pin_event_clock(&mut actual, &now);
        check(&format!("event {i}"), &expected, &actual);
    }
    let mut result = rust.result;
    if !result.is_null() {
        pin_clock(&mut result, &now);
    }
    check("result", &scenario["result"], &result);
    check("thrown", &scenario["thrown"], &rust.thrown);
    diffs
}

fn assert_no_differences(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Replays every scenario of `fixture`, in recorded order (generated tool call ids count across scenarios).
pub fn replay_all(fixture: &Value) {
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert_no_differences(scenarios.iter().flat_map(|s| differences(fixture, s)).collect());
}

/// Replays the scenarios of `fixture` named `names`.
pub fn replay(fixture: &Value, names: &[&str]) {
    let scenarios = fixture["scenarios"].as_array().unwrap();
    let failures = names
        .iter()
        .flat_map(|name| {
            let scenario = scenarios
                .iter()
                .find(|s| s["name"] == *name)
                .unwrap_or_else(|| panic!("no scenario {name}"));
            differences(fixture, scenario)
        })
        .collect();
    assert_no_differences(failures);
}
