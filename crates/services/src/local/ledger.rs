use super::{Branch, fields, identity, nonempty, proposal};
use serde_json::{Map, Value, json};

/// One refinement (`RefinementRecord`).
#[derive(Clone, Debug)]
pub(super) struct Record {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) target: String,
    pub(super) base_version: Value,
    pub(super) content: Value,
    pub(super) evidence: Value,
    pub(super) scope: Value,
    pub(super) version: Option<i64>,
    pub(super) previous_id: Option<String>,
    pub(super) state: &'static str,
    pub(super) approval: Option<&'static str>,
    pub(super) history: Vec<Value>,
}

impl Record {
    pub(super) fn json(&self) -> Value {
        json!({
            "id": self.id, "kind": self.kind, "target": self.target, "baseVersion": self.base_version,
            "content": self.content, "evidence": self.evidence, "scope": self.scope, "version": self.version,
            "previousId": self.previous_id, "state": self.state, "approval": self.approval, "history": self.history,
        })
    }

    fn transition(&mut self, state: &'static str, at: &str, cause: &str) {
        self.state = state;
        self.history.push(json!({"state": state, "at": at, "cause": cause}));
    }
}

pub(super) fn lookup(records: &[Record], id: &Value) -> Result<usize, String> {
    let id = id.as_str().filter(|id| !id.is_empty()).ok_or("Invalid refinement ID")?;
    records
        .iter()
        .position(|record| record.id == id)
        .ok_or_else(|| format!("Unknown refinement: {id}"))
}

pub(super) fn active(records: &[Record], kind: &str, target: &str) -> Option<usize> {
    records
        .iter()
        .position(|record| record.kind == kind && record.target == target && record.state == "active")
}

fn on_branch(event: &Value, branch: Option<&Branch>) -> bool {
    let Some(test) = branch.and_then(|branch| branch.on_branch.as_ref()) else {
        return true;
    };
    match event.as_object().and_then(|event| event.get("anchor")) {
        Some(Value::String(anchor)) => test(anchor),
        _ => true,
    }
}

fn action_optional_fields(action: &str) -> &'static [&'static str] {
    match action {
        "propose" => &["proposal"],
        "activate" => &["approval", "anchor"],
        "rollback" => &["anchor"],
        _ => &[],
    }
}

type EventHeader<'a> = (&'a Map<String, Value>, &'a str, &'a str, &'a str);

fn parse_event_header(event: &Value) -> Result<EventHeader<'_>, String> {
    let map = event.as_object().ok_or("Malformed refinement event")?;
    let action = map.get("action").and_then(Value::as_str).unwrap_or_default();
    fields(event, &["action", "id", "at"], action_optional_fields(action))?;
    let id = map["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("Malformed refinement event")?;
    let at = map["at"]
        .as_str()
        .filter(|at| jsrt::date::parse_date(at).is_some())
        .ok_or("Malformed refinement event")?;
    Ok((map, action, id, at))
}

fn apply_propose(records: &mut Vec<Record>, map: &Map<String, Value>, id: &str, at: &str) -> Result<usize, String> {
    let data = proposal(map.get("proposal").unwrap_or(&Value::Null))?;
    if records.iter().any(|record| record.id == id) {
        return Err("Duplicate refinement ID".into());
    }
    let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
    let mut record = Record {
        id: id.into(),
        kind: text("kind"),
        target: text("target"),
        base_version: data["baseVersion"].clone(),
        content: data["content"].clone(),
        evidence: data["evidence"].clone(),
        scope: data["scope"].clone(),
        version: None,
        previous_id: None,
        state: "proposed",
        approval: None,
        history: Vec::new(),
    };
    record.transition("proposed", at, id);
    records.push(record);
    Ok(records.len() - 1)
}

fn validate_activation_record(records: &[Record], index: usize) -> Result<(Option<usize>, i64), String> {
    let record = &records[index];
    if record.state != "proposed" {
        return Err("Only a proposed refinement can be activated".into());
    }
    identity(&json!(record.kind), &json!(record.target))?;
    if !nonempty(&record.evidence) {
        return Err("Activation requires nonempty evidence".into());
    }
    let previous = active(records, &record.kind, &record.target);
    let current = previous.and_then(|p| records[p].version).unwrap_or(0);
    if record.base_version.as_f64() != Some(current as f64) {
        return Err("Stale base version".into());
    }
    let version = records
        .iter()
        .filter(|item| item.kind == record.kind && item.target == record.target)
        .map(|item| item.version.unwrap_or(0))
        .fold(0, i64::max);
    if version + 1 > jsrt::number::MAX_SAFE_INTEGER {
        return Err("Version exhausted".into());
    }
    Ok((previous, version + 1))
}

