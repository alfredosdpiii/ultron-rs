//! The memorantado client over the in-process fake: Ultron's Hindsight-shaped calls as memorantado requests, claim
//! curation, file-snapshot evidence, errors and the outage pause.

use crate::fake::FakeMemorantado;
use kernel::Signal;
use kernel_apis::MemoryBackend;
use kernel_apis::memory::OUTAGE_PAUSE_AFTER as PAUSE_AFTER;
use memorantado::{MemorantadoBackend, MemorantadoOptions};
use serde_json::{Value, json};
use std::sync::Arc;

fn scopes() -> Value {
    json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"], "global": ["ultron:global:g1"]})
}

fn backend(fake: &FakeMemorantado) -> MemorantadoBackend {
    let mut options = MemorantadoOptions::new("http://memo.test", "ultron", scopes().as_object().unwrap().clone());
    options.transport = Some(Arc::new(fake.clone()));
    MemorantadoBackend::new(options).unwrap()
}

fn sig() -> Signal {
    Signal::new()
}

fn keep(backend: &MemorantadoBackend, key: &str, text: &str, scope: &str, metadata: Value) -> Value {
    backend
        .retain(
            &json!({"items": [{"content": text, "document_id": key, "tags": scopes()[scope], "update_mode": "replace",
                "metadata": metadata}], "async": true}),
            &sig(),
        )
        .unwrap()
}

#[test]
fn memorantado_retain_keeps_a_keyed_claim_synchronously_and_maps_a_typed_record() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    let answer = keep(
        &backend,
        "doc-1",
        "Run cargo fmt before committing",
        "project",
        json!({"ultron_operation": "op-1", "ultron_type": "procedure", "ultron_situation": "before a commit"}),
    );
    assert_eq!(answer, json!({"success": true, "async": false}));
    let sent = fake.captured().pop().unwrap();
    assert_eq!(
        (sent.method.as_str(), sent.path.as_str()),
        ("POST", "/semantic-memories")
    );
    assert_eq!(sent.query["project"], "ultron");
    let body = sent.body.unwrap();
    assert_eq!(body["project"], "ultron");
    assert_eq!(body["key"], "doc-1");
    assert_eq!(body["kind"], "procedure");
    assert_eq!(body["subject"], "before a commit");
    assert_eq!(body["predicate"], "procedure");
    assert_eq!(body["scope"], "project");
    assert!(
        body["object"].is_null(),
        "no object: claims never conflict by themselves"
    );
    assert_eq!(body["tags"], json!(["ultron:project:p1"]));

    // The key replaces its claim; the same text reinforces it.
    keep(
        &backend,
        "doc-1",
        "Run cargo fmt --all before committing",
        "project",
        json!({}),
    );
    let current = fake.active("doc-1").unwrap();
    assert_eq!(current["content"], "Run cargo fmt --all before committing");
    assert_eq!(fake.lock().memories.len(), 2);
    keep(
        &backend,
        "doc-1",
        "Run cargo fmt --all before committing",
        "project",
        json!({}),
    );
    assert_eq!(fake.lock().memories.len(), 2);
}

#[test]
fn memorantado_append_retain_is_an_episode() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    backend
        .retain(
            &json!({"items": [{"content": "summary", "document_id": "ultron:session:s1:summaries",
                "tags": scopes()["project"], "update_mode": "append", "metadata": {"ultron_source": "auto"}}]}),
            &sig(),
        )
        .unwrap();
    let body = fake.captured().pop().unwrap().body.unwrap();
    assert_eq!(body["session"], "ultron:session:s1:summaries");
    assert_eq!(body["extract"], false);
    assert_eq!(body["source"], "ultron:auto");
    assert_eq!(fake.lock().episodes.len(), 1);
}

