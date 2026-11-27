use super::validation::invalid;
use super::{Measurement, calls_of, known, merge_totals, reservations_of, totals};
use jsrt::json;
use serde_json::{Map, Value, json};

pub(super) fn normalize_measurement(measurement: Option<&Measurement>, elapsed: f64) -> Result<Value, String> {
    let wall = measurement.and_then(|m| m.wall_ms).unwrap_or(elapsed);
    if !wall.is_finite() || wall < 0.0 {
        return Err("Usage wallMs must be a non-negative finite number".into());
    }
    let fields = measurement.cloned().unwrap_or_default();
    for (name, value) in [
        ("inputTokens", fields.input_tokens),
        ("outputTokens", fields.output_tokens),
        ("totalTokens", fields.total_tokens),
        ("cost", fields.cost),
    ] {
        if value.is_some_and(|n| !n.is_finite()) {
            return Err(invalid(name));
        }
    }
    Ok(json!({
        "inputTokens": known(fields.input_tokens), "outputTokens": known(fields.output_tokens),
        "totalTokens": known(fields.total_tokens), "cost": known(fields.cost), "wallMs": json::number(wall),
    }))
}

/// A settled call from a reservation; `budget`: whether `budgetId` is copied (reconciliation does not copy it).
pub(super) fn call_from(reservation: &Value, id: String, now: f64, status: &str, usage: Value, budget: bool) -> Value {
    let mut call = Map::new();
    call.insert("id".into(), Value::String(id));
    call.insert("reservationId".into(), reservation["id"].clone());
    call.insert("rootId".into(), reservation["rootId"].clone());
    call.insert("kind".into(), reservation["kind"].clone());
    let mut keys = vec!["taskId", "parentTaskId", "requestKey"];
    if budget {
        keys.push("budgetId");
    }
    for key in keys {
        if let Some(value) = reservation.get(key) {
            call.insert(key.into(), value.clone());
        }
    }
    call.insert("admittedAt".into(), reservation["admittedAt"].clone());
    call.insert("settledAt".into(), json::number(now));
    call.insert("status".into(), Value::String(status.into()));
    call.insert("usage".into(), usage);
    Value::Object(call)
}

pub(super) fn last_activity(root: &Value) -> f64 {
    let mut latest = root["startedAt"].as_f64().unwrap_or(f64::NEG_INFINITY);
    for call in calls_of(root) {
        latest = latest
            .max(call["settledAt"].as_f64().unwrap_or(f64::NEG_INFINITY))
            .max(call["admittedAt"].as_f64().unwrap_or(f64::NEG_INFINITY));
    }
    for reservation in reservations_of(root) {
        latest = latest.max(reservation["admittedAt"].as_f64().unwrap_or(f64::NEG_INFINITY));
    }
    if let Some(at) = root["turns"]["lastAt"].as_f64() {
        latest = latest.max(at);
    }
    latest
}

pub(super) fn fold_root(history: &mut Value, root: &Value, spend: &Value) {
    let add = |value: &Value, n: f64| json::number(value.as_f64().unwrap_or(0.0) + n);
    history["roots"] = add(&history["roots"], 1.0);
    if !root["imported"].is_null() {
        history["importedRoots"] = add(&history["importedRoots"], 1.0);
    }
    let calls = calls_of(root);
    let started = root["startedAt"]
        .as_f64()
        .or_else(|| calls.iter().filter_map(|c| c["admittedAt"].as_f64()).reduce(f64::min));
    if let Some(started) = started {
        let first = history["firstStartedAt"].as_f64().map_or(started, |f| f.min(started));
        history["firstStartedAt"] = json::number(first);
    }
    for call in calls {
        let settled = call["settledAt"].as_f64().unwrap_or(0.0);
        let last = history["lastSettledAt"].as_f64().map_or(settled, |l| l.max(settled));
        history["lastSettledAt"] = json::number(last);
    }
    history["usage"] = merge_totals(&history["usage"], &totals(calls));
    for kind in ["task", "model"] {
        let of_kind: Vec<Value> = calls.iter().filter(|c| c["kind"] == kind).cloned().collect();
        history["byKind"][kind] = merge_totals(&history["byKind"][kind], &totals(&of_kind));
    }
    history["spentUsd"] = add(&history["spentUsd"], spend["spentUsd"].as_f64().unwrap_or(0.0));
    history["unknownPricedCalls"] = add(
        &history["unknownPricedCalls"],
        spend["unknownPricedCalls"].as_f64().unwrap_or(0.0),
    );
    for call in calls.iter().filter(|c| c["kind"] == "task") {
        if let Some(status) = call["status"].as_str() {
            history["taskAdmissions"][status] = add(&history["taskAdmissions"][status], 1.0);
        }
    }
}
