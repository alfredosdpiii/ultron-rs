//! Replays `fixtures/images.json` (recorded from the TypeScript images API by `fixtures/generate-images.mjs`) and
//! ports the catalog checks. The fixture's `via: "collection"` cases drive TS `builtinImagesModels()`, a runtime the
//! product does not use (kernel-apis resolves the key and calls [`generate_images`]), so they are skipped.

use ai::http::{AbortSignal, Body, BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use ai::images::{
    ImagesContext, ImagesModel, ImagesOptions, generate_images, image_model, image_models, image_providers,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const NOW: i64 = 1_767_225_600_000;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Default)]
struct Mock {
    queue: Mutex<Vec<(u16, HeaderMap, String)>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let next = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let (status, headers, body) = next.ok_or("unexpected request")?;
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(OnceBody(Some(body.into_bytes()))),
            })
        })
    }
}

fn options_from(case: &Value) -> ImagesOptions {
    let o = &case["options"];
    let signal = AbortSignal::new();
    if case["abort"] == true {
        signal.abort();
    }
    let patch = case["payloadPatch"].as_object().cloned();
    ImagesOptions {
        api_key: o["apiKey"].as_str().map(str::to_owned),
        headers: o["headers"].as_object().map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned)))
                .collect()
        }),
        max_retries: o["maxRetries"].as_u64().map(|n| n as u32),
        signal: Some(signal),
        on_payload: patch.map(|patch| {
            Arc::new(move |params: &Value, _: &ImagesModel| {
                let mut next = params.as_object().cloned().unwrap_or_default();
                next.extend(patch.clone());
                Some(Value::Object(next))
            }) as ai::images::ImagesPayloadHook
        }),
        now_ms: Some(NOW),
        ..Default::default()
    }
}

fn request_json(request: &HttpRequest) -> Value {
    json!({
        "url": request.url,
        "method": request.method,
        "headers": request.headers,
        "body": serde_json::from_str::<Value>(&request.body).unwrap(),
    })
}

#[tokio::test]
async fn ai_images_generate_matches_typescript() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../fixtures/images.json")).unwrap();
    assert_eq!(cases.len(), 20);
    let api_cases: Vec<Value> = cases.into_iter().filter(|case| case["via"] != "collection").collect();
    assert_eq!(api_cases.len(), 16);
    for case in api_cases {
        let name = case["name"].as_str().unwrap();
        let model: ImagesModel = serde_json::from_value(case["model"].clone()).unwrap();
        let context: ImagesContext = serde_json::from_value(case["context"].clone()).unwrap();
        let mock = Mock::default();
        *mock.queue.lock().unwrap() = case["responses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let body = match &r["body"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let mut headers: HeaderMap = r["headers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect();
                headers.insert("content-type".into(), "application/json".into());
                (r["status"].as_u64().unwrap_or(200) as u16, headers, body)
            })
            .collect();
        let options = options_from(&case);
        let (result, thrown) = match generate_images(&model, &context, &options, &mock).await {
            Ok(result) => (Some(result), None),
            Err(e) => (None, Some(e)),
        };
        let result = result.map_or(Value::Null, |r| {
            jsrt::json::normalize_numbers(serde_json::to_value(r).unwrap())
        });
        assert_eq!(result, case["result"], "{name}: result");
        assert_eq!(
            thrown.map_or(Value::Null, Value::from),
            case["thrown"],
            "{name}: thrown"
        );
        let requests: Vec<Value> = mock.requests.lock().unwrap().iter().map(request_json).collect();
        assert_eq!(Value::Array(requests), case["requests"], "{name}: requests");
    }
}

#[test]
fn ai_images_builtin_catalog_lists_openrouter_image_models() {
    assert_eq!(image_providers().collect::<Vec<_>>(), ["openrouter"]);
    let models: Vec<_> = image_models("openrouter").collect();
    assert_eq!(models.len(), 55);
    assert!(
        models
            .iter()
            .all(|m| m.api == "openrouter-images" && m.provider == "openrouter")
    );
    assert!(models.iter().all(|m| m.output.iter().any(|o| o == "image")));
    let flux = image_model("openrouter", "black-forest-labs/flux.2-pro").unwrap();
    assert_eq!(flux.name, "Black Forest Labs: FLUX.2 Pro");
    assert_eq!(flux.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(flux.input, ["text", "image"]);
    assert!(image_model("openrouter", "missing").is_none());
    assert_eq!(image_models("nope").count(), 0);
}

#[test]
fn ai_images_catalog_round_trips_every_model() {
    let raw: Value = serde_json::from_str(include_str!("../data/image-models.json")).unwrap();
    for (provider, models) in raw.as_object().unwrap() {
        for (id, model) in models.as_object().unwrap() {
            let parsed = image_model(provider, id).unwrap();
            assert_eq!(
                &jsrt::json::normalize_numbers(serde_json::to_value(parsed).unwrap()),
                model,
                "{id}"
            );
        }
    }
}
