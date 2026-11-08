//! The seat over a scripted transport and chat model: fallback order, escalation and review, limits, receipts and
//! the one retry a `retry-after` allows. No network.

use crate::fakes::{FakeChat, FakeTransport, d1_choice, fake_key, jev_yes_no, key, ok, status};
use blocking_http::TransportError;
use config::keys::{JudgeSettings, judge_settings};
use delegation::usage::{UsageLedger, UsageLimits};
use judge::chat::{default_escalate_model, family};
use judge::question::{Question, parse_questions};
use judge::seat::{Receipt, Rung, Seat, SeatDeps};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn settings(judge: Value) -> JudgeSettings {
    judge_settings(json!({"judge": judge}).as_object().unwrap())
}

fn yes_no() -> Vec<Question> {
    parse_questions(&json!({"q": {"type": "yesno", "instructions": "Does the code show the bug?"}})).unwrap()
}

fn choice() -> Vec<Question> {
    parse_questions(
        &json!({"q": {"type": "choice", "instructions": "Is the memory still true?",
        "criteria": {"supported": "still true", "stale": "partly outdated", "contradicted": "false"}}}),
    )
    .unwrap()
}

struct Rig {
    transport: Arc<FakeTransport>,
    chat: Arc<FakeChat>,
    receipts: Arc<Mutex<Vec<Receipt>>>,
    seat: Seat,
}

fn rig_with(judge: Value, jev_key: Option<&str>, chat: Vec<Result<Value, String>>, ledger: Option<UsageLedger>) -> Rig {
    let transport = Arc::new(FakeTransport::default());
    let chat = FakeChat::answering(chat);
    let receipts: Arc<Mutex<Vec<Receipt>>> = Arc::default();
    let log = receipts.clone();
    let mut deps = SeatDeps::new(transport.clone(), key(jev_key), key(Some("d1")));
    deps.chat = Some(chat.clone());
    deps.jev_url = "http://jev.test/v1/systemone".into();
    deps.ledger = ledger;
    deps.on_receipt = Some(Arc::new(move |r: &Receipt| log.lock().unwrap().push(r.clone())));
    let seat = Seat::new(settings(judge), deps);
    Rig {
        transport,
        chat,
        receipts,
        seat,
    }
}

fn rig(judge: Value) -> Rig {
    rig_with(judge, Some("jev"), Vec::new(), None)
}

const D1: &str = "http://d1.test:18091";

#[test]
fn judge_jev_answers_first_in_system_one_with_a_bearer_key() {
    let r = rig(json!({}));
    r.transport
        .script("/systemone", vec![ok(jev_yes_no(0.97, Some(0.0004)))]);
    let verdict = r
        .seat
        .decide("FINDING: x\nCODE: y", &yes_no(), Some("finding"), &Signal::new());
    assert_eq!(verdict.rung, Rung::First);
    assert_eq!(
        (verdict.backend.as_deref(), verdict.model.as_deref()),
        (Some("jev"), Some("jev-1.13"))
    );
    assert_eq!(verdict.answers[0].1.value, json!(true));
    assert!((verdict.answers[0].1.confidence - 0.97).abs() < 1e-9);
    let sent = r.transport.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].url, "http://jev.test/v1/systemone");
    assert!(
        sent[0]
            .headers
            .contains(&("authorization".into(), format!("Bearer {}", fake_key("jev"))))
    );
    assert_eq!(
        sent[0].body,
        json!({"state": "FINDING: x\nCODE: y", "model": "jev-latest",
            "questions": {"q": {"type": "noul", "instructions": "Does the code show the bug?"}}})
    );
    let json = verdict.to_json(Some("finding"));
    assert_eq!(
        (json["calls"].clone(), json["cost_usd"].clone()),
        (json!(1), json!(0.0004))
    );
    assert_eq!(json["receipts"][0]["input_tokens"], 400);
    // The receipt reached the session's usage rows, and never the key.
    assert_eq!(r.receipts.lock().unwrap().len(), 1);
    assert!(!json.to_string().contains(&fake_key("jev")));
}

