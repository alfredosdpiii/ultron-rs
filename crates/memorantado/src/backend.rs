//! memorantado as a [`MemoryBackend`]: Ultron's Hindsight-shaped document calls mapped onto memorantado's REST API
//! (one project, scope tags kept exactly as Ultron sends them), plus claim curation.
//!
//! - recall: `POST /api/retrieve-context` with the tags (`any_strict` is `any`); memories come back as `world` facts
//!   whose `document_id` is the memory's key, episodes as `experience`. memorantado's scores are fused ranks, not
//!   comparable to Hindsight's, so `min_scores` is not applied.
//! - retain: a `replace` item is a keyed claim (`POST /api/semantic-memories`, the key replaces its current claim);
//!   an `append` item is an episode. Writes are synchronous: the answer is `{success, async: false}`. An item's
//!   `evidence` (`[{source: "repo://<path>", content, quote}]`) is snapshotted (`POST /api/file-snapshots`) and
//!   cited: the first quote by the write, the others by a confirmation each.
//! - A typed claim maps onto subject (its situation), predicate (its type) and kind; no object is sent, so claims
//!   never conflict by themselves (memorantado opens a conflict only for differing objects).
//! - get / delete / mental models are by key; `curate` invalidates; reflect is unsupported (memorantado has no
//!   model: dreaming writes the briefs a mental model would hold, as keyed claims read here).
//!
//! Errors are memory error codes only, as Hindsight's client: HTTP_ERROR for a non-2xx answer or no answer,
//! INVALID_RESPONSE for a body that is not JSON, TIMEOUT, ABORTED, RESPONSE_TOO_LARGE. An outage pauses the client:
//! after [`PAUSE_AFTER`] calls in a row got no answer, calls fail at once (HTTP_ERROR) for [`PAUSE`], so a server
//! that is down costs a session no timeouts.

use blocking_http::{HttpRequest, ReqwestTransport, Transport, TransportError};
use kernel::Signal;
use kernel_apis::MemoryBackend;
use kernel_apis::memory::{ErrorCode, MemoryError};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Calls in a row without an answer that pause the client.
pub const PAUSE_AFTER: u32 = 3;
/// How long a paused client fails at once.
pub const PAUSE: Duration = Duration::from_secs(60);
/// Who writes the briefs read as mental models.
pub const BRIEF_WRITER: &str = "dreaming";

#[derive(Clone)]
pub struct MemorantadoOptions {
    /// The server, such as `http://127.0.0.1:3789` (the API is under `/api`).
    pub base_url: String,
    /// The memorantado project every call names.
    pub project: String,
    /// Scope ("session", "project", "global") → identity tags.
    pub scope_tags: Map<String, Value>,
    /// Per request; default 5 s.
    pub timeout: Duration,
    /// Default 1 MiB.
    pub max_response_bytes: usize,
    /// The HTTP stack; `None`: [`ReqwestTransport`].
    pub transport: Option<Arc<dyn Transport>>,
}

impl MemorantadoOptions {
    pub fn new(base_url: impl Into<String>, project: impl Into<String>, scope_tags: Map<String, Value>) -> Self {
        Self {
            base_url: base_url.into(),
            project: project.into(),
            scope_tags,
            timeout: Duration::from_secs(5),
            max_response_bytes: 1 << 20,
            transport: None,
        }
    }
}

#[derive(Default)]
struct Outage {
    misses: u32,
    until: Option<Instant>,
}

pub struct MemorantadoBackend {
    api: String,
    project: String,
    filters: Map<String, Value>,
    timeout: Duration,
    max_bytes: usize,
    transport: Arc<dyn Transport>,
    outage: Mutex<Outage>,
}

fn code(code: ErrorCode) -> String {
    code.as_str().to_string()
}

