use super::{
    EVIDENCE_CLASSES, ErrorCode, KINDS, MemoryError, OPERATION_FIELDS, RECEIPT_STATUSES, Result, STATES, evidence_refs,
    exact_fields, gate_decision, is_iso, is_mutation, is_sha256, nonempty, require_text, same_tags, scope_of,
    scope_tags,
};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

const INVALID: MemoryError = MemoryError::code(ErrorCode::InvalidJournal);

fn op_text(op: &Map<String, Value>, key: &str) -> String {
    op.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn validate_op_header(op: &Map<String, Value>, kind: &str, ids: &HashSet<String>) -> Result<()> {
    if !exact_fields(op, OPERATION_FIELDS)
        || !op.get("id").is_some_and(nonempty)
        || ids.contains(&op_text(op, "id"))
        || !KINDS.contains(&kind)
        || !op
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|s| STATES.contains(&s))
        || !matches!(op.get("phase").and_then(Value::as_str), Some("gate" | "backend"))
        || !op.get("createdAt").is_some_and(is_iso)
        || !op.get("updatedAt").is_some_and(is_iso)
    {
        return Err(INVALID);
    }
    Ok(())
}

fn validate_op_scalar_fields(op: &Map<String, Value>, kind: &str, scope: &str) -> Result<()> {
    for key in ["taskId", "memoryId"] {
        if let Some(value) = op.get(key) {
            require_text(value)?;
        }
    }
    if let Some(tags) = op.get("tags") {
        scope_tags(Some(tags), scope)?;
    }
    for key in ["queryHash", "textHash"] {
        if op.get(key).is_some_and(|value| !is_sha256(value)) {
            return Err(INVALID);
        }
    }
    if let Some(evidence) = op.get("evidence") {
        evidence_refs(evidence)?;
    }
    validate_op_gate_and_error(op, kind)
}

fn validate_op_gate_and_error(op: &Map<String, Value>, kind: &str) -> Result<()> {
    if let Some(gate) = op.get("gate") {
        let keys: &[&str] = if kind == "prepare" {
            &["retrieve", "probability"]
        } else {
            &["action", "confidence"]
        };
        if !gate.as_object().is_some_and(|gate| exact_fields(gate, keys)) {
            return Err(INVALID);
        }
        gate_decision(gate, kind == "prepare")?;
    }
    if let Some(error) = op.get("error")
        && !error.as_object().is_some_and(|error| {
            exact_fields(error, &["code"])
                && error
                    .get("code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| ErrorCode::parse(code).is_some())
        })
    {
        return Err(INVALID);
    }
    Ok(())
}

fn validate_op_references(references: &Value) -> Result<()> {
    let valid = references.as_array().is_some_and(|items| {
        items.iter().all(|reference| {
            reference.as_object().is_some_and(|r| {
                exact_fields(r, &["id", "textHash", "memoryId", "evidenceClass"])
                    && r.get("id").is_some_and(nonempty)
                    && r.get("textHash").is_some_and(is_sha256)
                    && r.get("memoryId").is_none_or(nonempty)
                    && r.get("evidenceClass")
                        .is_none_or(|c| c.as_str().is_some_and(|c| EVIDENCE_CLASSES.contains(&c)))
            })
        })
    });
    if valid { Ok(()) } else { Err(INVALID) }
}

fn validate_op_excluded(excluded: &Value) -> Result<()> {
    let valid = excluded.as_array().is_some_and(|items| {
        items.iter().all(|item| {
            item.as_object().is_some_and(|e| {
                exact_fields(e, &["id", "memoryId", "reason"])
                    && e.get("id").is_some_and(nonempty)
                    && e.get("memoryId").is_some_and(nonempty)
                    && matches!(
                        e.get("reason").and_then(Value::as_str),
                        Some("superseded" | "forgotten")
                    )
            })
        })
    });
    if valid { Ok(()) } else { Err(INVALID) }
}

fn validate_op_receipts(receipts: &Value) -> Result<()> {
    let valid = receipts.as_array().is_some_and(|items| {
        items.iter().all(|receipt| {
            receipt.as_object().is_some_and(|r| {
                exact_fields(r, &["id", "status"])
                    && r.get("id").is_some_and(nonempty)
                    && r.get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|s| RECEIPT_STATUSES.contains(&s))
            })
        })
    });
    if valid { Ok(()) } else { Err(INVALID) }
}

fn validate_op_ids(list: &Value) -> Result<()> {
    let items = list.as_array().ok_or(INVALID)?;
    let unique: HashSet<&str> = items.iter().filter_map(Value::as_str).collect();
    if items.iter().any(|id| !nonempty(id)) || unique.len() != items.len() {
        return Err(INVALID);
    }
    Ok(())
}

fn validate_op_collections(op: &Map<String, Value>) -> Result<()> {
    if let Some(list) = op.get("operationIds") {
        validate_op_ids(list)?;
    }
    if let Some(references) = op.get("references") {
        validate_op_references(references)?;
    }
    if let Some(excluded) = op.get("excluded") {
        validate_op_excluded(excluded)?;
    }
    if let Some(receipts) = op.get("receipts") {
        validate_op_receipts(receipts)?;
    }
    Ok(())
}

