//! `sessions.*` payload checks (kernel_apis::sessions): scope, limits, the span/session forms of `read`, and the
//! cancelled-request stop. The search itself is session_journal's (crates/session-journal/tests/search.rs). New in
//! Rust: literal expectations.

use kernel::Signal;
use kernel_apis::{ApiModule, Request, RootOnly, SessionsModule};
use serde_json::{Value, json};
use session_journal::{SessionRepo, TestClock, Write};
use std::sync::Arc;

fn call(module: &SessionsModule, kind: &str, payload: Value, signal: &Signal) -> Result<Value, String> {
    let host = RootOnly { now: Arc::new(|| 0) };
    module.handle(&Request {
        kind,
        payload: payload.as_object().unwrap(),
        lane: "main",
        caller_task: None,
        host: &host,
        signal,
    })
}

#[test]
fn kernel_apis_sessions_checks_payloads_and_scopes_to_the_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SessionRepo::new(dir.path(), TestClock::shared(1_790_000_000_000, 1));
    for (cwd, id) in [("/w", "here"), ("/x", "there")] {
        let mut session = repo.create(cwd, Some(id), None).unwrap();
        session
            .commit(&[Write::set(session_journal::ns::BRANCH_TIP, "main", Value::Null)])
            .unwrap();
        session
            .append_message("main", json!({"role": "user", "content": "a needle"}))
            .unwrap();
    }
    // Now: 10 days after the epoch; the files are newer, so `days=1` keeps them.
    let module = SessionsModule::new(
        vec![dir.path().to_path_buf(), dir.path().to_path_buf()],
        "here",
        "/w",
        Arc::new(|| 864_000_000),
    );
    let signal = Signal::new();
    let ids = |reply: &Value, key: &str, field: &str| -> Vec<String> {
        reply[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[field].as_str().unwrap().to_string())
            .collect()
    };
    let listed = call(&module, "sessions.list", json!({}), &signal).unwrap();
    assert_eq!(ids(&listed, "sessions", "id"), ["here"], "one root listed once");
    assert_eq!(listed["sessions"][0]["current"], true);
    assert_eq!(listed["sessions"][0]["first_message"], "a needle");
    let all = call(
        &module,
        "sessions.list",
        json!({"cwd": "all", "days": 1, "limit": 1}),
        &signal,
    )
    .unwrap();
    assert_eq!(all["sessions"].as_array().unwrap().len(), 1);
    let found = call(
        &module,
        "sessions.search",
        json!({"terms": ["NEEDLE"], "cwd": "/x"}),
        &signal,
    )
    .unwrap();
    assert_eq!(ids(&found, "hits", "session"), ["there"]);
    let span = found["hits"][0]["span"].as_str().unwrap().to_string();
    let read = call(&module, "sessions.read", json!({"span": span}), &signal).unwrap();
    assert_eq!(
        (read["span"].as_str(), read["session"]["current"].as_bool()),
        (Some(span.as_str()), Some(false))
    );
    let mine = call(&module, "sessions.read", json!({}), &signal).unwrap();
    assert_eq!(mine["session"]["id"], "here");

    for (kind, payload, error) in [
        (
            "sessions.list",
            json!({"cwd": "rel"}),
            "sessions cwd must be an absolute path or \"all\"",
        ),
        (
            "sessions.list",
            json!({"days": 0}),
            "sessions days must be a positive number",
        ),
        (
            "sessions.list",
            json!({"limit": 0}),
            "sessions limit must be an integer of at least 1",
        ),
        ("sessions.list", json!({"bogus": 1}), "Unknown payload field: bogus"),
        ("sessions.search", json!({}), "sessions.search takes a pattern or terms"),
        (
            "sessions.search",
            json!({"terms": "x"}),
            "sessions terms must be a list of strings",
        ),
        (
            "sessions.search",
            json!({"pattern": "x", "branch": ""}),
            "sessions branch must be a lane name or \"all\"",
        ),
        (
            "sessions.read",
            json!({"span": "session://here", "session": "here"}),
            "sessions.read takes a span or session/from_entry/to_entry, not both",
        ),
        (
            "sessions.read",
            json!({"branch": "all"}),
            "sessions.read reads one branch: a lane name, not \"all\"",
        ),
        (
            "sessions.nope",
            json!({}),
            "Ultron RLM host request is not wired: sessions.nope",
        ),
    ] {
        assert_eq!(
            call(&module, kind, payload.clone(), &signal),
            Err(error.to_string()),
            "{kind} {payload}"
        );
    }

    signal.abort("cell ended");
    let stopped = call(&module, "sessions.search", json!({"pattern": "needle"}), &signal).unwrap();
    assert_eq!(
        (stopped["searched"].as_u64(), stopped["truncated"].as_bool()),
        (Some(0), Some(true))
    );
}
