//! Automatic instruction refinements (session_core::auto_refine; Rust-only, no TypeScript reference): a session
//! actor against a scripted provider on loopback. After a compaction, the side request's proposal whose quote is in
//! the summary is activated in the session's refinements ledger and opens the next user run as an
//! `ultron.refinements` message; one whose quote is not is never stored; with `refine.auto` off neither happens.

use crate::p_session_support;

use p_session_support::{HANG, Setup, actor, text, user_texts};
use serde_json::{Value, json};
use session_core::core::Op;
use std::time::{Duration, Instant};

const SUMMARY: &str = "## Goal\nShip the parser.\n\n## Progress\nThe user corrected the agent: always run scripts/nextest.sh, never cargo test.";
const REAL: &str = "Run tests with scripts/nextest.sh only.";
const FAKE: &str = "Never touch the database migrations.";

fn system(body: &Value) -> String {
    body["system"].to_string()
}

/// Summary requests get [`SUMMARY`]; the refinement request gets one proposal quoting it and one inventing a quote.
fn script() -> p_session_support::Script {
    Box::new(|body: &Value, index: usize| {
        if system(body).contains("summarization") {
            text(SUMMARY)
        } else if system(body).contains("propose small, durable instructions") {
            let proposal = |text: &str, quote: &str| {
                json!({"text": text, "quote": quote, "rationale": "the user corrected it",
                    "expectedOutcome": "no wasted test runs"})
            };
            text(
                &json!({"proposals": [
                    proposal(REAL, "always run scripts/nextest.sh, never cargo test"),
                    proposal(FAKE, "the agent broke the database migrations"),
                ], "rollback": []})
                .to_string(),
            )
        } else {
            text(&format!("Answer {index}."))
        }
    })
}

/// The refinement events the session's ledger holds (its keyed document in the journal), newest document.
fn ledger_events(a: &p_session_support::Actor) -> Vec<Value> {
    a.writes()
        .into_iter()
        .rfind(|w| w["op"] == "set" && w["value"]["formatVersion"] == 1 && w["value"]["events"].is_array())
        .map(|w| w["value"]["events"].as_array().unwrap().clone())
        .unwrap_or_default()
}

