//! Ported from packages/coding-agent/test/experimental-lost-server.test.ts. A server error is lost by its code (the
//! worker went away); a transport failure and a status line, which have no code, by their text.

use client::lost::{describe, friendly_status, message};
use client::runtime::CallError;
use protocol::ProtocolError;

fn server(code: &str, text: &str) -> CallError {
    CallError::Server(ProtocolError::new(code, text))
}

#[test]
fn client_lost_bare_binding_error_becomes_a_clear_message() {
    assert_eq!(
        describe(&CallError::Disconnected(
            "Remote service ultron.agent-controller binding is closed".into()
        ))
        .as_deref(),
        Some(
            "The Ultron server stopped (Remote service ultron.agent-controller binding is closed); your session is saved — run `ultron -c` to resume"
        )
    );
    assert_eq!(
        friendly_status("Error: Remote service ultron.agent-controller binding is closed"),
        "Error: The Ultron server stopped (Remote service ultron.agent-controller binding is closed); your session is saved — run `ultron -c` to resume"
    );
}

#[test]
fn client_lost_recognizes_transport_and_worker_loss_only() {
    assert!(describe(&CallError::Disconnected("Byte transport closed".into())).is_some());
    // A local failure on the connection's side is not a lost server.
    let offline = CallError::Disconnected("Radius relay connections are unavailable in offline mode".into());
    assert!(describe(&offline).is_none());
    let worker = server(
        protocol::names::WORKER_LOST_CODE,
        "Session worker s-1 disconnected unexpectedly during an operation",
    );
    assert!(describe(&worker).is_some());
    assert!(
        describe(&server(
            protocol::names::RECONNECTING_CODE,
            "Session worker is reconnecting"
        ))
        .is_some()
    );
    // For a server error the code decides, not the text.
    assert!(
        describe(&server(
            "internal_error",
            "Session worker 0199 exited unexpectedly (signal 9)"
        ))
        .is_none()
    );
    assert_eq!(
        message(server("model_not_found", "Model not found: test/none")),
        "Model not found: test/none"
    );
    for status in [
        "Byte transport closed",
        "Session worker s-1 disconnected unexpectedly",
        "Session worker 0199 exited unexpectedly (signal 9)",
        "Ultron server lost its coordinator during a worker operation",
        "write EPIPE",
    ] {
        assert!(
            friendly_status(&format!("Error: {status}")).contains("your session is saved"),
            "{status}"
        );
    }
    assert_eq!(friendly_status("Error: write EPIPEX"), "Error: write EPIPEX");
    assert_eq!(
        friendly_status("Error: Model not found: test/none"),
        "Error: Model not found: test/none"
    );
}

#[test]
fn client_lost_clear_message_is_not_wrapped_twice() {
    let once = message(CallError::Disconnected("Unix connection is closed".into()));
    assert_eq!(message(CallError::Disconnected(once.clone())), once);
    assert_eq!(friendly_status(&format!("Error: {once}")), format!("Error: {once}"));
}
