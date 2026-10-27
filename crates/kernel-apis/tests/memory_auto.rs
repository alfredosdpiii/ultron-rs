//! Automatic memory's service calls (kernel_apis::memory `recall_for_turn`, `retain_for_session`,
//! `retain_session_tail`, `project_digest`), evidence quotes and staleness, and the model's `memory.reflect` /
//! `memory.invalidate`: Rust-only, with no TypeScript reference, so literal
//! expectations over an in-process Hindsight-shaped bank.

use kernel::Signal;
use kernel_apis::memory::{MemoryBackend, handle};
use kernel_apis::{MemoryDocumentStore, MemoryService, worker_gate};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

/// Answers every recall with `results`; keeps every request. Its units are `f1` (project) and `other` (another
/// project's); its mental model is `model` (`null`: none yet).
struct Bank {
    results: Arc<Mutex<Value>>,
    recalls: Arc<Mutex<Vec<Value>>>,
    retains: Arc<Mutex<Vec<Value>>>,
    model: Arc<Mutex<Value>>,
    /// Every Rust-only call: `[call, id, body]`.
    calls: Arc<Mutex<Vec<Value>>>,
}

impl MemoryBackend for Bank {
    fn namespace(&self) -> String {
        "fake://bank".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({
            "session": ["ultron:session:s1"],
            "project": ["ultron:project:p1"],
            "global": ["ultron:global:home"],
        })
        .as_object()
        .unwrap()
        .clone()
    }
    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.recalls.lock().unwrap().push(request.clone());
        Ok(json!({"results": *self.results.lock().unwrap()}))
    }
    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.retains.lock().unwrap().push(request.clone());
        Ok(json!({"success": true, "async": false}))
    }
    /// The document as its newest retain left it (its metadata, as Hindsight keeps it).
    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        let retains = self.retains.lock().unwrap();
        let item = retains
            .iter()
            .rev()
            .flat_map(|request| request["items"].as_array().cloned().unwrap_or_default())
            .find(|item| item["document_id"] == id);
        Ok(json!({
            "id": id, "original_text": "kept", "tags": ["ultron:project:p1"],
            "document_metadata": item.map_or(Value::Null, |item| item["metadata"].clone()),
        }))
    }
    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"success": true, "document_id": id}))
    }
    fn operation(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"operation_id": id, "status": "completed"}))
    }
    fn unit(&self, id: &str, _: &Signal) -> Result<Value, String> {
        self.calls.lock().unwrap().push(json!(["unit", id, null]));
        match id {
            "f1" => Ok(json!({"id": "f1", "text": "Stale.", "tags": ["ultron:project:p1"]})),
            "other" => Ok(json!({"id": "other", "text": "Theirs.", "tags": ["ultron:project:p2"]})),
            _ => Err("HTTP_ERROR".into()),
        }
    }
    fn curate(&self, id: &str, request: &Value, _: &Signal) -> Result<Value, String> {
        self.calls.lock().unwrap().push(json!(["curate", id, request]));
        Ok(json!({"id": id, "state": "invalidated"}))
    }
    fn reflect(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.calls.lock().unwrap().push(json!(["reflect", null, request]));
        Ok(json!({"text": "Run the tests with nextest.", "based_on": {"memories": []}}))
    }
    fn mental_model(&self, id: &str, _: &Signal) -> Result<Value, String> {
        self.calls.lock().unwrap().push(json!(["mental_model", id, null]));
        Ok(self.model.lock().unwrap().clone())
    }
    fn create_mental_model(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push(json!(["create_mental_model", null, request]));
        Ok(json!({"operation_id": "op1"}))
    }
}

type Requests = Arc<Mutex<Vec<Value>>>;

fn service(results: Value) -> (MemoryService, Requests, Requests) {
    let (memory, recalls, retains, _) = service_with(results);
    (memory, recalls, retains)
}

fn service_with(results: Value) -> (MemoryService, Requests, Requests, Arc<Mutex<Value>>) {
    let (memory, recalls, retains, results, _, _) = service_full(results);
    (memory, recalls, retains, results)
}

