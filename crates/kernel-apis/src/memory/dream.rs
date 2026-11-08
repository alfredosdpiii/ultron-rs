//! Dreaming over a curating backend (memorantado; `supports("claims")`): re-checking kept claims, and the pages
//! dreaming writes (the startup brief and topic pages) that a backend without a model cannot write itself.
//!
//! [`MemoryService::dream`]: the claims of each scope not confirmed for `days` are checked in plain code first: a
//! claim whose quoted files still hold their quotes is confirmed (with the files as evidence), with no model call. A
//! claim whose quote is gone goes to the judge (`judge.decide`: supported, stale or contradicted; anything it is not
//! sure of is left for review): supported is revised (the claim kept with the quotes that still hold), stale and
//! contradicted are retracted. A claim kept in the window and not looked at yet that shares its situation with an
//! older claim of the same scope is asked about too: contradicting claims are flagged as a conflict for a person or
//! a later dream, and once settled the newer claim is confirmed so it is not asked again. Nothing changed, nothing
//! asked: the judge is called only for those two cases.

use super::{ErrorCode, MemoryError, MemoryService, Quote, Result, quoted_path, staleness};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `judge.decide(state, questions, kind)` as the session asks it: the verdict's JSON.
pub type Decide = Arc<dyn Fn(&str, &Value, &str, &Signal) -> std::result::Result<Value, String> + Send + Sync>;

/// The startup brief's budget: it opens every context, so it stays short.
pub const BRIEF_MAX_LINES: usize = 200;
pub const BRIEF_MAX_CHARS: usize = 4000;
/// A topic page's budget.
pub const TOPIC_MAX_LINES: usize = 120;
pub const TOPIC_MAX_CHARS: usize = 6000;
/// Claims of a scope one dream reads, and checks at most.
const LIST_LIMIT: u64 = 200;
const DEFAULT_LIMIT: usize = 50;
const DEFAULT_DAYS: i64 = 7;
const DAY_MS: i64 = 86_400_000;
/// The most of a changed file the judge is shown.
const FILE_CHARS: usize = 6000;

fn status_question() -> Value {
    json!({"status": {
        "type": "choice",
        "instructions": "A coding agent kept the memory claim below because the quoted file text backed it. That \
            text is no longer in the file. Judge the claim against the file as it is now.",
        "criteria": {
            "supported": "The current file still backs the claim: the text moved or was reworded.",
            "stale": "The claim no longer applies: what it describes is gone or changed.",
            "contradicted": "The current file says the opposite of the claim.",
        },
    }})
}

fn contradiction_question() -> Value {
    json!({"contradict": {
        "type": "yesno",
        "instructions": "Do these two memory claims contradict each other, so that an agent cannot follow both?",
    }})
}

/// The answer to `name` when the verdict settled it (its first backend, or the escalation), else `None` (review,
/// abstained).
fn settled<'a>(verdict: &'a Value, name: &str) -> Option<&'a Value> {
    matches!(verdict["rung"].as_str(), Some("first" | "escalated")).then(|| &verdict["answers"][name]["value"])
}

