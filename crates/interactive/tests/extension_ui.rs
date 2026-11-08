//! `/mcp` in the TUI (client-tui.ts `#serveExtensionUI`): the command runs in the session's worker
//! (`ultron.legacy-extension-commands.run`); its dialogs arrive as Pi's `extension_ui_request` and are answered one at a
//! time (`select` in the selector, `input` in a text prompt, Esc cancels), and `notify` shows in the status line.
//! Pi extensions are not supported by ultron-rs: `/extension` says so, and a `confirm` request (only an extension
//! raised one) opens nothing.

use std::rc::Rc;

use crate::support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::keybindings::Platform;
use interactive::keybindings::native_app_keybindings;
use interactive::lane::{LaneCommand, LaneEvent, ModelInfo, SessionOpened};
use serde_json::{Value, json};
use theme::{ColorMode, builtin_theme_json, create_theme};
use tui::KeybindingsManager;

fn record(command: &LaneCommand) -> Option<String> {
    Some(match command {
        LaneCommand::Mcp(args) => format!("run mcp [{args}]"),
        LaneCommand::ExtensionUiRespond { id, answer } => format!("respond {id} {answer}"),
        _ => return None,
    })
}

fn app() -> (App, Rc<Sent>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let (lane, sent) = Sent::new(record);
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model.clone()],
            model,
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::parity(),
            bug_report_target: ("radius.pi.dev".into(), false),
        },
        lane,
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    (app, sent)
}

fn submit(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
    app.handle_input("\r");
}

fn screen(app: &mut App) -> String {
    app.render(100, 30)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn calls(c: &Sent) -> Vec<String> {
    c.calls()
}

fn request(value: Value) -> LaneEvent {
    LaneEvent::ExtensionUi(value)
}

#[test]
fn interactive_mcp_runs_in_the_worker_and_its_dialogs_are_answered_in_order() {
    let (mut app, recorded) = app();
    submit(&mut app, "/mcp");
    submit(&mut app, "/mcp login docs");
    assert_eq!(calls(&recorded), ["run mcp []", "run mcp [login docs]"]);
    // Two dialogs at once: the second waits for the first.
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "d1", "method": "select",
        "title": "MCP servers", "options": ["docs \u{2014} not connected", "web \u{2014} connected"]}),
    ));
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "d2", "method": "select",
        "title": "web (connected)", "options": ["Disconnect", "Disable"]}),
    ));
    let shown = screen(&mut app);
    assert!(shown.contains("MCP servers"), "{shown}");
    assert!(shown.contains("web \u{2014} connected"), "{shown}");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert!(screen(&mut app).contains("web (connected)"));
    app.handle_input("\x1b");
    assert_eq!(
        calls(&recorded)[2..],
        [
            "respond d1 {\"value\":\"web \u{2014} connected\"}".to_string(),
            "respond d2 {\"cancelled\":true}".to_string(),
        ]
    );
}

/// What only Pi extensions raised (`confirm`, `editor`, `setWidget`, ...) shows nothing and answers nothing, and
/// `/extension` says Pi extensions are not supported.
#[test]
fn interactive_mcp_dialogs_leave_extension_only_requests_alone() {
    let (mut app, recorded) = app();
    let before = screen(&mut app);
    for method in ["confirm", "editor", "setWidget", "setStatus"] {
        app.apply(request(
            json!({"type": "extension_ui_request", "id": method, "method": method,
            "title": "Sure?", "widgetKey": "w", "widgetLines": ["WIDGET"], "statusKey": "s", "statusText": "STATUS"}),
        ));
    }
    assert_eq!(screen(&mut app), before);
    submit(&mut app, "/extension mcp status");
    assert!(calls(&recorded).is_empty(), "{:?}", calls(&recorded));
    assert!(screen(&mut app).contains("Error: Pi extensions are not supported by ultron-rs"));
}

#[test]
fn interactive_mcp_input_dialogs_answer_or_cancel_and_notices_show_in_the_status_line() {
    let (mut app, recorded) = app();
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "p1", "method": "input",
        "title": "MCP sign-in for docs: paste the redirect URL, or wait for the browser.",
        "placeholder": "http://127.0.0.1/callback?code=..."}),
    ));
    assert!(screen(&mut app).contains("MCP sign-in for docs"));
    for c in "http://x/cb?code=1".chars() {
        app.handle_input(&c.to_string());
    }
    app.handle_input("\r");
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "p2", "method": "input", "title": "Again"}),
    ));
    app.handle_input("\x1b");
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "n1", "method": "notify",
        "message": "Signed in to \"docs\" (3 tools).", "notifyType": "info"}),
    ));
    assert_eq!(
        calls(&recorded),
        [
            "respond p1 {\"value\":\"http://x/cb?code=1\"}",
            "respond p2 {\"cancelled\":true}",
        ]
    );
    assert!(screen(&mut app).contains("Signed in to \"docs\" (3 tools)."));
}
