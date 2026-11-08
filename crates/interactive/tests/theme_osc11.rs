//! OSC 11 background detection (theme.ts `detectTerminalBackgroundTheme`, tui.ts `queryTerminalBackgroundColor` /
//! `consumeOsc11BackgroundResponse`): a start without a theme setting sends `ESC ] 11 ; ? BEL`; the reply, arriving
//! through the paint loop's input like any terminal input (split across reads too), switches the theme with a repaint
//! and saves it, and never reaches the editor; without a reply the `COLORFGBG` result stands. An automatic pair asks
//! too, and a scheme report wins over the background reply (`detectTerminalThemeForAuto`).

use std::path::PathBuf;
use std::rc::Rc;

use crate::support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::keybindings::Platform;
use interactive::keybindings::native_app_keybindings;
use interactive::lane::{LaneCommand, ModelInfo};
use interactive::run::{UiEvent, paint_loop};
use interactive::theme_setup::ThemeController;
use serde_json::Value;
use theme::registry::ThemeRegistry;
use theme::{ColorMode, Fg, Theme, builtin_theme_json, create_theme};
use tui::KeybindingsManager;
use tui::{StdinBuffer, StdinEvent};

const QUERY: &str = "\x1b]11;?\x07";

fn registry() -> ThemeRegistry {
    ThemeRegistry::new(PathBuf::from("/nonexistent/themes"), ColorMode::Truecolor)
}

fn builtin(name: &str) -> Theme {
    create_theme(&builtin_theme_json(name).unwrap(), ColorMode::Truecolor, None).unwrap()
}

fn which(theme: &Theme) -> &'static str {
    let probe = |t: &Theme| t.fg(Fg::Accent, "x");
    if probe(theme) == probe(&builtin("light")) {
        "light"
    } else if probe(theme) == probe(&builtin("dark")) {
        "dark"
    } else {
        "other"
    }
}

/// What the app told the session about the terminal's appearance (`LaneCommand::TerminalTheme`).
fn record(command: &LaneCommand) -> Option<String> {
    match command {
        LaneCommand::TerminalTheme(theme) => Some(theme.clone()),
        _ => None,
    }
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn new_app(theme: Rc<Theme>) -> (App, Rc<Sent>) {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "A".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let (lane, sent) = Sent::new(record);
    let app = App::new(
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
    (app, sent)
}

/// The terminal thread's input: each read split by the stdin buffer, a timed-out partial flushed before the next
/// read, every sequence one `UiEvent::Input`.
fn input_events(reads: &[&str]) -> Vec<UiEvent> {
    let mut buffer = StdinBuffer::new();
    let mut events = Vec::new();
    for read in reads {
        for event in buffer.process(read) {
            match event {
                StdinEvent::Data(d) => events.push(UiEvent::Input(d)),
                StdinEvent::Paste(p) => panic!("paste {p:?}"),
            }
        }
        events.extend(buffer.flush().into_iter().map(UiEvent::Input));
    }
    events
}

/// Runs the product's paint loop over `reads`, then stdin closes; what it wrote to the terminal.
fn run(app: &mut App, reads: &[&str]) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    for event in input_events(reads) {
        tx.send(event).unwrap();
    }
    tx.send(UiEvent::InputClosed).unwrap();
    let mut written = String::new();
    paint_loop(
        app,
        &rx,
        &mut |data| {
            written.push_str(data);
            Ok(())
        },
        &|| (80, 24),
        None,
    )
    .unwrap();
    written
}

fn saved(sent: &Sent) -> Vec<(String, Value)> {
    sent.commands()
        .into_iter()
        .filter_map(|command| match command {
            LaneCommand::SaveSetting { key, value } => Some((key, value)),
            _ => None,
        })
        .collect()
}

/// No setting, `COLORFGBG` says dark (and is saved), the terminal reports a light background split across reads: the
/// theme switches to light, light is saved, and the keys around the reply are typed as they were.
#[test]
fn interactive_osc11_reply_switches_the_startup_theme() {
    let theme = Rc::new(builtin("dark"));
    let (mut app, saves) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), None, Some("15;0")));
    assert_eq!(which(&theme), "dark");
    assert_eq!(saved(&saves), [("theme".to_string(), Value::from("dark"))]);
    let written = run(&mut app, &["a\x1b]11;rgb:fa", "fa/fafa/fafa\x1b", "\\b"]);
    assert_eq!(written.matches(QUERY).count(), 1, "asked once at startup");
    assert_eq!(which(&theme), "light", "the reported background wins");
    assert_eq!(
        saved(&saves),
        [
            ("theme".to_string(), Value::from("dark")),
            ("theme".to_string(), Value::from("light"))
        ]
    );
    assert_eq!(app.editor_text(), "ab", "the reply is not typed text");
}

