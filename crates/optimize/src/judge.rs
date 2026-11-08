//! The judge pipeline: the seat's decisions on the judgebench cases (crates/judge/tests/data/judgebench.json).
//!
//! - Offline (default): the seat's HTTP is answered from a [`Recording`] of a live run ([`Replay`]), so every
//!   variant (backend order, thresholds, escalation model) runs through the real seat for free. The frozen recording
//!   is sets/judgebench.recording.json; `--recording` replays another one.
//! - Live: each backend is asked every case once ([`judge::bench::run`], under the seat's budget and call ceiling)
//!   through a [`Recorder`]; the new recording is saved and the variants are then measured on it as offline.
//! - Escalations cannot be replayed (no chat answers are recorded): a variant naming `escalateModel` reports how many
//!   verdicts would escalate and their cost estimated from the model catalog, and those verdicts count as unsettled.
//!
//! Failure surface: a settled (`first` or `escalated`) wrong answer fails the run; an abstained case makes it
//! incomplete. Quality is the share of cases settled correctly; the penalty, the share left for a person or the
//! escalation model.
//!
//! [`fit`] finds, per question kind and backend, the lowest threshold that accepts no recorded wrong answer; the
//! result is a proposal for `judge.thresholds`, never applied here.

use crate::point::{Outcome, Point};
use crate::store::valid_name;
use blocking_http::{HttpRequest, HttpResponse, Transport, TransportError};
use delegation::estimate::{Priced, Rates, Unit, call_cost};
use judge::bench::Bench;
use judge::question::wire_answers;
use judge::seat::{Rung, Seat, SeatDeps, Verdict};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The frozen cases (shared with the judge crate's own bench test).
pub const CASES: &str = include_str!("../../judge/tests/data/judgebench.json");
/// The frozen recording of a live run on [`CASES`].
pub const RECORDING: &str = include_str!("../sets/judgebench.recording.json");

/// Fewer answers than this of a kind and backend are not enough to fit a threshold.
pub const MIN_FIT_ANSWERS: usize = 10;

const REPLAY_JEV_URL: &str = "replay://jev/v1/systemone";
const REPLAY_D1_URL: &str = "replay://d1";

/// The keys a judge variant's config may set (those of the `judge` setting that change decisions).
const CONFIG_KEYS: [&str; 5] = ["provider", "thresholds", "escalateModel", "fallbackModel", "model"];

/// The cases and their content hash.
pub struct JudgeSet {
    pub bench: Bench,
    pub hash: String,
    texts: HashMap<String, usize>,
}

impl JudgeSet {
    pub fn load(text: &str) -> Result<JudgeSet, String> {
        let bench = judge::bench::load(text)?;
        let texts = bench
            .cases
            .iter()
            .enumerate()
            .map(|(i, case)| (case.text.clone(), i))
            .collect();
        Ok(JudgeSet {
            bench,
            hash: jsrt::hash::sha256_hex(text),
            texts,
        })
    }

    fn case_of(&self, text: &str) -> Option<usize> {
        self.texts.get(text).copied()
    }
}

/// One recorded answer: what `backend` answered to case `case`'s question.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub backend: String,
    pub case: usize,
    /// The System One answer of the one question.
    pub answer: Value,
    /// `{input_tokens, output_tokens, cost?}` as the backend reported it.
    pub usage: Value,
    pub model: Option<String>,
    pub ms: f64,
}

/// A live run's answers, keyed to a set by its hash.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub source: String,
    pub set_hash: String,
    pub calls: Vec<Call>,
}

impl Recording {
    pub fn parse(text: &str) -> Result<Recording, String> {
        let file: Value = serde_json::from_str(text).map_err(|e| format!("recording: not JSON: {e}"))?;
        let calls = file["calls"]
            .as_array()
            .ok_or("recording: calls must be a list")?
            .iter()
            .enumerate()
            .map(|(i, call)| {
                let backend = call["backend"].as_str().filter(|b| matches!(*b, "jev" | "d1"));
                match (backend, call["case"].as_u64(), call["answer"].is_object()) {
                    (Some(backend), Some(case), true) => Ok(Call {
                        backend: backend.to_string(),
                        case: case as usize,
                        answer: call["answer"].clone(),
                        usage: call["usage"].clone(),
                        model: call["model"].as_str().map(str::to_string),
                        ms: call["ms"].as_f64().unwrap_or(0.0),
                    }),
                    _ => Err(format!("recording: calls[{i}] needs backend (jev|d1), case and answer")),
                }
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Recording {
            source: file["source"].as_str().unwrap_or("unnamed recording").to_string(),
            set_hash: file["setHash"]
                .as_str()
                .ok_or("recording: setHash is missing")?
                .to_string(),
            calls,
        })
    }

    pub fn to_json(&self) -> Value {
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|c| {
                let mut call = json!({"backend": c.backend, "case": c.case, "answer": c.answer, "ms": (c.ms * 10.0).round() / 10.0});
                if !c.usage.is_null() {
                    call["usage"] = c.usage.clone();
                }
                if let Some(model) = &c.model {
                    call["model"] = json!(model);
                }
                call
            })
            .collect();
        json!({"source": self.source, "setHash": self.set_hash, "calls": calls})
    }

