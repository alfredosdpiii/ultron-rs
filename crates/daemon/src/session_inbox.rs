//! The server service `ultron.session-inbox` (unlisted, Rust-only; `ultron session send`): `send(sessionId, message)`
//! hands `message` to the session as the user's prompt, through its worker (started for it when none runs). An idle
//! session runs it at once; a session in a run queues it as a follow-up.

use crate::chord::{Service, ServiceError};
use crate::routed::Hub;
use serde_json::{Value, json};
use std::sync::Weak;

pub fn prompt_call(message: &str) -> Value {
    json!({"serviceId": protocol::names::SESSION_OPS, "member": "op",
        "args": [{"op": "prompt", "message": message, "queue": "followUp"}]})
}

pub fn server_service(hub: Weak<Hub>) -> Service {
    Service::new(protocol::names::SESSION_INBOX)
        .unlisted()
        .method("send", move |args| {
            let hub = hub
                .upgrade()
                .ok_or_else(|| ServiceError::internal("Server is closed"))?;
            let text = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or_default().to_string();
            let (session, message) = (text(0), text(1));
            if session.is_empty() || message.trim().is_empty() {
                return Err(ServiceError::coded(
                    "service_invalid_value",
                    "send needs a session id and a message",
                ));
            }
            hub.session_call(&session, prompt_call(&message))
        })
}
