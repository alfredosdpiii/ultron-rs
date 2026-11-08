//! Replays crates/kernel-apis/fixtures/schedules.json (fixtures/generate-schedules.mjs: the TS schedule module of
//! ultron/schedules.ts behind NativeRlmHost, serving runtime.py through kernel.ts) against `SchedulesModule`, with
//! the same scripted task API, and checks literal cases ported from test/ultron-schedules.test.ts.

use crate::common;

use common::{FIXED_NOW, Made, assert_area, fixture, inner, run_scenario};
use kernel::store::{DocumentStore, MemoryDocumentStore};
use kernel_apis::{KernelApis, SchedulesModule, TaskHost};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

/// generate-schedules.mjs `fakeTasks`.
struct FakeTasks {
    clock: Arc<AtomicI64>,
    tasks: Mutex<Vec<Value>>,
    log: Mutex<Vec<Value>>,
}

impl FakeTasks {
    fn new(clock: Arc<AtomicI64>, preloaded: Vec<Value>) -> Self {
        Self {
            clock,
            tasks: Mutex::new(preloaded),
            log: Mutex::new(Vec::new()),
        }
    }
}

impl TaskHost for FakeTasks {
    fn caller_task_id(&self, _lane: &str) -> Option<String> {
        None
    }
    fn task_lane(&self, _task_id: &str) -> Option<String> {
        None
    }
    fn strategy(&self, definition: &str) -> Result<String, String> {
        Err(format!("Unknown Ultron definition: {definition}"))
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        jsrt::sync::lock(&self.log).push(json!(["tasks"]));
        Ok(jsrt::sync::lock(&self.tasks).clone())
    }
    fn spawn(&self, request: &Value, parent: Option<&str>) -> Result<Value, String> {
        jsrt::sync::lock(&self.log).push(json!(["spawn", request, parent]));
        if request["definition"] == "refused-check@1" {
            return Err("Usage budget exhausted".into());
        }
        let mut tasks = jsrt::sync::lock(&self.tasks);
        if let Some(existing) = tasks.iter().find(|task| task["key"] == request["key"]) {
            return Ok(existing.clone());
        }
        let task = json!({"id": format!("task-{}", tasks.len() + 1), "key": request["key"],
            "definition": request["definition"], "state": "succeeded"});
        tasks.push(task.clone());
        Ok(task)
    }
    fn result(&self, task_id: &str) -> Result<Value, String> {
        jsrt::sync::lock(&self.log).push(json!(["result", task_id]));
        let tasks = jsrt::sync::lock(&self.tasks);
        let task = tasks
            .iter()
            .find(|task| task["id"] == task_id)
            .ok_or("Unknown Ultron task")?;
        Ok(match task["definition"].as_str().unwrap_or("") {
            "lost-check@1" => return Err("result unavailable".into()),
            "pass-check@1" => json!({"status": "succeeded", "value": {"passed": true}}),
            "false-check@1" => json!({"status": "succeeded", "value": {"passed": false}}),
            "vague-check@1" => json!({"status": "succeeded", "value": "fine"}),
            "crash-check@1" => json!({"status": "failed", "error": "check crashed"}),
            _ => json!({"status": "succeeded", "value": null}),
        })
    }
    fn cancel(&self, _task_id: &str, _reason: &str) -> Result<Value, String> {
        Err("unused".into())
    }
    fn steer(&self, _task_id: &str, _message: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
}

fn host(preloaded: Option<&Value>) -> Made {
    let store = Arc::new(MemoryDocumentStore::new(preloaded.map(|p| p["document"].clone())));
    let clock = Arc::new(AtomicI64::new(FIXED_NOW));
    let tasks = preloaded
        .and_then(|p| p["tasks"].as_array().cloned())
        .unwrap_or_default();
    let fake = Arc::new(FakeTasks::new(clock.clone(), tasks));
    let module = SchedulesModule::new(store.clone(), None);
    let log = fake.clone();
    Made {
        handler: KernelApis::new(inner(clock))
            .with_host(fake, "main")
            .with(Arc::new(module)),
        env: Vec::new(),
        state: Box::new(move || json!({"document": store.read().unwrap(), "log": *jsrt::sync::lock(&log.log)})),
    }
}

#[test]
fn kernel_apis_schedules_cells_match_typescript_host() {
    assert_area("schedules", &|_: &Path| host(None));
}

#[test]
fn kernel_apis_schedules_restart_resumes_pending_slot_like_typescript() {
    let fixture = fixture("schedules");
    let preloaded = fixture["restart"]["preloaded"].clone();
    for scenario in fixture["restart"]["scenarios"].as_array().unwrap() {
        let actual = run_scenario(scenario, &|_: &Path| host(Some(&preloaded)));
        assert_eq!(actual, *scenario, "{}", scenario["name"]);
    }
}

fn direct(clock: &Arc<AtomicI64>) -> (SchedulesModule, Arc<FakeTasks>, Arc<MemoryDocumentStore>) {
    let store = Arc::new(MemoryDocumentStore::default());
    let fake = Arc::new(FakeTasks::new(clock.clone(), Vec::new()));
    let now = clock.clone();
    let module = SchedulesModule::new(store.clone(), Some(Arc::new(move || now.load(Ordering::SeqCst))));
    (module, fake, store)
}

fn call(module: &SchedulesModule, host: &FakeTasks, kind: &str, payload: Value) -> Result<Value, String> {
    module.handle(kind, payload.as_object().unwrap(), None, host)
}

/// "duplicate and concurrent ticks in the same slot run the slot once"; a restart on the same stores does not
/// re-run a completed or claimed slot.
#[test]
fn kernel_apis_schedules_slot_runs_once_across_ticks_and_restart() {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (module, fake, store) = direct(&clock);
    call(
        &module,
        &fake,
        "schedules.create",
        json!({"definition": "identity@1", "input": {"n": 1}, "every_ms": 1000}),
    )
    .unwrap();
    let first = call(&module, &fake, "schedules.tick", json!({})).unwrap();
    assert_eq!(first["fired"].as_array().unwrap().len(), 1);
    assert_eq!(
        call(&module, &fake, "schedules.tick", json!({})).unwrap(),
        json!({"fired": [], "failed": []})
    );
    let restarted = SchedulesModule::new(store.clone(), Some(Arc::new(|| 1_000_500)));
    restarted.start(fake.as_ref()).unwrap();
    assert_eq!(
        call(&restarted, &fake, "schedules.tick", json!({})).unwrap(),
        json!({"fired": [], "failed": []})
    );
    assert_eq!(jsrt::sync::lock(&fake.tasks).len(), 1);
}

/// "missed ticks coalesce into one run for the latest slot and record the skipped count"; "max_runs completes".
#[test]
fn kernel_apis_schedules_missed_slots_coalesce_and_max_runs_completes() {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (module, fake, _) = direct(&clock);
    let s = call(
        &module,
        &fake,
        "schedules.create",
        json!({"definition": "a@1", "input": null, "every_ms": 100, "max_runs": 2}),
    )
    .unwrap();
    call(&module, &fake, "schedules.tick", json!({})).unwrap();
    clock.store(1_000_550, Ordering::SeqCst);
    let report = call(&module, &fake, "schedules.tick", json!({})).unwrap();
    assert_eq!(report["fired"][0]["slot"], 5);
    assert_eq!(report["fired"][0]["skipped"], 4);
    let list = call(&module, &fake, "schedules.list", json!({})).unwrap();
    assert_eq!(list[0]["state"], "completed");
    assert_eq!(list[0]["skipped_total"], 4);
    clock.store(1_009_000, Ordering::SeqCst);
    assert_eq!(
        call(&module, &fake, "schedules.tick", json!({})).unwrap()["fired"],
        json!([])
    );
    assert_eq!(
        call(&module, &fake, "schedules.pause", json!({"id": s["id"]})).unwrap_err(),
        "Schedule is completed"
    );
}

/// "admission failure from the usage budget is recorded once per slot without a retry storm".
#[test]
fn kernel_apis_schedules_admission_failure_recorded_once_per_slot() {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let (module, fake, _) = direct(&clock);
    call(
        &module,
        &fake,
        "schedules.create",
        json!({"definition": "refused-check@1", "input": {}, "every_ms": 1000}),
    )
    .unwrap();
    let report = call(&module, &fake, "schedules.tick", json!({})).unwrap();
    assert_eq!(report["failed"][0]["error"], "Usage budget exhausted");
    for _ in 0..3 {
        assert_eq!(
            call(&module, &fake, "schedules.tick", json!({})).unwrap(),
            json!({"fired": [], "failed": []})
        );
    }
    let list = call(&module, &fake, "schedules.list", json!({})).unwrap();
    assert_eq!(list[0]["failures"], 1);
    assert_eq!(
        list[0]["last_failure"],
        json!({"slot": 0, "error": "Usage budget exhausted", "at": 1_000_000})
    );
    assert_eq!(list[0]["pending_slot"], Value::Null);
}

#[test]
fn kernel_apis_schedules_rejects_invalid_documents() {
    let clock = Arc::new(AtomicI64::new(0));
    let fake = FakeTasks::new(clock, Vec::new());
    for document in [
        json!({"version": 2, "schedules": [], "goals": []}),
        json!({"version": 1, "schedules": {}, "goals": []}),
        json!({"version": 1, "schedules": [{"id": "x"}], "goals": [{"id": "x"}]}),
        json!({"version": 1, "schedules": [{"id": 3}], "goals": []}),
    ] {
        let module = SchedulesModule::new(Arc::new(MemoryDocumentStore::new(Some(document.clone()))), None);
        assert!(module.start(&fake).is_err(), "{document}");
    }
    let module = SchedulesModule::new(
        Arc::new(MemoryDocumentStore::new(Some(json!({"schedules": [], "goals": []})))),
        None,
    );
    assert!(module.start(&fake).is_ok());
}

/// B-report #11: `goals.verify` waits for its checks with the lock released. A goal paused meanwhile is not moved
/// to achieved by the late result (paused -> achieved without a resume); the round is still recorded.
#[test]
fn kernel_apis_goals_verify_does_not_achieve_a_goal_paused_while_its_checks_ran() {
    struct Pausing {
        inner: FakeTasks,
        module: std::sync::OnceLock<Arc<SchedulesModule>>,
        goal: Mutex<Option<Value>>,
    }
    impl TaskHost for Pausing {
        fn caller_task_id(&self, lane: &str) -> Option<String> {
            self.inner.caller_task_id(lane)
        }
        fn task_lane(&self, task_id: &str) -> Option<String> {
            self.inner.task_lane(task_id)
        }
        fn strategy(&self, definition: &str) -> Result<String, String> {
            self.inner.strategy(definition)
        }
        fn tasks(&self) -> Result<Vec<Value>, String> {
            self.inner.tasks()
        }
        fn spawn(&self, request: &Value, parent: Option<&str>) -> Result<Value, String> {
            self.inner.spawn(request, parent)
        }
        fn result(&self, task_id: &str) -> Result<Value, String> {
            if let Some(id) = jsrt::sync::lock(&self.goal).take() {
                let module = self.module.get().unwrap();
                module
                    .handle("goals.pause", json!({"id": id}).as_object().unwrap(), None, &self.inner)
                    .unwrap();
            }
            self.inner.result(task_id)
        }
        fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
            self.inner.cancel(task_id, reason)
        }
        fn steer(&self, task_id: &str, message: &str) -> Result<bool, String> {
            self.inner.steer(task_id, message)
        }
        fn now(&self) -> i64 {
            self.inner.now()
        }
    }
    let clock = Arc::new(AtomicI64::new(FIXED_NOW));
    let (module, _, _) = direct(&clock);
    let module = Arc::new(module);
    let host = Pausing {
        inner: FakeTasks::new(clock, Vec::new()),
        module: std::sync::OnceLock::new(),
        goal: Mutex::new(None),
    };
    let _ = host.module.set(module.clone());
    let call = |kind: &str, payload: Value| module.handle(kind, payload.as_object().unwrap(), None, &host);
    let goal = call(
        "goals.create",
        json!({"title": "ship", "required_checks": ["pass-check@1"]}),
    )
    .unwrap();
    *jsrt::sync::lock(&host.goal) = Some(goal["id"].clone());
    let verified = call("goals.verify", json!({"id": goal["id"]})).unwrap();
    assert_eq!(verified["verification"]["status"], "achieved");
    assert_eq!(verified["state"], "paused");
}

