//! Dreaming and the pages it writes, through `MemoryService` over the memorantado client and the fake server:
//! plain-code checks first (no judge call when nothing changed), then the judge's verdicts as confirm, revise,
//! retract and conflict; the startup brief and topic pages within their budgets; file evidence as snapshots and
//! session spans on a write; `verified` kept for the host.

use crate::fake::FakeMemorantado;
use kernel::store::MemoryDocumentStore;
use kernel::{HostHandler, Signal};
use kernel_apis::memory::{Decide, MemoryGate, handle};
use kernel_apis::{KernelApis, MemoryApi, MemoryBackend, MemoryService};
use memorantado::{MemorantadoBackend, MemorantadoOptions};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

fn scopes() -> Value {
    json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"], "global": ["ultron:global:g1"]})
}

fn keep_all() -> MemoryGate {
    Arc::new(|request: &Value, _: &Signal| {
        let text = request["text"].as_str().unwrap_or_default();
        Ok(if text.contains("SECRET") {
            json!({"action": "sensitive"})
        } else {
            json!({"action": "keep", "retrieve": true})
        })
    })
}

struct Rig {
    fake: FakeMemorantado,
    root: tempfile::TempDir,
    asked: Arc<Mutex<Vec<(String, String)>>>,
}

impl Rig {
    fn new() -> Rig {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        let fake = FakeMemorantado::new();
        // The server's clock is the service's (dreaming's window is relative to now).
        fake.lock().now = jsrt::date::now_ms();
        Rig {
            fake,
            root,
            asked: Arc::default(),
        }
    }

    fn backend(&self) -> Arc<dyn MemoryBackend> {
        let mut options = MemorantadoOptions::new("http://memo.test", "ultron", scopes().as_object().unwrap().clone());
        options.transport = Some(Arc::new(self.fake.clone()));
        Arc::new(MemorantadoBackend::new(options).unwrap())
    }

    /// A service whose judge answers `verdicts` (by kind) in order.
    fn service(&self, verdicts: Option<Vec<Value>>) -> MemoryService {
        let service = MemoryService::new(Arc::new(MemoryDocumentStore::default()), self.backend(), keep_all())
            .unwrap()
            .with_root(self.root.path());
        match verdicts {
            None => service,
            Some(verdicts) => {
                let (asked, verdicts) = (self.asked.clone(), Arc::new(Mutex::new(verdicts)));
                let decide: Decide = Arc::new(move |state: &str, _: &Value, kind: &str, _: &Signal| {
                    asked.lock().unwrap().push((kind.to_string(), state.to_string()));
                    Ok(verdicts.lock().unwrap().remove(0))
                });
                service.with_judge(decide)
            }
        }
    }

    fn claim(&self, text: &str, scope: &str, metadata: Value, created: &str) -> u64 {
        self.fake
            .seed(json!({"content": text, "tags": scopes()[scope], "metadata": metadata,
            "createdAt": created, "key": format!("k-{text}").replace(' ', "-")}))
    }
}

fn quotes(items: &[(&str, &str)]) -> Value {
    let quotes: Vec<Value> = items.iter().map(|(p, q)| json!({"path": p, "quote": q})).collect();
    Value::Array(quotes).to_string().into()
}

fn verdict(name: &str, value: Value, rung: &str) -> Value {
    json!({"answers": {name: {"value": value, "confidence": 0.9}}, "rung": rung})
}