fn validate_op_kind_rules(
    op: &Map<String, Value>,
    kind: &str,
    operations: &[Value],
    ids: &HashSet<String>,
) -> Result<()> {
    if let Some(class) = op.get("evidenceClass")
        && (!matches!(kind, "propose" | "correct") || !class.as_str().is_some_and(|c| EVIDENCE_CLASSES.contains(&c)))
    {
        return Err(INVALID);
    }
    if let Some(supersedes) = op.get("supersedes") {
        let target = operations.iter().find(|other| other.get("id") == Some(supersedes));
        let valid = kind == "correct"
            && supersedes.as_str().is_some_and(|id| ids.contains(id))
            && target.is_some_and(|target| {
                target.get("memoryId") == op.get("memoryId")
                    && matches!(target["kind"].as_str(), Some("propose" | "correct"))
            });
        if !valid {
            return Err(INVALID);
        }
    }
    if op
        .get("observedState")
        .is_some_and(|s| !s.as_str().is_some_and(|s| STATES.contains(&s)))
    {
        return Err(INVALID);
    }
    Ok(())
}

fn validate_op_backend(op: &Map<String, Value>, kind: &str) -> Result<()> {
    if !is_mutation(kind) || op_text(op, "phase") != "backend" {
        return Ok(());
    }
    if !op.get("memoryId").is_some_and(nonempty) || !op.contains_key("tags") {
        return Err(INVALID);
    }
    let state = op_text(op, "state");
    let ids_ok = op
        .get("operationIds")
        .and_then(Value::as_array)
        .is_some_and(|ids| !(matches!(state.as_str(), "started" | "accepted" | "unknown") && ids.is_empty()));
    if kind != "forget" && !ids_ok {
        return Err(INVALID);
    }
    Ok(())
}

fn validate_op_required_and_backend(op: &Map<String, Value>, kind: &str) -> Result<()> {
    if kind == "prepare" && (!op.get("taskId").is_some_and(nonempty) || !op.get("queryHash").is_some_and(is_sha256)) {
        return Err(INVALID);
    }
    if matches!(kind, "propose" | "correct")
        && (!op.get("textHash").is_some_and(is_sha256) || !op.contains_key("evidence"))
    {
        return Err(INVALID);
    }
    validate_op_backend(op, kind)
}

fn validate_op_ownership(
    op: &Map<String, Value>,
    item: &Value,
    kind: &str,
    scope: &str,
    owners: &mut HashMap<String, Value>,
) -> Result<()> {
    if matches!(kind, "correct" | "get" | "forget") {
        let owner = owners.get(&op_text(op, "memoryId")).ok_or(INVALID)?;
        let owner_tags: Vec<String> = owner["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.as_str().map(str::to_string))
            .collect();
        if owner["scope"] != scope || op.get("tags").is_some_and(|tags| !same_tags(tags, &owner_tags)) {
            return Err(INVALID);
        }
    }
    if kind == "propose" && op_text(op, "phase") == "backend" {
        let memory_id = op_text(op, "memoryId");
        if owners.contains_key(&memory_id) {
            return Err(INVALID);
        }
        owners.insert(memory_id, item.clone());
    }
    Ok(())
}

fn validate_operation(
    item: &Value,
    operations: &[Value],
    ids: &mut HashSet<String>,
    owners: &mut HashMap<String, Value>,
) -> Result<()> {
    let op = item.as_object().ok_or(INVALID)?;
    let kind = op_text(op, "kind");
    validate_op_header(op, &kind, ids)?;
    ids.insert(op_text(op, "id"));
    let scope = scope_of(op.get("scope").unwrap_or(&Value::Null))?;
    validate_op_scalar_fields(op, &kind, scope)?;
    validate_op_collections(op)?;
    validate_op_kind_rules(op, &kind, operations, ids)?;
    validate_op_required_and_backend(op, &kind)?;
    validate_op_ownership(op, item, &kind, scope, owners)
}

fn read_journal_inner(value: &Value, namespace: &str) -> Result<Vec<Value>> {
    let document = value.as_object().ok_or(INVALID)?;
    if !exact_fields(document, &["version", "namespace", "operations"])
        || document.get("version") != Some(&json!(1))
        || !document.get("namespace").is_some_and(nonempty)
    {
        return Err(INVALID);
    }
    let operations = document.get("operations").and_then(Value::as_array).ok_or(INVALID)?;
    if document["namespace"] != namespace {
        return Err(MemoryError::code(ErrorCode::BackendMismatch));
    }
    let mut ids = HashSet::new();
    let mut owners: HashMap<String, Value> = HashMap::new();
    for item in operations {
        validate_operation(item, operations, &mut ids, &mut owners)?;
    }
    Ok(operations.clone())
}

/// `readJournal`: rejects unknown persisted fields and inconsistent histories rather than re-persisting them.
pub(super) fn read_journal(value: &Value, namespace: &str) -> Result<Vec<Value>> {
    read_journal_inner(value, namespace).map_err(|error| {
        if error.code == ErrorCode::BackendMismatch {
            error
        } else {
            INVALID
        }
    })
}
