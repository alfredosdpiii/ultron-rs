//! Claude-Code-style links in the product: markdown links keep only their color (no underline), the fullscreen link
//! under the pointer is underlined, pointer motion that changes nothing is not painted again, and
//! `terminal.linkHover` / `PI_LINK_HOVER` pick where hover is on. With `ULTRON_TUI_PARITY=1` links are as in
//! TypeScript: underlined, no hover.

use crate::support;

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneEvent, SessionOpened};
use interactive::qol::{LINK_HOVER_ENV, PARITY_ENV, QolSettings};
use interactive::render_settings::{self, RenderSettings};
use interactive::{AssistantBlock, AssistantMessage, AssistantView};
use serde_json::{Value, json};
use theme::Fg;
use tui::{Component, LinkHover, strip_terminal_sequences};

const URL: &str = "https://example.com/docs?q=1";
const W: usize = 60;
const H: usize = 16;

fn assistant(text: &str) -> Vec<String> {
    AssistantMessage {
        theme: support::dark_theme(),
        message: AssistantView {
            content: vec![AssistantBlock::Text(text.into())],
            stop_reason: "stop".into(),
            error_message: None,
        },
        hide_thinking: false,
        hidden_thinking_label: "Thinking...".into(),
        output_pad: 1,
        thinking_overrides: Vec::new(),
        code_highlight: None,
    }
    .render(W)
}

#[test]
fn interactive_markdown_links_keep_only_their_color_with_plain_links() {
    let text = format!("# Title\n\nRead [the docs]({URL}) first.");
    let link = support::dark_theme().fg(Fg::MdLink, "the docs");
    let row = |lines: &[String], needle: &str| lines.iter().find(|l| l.contains(needle)).unwrap().clone();

    let ts = assistant(&text);
    assert!(row(&ts, "the docs").contains("\x1b[4m"), "{ts:?}");

    render_settings::set(RenderSettings {
        plain_links: true,
        ..RenderSettings::default()
    });
    let plain = assistant(&text);
    render_settings::set(RenderSettings::default());
    let line = row(&plain, "the docs");
    assert!(line.contains(&link) && !line.contains("\x1b[4m"), "{line:?}");
    assert!(
        row(&plain, "Title").contains("\x1b[4m"),
        "headings stay underlined: {plain:?}"
    );
    assert_eq!(
        plain.iter().map(|l| strip_terminal_sequences(l)).collect::<Vec<_>>(),
        ts.iter().map(|l| strip_terminal_sequences(l)).collect::<Vec<_>>()
    );
}

#[test]
fn interactive_qol_link_hover_setting_env_and_parity() {
    let env = |value: &'static str| move |k: &str| (k == LINK_HOVER_ENV).then(|| value.to_string());
    let hover =
        |settings: Value, var: &dyn Fn(&str) -> Option<String>| QolSettings::from_settings(&settings, var).link_hover;
    let none = |_: &str| None;
    assert_eq!(hover(json!({}), &none), LinkHover::Auto);
    assert_eq!(hover(json!({"terminal": {"linkHover": true}}), &none), LinkHover::On);
    assert_eq!(hover(json!({"terminal": {"linkHover": false}}), &none), LinkHover::Off);
    assert_eq!(
        hover(json!({"terminal": {"linkHover": "auto"}}), &none),
        LinkHover::Auto
    );
    assert_eq!(hover(json!({}), &env("1")), LinkHover::On);
    assert_eq!(hover(json!({}), &env("0")), LinkHover::Off);
    assert_eq!(hover(json!({}), &env("auto")), LinkHover::Auto);
    assert_eq!(
        hover(json!({"terminal": {"linkHover": "auto"}}), &env("1")),
        LinkHover::On
    );
    assert_eq!(
        hover(json!({"terminal": {"linkHover": false}}), &env("1")),
        LinkHover::Off
    );
    assert_eq!(
        hover(json!({"terminal": {"linkHover": true}}), &env("0")),
        LinkHover::On
    );

    let defaults = QolSettings::from_settings(&json!({}), &none);
    assert!(defaults.plain_links);
    let parity = |k: &str| (k == PARITY_ENV).then(|| "1".to_string());
    let parity = QolSettings::from_settings(&json!({"terminal": {"linkHover": true}}), &parity);
    assert!(!parity.plain_links);
    assert_eq!(parity.link_hover, LinkHover::Off);
}

fn app(text: &str) -> App {
    let mut app = App::new(
        AppOptions {
            qol: QolSettings::parity(),
            ..support::options()
        },
        std::sync::mpsc::channel().0,
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: vec![
            json!({"role": "user", "content": "where are the docs?"}),
            json!({"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": text}]}),
        ],
    }));
    app
}

/// The row and column of `needle` on the plain screen.
fn find(app: &mut App, needle: &str) -> (usize, usize) {
    let rows: Vec<String> = app.render(W, H).iter().map(|l| strip_terminal_sequences(l)).collect();
    let row = rows.iter().position(|l| l.contains(needle)).unwrap();
    (row, rows[row].find(needle).unwrap())
}

fn motion(col: usize, row: usize) -> String {
    format!("\x1b[<35;{};{}M", col + 1, row + 1)
}

fn underlined(app: &mut App, row: usize) -> bool {
    app.render(W, H)[row].contains("\x1b[4m")
}

/// A plain URL and an OSC 8 link: underlined while the pointer is over them, and a motion that keeps the same link
/// under the pointer reports nothing to paint.
#[test]
fn interactive_fullscreen_hover_underlines_the_link_and_repaints_only_on_change() {
    for hyperlinks in [false, true] {
        render_settings::set(RenderSettings {
            hyperlinks,
            plain_links: true,
            ..RenderSettings::default()
        });
        let mut app = app(&format!("Read [the manual]({URL}) or {URL} first."));
        app.set_link_hover(true);
        let label = if hyperlinks { "the manual" } else { URL };
        let (row, col) = find(&mut app, label);
        assert!(!underlined(&mut app, row), "{hyperlinks}");
        assert!(app.handle_input(&motion(col + 1, row)), "{hyperlinks}");
        assert!(underlined(&mut app, row), "{hyperlinks}");
        assert!(!app.handle_input(&motion(col + 3, row)), "{hyperlinks}");
        assert!(app.handle_input(&motion(0, row)), "{hyperlinks}");
        assert!(!underlined(&mut app, row), "{hyperlinks}");
        assert!(!app.handle_input(&motion(0, row + 1)), "{hyperlinks}");
        assert!(app.handle_input("x"), "a key still repaints");
    }
    render_settings::set(RenderSettings::default());
}

/// Hover is the product's to turn on (with the tracking it needs): an app left alone underlines nothing.
#[test]
fn interactive_fullscreen_hover_is_off_until_the_product_turns_it_on() {
    render_settings::set(RenderSettings {
        plain_links: true,
        ..RenderSettings::default()
    });
    let mut app = app(&format!("Docs at {URL}"));
    let (row, col) = find(&mut app, URL);
    app.handle_input(&motion(col + 1, row));
    assert!(!underlined(&mut app, row));
    render_settings::set(RenderSettings::default());
}