#[test]
fn judge_falls_back_to_d1_without_a_jev_key_and_sends_nothing_to_jev() {
    let r = rig_with(json!({"d1": {"url": D1}}), None, Vec::new(), None);
    r.transport.script("/decide", vec![ok(d1_choice("stale", 0.93))]);
    let verdict = r.seat.decide("MEMORY: a\nEVIDENCE: b", &choice(), None, &Signal::new());
    assert_eq!((verdict.rung, verdict.backend.as_deref()), (Rung::First, Some("d1")));
    assert_eq!(verdict.answers[0].1.value, json!("stale"));
    assert_eq!(r.transport.urls(), [format!("{D1}/decide")]);
    let sent = &r.transport.sent()[0];
    assert_eq!(sent.body["text"], "MEMORY: a\nEVIDENCE: b");
    assert_eq!(sent.body["questions"]["q"]["criteria"]["stale"], "partly outdated");
    assert_eq!(verdict.receipts[0].status, "skipped");
    assert!(
        verdict.receipts[0]
            .error
            .as_deref()
            .unwrap()
            .contains("TYPESAFE_API_KEY")
    );
    // Only calls that went out are usage rows; d1 costs nothing.
    let rows = r.receipts.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].spend.cost, Some(0.0));
}

#[test]
fn judge_falls_back_on_errors_timeouts_and_bad_answers() {
    for failure in [
        status(500, None),
        status(401, None),
        Err(TransportError::Timeout),
        Err(TransportError::Failed),
        ok(json!({"answers": {"q": {"noul": 1.7}}})),
        ok(json!({"answers": {}})),
    ] {
        let r = rig(json!({"d1": {"url": D1}}));
        r.transport.script("/systemone", vec![failure]);
        r.transport
            .script("/decide", vec![ok(json!({"answers": {"q": {"noul": 0.1}}}))]);
        let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
        assert_eq!(verdict.backend.as_deref(), Some("d1"), "{:?}", verdict.receipts);
        assert_eq!(verdict.answers[0].1.value, json!(false));
        assert_eq!(verdict.receipts[0].status, "failed");
        assert_eq!(r.transport.sent().len(), 2, "no retry");
    }
}

#[test]
fn judge_retries_once_only_after_a_retry_after_and_each_try_has_a_receipt() {
    let r = rig(json!({"d1": {"url": D1}}));
    r.transport
        .script("/systemone", vec![status(503, Some("0")), ok(jev_yes_no(0.9, None))]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.backend.as_deref(), Some("jev"));
    let statuses: Vec<_> = verdict.receipts.iter().map(|r| r.status).collect();
    assert_eq!(statuses, ["failed", "succeeded"]);
    assert_eq!(verdict.receipts[0].error.as_deref(), Some("HTTP 503"));

    // Twice throttled: one retry, then d1.
    let r = rig(json!({"d1": {"url": D1}}));
    r.transport
        .script("/systemone", vec![status(429, Some("0")), status(429, Some("0"))]);
    r.transport
        .script("/decide", vec![ok(json!({"answers": {"q": {"noul": 0.95}}}))]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.backend.as_deref(), Some("d1"));
    assert_eq!(
        r.transport.urls().iter().filter(|u| u.ends_with("/systemone")).count(),
        2
    );

    // A wait over the cap, or an HTTP date, is not retried.
    for after in ["3600", "Wed, 21 Oct 2026 07:28:00 GMT"] {
        let r = rig(json!({"d1": {"url": D1}}));
        r.transport.script("/systemone", vec![status(429, Some(after))]);
        r.transport
            .script("/decide", vec![ok(json!({"answers": {"q": {"noul": 0.95}}}))]);
        r.seat.decide("s", &yes_no(), None, &Signal::new());
        assert_eq!(
            r.transport.urls().iter().filter(|u| u.ends_with("/systemone")).count(),
            1,
            "{after}"
        );
    }
}

