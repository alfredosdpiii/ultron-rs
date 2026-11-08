//! Automatic instruction refinements (session_core::auto_refine; Rust-only, no TypeScript reference): a session
//! actor against a scripted provider on loopback. After a compaction, the side request's proposal whose quote is in
//! the summary is activated in the session's refinements ledger and opens the next user run as an
//! `ultron.refinements` message; one whose quote is not is never stored; with `refine.auto` off neither happens.

use crate::p_session_support;

use p_session_support::{HANG, Setup, actor, text, user_texts};
use serde_json::{Value, json};
use session_core::core::Op;
use std::time::{Duration, Instant};

const SUMMARY: &str = "## Goal\nShip the parser.\n\n## Progress\nThe user corrected the agent: always run scripts/nextest.sh, never cargo test.";
const REAL: &str = "Run tests with scripts/nextest.sh only.";
const FAKE: &str = "Never touch the database migrations.";

fn system(body: &Value) -> String {
    body["system"].to_string()
}

/// Summary requests get [`SUMMARY`]; the refinement request gets one proposal quoting it and one inventing a quote.
fn script() -> p_session_support::Script {
    Box::new(|body: &Value, index: usize| {
        if system(body).contains("summarization") {
            text(SUMMARY)
        } else if system(body).contains("propose small, durable instructions") {
            let proposal = |text: &str, quote: &str| {
                json!({"text": text, "quote": quote, "rationale": "the user corrected it",
                    "expectedOutcome": "no wasted test runs"})
            };
            text(
                &json!({"proposals": [
                    proposal(REAL, "always run scripts/nextest.sh, never cargo test"),
                    proposal(FAKE, "the agent broke the database migrations"),
                ], "rollback": []})
                .to_string(),
            )
        } else {
            text(&format!("Answer {index}."))
        }
    })
}

/// The refinement events the session's ledger holds (its keyed document in the journal), newest document.
fn ledger_events(a: &p_session_support::Actor) -> Vec<Value> {
    a.writes()
        .into_iter()
        .rfind(|w| w["op"] == "set" && w["value"]["formatVersion"] == 1 && w["value"]["events"].is_array())
        .map(|w| w["value"]["events"].as_array().unwrap().clone())
        .unwrap_or_default()
}

fn compact_then_prompt(settings: Option<Value>, wait_for_activation: bool) -> (p_session_support::Actor, Vec<Value>) {
    let a = actor(
        script(),
        Setup {
            settings,
            vars: vec![(session_core::lane::CLOCK_CONTEXT_ENV, "off".into())],
            ..Setup::default()
        },
    );
    assert_eq!(a.prompt("start the parser work")["error"], Value::Null);
    a.call(Op::Compact { instructions: None }).unwrap();
    if wait_for_activation {
        // The pass runs in the background: wait for its activation (a hang guard, never a pass condition).
        let end = Instant::now() + HANG;
        while !ledger_events(&a).iter().any(|e| e["action"] == "activate") {
            assert!(Instant::now() < end, "no refinement was activated");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(a.prompt("now finish the parser please")["error"], Value::Null);
    let requests = a.requests();
    (a, requests)
}

fn conversation(requests: &[Value]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|r| !system(r).contains("summarization") && !system(r).contains("propose small, durable"))
        .collect()
}

#[test]
fn session_core_auto_refine_activates_a_quoted_proposal_and_delivers_it_to_the_main_lane() {
    let (a, requests) = compact_then_prompt(None, true);
    let events = ledger_events(&a);
    let proposed: Vec<&Value> = events.iter().filter(|e| e["action"] == "propose").collect();
    assert_eq!(proposed.len(), 1, "the invented quote is not stored: {events:#?}");
    let proposal = &proposed[0]["proposal"];
    assert_eq!(proposal["kind"], "instruction");
    assert!(proposal["target"].as_str().unwrap().starts_with("instruction:main."));
    assert_eq!(proposal["content"], REAL);
    assert_eq!(
        proposal["evidence"][0]["quote"],
        "always run scripts/nextest.sh, never cargo test"
    );
    assert!(
        proposal["evidence"][0]["ref"]
            .as_str()
            .unwrap()
            .starts_with("compaction:")
    );
    let id = proposed[0]["id"].clone();
    assert!(events.iter().any(|e| e["action"] == "activate" && e["id"] == id));
    let last = conversation(&requests).last().copied().unwrap().clone();
    let texts: Vec<String> = user_texts(&last)
        .iter()
        .map(|t| t.as_str().unwrap_or_default().to_string())
        .collect();
    let delivered = texts
        .iter()
        .find(|t| t.starts_with("[refinements]"))
        .expect("no refinements message");
    assert!(delivered.contains(REAL), "{delivered}");
    assert!(delivered.contains(id.as_str().unwrap()), "{delivered}");
    assert!(!texts.iter().any(|t| t.contains(FAKE)));
    // The first run had nothing to show.
    let first = conversation(&requests)[0].clone();
    assert!(!first["messages"].to_string().contains("[refinements]"));
}

#[test]
fn session_core_auto_refine_off_runs_the_unmodified_harness() {
    let (a, requests) = compact_then_prompt(Some(json!({"refine": {"auto": false}})), false);
    assert!(
        !requests.iter().any(|r| system(r).contains("propose small, durable")),
        "no refinement request"
    );
    assert!(ledger_events(&a).is_empty());
    assert!(
        !requests
            .iter()
            .any(|r| r["messages"].to_string().contains("[refinements]"))
    );
}
