use super::{
    ErrorCode, MemoryError, MemoryService, Quote, RECEIPT_STATUSES, Result, check_abort, evidence_class_of,
    evidence_refs, gate_decision, hash, require_text, same_tags, scope_of, split_evidence,
};
use crate::memory_records::{IndexEntry, Typed, content_cap, typed_fields};
use kernel::Signal;
use serde_json::{Map, Value, json};

impl MemoryService {
    /// The content of the mental model `id`, created from `query` over the memories tagged `tags` on first use.
    pub(super) fn brief(
        &self,
        id: &str,
        name: &str,
        query: &str,
        tags: &[String],
        signal: &Signal,
    ) -> Result<Option<Value>> {
        let model = self
            .backend
            .mental_model(id, signal)
            .map_err(|error| MemoryError::of(&error))?;
        if model.is_null() {
            let request = json!({
                "id": id, "name": name, "source_query": query,
                "tags": tags, "max_tokens": 1024,
                "trigger": {
                    "refresh_after_consolidation": true, "min_refresh_interval_seconds": 3600, "mode": "delta",
                    "budget": "low", "tags_match": "any_strict",
                    "exclude_mental_models": true,
                },
            });
            self.backend
                .create_mental_model(&request, signal)
                .map_err(|error| MemoryError::of(&error))?;
            return Ok(None);
        }
        let content = model.get("content").and_then(Value::as_str).map(jsrt::string::trim);
        Ok(content.filter(|content| !content.is_empty()).map(|content| {
            let mut brief = json!({"id": id, "content": content, "refreshedAt": model.get("last_refreshed_at").cloned().unwrap_or(Value::Null)});
            if let Some(writer) = model.get("writer").and_then(Value::as_str) {
                brief["writer"] = writer.into();
            }
            brief
        }))
    }
}

struct ProposeInputs<'a> {
    text: &'a str,
    scope: &'static str,
    refs: Vec<Value>,
    quotes: Vec<Quote>,
    class: String,
    typed: Option<Typed>,
    tags: Vec<String>,
}

fn retain_start_fields(
    memory_id: Option<&str>,
    scope: &str,
    text: &str,
    evidence: &[Value],
    class: &str,
    previous: Option<&Value>,
) -> Vec<(&'static str, Value)> {
    let mut fields = Vec::new();
    if let Some(memory_id) = memory_id {
        fields.push(("memoryId", Value::from(memory_id)));
    }
    fields.extend([
        ("scope", Value::from(scope)),
        ("textHash", hash(text).into()),
        ("evidence", Value::Array(evidence.to_vec())),
        ("evidenceClass", class.into()),
    ]);
    if let Some(previous) = previous {
        fields.push(("supersedes", previous["id"].clone()));
    }
    fields
}

fn build_retain_metadata(id: &str, class: &str, typed: Option<&Typed>, quotes: &[Quote], evidence: &[Value]) -> Value {
    let mut metadata = json!({"ultron_operation": id, "ultron_evidence_class": class});
    if let Some(typed) = typed {
        metadata["ultron_type"] = typed.kind.clone().into();
        metadata["ultron_situation"] = typed.situation.clone().into();
        if let Some(key) = &typed.key {
            metadata["ultron_key"] = key.clone().into();
        }
    }
    if !quotes.is_empty() {
        let quotes: Vec<Value> = quotes
            .iter()
            .map(|quote| json!({"path": quote.path, "quote": quote.quote}))
            .collect();
        metadata["ultron_quotes"] = Value::Array(quotes).to_string().into();
    }
    let spans: Vec<&Value> = evidence
        .iter()
        .map(|item| &item["ref"])
        .filter(|r| r.as_str().is_some_and(|r| r.starts_with("session://")))
        .collect();
    if !spans.is_empty() {
        metadata["ultron_spans"] = json!(spans).to_string().into();
    }
    metadata
}

