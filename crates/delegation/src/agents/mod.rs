//! Typed agents of `native-host.ts`: `agents.register` / `list` / `status` / `tasks` / `spawn` / `invoke` /
//! `inspect` / `result` / `cancel`, `background.*`, and the `workflows.run` graph, over one task journal, one
//! definition registry and an optional usage ledger.
//!
//! Every strategy shares one path: input validation before admission, idempotent admission (a retried key neither
//! takes a budget slot nor releases the original's), the task's usage reservation, its timeout capped by the root
//! wall deadline, first-terminal-wins finishing that commits before publishing, subtree cancellation that aborts
//! the work, and output validation. What differs is the work: a deterministic adapter (code), a predict adapter
//! (one model call per attempt, bounded repair), or an `rlm` lane run ([`LaneRunner`]: a fresh lane with its own
//! REPL and, when named, its own model). Only `rlm` opens a lane.
//!
//! Waits block only the request's host thread: `agents.invoke` and workflow nodes cancel their task when the
//! requesting cell is cancelled (TS: the request context's abort); `agents.result` ends with an error then (TS keeps
//! waiting). The model's tool list is untouched: these are kernel APIs (`agents_api.py`).

use crate::DEFAULT_TIMEOUT_MS;
use crate::contract::Brief;
use crate::definitions::{Definition, DefinitionRegistry, Strategy};
use crate::estimate::Pricing;
use crate::task_store::TaskJournal;
use crate::usage::{Measurement, Reservation, ReserveRequest, UsageLedger};
use harness::task_store::task_fingerprint;
use jsrt::payload::nonempty_string;
use jsrt::{json, random, string, sync, utf16};
use kernel::Signal;
use kernel::store::DocumentStore;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

mod dispatch;
mod execution;
mod graph_view;
mod lifecycle;
mod managed_workflow;
mod predict;
mod workflow;
mod workflow_parse;

use execution::{bound_value, condition_met, json_from, object_input, text_of, verdict_gate};
use graph_view::{
    GRAPH_PREVIEW_CHARS, GRAPH_TEXT_CHARS, GRAPH_WORKFLOW_LIMIT, GRAPH_WORKFLOW_NODE_LIMIT, fetch_hint, public_record,
    public_task,
};
use managed_workflow::ManagedWorkflow;
pub use predict::{Complete, Completion, PREDICT_SYSTEM_PROMPT, PredictCall, predict_adapter};
use workflow::{Node, graph_workflow};

const WORKFLOW_MAX_ROUNDS: u64 = 10;

/// One deterministic or predict call (`NativeDefinitionAdapterRequest`).
pub struct AdapterRequest<'a> {
    /// The definition's public descriptor.
    pub definition: &'a Value,
    pub input: &'a Value,
    /// Aborted when the task is cancelled or times out.
    pub signal: &'a Signal,
    /// `{attempt, previous, error}` on a predict repair.
    pub repair: Option<Value>,
}

/// A deterministic or predict adapter: the value, or an error that fails the task.
pub type Adapter = Arc<dyn Fn(AdapterRequest<'_>) -> Result<Value, String> + Send + Sync>;

/// One `rlm` strategy run: a prompt on a lane of its own (or a reused one), its model when named.
pub struct LaneRun {
    pub task_id: String,
    /// The lane the run acts as: its kernel's host requests name it, so they resolve to this task.
    pub lane: String,
    /// Run on an existing lane (a retained instance) instead of a fresh one.
    pub reuse: bool,
    pub prompt: String,
    /// `provider/model` when the request or definition names one; `None`: the lane's default model.
    pub model: Option<String>,
    pub timeout_ms: u64,
    pub deadline_at: Option<f64>,
    pub definition: String,
    /// Aborted when the task is cancelled or times out.
    pub signal: Signal,
}

/// The completed run: the assistant text at its tip and the provider-reported usage of its responses (`None`
/// unless every response reported it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaneReply {
    pub text: String,
    pub usage: Option<Measurement>,
    /// Assistant turns and tool calls of the run (the graph view's counts).
    pub turns: u64,
    pub tool_calls: u64,
}

