//! `HindsightLessons` replayed against the fake Hindsight server: the requests autoreview's lessons make must equal
//! TypeScript's (the `lessons` section of crates/hindsight/fixtures/exchanges.json, recorded by its generate.mjs).

use crate::common;

use common::{FakeHindsight, Normalizer, closed_port, fill, fixture, strings};
use review::ledger::{Fate, Lesson};
use review::lessons::HindsightLessons;
use serde_json::json;

#[test]
fn review_lessons_match_typescript() {
    let fixture = fixture();
    let expected = &fixture["lessons"];
    let transport = strings(&fixture["transportHeaders"]);
    let server = FakeHindsight::start();
    let closed = closed_port();
    server.reset();
    let normalizer = Normalizer::new(server.port, closed);
    let fates: Vec<Fate> = serde_json::from_value(expected["fates"].clone()).unwrap();
    let lessons: Vec<Lesson> = expected["lessons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| Lesson {
            category: l["category"].as_str().unwrap().into(),
            kind: l["kind"].as_str().unwrap().into(),
            accepted: l["accepted"].as_u64().unwrap() as u32,
            rejected: l["rejected"].as_u64().unwrap() as u32,
            lowered: l["lowered"].as_bool().unwrap(),
        })
        .collect();
    let mut cases = Vec::new();
    for case in expected["cases"].as_array().unwrap() {
        let base = fill(&case["baseUrl"], &server.base(), closed);
        let memory = HindsightLessons::new(base.as_str().unwrap(), Box::new(|| 1_760_000_000_000), None).unwrap();
        let mut record = json!({"name": case["name"], "baseUrl": case["baseUrl"], "repo": case["repo"]});
        if let Err(error) = memory.retain(case["repo"].as_str().unwrap(), &fates, &lessons) {
            record["error"] = json!(error.code.as_str());
            // What the reviewer logs: the error's message.
            assert_eq!(
                error.to_string(),
                format!("Memory operation did not complete ({})", error.code)
            );
        }
        cases.push(record);
    }
    let record = normalizer.apply(&json!({"cases": cases, "requests": server.requests(&transport)}));
    assert_eq!(record["cases"], expected["cases"]);
    assert_eq!(record["requests"], expected["requests"]);
}