/// A memorantado memory id from Ultron's string id (`UNKNOWN_MEMORY` for any other id, such as a Hindsight unit's).
fn memory_id(id: &str) -> Result<u64, String> {
    id.parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| code(ErrorCode::UnknownMemory))
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// A memorantado memory as a backend-neutral claim.
fn claim(memory: &Value) -> Value {
    json!({
        "id": memory["id"].as_u64().map(|id| id.to_string()),
        "key": memory["key"],
        "kind": memory["kind"],
        "text": memory["content"],
        "tags": memory["tags"],
        "metadata": if memory["metadata"].is_object() { memory["metadata"].clone() } else { json!({}) },
        "status": memory["status"],
        "lastConfirmedAt": memory["lastConfirmedAt"],
        "createdAt": memory["createdAt"],
    })
}

/// `any_strict` and `any` are `any`, `all_strict` and `all` are `all`, `exact` is exact.
fn tags_match(value: &Value) -> &'static str {
    match value.as_str() {
        Some("exact") => "exact",
        Some(text) if text.starts_with("all") => "all",
        _ => "any",
    }
}

impl MemorantadoBackend {
    /// INVALID_CONFIG for a URL that is not plain http(s) or a blank project; UNSUPPORTED_SCOPE for bad scope tags.
    pub fn new(options: MemorantadoOptions) -> Result<Self, MemoryError> {
        let base = url::Url::parse(&options.base_url).map_err(|_| MemoryError::code(ErrorCode::InvalidConfig))?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || jsrt::string::trim(&options.project).is_empty()
            || options.timeout.is_zero()
            || options.max_response_bytes == 0
        {
            return Err(MemoryError::code(ErrorCode::InvalidConfig));
        }
        let mut filters = Map::new();
        for (scope, tags) in &options.scope_tags {
            let valid = matches!(scope.as_str(), "session" | "project" | "global")
                && tags.as_array().is_some_and(|tags| {
                    !tags.is_empty()
                        && tags.iter().all(|t| {
                            t.as_str()
                                .is_some_and(|t| !t.is_empty() && !t.contains(char::is_whitespace))
                        })
                });
            if !valid {
                return Err(MemoryError::code(ErrorCode::UnsupportedScope));
            }
            filters.insert(scope.clone(), tags.clone());
        }
        let transport = match options.transport {
            Some(transport) => transport,
            None => Arc::new(ReqwestTransport::new().map_err(|_| MemoryError::code(ErrorCode::InvalidConfig))?),
        };
        let href = base.as_str();
        Ok(Self {
            api: format!("{}/api", href.strip_suffix('/').unwrap_or(href)),
            project: options.project,
            filters,
            timeout: options.timeout,
            max_bytes: options.max_response_bytes,
            transport,
            outage: Mutex::default(),
        })
    }

    /// The scope whose tags are exactly `tags`, if any.
    fn scope_of(&self, tags: &Value) -> Option<&str> {
        let mut want = string_list(tags);
        want.sort();
        self.filters.iter().find_map(|(scope, have)| {
            let mut have = string_list(have);
            have.sort();
            (have == want).then_some(scope.as_str())
        })
    }

