//! A server that exits as the handshake ends: the client stays closed instead of reading as connected.

use mcp::{Client, ClientOptions, ConnState, Error, Incoming, Sink, Transport};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Answers `initialize`, then reports the connection closed while the `initialized` notification is sent, before
/// the client's handshake returns (a stdio server exiting right after it: its reader reports `Closed`).
#[derive(Default)]
struct ExitsAfterInitialized {
    sink: Mutex<Option<Sink>>,
}

impl Transport for ExitsAfterInitialized {
    fn start(&self, sink: Sink) -> Result<(), Error> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(())
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        let sink = self.sink.lock().unwrap().clone().unwrap();
        if message["method"] == "initialize" {
            let result = json!({ "protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": { "name": "s", "version": "1" } });
            let id = message["id"].clone();
            std::thread::spawn(move || {
                sink(Incoming::Message(
                    json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                ))
            });
        } else if message["method"] == "notifications/initialized" {
            sink(Incoming::Closed);
        }
        Ok(())
    }

    fn close(&self) {}
}

#[test]
fn mcp_client_closed_during_the_handshake_is_not_connected() {
    let client = Client::new(ClientOptions {
        name: "c".into(),
        version: "1".into(),
        ..ClientOptions::default()
    });
    let result = client.connect(Arc::new(ExitsAfterInitialized::default()));
    assert!(result.is_err(), "{result:?}");
    assert_eq!(client.connection_state(), ConnState::Closed);
}
