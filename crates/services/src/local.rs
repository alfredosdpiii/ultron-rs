//! `refinements.*` and `experiments.*` of `ultron/local-services.ts` (`NativeLocalServices`): versioned
//! refinements (lessons, instruction and skill changes) with their proposal, approval, activation, rejection and
//! rollback flow over one append-only event ledger, and the experiment history of recorded runs.
//!
//! Requests (Python reaches them through `refinements.*` of `memory_api.py` and `rlm.host_request`):
//! - `refinements.propose` `{kind, target, baseVersion, content, evidence?, scope?}`
//! - `refinements.get` / `activate` / `approve` / `reject` / `rollback` `{id}`; `current` `{kind, target}`; `list` `{}`
//! - `experiments.record` `{run}`; `compare` `{baseline, candidate}`; `list` `{}`
//!
//! Guarantees the host enforces here, never the model: a target names a refinement kind and never human-owned
//! authority (policy, grants, acceptance, approval, trusted instructions); content cannot request tools, grants,
//! models, budgets or permissions, at proposal and again at activation; activation needs nonempty evidence, the
//! current base version and the domain validation; approval (off by default) never overrides validation; a
//! decided record is never re-opened. Every mutation is one appended event; reads replay the ledger, keeping only
//! the events anchored on the caller's conversation branch (A07). Experiments are append-only, and only runs on
//! the same frozen fixture compare. Calls serialize; a failed write makes the service unusable until reopened.

mod experiments;
mod ledger;
mod project;

pub use project::{
    BranchSource, InstructionHook, LocalServicesModule, check_refinement_safety, instruction_hook,
    instruction_hook_with_memory, local_module, refinement_view, safe_project_refinements,
};

use kernel_apis::artifacts::KeyedDocuments;
use kernel_apis::store::read_versioned;
use kernel_apis::util::{Clock, system_clock};
use ledger::{Record, active, apply, lookup, replay_refinement_document};
use serde_json::{Map, Value, json};
use session_journal::ns::REFINEMENTS;
use std::sync::{Arc, Mutex};

const KINDS: [&str; 4] = ["observation", "instruction", "skill", "agent"];
const PROTECTED_NAMES: [&str; 3] = ["agents.md", "security", "policy"];
/// Name segments of human-owned authority: trusted instructions, security policy, grants, the acceptance instrument,
/// judge thresholds and approval. Compared case- and separator-insensitively against every segment of a target.
const PROTECTED_TOKENS: [&str; 19] = [
    "agents",
    "agentsmd",
    "security",
    "policy",
    "policies",
    "grant",
    "grants",
    "permission",
    "permissions",
    "capability",
    "capabilities",
    "acceptance",
    "instrument",
    "judge",
    "threshold",
    "thresholds",
    "approval",
    "approvals",
    "sandbox",
];
/// Content keys that would grant tools, models, budgets or permissions if a consumer honored them.
const CAPABILITY_KEYS: [&str; 16] = [
    "allowedtools",
    "tools",
    "tool",
    "grant",
    "grants",
    "permission",
    "permissions",
    "capability",
    "capabilities",
    "model",
    "budget",
    "timeout",
    "policy",
    "approval",
    "sandbox",
    "env",
];
const MALFORMED: &str = "Malformed local service fields";
const UNCERTAIN: &str = "Document durability is uncertain; reopen the service before use";

/// The domain check run on activation only: `Ok(true)` passes, `Ok(false)` refuses it as failed validation, an
/// error refuses it with that message (TS `validate` returning true/undefined, false, or throwing).
pub type Validator = Arc<dyn Fn(&str, &Value) -> Result<bool, String> + Send + Sync>;
/// A fresh record id (`crypto.randomUUID()`).
pub type Ids = Arc<dyn Fn() -> String + Send + Sync>;
/// Whether an anchor lies on the conversation branch in use.
pub type OnBranch = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// `NativeLocalServicesOptions` for refinements and experiments.
#[derive(Clone)]
pub struct LocalOptions {
    pub validate: Option<Validator>,
    /// Opt-in control: activation requires a prior `refinements.approve`. Off by default.
    pub require_approval: bool,
    pub now: Clock,
    pub ids: Ids,
}

impl Default for LocalOptions {
    fn default() -> Self {
        LocalOptions {
            validate: None,
            require_approval: false,
            now: system_clock(),
            ids: Arc::new(jsrt::random::uuid_v4),
        }
    }
}

/// The conversation branch a request is made on (`RefinementBranch`).
#[derive(Clone, Default)]
pub struct Branch {
    /// The entry the change is made at; `None` for an empty conversation.
    pub anchor: Option<String>,
    /// Whether an anchor lies on the branch in use; `None` applies every event.
    pub on_branch: Option<OnBranch>,
}