type Bench = (
    MemoryService,
    Requests,
    Requests,
    Arc<Mutex<Value>>,
    Arc<Mutex<Value>>,
    Requests,
);

/// The service, its recall and retain requests, its recall results, its mental model and its other calls.
fn service_full(results: Value) -> Bench {
    let (recalls, retains, calls) = (Requests::default(), Requests::default(), Requests::default());
    let (results, model) = (Arc::new(Mutex::new(results)), Arc::new(Mutex::new(Value::Null)));
    let bank = Bank {
        results: results.clone(),
        recalls: recalls.clone(),
        retains: retains.clone(),
        model: model.clone(),
        calls: calls.clone(),
    };
    let memory = MemoryService::new(Arc::new(MemoryDocumentStore::default()), Arc::new(bank), worker_gate()).unwrap();
    (memory, recalls, retains, results, model, calls)
}

fn fact(id: &str, text: &str, tag: &str) -> Value {
    json!({"id": id, "text": text, "type": "observation", "tags": [tag]})
}

#[test]
fn kernel_apis_memory_recall_for_turn_asks_project_and_global_cheaply_and_keeps_their_results() {
    let (memory, recalls, _) = service(json!([
        fact("f1", "Tests run under nice -n 19.", "ultron:project:p1"),
        fact("f2", "The user prefers short answers.", "ultron:global:home"),
        fact("f3", "This session's scratch note.", "ultron:session:s1"),
        fact("f4", "Two scopes at once.", "ultron:project:p1"),
        json!({"id": "f5", "text": "untagged"}),
        fact("f6", "Over the limit.", "ultron:project:p1"),
    ]));
    let found = memory
        .recall_for_turn("how do I run the tests", 3, &Signal::new())
        .unwrap();
    assert_eq!(
        recalls.lock().unwrap()[0],
        json!({
            "query": "how do I run the tests", "tags": ["ultron:project:p1", "ultron:global:home"],
            "tags_match": "any_strict", "types": ["observation", "world", "experience"],
            "prefer_observations": true, "budget": "low", "max_tokens": 1024, "include": {"entities": null},
            "trace": false,
        })
    );
    // A session-scoped or untagged result is not this turn's; the limit cuts the rest.
    assert_eq!(
        found,
        [
            json!({"id": "f1", "text": "Tests run under nice -n 19.", "scope": "project", "type": "observation"}),
            json!({"id": "f2", "text": "The user prefers short answers.", "scope": "global", "type": "observation"}),
            json!({"id": "f4", "text": "Two scopes at once.", "scope": "project", "type": "observation"}),
        ]
    );
    assert_eq!(
        memory.recall_for_turn("  ", 3, &Signal::new()).unwrap(),
        Vec::<Value>::new()
    );
    assert_eq!(recalls.lock().unwrap().len(), 1, "a blank message recalls nothing");
}

#[test]
fn kernel_apis_memory_recall_for_turn_withholds_what_the_user_forgot() {
    let (memory, _, _, results) = service_with(json!([]));
    let call = |kind: &str, payload: Value| handle(Some(&memory), kind, payload.as_object().unwrap(), &Signal::new());
    let proposed = call(
        "memory.propose",
        json!({"text": "Deploys go through staging first.", "evidence": [{"ref": "notes"}], "scope": "project"}),
    )
    .unwrap();
    let memory_id = proposed["memoryId"].clone();
    assert_eq!(
        call("memory.forget", json!({"id": memory_id})).unwrap()["state"],
        "forgotten"
    );
    // The bank's index lags the delete and still returns the document: the local journal outranks it.
    let mut lagging = fact("f1", "Deploys go through staging first.", "ultron:project:p1");
    lagging["document_id"] = memory_id;
    *results.lock().unwrap() = json!([lagging, fact("f2", "Kept.", "ultron:project:p1")]);
    let found = memory
        .recall_for_turn("how do deploys work here", 6, &Signal::new())
        .unwrap();
    assert_eq!(
        found,
        [json!({"id": "f2", "text": "Kept.", "scope": "project", "type": "observation"})]
    );
}

