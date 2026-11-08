//! The agent hub's session operations on a real session with a real `rlm.spawn` subagent (the scripted provider
//! answers the root and the child by what their requests say):
//! - steer: `agent_message.send {receiver_role: "child", receiver_id, steer: true}` from the root lane, as the human
//!   correction of packages/coding-agent/test/ultron-a42-intervention-subtree-stop.test.ts, with its literal
//!   expectations: `{state: "delivered", steered: true}`, the child's lane receives the untrusted-message envelope
//!   naming "the root agent" and the JSON-encoded text, and a finished child refuses ("already finished");
//! - transcript: the child conversation's messages;
//! - revive: a subagent left `interrupted` runs again from its brief as a new subagent; anything else is refused.

use crate::p_session_support;
use crate::reopen_support;

use p_session_support::{HANG, text, tool};
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// A request of the child: its conversation starts with the brief (the root's only mentions it in its code).
fn is_child(body: &Value) -> bool {
    body["messages"].as_array().into_iter().flatten().any(|m| {
        m["role"] == "user"
            && match &m["content"] {
                Value::String(text) => text.starts_with("CHILD-BRIEF"),
                blocks => blocks
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|b| b["text"].as_str().is_some_and(|t| t.starts_with("CHILD-BRIEF"))),
            }
    })
}
const SPAWN: &str = "h = await rlm.spawn('CHILD-BRIEF: audit the tests', name='auditor')\nprint(h.rlm_child_id)";