    /// One call: `<api><path>?project=<project>&<query>`, a JSON body (with `project`) for POST; `Ok(None)` for a
    /// 404 when `missing_ok`.
    fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
        missing_ok: bool,
        signal: &Signal,
    ) -> Result<Option<Value>, String> {
        if signal.is_aborted() {
            return Err(code(ErrorCode::Aborted));
        }
        {
            let outage = jsrt::sync::lock(&self.outage);
            if outage.until.is_some_and(|until| Instant::now() < until) {
                return Err(code(ErrorCode::HttpError));
            }
        }
        let mut url = url::Url::parse(&format!("{}{path}", self.api)).map_err(|_| code(ErrorCode::InvalidConfig))?;
        url.query_pairs_mut().append_pair("project", &self.project);
        for (name, value) in query {
            url.query_pairs_mut().append_pair(name, value);
        }
        let body = body.map(|mut body| {
            body["project"] = self.project.clone().into();
            body.to_string().into_bytes()
        });
        let request = HttpRequest {
            method: method.into(),
            url: url.into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        };
        let sent = self.transport.send(request, self.max_bytes, self.timeout, signal);
        {
            let mut outage = jsrt::sync::lock(&self.outage);
            match &sent {
                Err(TransportError::Failed | TransportError::Timeout) => {
                    outage.misses += 1;
                    if outage.misses >= PAUSE_AFTER {
                        outage.misses = 0;
                        outage.until = Some(Instant::now() + PAUSE);
                    }
                }
                _ => *outage = Outage::default(),
            }
        }
        let response = sent.map_err(|error| {
            code(match error {
                TransportError::Timeout => ErrorCode::Timeout,
                TransportError::Aborted => ErrorCode::Aborted,
                TransportError::TooLarge => ErrorCode::ResponseTooLarge,
                TransportError::Failed => ErrorCode::HttpError,
            })
        })?;
        if signal.is_aborted() {
            return Err(code(ErrorCode::Aborted));
        }
        if missing_ok && response.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&response.status) {
            return Err(code(ErrorCode::HttpError));
        }
        serde_json::from_slice(&response.body)
            .map(Some)
            .map_err(|_| code(ErrorCode::InvalidResponse))
    }

    fn post(&self, path: &str, body: Value, signal: &Signal) -> Result<Value, String> {
        Ok(self
            .call("POST", path, &[], Some(body), false, signal)?
            .unwrap_or(Value::Null))
    }

    /// The current memory of `key`, `None` when it has none.
    fn by_key(&self, key: &str, signal: &Signal) -> Result<Option<Value>, String> {
        self.call(
            "GET",
            "/semantic-memories/by-key",
            &[("key", key.to_string())],
            None,
            true,
            signal,
        )
    }

    /// Snapshots each `{source, content, quote}` of `evidence`: `(episode id, quote)` per file.
    fn snapshots(&self, evidence: &Value, tags: &Value, signal: &Signal) -> Result<Vec<(u64, String)>, String> {
        let mut cited = Vec::new();
        for item in evidence.as_array().into_iter().flatten() {
            let (Some(source), Some(content)) = (item["source"].as_str(), item["content"].as_str()) else {
                return Err(code(ErrorCode::InvalidInput));
            };
            let mut body = json!({"source": source, "content": content});
            if tags.is_array() {
                body["tags"] = tags.clone();
            }
            let answer = self.post("/file-snapshots", body, signal)?;
            let episode = answer["snapshot"]["episodeId"]
                .as_u64()
                .ok_or_else(|| code(ErrorCode::InvalidResponse))?;
            cited.push((episode, item["quote"].as_str().unwrap_or_default().to_string()));
        }
        Ok(cited)
    }

    /// `body` citing `episode` with `quote`.
    fn cite(mut body: Value, cited: Option<&(u64, String)>) -> Value {
        if let Some((episode, quote)) = cited {
            body["sourceEpisodeId"] = json!(episode);
            if !quote.is_empty() {
                body["quote"] = quote.clone().into();
            }
        }
        body
    }

    /// A `replace` retain item as a keyed claim.
    fn keep_claim(&self, item: &Value, signal: &Signal) -> Result<(), String> {
        let metadata = if item["metadata"].is_object() {
            item["metadata"].clone()
        } else {
            json!({})
        };
        let typed = metadata["ultron_type"].as_str();
        let kind = metadata["ultron_page"]
            .as_str()
            .or(typed)
            .or(metadata.get("ultron_through").map(|_| "session_tail"))
            .unwrap_or("fact");
        let mut body = json!({
            "kind": kind,
            "subject": metadata["ultron_situation"].as_str().unwrap_or("ultron"),
            "predicate": typed.unwrap_or("states"),
            "content": item["content"],
            "key": item["document_id"],
            "tags": item["tags"],
            "metadata": metadata,
        });
        if let Some(scope) = self.scope_of(&item["tags"]) {
            body["scope"] = scope.into();
        }
        let cited = self.snapshots(&item["evidence"], &item["tags"], signal)?;
        let kept = self.post("/semantic-memories", Self::cite(body, cited.first()), signal)?;
        let id = kept["id"].as_u64().ok_or_else(|| code(ErrorCode::InvalidResponse))?;
        for extra in cited.iter().skip(1) {
            self.post(
                &format!("/semantic-memories/{id}/confirm"),
                Self::cite(json!({}), Some(extra)),
                signal,
            )?;
        }
        Ok(())
    }
}

