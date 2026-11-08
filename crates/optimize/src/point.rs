//! A measured variant: one cost/quality point, its failures, and the Pareto frontier over points of one set.

use serde_json::{Value, json};

/// The failure surface's verdict on a run (the experiment ledger's `outcome` words).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Inside the failure surface.
    Passed,
    /// A failure the pipeline must never have (an accepted wrong answer, a leaked memory, ...).
    Failed,
    /// Some cases were not measured (a backend stopped, a review errored).
    Incomplete,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub pipeline: &'static str,
    pub variant: String,
    pub version: u64,
    /// The test set's content hash: points compare only within one.
    pub set_hash: String,
    /// `offline` or `live`.
    pub mode: &'static str,
    pub outcome: Outcome,
    /// Higher is better (the pipeline names it in `metrics.quality`).
    pub quality: f64,
    /// Lower is better (the pipeline names it in `metrics.penalty`).
    pub penalty: f64,
    /// `None` when any part of the run had no price.
    pub cost_usd: Option<f64>,
    pub metrics: Value,
    /// What went wrong, case by case: kept with the point, never dropped.
    pub failures: Vec<Value>,
}

fn round(n: f64) -> f64 {
    (n * 1e6).round() / 1e6
}

impl Point {
    /// `<variant>@v<version>`.
    pub fn label(&self) -> String {
        format!("{}@v{}", self.variant, self.version)
    }

    /// The stored record (shaped like an `experiments.record` run: `variant`, `fixtureHash`, `outcome`).
    pub fn to_json(&self, recorded_at: &str) -> Value {
        json!({
            "pipeline": self.pipeline,
            "variant": self.label(),
            "fixtureHash": self.set_hash,
            "mode": self.mode,
            "outcome": self.outcome.name(),
            "quality": round(self.quality),
            "penalty": round(self.penalty),
            "costUsd": self.cost_usd.map(round),
            "metrics": self.metrics,
            "failures": self.failures,
            "recordedAt": recorded_at,
        })
    }

    /// One line of the table `ultron optimize` prints.
    pub fn line(&self, on_frontier: bool) -> String {
        let cost = self
            .cost_usd
            .map_or_else(|| "cost unknown".to_string(), |c| format!("${c:.4}"));
        format!(
            "{} {:<24} {:<10} {} {:.3}  {} {:.3}  {cost}  {} failure(s){}",
            if on_frontier { "*" } else { " " },
            self.label(),
            self.outcome.name(),
            self.metrics["quality"].as_str().unwrap_or("quality"),
            self.quality,
            self.metrics["penalty"].as_str().unwrap_or("penalty"),
            self.penalty,
            self.failures.len(),
            self.metrics["note"]
                .as_str()
                .map_or_else(String::new, |note| format!("  ({note})")),
        )
    }
}

/// Whether `a` dominates `b`: no worse in quality, penalty and cost, better in one. A cost that is unknown on
/// either side is left out of the comparison (neither better nor worse).
fn dominates(a: &Point, b: &Point) -> bool {
    let costs = a.cost_usd.zip(b.cost_usd);
    let no_worse = a.quality >= b.quality && a.penalty <= b.penalty && costs.is_none_or(|(x, y)| x <= y);
    let better = a.quality > b.quality || a.penalty < b.penalty || costs.is_some_and(|(x, y)| x < y);
    no_worse && better
}

/// For each point, whether it is on the Pareto frontier of its set: not failed, and dominated by no other
/// not-failed point of the same set hash.
pub fn frontier(points: &[Point]) -> Vec<bool> {
    let eligible = |p: &Point| p.outcome != Outcome::Failed;
    points
        .iter()
        .map(|p| {
            eligible(p)
                && !points
                    .iter()
                    .any(|q| eligible(q) && q.set_hash == p.set_hash && dominates(q, p))
        })
        .collect()
}