#[test]
fn memorantado_recall_maps_memories_and_episodes_with_exact_tags() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    keep(
        &backend,
        "doc-p",
        "The build uses nextest",
        "project",
        json!({"ultron_operation": "op-p"}),
    );
    keep(
        &backend,
        "doc-g",
        "The user wants nextest output short",
        "global",
        json!({}),
    );
    keep(&backend, "doc-s", "nextest in this session", "session", json!({}));
    backend
        .retain(
            &json!({"items": [{"content": "nextest flaked once", "document_id": "s", "tags": scopes()["project"],
                "update_mode": "append"}]}),
            &sig(),
        )
        .unwrap();
    let tags = json!(["ultron:project:p1", "ultron:global:g1"]);
    let answer = backend
        .recall(
            &json!({"query": "nextest", "tags": tags, "tags_match": "any_strict", "max_tokens": 1024,
                "types": ["observation", "world", "experience"], "min_scores": {"final": 0.005}}),
            &sig(),
        )
        .unwrap();
    let body = fake.captured().pop().unwrap().body.unwrap();
    assert_eq!(body["tagsMatch"], "any");
    assert_eq!(body["tokenBudget"], 1024);
    assert!(body.get("min_scores").is_none());
    let results = answer["results"].as_array().unwrap();
    let texts: Vec<&str> = results.iter().map(|r| r["text"].as_str().unwrap()).collect();
    assert_eq!(
        texts,
        [
            "The build uses nextest",
            "The user wants nextest output short",
            "nextest flaked once"
        ]
    );
    assert_eq!(results[0]["document_id"], "doc-p");
    assert_eq!(results[0]["type"], "world");
    assert_eq!(results[0]["metadata"]["ultron_operation"], "op-p");
    assert!(results[0]["id"].as_str().unwrap().parse::<u64>().is_ok());
    assert_eq!(results[2]["type"], "experience");
    assert!(results[2]["document_id"].is_null());

    let worlds = backend
        .recall(
            &json!({"query": "nextest", "tags": tags, "tags_match": "exact", "types": ["world"]}),
            &sig(),
        )
        .unwrap();
    assert_eq!(fake.captured().pop().unwrap().body.unwrap()["tagsMatch"], "exact");
    assert!(
        worlds["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["type"] == "world")
    );
}

#[test]
fn memorantado_file_evidence_is_snapshotted_and_cited() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    let evidence = json!([
        {"source": "repo://src/a.rs", "content": "fn a() {}\n", "quote": "fn a()"},
        {"source": "repo://src/b.rs", "content": "fn b() {}\n", "quote": "fn b()"},
    ]);
    backend
        .retain(
            &json!({"items": [{"content": "a and b exist", "document_id": "doc-e", "tags": scopes()["project"],
                "update_mode": "replace", "metadata": {}, "evidence": evidence}]}),
            &sig(),
        )
        .unwrap();
    let paths: Vec<String> = fake
        .captured()
        .iter()
        .map(|c| format!("{} {}", c.method, c.path))
        .collect();
    let memory = fake.active("doc-e").unwrap();
    let id = memory["id"].as_u64().unwrap();
    assert_eq!(
        paths,
        [
            "POST /file-snapshots".to_string(),
            "POST /file-snapshots".into(),
            "POST /semantic-memories".into(),
            format!("POST /semantic-memories/{id}/confirm"),
        ]
    );
    let quotes: Vec<&Value> = memory["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["quote"])
        .collect();
    assert_eq!(quotes, [&json!("fn a()"), &json!("fn b()")]);
    assert_eq!(fake.lock().snapshots.len(), 2);
    assert!(memory.get("body").is_none());
    // The same file content is one snapshot.
    keep(&backend, "doc-f", "other", "project", json!({}));
    backend
        .confirm(
            &id.to_string(),
            &json!({"evidence": [{"source": "repo://src/a.rs", "content": "fn a() {}\n", "quote": "fn a()"}]}),
            &sig(),
        )
        .unwrap();
    assert_eq!(fake.lock().snapshots.len(), 2);
}

#[test]
fn memorantado_get_delete_and_mental_models_go_by_key() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    keep(
        &backend,
        "doc-1",
        "first",
        "project",
        json!({"ultron_through": "entry-9"}),
    );
    keep(
        &backend,
        "doc-1",
        "second",
        "project",
        json!({"ultron_through": "entry-9"}),
    );
    let got = backend.get("doc-1", &sig()).unwrap();
    assert_eq!(
        got,
        json!({"id": "doc-1", "original_text": "second", "tags": ["ultron:project:p1"],
            "document_metadata": {"ultron_through": "entry-9"}})
    );
    assert_eq!(backend.get("missing", &sig()).unwrap_err(), "HTTP_ERROR");
    assert_eq!(
        backend.delete("doc-1", &sig()).unwrap(),
        json!({"success": true, "document_id": "doc-1", "memory_units_deleted": 1})
    );
    assert!(fake.lock().memories.is_empty(), "the superseded version goes too");
    assert_eq!(backend.delete("doc-1", &sig()).unwrap()["memory_units_deleted"], 0);

    assert!(backend.mental_model("ultron-project-p1", &sig()).unwrap().is_null());
    assert_eq!(
        backend.create_mental_model(&json!({"id": "x"}), &sig()).unwrap(),
        json!({})
    );
    keep(
        &backend,
        "ultron-project-p1",
        "Brief text",
        "project",
        json!({"ultron_page": "brief"}),
    );
    let model = backend.mental_model("ultron-project-p1", &sig()).unwrap();
    assert_eq!(model["content"], "Brief text");
    assert_eq!(model["writer"], "dreaming");
    assert!(model["last_refreshed_at"].is_string());
    assert_eq!(fake.active("ultron-project-p1").unwrap()["kind"], "brief");
    assert!(!backend.supports("operation"));
    assert!(backend.supports("claims") && backend.supports("fileEvidence"));
    assert_eq!(backend.reflect(&json!({}), &sig()).unwrap_err(), "UNSUPPORTED_API");
}