#[test]
fn judge_falls_back_to_the_chat_model_then_abstains() {
    let mut r = rig_with(
        json!({"fallbackModel": "openai/gpt-5-mini", "d1": {"url": D1}}),
        Some("jev"),
        vec![Ok(json!({"q": {"value": true, "confidence": 0.8}}))],
        None,
    );
    r.transport.script("/systemone", vec![status(500, None)]);
    r.transport.script("/decide", vec![Err(TransportError::Timeout)]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(
        (verdict.rung, verdict.backend.as_deref(), verdict.model.as_deref()),
        (Rung::First, Some("chat"), Some("openai/gpt-5-mini"))
    );
    assert_eq!(r.chat.models(), ["openai/gpt-5-mini"]);
    // The chat rung's closed answer set is its `answer` tool's schema.
    let asked = r.chat.asked.lock().unwrap()[0].1.clone();
    let system = serde_json::to_string(&asked.messages[0]).unwrap();
    assert!(
        system.contains(r#""name":"answer""#) && system.contains(r#""type":"boolean""#),
        "{system}"
    );
    assert_eq!(verdict.to_json(None)["cost_usd"], json!(0.002));

    // Nothing configured: abstained, with the reason.
    r = rig_with(json!({"provider": []}), None, Vec::new(), None);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.rung, Rung::Abstained);
    assert!(verdict.answers.is_empty());
    assert!(verdict.reason.unwrap().contains("no judge backend is configured"));
    // Configured but failing: abstained after trying each.
    let r = rig(json!({"d1": {"url": D1}}));
    r.transport.script("/systemone", vec![status(500, None)]);
    r.transport.script("/decide", vec![status(500, None)]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(
        (verdict.rung, verdict.reason.as_deref()),
        (Rung::Abstained, Some("no judge backend answered"))
    );
    assert_eq!(verdict.to_json(None)["abstained"], true);
}

#[test]
fn judge_escalates_an_unsure_answer_to_another_family_else_marks_it_for_review() {
    // Jev at 0.6 is under its 0.75 threshold: the escalation model answers.
    let r = rig_with(
        json!({"escalateModel": "anthropic/claude-haiku-5-5"}),
        Some("jev"),
        vec![Ok(json!({"q": {"value": false, "confidence": 0.9}}))],
        None,
    );
    r.transport.script("/systemone", vec![ok(jev_yes_no(0.6, None))]);
    let verdict = r.seat.decide("s", &yes_no(), Some("finding"), &Signal::new());
    assert_eq!(verdict.rung, Rung::Escalated);
    assert_eq!(verdict.answers[0].1.value, json!(false));
    assert_eq!(verdict.model.as_deref(), Some("anthropic/claude-haiku-5-5"));
    assert!(verdict.receipts[1].escalation);
    assert_eq!(verdict.reason.as_deref(), Some("confidence under 0.75 (jev)"));

    // No escalation model (no setting, no session model): review, with Jev's answer.
    let r = rig(json!({}));
    r.transport.script("/systemone", vec![ok(jev_yes_no(0.6, None))]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.rung, Rung::Review);
    assert_eq!(verdict.answers[0].1.value, json!(true));
    assert_eq!(verdict.to_json(None)["review"], true);
    assert!(verdict.reason.unwrap().contains("no escalation model"));

    // The escalation model failing: review.
    let r = rig_with(
        json!({"escalateModel": "anthropic/claude-haiku-5-5"}),
        Some("jev"),
        vec![Err("connection refused".into())],
        None,
    );
    r.transport.script("/systemone", vec![ok(jev_yes_no(0.4, None))]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.rung, Rung::Review);
    assert!(verdict.reason.unwrap().contains("gave no answer"));
}

#[test]
fn judge_never_escalates_to_the_family_that_answered() {
    // The fallback model answered unsure (a chat threshold set); the escalation model is the same family: review.
    let r = rig_with(
        json!({"provider": [], "fallbackModel": "claude-code/claude-opus-5-5",
            "escalateModel": "anthropic/claude-haiku-5-5", "thresholds": {"chat": 0.9}}),
        None,
        vec![Ok(json!({"q": {"value": true, "confidence": 0.5}}))],
        None,
    );
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.rung, Rung::Review);
    assert!(verdict.reason.unwrap().contains("same family"));
    assert_eq!(r.chat.models(), ["claude-code/claude-opus-5-5"]);
    assert_eq!(family("claude-subscription/claude-opus-5-5"), "claude");
    assert_eq!(family("openai/gpt-5.5"), "gpt");
    assert_eq!(family("jev-1.13"), "jev");
}

#[test]
fn judge_thresholds_follow_the_kind_then_the_backend() {
    let r = rig(json!({"thresholds": {"jev": 0.5, "memory": {"jev": 0.95}}}));
    assert_eq!(r.seat.threshold(Some("memory"), "jev"), Some(0.95));
    assert_eq!(r.seat.threshold(Some("finding"), "jev"), Some(0.5));
    assert_eq!(r.seat.threshold(None, "d1"), Some(0.85));
    assert_eq!(r.seat.threshold(None, "chat"), None);
    r.transport
        .script("/systemone", vec![ok(jev_yes_no(0.9, None)), ok(jev_yes_no(0.9, None))]);
    assert_eq!(
        r.seat.decide("s", &yes_no(), Some("memory"), &Signal::new()).rung,
        Rung::Review
    );
    assert_eq!(
        r.seat.decide("s", &yes_no(), Some("finding"), &Signal::new()).rung,
        Rung::First
    );
}

#[test]
fn judge_default_escalation_model_is_the_cheapest_of_the_session_family() {
    assert_eq!(
        default_escalate_model("anthropic/claude-opus-5-5").as_deref(),
        Some("anthropic/claude-haiku-5-5")
    );
    // A subscription has no prices: the family's cheapest model, when the provider serves it.
    assert_eq!(
        default_escalate_model("claude-code/claude-opus-5-5").as_deref(),
        Some("claude-code/claude-haiku-5-5")
    );
    assert_eq!(
        default_escalate_model("claude-subscription/claude-opus-5-5").as_deref(),
        Some("claude-subscription/claude-haiku-5-5")
    );
    let openai = default_escalate_model("openai/gpt-5.5").unwrap();
    assert!(openai.starts_with("openai/gpt-"), "{openai}");
    assert_eq!(default_escalate_model("nonsense"), None);

    // From the session model when no escalateModel is set.
    let transport = Arc::new(FakeTransport::default());
    let mut deps = SeatDeps::new(transport, key(Some("jev")), key(None));
    deps.chat = Some(FakeChat::answering(Vec::new()));
    deps.session_model = Some("anthropic/claude-opus-5-5".into());
    let seat = Seat::new(settings(json!({})), deps);
    assert_eq!(seat.escalate_model().as_deref(), Some("anthropic/claude-haiku-5-5"));
}

#[test]
fn judge_budget_passes_over_paid_backends_once_spent() {
    let r = rig(json!({"budgetUsd": 0.001, "d1": {"url": D1}}));
    r.transport
        .script("/systemone", vec![ok(jev_yes_no(0.99, Some(0.002)))]);
    r.transport
        .script("/decide", vec![ok(json!({"answers": {"q": {"noul": 0.99}}}))]);
    assert_eq!(
        r.seat.decide("s", &yes_no(), None, &Signal::new()).backend.as_deref(),
        Some("jev")
    );
    let second = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(second.backend.as_deref(), Some("d1"));
    assert!(second.receipts[0].error.as_deref().unwrap().contains("budget"));
    assert_eq!(
        r.transport.urls().iter().filter(|u| u.ends_with("/systemone")).count(),
        1
    );
}

#[test]
fn judge_under_a_budget_jev_without_a_cost_is_used_once_then_passed_over() {
    let r = rig(json!({"budgetUsd": 5, "d1": {"url": D1}}));
    r.transport.script(
        "/systemone",
        vec![ok(jev_yes_no(0.99, None)), ok(jev_yes_no(0.99, None))],
    );
    r.transport
        .script("/decide", vec![ok(json!({"answers": {"q": {"noul": 0.99}}}))]);
    let first = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(first.backend.as_deref(), Some("jev"));
    assert_eq!(first.to_json(None)["cost_usd"], Value::Null);
    let second = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(second.backend.as_deref(), Some("d1"));
    assert!(second.receipts[0].error.as_deref().unwrap().contains("no cost"));
    // Without a budget an unpriced Jev keeps answering.
    let r = rig(json!({}));
    r.transport.script(
        "/systemone",
        vec![ok(jev_yes_no(0.99, None)), ok(jev_yes_no(0.99, None))],
    );
    for _ in 0..2 {
        assert_eq!(
            r.seat.decide("s", &yes_no(), None, &Signal::new()).backend.as_deref(),
            Some("jev")
        );
    }
}

#[test]
fn judge_call_ceiling_stops_the_seat() {
    let r = rig(json!({"maxCalls": 1, "d1": {"url": D1}}));
    r.transport.script("/systemone", vec![ok(jev_yes_no(0.99, None))]);
    r.seat.decide("s", &yes_no(), None, &Signal::new());
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.rung, Rung::Abstained);
    assert!(verdict.reason.unwrap().contains("judge.maxCalls"));
    assert_eq!(r.transport.sent().len(), 1);
}