impl MemoryBackend for MemorantadoBackend {
    fn namespace(&self) -> String {
        format!("{}#{}", self.api, self.project)
    }

    fn scope_tags(&self) -> Map<String, Value> {
        self.filters.clone()
    }

    fn supports(&self, call: &str) -> bool {
        matches!(call, "recall" | "retain" | "get" | "delete" | "fileEvidence" | "claims")
    }

    fn recall(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        let mut body = json!({
            "query": request["query"],
            "tags": request["tags"],
            "tagsMatch": tags_match(&request["tags_match"]),
        });
        if let Some(budget) = request["max_tokens"].as_u64() {
            body["tokenBudget"] = budget.into();
        }
        let answer = self.post("/retrieve-context", body, signal)?;
        let wanted = |kind: &str| {
            request["types"]
                .as_array()
                .is_none_or(|types| types.iter().any(|t| t == kind))
        };
        let mut results = Vec::new();
        for hit in answer["memories"].as_array().into_iter().flatten() {
            let memory = &hit["memory"];
            if memory.is_object() && wanted("world") {
                results.push(json!({
                    "id": memory["id"].as_u64().map(|id| id.to_string()),
                    "text": memory["content"], "tags": memory["tags"], "document_id": memory["key"],
                    "metadata": memory["metadata"], "type": "world",
                }));
            }
        }
        for hit in answer["episodes"].as_array().into_iter().flatten() {
            let episode = &hit["episode"];
            if episode.is_object() && wanted("experience") {
                results.push(json!({
                    "id": episode["id"].as_u64().map(|id| format!("episode-{id}")),
                    "text": episode["content"], "tags": episode["tags"], "document_id": null,
                    "metadata": episode["metadata"], "type": "experience",
                }));
            }
        }
        if !answer.is_object() {
            return Err(code(ErrorCode::InvalidResponse));
        }
        Ok(json!({"results": results}))
    }

    fn retain(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        let items = request["items"]
            .as_array()
            .filter(|items| !items.is_empty())
            .ok_or_else(|| code(ErrorCode::InvalidInput))?;
        for item in items {
            if item["update_mode"] == "append" {
                self.post(
                    "/episodes",
                    json!({
                        "session": item["document_id"], "content": item["content"], "tags": item["tags"],
                        "metadata": item["metadata"], "source": "ultron:auto", "extract": false,
                    }),
                    signal,
                )?;
            } else {
                self.keep_claim(item, signal)?;
            }
        }
        Ok(json!({"success": true, "async": false}))
    }

    fn get(&self, document_id: &str, signal: &Signal) -> Result<Value, String> {
        let memory = self
            .by_key(document_id, signal)?
            .ok_or_else(|| code(ErrorCode::HttpError))?;
        Ok(json!({
            "id": document_id, "original_text": memory["content"], "tags": memory["tags"],
            "document_metadata": memory["metadata"],
        }))
    }

    fn delete(&self, document_id: &str, signal: &Signal) -> Result<Value, String> {
        let deleted = match self.by_key(document_id, signal)? {
            Some(memory) => {
                let id = memory["id"].as_u64().ok_or_else(|| code(ErrorCode::InvalidResponse))?;
                let answer = self.call("DELETE", &format!("/semantic-memories/{id}"), &[], None, false, signal)?;
                usize::from(answer.is_some_and(|a| a["deleted"] == true))
            }
            None => 0,
        };
        Ok(json!({"success": true, "document_id": document_id, "memory_units_deleted": deleted}))
    }

