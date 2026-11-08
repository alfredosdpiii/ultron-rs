//! An in-process memorantado (the REST endpoints Ultron's client calls, as memorantado-revive answers them): keyed
//! claims with supersession, tags with `any`/`all`/`exact` filters, curation, conflicts, episodes and file snapshots.
//! It is the client's [`Transport`] (every request captured), or a loopback HTTP server ([`FakeMemorantado::serve`])
//! for a process that builds its own client. Shared by the memorantado, acceptance and cli tests (`#[path]`).

#![allow(dead_code)]

use kernel::Signal;
use memorantado::{HttpRequest, HttpResponse, Transport, TransportError};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Debug, PartialEq)]
pub struct Captured {
    pub method: String,
    /// The path under `/api`, without the query.
    pub path: String,
    pub query: Map<String, Value>,
    pub body: Option<Value>,
}

#[derive(Default)]
pub struct State {
    pub captured: Vec<Captured>,
    pub memories: Vec<Value>,
    pub episodes: Vec<Value>,
    /// `{source, contentHash, episodeId}`.
    pub snapshots: Vec<Value>,
    pub conflicts: Vec<Value>,
    /// Request prefixes ("POST /semantic-memories") answered 503 once each.
    pub fail_next: Vec<String>,
    /// No answer at all (the transport fails), as a server that is down.
    pub down: bool,
    /// The server's clock, ms since the epoch.
    pub now: i64,
    next_id: u64,
}

#[derive(Clone)]
pub struct FakeMemorantado {
    pub state: Arc<Mutex<State>>,
}

/// 2026-11-01T00:00:00Z.
pub const EPOCH: i64 = 1_793_491_200_000;

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// memorantado's tag filter: an untagged item never matches.
fn tags_match(have: &Value, want: &[String], mode: &str) -> bool {
    let have = strings(have);
    if want.is_empty() {
        return true;
    }
    if have.is_empty() {
        return false;
    }
    match mode {
        "all" => want.iter().all(|t| have.contains(t)),
        "exact" => have.len() == want.len() && want.iter().all(|t| have.contains(t)),
        _ => want.iter().any(|t| have.contains(t)),
    }
}

/// `application/x-www-form-urlencoded` decoding of one query component.
fn form_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match u8::from_str_radix(text.get(i + 1..i + 3).unwrap_or("zz"), 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect()
}

