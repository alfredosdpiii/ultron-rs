//! The agent hub's actions reach the session: opening an agent's transcript reads it (`agentTranscript`) and shows
//! it, submitting in the transcript steers the agent (`steerAgent`) and reads the transcript again, `r` on a parked
//! (interrupted) agent revives it (`reviveAgent`) and lists the agents again, steering a parked agent revives it
//! first, and what the session answered shows as the hub's notice.

use std::rc::Rc;

mod support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::keybindings::Platform;
use interactive::keybindings::native_app_keybindings;
use interactive::lane::{AgentAction, LaneCommand, LaneEvent, ModelInfo, SessionOpened};
use serde_json::json;
use theme::{ColorMode, builtin_theme_json, create_theme};
use tui::KeybindingsManager;

fn record(command: &LaneCommand) -> Option<String> {
    Some(match command {
        LaneCommand::Agents => "agents".into(),
        LaneCommand::KillAgent(id) => format!("kill {id}"),
        LaneCommand::SteerAgent { id, text } => format!("steer {id} {text}"),
        LaneCommand::ReviveAgent(id) => format!("revive {id}"),
        LaneCommand::AgentTranscript(id) => format!("transcript {id}"),
        _ => return None,
    })
}

fn model() -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "a".into(),
        reasoning: true,
        context_window: 200_000,
        thinking_levels: Vec::new(),
    }
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
    let (lane, sent) = Sent::new(record);
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model()],
            model: model(),
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::default(),
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

fn screen(app: &mut App) -> String {
    app.render(100, 30)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drain the calls made so far.
fn take(c: &Sent) -> Vec<String> {
    let calls = c.calls();
    c.clear();
    calls
}

#[test]
fn interactive_agent_hub_reads_steers_and_revives_through_the_session() {
    let (mut app, c) = app();
    app.handle_input("\x1ba");
    assert_eq!(take(&c), ["agents"]);
    app.apply(LaneEvent::Agents(json!({"subagents": [
        {"id": "scout", "definition": "scout", "state": "running"},
        {"id": "fixer", "definition": "fixer", "state": "interrupted"},
    ]})));
    assert!(screen(&mut app).contains("scout"));

    // Enter opens the running agent's transcript, read from the session.
    app.handle_input("\r");
    assert_eq!(take(&c), ["transcript scout"]);
    app.apply(LaneEvent::AgentTranscript {
        id: "scout".into(),
        messages: Ok(vec![
            json!({"role": "user", "content": "map the parser", "timestamp": 1}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Reading parser.rs"}], "timestamp": 2}),
        ]),
    });
    assert!(screen(&mut app).contains("Reading parser.rs"), "{}", screen(&mut app));

    // Submitting steers it, then reads the transcript again.
    for ch in "focus on src".chars() {
        app.handle_input(&ch.to_string());
    }
    app.handle_input("\r");
    assert_eq!(take(&c), ["steer scout focus on src", "transcript scout"]);

    // Back in the table, `r` on the parked agent revives it and lists the agents again.
    app.handle_input("\x1b");
    app.handle_input("j");
    app.handle_input("r");
    assert_eq!(take(&c), ["revive fixer", "agents"]);
    app.apply(LaneEvent::AgentAnswered {
        id: "fixer".into(),
        action: AgentAction::Revive,
        answer: Ok(Some(json!({"id": "t9"}))),
    });
    assert!(
        screen(&mut app).contains("Revived \"fixer\" as \"t9\"."),
        "{}",
        screen(&mut app)
    );

    // Steering a parked agent revives it first.
    app.handle_input("\r");
    assert_eq!(take(&c), ["transcript fixer"]);
    for ch in "go on".chars() {
        app.handle_input(&ch.to_string());
    }
    app.handle_input("\r");
    assert_eq!(take(&c), ["revive fixer", "agents"]);
}
