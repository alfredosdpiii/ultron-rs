//! Server-to-client requests over an in-memory transport (client.ts `handleRequest`).

use mcp::{CancelToken, Client, ClientOptions, Error, Incoming, Sink, Transport};
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Answers `initialize` and passes everything else the client sends to `received`.
struct Fake {
    sink: Mutex<Option<Sink>>,
    sent: Mutex<Sender<Value>>,
    received: Mutex<Receiver<Value>>,
}

impl Fake {
    fn new() -> Self {
        let (sent, received) = channel();
        Fake {
            sink: Mutex::default(),
            sent: Mutex::new(sent),
            received: Mutex::new(received),
        }
    }
}

impl Transport for Fake {
    fn start(&self, sink: Sink) -> Result<(), Error> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(())
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        if message["method"] == "initialize" {
            let result = json!({ "protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": { "name": "s", "version": "1" } });
            let sink = self.sink.lock().unwrap().clone().unwrap();
            let id = message["id"].clone();
            std::thread::spawn(move || {
                sink(Incoming::Message(
                    json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                ))
            });
        } else {
            let _ = self.sent.lock().unwrap().send(message.clone());
        }
        Ok(())
    }

    fn close(&self) {}
}

fn ask(fake: &Fake, request: Value, expected_id: &Value) -> Value {
    let sink = fake.sink.lock().unwrap().clone().unwrap();
    sink(Incoming::Message(request));
    let deadline = Instant::now() + Duration::from_secs(120);
    let received = fake.received.lock().unwrap();
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        let message = received
            .recv_timeout(wait)
            .unwrap_or_else(|_| panic!("no reply to {expected_id}"));
        if &message["id"] == expected_id {
            return message;
        }
    }
}

#[test]
fn mcp_client_answers_server_requests_with_handler_results_and_errors() {
    let fake = Arc::new(Fake::new());
    let client = Client::new(ClientOptions {
        name: "c".into(),
        version: "1".into(),
        ..ClientOptions::default()
    });
    client.connect(fake.clone()).unwrap();
    client.set_request_handler("x/null", Arc::new(|_, _: &CancelToken| Ok(Value::Null)));
    client.set_request_handler(
        "x/echo",
        Arc::new(|p: Option<&Value>, _: &CancelToken| Ok(json!({ "got": p }))),
    );
    client.set_request_handler(
        "x/mcp",
        Arc::new(|_, _: &CancelToken| {
            Err(Error::Mcp {
                code: 7,
                message: "nope".into(),
                data: Some(json!([1])),
            })
        }),
    );
    client.set_request_handler(
        "x/other",
        Arc::new(|_, _: &CancelToken| Err(Error::Other("boom".into()))),
    );
    let cases = [
        ("x/null", json!(1), json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        (
            "x/echo",
            json!("e"),
            json!({ "jsonrpc": "2.0", "id": "e", "result": { "got": null } }),
        ),
        (
            "x/mcp",
            json!(2),
            json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": 7, "message": "nope", "data": [1] } }),
        ),
        (
            "x/other",
            json!(3),
            json!({ "jsonrpc": "2.0", "id": 3, "error": { "code": -32603, "message": "boom" } }),
        ),
        (
            "roots/list",
            json!(4),
            json!({ "jsonrpc": "2.0", "id": 4, "error": { "code": -32601, "message": "Method not found: roots/list" } }),
        ),
    ];
    for (method, id, expected) in cases {
        let reply = ask(&fake, json!({ "jsonrpc": "2.0", "id": id, "method": method }), &id);
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }
    client.close();
}