fn validate_activation_approval(record: &Record, approval: Option<&Value>) -> Result<Option<&'static str>, String> {
    if approval.is_some_and(|a| a != "approved" && a != "not_required") {
        return Err("Malformed refinement approval".into());
    }
    let claims = approval.is_some_and(|a| a == "approved");
    if claims && record.approval != Some("approved") {
        return Err("Activation claims an approval that was not recorded".into());
    }
    if record.approval == Some("approved") {
        Ok(record.approval)
    } else {
        Ok(approval.is_some_and(|a| a == "not_required").then_some("not_required"))
    }
}

fn apply_activate(
    records: &mut [Record],
    index: usize,
    map: &Map<String, Value>,
    id: &str,
    at: &str,
) -> Result<(), String> {
    let (previous, next_version) = validate_activation_record(records, index)?;
    let next_approval = validate_activation_approval(&records[index], map.get("approval"))?;
    let previous_id = previous.map(|p| records[p].id.clone());
    let record = &mut records[index];
    record.version = Some(next_version);
    record.previous_id = previous_id;
    record.approval = next_approval;
    if let Some(previous) = previous {
        records[previous].transition("superseded", at, id);
    }
    records[index].transition("active", at, id);
    Ok(())
}

fn apply_approve(record: &mut Record) -> Result<(), String> {
    if record.state != "proposed" {
        return Err("Only a proposed refinement can be approved".into());
    }
    record.approval = Some("approved");
    Ok(())
}

fn apply_reject(record: &mut Record, id: &str, at: &str) -> Result<(), String> {
    if record.state != "proposed" {
        return Err("Only a proposed refinement can be rejected".into());
    }
    record.transition("rejected", at, id);
    Ok(())
}

fn apply_rollback(records: &mut [Record], index: usize, id: &str, at: &str) -> Result<(), String> {
    let record = &records[index];
    if active(records, &record.kind, &record.target) != Some(index) {
        return Err("Only the latest active refinement can be rolled back".into());
    }
    let previous = record.previous_id.clone();
    records[index].transition("rolled_back", at, id);
    if let Some(previous) = previous {
        let previous = lookup(records, &json!(previous))?;
        records[previous].transition("active", at, id);
    }
    Ok(())
}

fn apply_existing(
    records: &mut [Record],
    index: usize,
    action: &str,
    map: &Map<String, Value>,
    id: &str,
    at: &str,
) -> Result<(), String> {
    match action {
        "activate" => apply_activate(records, index, map, id, at),
        "approve" => apply_approve(&mut records[index]),
        "reject" => apply_reject(&mut records[index], id, at),
        "rollback" => apply_rollback(records, index, id, at),
        _ => Err("Unknown refinement action".into()),
    }
}

/// `apply(records, event)`: one ledger event; returns the index of the record it changed.
pub(super) fn apply(records: &mut Vec<Record>, event: &Value) -> Result<usize, String> {
    let (map, action, id, at) = parse_event_header(event)?;
    if action == "propose" {
        return apply_propose(records, map, id, at);
    }
    let index = lookup(records, &map["id"])?;
    apply_existing(records, index, action, map, id, at)?;
    Ok(index)
}

pub(super) fn replay_refinement_document(
    stored: &Value,
    branch: Option<&Branch>,
) -> Result<(Vec<Record>, Vec<Value>), String> {
    let document = fields(stored, &["formatVersion", "events"], &[])?;
    let list = document["events"].as_array();
    if document["formatVersion"].as_f64() != Some(1.0) || list.is_none() {
        return Err("Unsupported document format".into());
    }
    let events = list.cloned().unwrap_or_default();
    let mut records = Vec::new();
    for event in &events {
        if on_branch(event, branch) {
            apply(&mut records, event)?;
        }
    }
    Ok((records, events))
}