#[test]
fn kernel_apis_memory_retain_for_session_appends_in_the_background_and_skips_secrets() {
    let (memory, _, retains) = service(json!([]));
    let kept = memory
        .retain_for_session(
            "ultron:session:s1:summaries",
            "## Goal\nFaster journal writes.",
            "Compaction summary.",
            "2026-10-27T08:00:00.000Z",
            "project",
            &Signal::new(),
        )
        .unwrap();
    assert!(kept);
    assert_eq!(
        retains.lock().unwrap()[0],
        json!({
            "items": [{
                "content": "## Goal\nFaster journal writes.", "context": "Compaction summary.",
                "timestamp": "2026-10-27T08:00:00.000Z", "document_id": "ultron:session:s1:summaries",
                "tags": ["ultron:project:p1"], "update_mode": "append", "metadata": {"ultron_source": "auto"},
            }],
            "async": true,
        })
    );
    let secret = format!("The {}={}", "api_key", ["sk", "fake", "0123456789"].join("-"));
    let kept = memory
        .retain_for_session("d", &secret, "c", "t", "project", &Signal::new())
        .unwrap();
    assert!(!kept);
    assert_eq!(
        retains.lock().unwrap().len(),
        1,
        "a summary holding a secret is not sent"
    );
}

fn request(memory: &MemoryService, kind: &str, payload: Value) -> Result<Value, String> {
    handle(Some(memory), kind, payload.as_object().unwrap(), &Signal::new())
}

#[test]
fn kernel_apis_memory_reflect_asks_the_read_scopes_or_the_one_named() {
    let (memory, _, _, _, _, calls) = service_full(json!([]));
    assert_eq!(
        request(&memory, "memory.reflect", json!({"query": "how are tests run"})),
        Ok(json!({"text": "Run the tests with nextest."}))
    );
    request(
        &memory,
        "memory.reflect",
        json!({"query": "what did we decide", "scope": "session"}),
    )
    .unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls[0][2],
        json!({"query": "how are tests run", "budget": "low", "max_tokens": 1024,
            "tags": ["ultron:project:p1", "ultron:global:home"], "tags_match": "any_strict"})
    );
    assert_eq!(calls[1][2]["tags"], json!(["ultron:session:s1"]));
    for bad in [
        json!({}),
        json!({"query": ""}),
        json!({"query": "q", "scope": "team"}),
        json!({"query": "q", "x": 1}),
    ] {
        assert_eq!(
            request(&memory, "memory.reflect", bad),
            Err("Invalid memory request: memory.reflect".into())
        );
    }
}

#[test]
fn kernel_apis_memory_invalidate_retires_only_this_sessions_facts() {
    let (memory, _, _, _, _, calls) = service_full(json!([]));
    assert_eq!(
        request(
            &memory,
            "memory.invalidate",
            json!({"id": "f1", "reason": "The suite moved to nextest."})
        ),
        Ok(json!({"id": "f1", "state": "invalidated"}))
    );
    assert_eq!(
        calls.lock().unwrap()[1],
        json!(["curate", "f1", {"state": "invalidated", "reason": "The suite moved to nextest."}])
    );
    assert_eq!(
        request(&memory, "memory.invalidate", json!({"id": "other"})),
        Err("Memory operation did not complete (UNKNOWN_MEMORY)".into()),
        "another project's fact is not this session's to retire"
    );
    let secret = format!("leaked {}={}", "api_key", ["sk", "fake", "0123456789"].join("-"));
    request(&memory, "memory.invalidate", json!({"id": "f1", "reason": secret})).unwrap();
    let calls = calls.lock().unwrap();
    let curated: Vec<&Value> = calls.iter().filter(|call| call[0] == "curate").collect();
    assert_eq!(curated.len(), 2, "{calls:?}");
    assert_eq!(
        curated[1][2],
        json!({"state": "invalidated"}),
        "a sensitive reason is not sent"
    );
}

