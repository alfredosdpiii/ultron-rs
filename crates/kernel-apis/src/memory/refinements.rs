use super::{
    ErrorCode, MAX_QUOTE_BYTES, MAX_QUOTES, MemoryError, MemoryService, Quote, Result, gate_decision, quoted_path,
};
use kernel::Signal;
use serde_json::{Value, json};

/// Metadata type for project-scoped refinements stored in a curating backend (memorantado).
pub const REFINEMENT_TYPE: &str = "refinement";
/// Prefix on refinement ids backed by project claims in memorantado (`project:<claim-id>`).
pub const PROJECT_REFINEMENT_PREFIX: &str = "project:";

/// Best-effort extraction of file quotes from refinement evidence: keeps items whose `ref` names a project file and
/// whose `quote` fits [`MAX_QUOTE_BYTES`], ignoring non-file refs such as `compaction:<id>`.
fn file_quotes_from_evidence(value: &Value) -> Vec<Quote> {
    let mut quotes = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        let Some(map) = item.as_object() else {
            continue;
        };
        let Some(text) = map.get("quote").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        let Some(path) = map.get("ref").and_then(Value::as_str).and_then(quoted_path) else {
            continue;
        };
        if !text.is_empty() && text.len() <= MAX_QUOTE_BYTES {
            quotes.push(Quote {
                path: path.to_string(),
                quote: text.to_string(),
            });
            if quotes.len() == MAX_QUOTES {
                break;
            }
        }
    }
    quotes
}

fn refinement_text(record: &Value) -> Result<String> {
    match &record["text"] {
        Value::String(text) if !jsrt::string::trim(text).is_empty() => Ok(text.clone()),
        _ => match &record["content"] {
            Value::String(text) if !jsrt::string::trim(text).is_empty() => Ok(text.clone()),
            other if !other.is_null() => Ok(jsrt::json::stringify(other)),
            _ => Err(MemoryError::code(ErrorCode::InvalidInput)),
        },
    }
}

fn refinement_metadata(record: &Value, kind: &str, target: &str, situation: Option<&str>, quotes: &[Quote]) -> Value {
    let source = record["source"].as_str().unwrap_or("manual");
    let mut metadata = json!({
        "ultron_type": REFINEMENT_TYPE,
        "ultron_source": source,
        "ultron_refinement_kind": kind,
        "ultron_refinement_target": target,
        "ultron_refinement_scope": "project",
    });
    if let Some(situation) = situation {
        metadata["ultron_situation"] = situation.into();
    }
    if let Some(globs) = record["pathGlobs"].as_array().filter(|g| !g.is_empty()) {
        let clean: Vec<Value> = globs
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(Value::from)
            .collect();
        if !clean.is_empty() {
            metadata["ultron_path_globs"] = Value::Array(clean).to_string().into();
        }
    }
    if !quotes.is_empty() {
        let quoted: Vec<Value> = quotes
            .iter()
            .map(|q| json!({"path": q.path, "quote": q.quote}))
            .collect();
        metadata["ultron_quotes"] = Value::Array(quoted).to_string().into();
    }
    metadata
}

impl MemoryService {
    /// Whether this memory service can store and list project-scoped refinements (a curating backend such as
    /// memorantado, with project scope tags).
    pub fn supports_project_refinements(&self) -> bool {
        self.backend.supports("claims") && self.tags("project").is_ok()
    }

    fn gate_refinement(&self, text: &str, situation: Option<&str>, signal: &Signal) -> Result<()> {
        let mut gate_req = json!({"action": "propose", "text": text, "scope": "project"});
        if let Some(situation) = situation {
            gate_req["situation"] = situation.into();
        }
        let decision = gate_decision(
            &(self.gate)(&gate_req, signal).map_err(|_| MemoryError::code(ErrorCode::OperationError))?,
            false,
        )?;
        if decision["action"] != "keep" {
            return Err(MemoryError::code(ErrorCode::InvalidInput));
        }
        Ok(())
    }

