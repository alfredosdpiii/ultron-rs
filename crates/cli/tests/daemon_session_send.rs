//! `ultron session send` through the product: a session created on a running server is prompted from outside
//! (the server service `ultron.session-inbox`); the idle session runs the message at once as the user's prompt.

use crate::daemon_common as common;

use client::runtime::Connection;
use common::{HANG_GUARD, Home, SERVER_ID, assert_no_processes, provider, text_reply};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

#[test]
fn cli_daemon_session_send_prompts_an_idle_session() {
    let provider = provider(
        vec![text_reply("Hello."), text_reply("Got your reply.")],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    let server = common::start_server(&home, SERVER_ID);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    let created = connection
        .call(
            connection.server_target(),
            protocol::names::SESSION_MANAGEMENT,
            "create",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    connection
        .call(
            connection.server_target(),
            protocol::names::SESSION_MANAGEMENT,
            "attach",
            json!([id]),
        )
        .unwrap();
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
        .unwrap();
    let session = connection.session_target().unwrap();
    connection
        .call(
            session,
            protocol::names::AGENT_CONTROLLER,
            "prompt",
            json!([{"message": "hi", "images": null}]),
        )
        .unwrap();
    connection.disconnect();

    let output = home
        .command(&["session", "send", &id, "# Re: page\nPicked B"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("Sent to session {id}")
    );
    let deadline = Instant::now() + HANG_GUARD;
    while provider.requests.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "the sent message did not run");
        std::thread::sleep(Duration::from_millis(20));
    }
    let second = provider.requests.lock().unwrap()[1].clone();
    assert_eq!(user_texts(&second).last().unwrap(), "# Re: page\nPicked B");

    let output = home
        .command(&["session", "send", "no-such-session", "x"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No running Ultron server holds session no-such-session"));
    let output = home.command(&["session", "send"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}
