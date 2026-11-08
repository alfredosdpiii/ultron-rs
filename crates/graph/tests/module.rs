//! `knowledge.*` as the REPL sends it, over a git repository: relations with what waits for the judge, verdicts
//! settled and mirrored, the whole mirror rewritten, and a mirror that fails reported without failing the
//! request.

use crate::fakes::{FakeHttp, git, password, write};
use config::keys::neo4j_settings;
use graph::{KnowledgeModule, Mirrors};
use kernel::Signal;
use kernel_apis::{ApiModule, Request, RootOnly};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const LIB: &str = r#"pub fn home() -> String {
    std::env::var("STORE_HOME").unwrap_or_default()
}

pub fn level(settings: &Settings) -> String {
    settings.get("log.level")
}
"#;

struct World {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    http: Arc<FakeHttp>,
    mirrors: Arc<Mutex<Mirrors>>,
    module: KnowledgeModule,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "src/lib.rs", LIB);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let http = Arc::new(FakeHttp::default());
    let mirrors: Arc<Mutex<Mirrors>> = Arc::default();
    let (source, transport) = (mirrors.clone(), http.clone());
    let module = KnowledgeModule::new(
        dir.path().join("agent/knowledge/graph.db"),
        root.join("src"),
        Arc::new(move || source.lock().unwrap().clone()),
        Arc::new(move || Ok(transport.clone() as Arc<dyn blocking_http::Transport>)),
        Arc::new(|_: &str| Ok(Some(password()))),
        // The session's judge issued one receipt, for d1.
        Arc::new(|receipt: &str| (receipt == "judge:7").then(|| ("d1".to_string(), "d1-2b".to_string()))),
    );
    World {
        _dir: dir,
        root,
        http,
        mirrors,
        module,
    }
}

fn with_neo4j(world: &World) {
    world.mirrors.lock().unwrap().neo4j = neo4j_settings(
        json!({"graph": {"neo4j": {"url": "https://neo4j.test"}}})
            .as_object()
            .unwrap(),
    );
}

fn call(module: &KnowledgeModule, kind: &str, payload: Value) -> Result<Value, String> {
    let host = RootOnly {
        now: kernel_apis::util::system_clock(),
    };
    let payload = payload.as_object().unwrap().clone();
    module.handle(&Request {
        kind,
        payload: &payload,
        lane: "main",
        caller_task: None,
        host: &host,
        signal: &Signal::new(),
    })
}

#[test]
fn knowledge_relations_checks_its_payload() {
    let w = world();
    for (payload, error) in [
        (json!({"extra": 1}), "Unknown payload field: extra"),
        (
            json!({"kinds": ["calls"]}),
            "unknown kind \"calls\" (known: reads_config, serves_route, touches_table, tests)",
        ),
        (json!({"kinds": "tests"}), "kinds must be a list of strings"),
        (
            json!({"paths": ["/etc"]}),
            "paths are relative to the repository root: \"/etc\"",
        ),
        (json!({"paths": ["src/../.."]}), "relative to the repository root"),
    ] {
        let got = call(&w.module, "knowledge.relations", payload.clone()).unwrap_err();
        assert!(got.contains(error), "{payload}: {got}");
    }
    let outside = tempfile::tempdir().unwrap();
    let module = KnowledgeModule::new(
        outside.path().join("graph.db"),
        outside.path().to_path_buf(),
        Arc::new(Mirrors::default),
        Arc::new(|| Err("unused".into())),
        Arc::new(|_: &str| Ok(None)),
        Arc::new(|_: &str| None),
    );
    let error = call(&module, "knowledge.relations", json!({})).unwrap_err();
    assert!(error.starts_with("knowledge needs a git repository"), "{error}");
}