    fn parse_refinement_claim(&self, claim: &Value) -> Option<(String, Value)> {
        let meta = &claim["metadata"];
        if meta["ultron_type"] != REFINEMENT_TYPE {
            return None;
        }
        let (Some(claim_id), Some(kind), Some(target), Some(text)) = (
            claim["id"].as_str().filter(|s| !s.is_empty()),
            meta["ultron_refinement_kind"].as_str().filter(|s| !s.is_empty()),
            meta["ultron_refinement_target"].as_str().filter(|s| !s.is_empty()),
            claim["text"].as_str().filter(|s| !jsrt::string::trim(s).is_empty()),
        ) else {
            return None;
        };
        let confirmed_at = claim["lastConfirmedAt"]
            .as_str()
            .or(claim["createdAt"].as_str())
            .unwrap_or("")
            .to_string();
        let mut record = json!({
            "id": format!("{PROJECT_REFINEMENT_PREFIX}{claim_id}"),
            "claimId": claim_id,
            "key": claim["key"],
            "kind": kind,
            "target": target,
            "content": text,
            "text": text,
            "scope": "project",
            "version": 1,
            "state": "active",
            "confirmedAt": confirmed_at,
            "createdAt": claim["createdAt"],
        });
        if let Some(situation) = meta["ultron_situation"].as_str().filter(|s| !s.is_empty()) {
            record["situation"] = situation.into();
        }
        if let Some(globs) = meta["ultron_path_globs"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
        {
            record["pathGlobs"] = globs;
        }
        if let Some(stale) = self.stale("project", meta) {
            record["stale"] = stale.into();
        }
        Some((confirmed_at, record))
    }

    fn attach_refinement_evidence(&self, item: &mut Value, quotes: &[Quote]) {
        if let Some(root) = &self.root
            && !quotes.is_empty()
        {
            let files = self.quoted_files_in(root, quotes);
            if !files.is_empty() {
                item["evidence"] = Value::Array(files);
            }
        }
    }

    /// Saves a validated refinement as a keyed project claim in memorantado (`document_id: refinement:<target>`).
    /// Writing to the same target supersedes the prior claim. Refused by the write gate if the text holds a secret.
    pub fn save_project_refinement(&self, record: &Value, signal: &Signal) -> Result<Value> {
        if !self.backend.supports("claims") {
            return Err(MemoryError::code(ErrorCode::UnsupportedApi));
        }
        let tags = self.tags("project")?;
        let target = record["target"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or(MemoryError::code(ErrorCode::InvalidInput))?;
        let kind = record["kind"]
            .as_str()
            .filter(|k| !k.is_empty())
            .ok_or(MemoryError::code(ErrorCode::InvalidInput))?;
        let text = refinement_text(record)?;
        let situation = record["situation"]
            .as_str()
            .map(jsrt::string::trim)
            .filter(|s| !s.is_empty());
        self.gate_refinement(&text, situation, signal)?;
        let quotes = file_quotes_from_evidence(&record["evidence"]);
        let key = format!("refinement:{target}");
        let metadata = refinement_metadata(record, kind, target, situation, &quotes);
        let mut item = json!({
            "content": text,
            "timestamp": jsrt::date::iso_string((self.clock)()),
            "document_id": key,
            "tags": tags,
            "update_mode": "replace",
            "metadata": metadata,
        });
        self.attach_refinement_evidence(&mut item, &quotes);
        let request = json!({"items": [item], "async": false});
        self.backend
            .retain(&request, signal)
            .map_err(|error| MemoryError::of(&error))?;
        Ok(json!({
            "key": key,
            "kind": kind,
            "target": target,
            "text": text,
            "scope": "project",
            "state": "active",
        }))
    }

    /// Lists active project-scoped refinements from memorantado, ordered most-recently-confirmed (or created) first.
    /// Returns an empty list when the backend does not support claims or has no project tags.
    pub fn list_project_refinements(&self, signal: &Signal) -> Result<Vec<Value>> {
        if !self.backend.supports("claims") {
            return Ok(Vec::new());
        }
        let Ok(tags) = self.tags("project") else {
            return Ok(Vec::new());
        };
        let request = json!({
            "tags": tags,
            "tagsMatch": "exact",
            "status": "active",
            "limit": 200,
        });
        let listed = self
            .backend
            .list(&request, signal)
            .map_err(|error| MemoryError::of(&error))?;
        let mut items: Vec<(String, Value)> = listed["claims"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|claim| self.parse_refinement_claim(claim))
            .collect();
        items.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(items.into_iter().map(|(_, record)| record).collect())
    }

    /// Retires (invalidates) a project-scoped refinement claim in memorantado.
    pub fn retire_project_refinement(&self, claim_id: &str, reason: &str, signal: &Signal) -> Result<bool> {
        if !self.backend.supports("claims") {
            return Err(MemoryError::code(ErrorCode::UnsupportedApi));
        }
        let id = claim_id.strip_prefix(PROJECT_REFINEMENT_PREFIX).unwrap_or(claim_id);
        let request = json!({"state": "invalidated", "reason": reason});
        self.backend
            .curate(id, &request, signal)
            .map(|_| true)
            .map_err(|error| MemoryError::of(&error))
    }

    /// Confirms project-scoped refinement claims (accepting either `project:<id>` or raw `<id>`).
    pub fn confirm_project_refinements(&self, ids: &[String], signal: &Signal) {
        let raw: Vec<String> = ids
            .iter()
            .map(|id| id.strip_prefix(PROJECT_REFINEMENT_PREFIX).unwrap_or(id).to_string())
            .collect();
        self.confirm_recalled(&raw, signal);
    }
}
