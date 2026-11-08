//! The slots in the editor's place that owe another party an answer (a `/mcp` dialog, a client login): whatever
//! replaces one settles it first, and a login's steps and answers stay with that login. Run status does not outlive
//! the session it belonged to.

use std::rc::Rc;

use crate::support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneCommand, LaneEvent, LoginStep, SessionOpened};
use serde_json::{Value, json};

fn record(command: &LaneCommand) -> Option<String> {
    Some(match command {
        LaneCommand::ExtensionUiRespond { id, answer } => format!("respond {id} {answer}"),
        LaneCommand::LoginReply { id, answer } => format!("login_reply {id} {answer:?}"),
        LaneCommand::Abort => "abort".into(),
        LaneCommand::AbortRetry => "abort_retry".into(),
        _ => return None,
    })
}

fn opened() -> LaneEvent {
    LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    })
}

fn app() -> (App, Rc<Sent>) {
    let (lane, sent) = Sent::new(record);
    let mut app = App::new(AppOptions { ..support::options() }, lane);
    app.apply(opened());
    (app, sent)
}

fn screen(app: &mut App) -> String {
    app.render(100, 30)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn dialog(id: &str, method: &str, title: &str) -> LaneEvent {
    let mut request: Value = json!({"type": "extension_ui_request", "id": id, "method": method, "title": title});
    if method == "select" {
        request["options"] = json!(["Connect", "Disable"]);
    }
    LaneEvent::ExtensionUi(request)
}

/// An input dialog replaced by an async panel (`/share`'s loader) is answered cancelled; the dialog queued behind it
/// shows once the panel closes, and later dialogs keep showing. Before, the dialog was dropped unanswered and the
/// "dialog open" flag stayed set, so no `/mcp` dialog showed again.
#[test]
fn interactive_ui_slots_a_replaced_input_dialog_is_cancelled_and_the_queue_moves_on() {
    let (mut app, sent) = app();
    app.apply(dialog("d1", "input", "Server URL"));
    app.apply(dialog("d2", "input", "Token name"));
    assert!(screen(&mut app).contains("Server URL"));
    app.apply(LaneEvent::ShareLoader(true));
    assert_eq!(sent.calls(), [r#"respond d1 {"cancelled":true}"#]);
    app.apply(LaneEvent::ShareLoader(false));
    assert!(screen(&mut app).contains("Token name"), "{}", screen(&mut app));
    app.handle_input("\x1b");
    app.apply(dialog("d3", "input", "Scope"));
    assert!(screen(&mut app).contains("Scope"), "{}", screen(&mut app));
    app.handle_input("\x1b");
    assert_eq!(
        sent.calls(),
        [
            r#"respond d1 {"cancelled":true}"#,
            r#"respond d2 {"cancelled":true}"#,
            r#"respond d3 {"cancelled":true}"#,
        ]
    );
}

/// A select dialog replaced by another selection (an import's "switch to it?") is answered cancelled; the queued one
/// shows when that selection closes.
#[test]
fn interactive_ui_slots_a_replaced_select_dialog_is_cancelled() {
    let (mut app, sent) = app();
    app.apply(dialog("d1", "select", "MCP servers"));
    app.apply(dialog("d2", "select", "web (connected)"));
    app.apply(LaneEvent::PiImported {
        imported: "Imported session".into(),
        source: "/tmp/pi.jsonl".into(),
        session_file: Some("/tmp/s.jsonl".into()),
        current: false,
    });
    assert_eq!(sent.calls(), [r#"respond d1 {"cancelled":true}"#]);
    app.handle_input("\x1b");
    assert!(screen(&mut app).contains("web (connected)"), "{}", screen(&mut app));
    app.handle_input("\r");
    assert_eq!(
        sent.calls(),
        [r#"respond d1 {"cancelled":true}"#, r#"respond d2 {"value":"Connect"}"#]
    );
}

/// A client login whose dialog is replaced is cancelled (its thread no longer waits on a dialog nobody sees), and a
/// login's steps reach only its own dialog: the first login ending never closes the second's.
#[test]
fn interactive_ui_slots_a_login_dialog_belongs_to_its_login() {
    let (mut app, sent) = app();
    let started = |name: &str| LoginStep::Started {
        name: name.into(),
        reach: "local".into(),
    };
    app.apply(LaneEvent::Login(1, started("First")));
    assert!(screen(&mut app).contains("First"));
    // A `/mcp` dialog replaces it: the login is cancelled.
    app.apply(dialog("d1", "input", "Server URL"));
    assert_eq!(sent.calls(), ["login_reply 1 None"]);
    app.handle_input("\x1b");
    sent.clear();
    // A second login; the first one's late end does not close it.
    app.apply(LaneEvent::Login(2, started("Second")));
    app.apply(LaneEvent::Login(1, LoginStep::Finished));
    assert!(screen(&mut app).contains("Second"), "{}", screen(&mut app));
    // Esc cancels the login the dialog shows.
    app.handle_input("\x1b");
    assert_eq!(sent.calls(), ["login_reply 2 None"]);
    assert!(!screen(&mut app).contains("Second"));
}

/// A session opened again (a worker restart, a reattach) clears the old lane's run status: Esc then neither stops a
/// retry nor cancels a compaction that no longer exists.
#[test]
fn interactive_ui_slots_reopening_resets_run_status() {
    let (mut app, sent) = app();
    app.apply(LaneEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 60_000,
    });
    app.apply(LaneEvent::CompactionStarted {
        reason: "threshold".into(),
    });
    app.apply(opened());
    app.handle_input("\x1b");
    assert_eq!(sent.calls(), Vec::<String>::new());
    // Without a reopen, the same Esc stops the retry.
    app.apply(LaneEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 60_000,
    });
    app.handle_input("\x1b");
    assert_eq!(sent.calls(), ["abort_retry"]);
}

/// A rewind whose message request failed (an error, not the messages) does not turn the next `/fork` into a rewind.
#[test]
fn interactive_ui_slots_a_failed_rewind_does_not_hijack_fork() {
    let (lane, _sent) = Sent::new(record);
    let qol = interactive::qol::QolSettings {
        overlays: true,
        ..interactive::qol::QolSettings::parity()
    };
    let mut app = App::new(
        AppOptions {
            qol,
            ..support::options()
        },
        lane,
    );
    app.apply(opened());
    // Esc-Esc in one read: the rewind picker's messages are asked for, and the request fails.
    app.handle_input("\x1b\x1b");
    app.apply(LaneEvent::Error("session_operation_failed: worker restarting".into()));
    for c in "/fork".chars() {
        app.handle_input(&c.to_string());
    }
    app.handle_input("\r");
    app.apply(LaneEvent::ForkMessages(Vec::new()));
    let shown = screen(&mut app);
    assert!(shown.contains("No messages to fork from"), "{shown}");
}