/// B-report #11: an older round that passes first, then a newer round that does not: the goal's state follows the
/// latest round (active again), never `achieved` beside a verification that says `unachieved`.
#[test]
fn kernel_apis_goals_verify_state_follows_the_latest_round() {
    /// Round 1's check passes once round 2 is admitted; round 2's fails, once round 1 has been recorded.
    struct Rounds {
        inner: FakeTasks,
        module: std::sync::OnceLock<Arc<SchedulesModule>>,
        goal: Value,
        newer: Mutex<Option<std::thread::JoinHandle<Value>>>,
        first_done: (Mutex<bool>, std::sync::Condvar),
    }
    impl TaskHost for Rounds {
        fn caller_task_id(&self, lane: &str) -> Option<String> {
            self.inner.caller_task_id(lane)
        }
        fn task_lane(&self, task_id: &str) -> Option<String> {
            self.inner.task_lane(task_id)
        }
        fn strategy(&self, definition: &str) -> Result<String, String> {
            self.inner.strategy(definition)
        }
        fn tasks(&self) -> Result<Vec<Value>, String> {
            self.inner.tasks()
        }
        fn spawn(&self, request: &Value, parent: Option<&str>) -> Result<Value, String> {
            self.inner.spawn(request, parent)
        }
        fn result(&self, task_id: &str) -> Result<Value, String> {
            let key = jsrt::sync::lock(&self.inner.tasks)
                .iter()
                .find(|task| task["id"] == task_id)
                .map(|task| task["key"].as_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            if key.contains(":verify:1:") {
                // Round 2 is admitted before round 1 is recorded.
                while jsrt::sync::lock(&self.inner.tasks).len() < 2 {
                    std::thread::yield_now();
                }
                return Ok(json!({"status": "succeeded", "value": {"passed": true}}));
            }
            let mut done = jsrt::sync::lock(&self.first_done.0);
            while !*done {
                done = self.first_done.1.wait(done).unwrap();
            }
            Ok(json!({"status": "succeeded", "value": {"passed": false}}))
        }
        fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
            self.inner.cancel(task_id, reason)
        }
        fn steer(&self, task_id: &str, message: &str) -> Result<bool, String> {
            self.inner.steer(task_id, message)
        }
        fn now(&self) -> i64 {
            self.inner.now()
        }
    }
    let clock = Arc::new(AtomicI64::new(FIXED_NOW));
    let (module, _, store) = direct(&clock);
    let module = Arc::new(module);
    let goal = module
        .handle(
            "goals.create",
            json!({"title": "ship", "required_checks": ["any-check@1"]})
                .as_object()
                .unwrap(),
            None,
            &FakeTasks::new(clock.clone(), Vec::new()),
        )
        .unwrap();
    let host = Arc::new(Rounds {
        inner: FakeTasks::new(clock, Vec::new()),
        module: std::sync::OnceLock::new(),
        goal: goal["id"].clone(),
        newer: Mutex::new(None),
        first_done: (Mutex::new(false), std::sync::Condvar::new()),
    });
    let _ = host.module.set(module.clone());
    let verify = |host: &Rounds| {
        host.module.get().unwrap().handle(
            "goals.verify",
            json!({"id": host.goal}).as_object().unwrap(),
            None,
            host,
        )
    };
    let first = {
        // Round 2 starts on a thread of its own once round 1 has admitted its check.
        let racing = host.clone();
        let watcher = std::thread::spawn(move || {
            while jsrt::sync::lock(&racing.inner.tasks).is_empty() {
                std::thread::yield_now();
            }
            verify(&racing).unwrap()
        });
        *jsrt::sync::lock(&host.newer) = Some(watcher);
        verify(&host).unwrap()
    };
    assert_eq!(first["verification"]["round"], 1);
    *jsrt::sync::lock(&host.first_done.0) = true;
    host.first_done.1.notify_all();
    let second = jsrt::sync::lock(&host.newer).take().unwrap().join().unwrap();
    assert_eq!(second["verification"]["round"], 2);
    assert_eq!(second["verification"]["status"], "unachieved");
    assert_eq!(second["state"], "active");
    let saved = store.read().unwrap().unwrap();
    assert_eq!(saved["goals"][0]["state"], "active");
}