pub type LaneRunner = Arc<dyn Fn(LaneRun) -> Result<LaneReply, String> + Send + Sync>;

/// One `rlm-frame@1` task run (`NativeFrameRun`): bounded inference owns its prompts, repairs and budget. The
/// frame's value succeeds the task; an error fails it (cancelled once the task's signal is aborted).
pub struct FrameRun {
    pub task_id: String,
    pub input: Value,
    /// The lane the frame acts as (`ultron.rlm-frame.<task id>`): its kernel's requests resolve to this task.
    pub lane: String,
    pub model: Option<String>,
    /// The usage root the task's admission was charged to.
    pub usage_root: Option<String>,
    pub deadline_at: Option<f64>,
    pub timeout_ms: u64,
    /// Aborted when the task is cancelled or times out.
    pub signal: Signal,
}

pub type FrameRunner = Arc<dyn Fn(FrameRun) -> Result<Value, String> + Send + Sync>;

/// A detached task reached its durable terminal result (`DetachedTaskEnd`).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskEnd {
    pub task: Value,
    /// "task_done".
    pub kind: &'static str,
    pub owner_lane: String,
    pub root_id: Option<String>,
    pub awaited: bool,
    pub fetch: String,
    pub cost: Option<f64>,
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
/// Called with a lane name (the lane's abort).
pub type LaneHook = Arc<dyn Fn(&str) + Send + Sync>;
/// Prepares a lane before a task reuses it.
pub type LaneCheck = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// Active refinements of a definition id: `[{id, version, text}]`.
pub type RefinementsHook = Arc<dyn Fn(&str) -> Result<Vec<Value>, String> + Send + Sync>;
/// A task's durable end, with its cost and model when known.
pub type TaskEndHook = Arc<dyn Fn(&Value, Option<f64>, Option<&str>) + Send + Sync>;
/// Extra `agents.status` fields for a calling lane.
pub type StatusExtras = Arc<dyn Fn(&str) -> Map<String, Value> + Send + Sync>;
/// A lane and a message or holder, answered yes or no (steering, pinning).
pub type LaneMessage = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;
/// A lane and a holder (unpinning).
pub type LaneRelease = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct AgentsOptions {
    pub store: Box<dyn DocumentStore>,
    /// Runtime registration needs it (`options.definitionStore`).
    pub definition_store: Option<Box<dyn DocumentStore>>,
    pub usage: Option<UsageLedger>,
    pub deterministic: Option<Adapter>,
    pub predict: Option<Adapter>,
    pub lanes: Option<LaneRunner>,
    /// Runs `rlm-frame@1` tasks (bounded inference frames); without it they fail.
    pub frames: Option<FrameRunner>,
    /// Best-effort abort of a task's lane (the lane's own `abort`).
    pub abort_lane: Option<LaneHook>,
    /// Before a task runs on a reused lane (clears per-invocation scratch).
    pub before_lane_reuse: Option<LaneCheck>,
    /// Active refinements for a definition id: `[{id, version, text}]`.
    pub refinements: Option<RefinementsHook>,
    /// Called once a task's terminal result is durable, with its cost and model when known.
    pub on_task_end: Option<TaskEndHook>,
    /// Extra `agents.status` fields for a calling lane.
    pub status_extras: Option<StatusExtras>,
    /// Queue a steering message into a lane; false when it has no live run.
    pub steer: Option<LaneMessage>,
    pub pin_lane: Option<LaneMessage>,
    pub unpin_lane: Option<LaneRelease>,
    /// Charge each root turn to a usage root of its own (see [`Agents::begin_root_turn`]).
    pub root_turns: bool,
    pub now: Clock,
    /// Model prices for `budget.estimate` and the stop-line of workflow nodes; without it nothing is priced.
    pub pricing: Option<Pricing>,
}