impl Default for FakeMemorantado {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeMemorantado {
    pub fn new() -> Self {
        FakeMemorantado {
            state: Arc::new(Mutex::new(State {
                now: EPOCH,
                ..State::default()
            })),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn captured(&self) -> Vec<Captured> {
        self.lock().captured.clone()
    }

    pub fn calls(&self, method: &str, path: &str) -> usize {
        self.captured()
            .iter()
            .filter(|c| c.method == method && c.path == path)
            .count()
    }

    /// The memory with `key` that is active.
    pub fn active(&self, key: &str) -> Option<Value> {
        self.lock()
            .memories
            .iter()
            .find(|m| m["key"] == key && m["status"] == "active")
            .cloned()
    }

    /// Adds a memory as stored long ago (for dreaming): `fields` over defaults; its id.
    pub fn seed(&self, fields: Value) -> u64 {
        let mut state = self.lock();
        state.next_id += 1;
        let id = state.next_id;
        let at = jsrt::date::iso_string(state.now);
        let mut memory = json!({
            "id": id, "project": "ultron", "scope": "project", "kind": "fact", "subject": "ultron",
            "predicate": "states", "object": null, "content": "", "confidence": 0.7, "importance": 0.5,
            "status": "active", "validFrom": null, "validTo": null, "lastConfirmedAt": null, "supersedesId": null,
            "metadata": {}, "tags": [], "key": null, "lifecycleStatus": "active", "createdAt": at,
            "updatedAt": at, "evidence": [],
        });
        for (name, value) in fields.as_object().into_iter().flatten() {
            memory[name] = value.clone();
        }
        state.memories.push(memory);
        id
    }

    fn insert(state: &mut State, body: &Value, supersedes: Option<u64>) -> Value {
        state.next_id += 1;
        let at = jsrt::date::iso_string(state.now);
        let memory = json!({
            "id": state.next_id, "project": body["project"], "scope": body["scope"].as_str().unwrap_or("project"),
            "kind": body["kind"].as_str().unwrap_or("fact"), "subject": body["subject"],
            "predicate": body["predicate"].as_str().unwrap_or("states"), "object": body["object"],
            "content": body["content"], "confidence": 0.7, "importance": 0.5, "status": "active",
            "validFrom": null, "validTo": null, "lastConfirmedAt": null, "supersedesId": supersedes,
            "metadata": if body["metadata"].is_object() { body["metadata"].clone() } else { Value::Null },
            "tags": if body["tags"].is_array() { body["tags"].clone() } else { json!([]) },
            "key": body["key"], "lifecycleStatus": "active", "createdAt": at, "updatedAt": at,
            "evidence": Self::evidence(body),
        });
        state.memories.push(memory.clone());
        memory
    }

    fn evidence(body: &Value) -> Value {
        match body["sourceEpisodeId"].as_u64() {
            Some(episode) => json!([{"episodeId": episode, "quote": body["quote"]}]),
            None => json!([]),
        }
    }

    fn find(state: &mut State, id: u64) -> Option<&mut Value> {
        state.memories.iter_mut().find(|m| m["id"] == id)
    }

    fn supersede(state: &mut State, id: u64, body: &Value) -> (u16, Value) {
        let Some(old) = Self::find(state, id).map(|m| m.clone()) else {
            return (404, json!({"error": "not_found"}));
        };
        if old["status"] != "active" {
            return (409, json!({"error": "conflict"}));
        }
        if let Some(memory) = Self::find(state, id) {
            memory["status"] = "superseded".into();
        }
        let mut claim = body.clone();
        for inherited in ["tags", "key", "kind", "scope", "subject", "predicate"] {
            if claim[inherited].is_null() {
                claim[inherited] = old[inherited].clone();
            }
        }
        (200, Self::insert(state, &claim, Some(id)))
    }

    /// One request under `/api`: `(status, body)`.
    pub fn handle(&self, method: &str, path: &str, query: &Map<String, Value>, body: Option<&Value>) -> (u16, Value) {
        let mut state = self.lock();
        state.captured.push(Captured {
            method: method.into(),
            path: path.into(),
            query: query.clone(),
            body: body.cloned(),
        });
        let line = format!("{method} {path}");
        if let Some(at) = state.fail_next.iter().position(|f| line.starts_with(f.as_str())) {
            state.fail_next.remove(at);
            return (503, json!({"error": "unavailable"}));
        }
        let empty = json!({});
        let body = body.unwrap_or(&empty);
        let q = |name: &str| query.get(name).and_then(Value::as_str).unwrap_or("").to_string();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let numeric = |at: usize| segments.get(at).and_then(|s| s.parse::<u64>().ok());
        match (method, segments.as_slice()) {
            ("GET", ["health"]) => (200, json!({"status": "ok"})),
            ("POST", ["retrieve-context"]) => {
                let tags = strings(&body["tags"]);
                let mode = body["tagsMatch"].as_str().unwrap_or("any");
                let query_words = words(body["query"].as_str().unwrap_or(""));
                let relevant = |text: &Value| {
                    let have = words(text.as_str().unwrap_or(""));
                    query_words.is_empty() || query_words.iter().any(|w| have.contains(w))
                };
                let memories: Vec<Value> = state
                    .memories
                    .iter()
                    .filter(|m| {
                        m["status"] == "active" && tags_match(&m["tags"], &tags, mode) && relevant(&m["content"])
                    })
                    .map(|m| json!({"type": "memory", "id": m["id"], "score": 0.5, "memory": m}))
                    .collect();
                let episodes: Vec<Value> = state
                    .episodes
                    .iter()
                    .filter(|e| tags_match(&e["tags"], &tags, mode) && relevant(&e["content"]))
                    .map(|e| json!({"type": "episode", "id": e["id"], "score": 0.4, "episode": e}))
                    .collect();
                (
                    200,
                    json!({"query": body["query"], "memories": memories, "episodes": episodes, "conflicts": []}),
                )
            }
            ("POST", ["semantic-memories"]) => {
                let keyed = body["key"]
                    .as_str()
                    .and_then(|key| {
                        state
                            .memories
                            .iter()
                            .find(|m| m["key"] == key && m["status"] == "active")
                    })
                    .cloned();
                match keyed {
                    Some(current) if current["content"] == body["content"] => {
                        let id = current["id"].as_u64().unwrap_or(0);
                        let now = jsrt::date::iso_string(state.now);
                        let memory = Self::find(&mut state, id).unwrap();
                        memory["updatedAt"] = now.into();
                        memory["metadata"] = body["metadata"].clone();
                        (200, memory.clone())
                    }
                    Some(current) => {
                        let id = current["id"].as_u64().unwrap_or(0);
                        Self::supersede(&mut state, id, body)
                    }
                    None => (200, Self::insert(&mut state, body, None)),
                }
            }
            ("GET", ["semantic-memories"]) => {
                let tags: Vec<String> = q("tags")
                    .split(',')
                    .filter(|t| !t.is_empty())
                    .map(String::from)
                    .collect();
                let mode = if q("tagsMatch").is_empty() {
                    "any".to_string()
                } else {
                    q("tagsMatch")
                };
                let (status, before) = (q("status"), q("confirmedBefore"));
                let confirmed = |m: &Value| {
                    m["lastConfirmedAt"]
                        .as_str()
                        .or(m["createdAt"].as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let mut found: Vec<Value> = state
                    .memories
                    .iter()
                    .filter(|m| status.is_empty() || m["status"] == status.as_str())
                    .filter(|m| tags_match(&m["tags"], &tags, &mode))
                    .filter(|m| before.is_empty() || confirmed(m) < before)
                    .cloned()
                    .collect();
                if q("sort") == "confirmed" {
                    found.sort_by_key(confirmed);
                }
                if let Ok(limit) = q("limit").parse::<usize>() {
                    found.truncate(limit);
                }
                (200, Value::Array(found))
            }
            ("GET", ["semantic-memories", "by-key"]) => {
                let key = q("key");
                match state
                    .memories
                    .iter()
                    .find(|m| m["key"] == key.as_str() && m["status"] == "active")
                {
                    Some(memory) => (200, memory.clone()),
                    None => (404, json!({"error": "not_found"})),
                }
            }
            ("GET", ["semantic-memories", _]) => match numeric(1).and_then(|id| Self::find(&mut state, id)) {
                Some(memory) => (200, memory.clone()),
                None => (404, json!({"error": "not_found"})),
            },
            ("DELETE", ["semantic-memories", _]) => {
                let Some(id) = numeric(1) else {
                    return (400, json!({"error": "invalid_input"}));
                };
                // The memory and the superseded versions it replaced.
                let mut gone = vec![id];
                let mut at = Some(id);
                while let Some(current) = at {
                    at = state
                        .memories
                        .iter()
                        .find(|m| m["id"] == current)
                        .and_then(|m| m["supersedesId"].as_u64());
                    gone.extend(at);
                }
                let before = state.memories.len();
                state.memories.retain(|m| !gone.iter().any(|id| m["id"] == *id));
                (200, json!({"deleted": state.memories.len() < before}))
            }
            ("POST", ["semantic-memories", _, "supersede"]) => {
                let id = numeric(1).unwrap_or(0);
                Self::supersede(&mut state, id, body)
            }
            ("POST", ["semantic-memories", _, "confirm"]) => {
                let now = jsrt::date::iso_string(state.now);
                match numeric(1).and_then(|id| Self::find(&mut state, id)) {
                    Some(memory) => {
                        memory["lastConfirmedAt"] = now.into();
                        if let Some(evidence) = Self::evidence(body).as_array().and_then(|e| e.first()) {
                            memory["evidence"].as_array_mut().unwrap().push(evidence.clone());
                        }
                        (200, memory.clone())
                    }
                    None => (404, json!({"error": "not_found"})),
                }
            }
            ("POST", ["semantic-memories", _, "invalidate"]) => {
                match numeric(1).and_then(|id| Self::find(&mut state, id)) {
                    Some(memory) => {
                        memory["status"] = "invalidated".into();
                        if !memory["metadata"].is_object() {
                            memory["metadata"] = json!({});
                        }
                        memory["metadata"]["invalidation"] = json!({"reason": body["reason"]});
                        (200, memory.clone())
                    }
                    None => (404, json!({"error": "not_found"})),
                }
            }
            ("POST", ["memory-conflicts"]) => {
                let exists = |id: &Value| state.memories.iter().any(|m| m["id"] == *id);
                if !exists(&body["memoryId"]) || !exists(&body["conflictingId"]) {
                    return (404, json!({"error": "not_found"}));
                }
                state.next_id += 1;
                let conflict = json!({
                    "id": state.next_id, "memoryId": body["memoryId"], "conflictingId": body["conflictingId"],
                    "reason": body["reason"], "resolutionStatus": "open",
                });
                state.conflicts.push(conflict.clone());
                (200, conflict)
            }
            ("POST", ["episodes"]) => {
                state.next_id += 1;
                let episode = json!({
                    "id": state.next_id, "session": body["session"], "role": body["role"].as_str().unwrap_or("user"),
                    "content": body["content"], "source": body["source"], "metadata": body["metadata"],
                    "tags": if body["tags"].is_array() { body["tags"].clone() } else { json!([]) },
                    "createdAt": jsrt::date::iso_string(state.now),
                });
                state.episodes.push(episode.clone());
                (200, json!({"episode": episode, "memories": []}))
            }
            ("POST", ["file-snapshots"]) => {
                let (source, content) = (body["source"].as_str().unwrap_or(""), body["content"].as_str());
                let Some(content) = content.filter(|_| source.starts_with("repo://")) else {
                    return (400, json!({"error": "invalid_input"}));
                };
                let hash = jsrt::hash::sha256_hex(content.as_bytes());
                if let Some(snapshot) = state
                    .snapshots
                    .iter()
                    .find(|s| s["source"] == source && s["contentHash"] == hash.as_str())
                {
                    return (200, json!({"snapshot": snapshot, "episode": null, "created": false}));
                }
                state.next_id += 1;
                let episode = json!({
                    "id": state.next_id, "role": "file", "source": source, "content": content,
                    "tags": body["tags"], "createdAt": jsrt::date::iso_string(state.now),
                });
                let snapshot = json!({"episodeId": state.next_id, "source": source, "contentHash": hash});
                state.snapshots.push(snapshot.clone());
                (200, json!({"snapshot": snapshot, "episode": episode, "created": true}))
            }
            _ => (404, json!({"error": "not_found"})),
        }
    }

    /// Serves this fake on a loopback port (one request per connection) until the process ends: its base URL.
    pub fn serve(&self) -> String {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let mut parts = line.split_whitespace();
                let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let (status, answer) = fake.dispatch(method, &format!("http://fake{target}"), &body);
                let text = answer.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// A request URL and body as the fake handles it.
    fn dispatch(&self, method: &str, url: &str, body: &[u8]) -> (u16, Value) {
        let target = url.splitn(4, '/').nth(3).map(|t| format!("/{t}")).unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let path = path.strip_prefix("/api").unwrap_or(path).to_string();
        let query: Map<String, Value> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (form_decode(k), Value::String(form_decode(v))))
            .collect();
        let body: Option<Value> = (!body.is_empty()).then(|| serde_json::from_slice(body).unwrap());
        self.handle(method, &path, &query, body.as_ref())
    }
}

impl Transport for FakeMemorantado {
    fn send(
        &self,
        request: HttpRequest,
        _: usize,
        _: std::time::Duration,
        _: &Signal,
    ) -> Result<HttpResponse, TransportError> {
        if self.lock().down {
            return Err(TransportError::Failed);
        }
        let (status, value) = self.dispatch(&request.method, &request.url, request.body.as_deref().unwrap_or(&[]));
        Ok(HttpResponse {
            status,
            retry_after: None,
            body: if (200..300).contains(&status) {
                value.to_string().into_bytes()
            } else {
                Vec::new()
            },
        })
    }
}