fn quotes_of(metadata: &Value) -> Vec<Quote> {
    let parsed: Vec<Value> = metadata["ultron_quotes"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    parsed
        .iter()
        .filter_map(|q| {
            Some(Quote {
                path: quoted_path(q["path"].as_str()?)?.to_string(),
                quote: q["quote"].as_str()?.to_string(),
            })
        })
        .collect()
}

/// When a claim was last confirmed, else made (ISO strings compare in time order).
fn confirmed_at(claim: &Value) -> &str {
    claim["lastConfirmedAt"]
        .as_str()
        .or(claim["createdAt"].as_str())
        .unwrap_or("")
}

fn situation(claim: &Value) -> Option<&str> {
    claim["metadata"]["ultron_situation"]
        .as_str()
        .map(jsrt::string::trim)
        .filter(|s| !s.is_empty())
}

/// Whether `text` fits a page's budget: the reason when it does not.
pub fn page_budget(text: &str, topic: bool) -> std::result::Result<(), String> {
    let (lines, chars) = if topic {
        (TOPIC_MAX_LINES, TOPIC_MAX_CHARS)
    } else {
        (BRIEF_MAX_LINES, BRIEF_MAX_CHARS)
    };
    let what = if topic { "a topic page" } else { "the brief" };
    if jsrt::string::trim(text).is_empty() {
        return Err(format!("{what} is empty"));
    }
    if text.lines().count() > lines || text.chars().count() > chars {
        return Err(format!("{what} is over its budget ({lines} lines, {chars} characters)"));
    }
    Ok(())
}

impl MemoryService {
    /// Have dreaming's verdicts asked of the judge (the session's `judge.decide`).
    pub fn with_judge(mut self, decide: Decide) -> Self {
        self.judge = Some(decide);
        self
    }

    fn claims_backend(&self) -> Result<()> {
        if self.backend.supports("claims") {
            Ok(())
        } else {
            Err(MemoryError::code(ErrorCode::UnsupportedApi))
        }
    }

    fn backend_call<T>(&self, outcome: std::result::Result<T, String>) -> Result<T> {
        outcome.map_err(|error| MemoryError::of(&error))
    }

    /// The files under `root` that `quotes` cite, as they are now, for a confirmation's evidence.
    fn evidence_of(&self, root: &Path, quotes: &[Quote]) -> Value {
        Value::Array(self.quoted_files_in(root, quotes))
    }

    /// The project a dream or page names (`project`: its directory), else the session's: its tags, and the
    /// directory its quotes are checked in.
    fn project_scope(&self, payload: &Map<String, Value>) -> Result<(Vec<String>, Option<PathBuf>)> {
        match payload.get("project").and_then(Value::as_str) {
            Some(path) if Path::new(path).is_absolute() => {
                let tags = vec![format!("ultron:project:{}", super::directory_identity(path))];
                Ok((tags, Some(PathBuf::from(path))))
            }
            Some(_) => Err(MemoryError::code(ErrorCode::InvalidInput)),
            None => Ok((self.tags("project")?, self.root.clone())),
        }
    }

    /// The scopes a dream re-checks: the project's (with its directory) and the global one (whose quotes name
    /// files of whatever project they came from, so they are not checked).
    fn dream_scopes(&self, payload: &Map<String, Value>) -> Result<Vec<(Vec<String>, Option<PathBuf>)>> {
        let mut scopes = Vec::new();
        if let Ok((tags, root)) = self.project_scope(payload) {
            scopes.push((tags, root));
        } else if payload.contains_key("project") {
            return Err(MemoryError::code(ErrorCode::InvalidInput));
        }
        if let Ok(tags) = self.tags("global") {
            scopes.push((tags, None));
        }
        Ok(scopes)
    }

    /// The identity a page of `scope` is named by: the project's (`project`, else the session's) or the global one.
    fn page_identity(&self, scope: &str, payload: &Map<String, Value>) -> Result<String> {
        let tag = match scope {
            "project" => self.project_scope(payload)?.0.into_iter().next(),
            _ => self.tags(scope)?.into_iter().next(),
        };
        tag.and_then(|tag| super::tag_identity(&tag))
            .ok_or(MemoryError::code(ErrorCode::UnsupportedScope))
    }

    /// `memory.dream({days?, limit?})`: see the module docs. `{confirmed, revised, retracted, conflicts, review,
    /// unchecked, judged}`; ids are the backend's claim ids.
    pub fn dream(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        self.claims_backend()?;
        let days = payload.get("days").and_then(Value::as_i64).unwrap_or(DEFAULT_DAYS);
        let limit = payload
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_LIMIT, |n| n as usize);
        let cutoff = jsrt::date::iso_string((self.clock)() - days * DAY_MS);
        let (mut confirmed, mut revised, mut retracted, mut conflicts, mut review) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut unchecked = 0;
        let mut changed: Vec<(Value, Vec<Quote>, String, PathBuf)> = Vec::new();
        let mut pairs: Vec<(Value, Value)> = Vec::new();
        for (tags, root) in self.dream_scopes(payload)? {
            let request = json!({
                "tags": tags, "tagsMatch": "exact", "status": "active", "sort": "confirmed", "limit": LIST_LIMIT,
            });
            let listed = self.backend_call(self.backend.list(&request, signal))?;
            let claims: Vec<Value> = listed["claims"].as_array().cloned().unwrap_or_default();
            for claim in claims.iter().filter(|c| confirmed_at(c) < cutoff.as_str()).take(limit) {
                let quotes = quotes_of(&claim["metadata"]);
                // A global claim quotes files of whatever project it came from: nothing here to check it against.
                let (Some(root), false) = (root.as_deref(), quotes.is_empty()) else {
                    unchecked += 1;
                    continue;
                };
                let id = claim["id"].as_str().unwrap_or_default();
                match staleness(root, &claim["metadata"]) {
                    None => {
                        let evidence = json!({"evidence": self.evidence_of(root, &quotes), "tags": tags});
                        self.backend_call(self.backend.confirm(id, &evidence, signal))?;
                        confirmed.push(Value::from(id));
                    }
                    Some(why) => changed.push((claim.clone(), quotes, why, root.to_path_buf())),
                }
            }
            // A claim kept in the window and not yet looked at, beside an older one of the same situation.
            for newer in claims
                .iter()
                .filter(|c| c["createdAt"].as_str() >= Some(cutoff.as_str()) && c["lastConfirmedAt"].is_null())
            {
                for older in &claims {
                    if older["createdAt"].as_str() < newer["createdAt"].as_str()
                        && older["id"] != newer["id"]
                        && situation(older).is_some()
                        && situation(older) == situation(newer)
                    {
                        pairs.push((newer.clone(), older.clone()));
                    }
                }
            }
        }
        let judge = match &self.judge {
            Some(judge) => judge.clone(),
            None => {
                for (claim, _, why, _) in &changed {
                    review.push(json!({"id": claim["id"], "why": why}));
                }
                for (newer, older) in &pairs {
                    review.push(json!({"id": newer["id"], "why": format!("may contradict {}", older["id"])}));
                }
                return Ok(json!({
                    "confirmed": confirmed, "revised": revised, "retracted": retracted, "conflicts": conflicts,
                    "review": review, "unchecked": unchecked, "judged": 0,
                }));
            }
        };
        let mut judged = 0;
        for (claim, quotes, why, root) in changed {
            let id = claim["id"].as_str().unwrap_or_default().to_string();
            let mut state = format!("Memory claim: {}\n", claim["text"].as_str().unwrap_or_default());
            let mut held = Vec::new();
            for quote in &quotes {
                let current = super::read_quoted(&root.join(&quote.path));
                let holds = current
                    .as_deref()
                    .is_some_and(|text| super::squeezed(text).contains(&super::squeezed(&quote.quote)));
                state.push_str(&format!("\nQuoted from {}: \"{}\"\n", quote.path, quote.quote));
                match (&current, holds) {
                    (_, true) => {
                        state.push_str("The file still holds this text.\n");
                        held.push(quote.clone());
                    }
                    (Some(text), false) => {
                        let shown: String = text.chars().take(FILE_CHARS).collect();
                        state.push_str(&format!("The file now reads:\n{shown}\n"));
                    }
                    (None, false) => state.push_str("The file is gone.\n"),
                }
            }
            judged += 1;
            let verdict = judge(&state, &status_question(), "memory.evidence", signal)
                .map_err(|_| MemoryError::code(ErrorCode::OperationError))?;
            match settled(&verdict, "status").and_then(Value::as_str) {
                Some("supported") => {
                    let mut metadata = claim["metadata"].clone();
                    match held.is_empty() {
                        true => {
                            if let Some(map) = metadata.as_object_mut() {
                                map.remove("ultron_quotes");
                            }
                        }
                        false => {
                            let kept: Vec<Value> =
                                held.iter().map(|q| json!({"path": q.path, "quote": q.quote})).collect();
                            metadata["ultron_quotes"] = Value::Array(kept).to_string().into();
                            // The host checked what remains: only it marks a claim verified.
                            metadata["ultron_evidence_class"] = "verified".into();
                        }
                    }
                    let request = json!({
                        "text": claim["text"], "metadata": metadata, "evidence": self.evidence_of(&root, &held),
                        "tags": claim["tags"],
                    });
                    let new = self.backend_call(self.backend.supersede(&id, &request, signal))?;
                    revised.push(json!({"id": id, "by": new["id"]}));
                }
                Some(answer @ ("stale" | "contradicted")) => {
                    let reason = format!("dream: {answer} ({why})");
                    let request = json!({"state": "invalidated", "reason": reason});
                    self.backend_call(self.backend.curate(&id, &request, signal))?;
                    retracted.push(json!({"id": id, "reason": reason}));
                }
                _ => review.push(json!({"id": id, "why": why, "verdict": verdict["rung"]})),
            }
        }
        let mut looked_at: Vec<Value> = Vec::new();
        for (newer, older) in pairs {
            let state = format!(
                "Both claims apply when: {}\n\nClaim A (kept recently): {}\n\nClaim B (kept earlier): {}",
                situation(&newer).unwrap_or_default(),
                newer["text"].as_str().unwrap_or_default(),
                older["text"].as_str().unwrap_or_default(),
            );
            judged += 1;
            let verdict = judge(&state, &contradiction_question(), "memory.conflict", signal)
                .map_err(|_| MemoryError::code(ErrorCode::OperationError))?;
            match settled(&verdict, "contradict") {
                Some(Value::Bool(true)) => {
                    let request = json!({
                        "id": newer["id"], "conflictingId": older["id"],
                        "reason": format!("dream: the claims contradict when {}", situation(&newer).unwrap_or_default()),
                    });
                    self.backend_call(self.backend.conflict(&request, signal))?;
                    conflicts.push(json!({"id": newer["id"], "conflictingId": older["id"]}));
                }
                Some(_) => {}
                None => {
                    review.push(json!({"id": newer["id"], "why": format!("may contradict {}", older["id"])}));
                    continue;
                }
            }
            // Settled: the newer claim is marked looked at (confirmed), so the next dream does not ask again.
            if !looked_at.contains(&newer["id"]) {
                let id = newer["id"].as_str().unwrap_or_default();
                self.backend_call(self.backend.confirm(id, &json!({}), signal))?;
                looked_at.push(newer["id"].clone());
            }
        }
        Ok(json!({
            "confirmed": confirmed, "revised": revised, "retracted": retracted, "conflicts": conflicts,
            "review": review, "unchecked": unchecked, "judged": judged,
        }))
    }

    /// The id of the brief of `scope` (`project`: the project brief, `global`: the user's brief), or of its topic
    /// page `topic`, for the scope's `identity`.
    pub(super) fn page_id(scope: &str, identity: &str, topic: Option<&str>) -> String {
        match (scope, topic) {
            (_, Some(topic)) => format!("ultron-topic-{scope}-{identity}-{topic}"),
            ("global", None) => format!("ultron-user-{identity}"),
            _ => format!("ultron-project-{identity}"),
        }
    }

    fn pages_tag(scope: &str, identity: &str) -> String {
        format!("ultron:pages:{scope}:{identity}")
    }

    /// `memory.brief(text, scope, topic?)` (dreaming): writes the startup brief of `scope` (what opens each context
    /// as the project's or the user's brief), or its topic page `topic`, replacing the one before. Within
    /// [`page_budget`]; a text holding a secret is refused. `{id, scope, topic}`.
    pub fn write_page(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        self.claims_backend()?;
        let text = payload.get("text").and_then(Value::as_str).unwrap_or_default();
        let scope = super::scope_of(payload.get("scope").unwrap_or(&json!("project")))?;
        let topic = payload.get("topic").and_then(Value::as_str);
        if page_budget(text, topic.is_some()).is_err()
            || topic.is_some_and(|t| !crate::memory_records::is_kebab(t))
            || scope == "session"
        {
            return Err(MemoryError::code(ErrorCode::InvalidInput));
        }
        let check = json!({"action": "propose", "text": text, "scope": scope});
        let decision = super::gate_decision(
            &(self.gate)(&check, signal).map_err(|_| MemoryError::code(ErrorCode::OperationError))?,
            false,
        )?;
        if decision["action"] != "keep" {
            return Err(MemoryError::code(ErrorCode::InvalidInput));
        }
        let identity = self.page_identity(scope, payload)?;
        let id = Self::page_id(scope, &identity, topic);
        let mut metadata =
            json!({"ultron_source": "dream", "ultron_page": if topic.is_some() { "topic" } else { "brief" }});
        if let Some(topic) = topic {
            metadata["ultron_topic"] = topic.into();
        }
        let request = json!({
            "items": [{"content": text, "document_id": id, "tags": [Self::pages_tag(scope, &identity)],
                "update_mode": "replace", "metadata": metadata}],
            "async": false,
        });
        self.backend_call(self.backend.retain(&request, signal))?;
        Ok(json!({"id": id, "scope": scope, "topic": topic}))
    }

    /// `memory.topic(topic?, scope)`: the topic page `topic` of `scope` (`{topic, content, refreshedAt}`, null when
    /// there is none), or without a topic the names of the scope's topic pages.
    pub fn topic(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        self.claims_backend()?;
        let scope = super::scope_of(payload.get("scope").unwrap_or(&json!("project")))?;
        let identity = self.page_identity(scope, payload)?;
        match payload.get("topic").and_then(Value::as_str) {
            Some(topic) => {
                let id = Self::page_id(scope, &identity, Some(topic));
                let page = self.backend_call(self.backend.mental_model(&id, signal))?;
                Ok(match page["content"].as_str() {
                    Some(content) => {
                        json!({"topic": topic, "content": content, "refreshedAt": page["last_refreshed_at"]})
                    }
                    None => Value::Null,
                })
            }
            None => {
                let request = json!({"tags": [Self::pages_tag(scope, &identity)], "tagsMatch": "exact", "status": "active", "limit": LIST_LIMIT});
                let listed = self.backend_call(self.backend.list(&request, signal))?;
                let topics: Vec<Value> = listed["claims"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["metadata"]["ultron_topic"].as_str().map(Value::from))
                    .collect();
                Ok(Value::Array(topics))
            }
        }
    }
}
