//! Fakes for the judge's tests: a scripted transport (what Jev and d1 answer), a scripted chat model, a loopback
//! HTTP server for the real transport, and keys built at run time.
#![allow(dead_code)]

use ai::{AssistantMessage, TranscriptContext};
use blocking_http::{HttpRequest, HttpResponse, Transport, TransportError};
use judge::backend::KeySource;
use judge::chat::ChatModel;
use kernel::Signal;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An obviously fake key, assembled at run time.
pub fn fake_key(name: &str) -> String {
    ["fake", "-", name, "-", "key"].concat()
}

pub fn key(value: Option<&str>) -> KeySource {
    let value = value.map(fake_key);
    Arc::new(move |_: &str| Ok(value.clone()))
}

/// One request as the fake saw it.
#[derive(Clone, Debug)]
pub struct Sent {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub timeout: Duration,
}

/// One scripted answer of the transport.
pub type Outcome = Result<HttpResponse, TransportError>;
/// What the loopback server saw: path, authorization, body.
pub type Seen = Arc<Mutex<Vec<(String, String, Value)>>>;
/// A loopback answer: status, headers, body.
pub type Canned = (u16, Vec<(&'static str, String)>, String);

/// Answers each request with the next scripted outcome, by URL suffix (`/systemone` or `/decide`).
#[derive(Default)]
pub struct FakeTransport {
    pub scripts: Mutex<Vec<(String, VecDeque<Outcome>)>>,
    pub sent: Mutex<Vec<Sent>>,
}

pub fn ok(body: Value) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status: 200,
        retry_after: None,
        body: body.to_string().into_bytes(),
    })
}

pub fn status(status: u16, retry_after: Option<&str>) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        retry_after: retry_after.map(str::to_string),
        body: Vec::new(),
    })
}

impl FakeTransport {
    pub fn script(&self, suffix: &str, outcomes: Vec<Result<HttpResponse, TransportError>>) {
        self.scripts
            .lock()
            .unwrap()
            .push((suffix.to_string(), outcomes.into_iter().collect()));
    }

    pub fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }

    pub fn urls(&self) -> Vec<String> {
        self.sent().into_iter().map(|s| s.url).collect()
    }
}

impl Transport for FakeTransport {
    fn send(
        &self,
        request: HttpRequest,
        _max_bytes: usize,
        timeout: Duration,
        _signal: &Signal,
    ) -> Result<HttpResponse, TransportError> {
        let body = request
            .body
            .as_ref()
            .map(|b| serde_json::from_slice(b).unwrap())
            .unwrap_or(Value::Null);
        self.sent.lock().unwrap().push(Sent {
            url: request.url.clone(),
            headers: request.headers.clone(),
            body,
            timeout,
        });
        let mut scripts = self.scripts.lock().unwrap();
        let script = scripts
            .iter_mut()
            .find(|(suffix, _)| request.url.ends_with(suffix.as_str()))
            .unwrap_or_else(|| panic!("unscripted request to {}", request.url));
        script.1.pop_front().unwrap_or(Err(TransportError::Failed))
    }
}

/// A Jev answer to a single yes/no question `q`.
pub fn jev_yes_no(p: f64, cost: Option<f64>) -> Value {
    let mut usage = json!({"input_tokens": 400, "output_tokens": 42});
    if let Some(cost) = cost {
        usage["cost"] = json!(cost);
    }
    json!({"answers": {"q": {"noul": p}}, "model": "jev-1.13", "usage": usage})
}

/// A d1 answer to a single choice question `q`.
pub fn d1_choice(choice: &str, confidence: f64) -> Value {
    json!({"answers": {"q": {"choice": choice, "confidence": confidence, "probabilities": {choice: confidence}}}, "ms": 194.3})
}

/// A chat model answering with the scripted `answer` arguments (or failing), at a known cost.
#[derive(Default)]
pub struct FakeChat {
    pub replies: Mutex<VecDeque<Result<Value, String>>>,
    pub asked: Mutex<Vec<(String, TranscriptContext)>>,
}

impl FakeChat {
    pub fn answering(replies: Vec<Result<Value, String>>) -> Arc<FakeChat> {
        Arc::new(FakeChat {
            replies: Mutex::new(replies.into_iter().collect()),
            asked: Mutex::default(),
        })
    }

    pub fn models(&self) -> Vec<String> {
        self.asked.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }
}

/// An assistant message calling `answer` with `arguments`, from `model_ref`, costing $0.002.
pub fn answer_message(model_ref: &str, arguments: Value) -> AssistantMessage {
    let (provider, model) = model_ref.split_once('/').unwrap();
    serde_json::from_value(json!({
        "role": "assistant",
        "content": [{"type": "toolCall", "id": "call-1", "name": "answer", "arguments": arguments}],
        "api": "anthropic-messages",
        "provider": provider,
        "model": model,
        "usage": {"input": 300, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 320,
            "cost": {"input": 0.0015, "output": 0.0005, "cacheRead": 0, "cacheWrite": 0, "total": 0.002}},
        "stopReason": "toolUse",
        "timestamp": 0,
    }))
    .unwrap()
}

impl ChatModel for FakeChat {
    fn complete(
        &self,
        model_ref: &str,
        context: &TranscriptContext,
        _signal: &Signal,
    ) -> Result<AssistantMessage, String> {
        self.asked
            .lock()
            .unwrap()
            .push((model_ref.to_string(), context.clone()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("an unscripted chat request")?;
        Ok(answer_message(model_ref, reply))
    }
}

/// A loopback HTTP server answering each connection with the next `(status, headers, body)` after an optional
/// hold; it records each request's path, headers and body. Ends after the last answer.
pub fn serve(answers: Vec<Canned>) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        let mut answers = answers.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let mut length = 0;
            let mut authorization = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if k.eq_ignore_ascii_case("authorization") {
                        authorization = v.trim().to_string();
                    }
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
            log.lock().unwrap().push((
                path,
                authorization,
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            let Some((status, headers, text)) = answers.next() else {
                return;
            };
            let mut head = format!("HTTP/1.1 {status} X\r\n");
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", text.len()));
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(text.as_bytes());
        }
    });
    (base, seen)
}
