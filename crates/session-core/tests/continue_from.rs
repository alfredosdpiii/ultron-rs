//! `Op::ContinueFrom` (`/continue`): a new session continues an old one by searching its log. Its first message names
//! the old session and how to read it, and the next request carries it; nothing of the old conversation is copied.
//! Refused for the session itself, an unknown id, and a session that already has messages. New in Rust: literal
//! expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, user_texts};
use serde_json::Value;
use session_core::core::Op;

#[test]
fn session_core_continue_from_opens_with_a_note_that_points_at_the_old_session() {
    let a = actor(in_order(vec![text("ok"), text("again")]), Setup::default());
    // The old session: another session of this cwd, with a turn of its own.
    let sessions = a.root.join("agent").join("experimental").join("sessions");
    let repo = session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared());
    let mut old = repo.create(&a.cwd.display().to_string(), None, None).unwrap();
    old.commit(&[session_journal::Write::set(
        session_journal::ns::BRANCH_TIP,
        session_journal::run::MAIN_LANE,
        Value::Null,
    )])
    .unwrap();
    old.append_message(
        session_journal::run::MAIN_LANE,
        serde_json::json!({"role": "user", "content": "OLD-SECRET-PLAN", "timestamp": 1}),
    )
    .unwrap();
    let old_id = old.id().to_string();
    drop(old);
    let own = a.call(Op::State).unwrap().unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let continue_from = |session: &str| {
        a.call(Op::ContinueFrom {
            session: session.into(),
        })
    };
    assert_eq!(continue_from(&own), Err("A session cannot continue itself".into()));
    assert_eq!(
        continue_from("no-such-session"),
        Err("No session no-such-session".into())
    );
    let answer = continue_from(&old_id).unwrap().unwrap();
    assert_eq!(answer["session"], old_id.as_str());
    let ended = a.prompt("pick up where we left off");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let texts: Vec<String> = user_texts(&a.requests()[0])
        .iter()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    let note = texts
        .iter()
        .find(|t| t.starts_with("[continuation]"))
        .expect("the note");
    assert!(note.contains(&format!("continues session {old_id}")), "{note}");
    assert!(
        note.contains(&format!(r#"sessions.search("regex", sessions=["{old_id}"])"#)),
        "{note}"
    );
    assert!(!texts.iter().any(|t| t.contains("OLD-SECRET-PLAN")), "{texts:?}");
    assert_eq!(texts.last().map(String::as_str), Some("pick up where we left off"));
    // Once it has messages, a session continues nothing.
    assert_eq!(
        continue_from(&old_id),
        Err("Only a new session can continue another: this one already has messages".into())
    );
}
