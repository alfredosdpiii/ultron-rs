//! Per-kind call ceilings (`rootBudget.maxCalls`, `ULTRON_MAX_TASK_CALLS`, `ULTRON_MAX_MODEL_CALLS`) and the cost
//! stop-line (a unit whose estimate would cross `max_cost_usd` is refused before it starts, with a receipt). New in
//! Rust: no TypeScript counterpart; literal expectations.

use delegation::usage::{
    MAX_MODEL_CALLS_ENV, MAX_TASK_CALLS_ENV, ReserveRequest, UsageLedger, UsageLimits, UsageOptions, limits_from_env,
};
use kernel::store::MemoryDocumentStore;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

fn ledger(limits: UsageLimits) -> (UsageLedger, Arc<Mutex<Vec<String>>>) {
    let refused: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = refused.clone();
    let ledger = UsageLedger::new(
        Box::new(MemoryDocumentStore::default()),
        UsageOptions {
            limits,
            on_refused: Some(Arc::new(move |reason: &str| sink.lock().unwrap().push(reason.into()))),
            ..UsageOptions::default()
        },
    )
    .unwrap();
    (ledger, refused)
}

fn ceilings(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
}

#[test]
fn delegation_usage_call_ceilings_come_from_settings_and_the_environment_wins() {
    let read = |vars: &[(&str, &str)], settings: Value| {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        limits_from_env(&|key| vars.get(key).cloned(), &settings).max_calls
    };
    assert!(read(&[], json!({})).is_empty(), "no ceiling unless one is set");
    assert_eq!(
        read(&[], json!({"maxCalls": {"task": 40, "model": 200}})),
        ceilings(&[("task", 40), ("model", 200)])
    );
    assert_eq!(
        read(&[(MAX_TASK_CALLS_ENV, "5")], json!({"maxCalls": {"task": 40}})),
        ceilings(&[("task", 5)]),
        "the variable wins over the setting"
    );
    assert_eq!(
        read(
            &[(MAX_TASK_CALLS_ENV, "off"), (MAX_MODEL_CALLS_ENV, "7")],
            json!({"maxCalls": {"task": 40}})
        ),
        ceilings(&[("model", 7)]),
        "off removes a set ceiling"
    );
    assert_eq!(
        read(
            &[(MAX_MODEL_CALLS_ENV, "zero")],
            json!({"maxCalls": {"task": 0, "model": 2.5, "jev": 3, "other": 1}})
        ),
        BTreeMap::new(),
        "only positive whole ceilings of reservable kinds count; a bad variable falls back to the setting"
    );
}

#[test]
fn delegation_usage_call_ceilings_refuse_the_call_past_the_ceiling_with_its_reason() {
    let (ledger, refused) = ledger(UsageLimits {
        max_calls: ceilings(&[("task", 2), ("model", 1)]),
        ..UsageLimits::default()
    });
    let first = ledger.reserve(&ReserveRequest::task("a")).unwrap();
    ledger.settle(&first, Some("succeeded"), None).unwrap();
    ledger.reserve(&ReserveRequest::task("b")).unwrap();
    // Settled and open calls both count.
    let error = ledger.reserve(&ReserveRequest::task("c")).unwrap_err();
    assert_eq!(
        error,
        "Usage call ceiling reached for root ultron-root: 2 of 2 task calls admitted (rootBudget.maxCalls.task, ULTRON_MAX_TASK_CALLS). No further task calls are admitted for this root; answer with what you have."
    );
    // A known request key still returns its reservation; another kind has its own ceiling.
    ledger.reserve(&ReserveRequest::task("b")).unwrap();
    ledger.reserve(&ReserveRequest::model("m1")).unwrap();
    let error = ledger.reserve(&ReserveRequest::model("m2")).unwrap_err();
    assert!(error.contains("1 of 1 model calls"), "{error}");
    assert!(error.contains("ULTRON_MAX_MODEL_CALLS"), "{error}");
    // Another root has its own count.
    ledger
        .reserve(&ReserveRequest {
            root_id: Some("turn:other".into()),
            ..ReserveRequest::task("d")
        })
        .unwrap();
    assert_eq!(refused.lock().unwrap().len(), 2, "each refusal is reported");
    let status = ledger.status(None).unwrap();
    assert_eq!(status["limits"]["maxCalls"], json!({"model": 1, "task": 2}));
    assert_eq!(status["calls"], json!({"task": 2, "model": 1}));
}

#[test]
fn delegation_usage_status_has_no_call_fields_without_ceilings() {
    let (ledger, _) = ledger(UsageLimits::default());
    let status = ledger.status(None).unwrap();
    assert!(status["limits"].get("maxCalls").is_none());
    assert!(status.get("calls").is_none());
}

#[test]
fn delegation_usage_stop_line_refuses_a_unit_whose_estimate_would_cross_the_cost_cap() {
    let (ledger, refused) = ledger(UsageLimits {
        max_cost_usd: Some(1.0),
        ..UsageLimits::default()
    });
    ledger.record_turn(None, Some(100.0), Some(0.7)).unwrap();
    let estimated = |key: &str, cost: f64| ReserveRequest {
        estimated_cost_usd: Some(cost),
        ..ReserveRequest::task(key)
    };
    let error = ledger.reserve(&estimated("big", 0.4)).unwrap_err();
    assert_eq!(
        error,
        "Usage cost stop-line for root ultron-root: this task is estimated at $0.4, and $0.7 of the $1 cap is spent ($0.3 left), so it would cross the cap (ULTRON_MAX_COST_USD); it was not started. Plan smaller work or ask the user to raise the cap."
    );
    assert_eq!(*refused.lock().unwrap(), vec![error]);
    // What fits is admitted, as is a unit without an estimate, and work that calls no model.
    ledger.reserve(&estimated("small", 0.25)).unwrap();
    ledger.reserve(&ReserveRequest::task("unknown")).unwrap();
    ledger
        .reserve(&ReserveRequest {
            model_backed: Some(false),
            ..estimated("deterministic", 5.0)
        })
        .unwrap();
    // The receipt for work admitted elsewhere (a workflow's subagent node) is the same check.
    let line = ledger.stop_line(None, 0.31, "workflow node fix").unwrap().unwrap();
    assert!(line.contains("workflow node fix is estimated at $0.31"), "{line}");
    assert_eq!(ledger.stop_line(None, 0.25, "workflow node fix").unwrap(), None);
    assert_eq!(refused.lock().unwrap().len(), 2);
}

#[test]
fn delegation_usage_stop_line_needs_a_cost_cap() {
    let (ledger, _) = ledger(UsageLimits::default());
    ledger.record_turn(None, Some(100.0), Some(50.0)).unwrap();
    ledger
        .reserve(&ReserveRequest {
            estimated_cost_usd: Some(1_000.0),
            ..ReserveRequest::task("any")
        })
        .unwrap();
    assert_eq!(ledger.stop_line(None, 1_000.0, "node").unwrap(), None);
}
