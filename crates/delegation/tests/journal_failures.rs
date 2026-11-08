//! A task journal that refuses a write (B-report #3, #4; a refused write poisons the journal until it is reopened):
//! an `rlm.spawn` child whose start is refused is never listed as running with nothing to run it; a child whose end
//! is refused is not published as an error while the journal still says running (a restart reattaches it and
//! delivers its reply: the parent would see two results) until the host closes; a typed task whose end is refused
//! still settles its usage reservation.

use delegation::agents::{Agents, AgentsOptions};
use delegation::host::{ChildRunner, Delegation, DelegationOptions};
use delegation::usage::{UsageLedger, UsageLimits};
use kernel::store::DocumentStore;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Refuse = Box<dyn Fn(&Value) -> bool + Send>;

/// An in-memory document that refuses the first write `refuse` matches, once.
#[derive(Clone, Default)]
struct Store {
    value: Arc<Mutex<Option<Value>>>,
    refuse: Arc<Mutex<Option<Refuse>>>,
}

impl Store {
    fn refusing(refuse: impl Fn(&Value) -> bool + Send + 'static) -> Store {
        let store = Store::default();
        *store.refuse.lock().unwrap() = Some(Box::new(refuse));
        store
    }
}

impl DocumentStore for Store {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.value.lock().unwrap().clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        let mut refuse = self.refuse.lock().unwrap();
        if refuse.as_ref().is_some_and(|refuse| refuse(document)) {
            *refuse = None;
            return Err("refused".into());
        }
        *self.value.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

/// Whether the document holds a task in `state`.
fn holds(document: &Value, state: &str) -> bool {
    document.to_string().contains(&format!("\"state\":\"{state}\""))
}

fn delegation(store: Store, runner: ChildRunner) -> Delegation {
    let mut options = DelegationOptions::new(runner);
    options.store = Box::new(store);
    Delegation::new(options).0
}

#[test]
fn delegation_a_child_whose_start_is_refused_is_not_left_running() {
    let ran = Arc::new(Mutex::new(0));
    let counted = ran.clone();
    let host = delegation(
        Store::refusing(|document| holds(document, "running")),
        Arc::new(move |_| {
            *counted.lock().unwrap() += 1;
            Ok("done".into())
        }),
    );
    host.handle(
        "rlm.spawn",
        json!({"prompt": "brief", "kwargs": {"name": "one"}}),
        "main",
    )
    .unwrap_err();
    assert_eq!(host.running("main"), Vec::<String>::new());
    host.close();
    assert_eq!(*ran.lock().unwrap(), 0);
}

#[test]
fn delegation_a_child_whose_end_is_refused_is_not_published_as_an_error() {
    let store = Store::refusing(|document| holds(document, "completed"));
    let host = delegation(store.clone(), Arc::new(|_| Ok("done".into())));
    let spawned = host
        .handle(
            "rlm.spawn",
            json!({"prompt": "brief", "kwargs": {"name": "one"}}),
            "main",
        )
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
    while store.refuse.lock().unwrap().is_some() {
        assert!(Instant::now() < deadline, "hang guard: the child's end");
        std::thread::sleep(Duration::from_millis(2));
    }
    // A later end (here a cancel) cannot write the poisoned journal either: still nothing is published.
    host.cancel(&id, "stop").unwrap_err();
    assert_eq!(host.wait(std::slice::from_ref(&id), Some(Duration::ZERO)), Vec::new());
    assert_eq!(host.running("main"), std::slice::from_ref(&id));
    // Closing publishes the error: this worker is going away, and the next one settles the child.
    host.close();
    let ended = host.wait(std::slice::from_ref(&id), Some(Duration::ZERO));
    assert!(matches!(ended.as_slice(), [(_, Err(_))]), "{ended:?}");
}

#[test]
fn delegation_a_typed_task_whose_end_is_refused_settles_its_usage() {
    let usage = UsageLedger::in_memory(UsageLimits::default());
    let mut options = AgentsOptions::new(Box::new(Store::refusing(|document| holds(document, "completed"))));
    options.definition_store = Some(Box::new(Store::default()));
    options.usage = Some(usage.clone());
    options.deterministic = Some(Arc::new(|request: delegation::agents::AdapterRequest<'_>| {
        Ok(request.input.clone())
    }));
    let (agents, _) = Agents::new(options);
    let definition = json!({
        "id": "answer", "version": "1", "strategy": "deterministic", "instructions": "Run answer.",
        "inputSchema": {}, "outputSchema": {}, "maxRepairs": 0, "inputDescription": "any",
        "outputDescription": "any",
    });
    agents
        .handle("agents.register", json!({"definition": definition}), "main", None)
        .unwrap();
    agents
        .handle(
            "agents.invoke",
            json!({"definition": "answer@1", "input": {"a": 1}}),
            "main",
            None,
        )
        .unwrap_err();
    let ledger = usage.document().unwrap();
    let open: Vec<&Value> = ledger["roots"]
        .as_object()
        .map(|roots| {
            roots
                .values()
                .flat_map(|root| root["reservations"].as_array().into_iter().flatten())
                .collect()
        })
        .unwrap_or_default();
    assert!(open.is_empty(), "open reservations: {ledger}");
}
