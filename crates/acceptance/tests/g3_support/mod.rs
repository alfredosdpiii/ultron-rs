//! The task side of the TypeScript `NativeRlmHost` as these acceptance tests use it, over the product's task host
//! (`delegation::agents::Agents`: admission, idempotency keys, parent links, cancellation cascades, lane aborts and
//! restart recovery over its task journal); the kernel-API modules are the real `kernel_apis` modules, started once
//! and dispatched by prefix as the TS host does.
//!
//! What the TS tests fake, this fakes the same way: task lanes are controlled probes (a prompt that blocks until the
//! test finishes it, a steer log, an abort count), named `ultron.<definition id>.<task id>` as the host names them.
//! The `agents.*` / `background.*` requests go to the host's own `handle`.

use delegation::agents::{Agents, AgentsOptions, LaneReply, LaneRun};
use kernel::Signal;
use kernel::store::DocumentStore;
use kernel_apis::{ApiModule, Request, TaskHost};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The session value holding the task journal (`memoryStore()` of the TS tests); clones share it.
#[derive(Clone, Default)]
pub struct JournalStore(pub Arc<Mutex<Option<Value>>>);

impl DocumentStore for JournalStore {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(lock(&self.0).clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        *lock(&self.0) = Some(document.clone());
        Ok(())
    }
}

/// A task lane as the TS `controlledHarness` / `waitingLane` fakes it.
#[derive(Default, Clone, Debug)]
pub struct Probe {
    pub steered: Vec<String>,
    pub aborts: usize,
}

/// The lanes' shared state: each task's probe, and the replies the test released.
#[derive(Default)]
struct Lanes {
    probes: Mutex<HashMap<String, Probe>>,
    replies: Mutex<HashMap<String, String>>,
    /// Tasks whose lane runs (the host knows the lane as the task's from then on).
    started: Mutex<HashSet<String>>,
    changed: Condvar,
}

impl Lanes {
    /// The probe of the task a lane (`ultron.<definition id>.<task id>`) runs.
    fn probe(&self, lane: &str, f: impl FnOnce(&mut Probe)) {
        let id = lane.rsplit_once('.').map_or(lane, |(_, id)| id);
        f(lock(&self.probes).entry(id.to_string()).or_default());
    }
}

pub struct Tasks {
    agents: Agents,
    lanes: Arc<Lanes>,
    pub clock: Arc<AtomicI64>,
}

pub fn lane_name(definition: &str, id: &str) -> String {
    format!("ultron.{}.{id}", definition.split('@').next().unwrap_or(definition))
}

impl Tasks {
    pub fn new(store: JournalStore, clock: Arc<AtomicI64>) -> Arc<Tasks> {
        let lanes: Arc<Lanes> = Arc::default();
        let mut options = AgentsOptions::new(Box::new(store));
        let now = clock.clone();
        options.now = Arc::new(move || now.load(Ordering::SeqCst) as f64);
        // A lane's prompt is pending until the test finishes it (`Tasks::finish`) or the task's work is aborted.
        let running = lanes.clone();
        options.lanes = Some(Arc::new(move |run: LaneRun| {
            lock(&running.started).insert(run.task_id.clone());
            running.changed.notify_all();
            loop {
                if let Some(text) = lock(&running.replies).remove(&run.task_id) {
                    return Ok(LaneReply {
                        text,
                        usage: None,
                        turns: 1,
                        tool_calls: 0,
                    });
                }
                if run.signal.wait(Duration::from_millis(2)) {
                    return Err(run.signal.reason().unwrap_or_else(|| "aborted".into()));
                }
            }
        }));
        let aborted = lanes.clone();
        options.abort_lane = Some(Arc::new(move |lane: &str| {
            aborted.probe(lane, |probe| probe.aborts += 1)
        }));
        let steered = lanes.clone();
        options.steer = Some(Arc::new(move |lane: &str, message: &str| {
            steered.probe(lane, |probe| probe.steered.push(message.to_string()));
            true
        }));
        let (agents, _events) = Agents::new(options);
        Arc::new(Tasks { agents, lanes, clock })
    }

    pub fn probe(&self, id: &str) -> Probe {
        lock(&self.lanes.probes).get(id).cloned().unwrap_or_default()
    }

    /// The journal document's tasks (`host.api.tasks()`), after the owner has loaded (and recovered) it.
    pub fn journal(&self) -> Vec<Value> {
        TaskHost::tasks(&self.agents).unwrap_or_default()
    }

