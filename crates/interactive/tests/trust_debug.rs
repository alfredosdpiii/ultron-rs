//! `/trust`, `/debug` and `/reload`: replays fixtures/trust-debug.json (recorded by fixtures/generate-trust-debug.mjs
//! from client-tui-pi-commands.ts `showTrustSelector` / `writeDebugLog` and trust-selector.ts) and drives the app with
//! the commands the app sends (slash-commands-provider.ts `/reload`).

use std::rc::Rc;

use crate::support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::debug_log::{ClientFacts, DebugCapture, debug_log, notice};
use interactive::lane::{LaneCommand, LaneEvent, ModelInfo, SessionOpened};
use interactive::trust_selector::{TrustAction, TrustChoice, TrustSelector, TrustView, saved_status};
use serde_json::{Value, json};
use theme::{ColorMode, builtin_theme_json, create_theme};
use tui::Component;
use tui::KeybindingsManager;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/trust-debug.json")).unwrap()
}

fn keys() -> KeybindingsManager {
    support::keys()
}

fn key(name: &str) -> &str {
    match name {
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "enter" => "\r",
        "esc" => "\x1b",
        other => other,
    }
}

fn theme(name: &str) -> Rc<theme::Theme> {
    let (theme, mode) = name.split_once('-').unwrap();
    let mode = if mode == "truecolor" {
        ColorMode::Truecolor
    } else {
        ColorMode::Ansi256
    };
    Rc::new(create_theme(&builtin_theme_json(theme).unwrap(), mode, None).unwrap())
}

