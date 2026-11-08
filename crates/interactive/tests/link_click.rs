//! Opening links from the TUI (tui-renderer.ts passes `openUrl: openBrowser` to the fullscreen screen): a primary
//! click on an OSC 8 link in the fullscreen transcript opens it, and so does a link chosen in the `/copy` picker.
//! The app hands the URL to its opener; the session launcher behind the product's opener (`config::browser`) is
//! tested in crates/mcp/tests/gateway_command.rs, and the click rule itself is replayed against the TypeScript in
//! crates/tui/tests/fullscreen_clicks.rs.

use crate::support;
use std::sync::{Arc, Mutex};

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneEvent, ModelInfo, SessionOpened};
use interactive::qol::QolSettings;
use interactive::render_settings::{self, RenderSettings};
use serde_json::json;
use tui::strip_terminal_sequences;

const URL: &str = "https://example.com/docs?q=1";
const W: usize = 60;
const H: usize = 16;

/// An app on one turn whose answer is `text`.
fn app(qol: QolSettings, text: &str) -> App {
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
            models: vec![model.clone()],
            model,
            thinking_level: "off".into(),
            qol,
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

/// An opener that records the URLs it was asked to open.
fn recording_opener() -> (tui::UrlOpener, Arc<Mutex<Vec<String>>>) {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&opened);
    (
        Arc::new(move |url: &str| record.lock().unwrap().push(url.to_string())),
        opened,
    )
}

fn screen(app: &mut App) -> Vec<String> {
    app.render(W, H)
        .iter()
        .map(|l| strip_terminal_sequences(l).trim_end().to_string())
        .collect()
}

#[test]
fn interactive_fullscreen_click_on_a_markdown_link_opens_it() {
    render_settings::set(RenderSettings {
        hyperlinks: true,
        ..RenderSettings::default()
    });
    let (opener, opened) = recording_opener();
    let mut app = app(QolSettings::parity(), &format!("Read [the manual]({URL}) first."));
    app.set_url_opener(opener);
    let rows = screen(&mut app);
    let row = rows.iter().position(|l| l.contains("the manual")).unwrap();
    let col = rows[row].find("the manual").unwrap() + 2;
    // A drag over the link selects text and opens nothing; a click opens it.
    app.handle_input(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    app.handle_input(&format!("\x1b[<32;{};{}M", col + 3, row + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", col + 3, row + 1));
    screen(&mut app);
    assert!(opened.lock().unwrap().is_empty());
    app.handle_input(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", col + 1, row + 1));
    assert_eq!(*opened.lock().unwrap(), [URL]);
    render_settings::set(RenderSettings::default());
}

#[test]
fn interactive_copy_picker_opens_a_link() {
    let (opener, opened) = recording_opener();
    let mut app = app(QolSettings::default(), &format!("Docs at {URL}"));
    app.set_url_opener(opener);
    for data in ["/copy", "\r"] {
        app.handle_input(data);
    }
    app.render(W, H);
    // Into the turn's blocks: the link is the only one; `o` opens it.
    app.handle_input("\x1b[C");
    app.render(W, H);
    app.handle_input("o");
    assert_eq!(*opened.lock().unwrap(), [URL]);
    let rows = screen(&mut app);
    assert!(rows.iter().any(|l| l.contains(&format!("Opened {URL}"))), "{rows:#?}");
}

#[test]
fn interactive_copy_picker_without_an_opener_opens_nothing() {
    let mut app = app(QolSettings::default(), &format!("Docs at {URL}"));
    for data in ["/copy", "\r"] {
        app.handle_input(data);
    }
    app.render(W, H);
    app.handle_input("\x1b[C");
    app.render(W, H);
    app.handle_input("o");
    let rows = screen(&mut app);
    assert!(rows.iter().any(|l| l.contains("Error: no browser opener")), "{rows:#?}");
}
