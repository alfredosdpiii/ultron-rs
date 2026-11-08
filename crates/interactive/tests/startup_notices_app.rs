//! The startup notices in the running app (interactive-mode.ts `init`): a session that opens without messages asks
//! for the changelog (a resumed one does not), notices that arrive before the session opened are shown with it,
//! the subscription-auth check runs for an `anthropic` model at open and on every model change until it warned,
//! and the warning shows once.

use std::rc::Rc;

use crate::support;
use support::Sent;

use interactive::app::{App, AppOptions};
use interactive::lane::{LaneCommand, LaneEvent, ModelInfo, SessionOpened, StartupNotice};

fn record(command: &LaneCommand) -> Option<String> {
    Some(match command {
        LaneCommand::StartupChangelog => "changelog".into(),
        LaneCommand::SubscriptionAuthWarning(provider) => format!("auth {provider}"),
        _ => return None,
    })
}

fn changelog(calls: &Sent) -> usize {
    calls.calls().iter().filter(|c| *c == "changelog").count()
}

fn auth_checks(calls: &Sent) -> Vec<String> {
    calls
        .calls()
        .iter()
        .filter_map(|c| c.strip_prefix("auth ").map(str::to_string))
        .collect()
}

fn model(provider: &str) -> ModelInfo {
    ModelInfo {
        provider: provider.into(),
        id: "m".into(),
        name: "M".into(),
        api: String::new(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    }
}

fn app(provider: &str) -> (App, Rc<Sent>) {
    let (lane, calls) = Sent::new(record);
    let app = App::new(
        AppOptions {
            models: vec![model(provider)],
            model: model(provider),
            ..support::options()
        },
        lane,
    );
    (app, calls)
}

fn opened(messages: Vec<serde_json::Value>) -> LaneEvent {
    LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages,
    })
}

fn screen(app: &mut App) -> String {
    app.render(100, 60)
        .iter()
        .map(|l| tui::strip_terminal_sequences(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn interactive_startup_asks_for_the_changelog_only_for_a_session_without_messages() {
    let (mut fresh, calls) = app("openai");
    fresh.apply(opened(Vec::new()));
    assert_eq!(changelog(&calls), 1);
    // Opening another session later (a switch) does not ask again.
    fresh.apply(opened(Vec::new()));
    assert_eq!(changelog(&calls), 1);
    let (mut resumed, calls) = app("openai");
    resumed.apply(opened(vec![
        serde_json::json!({"role": "user", "content": "hi", "timestamp": 1}),
    ]));
    assert_eq!(changelog(&calls), 0);
}

#[test]
fn interactive_startup_notices_show_once_the_session_is_open() {
    let (mut app, _) = app("openai");
    app.apply(LaneEvent::StartupNotice(StartupNotice::NewVersion("9.9.9".into())));
    app.apply(LaneEvent::StartupNotice(StartupNotice::WhatsNew {
        markdown: "## [1.2.3]\n\n- New thing".into(),
        collapse: false,
        version: "1.2.3".into(),
    }));
    assert!(!screen(&mut app).contains("Update Available"));
    app.apply(opened(Vec::new()));
    let text = screen(&mut app);
    assert!(text.contains("Update Available"), "{text}");
    assert!(
        text.contains("New version 9.9.9 is available. Run ultron update"),
        "{text}"
    );
    assert!(text.contains("What's New"), "{text}");
    assert!(text.contains("New thing"), "{text}");
    app.apply(LaneEvent::StartupNotice(StartupNotice::WhatsNew {
        markdown: "## [2.0.0]\n\n- Later".into(),
        collapse: true,
        version: "2.0.0".into(),
    }));
    let text = screen(&mut app);
    assert!(
        text.contains("Updated to v2.0.0. Use /changelog to view full changelog."),
        "{text}"
    );
}

#[test]
fn interactive_subscription_auth_warning_is_checked_for_anthropic_and_shown_once() {
    let (mut app, calls) = app("anthropic");
    app.apply(opened(Vec::new()));
    assert_eq!(auth_checks(&calls), vec!["anthropic".to_string()]);
    app.apply(LaneEvent::Model(model("openai")));
    assert_eq!(auth_checks(&calls).len(), 1, "only an anthropic model is checked");
    app.apply(LaneEvent::Model(model("anthropic")));
    assert_eq!(auth_checks(&calls).len(), 2);
    app.apply(LaneEvent::StartupNotice(StartupNotice::SubscriptionAuth));
    app.apply(LaneEvent::StartupNotice(StartupNotice::SubscriptionAuth));
    let text = screen(&mut app);
    assert_eq!(
        text.matches("Warning: Anthropic subscription auth is active.").count(),
        1,
        "{text}"
    );
    // Once it warned, no more checks.
    app.apply(LaneEvent::Model(model("anthropic")));
    assert_eq!(auth_checks(&calls).len(), 2);
}