fn p(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn sig() -> Signal {
    Signal::new()
}

const OLD: &str = "2026-01-01T00:00:00.000Z";

#[test]
fn memorantado_dream_confirms_in_plain_code_and_asks_nothing_when_nothing_changed() {
    let rig = Rig::new();
    let held = rig.claim(
        "alpha exists",
        "project",
        json!({"ultron_quotes": quotes(&[("src/a.rs", "fn alpha()")])}),
        OLD,
    );
    rig.claim("no evidence", "project", json!({}), OLD);
    rig.claim(
        "global rule",
        "global",
        json!({"ultron_quotes": quotes(&[("src/a.rs", "fn zeta()")])}),
        OLD,
    );
    let report = rig.service(Some(Vec::new())).dream(&p(json!({})), &sig()).unwrap();
    assert_eq!(report["confirmed"], json!([held.to_string()]));
    assert_eq!(
        report["unchecked"], 2,
        "no quotes; a global claim's files are elsewhere"
    );
    assert_eq!(report["judged"], 0);
    assert!(
        rig.asked.lock().unwrap().is_empty(),
        "no judge call when nothing changed"
    );
    let confirmed = rig
        .fake
        .lock()
        .memories
        .iter()
        .find(|m| m["id"] == held)
        .cloned()
        .unwrap();
    assert!(confirmed["lastConfirmedAt"].is_string());
    assert_eq!(
        confirmed["evidence"][0]["quote"], "fn alpha()",
        "the file is cited as it is now"
    );
    assert_eq!(rig.fake.lock().snapshots[0]["source"], "repo://src/a.rs");

    // Confirmed claims are not due again in the window.
    let again = rig.service(Some(Vec::new())).dream(&p(json!({})), &sig()).unwrap();
    assert_eq!(again["confirmed"], json!([]));
}

#[test]
fn memorantado_dream_judges_changed_evidence_and_new_claims_beside_old_ones() {
    let rig = Rig::new();
    let moved = rig.claim(
        "beta and gamma exist",
        "project",
        json!({"ultron_quotes": quotes(&[("src/a.rs", "fn beta()"), ("src/a.rs", "fn gamma()")]),
            "ultron_evidence_class": "tool_evidence", "ultron_operation": "op-1"}),
        OLD,
    );
    let gone = rig.claim(
        "c has delta",
        "project",
        json!({"ultron_quotes": quotes(&[("src/c.rs", "fn delta()")])}),
        OLD,
    );
    let unsure = rig.claim(
        "a has eta",
        "project",
        json!({"ultron_quotes": quotes(&[("src/a.rs", "fn eta()")])}),
        OLD,
    );
    let older = rig.claim(
        "use tabs",
        "project",
        json!({"ultron_situation": "formatting rust"}),
        OLD,
    );
    let now = jsrt::date::iso_string(jsrt::date::now_ms());
    let newer = rig.claim(
        "use spaces",
        "project",
        json!({"ultron_situation": "formatting rust"}),
        &now,
    );
    let service = rig.service(Some(vec![
        verdict("status", json!("supported"), "first"),
        verdict("status", json!("stale"), "escalated"),
        verdict("status", json!("contradicted"), "review"),
        verdict("contradict", json!(true), "first"),
    ]));
    let report = service.dream(&p(json!({})), &sig()).unwrap();
    assert_eq!(report["judged"], 4);
    let revised = &report["revised"][0];
    assert_eq!(revised["id"], moved.to_string());
    let by = rig
        .fake
        .lock()
        .memories
        .iter()
        .find(|m| m["id"].as_u64() == revised["by"].as_str().and_then(|by| by.parse().ok()))
        .cloned()
        .unwrap();
    assert_eq!(by["content"], "beta and gamma exist");
    assert_eq!(by["key"], "k-beta-and-gamma-exist", "the key carries over");
    assert_eq!(by["metadata"]["ultron_quotes"], quotes(&[("src/a.rs", "fn beta()")]));
    assert_eq!(
        by["metadata"]["ultron_evidence_class"], "verified",
        "only the host's check marks it"
    );
    assert_eq!(by["metadata"]["ultron_operation"], "op-1");
    assert_eq!(report["retracted"][0]["id"], gone.to_string());
    assert!(
        report["retracted"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("dream: stale")
    );
    let state = |id: u64| {
        rig.fake.lock().memories.iter().find(|m| m["id"] == id).unwrap()["status"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(state(gone), "invalidated");
    assert_eq!(state(unsure), "active", "a verdict for review changes nothing");
    assert_eq!(report["review"][0]["id"], unsure.to_string());
    assert_eq!(
        report["conflicts"],
        json!([{"id": newer.to_string(), "conflictingId": older.to_string()}])
    );
    assert_eq!(rig.fake.lock().conflicts.len(), 1);
    let looked_at = rig
        .fake
        .lock()
        .memories
        .iter()
        .find(|m| m["id"] == newer)
        .cloned()
        .unwrap();
    assert!(
        looked_at["lastConfirmedAt"].is_string(),
        "a settled pair is not asked again"
    );
    let asked = rig.asked.lock().unwrap().clone();
    let kinds: Vec<&str> = asked.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "memory.evidence",
            "memory.evidence",
            "memory.evidence",
            "memory.conflict"
        ]
    );
    assert!(asked[0].1.contains("The file still holds this text") && asked[0].1.contains("The file now reads"));
    assert!(asked[1].1.contains("The file is gone"));
    assert!(asked[3].1.contains("use spaces") && asked[3].1.contains("formatting rust"));
}

#[test]
fn memorantado_dream_without_a_judge_leaves_changes_for_review() {
    let rig = Rig::new();
    let gone = rig.claim(
        "c has delta",
        "project",
        json!({"ultron_quotes": quotes(&[("src/c.rs", "fn delta()")])}),
        OLD,
    );
    let report = rig.service(None).dream(&p(json!({})), &sig()).unwrap();
    assert_eq!(report["review"][0]["id"], gone.to_string());
    assert_eq!(report["judged"], 0);
    assert_eq!(report["retracted"], json!([]));
}

#[test]
fn memorantado_pages_are_the_startup_brief_and_topics_within_budget() {
    let rig = Rig::new();
    let service = rig.service(None);
    assert_eq!(
        service.project_digest(&sig()).unwrap(),
        None,
        "no brief before a dream writes one"
    );
    let written = service
        .write_page(&p(json!({"text": "Use nextest.\nRun fmt first."})), &sig())
        .unwrap();
    assert_eq!(written["scope"], "project");
    let digest = service.project_digest(&sig()).unwrap().unwrap();
    assert_eq!(digest["content"], "Use nextest.\nRun fmt first.");
    assert_eq!(digest["writer"], "dreaming");
    assert_eq!(digest["id"], written["id"]);
    service
        .write_page(&p(json!({"text": "Prefers short answers.", "scope": "global"})), &sig())
        .unwrap();
    assert_eq!(
        service.user_brief(&sig()).unwrap().unwrap()["content"],
        "Prefers short answers."
    );
    service
        .write_page(&p(json!({"text": "How releases go.", "topic": "releases"})), &sig())
        .unwrap();
    assert_eq!(service.topic(&p(json!({})), &sig()).unwrap(), json!(["releases"]));
    assert_eq!(
        service.topic(&p(json!({"topic": "releases"})), &sig()).unwrap()["content"],
        "How releases go."
    );
    assert_eq!(
        service.topic(&p(json!({"topic": "other"})), &sig()).unwrap(),
        Value::Null
    );
    // Pages never come back as recalled facts.
    let recalled = service.recall_for_turn("nextest releases answers", 6, &sig()).unwrap();
    assert!(recalled.is_empty(), "{recalled:?}");

    let long = "line\n".repeat(201);
    assert_eq!(
        handle(Some(&service), "memory.brief", &p(json!({"text": long})), &sig()).unwrap_err(),
        "memory.brief: the brief is over its budget (200 lines, 4000 characters)"
    );
    assert!(handle(Some(&service), "memory.brief", &p(json!({"text": "SECRET x"})), &sig()).is_err());
    assert!(
        handle(
            Some(&service),
            "memory.brief",
            &p(json!({"text": "x", "scope": "session"})),
            &sig()
        )
        .is_err()
    );
}

/// The REPL's `memory.*` (the session's host module), as a cell's request reaches it.
fn repl(service: MemoryService) -> impl Fn(&str, Value) -> Result<Value, String> {
    let refuse: Arc<dyn HostHandler> =
        Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| Err(format!("Unknown host request: {kind}")));
    let apis = KernelApis::new(refuse).with(Arc::new(MemoryApi { service: Some(service) }));
    move |kind: &str, payload: Value| apis.handle(kind, payload.as_object().unwrap().clone(), &Signal::new())
}

#[test]
fn memorantado_write_keeps_file_evidence_and_session_spans_and_refuses_verified_from_a_call() {
    let rig = Rig::new();
    let call = repl(rig.service(None));
    let kept = call(
        "memory.propose",
        json!({"text": "alpha is the entry point", "scope": "project", "evidence": [
            {"ref": "src/a.rs:1", "quote": "fn alpha()"},
            {"ref": "session://s-1#e1..e4"},
        ]}),
    )
    .unwrap();
    assert_eq!(kept["state"], "stored", "memorantado writes are synchronous");
    let memory = rig.fake.active(kept["memoryId"].as_str().unwrap()).unwrap();
    assert_eq!(memory["evidence"][0]["quote"], "fn alpha()");
    assert_eq!(rig.fake.lock().snapshots[0]["source"], "repo://src/a.rs");
    assert_eq!(
        memory["metadata"]["ultron_spans"],
        json!(["session://s-1#e1..e4"]).to_string()
    );
    let got = call("memory.get", json!({"id": kept["memoryId"]})).unwrap();
    assert_eq!(got["content"], "alpha is the entry point");

    let bad_span = call(
        "memory.propose",
        json!({"text": "x", "evidence": [{"ref": "session://"}]}),
    )
    .unwrap_err();
    assert!(bad_span.contains("INVALID_INPUT"), "{bad_span}");
    let verified = call(
        "memory.propose",
        json!({"text": "x", "evidence": [{"ref": "e"}], "evidenceClass": "verified"}),
    )
    .unwrap_err();
    assert!(verified.contains("only by the host"), "{verified}");

    let forgotten = call("memory.forget", json!({"id": kept["memoryId"]})).unwrap();
    assert_eq!(forgotten["state"], "forgotten");
    assert!(rig.fake.lock().memories.is_empty());
}

#[test]
fn memorantado_dream_and_pages_can_name_another_project_directory() {
    let rig = Rig::new();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("README"), "run make check\n").unwrap();
    let path = other.path().display().to_string();
    let tag = format!("ultron:project:{}", kernel_apis::memory::directory_identity(&path));
    let held = rig.fake.seed(
        json!({"content": "make check runs the tests", "tags": [tag], "createdAt": OLD,
        "metadata": {"ultron_quotes": quotes(&[("README", "run make check")])}}),
    );
    let service = rig.service(Some(Vec::new()));
    let own = service.dream(&p(json!({})), &sig()).unwrap();
    assert_eq!(own["confirmed"], json!([]), "not this session's project");
    let report = service.dream(&p(json!({"project": path})), &sig()).unwrap();
    assert_eq!(report["confirmed"], json!([held.to_string()]));
    assert!(service.dream(&p(json!({"project": "relative/dir"})), &sig()).is_err());

    service
        .write_page(&p(json!({"text": "Use make check.", "project": path})), &sig())
        .unwrap();
    assert_eq!(
        service.project_digest(&sig()).unwrap(),
        None,
        "this session's brief is another one"
    );
    let id = format!("ultron-project-{}", kernel_apis::memory::directory_identity(&path));
    assert_eq!(rig.fake.active(&id).unwrap()["content"], "Use make check.");
}
