//! `judge.decide` as the REPL sends it: payload checks, the verdict's keys, and a seat rebuilt for changed settings
//! keeping the session's call count. Also the settings reader and the services' keys.

use crate::fakes::{FakeTransport, jev_yes_no, key, ok};
use config::keys::{JudgeSettings, judge_settings};
use judge::JudgeModule;
use judge::seat::{Seat, SeatDeps};
use kernel::Signal;
use kernel_apis::{ApiModule, Request, RootOnly};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

fn call(module: &JudgeModule, payload: Value) -> Result<Value, String> {
    let host = RootOnly {
        now: kernel_apis::util::system_clock(),
    };
    let payload = payload.as_object().unwrap().clone();
    module.handle(&Request {
        kind: "judge.decide",
        payload: &payload,
        lane: "main",
        caller_task: None,
        host: &host,
        signal: &Signal::new(),
    })
}

fn module(settings: Arc<Mutex<Value>>, transport: Arc<FakeTransport>) -> JudgeModule {
    JudgeModule::new(
        Arc::new(move || judge_settings(json!({"judge": settings.lock().unwrap().clone()}).as_object().unwrap())),
        Arc::new(move |settings: JudgeSettings| {
            let mut deps = SeatDeps::new(transport.clone(), key(Some("jev")), key(None));
            deps.jev_url = "http://jev.test/v1/systemone".into();
            Ok(Seat::new(settings, deps))
        }),
    )
}

const QUESTIONS: &str = r#"{"q": {"type": "yesno", "instructions": "Shown?"}}"#;

#[test]
fn judge_decide_checks_its_payload() {
    let m = module(Arc::new(Mutex::new(json!({}))), Arc::default());
    let questions: Value = serde_json::from_str(QUESTIONS).unwrap();
    for (payload, error) in [
        (
            json!({"state": "s", "questions": questions, "extra": 1}),
            "Unknown payload field: extra",
        ),
        (json!({"questions": questions}), "state must be a string or JSON value"),
        (json!({"state": "  ", "questions": questions}), "must not be empty"),
        (json!({"state": "s", "questions": {}}), "1 to 16"),
        (
            json!({"state": "s", "questions": questions, "kind": "Memory!"}),
            "kind must be a short lowercase name",
        ),
        (
            json!({"state": "x".repeat(100_001), "questions": questions}),
            "over 100000 characters",
        ),
        (
            json!({"state": "s", "questions": questions, "expect": {"other": true}}),
            "expect names no question: other",
        ),
        (
            json!({"state": "s", "questions": questions, "expect": [true]}),
            "expect must be {question: answer}",
        ),
    ] {
        let got = call(&m, payload).unwrap_err();
        assert!(got.contains(error), "{got}");
    }
}

#[test]
fn judge_decide_returns_the_verdict_and_keeps_the_call_count_across_a_settings_change() {
    let settings = Arc::new(Mutex::new(json!({"maxCalls": 1})));
    let transport = Arc::new(FakeTransport::default());
    transport.script("/systemone", vec![ok(jev_yes_no(0.95, None))]);
    let m = module(settings.clone(), transport.clone());
    let questions: Value = serde_json::from_str(QUESTIONS).unwrap();
    let verdict = call(
        &m,
        json!({"state": {"claim": "a", "evidence": "b"}, "questions": questions, "kind": "finding"}),
    )
    .unwrap();
    let keys: Vec<&str> = verdict.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "answers",
            "backend",
            "model",
            "rung",
            "abstained",
            "review",
            "settled",
            "receipt",
            "reason",
            "kind",
            "calls",
            "cost_usd",
            "receipts"
        ]
    );
    assert_eq!(verdict["answers"]["q"]["value"], true);
    assert_eq!(
        (verdict["rung"].clone(), verdict["kind"].clone()),
        (json!("first"), json!("finding"))
    );
    // A JSON state is sent as its text.
    assert_eq!(transport.sent()[0].body["state"], r#"{"claim":"a","evidence":"b"}"#);
    // A changed setting builds a new seat; the session's one call is still spent.
    settings.lock().unwrap()["model"] = json!("jev-1.13");
    let verdict = call(&m, json!({"state": "s", "questions": questions})).unwrap();
    assert_eq!(verdict["abstained"], true);
    assert!(verdict["reason"].as_str().unwrap().contains("judge.maxCalls"));
    assert_eq!(transport.sent().len(), 1);
}

#[test]
fn judge_settings_read_the_global_judge_object() {
    let defaults = judge_settings(&Map::new());
    assert_eq!(defaults.providers, ["jev", "d1"]);
    assert_eq!((defaults.model.as_str(), defaults.d1_url.clone()), ("jev-latest", None));
    assert_eq!((defaults.budget_usd, defaults.max_calls), (None, 1000));
    let read = judge_settings(
        json!({"judge": {
            "provider": ["d1", "nope", "d1", "jev"],
            "d1": {"url": " http://mini:18091/ "},
            "model": "jev-1.13",
            "escalateModel": "anthropic/claude-haiku-5-5",
            "fallbackModel": "not a model",
            "thresholds": {"jev": 0.8, "d1": 2, "memory": {"jev": 0.9, "d1": "high"}, "bad": {"jev": -1}},
            "budgetUsd": 0.5,
            "maxCalls": 20,
        }})
        .as_object()
        .unwrap(),
    );
    assert_eq!(read.providers, ["d1", "jev"]);
    assert_eq!(read.d1_url.as_deref(), Some("http://mini:18091"));
    assert_eq!(read.model, "jev-1.13");
    assert_eq!(read.escalate_model.as_deref(), Some("anthropic/claude-haiku-5-5"));
    assert_eq!(read.fallback_model, None);
    assert_eq!(
        Value::Object(read.thresholds),
        json!({"jev": 0.8, "memory": {"jev": 0.9}})
    );
    assert_eq!((read.budget_usd, read.max_calls), (Some(0.5), 20));
}
