use super::{LocalServices, fields};
use kernel_apis::store::read_versioned;
use serde_json::{Map, Value, json};
use session_journal::ns::EXPERIMENTS;
use std::collections::HashSet;

fn valid_experiment_metadata(map: &Map<String, Value>) -> bool {
    let outcome = map.get("outcome").and_then(Value::as_str);
    map.get("variant").is_some_and(Value::is_string)
        && map.get("fixtureHash").is_some_and(Value::is_string)
        && outcome.is_some_and(|o| ["passed", "failed", "incomplete"].contains(&o))
        && map.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
        && map
            .get("recordedAt")
            .and_then(Value::as_str)
            .is_some_and(|at| jsrt::date::parse_date(at).is_some())
}

fn validate_experiment_usage(usage: &Value) -> Result<(), String> {
    let usage = usage.as_object().ok_or("Experiment usage must be an object")?;
    let mut sum = 0.0;
    for (key, amount) in usage {
        let amount = amount
            .as_f64()
            .filter(|a| a.is_finite() && *a >= 0.0)
            .ok_or("Experiment usage amounts must be finite and nonnegative")?;
        if key != "total" {
            sum += amount;
        }
    }
    let total = usage.get("total").and_then(Value::as_f64);
    if !total.is_some_and(|total| (total - sum).abs() <= 1e-9 * sum.max(1.0)) {
        return Err("Experiment usage total does not reconcile with its parts".into());
    }
    Ok(())
}

/// `experiment(value)`: a recorded run with valid metadata whose usage, when present, reconciles.
fn experiment(value: &Value) -> Result<(), String> {
    const INVALID: &str = "Experiment requires variant, fixtureHash, outcome and valid record metadata";
    let map = value.as_object().ok_or(INVALID)?;
    if !valid_experiment_metadata(map) {
        return Err(INVALID.into());
    }
    if let Some(usage) = map.get("usage") {
        validate_experiment_usage(usage)?;
    }
    Ok(())
}

fn parse_experiment_runs(stored: &Value) -> Result<Vec<Value>, String> {
    let document = fields(stored, &["formatVersion", "runs"], &[])?;
    let list = document["runs"].as_array();
    if document["formatVersion"].as_f64() != Some(1.0) || list.is_none() {
        return Err("Invalid experiment document".into());
    }
    let mut runs = Vec::new();
    let mut seen = HashSet::new();
    for run in list.into_iter().flatten() {
        experiment(run)?;
        if !seen.insert(run["id"].as_str().unwrap_or_default().to_string()) {
            return Err("Duplicate experiment ID".into());
        }
        runs.push(run.clone());
    }
    Ok(runs)
}

fn compare_experiments(runs: &[Value], payload: &Value) -> Result<Value, String> {
    let map = fields(payload, &["baseline", "candidate"], &[])?;
    let find = |id: &Value| runs.iter().find(|run| run["id"] == *id);
    let (Some(baseline), Some(candidate)) = (find(&map["baseline"]), find(&map["candidate"])) else {
        return Err("Unknown experiment".into());
    };
    if baseline["fixtureHash"] != candidate["fixtureHash"] {
        return Err("Different fixtures cannot form a matched comparison".into());
    }
    Ok(json!({
        "baseline": baseline, "candidate": candidate,
        "claim": "Observed runs only; no statistical superiority established",
    }))
}

impl LocalServices {
    fn load_experiments(&self) -> Result<Vec<Value>, String> {
        let saved = self.documents.get(EXPERIMENTS.key)?;
        let Some(stored) = read_versioned(&EXPERIMENTS.address(), saved, EXPERIMENTS.version_field)? else {
            return Ok(Vec::new());
        };
        parse_experiment_runs(&stored)
    }

    fn record_experiment(
        &self,
        mut runs: Vec<Value>,
        payload: &Value,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let map = fields(payload, &["run"], &[])?;
        let mut value = map["run"].as_object().cloned().ok_or("Invalid experiment run")?;
        value.insert("id".into(), (self.options.ids)().into());
        value.insert("recordedAt".into(), self.now().into());
        let value = Value::Object(value);
        experiment(&value)?;
        runs.push(value.clone());
        persist(EXPERIMENTS.key, &json!({"formatVersion": 1, "runs": runs}))?;
        Ok(value)
    }

    pub(super) fn experiments(
        &self,
        kind: &str,
        payload: &Value,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let runs = self.load_experiments()?;
        match kind {
            "experiments.list" => {
                fields(payload, &[], &[])?;
                Ok(Value::Array(runs))
            }
            "experiments.record" => self.record_experiment(runs, payload, persist),
            _ => compare_experiments(&runs, payload),
        }
    }
}
