//! `/mcp`'s dialogs follow the session's theme: the TUI draws the worker's dialog requests (`extension_ui_request`)
//! with its own theme, the settings theme. Here: an input dialog and a selection under a light theme use the light
//! theme's colors.

use crate::support;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneEvent, ModelInfo, SessionOpened};
use serde_json::json;
use theme::{ColorMode, Fg, Theme, builtin_theme_json, create_theme};

fn theme(name: &str) -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json(name).unwrap(), ColorMode::Truecolor, None).unwrap())
}

fn app(theme: Rc<Theme>) -> App {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let mut app = App::new(
        AppOptions {
            theme,
            models: vec![model.clone()],
            model,
            thinking_level: "off".into(),
            ..support::options()
        },
        std::sync::mpsc::channel().0,
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "s".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    app
}

/// The first rendered row containing `text`.
fn row_with(app: &mut App, text: &str) -> String {
    app.render(80, 30)
        .into_iter()
        .find(|l| tui::strip_terminal_sequences(l).contains(text))
        .unwrap_or_else(|| panic!("{text}"))
}

#[test]
fn interactive_mcp_dialogs_are_drawn_in_the_session_theme() {
    for name in ["light", "dark"] {
        let t = theme(name);
        let accent = t.fg(Fg::Accent, "Name?");
        let mut app = app(Rc::clone(&t));
        app.apply(LaneEvent::ExtensionUi(
            json!({"type": "extension_ui_request", "id": "p1", "method": "input", "title": "Name?"}),
        ));
        assert!(
            row_with(&mut app, "Name?").contains(&accent),
            "{name}: the input's title"
        );
        app.handle_input("\x1b");
        app.apply(LaneEvent::ExtensionUi(
            json!({"type": "extension_ui_request", "id": "d1", "method": "select",
            "title": "Pick", "options": ["one", "two"]}),
        ));
        let selected = row_with(&mut app, "one");
        let other = theme(if name == "light" { "dark" } else { "light" });
        assert_ne!(t.fg(Fg::Accent, "x"), other.fg(Fg::Accent, "x"));
        let accent_code = t.fg(Fg::Accent, "");
        let code = accent_code.trim_end_matches("\x1b[39m");
        assert!(selected.contains(code), "{name}: the selected option {selected:?}");
    }
}