    /// Wait until task `id`'s lane runs, as the TS host's lane is registered when `spawn` returns; a task that runs
    /// no lane (not `rlm`, or ended before it ran one) is not waited for.
    fn wait_started(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !lock(&self.lanes.started).contains(id) {
            let task = TaskHost::tasks(&self.agents)
                .unwrap_or_default()
                .into_iter()
                .find(|task| task["id"] == id);
            let lane = task.as_ref().is_some_and(|task| {
                task.get("result").is_none()
                    && TaskHost::strategy(&self.agents, task["definition"].as_str().unwrap_or_default())
                        .is_ok_and(|strategy| strategy == "rlm")
            });
            if !lane {
                return;
            }
            assert!(Instant::now() < deadline, "task {id} never started its lane");
            let started = lock(&self.lanes.started);
            if !started.contains(id) {
                let _ = self.lanes.changed.wait_timeout(started, Duration::from_millis(5));
            }
        }
    }

    /// The lane's prompt returned `text` (the TS probe's `finish`): the task completes with it. Returns the durable
    /// result once the host committed it.
    pub fn finish(&self, id: &str, text: &str) -> Value {
        lock(&self.lanes.replies).insert(id.to_string(), text.to_string());
        TaskHost::result(&self.agents, id).unwrap()
    }

    pub fn close(&self) {
        self.agents.close();
    }
}

impl TaskHost for Tasks {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        TaskHost::caller_task_id(&self.agents, lane)
    }
    fn task_lane(&self, task_id: &str) -> Option<String> {
        TaskHost::task_lane(&self.agents, task_id)
    }
    fn strategy(&self, definition: &str) -> Result<String, String> {
        TaskHost::strategy(&self.agents, definition)
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        TaskHost::tasks(&self.agents)
    }
    fn spawn(&self, request: &Value, parent: Option<&str>) -> Result<Value, String> {
        let admitted = TaskHost::spawn(&self.agents, request, parent)?;
        self.wait_started(admitted["id"].as_str().unwrap_or_default());
        Ok(admitted)
    }
    fn result(&self, task_id: &str) -> Result<Value, String> {
        TaskHost::result(&self.agents, task_id)
    }
    fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
        TaskHost::cancel(&self.agents, task_id, reason)
    }
    fn steer(&self, task_id: &str, message: &str) -> Result<bool, String> {
        TaskHost::steer(&self.agents, task_id, message)
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
}

/// `NativeRlmHost.handle(type, payload, context, {lane})`: the task host's requests, then modules by prefix.
pub struct Host {
    pub tasks: Arc<Tasks>,
    modules: Vec<Arc<dyn ApiModule>>,
    started: Mutex<Option<Result<(), String>>>,
}

impl Host {
    pub fn new(tasks: Arc<Tasks>, modules: Vec<Arc<dyn ApiModule>>) -> Host {
        Host {
            tasks,
            modules,
            started: Mutex::new(None),
        }
    }

    /// From the root lane ("main").
    pub fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.call_in("main", kind, payload)
    }

    pub fn call_in(&self, lane: &str, kind: &str, payload: Value) -> Result<Value, String> {
        let tasks = &self.tasks;
        if kind.starts_with("agents.") || kind.starts_with("background.") {
            let answer = tasks.agents.handle(kind, payload, lane, None)?;
            if matches!(kind, "agents.spawn" | "background.start") {
                tasks.wait_started(answer["id"].as_str().unwrap_or_default());
            }
            return Ok(answer);
        }
        let payload = payload.as_object().cloned().unwrap_or_default();
        let caller = tasks.caller_task_id(lane);
        lock(&self.started)
            .get_or_insert_with(|| self.modules.iter().try_for_each(|m| m.start(tasks.as_ref())))
            .clone()?;
        let module = self
            .modules
            .iter()
            .find(|m| m.prefixes().iter().any(|prefix| kind.starts_with(prefix)))
            .ok_or_else(|| format!("Ultron RLM host request is not wired: {kind}"))?;
        let signal = Signal::new();
        module.handle(&Request {
            kind,
            payload: &payload,
            lane,
            caller_task: caller.as_deref(),
            host: tasks.as_ref(),
            signal: &signal,
        })
    }
}

/// `expect(promise).rejects.toThrow(text)`.
pub fn rejects(result: Result<Value, String>, text: &str) {
    match result {
        Ok(value) => panic!("expected an error containing {text:?}, got {value}"),
        Err(error) => assert!(error.contains(text), "{error:?} does not contain {text:?}"),
    }
}