/// No reply: the `COLORFGBG` result stands and nothing more is saved; a fallback start (no `COLORFGBG`) saves nothing
/// until a reply makes the detection confident.
#[test]
fn interactive_osc11_no_reply_keeps_colorfgbg() {
    let theme = Rc::new(builtin("dark"));
    let (mut app, saves) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), None, Some("0;15")));
    let written = run(&mut app, &["hi"]);
    assert!(written.contains(QUERY));
    assert_eq!(which(&theme), "light", "COLORFGBG 0;15 is a light background");
    assert_eq!(saved(&saves), [("theme".to_string(), Value::from("light"))]);
    assert_eq!(app.editor_text(), "hi");

    let theme = Rc::new(builtin("light"));
    let (mut app, saves) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), None, None));
    assert_eq!(which(&theme), "dark", "the fallback");
    assert!(saved(&saves).is_empty(), "a fallback is not saved");
    run(&mut app, &["\x1b]11;rgb:1e1e/1e1e/2e2e\x07"]);
    assert_eq!(which(&theme), "dark");
    assert_eq!(saved(&saves), [("theme".to_string(), Value::from("dark"))]);
}

/// A named theme is never asked about, and a reply (late, or from elsewhere) changes nothing and is not typed.
#[test]
fn interactive_osc11_named_theme_ignores_replies() {
    let theme = Rc::new(builtin("dark"));
    let (mut app, saves) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), Some("dark".into()), Some("0;15")));
    let written = run(&mut app, &["\x1b]11;rgb:ffff/ffff/ffff\x07z"]);
    assert!(!written.contains(QUERY), "{written:?}");
    assert_eq!(which(&theme), "dark");
    assert!(saved(&saves).is_empty());
    assert_eq!(app.editor_text(), "z");
}

/// A pair asks both; the background reply picks the side until a scheme report arrives, and a report wins over a
/// later reply. Nothing is saved for a pair.
#[test]
fn interactive_osc11_pair_scheme_report_wins() {
    let theme = Rc::new(builtin("dark"));
    let (mut app, _) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    let written = run(&mut app, &["\x1b]11;rgb:ffff/ffff/ffff\x07"]);
    assert!(written.contains("\x1b[?996n") && written.contains(QUERY), "{written:?}");
    assert_eq!(which(&theme), "light", "the reply picks the light side");

    let theme = Rc::new(builtin("dark"));
    let (mut app, saves) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    run(&mut app, &["\x1b[?997;1n", "\x1b]11;rgb:ffff/ffff/ffff\x07"]);
    assert_eq!(which(&theme), "dark", "the scheme report came first and wins");
    assert!(saved(&saves).is_empty());
}

/// The reply is applied once: a second, unasked reply changes nothing.
#[test]
fn interactive_osc11_reply_applies_once() {
    let theme = Rc::new(builtin("dark"));
    let (mut app, saves) = new_app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), None, None));
    run(
        &mut app,
        &["\x1b]11;rgb:ffff/ffff/ffff\x07", "\x1b]11;rgb:0000/0000/0000\x07"],
    );
    assert_eq!(which(&theme), "light");
    assert_eq!(saved(&saves), [("theme".to_string(), Value::from("light"))]);
}

/// A reply that arrives after `/settings` chose a named theme leaves that theme alone (`applyFromSettings` only
/// detects without a setting).
#[test]
fn interactive_osc11_reply_after_named_choice_changes_nothing() {
    let mut controller = ThemeController::new(registry(), None, Some("15;0"));
    let applied = controller.apply_from_settings();
    assert!(applied.query_background);
    assert!(controller.expects_background());
    let applied = controller.set_setting("dark");
    assert!(!applied.query_background, "a named theme asks nothing");
    assert!(controller.background_reported(Some((255, 255, 255))).is_none());
    assert!(!controller.expects_background(), "the reply was the one asked for");
    assert_eq!(controller.terminal_theme(), "dark");
    assert_eq!(controller.active_theme_name().as_deref(), Some("dark"));
}

/// The terminal's appearance reaches the client, which tells the session (its `diagram` `auto` theme and the
/// settings lane follow it): `COLORFGBG`'s side when the controller starts, the OSC 11 reply's when it differs, a
/// scheme report's; once per change. A named theme still reports the terminal's side (it never asks OSC 11).
#[test]
fn interactive_osc11_detected_appearance_is_told_to_the_client() {
    let (mut app, told) = new_app(Rc::new(builtin("dark")));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    assert_eq!(told.calls(), ["dark"], "COLORFGBG before any reply");
    run(
        &mut app,
        &["\x1b]11;rgb:ffff/ffff/ffff\x07", "\x1b[?997;1n", "\x1b[?997;1n"],
    );
    assert_eq!(told.calls(), ["dark", "light", "dark"]);

    let (mut app, told) = new_app(Rc::new(builtin("dark")));
    app.set_theme_controller(ThemeController::new(registry(), None, Some("15;0")));
    run(&mut app, &["\x1b]11;rgb:ffff/ffff/ffff\x07"]);
    assert_eq!(told.calls(), ["dark", "light"], "no setting: the reply's side");

    let (mut app, told) = new_app(Rc::new(builtin("dark")));
    app.set_theme_controller(ThemeController::new(registry(), Some("dark".into()), Some("0;15")));
    run(&mut app, &["\x1b]11;rgb:0000/0000/0000\x07"]);
    assert_eq!(told.calls(), ["light"], "a named theme: COLORFGBG's side, no query");
}