#[test]
fn kernel_apis_memory_project_digest_creates_the_model_once_then_reads_it() {
    let (memory, _, _, _, model, calls) = service_full(json!([]));
    assert_eq!(memory.project_digest(&Signal::new()), Ok(None));
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0], json!(["mental_model", "ultron-project-p1", null]));
        let created = &calls[1][2];
        assert_eq!(
            (
                calls[1][0].as_str(),
                &created["id"],
                &created["tags"],
                &created["trigger"]
            ),
            (
                Some("create_mental_model"),
                &json!("ultron-project-p1"),
                &json!(["ultron:project:p1", "ultron:global:home"]),
                &json!({"refresh_after_consolidation": true, "min_refresh_interval_seconds": 3600,
                    "mode": "delta", "budget": "low", "tags_match": "any_strict"})
            )
        );
        assert!(created["source_query"].as_str().unwrap().contains("conventions"));
    }
    *model.lock().unwrap() = json!({"id": "ultron-project-p1", "content": "  ", "last_refreshed_at": null});
    assert_eq!(memory.project_digest(&Signal::new()), Ok(None), "no content yet");
    *model.lock().unwrap() = json!({"id": "ultron-project-p1", "content": "## Commands\n- nextest\n",
        "last_refreshed_at": "2026-10-27T08:00:00+00:00"});
    assert_eq!(
        memory.project_digest(&Signal::new()),
        Ok(Some(
            json!({"id": "ultron-project-p1", "content": "## Commands\n- nextest",
            "refreshedAt": "2026-10-27T08:00:00+00:00"})
        ))
    );
    let created = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call[0] == "create_mental_model")
        .count();
    assert_eq!(created, 1);
}

#[test]
fn kernel_apis_memory_retain_session_tail_replaces_its_document_once_per_end() {
    let (memory, _, retains) = service(json!([]));
    let keep = |through: &str, text: &str| {
        memory
            .retain_session_tail(
                "ultron:session:s1:tail:start",
                text,
                "Session tail.",
                through,
                &Signal::new(),
            )
            .unwrap()
    };
    assert!(keep("e4", "User: speed up the journal\n\nAgent: Done."));
    let sent = retains.lock().unwrap()[0].clone();
    let item = &sent["items"][0];
    assert_eq!(sent["async"], true);
    assert_eq!(item["update_mode"], "replace");
    assert_eq!(item["document_id"], "ultron:session:s1:tail:start");
    assert_eq!(item["tags"], json!(["ultron:project:p1"]));
    assert_eq!(
        item["metadata"],
        json!({"ultron_source": "auto", "ultron_through": "e4"})
    );
    assert_eq!(item["context"], "Session tail.");
    assert!(item["timestamp"].as_str().is_some_and(|at| at.ends_with('Z')));
    assert!(
        !keep("e4", "User: speed up the journal\n\nAgent: Done."),
        "a tail the document already ends with is not sent again"
    );
    assert!(keep(
        "e9",
        "User: speed up the journal\n\nAgent: Done.\n\nUser: and the tests?"
    ));
    assert_eq!(retains.lock().unwrap().len(), 2);
    let secret = format!("User: use {}={}", "api_key", ["sk", "fake", "0123456789"].join("-"));
    assert!(!keep("e12", &secret), "a tail holding a secret is not sent");
    assert_eq!(retains.lock().unwrap().len(), 2);
}