impl AgentsOptions {
    /// An in-memory journal, no ledger, no adapters, no lanes, the system clock.
    pub fn new(store: Box<dyn DocumentStore>) -> Self {
        AgentsOptions {
            store,
            definition_store: None,
            usage: None,
            deterministic: None,
            predict: None,
            lanes: None,
            frames: None,
            abort_lane: None,
            before_lane_reuse: None,
            refinements: None,
            on_task_end: None,
            status_extras: None,
            steer: None,
            pin_lane: None,
            unpin_lane: None,
            root_turns: false,
            now: Arc::new(|| jsrt::date::now_ms() as f64),
            pricing: None,
        }
    }
}

/// A request to admit one task (`TaskRequest`).
#[derive(Clone, Debug, Default)]
struct TaskRequest {
    definition: String,
    input: Value,
    model: Option<String>,
    key: Option<String>,
    timeout_ms: u64,
    lane: Option<String>,
    workflow: Option<Value>,
    untimed: bool,
    usage_root: Option<String>,
    /// The delegation brief (role, instruction, contract, hop) whose contract the result must meet.
    brief: Option<Brief>,
    /// What the task is expected to cost (a workflow node's estimate): its admission is checked against the stop-line.
    estimate_usd: Option<f64>,
}

#[derive(Default)]
struct Task {
    id: String,
    parent_id: Option<String>,
    definition: String,
    record: Value,
    /// The durable terminal result, or the commit failure.
    outcome: Option<Result<Value, String>>,
    finishing: bool,
    /// Admitted by this owner: its result is awaited, not read.
    live: bool,
    signal: Signal,
    /// Aborted once the task ended (stops its timer).
    done: Signal,
    lane: Option<String>,
    reservation: Option<Reservation>,
    usage_settled: bool,
    /// Model reservations of the task's run; the task's end settles any its run has not.
    model_reservations: Vec<Reservation>,
    notify: Option<&'static str>,
    waiters: usize,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    input_preview: Option<String>,
    workflow: Option<Value>,
    cost: Option<f64>,
    tokens: Option<f64>,
    model: Option<String>,
    turns: Option<u64>,
    tool_calls: Option<u64>,
    last_text: Option<String>,
    verdict: Option<Value>,
    verdict_rejections: u32,
    verdict_problems: Vec<String>,
    verdict_invalid: bool,
}

struct State {
    journal: TaskJournal,
    registry: DefinitionRegistry,
    tasks: Vec<Task>,
    lane_tasks: HashMap<String, String>,
    closed: bool,
    loaded: Option<Result<(), String>>,
    workflow_runs: Vec<Value>,
    managed_workflows: HashMap<String, ManagedWorkflow>,
    root_turn: Option<String>,
    root_turn_active: bool,
    root_aliases: Vec<(String, String)>,
}

impl State {
    fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
    fn parent_of(&self, id: &str) -> Option<String> {
        self.task(id).and_then(|t| t.parent_id.clone())
    }
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    /// Admissions run one at a time (TS serializes them through installation of the live task).
    admissions: Mutex<()>,
    usage: Option<UsageLedger>,
    deterministic: Option<Adapter>,
    predict: Option<Adapter>,
    lanes: Option<LaneRunner>,
    frames: Option<FrameRunner>,
    abort_lane: Option<LaneHook>,
    before_lane_reuse: Option<LaneCheck>,
    refinements: Option<RefinementsHook>,
    on_task_end: Option<TaskEndHook>,
    status_extras: Option<StatusExtras>,
    steer: Option<LaneMessage>,
    pin_lane: Option<LaneMessage>,
    unpin_lane: Option<LaneRelease>,
    root_turns: bool,
    now: Clock,
    pricing: Option<Pricing>,
    events: Mutex<Sender<TaskEnd>>,
    /// The subagent host, for `rlm-child@1` workflow nodes with `worktree=True` / `"auto"` (`workflowTask` +
    /// `newWorktree` of native-host.ts). It owns worktrees: their creation, setup, commit at the end, `rlm.merge` and
    /// cleanup, so a worktree node is a subagent of the workflow's caller there.
    worktree_children: std::sync::OnceLock<crate::host::Delegation>,
}

