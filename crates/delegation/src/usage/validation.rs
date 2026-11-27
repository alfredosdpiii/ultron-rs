use super::{STATUSES, STORED_KINDS};
use serde_json::{Map, Value};

pub(super) fn invalid(what: &str) -> String {
    format!("Invalid usage ledger {what}")
}

pub(super) fn object(value: &Value) -> Result<&Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "Invalid usage ledger document".to_string())
}

pub(super) fn finite(value: &Value, name: &str) -> Result<f64, String> {
    value.as_f64().filter(|n| n.is_finite()).ok_or_else(|| invalid(name))
}

pub(super) fn finite_integer(value: &Value, name: &str, minimum: f64) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|n| jsrt::number::is_safe_integer(*n) && *n >= minimum)
        .ok_or_else(|| invalid(name))
}

fn optional_finite(value: &Value, name: &str) -> Result<(), String> {
    if value.is_null() {
        Ok(())
    } else {
        finite(value, name).map(|_| ())
    }
}

fn optional_nonempty(item: &Map<String, Value>, key: &str, error: &str) -> Result<(), String> {
    match item.get(key) {
        None => Ok(()),
        Some(Value::String(text)) if !text.is_empty() => Ok(()),
        Some(_) => Err(error.into()),
    }
}

fn validate_measurement(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    for key in ["inputTokens", "outputTokens", "totalTokens", "cost", "wallMs"] {
        if !item.contains_key(key) {
            return Err(invalid("measurement"));
        }
    }
    for key in ["inputTokens", "outputTokens", "totalTokens", "cost"] {
        optional_finite(&item[key], key)?;
    }
    if finite(&item["wallMs"], "wallMs")? < 0.0 {
        return Err(invalid("wallMs"));
    }
    Ok(())
}

fn has_nonempty(item: &Map<String, Value>, key: &str) -> bool {
    item.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

fn validate_optional_fields(item: &Map<String, Value>, specs: &[(&str, &str)]) -> Result<(), String> {
    for (key, err) in specs {
        optional_nonempty(item, key, err)?;
    }
    Ok(())
}

fn validate_reservation(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    if !has_nonempty(item, "id") || !has_nonempty(item, "rootId") {
        return Err(invalid("reservation"));
    }
    if !item
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| STORED_KINDS.contains(&k))
    {
        return Err(invalid("reservation kind"));
    }
    validate_optional_fields(
        item,
        &[
            ("taskId", "Invalid usage ledger task ID"),
            ("parentTaskId", "Invalid usage ledger parent task ID"),
            ("requestKey", "Invalid usage ledger request key"),
            ("budgetId", "Invalid usage ledger budget ID"),
        ],
    )?;
    if finite(item.get("admittedAt").unwrap_or(&Value::Null), "admittedAt")? < 0.0 {
        return Err(invalid("admittedAt"));
    }
    optional_finite(item.get("deadlineAt").unwrap_or(&Value::Bool(false)), "deadlineAt")
}

fn validate_call(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    if !has_nonempty(item, "id") || !has_nonempty(item, "reservationId") {
        return Err(invalid("call"));
    }
    if !has_nonempty(item, "rootId") {
        return Err(invalid("call root ID"));
    }
    if !item
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| STORED_KINDS.contains(&k))
    {
        return Err(invalid("call kind"));
    }
    validate_optional_fields(
        item,
        &[
            ("requestKey", "Invalid usage ledger call request key"),
            ("budgetId", "Invalid usage ledger call budget ID"),
            ("taskId", "Invalid usage ledger call task ID"),
            ("parentTaskId", "Invalid usage ledger call task ID"),
        ],
    )?;
    finite(item.get("admittedAt").unwrap_or(&Value::Null), "admittedAt")?;
    finite(item.get("settledAt").unwrap_or(&Value::Null), "settledAt")?;
    if !item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| STATUSES.contains(&s))
    {
        return Err(invalid("call status"));
    }
    validate_measurement(item.get("usage").unwrap_or(&Value::Null))
}

fn validate_totals(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    for name in ["calls", "taskCalls", "modelCalls", "unknownCalls"] {
        finite_integer(item.get(name).unwrap_or(&Value::Null), &format!("history {name}"), 0.0)?;
    }
    if finite(item.get("wallMs").unwrap_or(&Value::Null), "history wallMs")? < 0.0 {
        return Err(invalid("history wallMs"));
    }
    for name in ["inputTokens", "outputTokens", "totalTokens", "cost"] {
        let Some(value) = item.get(name) else {
            return Err(invalid("history totals"));
        };
        optional_finite(value, &format!("history {name}"))?;
    }
    Ok(())
}