#[test]
fn kernel_apis_memory_evidence_quotes_go_to_hindsight_never_to_the_journal() {
    let (memory, _, retains) = service(json!([]));
    let proposed = request(
        &memory,
        "memory.propose",
        json!({
            "text": "Journal writes go through op_journal.", "scope": "project",
            "evidence": [{"ref": "src/journal.rs:12-14", "quote": "fn  commit(\n  &self"}, {"ref": "notes"}],
        }),
    )
    .unwrap();
    let item = &retains.lock().unwrap()[0]["items"][0];
    assert_eq!(
        item["metadata"]["ultron_quotes"],
        json!([{"path": "src/journal.rs", "quote": "fn  commit(\n  &self"}]).to_string()
    );
    assert_eq!(
        proposed["evidence"],
        json!([{"ref": "src/journal.rs:12-14"}, {"ref": "notes"}]),
        "the journal keeps refs only"
    );
    for evidence in [
        json!([{"ref": "/etc/passwd", "quote": "root"}]),
        json!([{"ref": "../other/a.rs", "quote": "x"}]),
        json!([{"ref": "https://example.com/a", "quote": "x"}]),
        json!([{"ref": "a.rs", "quote": "x".repeat(301)}]),
        json!([{"ref": "a.rs", "quote": " "}]),
        json!([{"ref": "a.rs", "quote": ""}]),
        json!(
            (0..5)
                .map(|n| json!({"ref": format!("a{n}.rs"), "quote": "x"}))
                .collect::<Vec<_>>()
        ),
    ] {
        assert!(
            request(
                &memory,
                "memory.propose",
                json!({"text": "t", "evidence": evidence.clone()})
            )
            .is_err(),
            "{evidence}"
        );
    }
    let secret = format!("{}={}", "api_key", ["sk", "fake", "0123456789"].join("-"));
    let refused = request(
        &memory,
        "memory.propose",
        json!({"text": "Keys live in the env file.", "evidence": [{"ref": "a.rs", "quote": secret}]}),
    )
    .unwrap();
    assert_eq!(
        refused["state"], "sensitive",
        "a quote holding a secret is screened too"
    );
}

#[test]
fn kernel_apis_memory_recall_marks_a_memory_stale_when_its_quote_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/journal.rs"), "pub fn commit(\n    &self,\n) {}\n").unwrap();
    std::fs::write(dir.path().join("src/old.rs"), "fn renamed() {}\n").unwrap();
    let quoted = |id: &str, path: &str, quote: &str, tag: &str| {
        let mut item = fact(id, &format!("{id} text"), tag);
        item["metadata"] = json!({"ultron_quotes": json!([{"path": path, "quote": quote}]).to_string()});
        item
    };
    let (memory, _, _, results) = service_with(json!([
        quoted("held", "src/journal.rs", "fn commit( &self,", "ultron:project:p1"),
        quoted("changed", "src/old.rs", "fn original()", "ultron:project:p1"),
        quoted("gone", "src/removed.rs", "x", "ultron:project:p1"),
        quoted("elsewhere", "src/removed.rs", "x", "ultron:global:home"),
        fact("plain", "No quotes.", "ultron:project:p1"),
    ]));
    let memory = memory.with_root(dir.path());
    let found = memory
        .recall_for_turn("how are journal writes done", 6, &Signal::new())
        .unwrap();
    let stale: Vec<(&str, Option<&str>)> = found
        .iter()
        .map(|item| (item["id"].as_str().unwrap(), item["stale"].as_str()))
        .collect();
    assert_eq!(
        stale,
        [
            ("held", None),
            ("changed", Some("src/old.rs no longer has the quoted text")),
            ("gone", Some("src/removed.rs is gone")),
            ("elsewhere", None),
            ("plain", None),
        ],
        "whitespace aside; a global memory's quotes name another project's files"
    );
    // A deliberate `memory.prepare` marks them too.
    *results.lock().unwrap() = json!([
        quoted("held", "src/journal.rs", "fn commit( &self,", "ultron:project:p1"),
        quoted("gone", "src/removed.rs", "x", "ultron:project:p1"),
    ]);
    let prepared = request(
        &memory,
        "memory.prepare",
        json!({"query": "journal writes", "taskId": "t1", "scope": "project"}),
    )
    .unwrap();
    assert_eq!(prepared["results"][0].get("stale"), None);
    assert_eq!(prepared["results"][1]["stale"], "src/removed.rs is gone");
    assert!(
        prepared["context"]
            .as_str()
            .unwrap()
            .ends_with("2. gone text [stale: src/removed.rs is gone]"),
        "{}",
        prepared["context"]
    );
}
