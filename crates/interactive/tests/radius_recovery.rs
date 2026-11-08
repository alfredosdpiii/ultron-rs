//! The client TUI while its Radius connection is recovered (client-tui.ts `#handleConnectionState`,
//! `#handleAttachmentState` and `handleInput` while `#busy`), against a recording lane: the recovery status
//! shows, typed input and Enter wait, Ctrl+C still clears and a second one exits, Ctrl+D exits on an empty editor,
//! and the end of the recovery clears the status and takes input again.

use std::rc::Rc;

use crate::support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneCommand, LaneEvent, ModelInfo, SessionOpened};
use protocol::watch::QueueKind;

fn record(command: &LaneCommand) -> Option<String> {
    Some(match command {
        LaneCommand::Prompt { text, .. } => format!("prompt {text}"),
        LaneCommand::Enqueue {
            kind: QueueKind::Steer,
            text,
            ..
        } => format!("steer {text}"),
        LaneCommand::Enqueue { text, .. } => format!("follow_up {text}"),
        LaneCommand::CancelQueued(id) => format!("cancel {id}"),
        LaneCommand::Abort => "abort".into(),
        LaneCommand::NewSession => "new".into(),
        LaneCommand::Resume(path) => format!("resume {path}"),
        LaneCommand::ListSessions => "list".into(),
        LaneCommand::SetName(name) => format!("name {name}"),
        LaneCommand::SelectModel { provider, id } => format!("model {provider}/{id}"),
        _ => return None,
    })
}

fn app() -> (App, Rc<Sent>) {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "m".into(),
        name: "M".into(),
        api: String::new(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let (lane, calls) = Sent::new(record);
    let mut app = App::new(
        AppOptions {
            models: vec![model.clone()],
            model,
            ..support::options()
        },
        lane,
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    (app, calls)
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
}

fn screen(app: &mut App) -> String {
    app.render(80, 30)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn recovery(busy: bool, status: &str) -> LaneEvent {
    LaneEvent::Recovery {
        busy,
        status: status.into(),
    }
}

#[test]
fn interactive_radius_recovery_waits_for_the_connection() {
    let (mut app, calls) = app();
    typed(&mut app, "draft");
    app.apply(recovery(true, "Radius disconnected; retrying…"));
    assert!(screen(&mut app).contains("Radius disconnected; retrying…"));
    // Typing and Enter wait: the editor keeps its draft, nothing is sent.
    typed(&mut app, "more");
    app.handle_input("\r");
    assert!(calls.calls().is_empty());
    let shown = screen(&mut app);
    assert!(shown.contains("draft") && !shown.contains("draftmore"), "{shown}");
    app.apply(recovery(true, "Reconnecting to Radius…"));
    let shown = screen(&mut app);
    assert!(
        shown.contains("Reconnecting to Radius…") && !shown.contains("retrying"),
        "{shown}"
    );
    // Ctrl+D waits while the editor has text; Ctrl+C clears it, then Ctrl+D exits.
    app.handle_input("\x04");
    assert!(!app.exit);
    app.handle_input("\x03");
    assert!(!screen(&mut app).contains("draft"));
    app.handle_input("\x04");
    assert!(app.exit);
}

#[test]
fn interactive_radius_recovery_ends_with_input_back() {
    let (mut app, calls) = app();
    app.apply(recovery(true, "Reattaching Session…"));
    assert!(screen(&mut app).contains("Reattaching Session…"));
    // A second Ctrl+C within the window exits, as it does outside recovery.
    let (mut other, _) = self::app();
    other.apply(recovery(true, "Reattaching Session…"));
    other.handle_input("\x03");
    assert!(!other.exit);
    other.handle_input("\x03");
    assert!(other.exit);
    // Attached again: the status clears and input reaches the session.
    app.apply(recovery(false, ""));
    assert!(!screen(&mut app).contains("Reattaching Session…"));
    typed(&mut app, "hello");
    app.handle_input("\r");
    assert_eq!(calls.calls(), vec!["prompt hello".to_string()]);
}

/// Whether the rendered frame places the hardware cursor (the focused editor's `CURSOR_MARKER`).
fn shows_cursor(app: &mut App, width: usize) -> bool {
    app.render(width, 30)
        .iter()
        .any(|line| line.contains(tui::CURSOR_MARKER))
}

#[test]
fn interactive_radius_recovery_hides_the_editor_cursor() {
    // `#chatInput.focused = !#busy && !(#rlmPaneFocused && #rlmPaneShown())` (client-tui.ts `#rebuild`).
    let (mut app, _) = app();
    assert!(shows_cursor(&mut app, 80));
    app.apply(recovery(true, "Radius disconnected; retrying…"));
    assert!(!shows_cursor(&mut app, 80));
    app.apply(recovery(true, "Reattaching Session…"));
    assert!(!shows_cursor(&mut app, 80));
    app.apply(recovery(false, ""));
    assert!(shows_cursor(&mut app, 80));
    // The RLM pane beside the chat with the focus (Alt+W on a wide terminal) takes the cursor too; Alt+W again gives
    // the focus, and the cursor, back to the chat.
    assert!(shows_cursor(&mut app, 140));
    app.handle_input("\x1bw");
    assert!(!shows_cursor(&mut app, 140));
    app.handle_input("\x1bw");
    assert!(shows_cursor(&mut app, 140));
}
