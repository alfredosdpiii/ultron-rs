//! A16-A18 and A28-A30 over memorantado: the same rows as the Hindsight ports beside this file, through the real
//! memorantado client (`memorantado::MemorantadoBackend`) over the fake memorantado server it is tested with
//! (crates/memorantado/tests/support/fake.rs). What differs is the backend's own contract: memorantado writes are
//! synchronous (a write is `stored` at once, never only `accepted`), a key replaces its claim, and scopes are tags
//! matched exactly.

use crate::memorantado_fake::FakeMemorantado;
use crate::memory_support::{GateLog, assert_match, durable_store, gate, keep_gate, p, sha256, sig};
use kernel::store::MemoryDocumentStore;
use kernel_apis::memory::MemoryGate;
use kernel_apis::{MemoryBackend, MemoryService};
use memorantado::{MemorantadoBackend, MemorantadoOptions};
use serde_json::{Value, json};
use std::sync::Arc;

fn scopes() -> Value {
    json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"], "global": ["ultron:global:g1"]})
}

fn backend(fake: &FakeMemorantado) -> Arc<dyn MemoryBackend> {
    let mut options = MemorantadoOptions::new("http://memo.test", "ultron", scopes().as_object().unwrap().clone());
    options.transport = Some(Arc::new(fake.clone()));
    Arc::new(MemorantadoBackend::new(options).unwrap())
}

fn open(fake: &FakeMemorantado, store: &Arc<MemoryDocumentStore>, gate: MemoryGate) -> MemoryService {
    MemoryService::new(store.clone(), backend(fake), gate).unwrap()
}

fn recalls(fake: &FakeMemorantado) -> Vec<Value> {
    fake.captured()
        .into_iter()
        .filter(|c| c.path == "/retrieve-context")
        .map(|c| c.body.unwrap())
        .collect()
}

fn writes(fake: &FakeMemorantado) -> usize {
    fake.calls("POST", "/semantic-memories")
}

