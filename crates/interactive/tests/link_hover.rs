//! Claude-Code-style links in the product: markdown links keep only their color (no underline), the fullscreen link
//! under the pointer is underlined, pointer motion that changes nothing is not painted again, and
//! `terminal.linkHover` / `PI_LINK_HOVER` pick where hover is on. Markdown links carry their URL for the viewport on
//! every row they wrap onto, and the terminal never sees those targets. With `ULTRON_TUI_PARITY=1` links are as in
//! TypeScript: underlined, no hover, no targets.

use crate::support;

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneEvent, SessionOpened};
use interactive::qol::{LINK_HOVER_ENV, PARITY_ENV, QolSettings};
use interactive::render_settings::{self, RenderSettings};
use interactive::run::{UiEvent, paint_loop};
use interactive::{AssistantBlock, AssistantMessage, AssistantView};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};
use theme::Fg;
use tui::{Component, LinkHover, strip_link_targets, strip_terminal_sequences};

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

/// A URL too long for one row of a `W`-column transcript.
const LONG: &str = "https://example.com/a/very/long/path/that/does/not/fit/on/one/row/of/the/screen?q=1";

/// The settings the product paints with when the terminal gets OSC 8 links or not, with or without link targets.
fn product(hyperlinks: bool, link_targets: bool) -> RenderSettings {
    RenderSettings {
        hyperlinks,
        plain_links: true,
        link_targets,
        ..RenderSettings::default()
    }
}

/// A session whose links wrap: in a user message, as a bare URL, under a label, in a list, a quote, a table and tool
/// output.
fn link_transcript() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": format!("see {LONG} and [the docs]({LONG})"), "timestamp": 1}),
        json!({"role": "assistant", "stopReason": "toolUse", "content": [
            {"type": "text", "text": format!("Open {LONG} now.\n\nOr read [the long manual with a label]({LONG}).")},
            {"type": "toolCall", "id": "call_1", "name": "rlm", "arguments": {"code": "print('https://example.com/tool')"}}
        ]}),
        json!({"role": "toolResult", "toolCallId": "call_1", "toolName": "rlm", "isError": false,
            "content": [{"type": "text", "text": format!("fetched {LONG}")}]}),
        json!({"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": format!(
            "- item [docs](https://example.com/docs), then (see {LONG}).\n- <{LONG}>\n- see [the manual]({LONG})\n\n\
             > quoted [q](https://example.com/q) and [the manual]({LONG})\n\n\
             | link | note |\n|---|---|\n| [the manual]({LONG}) | x |"
        )}]}),
    ]
}

/// Every write of the paint loop for [`link_transcript`] under `settings`: a drag selects over the links, then one
/// frame is narrower.
fn paint_links(qol: QolSettings, settings: RenderSettings) -> Vec<String> {
    render_settings::set(settings);
    let mut app = crate::qol_parity::parity_app(qol);
    let batches = vec![
        vec![UiEvent::Lane(LaneEvent::Opened(SessionOpened {
            session_id: "sess-1".into(),
            session_file: Some("/s/sess-1.jsonl".into()),
            name: None,
            messages: link_transcript(),
        }))],
        vec![
            UiEvent::Input("\x1b[<0;4;7M".into()),
            UiEvent::Input("\x1b[<32;31;12M".into()),
        ],
        vec![UiEvent::Input("x".into())],
        vec![UiEvent::Input("y".into())],
        vec![UiEvent::InputClosed],
    ];
    let (tx, rx) = std::sync::mpsc::channel();
    let pending = RefCell::new(batches.into_iter());
    let calls = Cell::new(0usize);
    let size = || {
        if let Some(batch) = pending.borrow_mut().next() {
            for event in batch {
                tx.send(event).unwrap();
            }
        }
        calls.set(calls.get() + 1);
        if calls.get() == 4 { (44, 40) } else { (60, 40) }
    };
    let mut writes = Vec::new();
    paint_loop(
        &mut app,
        &rx,
        &mut |data| {
            writes.push(data.to_string());
            Ok(())
        },
        &size,
        None,
    )
    .unwrap();
    render_settings::set(RenderSettings::default());
    writes
}

/// An OSC 8 sequence that opens a link (a URL after its params), as against the empty-URL resets rows end with.
fn opens_a_link(write: &str) -> bool {
    write.match_indices("\x1b]8;").any(|(at, _)| {
        let body = &write[at + 4..];
        body.split_once(';')
            .is_some_and(|(_, url)| !url.starts_with(['\x07', '\x1b']))
    })
}