/// `value.toLowerCase().replace(/[-_\s]/g, "")`.
fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && !jsrt::string::is_space(*ch))
        .collect()
}

/// Positions where `text` has `\r?\n---` (`/\r?\n---/` tried at each index, `\r` first).
fn closing_fence(text: &str, from: usize, then_newline: bool) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let newline_at = |at: usize| -> Option<usize> {
        if bytes.get(at) == Some(&b'\r') && bytes.get(at + 1) == Some(&b'\n') {
            Some(at + 2)
        } else if bytes.get(at) == Some(&b'\n') {
            Some(at + 1)
        } else {
            None
        }
    };
    (from..=text.len()).find_map(|at| {
        let after = newline_at(at)?;
        if !text[after..].starts_with("---") {
            return None;
        }
        let end = after + 3;
        if !then_newline {
            return Some((at, end));
        }
        newline_at(end).map(|body| (at, body))
    })
}

/// `/^---\r?\n([\s\S]*?)\r?\n---/` on `text`: the frontmatter, and where the text after the closing fence starts
/// (past its newline when `then_newline`, as `/...\r?\n---\r?\n([\s\S]*)$/`).
pub(crate) fn frontmatter(text: &str, then_newline: bool) -> Option<(&str, &str)> {
    let start = if text.starts_with("---\r\n") {
        5
    } else if text.starts_with("---\n") {
        4
    } else {
        return None;
    };
    let (close, rest) = closing_fence(text, start, then_newline)?;
    Some((&text[start..close], &text[rest..]))
}

fn frontmatter_line_key(line: &str) -> Option<&str> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let rest = line.trim_start_matches(jsrt::string::is_space);
    let mut chars = rest.char_indices();
    if !chars.next().is_some_and(|(_, ch)| ch.is_ascii_alphabetic()) {
        return None;
    }
    let end = chars
        .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-'))
        .map_or(rest.len(), |(at, _)| at);
    rest[end..]
        .trim_start_matches(jsrt::string::is_space)
        .starts_with(':')
        .then_some(&rest[..end])
}

fn frontmatter_escalation(text: &str) -> Option<String> {
    let (front, _) = frontmatter(text.trim_start_matches(jsrt::string::is_space), false)?;
    front.split('\n').find_map(|line| {
        let key = frontmatter_line_key(line)?;
        CAPABILITY_KEYS
            .contains(&normalize(key).as_str())
            .then(|| key.to_string())
    })
}

/// Capability non-escalation: a structured or frontmatter key that would claim authority.
fn escalation(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => frontmatter_escalation(text),
        Value::Array(items) => items.iter().find_map(escalation),
        Value::Object(map) => map.iter().find_map(|(key, item)| {
            if CAPABILITY_KEYS.contains(&normalize(key).as_str()) {
                Some(key.clone())
            } else {
                escalation(item)
            }
        }),
        _ => None,
    }
}

/// `sameJson`: deep equality with object keys in any order (numbers compare as JavaScript numbers).
fn same_json(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_json(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| same_json(x, y)))
        }
        _ => left == right,
    }
}

/// `fields(value, required, optional)`: an object with every required key and no other than the optional ones.
fn fields<'a>(value: &'a Value, required: &[&str], optional: &[&str]) -> Result<&'a Map<String, Value>, String> {
    let map = value.as_object().ok_or(MALFORMED)?;
    if required.iter().any(|key| !map.contains_key(*key))
        || map
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(MALFORMED.into());
    }
    Ok(map)
}

/// `/^[a-z]+:[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*$/`.
fn target_shape(target: &str) -> bool {
    let Some((namespace, name)) = target.split_once(':') else {
        return false;
    };
    let word = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    !namespace.is_empty() && namespace.bytes().all(|b| b.is_ascii_lowercase()) && name.split('.').all(word)
}

/// `identity(kind, target)`: a known kind, and a `kind:name` target that names no protected authority.
fn identity(kind: &Value, target: &Value) -> Result<(String, String), String> {
    let kind = kind
        .as_str()
        .filter(|kind| KINDS.contains(kind))
        .ok_or("Invalid refinement kind")?;
    let target = target
        .as_str()
        .filter(|target| target_shape(target))
        .ok_or("Target must be an explicit namespace:name ID, not a path")?;
    let (namespace, name) = target.split_once(':').unwrap_or_default();
    if namespace != kind
        || PROTECTED_NAMES.contains(&name.to_lowercase().as_str())
        || PROTECTED_TOKENS.contains(&normalize(name).as_str())
        || name
            .split(['.', '-', '_'])
            .any(|segment| PROTECTED_TOKENS.contains(&normalize(segment).as_str()))
    {
        return Err("Forbidden refinement target".into());
    }
    Ok((kind.to_string(), target.to_string()))
}