    /// The recording checked against `set`: same hash, every case index in range.
    pub fn check(&self, set: &JudgeSet) -> Result<(), String> {
        if self.set_hash != set.hash {
            return Err(format!(
                "the recording is of another case set ({}…, cases are {}…)",
                self.set_hash.chars().take(12).collect::<String>(),
                &set.hash[..12]
            ));
        }
        match self.calls.iter().find(|c| c.case >= set.bench.cases.len()) {
            Some(call) => Err(format!("the recording names case {}, past the set's end", call.case)),
            None => Ok(()),
        }
    }

    fn find(&self, backend: &str, case: usize) -> Option<&Call> {
        self.calls.iter().find(|c| c.backend == backend && c.case == case)
    }
}

/// Which backend a seat request is for, and its case text.
fn request_of(request: &HttpRequest) -> Option<(&'static str, Value)> {
    let body: Value = serde_json::from_slice(request.body.as_deref()?).ok()?;
    if request.url.ends_with("/systemone") {
        Some(("jev", body))
    } else if request.url.ends_with("/decide") {
        Some(("d1", body))
    } else {
        None
    }
}

fn case_text(backend: &str, body: &Value) -> Option<String> {
    body[if backend == "jev" { "state" } else { "text" }]
        .as_str()
        .map(str::to_string)
}

/// The judge seat's HTTP answered from a recording: a case the recording lacks gets no answer (a failed call).
pub struct Replay {
    set: Arc<JudgeSet>,
    recording: Recording,
    served: Mutex<Vec<f64>>,
}

impl Replay {
    pub fn new(set: Arc<JudgeSet>, recording: Recording) -> Result<Replay, String> {
        recording.check(&set)?;
        Ok(Replay {
            set,
            recording,
            served: Mutex::default(),
        })
    }

    /// The recorded latencies of the calls served since the last take.
    pub fn take_ms(&self) -> Vec<f64> {
        std::mem::take(&mut *jsrt::sync::lock(&self.served))
    }
}

impl Transport for Replay {
    fn send(&self, request: HttpRequest, _: usize, _: Duration, _: &Signal) -> Result<HttpResponse, TransportError> {
        let (backend, body) = request_of(&request).ok_or(TransportError::Failed)?;
        let case = case_text(backend, &body)
            .and_then(|text| self.set.case_of(&text))
            .ok_or(TransportError::Failed)?;
        let call = self.recording.find(backend, case).ok_or(TransportError::Failed)?;
        let name = body["questions"]
            .as_object()
            .and_then(|q| q.keys().next())
            .ok_or(TransportError::Failed)?;
        let mut answers = Map::new();
        answers.insert(name.clone(), call.answer.clone());
        let mut reply = json!({"answers": answers, "usage": call.usage});
        if let Some(model) = &call.model {
            reply["model"] = json!(model);
        }
        jsrt::sync::lock(&self.served).push(call.ms);
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: reply.to_string().into_bytes(),
        })
    }
}

/// A live transport that keeps every answered case as a [`Call`].
pub struct Recorder {
    inner: Arc<dyn Transport>,
    set: Arc<JudgeSet>,
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    pub fn new(inner: Arc<dyn Transport>, set: Arc<JudgeSet>) -> Recorder {
        Recorder {
            inner,
            set,
            calls: Mutex::default(),
        }
    }

    pub fn recording(&self, source: String) -> Recording {
        let mut calls = jsrt::sync::lock(&self.calls).clone();
        calls.sort_by_key(|c| (c.case, c.backend != "jev"));
        Recording {
            source,
            set_hash: self.set.hash.clone(),
            calls,
        }
    }
}

