use crate::tui_harness::{Mock, QUIET, TempDir, Tui};

#[test]
fn cli_interactive_observer_controls_never_start_a_model_request() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let tui = Tui::start_at(
        &tmp.0,
        mock.port,
        120,
        40,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &QUIET,
    );
    tui.wait("Session:");
    for (command, expected) in [
        ("on", "Observer enabled for this session."),
        ("show", "Observer is on. No suggestion."),
        ("dismiss", "Observer suggestion cleared."),
        ("known", "Observer suggestion cleared."),
        ("off", "Observer disabled for this session."),
    ] {
        tui.send_text(&format!("/observer {command}"));
        tui.send_keys(&["Enter"]);
        tui.wait(expected);
    }
    assert!(mock.requests().is_empty());
}

#[test]
fn cli_interactive_observer_model_picker_saves_without_model_requests() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let tui = Tui::start_at(
        &tmp.0,
        mock.port,
        140,
        45,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &QUIET,
    );
    tui.wait("Session:");
    tui.send_text("/settings");
    tui.send_keys(&["Enter"]);
    tui.wait("Models of the session, inference frames");
    tui.send_keys(&["Enter"]);
    tui.wait("Observer model");
    tui.send_keys(&["Down", "Down", "Down", "Down", "Down", "Down", "Down", "Down", "Enter"]);
    tui.wait("Same as session model");
    tui.send_text("claude-haiku-4-5");
    tui.send_keys(&["Enter"]);
    tui.wait("Observer model               anthropic/claude-haiku-4-5 (setting)");
    tui.send_keys(&["Escape"]);
    tui.wait("Models of the session, inference frames");
    tui.send_keys(&["Escape"]);
    tui.wait("the next observer check");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.0.join("agent/settings.json")).unwrap()).unwrap();
    assert!(
        settings["observer"]["model"]
            .as_str()
            .unwrap()
            .contains("claude-haiku-4-5")
    );
    assert!(mock.requests().is_empty());
}