/// Polls the session's answers to `done` (a hang guard, never a pass condition): no event announces a subagent's
/// state.
fn poll_until(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + HANG;
    while !done() {
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The root spawns one child; with `release`, the child's first answer waits for the test to send on it.
fn script(release: Option<Receiver<()>>) -> p_session_support::Script {
    Box::new(move |body: &Value, _| {
        let messages = body["messages"].to_string();
        if is_child(body) {
            if messages.contains("Untrusted family message") {
                return text("child steered");
            }
            if let Some(release) = &release {
                release.recv_timeout(HANG).expect("the test releases the child");
            }
            return text("child first");
        }
        if messages.contains("tool_result") {
            text("root done")
        } else {
            tool("toolu_root", SPAWN)
        }
    })
}

fn child_requests(w: &reopen_support::Worker) -> Vec<Value> {
    w.requests().into_iter().filter(is_child).collect()
}

/// Waits until the server saw `n` requests of the child.
fn wait_child_requests(w: &reopen_support::Worker, n: usize) {
    w.requests.wait(&format!("{n} child requests"), |requests| {
        (requests.iter().filter(|r| is_child(r)).count() >= n).then_some(())
    });
}

fn subagents(w: &reopen_support::Worker) -> Vec<Value> {
    w.call(Op::Agents).unwrap().unwrap()["subagents"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn session_core_agent_hub_steers_a_running_subagent_and_reads_its_transcript() {
    let home = Home::new(None);
    let (release, released) = channel();
    let w = home.open(script(Some(released)), &[], None);
    let tx = w.tx.clone();
    std::thread::spawn(move || {
        let _ = session_core::core::call(
            &tx,
            Op::PromptAndWait {
                message: "delegate".into(),
                images: Vec::new(),
            },
        );
    });
    wait_child_requests(&w, 1);
    let listed = subagents(&w);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["state"], "running");
    // What the TUI's views show besides the cell API's listing: the spawn name and the start (ms since the epoch).
    assert_eq!(listed[0]["name"], "auditor");
    assert!(
        listed[0]["startedAt"].as_f64().is_some_and(|at| at > 1e12),
        "{listed:?}"
    );
    let id = listed[0]["id"].as_str().unwrap().to_string();

    let sent = w
        .call(Op::SteerAgent {
            id: id.clone(),
            message: "Stop auditing tests; focus on src/".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        (&sent["state"], &sent["steered"]),
        (&json!("delivered"), &json!(true)),
        "{sent}"
    );
    assert_eq!(sent["duplicate"], false);
    release.send(()).unwrap();

    // The child's next request carries the envelope, from the root, with the text JSON-encoded.
    wait_child_requests(&w, 2);
    let steered = child_requests(&w)[1]["messages"].to_string();
    assert!(
        steered.contains("Verified sender: the root agent (your parent)"),
        "{steered}"
    );
    assert!(
        steered.contains(r#"Content (JSON string): \"Stop auditing tests; focus on src/\""#),
        "{steered}"
    );
    poll_until("the child ends", || subagents(&w)[0]["state"] == "completed");
    // The running-work panel's detached cells: none left.
    let detached = Op::Inspect {
        request: "rlm.detached".into(),
        payload: json!({}),
    };
    assert_eq!(w.call(detached).unwrap().unwrap(), json!({"cells": []}));
    // Its result is its answer after the steering, as an agent loop takes steering before it ends.
    assert_eq!(subagents(&w)[0]["result"]["value"], "child steered");

    let transcript = w.call(Op::AgentTranscript { id: id.clone() }).unwrap().unwrap();
    assert_eq!(transcript["id"], json!(id));
    assert_eq!(transcript["busy"], false);
    let lines: Vec<String> = transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let text = match &m["content"] {
                Value::String(text) => text.clone(),
                blocks => blocks
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect(),
            };
            format!(
                "{}: {}",
                m["role"].as_str().unwrap(),
                text.lines().next().unwrap_or_default()
            )
        })
        .collect();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].starts_with("user: CHILD-BRIEF: audit the tests"), "{lines:?}");
    assert_eq!(lines[1], "assistant: child first");
    assert_eq!(lines[2], "user: [Untrusted family message]");
    assert_eq!(lines[3], "assistant: child steered");

    let late = w.call(Op::SteerAgent {
        id: id.clone(),
        message: "late".into(),
    });
    assert!(late.as_ref().unwrap_err().contains("already finished"), "{late:?}");
    assert_eq!(
        w.call(Op::AgentTranscript { id: "nope".into() }),
        Err("No transcript for agent nope".into())
    );
    assert_eq!(
        w.call(Op::ReviveAgent { id: id.clone() }),
        Err(format!(
            "Subagent {id} is completed: only a subagent its worker left interrupted can be revived"
        ))
    );
}

#[test]
fn session_core_agent_hub_revives_an_interrupted_subagent_from_its_brief() {
    let home = Home::new(None);
    let w = home.open(script(None), &[], None);
    w.call(Op::PromptAndWait {
        message: "delegate".into(),
        images: Vec::new(),
    })
    .unwrap();
    poll_until("the child ends", || {
        subagents(&w).first().is_some_and(|s| s["state"] == "completed")
    });
    let id = subagents(&w)[0]["id"].as_str().unwrap().to_string();
    w.close();

    // As a worker that died while the child ran leaves it: its task journal says `interrupted`.
    {
        let sessions = home.root.join("agent/experimental/sessions");
        let repo = session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared());
        let metadata = repo.list(None).into_iter().next().unwrap();
        let mut session = repo.open(&metadata).unwrap();
        let mut journal = session.value("ultron.tasks", "root").cloned().unwrap();
        for task in journal["tasks"].as_array_mut().unwrap() {
            if task["id"] == json!(id) {
                task["state"] = json!("interrupted");
                task["result"] = json!({"status": "interrupted", "verification": "unverified",
                    "error": "Ultron task interrupted"});
            }
        }
        session
            .commit(&[session_journal::Write::set("ultron.tasks", "root", journal)])
            .unwrap();
    }

    let w = home.open(script(None), &[], None);
    let revived = w.call(Op::ReviveAgent { id: id.clone() }).unwrap().unwrap();
    assert_eq!(revived["revived"], json!(id), "{revived}");
    let new_id = revived["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, id);
    // It keeps the name it was spawned with, recorded with its task (and so with the revived one's, once it runs).
    assert_eq!(revived["name"], "auditor", "{revived}");
    wait_child_requests(&w, 1);
    assert!(
        child_requests(&w)[0]["messages"]
            .to_string()
            .contains("CHILD-BRIEF: audit the tests")
    );
    poll_until("the revived child ends", || {
        subagents(&w)
            .iter()
            .any(|s| s["id"] == json!(new_id) && s["state"] == "completed")
    });
    let names: Vec<Value> = home
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == session_core::subagents::NAMES_NAMESPACE && w["op"] == "set")
        .map(|w| json!([w["key"], w["value"]]))
        .collect();
    assert_eq!(names, [json!([id, "auditor"]), json!([new_id, "auditor"])]);
}

#[test]
fn session_core_agent_hub_ops_round_trip_as_json() {
    for (op, wire) in [
        (Op::Agents, json!({"op": "agents"})),
        (
            Op::KillAgent { id: "t1".into() },
            json!({"op": "killAgent", "id": "t1"}),
        ),
        (
            Op::SteerAgent {
                id: "t1".into(),
                message: "m".into(),
            },
            json!({"op": "steerAgent", "id": "t1", "message": "m"}),
        ),
        (
            Op::ReviveAgent { id: "t1".into() },
            json!({"op": "reviveAgent", "id": "t1"}),
        ),
        (
            Op::AgentTranscript { id: "t1".into() },
            json!({"op": "agentTranscript", "id": "t1"}),
        ),
    ] {
        assert_eq!(op.to_json(), wire);
        assert_eq!(Op::from_json(&wire).unwrap(), op);
    }
    assert_eq!(
        Op::from_json(&json!({"op": "steerAgent", "id": "t1"})).unwrap_err(),
        "message must be a string"
    );
    assert_eq!(
        Op::from_json(&json!({"op": "killAgent"})).unwrap_err(),
        "id must be a string"
    );
}