/// The targets the transcript carries for the viewport change no byte the terminal gets: with them on, the paint
/// loop writes exactly what it writes with them off, in the fullscreen and the split layout, with or without OSC 8.
/// Without OSC 8 no link reaches the terminal at all.
#[test]
fn interactive_link_targets_never_reach_the_terminal() {
    render_settings::set(product(false, true));
    let rows = assistant(&format!("Open {LONG} now."));
    render_settings::set(RenderSettings::default());
    assert!(
        rows.iter().any(|row| strip_link_targets(row) != row.as_str()),
        "{rows:?}"
    );

    let fullscreen = QolSettings {
        render_gate: false,
        ..QolSettings::default()
    };
    let split = QolSettings {
        viewport_split: true,
        ..fullscreen.clone()
    };
    for qol in [fullscreen, split] {
        for hyperlinks in [false, true] {
            let with = paint_links(qol.clone(), product(hyperlinks, true));
            let without = paint_links(qol.clone(), product(hyperlinks, false));
            assert!(with.concat().contains("example.com/a/very"), "{with:?}");
            assert_eq!(with.len(), without.len());
            for (i, (got, want)) in with.iter().zip(&without).enumerate() {
                assert_eq!(
                    got, want,
                    "write {i}, hyperlinks {hyperlinks}, split {}",
                    qol.viewport_split
                );
            }
            assert_eq!(with.iter().any(|w| opens_a_link(w)), hyperlinks);
        }
    }
}

/// The app, with the product's settings and no OSC 8, on a turn whose answer has `text`; links hover and open.
fn targets_app(text: &str) -> (App, Arc<Mutex<Vec<String>>>) {
    render_settings::set(product(false, true));
    let mut app = app(text);
    app.set_link_hover(true);
    let opened = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&opened);
    app.set_url_opener(Arc::new(move |url: &str| record.lock().unwrap().push(url.to_string())));
    (app, opened)
}

fn click(app: &mut App, button: u8, col: usize, row: usize) {
    for end in ['M', 'm'] {
        app.handle_input(&format!("\x1b[<{button};{};{}{end}", col + 1, row + 1));
    }
}

/// A markdown link too long for its row wraps, and a click or an Option-click on any row it covers opens the whole
/// URL, a bare one or one under a label; hover underlines the link on the hovered row only.
#[test]
fn interactive_a_wrapped_markdown_link_opens_whole_from_any_row() {
    let (mut app, opened) = targets_app(&format!("Open {LONG} now.\n\nOr read [the manual]({LONG})."));
    let (first, start) = find(&mut app, "https://example.com/a/very");
    let (second, _) = find(&mut app, "one/row/of/the/screen?q=1 now.");
    assert_eq!(second, first + 1);
    let (label, _) = find(&mut app, "the manual");
    let (label_end, _) = find(&mut app, "screen?q=1).");
    assert!(label_end > label);
    click(&mut app, 0, 3, second);
    click(&mut app, 0, start + 2, first);
    click(&mut app, 8, 3, second);
    click(&mut app, 8, 5, label_end);
    assert_eq!(*opened.lock().unwrap(), [LONG; 4]);

    app.handle_input(&motion(3, second));
    let frame = app.render(W, H);
    assert!(frame[second].contains("\x1b[4m"), "{:?}", frame[second]);
    assert!(!frame[first].contains("\x1b[4m"), "{:?}", frame[first]);
    let underlined = frame[second]
        .split("\x1b[4m")
        .nth(1)
        .unwrap()
        .split("\x1b[24m")
        .next()
        .unwrap();
    assert_eq!(strip_terminal_sequences(underlined), "one/row/of/the/screen?q=1");
    render_settings::set(RenderSettings::default());
}

/// A markdown link whose URL has a control character shows as its label alone, with or without OSC 8 and link
/// targets: after the row's zone markers, no OSC 8 sequence, no raw escape or BEL, the row unbroken.
#[test]
fn interactive_markdown_link_with_control_characters_is_plain_text() {
    for hyperlinks in [false, true] {
        render_settings::set(product(hyperlinks, true));
        for href in ["https://x.dev/a\u{1b}[2Jb", "https://x.dev/a\u{7}b"] {
            let rows = assistant(&format!("Read [the docs](<{href}>) now."));
            let row = rows.iter().find(|l| l.contains("the docs")).unwrap();
            let row = &row[row.find(" Read").unwrap()..];
            assert!(!row.contains("\x1b]8;") && !row.contains('\x07'), "{row:?}");
            assert!(
                row.match_indices('\x1b')
                    .all(|(at, _)| row[at..].starts_with("\x1b[") && !row[at..].starts_with("\x1b[2J")),
                "{row:?}"
            );
            assert_eq!(strip_terminal_sequences(row).trim(), "Read the docs now.", "{href:?}");
        }
    }
    render_settings::set(RenderSettings::default());
}