/// The typed agent host; clones share it.
#[derive(Clone)]
pub struct Agents {
    inner: Arc<Inner>,
}

/// `definitionKey`: `id@version`.
fn definition_key(value: Option<&Value>) -> Result<String, String> {
    value
        .and_then(Value::as_str)
        .filter(|text| harness::task_store::is_definition(text))
        .map(str::to_string)
        .ok_or_else(|| "definition must be id@version".into())
}

/// `taskOptions`: model, key and timeout of a request.
fn task_options(payload: &Map<String, Value>) -> Result<(Option<String>, Option<String>, u64), String> {
    let model = match payload.get("model") {
        None | Some(Value::Null) => None,
        Some(model) => Some(model_scope::model_ref(model)?),
    };
    let key = match payload.get("key") {
        None | Some(Value::Null) => None,
        value => Some(nonempty_string(value, "key")?),
    };
    let timeout = crate::timeout_ms(payload.get("timeout_ms"))?;
    Ok((model, key, timeout))
}

fn failed(error: impl Into<String>) -> Value {
    json!({ "status": "failed", "error": error.into(), "verification": "unverified" })
}

fn cancelled(error: impl Into<String>) -> Value {
    json!({ "status": "cancelled", "error": error.into(), "verification": "unverified" })
}

fn succeeded(value: Value) -> Value {
    json!({ "status": "succeeded", "value": value, "verification": "unverified" })
}