fn nonempty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !jsrt::string::trim(text).is_empty(),
        Value::Array(items) => items.iter().any(nonempty),
        Value::Object(map) => map.values().any(nonempty),
        _ => true,
    }
}

fn validate_proposal_content(content: &Value) -> Result<(), String> {
    if !(content.is_string() || content.is_object() || content.is_array()) || !nonempty(content) {
        return Err("Content must be nonempty JSON text, an object, or an array".into());
    }
    Ok(())
}

fn validate_proposal_extras(map: &Map<String, Value>) -> Result<(Value, Value), String> {
    let evidence = map.get("evidence").cloned().unwrap_or_else(|| json!([]));
    if evidence.is_number() || evidence.is_boolean() {
        return Err("Evidence must be JSON text, an object, or an array".into());
    }
    let scope = map.get("scope").cloned().unwrap_or_else(|| json!("session"));
    if scope.as_str().is_none_or(|scope| jsrt::string::trim(scope).is_empty()) {
        return Err("Scope must be a nonempty string".into());
    }
    Ok((evidence, scope))
}

/// `proposal(value)`: `{kind, target, baseVersion, content, evidence, scope}` with the defaults filled in.
fn proposal(value: &Value) -> Result<Value, String> {
    let map = fields(
        value,
        &["kind", "target", "baseVersion", "content"],
        &["evidence", "scope"],
    )?;
    let (kind, target) = identity(&map["kind"], &map["target"])?;
    let base = &map["baseVersion"];
    if jsrt::number::safe_integer(base).is_none_or(|version| version < 0) {
        return Err("baseVersion must be a nonnegative safe integer".into());
    }
    let content = &map["content"];
    validate_proposal_content(content)?;
    let (evidence, scope) = validate_proposal_extras(map)?;
    Ok(json!({
        "kind": kind, "target": target, "baseVersion": base, "content": content, "evidence": evidence, "scope": scope,
    }))
}

fn is_duplicate_proposal(record: &Record, data: &Value) -> bool {
    record.kind == data["kind"]
        && record.target == data["target"]
        && record.scope == data["scope"]
        && same_json(&record.content, &data["content"])
        && (record.state == "proposed"
            || record.state == "active"
            || (record.state == "rejected" && same_json(&record.evidence, &data["evidence"])))
}

/// Refinements and experiments over keyed durable documents (one service instance per owner).
pub struct LocalServices {
    documents: Box<dyn KeyedDocuments>,
    options: LocalOptions,
    /// A failed write left durability unknown.
    uncertain: Mutex<bool>,
}

impl LocalServices {
    pub fn new(documents: Box<dyn KeyedDocuments>, options: LocalOptions) -> Self {
        LocalServices {
            documents,
            options,
            uncertain: Mutex::new(false),
        }
    }

    /// The session worker's service (`createWorkerServices`): activation runs the worker's content checks
    /// ([`crate::validation::validate_refinement_content`]); approval stays off.
    pub fn worker(documents: Box<dyn KeyedDocuments>) -> Self {
        LocalServices::new(
            documents,
            LocalOptions {
                validate: Some(crate::validation::worker_validator()),
                ..LocalOptions::default()
            },
        )
    }

    /// Serves one request; `branch` is the caller's conversation branch (none: every event applies).
    pub fn handle(&self, kind: &str, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let mut uncertain = jsrt::sync::lock(&self.uncertain);
        if *uncertain {
            return Err(UNCERTAIN.into());
        }
        let persist = |key: &str, value: &Value, uncertain: &mut bool| -> Result<(), String> {
            self.documents.set(key, value).inspect_err(|_| *uncertain = true)
        };
        match kind {
            "refinements.propose"
            | "refinements.list"
            | "refinements.get"
            | "refinements.current"
            | "refinements.activate"
            | "refinements.approve"
            | "refinements.reject"
            | "refinements.rollback" => self.refinements(kind, payload, branch, &mut |key, value| {
                persist(key, value, &mut uncertain)
            }),
            "experiments.record" | "experiments.list" | "experiments.compare" => {
                self.experiments(kind, payload, &mut |key, value| persist(key, value, &mut uncertain))
            }
            _ => Err(format!("Unknown local service request: {kind}")),
        }
    }

    pub fn current(&self, kind: &str, target: &str, branch: Option<&Branch>) -> Result<Option<Value>, String> {
        let value = self.handle("refinements.current", &json!({"kind": kind, "target": target}), branch)?;
        Ok((!value.is_null()).then_some(value))
    }