impl Transport for Recorder {
    fn send(
        &self,
        request: HttpRequest,
        max_bytes: usize,
        timeout: Duration,
        signal: &Signal,
    ) -> Result<HttpResponse, TransportError> {
        let asked = request_of(&request)
            .and_then(|(backend, body)| Some((backend, self.set.case_of(&case_text(backend, &body)?)?)));
        let started = Instant::now();
        let response = self.inner.send(request, max_bytes, timeout, signal)?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        let answer = body["answers"].as_object().and_then(|a| a.values().next()).cloned();
        if let (Some((backend, case)), true, Some(answer)) = (asked, (200..300).contains(&response.status), answer) {
            jsrt::sync::lock(&self.calls).push(Call {
                backend: backend.to_string(),
                case,
                answer,
                usage: body["usage"].clone(),
                model: body["model"].as_str().map(str::to_string),
                ms,
            });
        }
        Ok(response)
    }
}

/// A variant's config checked: only the decision keys of the `judge` setting, with valid values.
pub fn check_config(config: &Value) -> Result<(), String> {
    let map = config.as_object().ok_or("a judge variant's config must be an object")?;
    if let Some(key) = map.keys().find(|k| !CONFIG_KEYS.contains(&k.as_str())) {
        return Err(format!("judge variants set {}; not {key}", CONFIG_KEYS.join(", ")));
    }
    let settings = config::keys::judge_settings(json!({"judge": config}).as_object().expect("an object"));
    let given = map.get("provider").and_then(Value::as_array).map_or(0, Vec::len);
    if map.contains_key("provider") && (settings.providers.len() != given || given == 0) {
        return Err("provider must list jev and/or d1, once each".into());
    }
    if map
        .get("thresholds")
        .is_some_and(|t| Value::Object(settings.thresholds.clone()) != *t)
    {
        return Err("thresholds must be {backend: 0..1} or {kind: {backend: 0..1}}".into());
    }
    for key in ["escalateModel", "fallbackModel"] {
        if map
            .get(key)
            .is_some_and(|m| m.as_str().is_none_or(|m| !m.contains('/')))
        {
            return Err(format!("{key} must be a provider/model reference"));
        }
    }
    Ok(())
}

/// The seat a variant runs as offline: its config, answered by `replay`, with no chat model.
pub fn replay_seat(config: &Value, replay: Arc<Replay>) -> Seat {
    let mut judge = config.clone();
    judge["d1"] = json!({"url": REPLAY_D1_URL});
    let settings = config::keys::judge_settings(json!({"judge": judge}).as_object().expect("an object"));
    let key: judge::backend::KeySource = Arc::new(|| Some("replay".to_string()));
    let mut deps = SeatDeps::new(replay, key.clone(), key);
    deps.jev_url = REPLAY_JEV_URL.into();
    Seat::new(settings, deps)
}

/// Every case decided by `seat`, with the case's kind (so per-kind thresholds apply).
pub fn decide_all(seat: &Seat, set: &JudgeSet, signal: &Signal) -> Vec<Verdict> {
    set.bench
        .cases
        .iter()
        .map(|case| {
            let question = &set.bench.questions[&case.kind];
            seat.decide(&case.text, std::slice::from_ref(question), Some(&case.kind), signal)
        })
        .collect()
}

