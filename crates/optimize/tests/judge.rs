//! The judge pipeline offline: the real seat answered from the frozen judgebench recording, points per variant,
//! threshold fitting and its proposal, escalation estimates, and a live run's recording replayed.

use blocking_http::{HttpRequest, HttpResponse, Transport, TransportError};
use judge::seat::{Seat, SeatDeps};
use kernel::Signal;
use optimize::Outcome;
use optimize::judge::{
    CASES, JudgeSet, RECORDING, Recorder, Recording, Replay, check_config, decide_all, default_variants, fit, point,
    proposal, replay_seat,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn set() -> Arc<JudgeSet> {
    Arc::new(JudgeSet::load(CASES).unwrap())
}

fn replay(set: &Arc<JudgeSet>) -> Arc<Replay> {
    Arc::new(Replay::new(set.clone(), Recording::parse(RECORDING).unwrap()).unwrap())
}

fn measure(config: Value) -> optimize::Point {
    let set = set();
    let replay = replay(&set);
    let verdicts = decide_all(&replay_seat(&config, replay.clone()), &set, &Signal::new());
    point(&set, &config, &verdicts, &replay.take_ms())
}

/// The defaults of the seat (Jev 0.75, d1 0.85).
fn defaults(_: &str, backend: &str) -> Option<f64> {
    judge::seat::default_threshold(backend)
}

#[test]
fn the_judge_set_and_its_recording_are_frozen() {
    let set = set();
    let recording = Recording::parse(RECORDING).unwrap();
    assert_eq!(
        set.hash, "0f794299114ec621911b293a7cb2244f977df891401dd50ceb7f1ba5399cb0f9",
        "judgebench.json changed: a frozen set is never edited (add a new set instead)"
    );
    assert_eq!(recording.set_hash, set.hash);
    assert_eq!(recording.calls.len(), 70);
    recording.check(&set).unwrap();
    // The recording round-trips through its own format.
    let again = Recording::parse(&recording.to_json().to_string()).unwrap();
    assert_eq!(again.calls, recording.calls);
}

#[test]
fn judge_variants_replay_the_recording_through_the_real_seat() {
    let d1 = measure(json!({"provider": ["d1"]}));
    assert_eq!(d1.outcome, Outcome::Passed, "{:?}", d1.failures);
    assert_eq!(d1.metrics["wrong"], 0);
    let (correct, review) = (
        d1.metrics["correct"].as_u64().unwrap(),
        d1.metrics["review"].as_u64().unwrap(),
    );
    assert_eq!(correct + review, 35);
    assert!((d1.quality - correct as f64 / 35.0).abs() < 1e-9);
    assert_eq!(d1.cost_usd, Some(0.0), "d1 is free");
    assert!(
        d1.metrics["latencyMs"]["p50"].as_f64().unwrap() > 100.0,
        "{}",
        d1.metrics
    );

    let jev = measure(json!({"provider": ["jev"]}));
    assert_eq!(jev.outcome, Outcome::Passed, "{:?}", jev.failures);
    assert_eq!(jev.cost_usd, None, "Jev reports no cost");
    assert!(jev.metrics["tokens"].as_u64().unwrap() > 10_000);
}

#[test]
fn a_threshold_that_accepts_wrong_answers_fails_and_keeps_them() {
    let point = measure(json!({"provider": ["jev"], "thresholds": {"jev": 0.0}}));
    assert_eq!(point.outcome, Outcome::Failed);
    assert_eq!(point.penalty, 0.0, "nothing left unsettled");
    let wrong = point.metrics["wrong"].as_u64().unwrap();
    assert!(wrong >= 1);
    assert_eq!(point.failures.len() as u64, wrong);
    assert_eq!(point.failures[0]["failure"], "accepted a wrong answer");
    assert!(point.failures[0]["gold"] != point.failures[0]["got"]);
}

#[test]
fn fitted_thresholds_accept_no_recorded_wrong_answer_and_are_only_proposed() {
    let set = set();
    let recording = Recording::parse(RECORDING).unwrap();
    let fits = fit(&set, &recording, &defaults);
    assert_eq!(fits.len(), 6, "three kinds by two backends");
    for f in &fits {
        assert!(f.answers >= 10, "{f:?}");
        match (f.max_wrong, f.fitted) {
            (Some(max), Some(t)) => {
                assert!(t > max && t - max <= 0.011, "{f:?}");
                assert!(f.note.contains("accepts none"), "{f:?}");
            }
            (None, fitted) => {
                assert_eq!(fitted, None);
                assert!(f.note.contains("no wrong answer"), "{f:?}");
            }
            (Some(_), None) => assert!(f.note.contains("full confidence"), "{f:?}"),
        }
    }
    let document = proposal(&set, &recording, &fits, "2026-11-08T00:00:00.000Z");
    assert_eq!(document["proposal"], "judge.thresholds");
    assert!(document["apply"].as_str().unwrap().contains("nothing was changed"));
    assert_eq!(document["rows"].as_array().unwrap().len(), 6);
    let thresholds = document["thresholds"].as_object().unwrap();
    assert!(!thresholds.is_empty(), "the recording has wrong answers to fit around");

    // The built-in variants: each backend, and each with its fitted thresholds; the fitted ones pass.
    let variants = default_variants(&fits);
    let names: Vec<&str> = variants.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.starts_with(&["jev"]) && names.contains(&"d1"), "{names:?}");
    for (name, config) in variants.iter().filter(|(n, _)| n.ends_with("-fitted")) {
        check_config(config).unwrap();
        let point = measure(config.clone());
        assert_eq!(point.outcome, Outcome::Passed, "{name}: {:?}", point.failures);
    }
}