#[test]
fn acceptance_memorantado_a16_skip_sends_nothing_secrets_never_leave_and_scopes_stay_exact() {
    let fake = FakeMemorantado::new();
    let store = durable_store();
    let log = GateLog::default();
    let skipping = open(&fake, &store, gate(false, log.clone()));
    let skipped = skipping
        .prepare(&p(json!({"query": "anything", "taskId": "t"})), &sig())
        .unwrap();
    assert_eq!(skipped["operation"]["state"], "skipped");
    assert!(recalls(&fake).is_empty(), "a skipped recall sends nothing");

    let memory = open(&fake, &store, kernel_apis::worker_gate());
    let token = format!("ghp_{}", "a".repeat(36));
    let refused = memory
        .propose(
            &p(json!({"text": format!("the token is {token}"), "evidence": [{"ref": "e"}]})),
            &sig(),
        )
        .unwrap();
    assert_eq!(refused["state"], "sensitive");
    assert_eq!(writes(&fake), 0, "a secret is refused before any request");

    for scope in ["session", "project", "global"] {
        memory
            .propose(
                &p(json!({"text": format!("{scope} runner is nextest"), "evidence": [{"ref": "e"}], "scope": scope})),
                &sig(),
            )
            .unwrap();
    }
    for scope in ["session", "project", "global"] {
        let found = memory
            .prepare(
                &p(json!({"query": "runner nextest", "taskId": scope, "scope": scope})),
                &sig(),
            )
            .unwrap();
        let texts: Vec<&str> = found["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, [format!("{scope} runner is nextest")], "{scope}");
        let sent = recalls(&fake).pop().unwrap();
        assert_eq!((&sent["tags"], &sent["tagsMatch"]), (&scopes()[scope], &json!("exact")));
    }
    assert!(
        fake.lock()
            .memories
            .iter()
            .all(|m| m["tags"].as_array().unwrap().len() == 1)
    );
}

#[test]
fn acceptance_memorantado_a17_correction_supersedes_and_evidence_classes_stay_distinct() {
    let fake = FakeMemorantado::new();
    let store = durable_store();
    let first = open(&fake, &store, keep_gate());
    let kept = first
        .propose(
            &p(json!({"text": "deploys run on fridays", "evidence": [{"ref": "e:1"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    let memory_id = kept["memoryId"].clone();
    let corrected = open(&fake, &store, keep_gate())
        .correct(
            &p(json!({"id": memory_id, "text": "deploys run on mondays", "evidence": [{"ref": "e:2"}]})),
            &sig(),
        )
        .unwrap();
    assert_eq!(corrected["supersedes"], kept["id"]);
    // memorantado keeps the history: the old claim is superseded, the key's current claim is the correction.
    let statuses: Vec<(String, String)> = fake
        .lock()
        .memories
        .iter()
        .map(|m| {
            (
                m["content"].as_str().unwrap().into(),
                m["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [
            ("deploys run on fridays".to_string(), "superseded".to_string()),
            ("deploys run on mondays".into(), "active".into())
        ]
    );
    let fresh = open(&fake, &store, keep_gate())
        .prepare(
            &p(json!({"query": "when do deploys run", "taskId": "t", "scope": "project"})),
            &sig(),
        )
        .unwrap();
    let texts: Vec<&Value> = fresh["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["text"])
        .collect();
    assert_eq!(texts, [&json!("deploys run on mondays")]);

    for class in ["hypothesis", "user_statement", "tool_evidence"] {
        first
            .propose(
                &p(json!({"text": format!("claim {class}"), "evidence": [{"ref": "e"}], "evidenceClass": class})),
                &sig(),
            )
            .unwrap();
    }
    let labels = open(&fake, &store, keep_gate())
        .prepare(&p(json!({"query": "claim", "taskId": "c"})), &sig())
        .unwrap()["context"]
        .as_str()
        .unwrap()
        .to_string();
    for label in [
        "[unconfirmed hypothesis] claim hypothesis",
        "[user statement] claim user_statement",
        "[tool evidence] claim tool_evidence",
    ] {
        assert!(labels.contains(label), "{labels}");
    }
    // The REPL's module refuses a claim the model calls verified (only the host's checks mark one).
    let refuse: Arc<dyn kernel::HostHandler> =
        Arc::new(|kind: &str, _: serde_json::Map<String, Value>, _: &kernel::Signal| Err(format!("Unknown: {kind}")));
    let repl = kernel_apis::KernelApis::new(refuse).with(Arc::new(kernel_apis::MemoryApi { service: Some(first) }));
    let verified = kernel::HostHandler::handle(
        &repl,
        "memory.propose",
        p(json!({"text": "x", "evidence": [{"ref": "e"}], "evidenceClass": "verified"})),
        &sig(),
    );
    assert!(verified.unwrap_err().contains("only by the host"));
}

#[test]
fn acceptance_memorantado_a18_writes_are_stored_at_once_a_lost_write_is_unknown_and_forget_persists() {
    let fake = FakeMemorantado::new();
    let store = durable_store();
    let memory = open(&fake, &store, keep_gate());
    let stored = memory
        .propose(&p(json!({"text": "fact A", "evidence": [{"ref": "task:1"}]})), &sig())
        .unwrap();
    assert_match(&stored, &json!({"state": "stored", "operationIds": []}));
    let got = open(&fake, &store, keep_gate())
        .get(&p(json!({"id": stored["memoryId"]})), &sig())
        .unwrap();
    assert_match(&got, &json!({"state": "stored", "content": "fact A"}));

    // A write whose answer is lost is unknown: never replayed, and its document stays busy.
    fake.lock().fail_next.push("POST /semantic-memories".into());
    let error = memory
        .propose(&p(json!({"text": "fact C", "evidence": [{"ref": "task:3"}]})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "HTTP_ERROR");
    let before = writes(&fake);
    let reopened = open(&fake, &store, keep_gate());
    let lost = reopened
        .list()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["kind"] == "propose" && op["state"] == "unknown")
        .cloned()
        .unwrap();
    let busy = reopened
        .correct(
            &p(json!({"id": lost["memoryId"], "text": "fact C'", "evidence": [{"ref": "task:4"}]})),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(busy.code, "BUSY");
    assert_eq!(writes(&fake), before);

    // Forgetting persists across a restart and outranks a server still returning the text.
    let forgotten = reopened.forget(&p(json!({"id": stored["memoryId"]})), &sig()).unwrap();
    assert_eq!(forgotten["state"], "forgotten");
    assert!(fake.active(stored["memoryId"].as_str().unwrap()).is_none());
    fake.seed(
        json!({"content": "fact A", "tags": scopes()["session"], "key": stored["memoryId"],
        "metadata": {"ultron_operation": stored["id"]}}),
    );
    let found = open(&fake, &store, keep_gate())
        .prepare(&p(json!({"query": "fact", "taskId": "after"})), &sig())
        .unwrap();
    assert!(found["results"].as_array().unwrap().is_empty(), "{found}");
}

#[test]
fn acceptance_memorantado_a28_recorded_decision_equals_the_executed_search() {
    let fake = FakeMemorantado::new();
    let memory = open(&fake, &durable_store(), keep_gate());
    memory
        .propose(&p(json!({"text": "project uses vitest", "evidence": [{"ref": "tool:cat package.json"}], "scope": "project"})), &sig())
        .unwrap();
    let query = "which test runner does the project use vitest";
    let prepared = memory
        .prepare(
            &p(json!({"query": query, "scope": "project", "taskId": "task-1"})),
            &sig(),
        )
        .unwrap();
    let sent = recalls(&fake).pop().unwrap();
    let decision = memory.why(&json!("task-1")).unwrap()[0].clone();
    assert_match(
        &decision,
        &json!({"kind": "prepare", "state": "recalled", "scope": "project"}),
    );
    assert_eq!(decision["queryHash"], sha256(sent["query"].as_str().unwrap()));
    assert_eq!(decision["tags"], sent["tags"]);
    let references = decision["references"].as_array().unwrap();
    let results = prepared["results"].as_array().unwrap();
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["id"], results[0]["id"]);
    assert_eq!(references[0]["textHash"], sha256("project uses vitest"));

    // A scope the service has no tags for is refused before the gate or the server.
    let mut narrow = scopes();
    narrow.as_object_mut().unwrap().remove("global");
    let mut options = MemorantadoOptions::new("http://memo.test", "ultron", narrow.as_object().unwrap().clone());
    options.transport = Some(Arc::new(fake.clone()));
    let log = GateLog::default();
    let scoped = MemoryService::new(
        durable_store(),
        Arc::new(MemorantadoBackend::new(options).unwrap()),
        gate(true, log.clone()),
    )
    .unwrap();
    let before = recalls(&fake).len();
    let error = scoped
        .prepare(&p(json!({"query": "q", "scope": "global", "taskId": "t"})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "UNSUPPORTED_SCOPE");
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(recalls(&fake).len(), before);
}

#[test]
fn acceptance_memorantado_a29_why_shows_the_injected_evidence_without_a_new_search() {
    let fake = FakeMemorantado::new();
    let memory = open(&fake, &durable_store(), keep_gate());
    memory
        .propose(
            &p(json!({"text": "use pnpm here", "evidence": [{"ref": "user:said"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    memory
        .prepare(
            &p(json!({"query": "package manager pnpm", "scope": "project", "taskId": "t1"})),
            &sig(),
        )
        .unwrap();
    let searches = recalls(&fake).len();
    let why = memory.why(&json!("t1")).unwrap();
    assert_eq!(why.as_array().unwrap().len(), 1);
    assert_eq!(why[0]["references"][0]["textHash"], sha256("use pnpm here"));
    assert_eq!(recalls(&fake).len(), searches, "no new search");
    assert!(memory.why(&json!("other")).unwrap().as_array().unwrap().is_empty());
}

#[test]
fn acceptance_memorantado_a30_forgotten_and_corrected_memories_leave_every_view() {
    let fake = FakeMemorantado::new();
    let store = durable_store();
    let memory = open(&fake, &store, kernel_apis::worker_gate());
    let wrong = memory
        .propose(
            &p(json!({"text": "the api lives in src/old", "evidence": [{"ref": "user:said"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    let fixed = memory
        .propose(
            &p(json!({"text": "the cache lives in tmp", "evidence": [{"ref": "user:said"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    memory.forget(&p(json!({"id": wrong["memoryId"]})), &sig()).unwrap();
    memory
        .correct(
            &p(json!({"id": fixed["memoryId"], "text": "the cache lives in var", "evidence": [{"ref": "user:said"}]})),
            &sig(),
        )
        .unwrap();
    let fresh = open(&fake, &store, kernel_apis::worker_gate());
    let texts: Vec<String> = fresh
        .prepare(
            &p(json!({"query": "where does the api cache live", "taskId": "v", "scope": "project"})),
            &sig(),
        )
        .unwrap()["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts, ["the cache lives in var"]);
    let error = fresh.get(&p(json!({"id": wrong["memoryId"]})), &sig()).unwrap_err();
    assert_eq!(error.code, "FORGOTTEN");
    let active: Vec<String> = fake
        .lock()
        .memories
        .iter()
        .filter(|m| m["status"] == "active")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(active, ["the cache lives in var"]);
}
