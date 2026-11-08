//! `ultron judge bench`: labelled cases asked of each backend directly (no fallback, no escalation), interleaved so
//! the backends see the same conditions, under the seat's call ceiling and budget; accuracy per kind, latency and
//! spend. A backend stops at its first failure (nothing is retried but what a `retry-after` allows).
//!
//! A cases file is `{"questions": {<kind>: <question spec>}, "cases": [{"q": <kind>, "gold": <answer>, "text"}]}`;
//! `gold` is a label, a bool (yes/no), a level or a number (crates/judge/tests/data/judgebench.json).

use crate::question::{Question, parse_questions};
use crate::seat::Seat;
use kernel::Signal;
use serde_json::Value;
use std::collections::BTreeMap;

pub struct Case {
    pub kind: String,
    pub gold: Value,
    pub text: String,
}

pub struct Bench {
    /// One question per kind, named by its kind.
    pub questions: BTreeMap<String, Question>,
    pub cases: Vec<Case>,
}

/// A cases file.
pub fn load(text: &str) -> Result<Bench, String> {
    let file: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let questions: BTreeMap<String, Question> = parse_questions(&file["questions"])?
        .into_iter()
        .map(|q| (q.name.clone(), q))
        .collect();
    let cases = file["cases"]
        .as_array()
        .filter(|c| !c.is_empty())
        .ok_or("cases must be a nonempty list")?
        .iter()
        .enumerate()
        .map(|(i, case)| {
            let kind = case["q"].as_str().filter(|k| questions.contains_key(*k));
            let text = case["text"].as_str().filter(|t| !t.trim().is_empty());
            match (kind, text, &case["gold"]) {
                (Some(kind), Some(text), gold) if !gold.is_null() => Ok(Case {
                    kind: kind.to_string(),
                    gold: gold.clone(),
                    text: text.to_string(),
                }),
                _ => Err(format!("cases[{i}] needs q (a question's name), gold and text")),
            }
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Bench { questions, cases })
}

/// One backend's run.
#[derive(Default)]
pub struct Run {
    pub backend: String,
    /// The model that answered (the last one named).
    pub model: Option<String>,
    /// `(kind, correct, asked)`.
    pub kinds: BTreeMap<String, (usize, usize)>,
    pub ms: Vec<f64>,
    pub tokens: u64,
    /// `None` once any answer's cost is unknown.
    pub cost: Option<f64>,
    /// `(kind, gold, got, confidence)` of each wrong answer.
    pub wrong: Vec<(String, Value, Value, f64)>,
    /// Why it stopped early.
    pub stopped: Option<String>,
}

/// Ask every case of `bench` (the first `limit`) of each of `backends`.
pub fn run(seat: &Seat, bench: &Bench, backends: &[String], limit: Option<usize>, signal: &Signal) -> Vec<Run> {
    let mut runs: Vec<Run> = backends
        .iter()
        .map(|b| Run {
            backend: b.clone(),
            cost: Some(0.0),
            ..Run::default()
        })
        .collect();
    for case in bench.cases.iter().take(limit.unwrap_or(usize::MAX)) {
        let question = &bench.questions[&case.kind];
        for run in runs.iter_mut().filter(|r| r.stopped.is_none()) {
            let (outcome, receipts) = seat.ask(&run.backend, &case.text, std::slice::from_ref(question), signal);
            let reply = match outcome {
                Ok(reply) => reply,
                Err(reason) => {
                    run.stopped = Some(reason);
                    continue;
                }
            };
            let answer = &reply.answers[0];
            let tally = run.kinds.entry(case.kind.clone()).or_default();
            tally.1 += 1;
            if answer.value == case.gold {
                tally.0 += 1;
            } else {
                run.wrong.push((
                    case.kind.clone(),
                    case.gold.clone(),
                    answer.value.clone(),
                    answer.confidence,
                ));
            }
            run.ms.extend(receipts.iter().filter(|r| r.sent()).map(|r| r.ms));
            run.tokens += reply.spend.input_tokens.unwrap_or(0) + reply.spend.output_tokens.unwrap_or(0);
            run.cost = run.cost.zip(reply.spend.cost).map(|(a, b)| a + b);
            run.model = Some(reply.model);
        }
    }
    runs
}

/// The value at `q` (0..=1) of sorted `ms`, as the judgebench script reads it.
fn percentile(ms: &[f64], q: f64) -> f64 {
    let index = ((ms.len() as f64 * q) as usize).saturating_sub(1).min(ms.len() - 1);
    ms[index]
}

fn shown(value: &Value) -> String {
    value.as_str().map_or_else(|| value.to_string(), str::to_string)
}

/// The report's lines.
pub fn report(runs: &[Run]) -> Vec<String> {
    let mut lines = Vec::new();
    for run in runs {
        let (correct, asked) = run.kinds.values().fold((0, 0), |(c, a), (kc, ka)| (c + kc, a + ka));
        let mut line = format!(
            "{} ({}): accuracy {correct}/{asked}",
            run.backend,
            run.model.as_deref().unwrap_or("no answer")
        );
        for (kind, (c, a)) in &run.kinds {
            line.push_str(&format!(" {kind} {c}/{a}"));
        }
        if !run.ms.is_empty() {
            let mut ms = run.ms.clone();
            ms.sort_by(f64::total_cmp);
            line.push_str(&format!(
                " | latency p50 {:.0} ms p95 {:.0} ms max {:.0} ms",
                percentile(&ms, 0.5),
                percentile(&ms, 0.95),
                ms[ms.len() - 1]
            ));
        }
        if run.tokens > 0 {
            line.push_str(&format!(" | tokens {}", run.tokens));
        }
        line.push_str(&match run.cost {
            Some(cost) => format!(" | cost ${cost:.4}"),
            None => " | cost unknown".into(),
        });
        lines.push(line);
        for (kind, gold, got, confidence) in &run.wrong {
            lines.push(format!(
                "   wrong {kind}: gold {} got {} conf {confidence:.2}",
                shown(gold),
                shown(got)
            ));
        }
        if let Some(reason) = &run.stopped {
            lines.push(format!("   stopped: {reason}"));
        }
    }
    lines
}

/// A one-question check that `backend` answers (the setup wizard's probe): its model and latency, or why not. It is
/// one real call.
pub fn probe(seat: &Seat, backend: &str, signal: &Signal) -> Result<String, String> {
    let questions = parse_questions(&serde_json::json!({
        "ok": {"type": "yesno", "instructions": "Does the text say the service is reachable?"}
    }))?;
    let (outcome, receipts) = seat.ask(backend, "The service is reachable.", &questions, signal);
    let reply = outcome?;
    let ms = receipts.iter().filter(|r| r.sent()).map(|r| r.ms).sum::<f64>();
    Ok(format!("{} answered in {ms:.0} ms", reply.model))
}