fn compact_then_prompt(settings: Option<Value>, wait_for_activation: bool) -> (p_session_support::Actor, Vec<Value>) {
    let a = actor(
        script(),
        Setup {
            settings,
            vars: vec![(session_core::lane::CLOCK_CONTEXT_ENV, "off".into())],
            ..Setup::default()
        },
    );
    assert_eq!(a.prompt("start the parser work")["error"], Value::Null);
    a.call(Op::Compact { instructions: None }).unwrap();
    if wait_for_activation {
        // The pass runs in the background: wait for its activation (a hang guard, never a pass condition).
        let end = Instant::now() + HANG;
        while !ledger_events(&a).iter().any(|e| e["action"] == "activate") {
            assert!(Instant::now() < end, "no refinement was activated");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(a.prompt("now finish the parser please")["error"], Value::Null);
    let requests = a.requests();
    (a, requests)
}

fn conversation(requests: &[Value]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|r| !system(r).contains("summarization") && !system(r).contains("propose small, durable"))
        .collect()
}

#[test]
fn session_core_auto_refine_activates_a_quoted_proposal_and_delivers_it_to_the_main_lane() {
    let (a, requests) = compact_then_prompt(None, true);
    let events = ledger_events(&a);
    let proposed: Vec<&Value> = events.iter().filter(|e| e["action"] == "propose").collect();
    assert_eq!(proposed.len(), 1, "the invented quote is not stored: {events:#?}");
    let proposal = &proposed[0]["proposal"];
    assert_eq!(proposal["kind"], "instruction");
    assert!(proposal["target"].as_str().unwrap().starts_with("instruction:main."));
    assert_eq!(proposal["content"], REAL);
    assert_eq!(
        proposal["evidence"][0]["quote"],
        "always run scripts/nextest.sh, never cargo test"
    );
    assert!(
        proposal["evidence"][0]["ref"]
            .as_str()
            .unwrap()
            .starts_with("compaction:")
    );
    let id = proposed[0]["id"].clone();
    assert!(events.iter().any(|e| e["action"] == "activate" && e["id"] == id));
    let last = conversation(&requests).last().copied().unwrap().clone();
    let texts: Vec<String> = user_texts(&last)
        .iter()
        .map(|t| t.as_str().unwrap_or_default().to_string())
        .collect();
    let delivered = texts
        .iter()
        .find(|t| t.starts_with("[refinements]"))
        .expect("no refinements message");
    assert!(delivered.contains(REAL), "{delivered}");
    assert!(delivered.contains(id.as_str().unwrap()), "{delivered}");
    assert!(!texts.iter().any(|t| t.contains(FAKE)));
    // The first run had nothing to show.
    let first = conversation(&requests)[0].clone();
    assert!(!first["messages"].to_string().contains("[refinements]"));
}

#[test]
fn session_core_auto_refine_off_runs_the_unmodified_harness() {
    let (a, requests) = compact_then_prompt(Some(json!({"refine": {"auto": false}})), false);
    assert!(
        !requests.iter().any(|r| system(r).contains("propose small, durable")),
        "no refinement request"
    );
    assert!(ledger_events(&a).is_empty());
    assert!(
        !requests
            .iter()
            .any(|r| r["messages"].to_string().contains("[refinements]"))
    );
}

#[test]
fn session_core_auto_refine_keyed_superseding_recency_limit_and_logging() {
    use kernel_apis::artifacts::MemoryKeyedDocuments;
    use services::LocalServices;
    use session_core::auto_refine::{Ledger, MESSAGE_LIMIT, message, propose_sync};
    use std::sync::Arc;

    let services = Arc::new(LocalServices::worker(Box::new(MemoryKeyedDocuments::default())));
    let ledger = Ledger {
        services: services.clone(),
        branch: None,
        memory: None,
    };
    let dir = tempfile::tempdir().unwrap();

    // First pass proposes under semantic key `test-runner` (v1).
    let out1 = propose_sync(&ledger, "compaction:c1", SUMMARY, Some(dir.path()), |_, _| {
        Ok(json!({
            "proposals": [{
                "key": "test-runner",
                "text": "Run tests with scripts/nextest.sh.",
                "quote": "always run scripts/nextest.sh, never cargo test",
                "rationale": "user correction",
                "expectedOutcome": "faster tests",
            }],
            "rollback": [],
            "used": [],
        })
        .to_string())
    });
    assert_eq!(out1.proposed, 1);
    assert_eq!(ledger.active().len(), 1);
    assert_eq!(ledger.active()[0]["version"], 1);
    assert_eq!(ledger.active()[0]["target"], "instruction:main.test-runner");

    // Second pass with the same key supersedes v1 -> v2 instead of colliding.
    let out2 = propose_sync(&ledger, "compaction:c2", SUMMARY, Some(dir.path()), |_, _| {
        Ok(json!({
            "proposals": [{
                "key": "test-runner",
                "text": "Always run scripts/nextest.sh --locked.",
                "quote": "always run scripts/nextest.sh, never cargo test",
                "rationale": "refined command",
                "expectedOutcome": "locked runs",
            }],
            "rollback": [],
            "used": [],
        })
        .to_string())
    });
    assert_eq!(out2.proposed, 1);
    let active = ledger.active();
    assert_eq!(active.len(), 1, "superseded instead of duplicating");
    assert_eq!(active[0]["version"], 2);
    assert_eq!(active[0]["text"], "Always run scripts/nextest.sh --locked.");

    // Adding more than MESSAGE_LIMIT refinements delivers the NEWEST ones (not oldest).
    for i in 0..(MESSAGE_LIMIT + 3) {
        let p = services
            .handle(
                "refinements.propose",
                &json!({
                    "kind": "instruction",
                    "target": format!("instruction:main.extra-{i}"),
                    "baseVersion": 0,
                    "content": format!("Rule {i}"),
                    "evidence": [{"ref": "compaction:c3"}],
                }),
                None,
            )
            .unwrap();
        services
            .handle("refinements.activate", &json!({"id": p["id"]}), None)
            .unwrap();
    }
    let active = ledger.active();
    let msg = message(&active, None, 100).unwrap();
    let content = msg["content"].as_str().unwrap();
    assert!(
        content.contains(&format!("Rule {}", MESSAGE_LIMIT + 2)),
        "newest refinement must be delivered: {content}"
    );
    assert!(
        !content.contains("Rule 0"),
        "oldest refinements beyond MESSAGE_LIMIT are trimmed first: {content}"
    );

    // Log file recorded both passes.
    let log = std::fs::read_to_string(dir.path().join("memory/auto-refine.log")).unwrap();
    assert!(log.contains("source=compaction:c1 proposed=1"));
    assert!(log.contains("source=compaction:c2 proposed=1"));
}

#[test]
fn session_core_auto_refine_promotes_to_memorantado_confirms_rolls_back_and_loads_skills() {
    use kernel::Signal;
    use kernel::store::MemoryDocumentStore;
    use kernel_apis::artifacts::MemoryKeyedDocuments;
    use kernel_apis::{MemoryBackend, MemoryService, worker_gate};
    use serde_json::Map;
    use services::LocalServices;
    use session_core::auto_refine::{Ledger, message, propose_sync};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeMem {
        claims: Mutex<Vec<Value>>,
        confirmed: Mutex<Vec<String>>,
        next_id: Mutex<u64>,
    }

    impl MemoryBackend for FakeMem {
        fn namespace(&self) -> String {
            "fake-mem".into()
        }
        fn scope_tags(&self) -> Map<String, Value> {
            json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"]})
                .as_object()
                .cloned()
                .unwrap()
        }
        fn supports(&self, call: &str) -> bool {
            matches!(call, "recall" | "retain" | "get" | "delete" | "fileEvidence" | "claims")
        }
        fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
            Ok(json!({"results": []}))
        }
        fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
            let mut claims = self.claims.lock().unwrap();
            let mut next = self.next_id.lock().unwrap();
            for item in request["items"].as_array().into_iter().flatten() {
                let key = item["document_id"].as_str().unwrap_or_default().to_string();
                for existing in claims.iter_mut() {
                    if existing["key"] == key.as_str() && existing["status"] == "active" {
                        existing["status"] = "superseded".into();
                    }
                }
                *next += 1;
                claims.push(json!({
                    "id": next.to_string(),
                    "key": key,
                    "kind": "refinement",
                    "text": item["content"],
                    "tags": item["tags"],
                    "metadata": item["metadata"],
                    "status": "active",
                    "createdAt": item["timestamp"],
                    "lastConfirmedAt": Value::Null,
                }));
            }
            Ok(json!({"success": true, "async": false}))
        }
        fn get(&self, _: &str, _: &Signal) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn delete(&self, _: &str, _: &Signal) -> Result<Value, String> {
            Ok(json!({"success": true}))
        }
        fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
            Ok(json!({"status": "completed"}))
        }
        fn list(&self, request: &Value, _: &Signal) -> Result<Value, String> {
            let want_tags = &request["tags"];
            let want_status = request["status"].as_str();
            let claims: Vec<Value> = self
                .claims
                .lock()
                .unwrap()
                .iter()
                .filter(|c| &c["tags"] == want_tags && want_status.is_none_or(|s| c["status"] == s))
                .cloned()
                .collect();
            Ok(json!({"claims": claims}))
        }
        fn curate(&self, unit_id: &str, request: &Value, _: &Signal) -> Result<Value, String> {
            let mut claims = self.claims.lock().unwrap();
            let claim = claims.iter_mut().find(|c| c["id"] == unit_id).ok_or("UNKNOWN_MEMORY")?;
            claim["status"] = request["state"].clone();
            Ok(json!({"id": unit_id, "state": request["state"]}))
        }
        fn confirm(&self, id: &str, _: &Value, _: &Signal) -> Result<Value, String> {
            self.confirmed.lock().unwrap().push(id.to_string());
            Ok(json!({"id": id}))
        }
    }

    let backend = Arc::new(FakeMem::default());
    let memory =
        Arc::new(MemoryService::new(Arc::new(MemoryDocumentStore::default()), backend.clone(), worker_gate()).unwrap());

    // Session 1 proposes one always-on durable refinement and one situational (pathGlobs) durable refinement.
    let s1 = Ledger {
        services: Arc::new(LocalServices::worker(Box::new(MemoryKeyedDocuments::default()))),
        branch: None,
        memory: Some(memory.clone()),
    };
    let out = propose_sync(&s1, "tail:t1", SUMMARY, None, |_, _| {
        Ok(json!({
            "proposals": [
                {
                    "key": "test-runner",
                    "text": "Always run scripts/nextest.sh.",
                    "quote": "always run scripts/nextest.sh, never cargo test",
                    "rationale": "user correction",
                    "expectedOutcome": "consistent tests",
                    "durable": true,
                    "situation": "running tests",
                },
                {
                    "key": "parser-only",
                    "text": "Run parser fuzzing on parser edits.",
                    "quote": "Ship the parser.",
                    "rationale": "parser invariant",
                    "expectedOutcome": "no parser regressions",
                    "durable": true,
                    "pathGlobs": ["crates/parser/**/*.rs"],
                }
            ],
            "rollback": [],
            "used": [],
        })
        .to_string())
    });
    assert_eq!(out.proposed, 2);
    assert_eq!(out.promoted, 2);

    // Session 2 (fresh empty local ledger) sees ONLY the always-on project refinement (not the pathGlobs one).
    let s2 = Ledger {
        services: Arc::new(LocalServices::worker(Box::new(MemoryKeyedDocuments::default()))),
        branch: None,
        memory: Some(memory.clone()),
    };
    let active = s2.active();
    assert_eq!(
        active.len(),
        1,
        "situational pathGlobs refinement excluded from always-on: {active:#?}"
    );
    assert_eq!(active[0]["id"], "project:1");
    assert_eq!(active[0]["scope"], "project");
    let msg = message(&active, None, 10).unwrap();
    assert!(
        msg["content"]
            .as_str()
            .unwrap()
            .contains("[project:1 project] Always run scripts/nextest.sh.")
    );
    // `prompt_block` formats active refinements for fresh `rlm.spawn` subagents.
    let block = session_core::auto_refine::prompt_block(&active).unwrap();
    assert!(block.contains("[project:1] Always run scripts/nextest.sh."));

    // `RefinementPointers` surfaces situational `pathGlobs` refinements when a matching file is edited (once).
    {
        use host::file_hooks::{CellChanges, CellObserver};
        use session_core::auto_refine::RefinementPointers;
        let cwd = std::path::Path::new("/repo");
        let observer = RefinementPointers::new(s2.clone(), true);
        assert!(observer.enabled());
        let changes = CellChanges {
            files: vec![cwd.join("crates/parser/src/lib.rs")],
            checked: Vec::new(),
            deleted: Vec::new(),
            complete: true,
        };
        let notes = observer.cell_changed(&changes, cwd, "main");
        assert_eq!(
            notes.len(),
            1,
            "situational refinement emitted on matching file: {notes:?}"
        );
        assert!(notes[0].contains("[refinement] project:2: Run parser fuzzing on parser edits."));
        // Second edit to a matching file does not repeat the same refinement in the same session.
        assert!(observer.cell_changed(&changes, cwd, "main").is_empty());
    }

    // Confirming `project:1` records confirmation in memorantado; rolling it back retires it across sessions.
    let out2 = propose_sync(&s2, "compaction:c1", SUMMARY, None, |_, _| {
        Ok(json!({"proposals": [], "rollback": [], "used": ["project:1"]}).to_string())
    });
    assert_eq!(out2.confirmed, 1);
    assert_eq!(*backend.confirmed.lock().unwrap(), vec!["1".to_string()]);

    let out3 = propose_sync(&s2, "compaction:c2", SUMMARY, None, |_, _| {
        Ok(json!({"proposals": [], "rollback": ["project:1"], "used": []}).to_string())
    });
    assert_eq!(out3.rolled_back, 1);
    assert!(s2.active().is_empty(), "retired project refinement is gone");

    // Active skill refinements are surfaced by `active_skills()`.
    let skill_md =
        "---\nname: nextest-runner\ndescription: Run workspace tests safely\n---\nAlways use scripts/nextest.sh.";
    let p = s2
        .services
        .handle(
            "refinements.propose",
            &json!({
                "kind": "skill",
                "target": "skill:nextest-runner",
                "baseVersion": 0,
                "content": skill_md,
                "evidence": [{"ref": "compaction:c2"}],
            }),
            None,
        )
        .unwrap();
    s2.services
        .handle("refinements.activate", &json!({"id": p["id"]}), None)
        .unwrap();
    let skills = s2.active_skills();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].0, "skill:nextest-runner");
}