/// Catalog rates of a `provider/model`, when the catalog prices it.
fn catalog_rates(model: &str) -> Option<Rates> {
    let (provider, id) = model.split_once('/')?;
    let cost = &ai::models::builtin_model(provider, id)?.cost;
    (cost.input > 0.0 || cost.output > 0.0).then_some(Rates {
        input: cost.input,
        output: cost.output,
    })
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    let index = ((sorted.len() as f64 * q) as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

/// The point of `verdicts` (one per case of `set`) of a variant with `config`; `ms` are the calls' latencies.
pub fn point(set: &JudgeSet, config: &Value, verdicts: &[Verdict], ms: &[f64]) -> Point {
    let (mut correct, mut wrong, mut review, mut abstained, mut escalated) = (0, 0, 0, 0, 0);
    let mut kinds: BTreeMap<String, [usize; 4]> = BTreeMap::new();
    let mut failures = Vec::new();
    let (mut tokens, mut cost, mut escalation_cost) = (0u64, Some(0.0), Some(0.0));
    let escalate_model = config["escalateModel"].as_str();
    let rates = escalate_model.and_then(catalog_rates);
    for (index, (case, verdict)) in set.bench.cases.iter().zip(verdicts).enumerate() {
        let tally = kinds.entry(case.kind.clone()).or_default();
        tally[0] += 1;
        for receipt in verdict.receipts.iter().filter(|r| r.sent()) {
            tokens += receipt.spend.input_tokens.unwrap_or(0) + receipt.spend.output_tokens.unwrap_or(0);
            cost = cost.zip(receipt.spend.cost).map(|(a, b)| a + b);
        }
        match verdict.rung {
            Rung::First | Rung::Escalated => {
                escalated += usize::from(verdict.rung == Rung::Escalated);
                let answer = &verdict.answers[0].1;
                if answer.value == case.gold {
                    correct += 1;
                    tally[1] += 1;
                } else {
                    wrong += 1;
                    tally[2] += 1;
                    failures.push(json!({
                        "case": index, "kind": case.kind, "failure": "accepted a wrong answer",
                        "gold": case.gold, "got": answer.value, "confidence": answer.confidence,
                        "backend": verdict.backend, "rung": verdict.rung.name(),
                    }));
                }
            }
            Rung::Review => {
                review += 1;
                tally[3] += 1;
                if escalate_model.is_some() {
                    let input = verdict.receipts.iter().find_map(|r| r.spend.input_tokens);
                    let unit = Unit {
                        input_tokens: input.map(|n| n as f64),
                        ..Unit::new("judge", 1, escalate_model.map(str::to_string))
                    };
                    let priced = Priced {
                        model: unit.model.clone(),
                        rates,
                    };
                    escalation_cost = escalation_cost.zip(call_cost(&unit, &priced)).map(|(a, b)| a + b);
                }
            }
            Rung::Abstained => {
                abstained += 1;
                failures.push(json!({
                    "case": index, "kind": case.kind, "failure": "abstained", "reason": verdict.reason,
                }));
            }
        }
    }
    let cases = set.bench.cases.len().max(1) as f64;
    let mut sorted = ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let latency = if sorted.is_empty() {
        Value::Null
    } else {
        json!({"p50": percentile(&sorted, 0.5).round(), "p95": percentile(&sorted, 0.95).round(), "max": sorted[sorted.len() - 1].round()})
    };
    let kinds: Map<String, Value> = kinds
        .into_iter()
        .map(|(kind, [n, c, w, r])| (kind, json!({"cases": n, "correct": c, "wrong": w, "review": r})))
        .collect();
    let mut metrics = json!({
        "quality": "settled correctly", "penalty": "left unsettled",
        "cases": set.bench.cases.len(), "correct": correct, "wrong": wrong, "review": review,
        "abstained": abstained, "escalated": escalated, "kinds": kinds, "tokens": tokens, "latencyMs": latency,
    });
    let mut total = cost;
    if let Some(model) = escalate_model {
        metrics["escalation"] = json!({
            "model": model, "wouldEscalate": review, "estimatedCostUsd": escalation_cost,
            "answers": "not replayed: these verdicts count as unsettled",
        });
        metrics["note"] = json!(format!("{review} would escalate to {model}"));
        total = total.zip(escalation_cost).map(|(a, b)| a + b);
    }
    Point {
        pipeline: "judge",
        variant: String::new(),
        version: 0,
        set_hash: set.hash.clone(),
        mode: "offline",
        outcome: if wrong > 0 {
            Outcome::Failed
        } else if abstained > 0 {
            Outcome::Incomplete
        } else {
            Outcome::Passed
        },
        quality: correct as f64 / cases,
        penalty: (review + abstained) as f64 / cases,
        cost_usd: total,
        metrics,
        failures,
    }
}

/// One kind and backend's fit.
#[derive(Clone, Debug, PartialEq)]
pub struct Fit {
    pub kind: String,
    pub backend: String,
    pub answers: usize,
    pub wrong: usize,
    /// The highest confidence of a wrong answer.
    pub max_wrong: Option<f64>,
    pub current: Option<f64>,
    pub fitted: Option<f64>,
    /// Answers under the current and the fitted threshold (escalated or reviewed).
    pub unsure_now: usize,
    pub unsure_fitted: usize,
    pub note: String,
}

/// The lowest two-decimal threshold above `max_wrong` (a confidence under it escalates).
fn above(max_wrong: f64) -> f64 {
    let t = (max_wrong * 100.0 + 1e-9).ceil() / 100.0;
    if t <= max_wrong { t + 0.01 } else { t }
}

/// Fit thresholds per kind and backend on `recording`: the lowest threshold that accepts none of the recorded wrong
/// answers. `current` is the threshold in force for (kind, backend).
pub fn fit(set: &JudgeSet, recording: &Recording, current: &dyn Fn(&str, &str) -> Option<f64>) -> Vec<Fit> {
    let mut answers: BTreeMap<(String, String), Vec<(bool, f64)>> = BTreeMap::new();
    for call in &recording.calls {
        let Some(case) = set.bench.cases.get(call.case) else {
            continue;
        };
        let question = &set.bench.questions[&case.kind];
        let mut wire = Map::new();
        wire.insert(question.name.clone(), call.answer.clone());
        if let Ok(parsed) = wire_answers(std::slice::from_ref(question), &Value::Object(wire)) {
            answers
                .entry((case.kind.clone(), call.backend.clone()))
                .or_default()
                .push((parsed[0].value == case.gold, parsed[0].confidence));
        }
    }
    answers
        .into_iter()
        .map(|((kind, backend), answers)| {
            let wrong: Vec<f64> = answers.iter().filter(|(ok, _)| !ok).map(|(_, c)| *c).collect();
            let max_wrong = wrong.iter().copied().reduce(f64::max);
            let now = current(&kind, &backend);
            let unsure = |t: Option<f64>| t.map_or(0, |t| answers.iter().filter(|(_, c)| *c < t).count());
            let (fitted, note) = if answers.len() < MIN_FIT_ANSWERS {
                (
                    None,
                    format!("{} answers: too few to fit (at least {MIN_FIT_ANSWERS})", answers.len()),
                )
            } else {
                match max_wrong.map(above) {
                    None => (None, "no wrong answer recorded: kept".to_string()),
                    Some(t) if t > 1.0 => (
                        None,
                        "a wrong answer at full confidence: no threshold separates it".into(),
                    ),
                    Some(t) => (Some(t), format!("accepts none of the {} wrong answer(s)", wrong.len())),
                }
            };
            Fit {
                unsure_now: unsure(now),
                unsure_fitted: unsure(fitted),
                answers: answers.len(),
                wrong: wrong.len(),
                max_wrong,
                current: now,
                fitted,
                note,
                kind,
                backend,
            }
        })
        .collect()
}

/// The fitted thresholds as a `judge.thresholds` value: `{kind: {backend: t}}`.
pub fn thresholds(fits: &[Fit]) -> Map<String, Value> {
    let mut out = Map::new();
    for fit in fits {
        if let Some(t) = fit.fitted {
            out.entry(fit.kind.clone())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("an object")
                .insert(fit.backend.clone(), json!(t));
        }
    }
    out
}

/// The proposal document: the fitted `judge.thresholds` and the evidence row behind each, for a person to apply.
pub fn proposal(set: &JudgeSet, recording: &Recording, fits: &[Fit], at: &str) -> Value {
    let rows: Vec<Value> = fits
        .iter()
        .map(|f| {
            json!({
                "kind": f.kind, "backend": f.backend, "answers": f.answers, "wrong": f.wrong, "maxWrongConfidence": f.max_wrong,
                "current": f.current, "fitted": f.fitted, "unsureNow": f.unsure_now, "unsureFitted": f.unsure_fitted, "note": f.note,
            })
        })
        .collect();
    json!({
        "pipeline": "judge",
        "proposal": "judge.thresholds",
        "apply": "by hand, as the global judge.thresholds setting (merge with what is there); nothing was changed",
        "fixtureHash": set.hash,
        "recording": recording.source,
        "createdAt": at,
        "thresholds": thresholds(fits),
        "rows": rows,
    })
}

/// The built-in variants: each backend alone with the thresholds in force, and with the fitted ones when any were
/// fitted.
pub fn default_variants(fits: &[Fit]) -> Vec<(String, Value)> {
    let mut variants = Vec::new();
    for backend in config::keys::JUDGE_BACKENDS {
        variants.push((backend.to_string(), json!({"provider": [backend]})));
        let own: Vec<Fit> = fits.iter().filter(|f| f.backend == *backend).cloned().collect();
        let fitted = thresholds(&own);
        if !fitted.is_empty() {
            variants.push((
                format!("{backend}-fitted"),
                json!({"provider": [backend], "thresholds": fitted}),
            ));
        }
    }
    debug_assert!(variants.iter().all(|(name, _)| valid_name(name)));
    variants
}