/// `getProjectTrustOptions("/work/project")` (the cli computes them with `config::trust::trust_options`).
fn choices() -> Vec<TrustChoice> {
    let update = |p: &str, d: Option<bool>| (p.to_string(), d);
    vec![
        TrustChoice {
            label: "Trust".into(),
            trusted: true,
            updates: vec![update("/work/project", Some(true))],
            saved_path: Some("/work/project".into()),
        },
        TrustChoice {
            label: "Trust parent folder (/work)".into(),
            trusted: true,
            updates: vec![update("/work", Some(true)), update("/work/project", None)],
            saved_path: Some("/work".into()),
        },
        TrustChoice {
            label: "Do not trust".into(),
            trusted: false,
            updates: vec![update("/work/project", Some(false))],
            saved_path: Some("/work/project".into()),
        },
    ]
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn interactive_trust_selector_renders_and_selects_like_ts() {
    let f = fixture();
    let cases = f["trust"].as_array().unwrap();
    assert_eq!(cases.len(), 2 * 2 * 5 * 2 * 5);
    for case in cases {
        let label = format!(
            "{} w{} saved={} trusted={} {:?}",
            case["theme"], case["width"], case["saved"], case["projectTrusted"], case["script"]
        );
        let width = case["width"].as_u64().unwrap() as usize;
        let saved = case["saved"]
            .as_object()
            .map(|s| (s["path"].as_str().unwrap().to_string(), s["decision"] == true));
        let view = TrustView {
            cwd: f["cwd"].as_str().unwrap().into(),
            choices: choices(),
            saved,
            project_trusted: case["projectTrusted"] == true,
        };
        let mut selector = TrustSelector::new(theme(case["theme"].as_str().unwrap()), keys(), view);
        let steps = case["steps"].as_array().unwrap();
        assert_eq!(
            selector.render(width),
            strings(&steps[0]["lines"]),
            "{label}: first frame"
        );
        let mut outcome = None;
        for step in &steps[1..] {
            outcome = selector.handle_input(key(step["key"].as_str().unwrap()));
            if step["lines"].is_null() {
                assert!(outcome.is_some(), "{label}: closed after {}", step["key"]);
            } else {
                assert_eq!(outcome, None, "{label}");
                assert_eq!(
                    selector.render(width),
                    strings(&step["lines"]),
                    "{label}: after {}",
                    step["key"]
                );
            }
        }
        match outcome {
            Some(TrustAction::Selected { trusted, updates }) => {
                let updates: Vec<Value> = updates
                    .into_iter()
                    .map(|(path, decision)| json!({"path": path, "decision": decision}))
                    .collect();
                assert_eq!(json!(updates), case["updates"], "{label}");
                assert_eq!(json!(saved_status(trusted)), case["status"], "{label}");
            }
            Some(TrustAction::Cancelled) => assert!(case["updates"].is_null() && case["status"].is_null(), "{label}"),
            None => panic!("{label}: the script ends the selector"),
        }
    }
    assert_eq!(
        f["failed"],
        json!([
            "Error: No Session is attached",
            "Error: worker gone",
            "Error: trust.json is read-only"
        ])
    );
}

#[test]
fn interactive_debug_log_text_and_notice_match_ts() {
    let f = fixture();
    let dark = theme("dark-truecolor");
    for case in f["debug"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if name == "unwritable" {
            assert_eq!(case["status"], "Error: Could not write debug log: <reason>");
            continue;
        }
        let input = &case["input"];
        let text = |v: &Value| v.as_str().map(str::to_string);
        let capture = DebugCapture {
            width: input["width"].as_u64().unwrap() as usize,
            height: input["height"].as_u64().unwrap() as usize,
            lines: strings(&input["lines"]),
            messages: input["messages"].as_array().unwrap().clone(),
            session_id: text(&input["sessionId"]),
        };
        let client = ClientFacts {
            time: "<time>".into(),
            process: "<client>".into(),
            cwd: "<cwd>".into(),
            server_id: text(&input["server"]["serverId"]),
            transport: text(&input["server"]["transport"]),
            socket_path: text(&input["server"]["socketPath"]),
        };
        let worker = match case["worker"]["error"].as_str() {
            Some(error) => Err(error.to_string()),
            None => Ok(case["worker"].clone()),
        };
        let (log, summary) = debug_log(&capture, &client, &worker);
        assert_eq!(log, case["text"].as_str().unwrap(), "{name}");
        // The notice pads to its width; the recorded path was replaced after rendering.
        let trimmed = |lines: Vec<String>| -> Vec<String> { lines.iter().map(|l| l.trim_end().to_string()).collect() };
        assert_eq!(
            trimmed(notice(&dark, "<path>", &summary, 200)),
            trimmed(strings(&case["notice"])),
            "{name}"
        );
        assert!(case["status"].is_null());
    }
}

fn record(command: &LaneCommand) -> Option<String> {
    Some(match command {
        LaneCommand::Prompt { text, .. } => format!("prompt {text}"),
        LaneCommand::ReadTrust => "read_trust".into(),
        LaneCommand::SetProjectTrust { trusted, updates } => format!("set_project_trust {trusted} {updates:?}"),
        LaneCommand::WriteDebugLog(capture) => format!(
            "write_debug_log {}x{} lines={} messages={} session={:?}",
            capture.width,
            capture.height,
            capture.lines.len(),
            capture.messages.len(),
            capture.session_id
        ),
        LaneCommand::ReloadPlugins => "reload_plugins".into(),
        _ => return None,
    })
}

fn app() -> (App, Rc<Sent>) {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "A".into(),
        api: String::new(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let (lane, calls) = Sent::new(record);
    let mut app = App::new(
        AppOptions {
            theme: theme("dark-truecolor"),
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
        messages: vec![json!({"role": "user", "content": "hi", "timestamp": 1})],
    }));
    (app, calls)
}

fn submit(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
    app.handle_input("\r");
}

fn screen(app: &mut App) -> String {
    app.render(80, 30)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn take(calls: &Sent) -> Vec<String> {
    let taken = calls.calls();
    calls.clear();
    taken
}

#[test]
fn interactive_trust_command_reads_shows_and_saves_through_the_client() {
    let (mut app, calls) = app();
    submit(&mut app, "/trust");
    assert_eq!(take(&calls), vec!["read_trust"]);
    app.apply(LaneEvent::Trust(TrustView {
        cwd: "/work/project".into(),
        choices: choices(),
        saved: Some(("/work".into(), true)),
        project_trusted: true,
    }));
    let shown = screen(&mut app);
    assert!(shown.contains("Project trust"), "{shown}");
    assert!(
        shown.contains("Saved decision: trusted (inherited from /work)"),
        "{shown}"
    );
    assert!(shown.contains("→ ✓ Trust parent folder (/work)"), "{shown}");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(
        take(&calls),
        vec![r#"set_project_trust false [("/work/project", Some(false))]"#]
    );
    assert!(!screen(&mut app).contains("Project trust"));
    // Escape closes it without saving.
    submit(&mut app, "/trust");
    app.apply(LaneEvent::Trust(TrustView {
        cwd: "/work/project".into(),
        choices: choices(),
        saved: None,
        project_trusted: false,
    }));
    app.handle_input("\x1b");
    assert_eq!(take(&calls), vec!["read_trust"]);
    assert!(!screen(&mut app).contains("Project trust"));
}

#[test]
fn interactive_debug_and_reload_commands_go_through_the_client() {
    let (mut app, calls) = app();
    app.render(80, 30);
    submit(&mut app, "/debug");
    assert_eq!(
        take(&calls),
        vec!["write_debug_log 80x30 lines=30 messages=1 session=Some(\"sess\")"]
    );
    app.apply(LaneEvent::DebugLogWritten {
        path: "/agent/ultron-debug.log".into(),
        summary: vec![
            "Session: sess".into(),
            "Server: s (unix) in /run".into(),
            "Worker: pid 1 · no model · kernels {}".into(),
        ],
    });
    let shown = screen(&mut app);
    assert!(
        shown.contains("✓ Debug log written\n /agent/ultron-debug.log\n Session: sess"),
        "{shown}"
    );
    submit(&mut app, "/reload");
    assert_eq!(take(&calls), vec!["reload_plugins"]);
    assert!(screen(&mut app).contains("Reloading plugins…"));
    app.apply(LaneEvent::Status("Reloaded plugins.".into()));
    assert!(screen(&mut app).contains("Reloaded plugins."));
    assert!(!screen(&mut app).contains("not available in the Rust build"));
}