#[test]
fn memorantado_curation_lists_confirms_supersedes_invalidates_and_flags_conflicts() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    let old = fake.seed(json!({"content": "old claim", "tags": ["ultron:project:p1"], "key": "k-old"}));
    fake.lock().now += 86_400_000;
    let new = fake.seed(json!({"content": "new claim", "tags": ["ultron:project:p1"], "key": "k-new"}));
    fake.seed(json!({"content": "global claim", "tags": ["ultron:global:g1"]}));

    let listed = backend
        .list(
            &json!({"tags": ["ultron:project:p1"], "status": "active", "sort": "confirmed", "limit": 10}),
            &sig(),
        )
        .unwrap();
    let query = &fake.captured().pop().unwrap().query;
    assert_eq!(query["tags"], "ultron:project:p1");
    assert_eq!(query["tagsMatch"], "any");
    assert_eq!(query["sort"], "confirmed");
    let claims = listed["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0]["id"], old.to_string());
    assert_eq!(claims[0]["text"], "old claim");
    assert_eq!(claims[0]["key"], "k-old");
    assert_eq!(claims[0]["metadata"], json!({}));

    let confirmed = backend.confirm(&old.to_string(), &json!({}), &sig()).unwrap();
    assert!(confirmed["lastConfirmedAt"].is_string());
    let revised = backend
        .supersede(
            &new.to_string(),
            &json!({"text": "newer claim", "metadata": {"a": "b"}}),
            &sig(),
        )
        .unwrap();
    assert_eq!(revised["text"], "newer claim");
    assert_eq!(revised["key"], "k-new");
    assert_eq!(revised["tags"], json!(["ultron:project:p1"]));
    let conflict = backend
        .conflict(
            &json!({"id": old.to_string(), "conflictingId": revised["id"], "reason": "they disagree"}),
            &sig(),
        )
        .unwrap();
    assert_eq!(conflict["resolutionStatus"], "open");
    let retired = backend
        .curate(
            &old.to_string(),
            &json!({"state": "invalidated", "reason": "wrong"}),
            &sig(),
        )
        .unwrap();
    assert_eq!(retired["status"], "invalidated");
    let unit = backend.unit(&old.to_string(), &sig()).unwrap();
    assert_eq!(unit["text"], "old claim");
    assert_eq!(unit["type"], "world");
    assert_eq!(backend.unit("unit-abc", &sig()).unwrap_err(), "UNKNOWN_MEMORY");
    assert_eq!(
        backend.list(&json!({"tags": []}), &sig()).unwrap_err(),
        "UNSUPPORTED_SCOPE"
    );
}

#[test]
fn memorantado_errors_are_codes_and_an_outage_pauses_the_client() {
    let fake = FakeMemorantado::new();
    let backend = backend(&fake);
    fake.lock().fail_next.push("POST /semantic-memories".into());
    let error = backend
        .retain(
            &json!({"items": [{"content": "x", "document_id": "d", "tags": scopes()["project"], "update_mode": "replace"}]}),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(error, "HTTP_ERROR");

    fake.lock().down = true;
    for _ in 0..PAUSE_AFTER {
        assert_eq!(backend.get("d", &sig()).unwrap_err(), "HTTP_ERROR");
    }
    fake.lock().down = false;
    let before = fake.captured().len();
    assert_eq!(backend.get("d", &sig()).unwrap_err(), "HTTP_ERROR");
    assert_eq!(fake.captured().len(), before, "a paused client sends nothing");

    let aborted = Signal::new();
    aborted.abort("stop");
    assert_eq!(backend.get("d", &aborted).unwrap_err(), "ABORTED");

    let bad = |url: &str, project: &str| {
        MemorantadoBackend::new(MemorantadoOptions::new(url, project, Default::default()))
            .err()
            .map(|e| e.code.as_str())
    };
    assert_eq!(bad("ftp://x", "ultron"), Some("INVALID_CONFIG"));
    assert_eq!(bad("http://x", " "), Some("INVALID_CONFIG"));
    let mut tags = serde_json::Map::new();
    tags.insert("project".into(), json!(["has space"]));
    assert_eq!(
        MemorantadoBackend::new(MemorantadoOptions::new("http://x", "ultron", tags))
            .err()
            .map(|e| e.code.as_str()),
        Some("UNSUPPORTED_SCOPE")
    );
}