impl Agents {
    pub fn new(options: AgentsOptions) -> (Self, Receiver<TaskEnd>) {
        let (sender, receiver) = channel();
        let registry = DefinitionRegistry::new(
            options.definition_store,
            options.deterministic.is_some(),
            options.predict.is_some(),
        );
        let agents = Agents {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    journal: TaskJournal::new(options.store),
                    registry,
                    tasks: Vec::new(),
                    lane_tasks: HashMap::new(),
                    closed: false,
                    loaded: None,
                    workflow_runs: Vec::new(),
                    managed_workflows: HashMap::new(),
                    root_turn: None,
                    root_turn_active: false,
                    root_aliases: Vec::new(),
                }),
                changed: Condvar::new(),
                admissions: Mutex::new(()),
                usage: options.usage,
                deterministic: options.deterministic,
                predict: options.predict,
                lanes: options.lanes,
                frames: options.frames,
                abort_lane: options.abort_lane,
                before_lane_reuse: options.before_lane_reuse,
                refinements: options.refinements,
                on_task_end: options.on_task_end,
                status_extras: options.status_extras,
                steer: options.steer,
                pin_lane: options.pin_lane,
                unpin_lane: options.unpin_lane,
                worktree_children: std::sync::OnceLock::new(),
                root_turns: options.root_turns,
                now: options.now,
                pricing: options.pricing,
                events: Mutex::new(sender),
            }),
        };
        (agents, receiver)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        sync::lock(&self.inner.state)
    }

    fn now(&self) -> f64 {
        (self.inner.now)()
    }

    fn load_tasks_once(&self, state: &mut State) -> Result<(), String> {
        state.registry.ready()?;
        for record in state.journal.list()? {
            Self::adopt(state, record);
        }
        if let Some(usage) = &self.inner.usage {
            usage.ready()?;
            usage.reconcile(&[])?;
        }
        Ok(())
    }

    /// `loadTasks`: the registry, the journal (an earlier owner's unfinished tasks become interrupted), and the
    /// ledger, whose open reservations all belong to an ended owner and settle as unknown.
    pub fn load_tasks(&self) -> Result<(), String> {
        let mut state = self.state();
        if state.loaded.is_none() {
            let outcome = self.load_tasks_once(&mut state);
            state.loaded = Some(outcome);
        }
        state.loaded.clone().unwrap_or(Ok(()))?;
        state.registry.ready()?;
        state.journal.list().map(|_| ())
    }

    /// Tasks still running whose end is announced to `lane` ("main" for the root) as a `task_done`-style event: what
    /// a turn scheduler (the print client's `async.pending`) waits for before it ends.
    pub fn running_announced(&self, lane: &str) -> usize {
        let state = self.state();
        state
            .tasks
            .iter()
            .filter(|t| t.outcome.is_none() && t.notify.is_some())
            .filter(|t| {
                let owner = t
                    .parent_id
                    .as_deref()
                    .and_then(|p| state.task(p))
                    .and_then(|p| p.lane.as_deref());
                owner.unwrap_or("main") == lane
            })
            .count()
    }

    /// The task that owns `lane` (`None`: the root lane or a lane this host did not start).
    pub fn caller_task(&self, lane: &str) -> Option<String> {
        self.state().lane_tasks.get(lane).cloned()
    }

    /// Whether `lane` runs a task of this host (its `rlm.finish` is answered here).
    pub fn owns_lane(&self, lane: &str) -> bool {
        self.state().lane_tasks.contains_key(lane)
    }

    /// `beginRootTurn(runId)`: work admitted while the turn runs is charged to the turn's own root.
    pub fn begin_root_turn(&self, run_id: &str) {
        let mut state = self.state();
        let root = Self::root_of_run(&state, run_id);
        state.root_turn = Some(root);
        state.root_turn_active = true;
    }

    pub fn end_root_turn(&self, run_id: &str) {
        let mut state = self.state();
        if state.root_turn.as_deref() == Some(Self::root_of_run(&state, run_id).as_str()) {
            state.root_turn_active = false;
        }
    }

    /// `continueRootTurn(runId, rootId)`: a run started by a completion event keeps its earlier root.
    pub fn continue_root_turn(&self, run_id: &str, root_id: &str) {
        let mut state = self.state();
        state.root_aliases.retain(|(run, _)| run != run_id);
        state.root_aliases.push((run_id.into(), root_id.into()));
        if state.root_aliases.len() > 256 {
            state.root_aliases.remove(0);
        }
    }

    fn root_of_run(state: &State, run_id: &str) -> String {
        state
            .root_aliases
            .iter()
            .find(|(run, _)| run == run_id)
            .map_or_else(|| format!("turn:{run_id}"), |(_, root)| root.clone())
    }

    /// `usageRootForLane(lane, runId)`: the root a model turn on `lane` is charged to.
    pub fn usage_root_for_lane(&self, lane: &str, run_id: &str) -> Option<String> {
        let state = self.state();
        if lane == "main" {
            return self.inner.root_turns.then(|| Self::root_of_run(&state, run_id));
        }
        let task = state.lane_tasks.get(lane)?;
        state
            .task(task)
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string())
    }

    fn admission_root(&self, state: &State, parent: Option<&str>) -> Option<String> {
        let inherited = parent
            .and_then(|id| state.task(id))
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string());
        if inherited.is_some() {
            return inherited;
        }
        if !self.inner.root_turns {
            return None;
        }
        if state.root_turn_active && state.root_turn.is_some() {
            return state.root_turn.clone();
        }
        Some(format!("job:{}", random::uuid_v4()))
    }

    fn status_root(&self, state: &State, parent: Option<&str>) -> Option<String> {
        let inherited = parent
            .and_then(|id| state.task(id))
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string());
        inherited.or_else(|| state.root_turn.clone().filter(|_| self.inner.root_turns))
    }

    fn definition(&self, key: &str) -> Result<Definition, String> {
        self.state().registry.get(key)
    }

    /// `request(payload)` of `agents.spawn` / `agents.invoke`.
    fn request(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<TaskRequest, String> {
        jsrt::payload::fields(payload, &["definition", "input", "model", "key", "timeout_ms", "brief"])?;
        let definition = definition_key(payload.get("definition"))?;
        let Some(input) = payload.get("input") else {
            return Err("Agent input is not JSON".into());
        };
        let item = self.definition(&definition)?;
        {
            let state = self.state();
            if !state.registry.is_valid_input(&item, input) {
                return Err(state.registry.validation_error(&item, input, "input"));
            }
        }
        let (model, key, timeout_ms) = task_options(payload)?;
        let brief = match payload.get("brief") {
            None | Some(Value::Null) => None,
            Some(brief) => Some(Brief::parse(brief, Some(self.hop_below(parent)))?),
        };
        Ok(TaskRequest {
            definition,
            input: input.clone(),
            model,
            key,
            timeout_ms,
            brief,
            ..TaskRequest::default()
        })
    }

    /// The hop of a task `parent` starts: one more than the delegations above it (the root's children are hop 1).
    fn hop_below(&self, parent: Option<&str>) -> u32 {
        let state = self.state();
        let mut hop = 1;
        let mut current = parent.map(str::to_string);
        while let Some(id) = current {
            hop += 1;
            current = state.parent_of(&id);
        }
        hop
    }

    /// `spawnTask`: idempotent admission under the usage ledger, then the run starts on a thread of its own.
    fn spawn_task(
        &self,
        request: TaskRequest,
        cell: Option<&Signal>,
        parent: Option<&str>,
        notify: Option<&'static str>,
    ) -> Result<String, String> {
        self.spawn_task_with_record(request, cell, parent, notify)
            .map(|(id, _)| id)
    }

    fn find_idempotent_task(&self, key: &str, fingerprint: &str) -> Result<Option<(String, Value)>, String> {
        let mut state = self.state();
        let prior = state.journal.list()?.into_iter().find(|t| t["key"] == key);
        let Some(prior) = prior else {
            return Ok(None);
        };
        if prior["fingerprint"] != fingerprint {
            return Err("Idempotency key reused for a different task".into());
        }
        let id = prior["id"].as_str().unwrap_or_default().to_string();
        Self::adopt(&mut state, prior);
        let record = state.task(&id).map_or(Value::Null, |t| t.record.clone());
        Ok(Some((id, record)))
    }

    fn reserve_task_slot(
        &self,
        request: &TaskRequest,
        key: &str,
        parent: Option<&str>,
    ) -> Result<Option<Reservation>, String> {
        let strategy = self.definition(&request.definition)?.strategy();
        let root = {
            let state = self.state();
            request
                .usage_root
                .clone()
                .or_else(|| self.admission_root(&state, parent))
        };
        let Some(usage) = &self.inner.usage else {
            return Ok(None);
        };
        usage
            .reserve(&ReserveRequest {
                kind: "task".into(),
                root_id: root,
                model_backed: Some(strategy != Strategy::Deterministic),
                request_key: Some(key.to_string()),
                timeout_ms: (!request.untimed).then_some(request.timeout_ms as f64),
                estimated_cost_usd: request.estimate_usd,
                ..ReserveRequest::default()
            })
            .map(Some)
    }

    fn spawn_deadline_watcher(
        &self,
        id: &str,
        request: &TaskRequest,
        done: Signal,
        now: f64,
        deadline_at: Option<f64>,
        admitted_at: Option<f64>,
    ) {
        let delay = match deadline_at {
            None => (!request.untimed).then_some(request.timeout_ms as f64),
            Some(deadline) => Some(if request.untimed {
                (deadline - now).max(1.0)
            } else {
                (request.timeout_ms as f64).min(deadline - now).max(1.0)
            }),
        };
        let Some(delay) = delay else {
            return;
        };
        let wall = deadline_at.is_some_and(|deadline| {
            request.untimed || deadline < admitted_at.unwrap_or(now) + request.timeout_ms as f64
        });
        let host = self.clone();
        let (task_id, timeout) = (id.to_string(), request.timeout_ms);
        std::thread::spawn(move || {
            if !done.wait(Duration::from_secs_f64(delay / 1000.0)) {
                let reason = if wall {
                    "Ultron root wall deadline exceeded".to_string()
                } else {
                    format!("Ultron task exceeded {timeout}ms timeout")
                };
                let _ = host.cancel(&task_id, &reason);
            }
        });
    }

    fn settle_reservation(&self, reservation: Option<&Reservation>, status: &str) {
        if let (Some(usage), Some(reservation)) = (&self.inner.usage, reservation) {
            let _ = usage.settle(reservation, Some(status), None);
        }
    }

    fn start_admitted_task(
        &self,
        id: String,
        request: TaskRequest,
        record: Value,
        reservation: Option<Reservation>,
        parent: Option<&str>,
        notify: Option<&'static str>,
    ) -> (String, Value) {
        let now = self.now();
        let deadline_at = reservation.as_ref().and_then(Reservation::deadline_at);
        let admitted_at = reservation.as_ref().map(Reservation::admitted_at);
        let task = Task {
            id: id.clone(),
            parent_id: parent.map(String::from),
            definition: request.definition.clone(),
            record,
            live: true,
            reservation,
            notify,
            started_at: Some(now),
            input_preview: Some(json::preview(&request.input, GRAPH_PREVIEW_CHARS)),
            workflow: request.workflow.clone(),
            ..Task::default()
        };
        let done = task.done.clone();
        let closed = {
            let mut state = self.state();
            state.tasks.push(task);
            state.closed
        };
        self.spawn_deadline_watcher(&id, &request, done, now, deadline_at, admitted_at);
        let admitted = self.record(&id);
        if closed {
            let _ = self.cancel(&id, "Ultron task host closed");
        } else {
            let host = self.clone();
            let task_id = id.clone();
            std::thread::spawn(move || host.run(&task_id, request));
        }
        (id, admitted)
    }

    /// [`Self::spawn_task`], with the task's record as admitted: read before its run starts on another thread, so
    /// a spawn reports `admitted` as TS does (its run starts after the call returns), never a state the run reached.
    fn spawn_task_with_record(
        &self,
        request: TaskRequest,
        cell: Option<&Signal>,
        parent: Option<&str>,
        notify: Option<&'static str>,
    ) -> Result<(String, Value), String> {
        let _admitting = sync::lock(&self.inner.admissions);
        if self.state().closed {
            return Err("Ultron task host is closed".into());
        }
        if let Some(reason) = cell.and_then(Signal::reason) {
            return Err(reason);
        }
        let fingerprint = request_fingerprint(&request);
        let key = request.key.clone().unwrap_or_else(random::uuid_v4);
        if request.key.is_some()
            && let Some(existing) = self.find_idempotent_task(&key, &fingerprint)?
        {
            return Ok(existing);
        }
        let reservation = self.reserve_task_slot(&request, &key, parent)?;
        let (record, created) = match self
            .state()
            .journal
            .admit(&request.definition, &fingerprint, &key, parent)
        {
            Ok(admitted) => admitted,
            Err(error) => {
                self.settle_reservation(reservation.as_ref(), "unknown");
                return Err(error);
            }
        };
        let id = record["id"].as_str().unwrap_or_default().to_string();
        if !created {
            self.settle_reservation(reservation.as_ref(), "succeeded");
            Self::adopt(&mut self.state(), record);
            return Ok((id.clone(), self.record(&id)));
        }
        Ok(self.start_admitted_task(id, request, record, reservation, parent, notify))
    }

    /// A journal record this owner did not start (an earlier owner's, or a retried key's) joins the task table.
    fn adopt(state: &mut State, record: Value) {
        let id = record["id"].as_str().unwrap_or_default().to_string();
        if state.task(&id).is_some() {
            return;
        }
        state.tasks.push(Task {
            parent_id: record["parentId"].as_str().map(String::from),
            definition: record["definition"].as_str().unwrap_or_default().into(),
            outcome: record.get("result").cloned().map(Ok),
            record,
            id,
            ..Task::default()
        });
    }

    /// `run`: the running transition, the work, then the terminal result.
    fn run(&self, id: &str, request: TaskRequest) {
        let started = {
            let mut state = self.state();
            match state.journal.transition(id, "running", None) {
                Ok(record) => {
                    let finishing = state.task(id).is_none_or(|t| t.finishing || t.outcome.is_some());
                    if let Some(task) = state.task_mut(id)
                        && !finishing
                    {
                        task.record = record;
                    }
                    Ok(!finishing)
                }
                Err(error) => Err(error),
            }
        };
        match started {
            Ok(false) => {}
            Ok(true) => {
                let result = self.execute(id, &request);
                let result = self.with_limit_reason(id, result);
                let result = match &request.brief {
                    Some(brief) => brief.verify(result),
                    None => result,
                };
                let _ = self.finish(id, result);
            }
            Err(error) => {
                let mut state = self.state();
                if let Some(task) = state.task_mut(id) {
                    task.signal.abort(error.clone());
                    task.done.abort("ended");
                    task.outcome = Some(Err(error));
                }
                self.settle_task(&mut state, id, "unknown");
                self.inner.changed.notify_all();
            }
        }
    }

    /// `withLimitReason`: a task that failed once its root's tree ran out of turns, tokens or cost fails with the
    /// limit message.
    fn with_limit_reason(&self, id: &str, result: Value) -> Value {
        if result["status"] != "failed" {
            return result;
        }
        let (Some(usage), root) = (&self.inner.usage, self.root_of_task(id)) else {
            return result;
        };
        match usage.turn_budget_exhausted(root.as_deref()) {
            Ok(Some(reason)) if result["error"] != reason.as_str() => {
                let mut result = result;
                result["error"] = json!(reason);
                result
            }
            _ => result,
        }
    }

    fn root_of_task(&self, id: &str) -> Option<String> {
        self.state()
            .task(id)
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string())
    }

    fn reserve_model(
        &self,
        id: &str,
        request_key: String,
        request: &TaskRequest,
    ) -> Result<Option<Reservation>, String> {
        let Some(usage) = &self.inner.usage else {
            return Ok(None);
        };
        let mut state = self.state();
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        if let Some(reason) = task.signal.reason() {
            return Err(reason);
        }
        if task.outcome.is_some() || task.finishing {
            return Err("Ultron task already ended".into());
        }
        let reservation = task.reservation.clone();
        let (root, deadline) = (
            reservation.as_ref().map(|r| r.root_id().to_string()),
            reservation.as_ref().and_then(Reservation::deadline_at),
        );
        let reservation = usage.reserve(&ReserveRequest {
            kind: "model".into(),
            root_id: root,
            parent_task_id: Some(id.into()),
            task_id: Some(id.into()),
            request_key: Some(request_key),
            timeout_ms: (deadline.is_none() && !request.untimed).then_some(request.timeout_ms as f64),
            deadline_at: deadline,
            ..ReserveRequest::default()
        })?;
        if let Some(task) = state.task_mut(id) {
            task.model_reservations.push(reservation.clone());
        }
        Ok(Some(reservation))
    }

    fn abort_lane(&self, lane: &str) {
        if let Some(hook) = &self.inner.abort_lane {
            hook(lane);
        }
    }

    fn settle_task(&self, state: &mut State, id: &str, status: &str) {
        let Some(task) = state.task_mut(id) else {
            return;
        };
        if task.usage_settled {
            return;
        }
        task.usage_settled = true;
        let Some(usage) = &self.inner.usage else {
            return;
        };
        for reservation in std::mem::take(&mut task.model_reservations) {
            let _ = usage.settle(&reservation, Some(status), None);
        }
        if let Some(reservation) = task.reservation.clone() {
            let _ = usage.settle(&reservation, Some(status), None);
        }
    }
}

fn request_fingerprint(request: &TaskRequest) -> String {
    let mut fingerprinted = json!({
        "definition": request.definition, "input": request.input, "model": request.model,
        "timeout_ms": request.timeout_ms,
    });
    if let Some(brief) = &request.brief {
        fingerprinted["brief"] = brief.to_value();
    }
    task_fingerprint(&fingerprinted)
}
