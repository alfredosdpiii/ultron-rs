//! The autoreview pipeline: the reports `ultron autoreview eval` keeps (`<agent dir>/autoreview/evals/*.json`, one
//! live review of every case each) re-scored at each `blockAt` level with the eval's own scorer
//! ([`review::eval::score`], [`review::eval::summary`]). A report is one model's run; re-scoring spends nothing.
//! Reports compare only on the same cases (the set hash is that of the case list).
//!
//! Failure surface: a run whose controls are blocked at least as often as the reverted and fix-inducing commits
//! (the reviewer does not tell a bug from a change) fails; an errored or incomplete review makes it incomplete.
//! Quality is the share of reverted and fix-inducing commits blocked; the penalty, the share of controls blocked (an
//! upper bound on false alarms).

use crate::point::{Outcome, Point};
use review::eval::{BugLine, Case, CaseKind, CaseScore, score, summary};
use review::plan::{EngineFinding, EngineResult, Usage};
use serde_json::{Value, json};

/// One case of a report and how its review went (`None`: the review failed).
pub struct Row {
    pub case: Case,
    pub result: Option<EngineResult>,
    pub error: Option<String>,
}

pub struct Report {
    /// The report's `model` (the reviewer's), else its file name.
    pub name: String,
    pub block_at: String,
    pub rows: Vec<Row>,
    /// The hash of its case list (kind and commit of each case).
    pub hash: String,
}

fn kind(name: &str) -> Option<CaseKind> {
    match name {
        "reverted" => Some(CaseKind::Reverted),
        "fixInducing" => Some(CaseKind::FixInducing),
        "control" => Some(CaseKind::Control),
        _ => None,
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// The review result a report row's score stands for: its confirmed findings (all a verdict reads), coverage and
/// cost.
fn result_of(score: &Value) -> EngineResult {
    let findings = score["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| EngineFinding {
            file: f["file"].as_str().unwrap_or_default().to_string(),
            line: f["line"].as_i64().unwrap_or(0),
            end_line: f["endLine"].as_i64(),
            level: f["level"].as_str().map(str::to_string),
            claim: f["claim"].as_str().unwrap_or_default().to_string(),
            verification: "confirmed".into(),
            ..EngineFinding::default()
        })
        .collect();
    EngineResult {
        complete: score["complete"].as_bool().unwrap_or(false),
        findings,
        incomplete: strings(&score["incomplete"]),
        usage: Usage {
            cost_usd: score["costUsd"].as_f64().unwrap_or(0.0),
            ..Usage::default()
        },
        ..EngineResult::default()
    }
}

/// A report `ultron autoreview eval` wrote; `fallback_name` names it when it names no model.
pub fn parse_report(text: &str, fallback_name: &str) -> Result<Report, String> {
    let file: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let rows = file["cases"]
        .as_array()
        .filter(|c| !c.is_empty())
        .ok_or("cases must be a nonempty list")?
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let fields = (
                row["kind"].as_str().and_then(kind),
                row["sha"].as_str(),
                row["parent"].as_str(),
            );
            let (Some(kind), Some(sha), Some(parent)) = fields else {
                return Err(format!("cases[{i}] needs kind, sha and parent"));
            };
            let lines = row["lines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| {
                    Some(BugLine {
                        path: l["path"].as_str()?.to_string(),
                        line: l["line"].as_i64()?,
                    })
                })
                .collect();
            let case = Case {
                kind,
                sha: sha.to_string(),
                parent: parent.to_string(),
                subject: row["subject"].as_str().unwrap_or_default().to_string(),
                by: strings(&row["by"]),
                lines,
            };
            let result = row["score"].is_object().then(|| result_of(&row["score"]));
            let error = match &result {
                Some(_) => None,
                None => Some(row["error"].as_str().unwrap_or("no score").to_string()),
            };
            Ok(Row { case, result, error })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut list: Vec<String> = rows
        .iter()
        .map(|r| format!("{:?} {}", r.case.kind, r.case.sha))
        .collect();
    list.sort();
    Ok(Report {
        name: file["model"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or(fallback_name)
            .to_string(),
        block_at: file["blockAt"].as_str().unwrap_or("medium").to_string(),
        hash: jsrt::hash::sha256_hex(list.join("\n")),
        rows,
    })
}

/// The report scored as if it had been reviewed with `block_at`.
pub fn point(report: &Report, block_at: &str) -> Point {
    let scored: Vec<(Case, CaseScore)> = report
        .rows
        .iter()
        .filter_map(|row| Some((row.case.clone(), score(&row.case, row.result.as_ref()?, block_at))))
        .collect();
    let totals = summary(&scored);
    let count = |kind: &str, key: &str| totals[kind][key].as_u64().unwrap_or(0) as f64;
    let bugs = count("reverted", "cases") + count("fixInducing", "cases");
    let caught = count("reverted", "blocked") + count("fixInducing", "blocked");
    let quality = if bugs > 0.0 { caught / bugs } else { 0.0 };
    let controls = count("controls", "cases");
    let penalty = if controls > 0.0 {
        count("controls", "blocked") / controls
    } else {
        0.0
    };
    let mut failures: Vec<Value> = Vec::new();
    for row in &report.rows {
        if let Some(error) = &row.error {
            failures.push(json!({"sha": row.case.sha, "failure": "review failed", "error": error}));
        }
    }
    for (case, got) in &scored {
        let short: String = case.sha.chars().take(8).collect();
        match (case.kind, got.blocked) {
            (CaseKind::Control, true) => failures.push(json!({
                "sha": short, "failure": "control blocked", "subject": case.subject, "findings": got.findings.len(),
            })),
            (CaseKind::Reverted | CaseKind::FixInducing, false) => {
                failures.push(json!({"sha": short, "failure": "bug not blocked", "subject": case.subject}))
            }
            _ => {}
        }
        if !got.complete {
            failures.push(json!({"sha": short, "failure": "incomplete review", "why": got.incomplete}));
        }
    }
    let incomplete = report.rows.iter().any(|r| r.error.is_some()) || scored.iter().any(|(_, s)| !s.complete);
    let mut metrics = totals.clone();
    metrics["quality"] = json!("bugs blocked");
    metrics["penalty"] = json!("controls blocked");
    metrics["blockAt"] = json!(block_at);
    metrics["reviewedWith"] = json!(report.block_at);
    Point {
        pipeline: "autoreview",
        variant: String::new(),
        version: 0,
        set_hash: report.hash.clone(),
        mode: "offline",
        outcome: if bugs > 0.0 && controls > 0.0 && penalty >= quality {
            Outcome::Failed
        } else if incomplete {
            Outcome::Incomplete
        } else {
            Outcome::Passed
        },
        quality,
        penalty,
        cost_usd: totals["costUsd"].as_f64(),
        metrics,
        failures,
    }
}