#[test]
fn too_few_answers_are_not_fitted() {
    let set = set();
    let mut recording = Recording::parse(RECORDING).unwrap();
    recording.calls.truncate(8);
    let fits = fit(&set, &recording, &defaults);
    assert!(
        fits.iter().all(|f| f.fitted.is_none() && f.note.contains("too few")),
        "{fits:?}"
    );
}

#[test]
fn an_escalation_model_is_estimated_from_the_catalog_not_replayed() {
    let point = measure(json!({"provider": ["d1"], "escalateModel": "anthropic/claude-haiku-5-5"}));
    let review = point.metrics["review"].as_u64().unwrap();
    assert!(review > 0);
    assert_eq!(point.metrics["escalation"]["wouldEscalate"], review);
    let estimate = point.metrics["escalation"]["estimatedCostUsd"].as_f64().unwrap();
    assert!(estimate > 0.0);
    assert_eq!(point.cost_usd, Some(estimate), "d1 is free; the cost is the estimate");
    assert!(point.metrics["note"].as_str().unwrap().contains("would escalate"));
}

#[test]
fn a_case_missing_from_the_recording_falls_back_or_abstains() {
    let set = set();
    let mut recording = Recording::parse(RECORDING).unwrap();
    recording.calls.retain(|c| !(c.case == 0 && c.backend == "jev"));
    let replay = Arc::new(Replay::new(set.clone(), recording.clone()).unwrap());
    let alone = decide_all(
        &replay_seat(&json!({"provider": ["jev"]}), replay.clone()),
        &set,
        &Signal::new(),
    );
    let alone = point(&set, &json!({}), &alone, &[]);
    assert_eq!(alone.outcome, Outcome::Incomplete);
    assert_eq!(alone.metrics["abstained"], 1);
    assert_eq!(alone.failures[0]["failure"], "abstained");
    let both = decide_all(
        &replay_seat(&json!({"provider": ["jev", "d1"]}), replay),
        &set,
        &Signal::new(),
    );
    assert_eq!(both[0].backend.as_deref(), Some("d1"), "d1 answers what Jev did not");
}

