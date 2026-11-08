//! Live theme follow (theme-controller.ts auto-sync, tui.ts color palette notifications): an automatic `light/dark`
//! pair turns on `CSI ? 2031 h`, asks `CSI ? 996 n`, and each `CSI ? 997 ; 1|2 n` report switches the theme, without
//! waiting for any answer. Ported from test/theme-controller.test.ts and packages/tui/test/terminal-colors.test.ts
//! (`parseTerminalColorSchemeReport`), with literal expectations.

use crate::support;
use std::path::PathBuf;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::lane::ModelInfo;
use interactive::theme_setup::{ThemeController, parse_color_scheme_report};
use theme::registry::ThemeRegistry;
use theme::{ColorMode, Fg, TerminalTheme, Theme, builtin_theme_json, create_theme};

#[test]
fn interactive_theme_color_scheme_reports_parse_like_ts() {
    assert_eq!(parse_color_scheme_report("\x1b[?997;1n"), Some(TerminalTheme::Dark));
    assert_eq!(parse_color_scheme_report("\x1b[?997;2n"), Some(TerminalTheme::Light));
    assert_eq!(
        parse_color_scheme_report("\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n"),
        Some(TerminalTheme::Dark)
    );
    assert_eq!(
        parse_color_scheme_report("\x1b[?997;1n\x1b[?997;2n\x1b[?997;2n"),
        Some(TerminalTheme::Light)
    );
    assert_eq!(parse_color_scheme_report("\x1b[?997;3n"), None);
    assert_eq!(parse_color_scheme_report("\x1b[?996n"), None);
    assert_eq!(parse_color_scheme_report("x\x1b[?997;1n"), None);
    assert_eq!(parse_color_scheme_report(""), None);
}

fn registry() -> ThemeRegistry {
    ThemeRegistry::new(PathBuf::from("/nonexistent/themes"), ColorMode::Truecolor)
}

fn builtin(name: &str) -> Theme {
    create_theme(&builtin_theme_json(name).unwrap(), ColorMode::Truecolor, None).unwrap()
}

/// Which built-in `theme` draws like.
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

/// "resolves a theme pair and follows terminal appearance changes".
#[test]
fn interactive_theme_pair_follows_terminal_appearance_changes() {
    let mut controller = ThemeController::new(registry(), Some("light/dark".into()), Some("15;0"));
    let applied = controller.apply_from_settings();
    assert_eq!(which(&applied.theme), "dark", "COLORFGBG 15;0 is a dark background");
    assert!(applied.query_scheme);
    assert!(controller.auto_sync());
    let light = controller
        .terminal_scheme_changed(TerminalTheme::Light)
        .expect("switches");
    assert_eq!(which(&light), "light");
    assert_eq!(controller.terminal_theme(), "light");
    assert!(
        controller.terminal_scheme_changed(TerminalTheme::Light).is_none(),
        "the same side changes nothing"
    );
    let dark = controller
        .terminal_scheme_changed(TerminalTheme::Dark)
        .expect("switches back");
    assert_eq!(which(&dark), "dark");
}

/// A named theme does not follow; selecting a pair later does ("detects the current terminal appearance when
/// selecting a theme pair").
#[test]
fn interactive_theme_only_a_pair_syncs() {
    let mut controller = ThemeController::new(registry(), Some("dark".into()), None);
    let applied = controller.apply_from_settings();
    assert!(!applied.query_scheme);
    assert!(!controller.auto_sync());
    assert!(controller.terminal_scheme_changed(TerminalTheme::Light).is_none());
    let applied = controller.set_setting("light/dark");
    assert!(applied.query_scheme, "a pair asks for the current scheme");
    assert!(controller.auto_sync());
    let light = controller.terminal_scheme_changed(TerminalTheme::Light).unwrap();
    assert_eq!(which(&light), "light");
    let applied = controller.set_setting("dark");
    assert!(!controller.auto_sync());
    assert_eq!(which(&applied.theme), "dark");
    assert!(controller.terminal_scheme_changed(TerminalTheme::Light).is_none());
}

fn app(theme: Rc<Theme>) -> App {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "A".into(),
        api: String::new(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    App::new(
        AppOptions {
            theme,
            models: vec![model.clone()],
            model,
            ..support::options()
        },
        std::sync::mpsc::channel().0,
    )
}

/// The app's side: notifications on with the query, reports switch the shared theme and never reach the editor,
/// and the notifications go off when the setting stops being a pair and when the TUI stops ("disables terminal
/// appearance updates when disposed").
#[test]
fn interactive_theme_reports_switch_the_running_app() {
    let theme = Rc::new(builtin("dark"));
    let mut app = app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    assert_eq!(
        std::mem::take(&mut app.terminal_writes),
        ["\x1b[?2031h", "\x1b[?996n", "\x1b]11;?\x07"]
    );
    assert_eq!(which(&theme), "dark");
    app.handle_input("\x1b[?997;2n");
    assert_eq!(which(&theme), "light");
    app.handle_input("\x1b[?997;1n");
    assert_eq!(which(&theme), "dark");
    assert!(app.terminal_writes.is_empty(), "{:?}", app.terminal_writes);
    let editor: String = app
        .render(80, 30)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l))
        .collect();
    assert!(!editor.contains("997"), "a report is not typed into the editor");
    app.stop_terminal_notifications();
    assert_eq!(std::mem::take(&mut app.terminal_writes), ["\x1b[?2031l"]);
    app.stop_terminal_notifications();
    assert!(app.terminal_writes.is_empty(), "turned off once");
}

/// Without a pair the terminal is never asked and reports change nothing.
#[test]
fn interactive_theme_named_setting_sends_nothing() {
    let theme = Rc::new(builtin("dark"));
    let mut app = app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), Some("dark".into()), None));
    assert!(app.terminal_writes.is_empty(), "{:?}", app.terminal_writes);
    app.handle_input("\x1b[?997;2n");
    assert_eq!(which(&theme), "dark");
    app.stop_terminal_notifications();
    assert!(app.terminal_writes.is_empty());
}

/// The paint loop leaves the terminal as it found it: the notifications go off on exit (`TUI.stop`).
#[test]
fn interactive_theme_exit_turns_the_notifications_off() {
    let theme = Rc::new(builtin("dark"));
    let mut app = app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(interactive::run::UiEvent::InputClosed).unwrap();
    let mut written = String::new();
    interactive::run::paint_loop(
        &mut app,
        &rx,
        &mut |data| {
            written.push_str(data);
            Ok(())
        },
        &|| (80, 24),
        None,
    )
    .unwrap();
    let on = written.find("\x1b[?2031h").expect("turned on");
    let off = written.rfind("\x1b[?2031l").expect("turned off");
    assert!(on < off);
    assert_eq!(written.matches("\x1b[?2031l").count(), 1);
}