    fn operation(&self, _operation_id: &str, _signal: &Signal) -> Result<Value, String> {
        Err(code(ErrorCode::UnsupportedApi))
    }

    fn unit(&self, unit_id: &str, signal: &Signal) -> Result<Value, String> {
        let id = memory_id(unit_id)?;
        let memory = self
            .call("GET", &format!("/semantic-memories/{id}"), &[], None, false, signal)?
            .unwrap_or(Value::Null);
        Ok(json!({
            "id": unit_id, "text": memory["content"], "tags": memory["tags"], "type": "world",
            "document_id": memory["key"], "state": memory["status"],
        }))
    }

    fn curate(&self, unit_id: &str, request: &Value, signal: &Signal) -> Result<Value, String> {
        if request["state"] != "invalidated" {
            return Err(code(ErrorCode::UnsupportedApi));
        }
        let id = memory_id(unit_id)?;
        let mut body = json!({});
        if let Some(reason) = request["reason"].as_str() {
            body["reason"] = reason.into();
        }
        self.post(&format!("/semantic-memories/{id}/invalidate"), body, signal)
            .map(|memory| claim(&memory))
    }

    fn mental_model(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        Ok(match self.by_key(id, signal)? {
            Some(memory) => json!({
                "id": id, "content": memory["content"], "last_refreshed_at": memory["updatedAt"],
                "writer": BRIEF_WRITER,
            }),
            None => Value::Null,
        })
    }

    /// Nothing to create: dreaming writes the brief (as the keyed claim [`Self::mental_model`] reads).
    fn create_mental_model(&self, _request: &Value, _signal: &Signal) -> Result<Value, String> {
        Ok(json!({}))
    }

    fn list(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        let tags = string_list(&request["tags"]);
        if tags.is_empty() {
            return Err(code(ErrorCode::UnsupportedScope));
        }
        let mut query = vec![
            ("tags", tags.join(",")),
            ("tagsMatch", tags_match(&request["tagsMatch"]).to_string()),
        ];
        for name in ["status", "confirmedBefore", "sort"] {
            if let Some(value) = request[name].as_str() {
                query.push((name, value.to_string()));
            }
        }
        if let Some(limit) = request["limit"].as_u64() {
            query.push(("limit", limit.to_string()));
        }
        let answer = self
            .call("GET", "/semantic-memories", &query, None, false, signal)?
            .unwrap_or(Value::Null);
        let memories = answer.as_array().ok_or_else(|| code(ErrorCode::InvalidResponse))?;
        Ok(json!({"claims": memories.iter().map(claim).collect::<Vec<_>>()}))
    }

    fn confirm(&self, id: &str, request: &Value, signal: &Signal) -> Result<Value, String> {
        let id = memory_id(id)?;
        let cited = self.snapshots(&request["evidence"], &request["tags"], signal)?;
        self.post(
            &format!("/semantic-memories/{id}/confirm"),
            Self::cite(json!({}), cited.first()),
            signal,
        )
        .map(|memory| claim(&memory))
    }

    fn supersede(&self, id: &str, request: &Value, signal: &Signal) -> Result<Value, String> {
        let id = memory_id(id)?;
        let cited = self.snapshots(&request["evidence"], &request["tags"], signal)?;
        let body = json!({"content": request["text"], "metadata": request["metadata"]});
        self.post(
            &format!("/semantic-memories/{id}/supersede"),
            Self::cite(body, cited.first()),
            signal,
        )
        .map(|memory| claim(&memory))
    }

    fn conflict(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        let (Some(first), Some(second)) = (request["id"].as_str(), request["conflictingId"].as_str()) else {
            return Err(code(ErrorCode::InvalidInput));
        };
        self.post(
            "/memory-conflicts",
            json!({"memoryId": memory_id(first)?, "conflictingId": memory_id(second)?, "reason": request["reason"]}),
            signal,
        )
    }
}
