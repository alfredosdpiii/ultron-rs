//! Whether a completion may re-invoke a root (session-worker.ts, the async-event dispatcher's `refuse`): a spent
//! turn/token/cost limit or a cost cap that is reached or cannot be
//! enforced keeps the root quiet. The ledger's clock is injected: time moves only when the test moves it.

use delegation::usage::{ReserveRequest, UsageLedger, UsageLimits, UsageOptions};
use kernel::store::MemoryDocumentStore;
use std::sync::{Arc, Mutex};

fn ledger(limits: UsageLimits, now: Arc<Mutex<f64>>) -> UsageLedger {
    let clock = now;
    UsageLedger::new(
        Box::new(MemoryDocumentStore::default()),
        UsageOptions {
            limits,
            now: Arc::new(move || *clock.lock().unwrap()),
            ..UsageOptions::default()
        },
    )
    .unwrap()
}

fn reserve(ledger: &UsageLedger, root: &str, key: &str) {
    ledger
        .reserve(&ReserveRequest {
            root_id: Some(root.into()),
            ..ReserveRequest::task(key)
        })
        .unwrap();
}

#[test]
fn delegation_usage_reinvoke_is_allowed_after_the_wall_budget_is_used_up() {
    let now = Arc::new(Mutex::new(1_000.0));
    let limits = UsageLimits {
        max_wall_ms: Some(500),
        ..UsageLimits::default()
    };
    let ledger = ledger(limits, now.clone());
    reserve(&ledger, "turn:a", "k1");
    *now.lock().unwrap() = 1_500.0;
    assert_eq!(ledger.status(Some("turn:a")).unwrap()["remainingWallMs"], 0);
    assert_eq!(ledger.reinvoke_refusal("turn:a").unwrap(), None);
    // The deadline still refuses new work.
    let refused = ledger.reserve(&ReserveRequest {
        root_id: Some("turn:a".into()),
        ..ReserveRequest::task("k2")
    });
    assert!(refused.unwrap_err().contains("wall deadline exceeded"));
}

#[test]
fn delegation_usage_reinvoke_reports_a_spent_tree_before_the_wall() {
    let now = Arc::new(Mutex::new(0.0));
    let limits = UsageLimits {
        max_wall_ms: Some(10),
        max_total_turns: Some(1),
        ..UsageLimits::default()
    };
    let ledger = ledger(limits, now.clone());
    reserve(&ledger, "turn:a", "k1");
    ledger.record_turn(Some("turn:a"), Some(5.0), Some(0.0)).unwrap();
    *now.lock().unwrap() = 100.0;
    assert_eq!(
        ledger.reinvoke_refusal("turn:a").unwrap().as_deref(),
        Some(
            "Usage turn limit reached for root turn:a: 1 of 1 model turns used (max_total_turns, ULTRON_MAX_TOTAL_TURNS). No further model work is admitted for this root; answer with what you have."
        )
    );
}

#[test]
fn delegation_usage_reinvoke_is_refused_at_a_cost_cap_that_cannot_be_enforced() {
    let now = Arc::new(Mutex::new(0.0));
    let limits = UsageLimits {
        max_cost_usd: Some(1.0),
        ..UsageLimits::default()
    };
    let ledger = ledger(limits, now);
    ledger.record_turn(Some("turn:a"), Some(5.0), Some(0.25)).unwrap();
    assert_eq!(
        ledger.reinvoke_refusal("turn:a").unwrap(),
        None,
        "a quarter of the cap is spent"
    );
    // A response without a reported cost leaves the cap unenforceable.
    ledger.record_turn(Some("turn:a"), Some(5.0), None).unwrap();
    assert!(ledger.reinvoke_refusal("turn:a").unwrap().unwrap().starts_with(
        "Usage pricing unknown; cannot enforce cost cap of $1 for root turn:a (1 model call(s) reported no cost)."
    ));
}
