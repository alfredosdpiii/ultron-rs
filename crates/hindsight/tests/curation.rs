//! The Rust-only Hindsight 1.1 calls of `HindsightBackend` (memory units, reflect, mental models): no TypeScript
//! reference, so literal requests over a recording transport.

use hindsight::{HindsightBackend, HindsightOptions, HttpRequest, HttpResponse, Transport, TransportError};
use kernel::Signal;
use kernel_apis::MemoryBackend;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Answers each request with the next of `answers` (status, body); keeps `(method, url, body)`.
struct Recorder {
    answers: Mutex<Vec<(u16, Value)>>,
    sent: Mutex<Vec<(String, String, Value)>>,
}

impl Transport for Recorder {
    fn send(&self, request: HttpRequest, _: usize, _: Duration, _: &Signal) -> Result<HttpResponse, TransportError> {
        let body = request
            .body
            .map(|body| serde_json::from_slice(&body).unwrap())
            .unwrap_or(Value::Null);
        self.sent.lock().unwrap().push((request.method, request.url, body));
        let (status, body) = self.answers.lock().unwrap().remove(0);
        Ok(HttpResponse {
            status,
            body: if (200..300).contains(&status) {
                body.to_string().into_bytes()
            } else {
                Vec::new()
            },
        })
    }
}

fn backend(answers: Vec<(u16, Value)>) -> (HindsightBackend, Arc<Recorder>) {
    let recorder = Arc::new(Recorder {
        answers: Mutex::new(answers),
        sent: Mutex::new(Vec::new()),
    });
    let tags = json!({"project": ["ultron:project:p1"]}).as_object().unwrap().clone();
    let mut options = HindsightOptions::new("http://hs.test", "ultron", tags);
    options.transport = Some(recorder.clone());
    (HindsightBackend::new(options).unwrap(), recorder)
}

#[test]
fn hindsight_backend_units_reflect_and_mental_models_use_the_1_1_endpoints() {
    let model = json!({"id": "ultron-project-p1", "content": "brief"});
    let (backend, recorder) = backend(vec![
        (200, json!({"id": "u/1", "tags": []})),
        (200, json!({"id": "u/1", "state": "invalidated"})),
        (200, json!({"text": "answer"})),
        (404, Value::Null),
        (200, model.clone()),
        (500, Value::Null),
        (200, json!({"operation_id": "op1"})),
    ]);
    let signal = Signal::new();
    let invalidate = json!({"state": "invalidated", "reason": "moved"});
    assert_eq!(backend.unit("u/1", &signal), Ok(json!({"id": "u/1", "tags": []})));
    assert!(backend.curate("u/1", &invalidate, &signal).is_ok());
    assert_eq!(
        backend.reflect(&json!({"query": "q"}), &signal),
        Ok(json!({"text": "answer"}))
    );
    assert_eq!(
        backend.mental_model("ultron-project-p1", &signal),
        Ok(Value::Null),
        "a missing model is no error"
    );
    assert_eq!(backend.mental_model("ultron-project-p1", &signal), Ok(model));
    assert_eq!(
        backend.mental_model("ultron-project-p1", &signal),
        Err("HTTP_ERROR".into()),
        "any other failure is"
    );
    assert!(
        backend
            .create_mental_model(&json!({"id": "ultron-project-p1"}), &signal)
            .is_ok()
    );
    let bank = "http://hs.test/v1/default/banks/ultron";
    let sent: Vec<(String, String, Value)> = recorder.sent.lock().unwrap().clone();
    let expected = [
        ("GET", format!("{bank}/memories/u%2F1"), Value::Null),
        ("PATCH", format!("{bank}/memories/u%2F1"), invalidate),
        ("POST", format!("{bank}/reflect"), json!({"query": "q"})),
        ("GET", format!("{bank}/mental-models/ultron-project-p1"), Value::Null),
        ("GET", format!("{bank}/mental-models/ultron-project-p1"), Value::Null),
        ("GET", format!("{bank}/mental-models/ultron-project-p1"), Value::Null),
        (
            "POST",
            format!("{bank}/mental-models"),
            json!({"id": "ultron-project-p1"}),
        ),
    ];
    assert_eq!(sent.len(), expected.len());
    for ((method, url, body), (want_method, want_url, want_body)) in sent.iter().zip(expected) {
        assert_eq!(
            (method.as_str(), url.as_str(), body),
            (want_method, want_url.as_str(), &want_body)
        );
    }
}