#[test]
fn judge_settles_every_call_in_the_usage_ledger() {
    let ledger = UsageLedger::in_memory(UsageLimits::default());
    let r = rig_with(
        json!({"d1": {"url": D1}}),
        Some("jev"),
        Vec::new(),
        Some(ledger.clone()),
    );
    r.transport.script("/systemone", vec![status(500, None)]);
    r.transport
        .script("/decide", vec![ok(json!({"answers": {"q": {"noul": 0.99}}}))]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    let document = ledger.document().unwrap();
    let calls = document["roots"][delegation::usage::DEFAULT_ROOT_ID]["calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(calls.len(), 2, "{document}");
    for (call, receipt) in calls.iter().zip(&verdict.receipts) {
        assert_eq!(call["kind"], "model");
        assert_eq!(call["requestKey"], receipt.id.as_str());
        assert!(receipt.id.starts_with("judge:"));
    }
    assert_eq!(calls[0]["status"], "failed");
    assert_eq!(calls[1]["status"], "succeeded");
    assert_eq!(calls[1]["usage"]["cost"], 0.0);
}

#[test]
fn judge_stops_when_the_session_ledger_refuses_and_skips_jev_under_its_cost_cap() {
    let limits = UsageLimits {
        max_cost_usd: Some(0.001),
        ..UsageLimits::default()
    };
    let ledger = UsageLedger::in_memory(limits);
    let r = rig_with(
        json!({"d1": {"url": D1}, "fallbackModel": "openai/gpt-5-mini"}),
        Some("jev"),
        vec![Ok(json!({"q": {"value": true, "confidence": 0.99}}))],
        Some(ledger),
    );
    r.transport.script("/decide", vec![status(500, None)]);
    // Jev is passed over (it reports no cost); d1 fails; the chat model's $0.002 goes over the cap.
    let first = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(first.backend.as_deref(), Some("chat"));
    assert!(first.receipts[0].error.as_deref().unwrap().contains("max_cost_usd"));
    assert!(r.transport.urls().iter().all(|u| !u.ends_with("/systemone")));
    let second = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(second.rung, Rung::Abstained);
    assert!(second.reason.unwrap().contains("cost cap"));
}

#[test]
fn judge_d1_waits_for_a_cold_start_then_answers_warm() {
    let r = rig_with(json!({"provider": ["d1"], "d1": {"url": D1}}), None, Vec::new(), None);
    let answer = || ok(json!({"answers": {"q": {"noul": 0.99}}}));
    r.transport.script(
        "/decide",
        vec![answer(), answer(), Err(TransportError::Timeout), answer()],
    );
    for _ in 0..4 {
        r.seat.decide("s", &yes_no(), None, &Signal::new());
    }
    let timeouts: Vec<Duration> = r.transport.sent().iter().map(|s| s.timeout).collect();
    assert_eq!(
        timeouts,
        [15, 3, 3, 15].map(Duration::from_secs),
        "cold, warm, warm (times out), cold again"
    );
}

#[test]
fn judge_abort_ends_the_decision() {
    let r = rig(json!({"d1": {"url": D1}}));
    r.transport.script("/systemone", vec![Err(TransportError::Aborted)]);
    let verdict = r.seat.decide("s", &yes_no(), None, &Signal::new());
    assert_eq!(
        (verdict.rung, verdict.reason.as_deref()),
        (Rung::Abstained, Some("aborted"))
    );
    assert_eq!(r.transport.sent().len(), 1, "d1 is not asked after an abort");
}
