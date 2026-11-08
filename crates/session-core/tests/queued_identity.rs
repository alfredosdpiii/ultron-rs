//! A queued message is matched to the message the durable runtime placed by its submission, never by its text: two
//! queued messages that say the same thing keep their own entry ids, kinds and order.

use crate::p_session_support;
use crate::reopen_support;

use p_session_support::{in_order, text, tool};
use protocol::watch::QueueKind;
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::Op;

fn enqueue(w: &reopen_support::Worker, kind: QueueKind, text: &str) -> String {
    w.call(Op::Enqueue {
        kind,
        message: text.into(),
        images: Vec::new(),
        entry_id: None,
    })
    .unwrap()
    .unwrap()
    .as_str()
    .unwrap()
    .to_string()
}

/// The message entries the journal inserted, in order: `(id, text)`.
fn entries(home: &Home) -> Vec<(String, String)> {
    home.writes()
        .into_iter()
        .filter(|w| w["kind"] == "entry" || w.get("entry").is_some() || w["type"] == "message")
        .filter_map(|w| {
            let entry = w.get("entry").unwrap_or(&w);
            let text = entry["message"]["content"]
                .as_array()?
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<String>();
            Some((entry["id"].as_str()?.to_string(), text))
        })
        .collect()
}

#[test]
fn session_core_queued_messages_with_the_same_text_keep_their_own_entries() {
    let home = Home::new(None);
    let go = home.cwd.join("go");
    let cell = "import os, time\nwhile not os.path.exists('go'):\n    time.sleep(0.01)\nprint('went')";
    let w = home.open(in_order(vec![tool("toolu_1", cell), text("A."), text("B.")]), &[], None);
    let tx = w.tx.clone();
    let (answer, answered) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = answer.send(session_core::core::call(
            &tx,
            Op::PromptAndWait {
                message: "work".into(),
                images: Vec::new(),
            },
        ));
    });
    w.wait_event("tool_start");
    // The follow-up is queued first; the steer, saying the same, is placed first (at the next boundary).
    let follow_up = enqueue(&w, QueueKind::FollowUp, "same");
    let steer = enqueue(&w, QueueKind::Steer, "same");
    std::fs::write(&go, "").unwrap();
    let ended = answered
        .recv_timeout(p_session_support::HANG)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let same: Vec<String> = entries(&home)
        .into_iter()
        .filter(|(_, text)| text == "same")
        .map(|(id, _)| id)
        .collect();
    assert_eq!(same, [steer.clone(), follow_up.clone()]);
    // Once the steer was placed, the queue held the follow-up, never the steer alone.
    let updates = w.events("queue_update");
    assert!(
        updates.iter().all(|update| update["queues"]
            .as_array()
            .is_none_or(|q| q.len() != 1 || q[0]["entryId"] != json!(steer))),
        "{updates:?}"
    );
}