fn parse_operation_ids(response: &Value) -> Result<Vec<Value>> {
    let response_id = response["operation_id"].as_str();
    let ids: Vec<Value> = match response.get("operation_ids") {
        None | Some(Value::Null) => response_id.map_or_default(|id| vec![Value::from(id)]),
        Some(Value::Array(ids)) => ids.clone(),
        Some(_) => Vec::new(),
    };
    if ids.is_empty()
        || !ids.iter().all(super::nonempty)
        || response_id.is_some_and(|response_id| !ids.iter().any(|id| id == response_id))
    {
        return Err(MemoryError::code(ErrorCode::InvalidResponse));
    }
    let mut unique: Vec<Value> = Vec::new();
    for id in ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    Ok(unique)
}

fn resolve_receipts_state(receipts: &[(String, String)]) -> &'static str {
    let has = |status: &str| receipts.iter().any(|(_, s)| s == status);
    if has("not_found") {
        "unknown"
    } else if has("pending") || has("processing") {
        "accepted"
    } else if has("failed") {
        "failed"
    } else if has("cancelled") {
        "cancelled"
    } else {
        "stored"
    }
}

impl MemoryService {
    fn parse_propose_inputs<'a>(&self, payload: &'a Map<String, Value>) -> Result<ProposeInputs<'a>> {
        let text = payload.get("text").unwrap_or(&Value::Null);
        require_text(text)?;
        let text = text.as_str().unwrap_or("");
        let scope = scope_of(payload.get("scope").unwrap_or(&json!("session")))?;
        let (evidence, quotes) = split_evidence(payload.get("evidence").unwrap_or(&Value::Null))?;
        let refs = evidence_refs(&evidence)?;
        let class = evidence_class_of(payload.get("evidenceClass"), "hypothesis")?;
        let typed = typed_fields(payload, text)?;
        let tags = self.tags(scope)?;
        Ok(ProposeInputs {
            text,
            scope,
            refs,
            quotes,
            class,
            typed,
            tags,
        })
    }

    pub fn propose(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        let inputs = self.parse_propose_inputs(payload)?;
        self.load()?;
        let existing = match inputs.typed.as_ref().and_then(|typed| typed.key.as_deref()) {
            Some(key) => self
                .records
                .by_key(key, inputs.scope, &self.filters)
                .map_err(|_| MemoryError::code(ErrorCode::StoreError))?,
            None => None,
        };
        let written = match existing {
            Some(entry) => {
                let kind = if self.journal_owns(&entry.id) {
                    "correct"
                } else {
                    "propose"
                };
                let memory_id = entry.id;
                self.locked(&memory_id, || {
                    self.retain(
                        kind,
                        Some(&memory_id),
                        inputs.text,
                        inputs.refs,
                        &inputs.quotes,
                        &inputs.class,
                        inputs.scope,
                        inputs.tags,
                        inputs.typed.as_ref(),
                        signal,
                    )
                })?
            }
            None => self.retain(
                "propose",
                None,
                inputs.text,
                inputs.refs,
                &inputs.quotes,
                &inputs.class,
                inputs.scope,
                inputs.tags,
                inputs.typed.as_ref(),
                signal,
            )?,
        };
        Ok(self.index_written(written, inputs.typed.as_ref(), inputs.text))
    }

    pub fn correct(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        let id = payload.get("id").cloned().unwrap_or(Value::Null);
        let text = payload.get("text").cloned().unwrap_or(Value::Null);
        require_text(&text)?;
        let (evidence, quotes) = split_evidence(payload.get("evidence").unwrap_or(&Value::Null))?;
        let refs = evidence_refs(&evidence)?;
        let class = evidence_class_of(payload.get("evidenceClass"), "user_statement")?;
        self.load()?;
        let (scope, tags) = self.owned(&id)?;
        let memory_id = id.as_str().unwrap_or("").to_string();
        let text = text.as_str().unwrap_or("");
        let typed = self.indexed(&memory_id).map(|entry| entry.typed);
        if typed
            .as_ref()
            .is_some_and(|typed| text.len() > content_cap(&typed.kind))
        {
            return Err(MemoryError::code(ErrorCode::InvalidInput));
        }
        let written = self.locked(&memory_id, || {
            self.retain(
                "correct",
                Some(&memory_id),
                text,
                refs,
                &quotes,
                &class,
                &scope,
                tags,
                typed.as_ref(),
                signal,
            )
        })?;
        Ok(self.index_written(written, typed.as_ref(), text))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn screen_retain(
        &self,
        id: &str,
        text: &str,
        evidence: &[Value],
        scope: &str,
        typed: Option<&Typed>,
        quotes: &[Quote],
        signal: &Signal,
    ) -> Result<Option<Value>> {
        let mut request = json!({"action": "retain", "text": text, "evidence": evidence, "scope": scope});
        let mut screened: Vec<String> = typed.map(Typed::screened).into_iter().collect();
        screened.extend(quotes.iter().map(|quote| quote.quote.clone()));
        if !screened.is_empty() {
            request["situation"] = screened.join("\n").into();
        }
        let decision = gate_decision(
            &(self.gate)(&request, signal).map_err(|_| MemoryError::code(ErrorCode::OperationError))?,
            false,
        )?;
        check_abort(signal)?;
        self.update(id, vec![("gate", decision.clone())])?;
        match decision["action"].as_str() {
            Some("keep") => Ok(None),
            Some("skip") => self.update(id, vec![("state", "skipped".into())]).map(Some),
            _ => self.update(id, vec![("state", "sensitive".into())]).map(Some),
        }
    }

    fn finish_retain_response(&self, id: &str, response: &Value) -> Result<Value> {
        if !response.is_object() || response["success"] != true || !response["async"].is_boolean() {
            return Err(MemoryError::code(ErrorCode::InvalidResponse));
        }
        if response["async"] == false {
            return self.update(id, vec![("state", "stored".into()), ("operationIds", json!([]))]);
        }
        let unique = parse_operation_ids(response)?;
        self.update(
            id,
            vec![("state", "accepted".into()), ("operationIds", Value::Array(unique))],
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn send_retain_backend(
        &self,
        id: &str,
        memory_id: &str,
        text: &str,
        evidence: &[Value],
        quotes: &[Quote],
        class: &str,
        tags: &[String],
        typed: Option<&Typed>,
        signal: &Signal,
    ) -> Result<Value> {
        if !self.backend.supports("retain") {
            return Err(MemoryError::code(ErrorCode::UnsupportedApi));
        }
        self.update(
            id,
            vec![
                ("memoryId", memory_id.into()),
                ("tags", json!(tags)),
                ("operationIds", json!([id])),
                ("phase", "backend".into()),
            ],
        )?;
        jsrt::sync::lock(&self.inner).reuse.clear();
        check_abort(signal)?;
        let metadata = build_retain_metadata(id, class, typed, quotes, evidence);
        let mut request = json!({
            "async": true,
            "operation_id": id,
            "items": [{
                "content": text,
                "document_id": memory_id,
                "tags": tags,
                "observation_scopes": [tags],
                "update_mode": "replace",
                "metadata": metadata,
            }],
        });
        let files = self.quoted_files(quotes);
        if !files.is_empty() {
            request["items"][0]["evidence"] = Value::Array(files);
        }
        let response = self
            .backend
            .retain(&request, signal)
            .map_err(|error| MemoryError::of(&error))?;
        check_abort(signal)?;
        self.finish_retain_response(id, &response)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn retain(
        &self,
        kind: &str,
        memory_id: Option<&str>,
        text: &str,
        evidence: Vec<Value>,
        quotes: &[Quote],
        class: &str,
        scope: &str,
        tags: Vec<String>,
        typed: Option<&Typed>,
        signal: &Signal,
    ) -> Result<Value> {
        let previous = memory_id
            .filter(|_| kind == "correct")
            .and_then(|id| Self::current_claim(&jsrt::sync::lock(&self.inner).records, id));
        let fields = retain_start_fields(memory_id, scope, text, &evidence, class, previous.as_ref());
        let id = self.start(kind, fields)?;
        self.attempt(&id, signal, || {
            if let Some(memory_id) = memory_id.filter(|_| kind == "correct") {
                self.editable(memory_id)?;
            }
            if let Some(early) = self.screen_retain(&id, text, &evidence, scope, typed, quotes, signal)? {
                return Ok(early);
            }
            let memory_id = memory_id.map_or_else(jsrt::random::uuid_v4, str::to_string);
            self.send_retain_backend(&id, &memory_id, text, &evidence, quotes, class, &tags, typed, signal)
        })
    }

    pub fn forget(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        let id = payload.get("id").cloned().unwrap_or(Value::Null);
        self.load()?;
        let (scope, tags) = self.owned(&id)?;
        let memory_id = id.as_str().unwrap_or("").to_string();
        let outcome = self.forget_owned(&memory_id, scope, tags, signal);
        if Self::withdrawn(&jsrt::sync::lock(&self.inner).records, &memory_id) && self.indexed(&memory_id).is_some() {
            let _ = self.records.remove(&memory_id);
        }
        outcome
    }

    fn delete_in_backend(&self, op: &str, memory_id: &str, signal: &Signal) -> Result<Value> {
        self.editable(memory_id)?;
        if !self.backend.supports("delete") {
            return Err(MemoryError::code(ErrorCode::UnsupportedApi));
        }
        self.update(op, vec![("phase", "backend".into())])?;
        jsrt::sync::lock(&self.inner).reuse.clear();
        check_abort(signal)?;
        let response = self
            .backend
            .delete(memory_id, signal)
            .map_err(|error| MemoryError::of(&error))?;
        check_abort(signal)?;
        if !response.is_object() || response["success"] != true || response["document_id"] != memory_id {
            return Err(MemoryError::code(ErrorCode::InvalidResponse));
        }
        self.update(op, vec![("state", "forgotten".into())])
    }

    fn forget_owned(&self, memory_id: &str, scope: String, tags: Vec<String>, signal: &Signal) -> Result<Value> {
        let memory_id = memory_id.to_string();
        self.locked(&memory_id, || {
            let op = self.start(
                "forget",
                vec![
                    ("memoryId", memory_id.clone().into()),
                    ("scope", scope.clone().into()),
                    ("tags", json!(tags)),
                ],
            )?;
            self.attempt(&op, signal, || self.delete_in_backend(&op, &memory_id, signal))
        })
    }

    pub fn get(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value> {
        let id = payload.get("id").cloned().unwrap_or(Value::Null);
        self.load()?;
        let memory_id = id.as_str().unwrap_or("").to_string();
        let entry = self.indexed(&memory_id);
        let (scope, tags) = match (self.owned(&id), &entry) {
            (Ok(owned), _) => owned,
            (Err(error), Some(entry)) if error.code == ErrorCode::UnknownMemory => {
                return self.get_indexed(entry, signal);
            }
            (Err(error), _) => return Err(error),
        };
        let mut read = self.get_owned(&memory_id, scope, tags, signal)?;
        if let Some(entry) = entry {
            read["record"] = entry.typed.to_json();
        }
        Ok(read)
    }

    /// `get` of an indexed record whose propose is not in this session's journal.
    fn get_indexed(&self, entry: &IndexEntry, signal: &Signal) -> Result<Value> {
        check_abort(signal)?;
        if !self.backend.supports("get") {
            return Err(MemoryError::code(ErrorCode::UnsupportedApi));
        }
        let document = self
            .backend
            .get(&entry.id, signal)
            .map_err(|error| MemoryError::of(&error))?;
        check_abort(signal)?;
        if !document.is_object()
            || document["id"] != entry.id.as_str()
            || !same_tags(&document["tags"], &entry.tags)
            || !(document["original_text"].is_null() || document["original_text"].is_string())
        {
            return Err(MemoryError::code(ErrorCode::InvalidResponse));
        }
        Ok(json!({
            "id": entry.id, "scope": entry.scope, "state": "stored", "operation": null,
            "content": document["original_text"], "record": entry.typed.to_json(),
        }))
    }

    fn poll_pending_operations(&self, op: &str, current: &Value, signal: &Signal) -> Result<Value> {
        let ids: Vec<String> = current["operationIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(str::to_string))
            .collect();
        self.update(op, vec![("phase", "backend".into())])?;
        let mut receipts = Vec::new();
        for operation_id in &ids {
            check_abort(signal)?;
            let receipt = self
                .backend
                .operation(operation_id, signal)
                .map_err(|error| MemoryError::of(&error))?;
            check_abort(signal)?;
            let status = receipt["status"]
                .as_str()
                .filter(|status| RECEIPT_STATUSES.contains(status));
            let Some(status) = status.filter(|_| receipt["operation_id"] == operation_id.as_str()) else {
                return Err(MemoryError::code(ErrorCode::InvalidResponse));
            };
            receipts.push((operation_id.clone(), status.to_string()));
        }
        let state = resolve_receipts_state(&receipts);
        let receipts: Vec<Value> = receipts
            .iter()
            .map(|(id, status)| json!({"id": id, "status": status}))
            .collect();
        let current_id = current["id"].as_str().unwrap_or("").to_string();
        self.update(
            &current_id,
            vec![("state", state.into()), ("receipts", Value::Array(receipts))],
        )
    }

    fn valid_document(document: &Value, memory_id: &str, tags: &[String]) -> bool {
        document.is_object()
            && document["id"] == memory_id
            && same_tags(&document["tags"], tags)
            && (document["original_text"].is_null() || document["original_text"].is_string())
    }

    fn fetch_stored_content(&self, op: &str, memory_id: &str, tags: &[String], signal: &Signal) -> Result<Value> {
        if !self.backend.supports("get") {
            return Err(MemoryError::code(ErrorCode::UnsupportedApi));
        }
        self.update(op, vec![("phase", "backend".into())])?;
        check_abort(signal)?;
        let document = self
            .backend
            .get(memory_id, signal)
            .map_err(|error| MemoryError::of(&error))?;
        check_abort(signal)?;
        if !Self::valid_document(&document, memory_id, tags) {
            return Err(MemoryError::code(ErrorCode::InvalidResponse));
        }
        Ok(document["original_text"].clone())
    }

    fn read_owned_record(
        &self,
        op: &str,
        memory_id: &str,
        scope: &str,
        tags: &[String],
        signal: &Signal,
    ) -> Result<Value> {
        let mut current = Self::latest(&jsrt::sync::lock(&self.inner).records, memory_id)?;
        if current["state"] == "forgotten" {
            return Err(MemoryError::code(ErrorCode::Forgotten));
        }
        if matches!(current["state"].as_str(), Some("accepted" | "unknown"))
            && current["kind"] != "forget"
            && self.backend.supports("operation")
        {
            current = self.poll_pending_operations(op, &current, signal)?;
        }
        let content = if current["state"] == "stored" {
            self.fetch_stored_content(op, memory_id, tags, signal)?
        } else {
            Value::Null
        };
        self.update(
            op,
            vec![("state", "read".into()), ("observedState", current["state"].clone())],
        )?;
        Ok(
            json!({"id": memory_id, "scope": scope, "state": current["state"], "operation": current, "content": content}),
        )
    }

    fn get_owned(&self, memory_id: &str, scope: String, tags: Vec<String>, signal: &Signal) -> Result<Value> {
        let memory_id = memory_id.to_string();
        self.locked(&memory_id, || {
            let op = self.start(
                "get",
                vec![("memoryId", memory_id.clone().into()), ("scope", scope.clone().into())],
            )?;
            self.attempt(&op, signal, || {
                self.read_owned_record(&op, &memory_id, &scope, &tags, signal)
            })
        })
    }
}
