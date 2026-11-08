//! Replays `fixtures/client.json`, recorded by driving the TypeScript `Client` over an in-memory transport:
//! the Rust `Client` must send the same frames and settle each step with the same result or error.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::rc::Rc;

use crate::support::gateway::{hex, unhex};
use client::{Client, ClientError};
use protocol::json::{to_cbor, to_json};
use protocol::{RpcTarget, SessionTarget};
use serde_json::{Value as Json, json};

/// Scripted server side: queued reads (`None` = orderly close) and the frames the client wrote.
#[derive(Default)]
struct Wire {
    replies: VecDeque<Option<Vec<u8>>>,
    sent: Vec<String>,
}

#[derive(Clone, Default)]
struct Stream(Rc<RefCell<Wire>>);

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.borrow_mut().replies.pop_front() {
            Some(Some(chunk)) => {
                buf[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
            Some(None) => Ok(0),
            None => panic!("client read past the scripted replies"),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().sent.push(hex(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn target(json: &Json) -> RpcTarget {
    let field = |k: &str| json[k].as_str().expect("target field").to_owned();
    match json.get("sessionId") {
        None => RpcTarget::Server {
            server_id: field("serverId"),
        },
        Some(_) => RpcTarget::Session(SessionTarget {
            server_id: field("serverId"),
            session_id: field("sessionId"),
            attachment_id: field("attachmentId"),
        }),
    }
}

/// The TS error a Rust error corresponds to: `{name, message, code}`.
fn error_json(error: &ClientError) -> Json {
    let (name, message, code) = match error {
        ClientError::Server(e) => ("ServerError", e.message.clone(), json!(e.code)),
        ClientError::Validation(m) => ("ProtocolValidationError", m.clone(), Json::Null),
        ClientError::Disconnected(m) => ("DisconnectedError", m.clone(), Json::Null),
        ClientError::InvalidCall => ("TypeError", "Invalid service call".into(), Json::Null),
        ClientError::Io(e) => ("Error", e.to_string(), Json::Null),
    };
    json!({ "error": { "name": name, "message": message, "code": code } })
}

fn attachment_json(attachment: Option<&SessionTarget>) -> Json {
    attachment.map_or(
        Json::Null,
        |a| json!({ "serverId": a.server_id, "sessionId": a.session_id, "attachmentId": a.attachment_id }),
    )
}

#[test]
fn client_scripts_match_typescript() {
    let fixture: Json = serde_json::from_str(include_str!("../fixtures/client.json")).expect("fixture");
    let server_id = fixture["serverId"].as_str().expect("server id");
    let scripts = fixture["scripts"].as_array().expect("scripts");
    assert!(scripts.len() >= 14);
    for script in scripts {
        let stream = Stream::default();
        let mut client: Option<Client<Stream>> = None;
        let steps = script["steps"].as_array().expect("steps");
        for (step, result) in steps.iter().zip(script["results"].as_array().expect("results")) {
            {
                let mut wire = stream.0.borrow_mut();
                wire.sent.clear();
                for reply in step["replies"].as_array().expect("replies") {
                    let reply = reply.as_str().expect("reply");
                    wire.replies.push_back((reply != "close").then(|| unhex(reply)));
                }
            }
            let settled = if step["action"] == "connect" {
                match Client::connect(stream.clone(), server_id) {
                    Ok(connected) => {
                        client = Some(connected);
                        json!({ "ok": { "type": "hello", "version": 8, "serverId": server_id } })
                    }
                    Err(error) => error_json(&error),
                }
            } else {
                let client = client.as_mut().expect("connected in an earlier step");
                match client.request(target(&step["target"]), &to_cbor(&step["call"])) {
                    Ok(value) => json!({ "ok": value.as_ref().map_or(Json::Null, to_json) }),
                    Err(error) => error_json(&error),
                }
            };
            let name = &script["name"];
            assert_eq!(settled, result["settled"], "{name}");
            assert_eq!(Json::from(stream.0.borrow().sent.clone()), result["sent"], "{name}");
            let attachment = attachment_json(client.as_ref().and_then(Client::attachment));
            assert_eq!(attachment, result["attachment"], "{name}");
            assert!(stream.0.borrow().replies.is_empty(), "{name}: unread replies");
        }
    }
}
