//! Automatic memory's service calls (kernel_apis::memory `recall_for_turn`, `retain_for_session`): a Rust-only
//! feature with no TypeScript reference, so literal expectations over an in-process Hindsight-shaped bank.

use kernel::Signal;
use kernel_apis::memory::{MemoryBackend, handle};
use kernel_apis::{MemoryDocumentStore, MemoryService, worker_gate};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

/// Answers every recall with `results`; keeps every request.
struct Bank {
    results: Arc<Mutex<Value>>,
    recalls: Arc<Mutex<Vec<Value>>>,
    retains: Arc<Mutex<Vec<Value>>>,
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
    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"id": id, "original_text": "kept", "tags": ["ultron:project:p1"]}))
    }
    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"success": true, "document_id": id}))
    }
    fn operation(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"operation_id": id, "status": "completed"}))
    }
}

type Requests = Arc<Mutex<Vec<Value>>>;

fn service(results: Value) -> (MemoryService, Requests, Requests) {
    let (memory, recalls, retains, _) = service_with(results);
    (memory, recalls, retains)
}

fn service_with(results: Value) -> (MemoryService, Requests, Requests, Arc<Mutex<Value>>) {
    let (recalls, retains) = (Requests::default(), Requests::default());
    let results = Arc::new(Mutex::new(results));
    let bank = Bank {
        results: results.clone(),
        recalls: recalls.clone(),
        retains: retains.clone(),
    };
    let memory = MemoryService::new(Arc::new(MemoryDocumentStore::default()), Arc::new(bank), worker_gate()).unwrap();
    (memory, recalls, retains, results)
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