fn validate_history_admissions(admissions: &Map<String, Value>) -> Result<(), String> {
    for status in STATUSES {
        finite_integer(
            admissions.get(status).unwrap_or(&Value::Null),
            "history taskAdmissions",
            0.0,
        )?;
    }
    Ok(())
}

fn validate_history_by_kind(by_kind: &Map<String, Value>) -> Result<(), String> {
    for kind in ["task", "model"] {
        validate_totals(by_kind.get(kind).unwrap_or(&Value::Null))?;
    }
    Ok(())
}

fn validate_history_counts(item: &Map<String, Value>) -> Result<(), String> {
    let field = |name: &str| item.get(name).unwrap_or(&Value::Null);
    finite_integer(field("roots"), "history roots", 0.0)?;
    finite_integer(field("importedRoots"), "history importedRoots", 0.0)?;
    optional_finite(
        item.get("firstStartedAt").unwrap_or(&Value::Bool(false)),
        "history firstStartedAt",
    )?;
    optional_finite(
        item.get("lastSettledAt").unwrap_or(&Value::Bool(false)),
        "history lastSettledAt",
    )
}

fn validate_history(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    let field = |name: &str| item.get(name).unwrap_or(&Value::Null);
    validate_history_counts(item)?;
    validate_totals(field("usage"))?;
    validate_history_by_kind(object(field("byKind"))?)?;
    finite(field("spentUsd"), "history spentUsd")?;
    finite_integer(field("unknownPricedCalls"), "history unknownPricedCalls", 0.0)?;
    validate_history_admissions(object(field("taskAdmissions"))?)
}

fn validate_turn_cost_fields(turns: &Map<String, Value>) -> Result<(), String> {
    if let Some(cost) = turns.get("cost")
        && finite(cost, "turn cost")? < 0.0
    {
        return Err(invalid("turn cost"));
    }
    if let Some(unpriced) = turns.get("unpriced") {
        finite_integer(unpriced, "turn unpriced", 0.0)?;
    }
    Ok(())
}

fn validate_root_turns(turns: &Value) -> Result<(), String> {
    let turns = object(turns)?;
    let field = |name: &str| turns.get(name).unwrap_or(&Value::Null);
    finite_integer(field("count"), "turn count", 0.0)?;
    if finite(field("tokens"), "turn tokens")? < 0.0 {
        return Err(invalid("turn tokens"));
    }
    finite(field("lastAt"), "turn lastAt")?;
    validate_turn_cost_fields(turns)
}

fn validate_root_imported(imported: &Value) -> Result<(), String> {
    let imported = object(imported)?;
    if imported.get("source").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(invalid("import"));
    }
    finite_integer(imported.get("entries").unwrap_or(&Value::Null), "import entries", 0.0)?;
    Ok(())
}

fn validate_unique_records(
    items: &[Value],
    check: fn(&Value) -> Result<(), String>,
    duplicate_err: &str,
) -> Result<(), String> {
    let mut ids: Vec<&str> = Vec::new();
    for item in items {
        check(item)?;
        let id = item["id"].as_str().unwrap_or_default();
        if ids.contains(&id) {
            return Err(duplicate_err.into());
        }
        ids.push(id);
    }
    Ok(())
}

fn validate_root_metadata(root_id: &str, root: &Map<String, Value>) -> Result<(), String> {
    if root.get("rootId").and_then(Value::as_str) != Some(root_id) || root_id.is_empty() {
        return Err(invalid("root"));
    }
    optional_finite(root.get("startedAt").unwrap_or(&Value::Bool(false)), "startedAt")?;
    optional_finite(root.get("deadlineAt").unwrap_or(&Value::Bool(false)), "deadlineAt")?;
    if let Some(turns) = root.get("turns") {
        validate_root_turns(turns)?;
    }
    if let Some(imported) = root.get("imported") {
        validate_root_imported(imported)?;
    }
    Ok(())
}

fn validate_root_entry(root_id: &str, root: &Value) -> Result<(), String> {
    let root = object(root)?;
    validate_root_metadata(root_id, root)?;
    let (Some(reservations), Some(calls)) = (
        root.get("reservations").and_then(Value::as_array),
        root.get("calls").and_then(Value::as_array),
    ) else {
        return Err(invalid("root records"));
    };
    validate_unique_records(reservations, validate_reservation, "Duplicate usage ledger reservation")?;
    validate_unique_records(calls, validate_call, "Duplicate usage ledger call")
}

pub(super) fn validate_document(value: &Value) -> Result<(), String> {
    let document = object(value)?;
    if document.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err("Unsupported usage ledger version".into());
    }
    if let Some(history) = document.get("history") {
        validate_history(history)?;
    }
    let roots = object(document.get("roots").unwrap_or(&Value::Null))?;
    for (root_id, root) in roots {
        validate_root_entry(root_id, root)?;
    }
    Ok(())
}
