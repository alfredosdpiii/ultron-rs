//! `ultron judge bench` over the judgebench cases (tests/data/judgebench.json) with fake backends that answer from
//! the cases' labels: accuracy per kind, the wrong answers, a backend stopping at its first failure, the call
//! ceiling.

use crate::fakes::key;
use blocking_http::{HttpRequest, HttpResponse, Transport, TransportError};
use config::keys::judge_settings;
use judge::bench;
use judge::seat::{Seat, SeatDeps};
use kernel::Signal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CASES: &str = include_str!("data/judgebench.json");

/// Answers each case with its gold label (Jev gets the first memory case wrong); d1 fails after `d1_answers`.
struct Oracle {
    gold: HashMap<String, Value>,
    d1_answers: Mutex<usize>,
}

impl Transport for Oracle {
    fn send(&self, request: HttpRequest, _: usize, _: Duration, _: &Signal) -> Result<HttpResponse, TransportError> {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        let jev = request.url.ends_with("/systemone");
        let text = body[if jev { "state" } else { "text" }].as_str().unwrap();
        if !jev {
            let mut left = self.d1_answers.lock().unwrap();
            if *left == 0 {
                return Err(TransportError::Timeout);
            }
            *left -= 1;
        }
        let (name, question) = body["questions"].as_object().unwrap().iter().next().unwrap();
        let gold = &self.gold[text];
        let answer = match question["type"].as_str().unwrap() {
            "noul" => json!({"noul": if gold == true { 0.9 } else { 0.1 }}),
            _ => {
                let wrong = jev && text.starts_with("MEMORY: Tests must run");
                let choice = if wrong { "stale" } else { gold.as_str().unwrap() };
                json!({"choice": choice, "confidence": 0.8})
            }
        };
        let mut reply = json!({"answers": {name.clone(): answer}});
        if jev {
            reply["model"] = json!("jev-1.13");
            reply["usage"] = json!({"input_tokens": 400, "output_tokens": 40});
        }
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: reply.to_string().into_bytes(),
        })
    }
}

fn seat(d1_answers: usize, judge: Value) -> Seat {
    let file: Value = serde_json::from_str(CASES).unwrap();
    let gold = file["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["text"].as_str().unwrap().to_string(), c["gold"].clone()))
        .collect();
    let oracle = Arc::new(Oracle {
        gold,
        d1_answers: Mutex::new(d1_answers),
    });
    let mut deps = SeatDeps::new(oracle, key(Some("jev")), key(Some("d1")));
    deps.jev_url = "http://jev.test/v1/systemone".into();
    let mut judge = judge;
    judge["d1"] = json!({"url": "http://d1.test"});
    Seat::new(judge_settings(json!({"judge": judge}).as_object().unwrap()), deps)
}

#[test]
fn judge_bench_reports_accuracy_per_kind_and_the_wrong_answers() {
    let cases = bench::load(CASES).unwrap();
    assert_eq!(cases.cases.len(), 35);
    let backends = ["jev".to_string(), "d1".to_string()];
    let runs = bench::run(&seat(usize::MAX, json!({})), &cases, &backends, None, &Signal::new());
    let lines = bench::report(&runs);
    assert!(
        lines[0].starts_with("jev (jev-1.13): accuracy 34/35 finding 10/10 memory 14/15 relation 10/10 | latency p50 "),
        "{lines:?}"
    );
    assert!(lines[0].ends_with("| tokens 15400 | cost unknown"), "{lines:?}");
    assert_eq!(lines[1], "   wrong memory: gold supported got stale conf 0.80");
    assert!(lines[2].starts_with("d1 (d1): accuracy 35/35 "), "{lines:?}");
    assert!(lines[2].ends_with("| cost $0.0000"), "{lines:?}");
}

#[test]
fn judge_bench_stops_a_backend_at_its_first_failure_and_honours_the_ceiling() {
    let cases = bench::load(CASES).unwrap();
    let runs = bench::run(&seat(3, json!({})), &cases, &["d1".to_string()], None, &Signal::new());
    let lines = bench::report(&runs);
    assert!(lines[0].starts_with("d1 (d1): accuracy 3/3 "), "{lines:?}");
    assert_eq!(lines.last().unwrap(), "   stopped: timed out after 3 s");

    let runs = bench::run(
        &seat(usize::MAX, json!({"maxCalls": 5})),
        &cases,
        &["jev".to_string()],
        Some(10),
        &Signal::new(),
    );
    let lines = bench::report(&runs);
    assert!(lines[0].contains("accuracy 4/5"), "{lines:?}");
    assert!(lines.last().unwrap().contains("judge.maxCalls"), "{lines:?}");
}

#[test]
fn judge_bench_files_are_checked() {
    for (text, error) in [
        ("[", "not JSON"),
        (
            r#"{"questions": {"q": {"type": "yesno", "instructions": "x"}}, "cases": []}"#,
            "nonempty list",
        ),
        (
            r#"{"questions": {"q": {"type": "yesno", "instructions": "x"}}, "cases": [{"q": "other", "gold": true, "text": "t"}]}"#,
            "cases[0]",
        ),
    ] {
        let got = bench::load(text).err().unwrap();
        assert!(got.contains(error), "{text}: {got}");
    }
}