#[test]
fn knowledge_relations_answers_settled_and_pending_and_settles_verdicts() {
    let w = world();
    let found = call(&w.module, "knowledge.relations", json!({"paths": null, "kinds": null})).unwrap();
    let rev = git(&w.root, &["rev-parse", "HEAD"]);
    let top = w.root.canonicalize().unwrap();
    assert_eq!(found["repo"], format!("local:{}", top.display()));
    assert_eq!(
        (found["rev"].as_str(), found["files"].clone(), found["read"].clone()),
        (Some(rev.as_str()), json!(1), json!(1))
    );
    assert_eq!(found["mirror"], Value::Null);
    let relations = found["relations"].as_array().unwrap();
    assert_eq!(relations.len(), 2);
    assert_eq!(
        relations[0],
        json!({"id": relations[0]["id"], "kind": "reads_config", "subject": "src/lib.rs::home", "object": "STORE_HOME",
            "path": "src/lib.rs", "line": 2, "evidence": "repo://src/lib.rs#L2",
            "quote": "std::env::var(\"STORE_HOME\").unwrap_or_default()", "status": "accepted", "checked_by": "code",
            "rung": null, "model": null, "confidence": null})
    );
    let pending = found["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["object"], "log.level");
    assert!(
        pending[0]["context"]
            .as_str()
            .unwrap()
            .contains("settings.get(\"log.level\")")
    );
    assert!(
        pending[0]["question"]
            .as_str()
            .unwrap()
            .starts_with("Does `src/lib.rs::level` read `log.level`")
    );
    // Nothing is sent without a mirror.
    assert!(w.http.seen().is_empty());

    with_neo4j(&w);
    let id = pending[0]["id"].clone();
    // A verdict the model only says a judge gave (no receipt, or one this session's judge never issued for that
    // backend) is kept as the model's.
    for receipt in [json!(null), json!("judge:99"), json!("judge:7")] {
        let backend = if receipt == "judge:7" { "jev" } else { "d1" };
        let claimed = call(
            &w.module,
            "knowledge.settle",
            json!({"rev": rev, "verdicts": [{"id": id, "status": "review", "backend": backend, "rung": "first",
                "model": "m", "confidence": 0.91, "receipt": receipt}]}),
        )
        .unwrap();
        assert_eq!(claimed["settled"], 1);
        let level =
            &call(&w.module, "knowledge.relations", json!({"kinds": ["reads_config"]})).unwrap()["relations"][1];
        assert_eq!(
            (
                level["checked_by"].clone(),
                level["rung"].clone(),
                level["model"].clone()
            ),
            (json!("model"), json!(null), json!(null)),
            "{receipt}"
        );
    }
    let settled = call(
        &w.module,
        "knowledge.settle",
        json!({"rev": rev, "verdicts": [{"id": id, "status": "accepted", "backend": "d1", "rung": "first",
            "model": "d1-2b", "confidence": 0.91, "receipt": "judge:7"}]}),
    )
    .unwrap();
    assert_eq!(settled, json!({"settled": 1, "mirror": {"mirrored": 1}}));
    let statements = w.http.statements();
    assert!(
        statements
            .iter()
            .any(|(s, p)| s.contains("READS_CONFIG") && p["rows"][0]["object"] == "log.level")
    );
    let again = call(&w.module, "knowledge.relations", json!({"kinds": ["reads_config"]})).unwrap();
    assert_eq!(again["read"], 0);
    assert!(again["pending"].as_array().unwrap().is_empty());
    let level = &again["relations"][1];
    assert_eq!(
        (level["status"].clone(), level["checked_by"].clone()),
        (json!("accepted"), json!("d1"))
    );
    assert_eq!(again["mirror"], json!({"mirrored": 2}));
    for (payload, error) in [
        (json!({"verdicts": []}), "rev must be the commit"),
        (
            json!({"rev": rev, "verdicts": [{"id": 1, "status": "maybe"}]}),
            "verdicts are {id, status",
        ),
        (
            json!({"rev": "0".repeat(40), "verdicts": []}),
            "no relations were read at",
        ),
    ] {
        let got = call(&w.module, "knowledge.settle", payload.clone()).unwrap_err();
        assert!(got.contains(error), "{payload}: {got}");
    }
}

#[test]
fn knowledge_a_mirror_that_fails_is_reported_and_the_request_succeeds() {
    let w = world();
    with_neo4j(&w);
    *w.http.down.lock().unwrap() = true;
    let found = call(&w.module, "knowledge.relations", json!({})).unwrap();
    assert_eq!(
        found["mirror"],
        json!({"error": "Neo4j at https://neo4j.test could not be reached"})
    );
    assert_eq!(found["relations"].as_array().unwrap().len(), 2);
}

#[test]
fn knowledge_mirror_rewrites_relations_and_memories() {
    let w = world();
    let error = call(&w.module, "knowledge.mirror", json!({})).unwrap_err();
    assert_eq!(
        error,
        "knowledge.mirror: graph.neo4j.url is not set, so there is no mirror"
    );
    with_neo4j(&w);
    // Nothing read yet at HEAD: the repository's mirror is emptied, nothing else.
    let empty = call(&w.module, "knowledge.mirror", json!({})).unwrap();
    assert_eq!(empty, json!({"relations": 0, "errors": []}));
    call(&w.module, "knowledge.relations", json!({"paths": ["src"]})).unwrap();
    w.mirrors.lock().unwrap().memorantado = Some(memorantado::Endpoint {
        url: "http://mem.test".into(),
        project: "ultron".into(),
    });
    *w.http.memories.lock().unwrap() = vec![json!({"id": 7, "status": "active", "kind": "gotcha", "subject": "s",
        "content": "c", "tags": []})];
    let before = w.http.seen().len();
    let mirrored = call(&w.module, "knowledge.mirror", json!({})).unwrap();
    assert_eq!(mirrored, json!({"relations": 1, "memories": 1, "errors": []}));
    let sent: Vec<String> = w.http.seen()[before..].iter().map(|s| s.url.clone()).collect();
    assert_eq!(sent.iter().filter(|u| u.starts_with("http://mem.test/api/")).count(), 4);
    // A memory without a numeric id is passed over.
    *w.http.memories.lock().unwrap() = vec![json!({"id": "not a number"})];
    let partial = call(&w.module, "knowledge.mirror", json!({})).unwrap();
    assert_eq!(
        (partial["relations"].clone(), partial["memories"].clone()),
        (json!(1), json!(0))
    );
}