    fn now(&self) -> String {
        jsrt::date::iso_string((self.options.now)())
    }

    fn records(&self, branch: Option<&Branch>) -> Result<(Vec<Record>, Vec<Value>), String> {
        let saved = self.documents.get(REFINEMENTS.key)?;
        let Some(stored) = read_versioned(&REFINEMENTS.address(), saved, REFINEMENTS.version_field)? else {
            return Ok((Vec::new(), Vec::new()));
        };
        replay_refinement_document(&stored, branch).map_err(|_| "Invalid refinement document".to_string())
    }

    fn build_propose_event(&self, records: &[Record], payload: &Value) -> Result<Value, String> {
        let data = proposal(payload)?;
        if let Some(key) = escalation(&data["content"]) {
            return Err(format!("Refinement content cannot request capabilities ({key})"));
        }
        if let Some(duplicate) = records.iter().find(|record| is_duplicate_proposal(record, &data)) {
            return Err(format!(
                "Duplicate refinement proposal: {} ({})",
                duplicate.id, duplicate.state
            ));
        }
        Ok(json!({
            "action": "propose",
            "id": (self.options.ids)(),
            "proposal": data,
            "at": self.now(),
        }))
    }

    fn prepare_activation_approval(&self, pending: &Record) -> Result<&'static str, String> {
        if let Some(key) = escalation(&pending.content) {
            return Err(format!("Refinement content cannot request capabilities ({key})"));
        }
        if self.options.require_approval && pending.approval != Some("approved") {
            return Err("Activation requires approval".into());
        }
        if pending.approval == Some("approved") {
            Ok("approved")
        } else {
            Ok("not_required")
        }
    }

    fn build_mutation_event(
        &self,
        kind: &str,
        records: &[Record],
        payload: &Value,
        branch: Option<&Branch>,
    ) -> Result<Value, String> {
        let map = fields(payload, &["id"], &[])?;
        let action = &kind["refinements.".len()..];
        let mut event = Map::new();
        event.insert("action".into(), action.into());
        event.insert("id".into(), map["id"].clone());
        event.insert("at".into(), self.now().into());
        if (action == "activate" || action == "rollback")
            && let Some(anchor) = branch.and_then(|branch| branch.anchor.clone())
        {
            event.insert("anchor".into(), anchor.into());
        }
        if action == "activate" {
            let pending = &records[lookup(records, &map["id"])?];
            let approval = self.prepare_activation_approval(pending)?;
            event.insert("approval".into(), approval.into());
        }
        Ok(Value::Object(event))
    }

    fn apply_and_persist_refinement(
        &self,
        records: &mut Vec<Record>,
        mut events: Vec<Value>,
        event: Value,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let index = apply(records, &event)?;
        let result = &records[index];
        if event["action"] == "activate"
            && let Some(validate) = &self.options.validate
            && !validate(&result.kind, &result.content)?
        {
            return Err("Refinement content validation failed".into());
        }
        events.push(event);
        persist(REFINEMENTS.key, &json!({"formatVersion": 1, "events": events}))?;
        Ok(result.json())
    }

    fn list_refinements(records: &[Record], payload: &Value) -> Result<Value, String> {
        fields(payload, &[], &[])?;
        Ok(Value::Array(records.iter().map(Record::json).collect()))
    }

    fn current_refinement(records: &[Record], payload: &Value) -> Result<Value, String> {
        let map = fields(payload, &["kind", "target"], &[])?;
        let (kind, target) = identity(&map["kind"], &map["target"])?;
        Ok(active(records, &kind, &target).map_or(Value::Null, |index| records[index].json()))
    }

    fn get_refinement(records: &[Record], payload: &Value) -> Result<Value, String> {
        let map = fields(payload, &["id"], &[])?;
        Ok(records[lookup(records, &map["id"])?].json())
    }

    fn mutate_refinement(
        &self,
        kind: &str,
        mut records: Vec<Record>,
        events: Vec<Value>,
        payload: &Value,
        branch: Option<&Branch>,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let event = if kind == "refinements.propose" {
            self.build_propose_event(&records, payload)?
        } else {
            self.build_mutation_event(kind, &records, payload, branch)?
        };
        self.apply_and_persist_refinement(&mut records, events, event, persist)
    }

    fn refinements(
        &self,
        kind: &str,
        payload: &Value,
        branch: Option<&Branch>,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let (records, events) = self.records(branch)?;
        match kind {
            "refinements.list" => Self::list_refinements(&records, payload),
            "refinements.current" => Self::current_refinement(&records, payload),
            "refinements.get" => Self::get_refinement(&records, payload),
            _ => self.mutate_refinement(kind, records, events, payload, branch, persist),
        }
    }
}