#[test]
fn a_recording_of_another_set_is_refused() {
    let set = set();
    let mut recording = Recording::parse(RECORDING).unwrap();
    recording.set_hash = "0".repeat(64);
    let error = Replay::new(set.clone(), recording).err().unwrap();
    assert!(error.contains("another case set"), "{error}");
    for (text, expected) in [
        ("[", "not JSON"),
        (r#"{"calls": []}"#, "setHash"),
        (
            r#"{"setHash": "x", "calls": [{"backend": "gpt", "case": 0, "answer": {}}]}"#,
            "calls[0]",
        ),
    ] {
        let error = Recording::parse(text).err().unwrap();
        assert!(error.contains(expected), "{text}: {error}");
    }
}

#[test]
fn judge_variant_configs_are_checked() {
    check_config(&json!({"provider": ["jev", "d1"], "thresholds": {"memory": {"jev": 0.8}}})).unwrap();
    for (config, expected) in [
        (json!([]), "an object"),
        (json!({"budgetUsd": 1}), "not budgetUsd"),
        (json!({"provider": ["gpt"]}), "provider"),
        (json!({"provider": []}), "provider"),
        (json!({"thresholds": {"jev": 2}}), "thresholds"),
        (json!({"escalateModel": "haiku"}), "provider/model"),
    ] {
        let error = check_config(&config).err().unwrap();
        assert!(error.contains(expected), "{config}: {error}");
    }
}

/// Answers like Jev and d1 do: every case `supported` / yes, with a fixed confidence.
struct Live;

impl Transport for Live {
    fn send(&self, request: HttpRequest, _: usize, _: Duration, _: &Signal) -> Result<HttpResponse, TransportError> {
        let body: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        let (name, question) = body["questions"].as_object().unwrap().iter().next().unwrap();
        let answer = match question["type"].as_str().unwrap() {
            "noul" => json!({"noul": 0.9}),
            _ => json!({"choice": "supported", "confidence": 0.9}),
        };
        let reply = json!({"answers": {name.clone(): answer}, "model": "jev-test", "usage": {"input_tokens": 300, "output_tokens": 30}});
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: reply.to_string().into_bytes(),
        })
    }
}

#[test]
fn a_live_run_is_recorded_and_replays_to_the_same_answers() {
    let set = set();
    let recorder = Arc::new(Recorder::new(Arc::new(Live), set.clone()));
    let key: judge::backend::KeySource = Arc::new(|| Some(["fake", "-key"].concat()));
    let mut deps = SeatDeps::new(recorder.clone(), key.clone(), key);
    deps.jev_url = "http://jev.test/v1/systemone".into();
    let settings = config::keys::judge_settings(
        json!({"judge": {"d1": {"url": "http://d1.test"}, "maxCalls": 6}})
            .as_object()
            .unwrap(),
    );
    let seat = Seat::new(settings, deps);
    let backends = ["jev".to_string(), "d1".to_string()];
    let runs = judge::bench::run(&seat, &set.bench, &backends, None, &Signal::new());
    assert!(
        runs[0].stopped.as_deref().unwrap().contains("judge.maxCalls"),
        "the ceiling bounds a live run"
    );
    let recording = recorder.recording("test".into());
    assert_eq!(recording.calls.len(), 6);
    assert_eq!(recording.calls[0].model.as_deref(), Some("jev-test"));
    assert_eq!(
        (recording.calls[0].case, recording.calls[1].backend.as_str()),
        (0, "d1")
    );

    let replay = Arc::new(Replay::new(set.clone(), recording).unwrap());
    let verdicts = decide_all(
        &replay_seat(&json!({"provider": ["jev"]}), replay),
        &set,
        &Signal::new(),
    );
    assert_eq!(verdicts[0].model.as_deref(), Some("jev-test"));
    assert_eq!(verdicts[0].answers[0].1.value, json!("supported"));
    assert_eq!(
        verdicts[3].rung,
        judge::seat::Rung::Abstained,
        "only three cases were recorded"
    );
}
