use super::{EVIDENCE_CLASSES, MemoryService, exact_fields, is_sha256, page_budget};
use kernel::Signal;
use serde_json::{Map, Value};

fn string_min1(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| !text.is_empty())
}

fn optional_in(value: Option<&Value>, allowed: &[&str]) -> bool {
    value.is_none_or(|value| value.as_str().is_some_and(|text| allowed.contains(&text)))
}

fn evidence_schema(value: Option<&Value>) -> bool {
    value.and_then(Value::as_array).is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|item| {
                item.as_object().is_some_and(|map| {
                    exact_fields(map, &["ref", "sha256", "quote"])
                        && string_min1(map.get("ref"))
                        && map.get("sha256").is_none_or(is_sha256)
                        && map.get("quote").is_none_or(|quote| string_min1(Some(quote)))
                })
            })
    })
}

pub(super) const SCOPES: &[&str] = &["session", "project", "global"];

fn optional_string_min1(value: Option<&Value>) -> bool {
    value.is_none_or(|v| string_min1(Some(v)))
}

fn optional_kebab(value: Option<&Value>) -> bool {
    value.is_none_or(|t| t.as_str().is_some_and(crate::memory_records::is_kebab))
}

fn valid_prepare(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && string_min1(payload.get("query"))
        && string_min1(payload.get("taskId"))
        && optional_in(payload.get("scope"), SCOPES)
        && payload.get("refresh").is_none_or(Value::is_boolean)
}

fn valid_propose(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && string_min1(payload.get("text"))
        && evidence_schema(payload.get("evidence"))
        && optional_in(payload.get("scope"), SCOPES)
        && optional_in(payload.get("evidenceClass"), EVIDENCE_CLASSES)
        && optional_in(payload.get("type"), &crate::memory_records::RECORD_TYPES)
        && optional_string_min1(payload.get("key"))
        && optional_string_min1(payload.get("situation"))
        && payload.get("pathGlobs").is_none_or(|globs| {
            globs
                .as_array()
                .is_some_and(|items| items.iter().all(|g| string_min1(Some(g))))
        })
}

fn valid_correct(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && string_min1(payload.get("id"))
        && string_min1(payload.get("text"))
        && evidence_schema(payload.get("evidence"))
        && optional_in(payload.get("evidenceClass"), EVIDENCE_CLASSES)
}

fn valid_id_only(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok() && string_min1(payload.get("id"))
}

fn valid_reflect(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && string_min1(payload.get("query"))
        && optional_in(payload.get("scope"), SCOPES)
}

fn valid_invalidate(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && string_min1(payload.get("id"))
        && optional_string_min1(payload.get("reason"))
}

fn valid_why(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok() && string_min1(payload.get("taskId"))
}

fn valid_list(_kind: &str, payload: &Map<String, Value>) -> bool {
    payload.is_empty()
}

fn valid_dream(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && optional_string_min1(payload.get("project"))
        && payload
            .get("days")
            .is_none_or(|days| days.as_i64().is_some_and(|d| (0..=3650).contains(&d)))
        && payload
            .get("limit")
            .is_none_or(|limit| limit.as_u64().is_some_and(|n| (1..=200).contains(&n)))
}

fn valid_brief(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && optional_string_min1(payload.get("project"))
        && string_min1(payload.get("text"))
        && optional_in(payload.get("scope"), &["project", "global"])
        && optional_kebab(payload.get("topic"))
}

fn valid_topic(kind: &str, payload: &Map<String, Value>) -> bool {
    kernel::ops::fields(kind, payload).is_ok()
        && optional_string_min1(payload.get("project"))
        && optional_in(payload.get("scope"), &["project", "global"])
        && optional_kebab(payload.get("topic"))
}

type Validator = fn(&str, &Map<String, Value>) -> bool;

const VALIDATORS: &[(&str, Validator)] = &[
    ("memory.prepare", valid_prepare),
    ("memory.propose", valid_propose),
    ("memory.correct", valid_correct),
    ("memory.get", valid_id_only),
    ("memory.forget", valid_id_only),
    ("memory.reflect", valid_reflect),
    ("memory.invalidate", valid_invalidate),
    ("memory.why", valid_why),
    ("memory.list", valid_list),
    ("memory.dream", valid_dream),
    ("memory.brief", valid_brief),
    ("memory.topic", valid_topic),
];

/// The worker's payload schemas (`Check(prepare | propose | correct | id | why, payload)`).
fn payload_valid(kind: &str, payload: &Map<String, Value>) -> bool {
    VALIDATORS
        .iter()
        .find(|(name, _)| *name == kind)
        .is_some_and(|(_, check)| check(kind, payload))
}

fn handle_why(service: &MemoryService, payload: &Map<String, Value>, _signal: &Signal) -> super::Result<Value> {
    service.why(payload.get("taskId").unwrap_or(&Value::Null))
}

fn handle_list(service: &MemoryService, _payload: &Map<String, Value>, _signal: &Signal) -> super::Result<Value> {
    service.list()
}

type Handler = fn(&MemoryService, &Map<String, Value>, &Signal) -> super::Result<Value>;

const HANDLERS: &[(&str, Handler)] = &[
    ("memory.prepare", MemoryService::prepare),
    ("memory.propose", MemoryService::propose),
    ("memory.correct", MemoryService::correct),
    ("memory.get", MemoryService::get),
    ("memory.forget", MemoryService::forget),
    ("memory.reflect", MemoryService::reflect),
    ("memory.invalidate", MemoryService::invalidate),
    ("memory.why", handle_why),
    ("memory.dream", MemoryService::dream),
    ("memory.brief", MemoryService::write_page),
    ("memory.topic", MemoryService::topic),
    ("memory.list", handle_list),
];

/// Serves one `memory.*` request as the session worker does. `service` is `None` when Hindsight is not configured.
pub fn handle(
    service: Option<&MemoryService>,
    kind: &str,
    payload: &Map<String, Value>,
    signal: &Signal,
) -> std::result::Result<Value, String> {
    if signal.is_aborted() {
        return Err(signal.reason().unwrap_or_default());
    }
    let service = service.ok_or("Hindsight is not configured. Set ULTRON_HINDSIGHT_URL.")?;
    if !payload_valid(kind, payload) {
        return Err(format!("Invalid memory request: {kind}"));
    }
    if service.backend.supports("claims") && payload.get("evidenceClass").and_then(Value::as_str) == Some("verified") {
        return Err(format!(
            "{kind}: evidenceClass \"verified\" is set only by the host's checks (dreaming), not by a call"
        ));
    }
    if kind == "memory.brief"
        && let Err(reason) = page_budget(
            payload.get("text").and_then(Value::as_str).unwrap_or_default(),
            payload.contains_key("topic"),
        )
    {
        return Err(format!("memory.brief: {reason}"));
    }
    let dispatch = HANDLERS
        .iter()
        .find(|(name, _)| *name == kind)
        .map_or(handle_list as Handler, |(_, handler)| *handler);
    dispatch(service, payload, signal).map_err(|error| error.to_string())
}

/// `memory.*` as a host module; `service` is `None` in a worker without Hindsight.
pub struct MemoryApi {
    pub service: Option<MemoryService>,
}

impl crate::ApiModule for MemoryApi {
    fn prefixes(&self) -> &[&str] {
        &["memory."]
    }

    fn handle(&self, request: &crate::Request<'_>) -> std::result::Result<Value, String> {
        handle(self.service.as_ref(), request.kind, request.payload, request.signal)
    }
}
