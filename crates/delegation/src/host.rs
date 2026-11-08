//! The `rlm.spawn` / `rlm.collect` / `rlm.finish` / `rlm.list_subagents` / `rlm.delete_subagent` host requests of
//! `native-host.ts`, for subagents run by a [`ChildRunner`] (its `externalChild` path): each child runs on a thread of
//! its own with its own lane name, so its host requests (its `rlm.finish`, nested spawns) arrive through
//! [`Delegation::lane_handler`] like any kernel's.
//!
//! Waits are host-side. With [`CollectMode::Host`] a cell's `rlm.collect` never waits (it fails at once naming the
//! running children) and the turn scheduler waits between turns ([`Delegation::running`], [`Delegation::wait`], the
//! [`DetachedEnd`] channel); its `rlm.merge` waits on its own host thread and reports itself through
//! [`CellContext::park`] so the lane detaches the cell. With [`CollectMode::InCell`] the wait blocks the cell itself
//! (never parked, so the cell is never detached): for a lane nothing wakes when its work ends.
//! [`DelegationOptions::collect`] picks the mode per lane.
//! A detached child's end is pushed to the [`DetachedEnd`] channel as soon as its result is durable.

mod restore;
pub use restore::{ChildWaiter, Reattached};

use crate::git_baseline::GitBaseline;
use crate::spawn_contract::{CheckPlace, CheckRunner, Enforcer, SpawnContract, enforce, plain_check_runner};
use crate::task_store::{TaskJournal, task_fingerprint};
use crate::verdict::{MAX_VERDICT_REJECTIONS, validate_verdict};
use crate::workspace::{WorkspaceSnapshot, check_files, diff_snapshots, snapshot_workspace};
use crate::worktrees::{self as wt, WorktreeRecord, WorktreeSetup};
use jsrt::payload::nonempty_string;
use jsrt::{number, sync};
use kernel::store::{DocumentStore, MemoryDocumentStore};
use kernel::{HostHandler, Signal};
use lanes::Park;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

pub const SPAWN_DEPTH_ENV: &str = "ULTRON_SPAWN_DEPTH";

const CHILD: &str = "rlm-child@1";
/// Default ceiling on `rlm.spawn` nesting (`ULTRON_SPAWN_DEPTH`).
pub const DEFAULT_SPAWN_DEPTH: u32 = 3;

/// `spawnDepthLimit`: `ULTRON_SPAWN_DEPTH` as a nonnegative integer, else the default; 0 means no ceiling.
pub fn spawn_depth_limit(raw: Option<&str>) -> u32 {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| number::is_safe_integer(*v) && *v >= 0.0)
        .map_or(DEFAULT_SPAWN_DEPTH, |v| v.min(f64::from(u32::MAX)) as u32)
}

/// One subagent run (`NativeExternalChildRun`).
#[derive(Clone)]
pub struct ChildRun {
    pub task_id: String,
    /// The `rlm.spawn(name=...)` the child was started with (kept with its task: a revived child is named so again).
    pub name: String,
    /// The lane the child acts as: its host requests go through `host.lane_handler(&lane_name)`.
    pub lane_name: String,
    pub prompt: String,
    /// Nesting levels above the child (1 for a child of the root) and how many more it may create.
    pub level: u32,
    pub allowance: u32,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub timeout_ms: u64,
    /// A worktree child's (or its descendant's) working directory and extra environment; `None` / empty: the host's.
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// `rlm.spawn(fork=True)`: the anchor (from the [`crate::fork::ForkPointHook`]) of the parent's conversation the
    /// child's conversation starts from; its brief then leads with [`crate::fork::FORK_BRIEF_NOTE`].
    pub fork_at: Option<String>,
    /// Aborted when the child is cancelled (delete, timeout, ancestor cancelled, host closed).
    pub signal: Signal,
    pub host: Delegation,
}

/// Runs a child to its final reply text, or fails with an error message.
pub type ChildRunner = Arc<dyn Fn(ChildRun) -> Result<String, String> + Send + Sync>;
/// Sees a subagent's terminal record.
pub type TaskEndObserver = Arc<dyn Fn(&Value) + Send + Sync>;
/// Sees a subagent admitted by `rlm.spawn`, before it starts: its task id, its parent task (none for a child of the
/// root) and the lane that spawned it.
pub type SpawnObserver = Arc<dyn Fn(&str, Option<&str>, &str) + Send + Sync>;
/// The limit text of child `task_id`'s root while its tree is spent (`turnBudgetExhausted(task.usageReservation.rootId)`).
pub type LimitReason = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A detached task reached its durable terminal result (`DetachedTaskEnd`).
#[derive(Clone, Debug, PartialEq)]
pub struct DetachedEnd {
    pub task: Value,
    pub kind: &'static str,
    pub owner_lane: String,
    /// A request was waiting on the result when it ended.
    pub awaited: bool,
    /// The kernel call that returns the full result.
    pub fetch: String,
}

/// What a request knows about the cell that made it.
#[derive(Clone, Default)]
pub struct CellContext {
    /// Aborted when the cell ends or is cancelled: ends a waiting `rlm.collect` with an error.
    pub cancel: Option<Signal>,
    pub park: Option<Park>,
}

/// How `rlm.collect` treats children that are still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectMode {
    /// The cell never waits: with every selected child ended it returns their results; otherwise it fails at once,
    /// naming the running children, and the host waits instead (their ends arrive as `child_done` events).
    Host,
    /// The cell waits for every selected child (cancelled with the cell, bounded by the request's `timeout_ms`) and
    /// is never parked: for a lane whose host does not wake it when its work ends, a parked cell's `cell_done` would
    /// reach nobody. `rlm.merge` waits the same way.
    InCell,
}

/// The [`CollectMode`] of each lane.
pub type LaneCollect = Arc<dyn Fn(&str) -> CollectMode + Send + Sync>;

pub struct DelegationOptions {
    pub store: Box<dyn DocumentStore>,
    /// How `rlm.collect` (and `rlm.merge`'s wait) treats running children, per requesting lane: `Host` only where
    /// the host wakes the lane when its work ends, `InCell` elsewhere.
    pub collect: LaneCollect,
    pub runner: ChildRunner,
    /// See [`spawn_depth_limit`].
    pub spawn_depth: u32,
    pub now: Box<dyn Fn() -> f64 + Send + Sync>,
    /// The working directory subagents share: snapshotted when a child starts and ends to check its verdict's
    /// `changed_files`, and the tree worktree children branch from and merge into.
    pub workspace: Option<PathBuf>,
    /// Names worktree branches (`ultron/<session>/<child>`).
    pub session_id: String,
    /// Default worktree setup (the `worktrees` setting), read at each worktree spawn (TS `worktreeSetup`); a
    /// spawn's `worktree_setup=` overrides it key by key.
    pub worktree_setup: WorktreeSetupSource,
    /// `ULTRON_KEEP_WORKTREES`: keep worktrees and branches after merges and on close.
    pub keep_worktrees: bool,
    /// Refuses a spawn's `model=` the session cannot run; none: only the `provider/model` form is checked.
    pub model_check: Option<ModelCheck>,
    /// Runs contract `run` checks (the session's runs them under its sandbox).
    pub check_runner: CheckRunner,
    /// The session's environment, for the git commands of contract baselines (empty: git is looked up on
    /// `/usr/local/bin:/usr/bin:/bin` with an otherwise empty environment, never this process's).
    pub command_env: Vec<(String, String)>,
    /// Told how each worktree child's merge ended (`onMerge`: the session's counters); it never affects the merge.
    pub on_merge: Option<MergeObserver>,
}

impl DelegationOptions {
    /// Defaults: in-memory journal, in-cell collect everywhere, depth 3, no workspace, session "session", a zero
    /// clock, no default worktree setup, contract checks run unsandboxed ([`plain_check_runner`]).
    pub fn new(runner: ChildRunner) -> Self {
        DelegationOptions {
            store: Box::new(MemoryDocumentStore::default()),
            collect: Arc::new(|_: &str| CollectMode::InCell),
            runner,
            spawn_depth: DEFAULT_SPAWN_DEPTH,
            now: Box::new(|| 0.0),
            workspace: None,
            session_id: "session".into(),
            worktree_setup: Arc::new(|| None),
            keep_worktrees: false,
            model_check: None,
            check_runner: plain_check_runner(),
            command_env: Vec::new(),
            on_merge: None,
        }
    }
}

struct Task {
    id: String,
    parent_id: Option<String>,
    record: Value,
    outcome: Option<Result<Value, String>>,
    spawn_depth: u32,
    lane_name: Option<String>,
    /// The `name` and `model` it was spawned with (for the TUI's views: [`Delegation::subagent_views`]).
    name: Option<String>,
    model: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    verdict: Option<Value>,
    verdict_rejections: u32,
    verdict_problems: Vec<String>,
    verdict_invalid: bool,
    waiters: usize,
    signal: Signal,
    /// A terminal result is being finalized (worktree commit) before it is committed.
    finishing: bool,
    worktree: Option<WorktreeRecord>,
    /// The worktree's files when the child started (after setup), and the parent's tree then.
    worktree_baseline: Option<WorkspaceSnapshot>,
    outside_before: Option<WorkspaceSnapshot>,
    /// The child's working directory when it started (its verdict check's "before").
    before: Option<WorkspaceSnapshot>,
    merge_status: Option<String>,
    /// `rlm.spawn(role=, contract=, hop=)`: what the host checks when the child ends (see [`crate::spawn_contract`]).
    contract: Option<Arc<SpawnContract>>,
    /// The Git baseline of its working directory when it started (a contract with workspace checks), kept with the
    /// contract so a restarted host can still compare (see [`crate::git_baseline`]).
    git_baseline: Option<GitBaseline>,
}

struct State {
    journal: TaskJournal,
    tasks: Vec<Task>,
    lane_tasks: HashMap<String, String>,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    runner: ChildRunner,
    collect: LaneCollect,
    events: Mutex<Sender<DetachedEnd>>,
    spawn_depth: u32,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
    workspace: Option<PathBuf>,
    session_id: String,
    worktree_setup: WorktreeSetupSource,
    keep_worktrees: bool,
    model_check: Option<ModelCheck>,
    check_runner: CheckRunner,
    command_env: Vec<(String, String)>,
    on_merge: Option<MergeObserver>,
    /// `rlm.merge` calls run one at a time: each writes into a working tree.
    merging: Mutex<()>,
    /// The limit text of a child's root once its tree is spent (see [`Delegation::set_limit_reason`]).
    limit_reason: Mutex<Option<LimitReason>>,
    /// Where a forked child starts (see [`Delegation::set_fork_point`]); none: `fork=True` is refused.
    fork_point: Mutex<Option<crate::fork::ForkPointHook>>,
    /// The model of spawns that name none (see [`Delegation::set_child_model`]).
    child_model: Mutex<ChildModelSource>,
    /// Sees every subagent's terminal record (`onTaskEnd`).
    on_task_end: Mutex<Option<TaskEndObserver>>,
    /// Sees every admitted subagent before it starts (see [`Delegation::on_spawn`]).
    on_spawn: Mutex<Option<SpawnObserver>>,
    /// See [`Delegation::set_root_spawn`], [`Delegation::set_root_finish`].
    root: Mutex<RootOptions>,
}

/// Where a root that is itself a subagent (an `ultron mcp --child` server) sends its `rlm.finish` verdict
/// (`rootFinish`): the payload in, its parent's answer out.
pub type RootFinish = Arc<dyn Fn(Map<String, Value>) -> Result<Value, String> + Send + Sync>;

/// The root of this host when it is itself a subagent (`rootSpawn`, `rootFinish`).
#[derive(Clone, Default)]
struct RootOptions {
    /// Nesting levels above the root.
    level: u32,
    /// How many more levels the root may create (`None`: unlimited).
    allowance: Option<i64>,
    finish: Option<RootFinish>,
}

/// The model of spawns that name none, read at each spawn (TS `childModel` as a function: a changed setting
/// applies to the next subagent).
pub type ChildModelSource = Arc<dyn Fn() -> Option<String> + Send + Sync>;
/// The default worktree setup, read at each worktree spawn (TS `worktreeSetup`).
pub type WorktreeSetupSource = Arc<dyn Fn() -> Option<WorktreeSetup> + Send + Sync>;

/// Sees how a worktree child's merge ended: its task id and merge status (`merged`, `conflict`, `empty`, ...).
pub type MergeObserver = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Checks a well-formed `provider/model` a spawn names; an error refuses the spawn with that message.
pub type ModelCheck = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// `worktreeChanges`: Git's changes in a worktree minus files unchanged since the child started (its setup command's
/// output); `None` lets the commit take every change.
fn worktree_changes(record: &WorktreeRecord, baseline: Option<&WorkspaceSnapshot>) -> Option<Vec<String>> {
    let baseline = baseline.filter(|b| b.complete)?;
    let now = snapshot_workspace(&baseline.root);
    if !now.complete {
        return None;
    }
    let status = wt::list_dirty(Path::new(&record.path))?;
    let prefix = wt::relative_inside(Path::new(&record.path), &baseline.root).unwrap_or_default();
    Some(
        status
            .into_iter()
            .filter(|path| {
                let inside = if prefix.is_empty() {
                    Some(path.as_str())
                } else {
                    path.strip_prefix(&format!("{prefix}/"))
                };
                // Outside the pictured directory nothing is known: keep it.
                inside.is_none_or(|inside| baseline.entries.get(inside) != now.entries.get(inside))
            })
            .collect(),
    )
}

/// The delegation host; clones share it.
#[derive(Clone)]
pub struct Delegation {
    inner: Arc<Inner>,
}

/// `rlm.collect(timeout_ms=...)`: absent, null or 0 waits without a bound; a positive number bounds the wait.
fn collect_timeout(value: Option<&Value>) -> Result<Option<Duration>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(ms) if ms.is_finite() && ms >= 0.0 => Ok((ms > 0.0).then(|| Duration::from_millis(ms.ceil() as u64))),
            _ => Err("rlm.collect timeout_ms must be a nonnegative number of milliseconds".into()),
        },
        Some(_) => Err("rlm.collect timeout_ms must be a nonnegative number of milliseconds".into()),
    }
}

/// `worktreeSetupOption`: a spawn's `worktree_setup=`, checked.
fn check_worktree_setup(value: Option<&Value>) -> Result<Option<WorktreeSetup>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let setup = value.as_object().ok_or("agent input must be a JSON object")?;
    jsrt::payload::fields(setup, &["link", "copy", "command", "timeout_ms"])?;
    for name in ["link", "copy"] {
        let Some(item) = setup.get(name).filter(|v| !v.is_null()) else {
            continue;
        };
        let ok = item
            .as_array()
            .is_some_and(|paths| paths.iter().all(|p| p.as_str().is_some_and(|s| !s.trim().is_empty())));
        if !ok {
            return Err(format!("worktree_setup.{name} must be a list of relative paths"));
        }
    }
    if setup.get("command").is_some_and(|c| !c.is_null() && !c.is_string()) {
        return Err("worktree_setup.command must be a string".into());
    }
    if setup
        .get("timeout_ms")
        .is_some_and(|t| number::safe_integer(t).is_none_or(|n| n < 1))
    {
        return Err("worktree_setup.timeout_ms must be a positive integer".into());
    }
    let paths = |name: &str| {
        setup
            .get(name)
            .filter(|v| !v.is_null())
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
    };
    Ok(Some(WorktreeSetup {
        link: paths("link"),
        copy: paths("copy"),
        command: setup
            .get("command")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(String::from),
        timeout_ms: setup.get("timeout_ms").and_then(Value::as_u64),
    }))
}

/// `worktreeMode`: off, on, or "auto" (falls back to the shared tree outside Git).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorktreeMode {
    Off,
    On,
    Auto,
}

/// The setting's setup, overridden key by key by the spawn's.
fn merged_setup(default: Option<&WorktreeSetup>, spawn: Option<&WorktreeSetup>) -> WorktreeSetup {
    let base = default.cloned().unwrap_or_default();
    let Some(spawn) = spawn else {
        return base;
    };
    WorktreeSetup {
        link: spawn.link.clone().or(base.link),
        copy: spawn.copy.clone().or(base.copy),
        command: spawn.command.clone().or(base.command),
        timeout_ms: spawn.timeout_ms.or(base.timeout_ms),
    }
}

/// `publicRecord`: id, key, fingerprint, definition, state, result?, parentId?.
fn public_record(record: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "key", "fingerprint", "definition", "state", "result", "parentId"] {
        if let Some(value) = record.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

fn public_task(task: &Task) -> Value {
    let mut out = json!({ "id": task.id, "definition": task.record["definition"], "state": task.record["state"] });
    if let Some(parent) = &task.parent_id {
        out["parentId"] = json!(parent);
    }
    if let Some(Ok(result)) = &task.outcome {
        out["result"] = result.clone();
    }
    out
}

impl State {
    fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
    fn is_child(&self, id: &str) -> bool {
        self.task(id).is_some_and(|t| t.record["definition"] == CHILD)
    }
    /// The chain of task ids from `id` up through its ancestors.
    fn line(&self, id: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = id.map(String::from);
        while let Some(id) = current {
            current = self.task(&id).and_then(|t| t.parent_id.clone());
            out.push(id);
        }
        out
    }
    /// The nearest worktree of a task and its ancestors: a child without its own works in its parent's.
    fn worktree_of(&self, id: Option<&str>) -> Option<&WorktreeRecord> {
        self.line(id)
            .iter()
            .find_map(|id| self.task(id).and_then(|t| t.worktree.as_ref()))
    }
    /// The directory a task works in: its (or an ancestor's) worktree, else the shared workspace.
    fn workspace_of(&self, id: Option<&str>, shared: Option<&Path>) -> Option<PathBuf> {
        self.worktree_of(id)
            .map(|w| PathBuf::from(&w.cwd))
            .or_else(|| shared.map(Path::to_path_buf))
    }
    /// A child lane sees only its own task and descendants; the root sees everything.
    fn visible(&self, id: &str, parent: Option<&str>) -> bool {
        parent.is_none_or(|parent| self.line(Some(id)).iter().any(|a| a == parent))
    }
}

impl Delegation {
    pub fn new(options: DelegationOptions) -> (Self, Receiver<DetachedEnd>) {
        let (sender, receiver) = channel();
        let host = Delegation {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    journal: TaskJournal::new(options.store),
                    tasks: Vec::new(),
                    lane_tasks: HashMap::new(),
                    closed: false,
                }),
                changed: Condvar::new(),
                runner: options.runner,
                collect: options.collect,
                events: Mutex::new(sender),
                spawn_depth: options.spawn_depth,
                now: options.now,
                workspace: options.workspace,
                session_id: options.session_id,
                worktree_setup: options.worktree_setup,
                keep_worktrees: options.keep_worktrees,
                model_check: options.model_check,
                check_runner: options.check_runner,
                command_env: options.command_env,
                on_merge: options.on_merge,
                merging: Mutex::new(()),
                limit_reason: Mutex::new(None),
                fork_point: Mutex::new(None),
                child_model: Mutex::new(Arc::new(|| None)),
                on_task_end: Mutex::new(None),
                on_spawn: Mutex::new(None),
                root: Mutex::new(RootOptions::default()),
            }),
        };
        (host, receiver)
    }

    /// The collect mode of `lane`.
    pub fn collect_mode(&self, lane: &str) -> CollectMode {
        (self.inner.collect)(lane)
    }

    /// `withLimitReason`: a child that failed once its root's tree ran out of turns, tokens or cost (its run was
    /// stopped by the budget gate) fails with the limit text rather than whatever the stopped run left behind.
    /// `reason(task_id)` is that text while the child's root is spent.
    pub fn set_limit_reason(&self, reason: LimitReason) {
        *self.inner.limit_reason.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    }

    fn with_limit_reason(&self, id: &str, result: Value) -> Value {
        if result["status"] != "failed" {
            return result;
        }
        let hook = self
            .inner
            .limit_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match hook.and_then(|reason| reason(id)) {
            Some(reason) if result["error"] != reason.as_str() => {
                let mut result = result;
                result["error"] = json!(reason);
                result
            }
            _ => result,
        }
    }

    /// Lets `rlm.spawn(fork=True)` start a child on its caller's conversation: `hook` names where (the conversation
    /// so far, before the call still running), which the runner receives as [`ChildRun::fork_at`].
    pub fn set_fork_point(&self, hook: crate::fork::ForkPointHook) {
        *self.inner.fork_point.lock().unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Read the model of spawns that name none from `source` at each spawn (TS `childModel` as a function: a
    /// changed setting applies to the next subagent); by default they name none.
    pub fn set_child_model(&self, source: ChildModelSource) {
        *self.inner.child_model.lock().unwrap_or_else(PoisonError::into_inner) = source;
    }

    fn child_model(&self) -> Option<String> {
        let source = self
            .inner
            .child_model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        source()
    }

    /// Startup pruning (native-host.ts `loadTasks`): the worktrees of sessions that ended without cleaning up (a
    /// crash) are removed, their work kept on its branch; none with `ULTRON_KEEP_WORKTREES` or without a workspace,
    /// and never this session's own (see [`wt::prune_orphan_worktrees`]).
    pub fn prune_orphan_worktrees(&self) -> wt::PruneReport {
        match self.inner.workspace.as_deref().filter(|_| !self.inner.keep_worktrees) {
            Some(workspace) => wt::prune_orphan_worktrees(workspace, &self.inner.session_id),
            None => wt::PruneReport::default(),
        }
    }

    /// `rootSpawn`: this host's root is itself a subagent, `level` levels deep, that may create `allowance` more
    /// levels (`None`: unlimited). Nesting limits and children's levels count from it.
    pub fn set_root_spawn(&self, level: u32, allowance: Option<i64>) {
        let mut root = self.inner.root.lock().unwrap_or_else(PoisonError::into_inner);
        root.level = level;
        root.allowance = allowance;
    }

    /// `rootFinish`: the root's own `rlm.finish` goes to `finish` (its parent's host) instead of being refused.
    pub fn set_root_finish(&self, finish: RootFinish) {
        self.inner.root.lock().unwrap_or_else(PoisonError::into_inner).finish = Some(finish);
    }

    /// Who sees each subagent's end (`onTaskEnd(publicRecord(task), info)`): its terminal journal record (id, key,
    /// definition, state, result, parentId?). Observers never affect the result.
    pub fn on_task_end(&self, observer: TaskEndObserver) {
        *self.inner.on_task_end.lock().unwrap_or_else(PoisonError::into_inner) = Some(observer);
    }

    /// Tell `observer` of each subagent `rlm.spawn` admits, on the spawning request's thread before the child starts
    /// (an external root charges the child's work to the turn that spawned it).
    pub fn on_spawn(&self, observer: SpawnObserver) {
        *self.inner.on_spawn.lock().unwrap_or_else(PoisonError::into_inner) = Some(observer);
    }

    /// `merged(task, status)`: keep how a child's merge ended and tell the observer.
    fn merged(&self, id: &str, status: &str) {
        if let Some(observer) = &self.inner.on_merge {
            observer(id, status);
        }
    }

    /// The kernel host handler for `lane` ("main" for the root; a child's `lane_name`).
    pub fn lane_handler(&self, lane: &str) -> impl HostHandler + use<> {
        let (host, lane) = (self.clone(), lane.to_string());
        move |kind: &str, payload: Map<String, Value>, signal: &Signal| {
            let cell = CellContext {
                cancel: Some(signal.clone()),
                park: None,
            };
            host.handle_with(kind, Value::Object(payload), &lane, &cell)
        }
    }

    /// The host handler for one cell of `lane`: its waits end with `signal` (instead of the kernel's request signal)
    /// and report themselves through `park`.
    pub fn cell_handler(&self, lane: &str, signal: Signal, park: Park) -> impl HostHandler + use<> {
        let (host, lane) = (self.clone(), lane.to_string());
        let cell = CellContext {
            cancel: Some(signal),
            park: Some(park),
        };
        move |kind: &str, payload: Map<String, Value>, _signal: &Signal| {
            host.handle_with(kind, Value::Object(payload), &lane, &cell)
        }
    }

    /// The host requests [`Delegation::handle_with`] answers.
    pub fn serves(kind: &str) -> bool {
        matches!(
            kind,
            "rlm.spawn"
                | "rlm.finish"
                | "rlm.list_subagents"
                | "rlm.collect"
                | "rlm.results"
                | "rlm.delete_subagent"
                | "rlm.merge"
        ) || kind.starts_with("rlm.worktrees.")
    }

    /// Serve one host request from `lane`.
    pub fn handle(&self, kind: &str, payload: Value, lane: &str) -> Result<Value, String> {
        self.handle_with(kind, payload, lane, &CellContext::default())
    }

    /// Serve one host request from `lane`, made by `cell`.
    pub fn handle_with(&self, kind: &str, payload: Value, lane: &str, cell: &CellContext) -> Result<Value, String> {
        let cancel = cell.cancel.as_ref();
        let Value::Object(payload) = payload else {
            return Err("Host payload must be a JSON object".into());
        };
        let parent = {
            let mut state = sync::lock(&self.inner.state);
            if state.closed {
                return Err("Ultron task host is closed".into());
            }
            state.journal.list()?;
            state.lane_tasks.get(lane).cloned()
        };
        let parent = parent.as_deref();
        match kind {
            "rlm.spawn" => self.spawn(&payload, parent, lane),
            "rlm.finish" => self.finish_request(payload, parent),
            "rlm.list_subagents" => {
                jsrt::payload::fields(&payload, &[])?;
                let state = sync::lock(&self.inner.state);
                let subagents: Vec<Value> = state
                    .tasks
                    .iter()
                    .filter(|t| t.record["definition"] == CHILD && Some(t.id.as_str()) != parent)
                    .filter(|t| state.visible(&t.id, parent))
                    .map(public_task)
                    .collect();
                Ok(json!({ "subagents": subagents }))
            }
            "rlm.collect" | "rlm.results" => {
                let ids = self.selected(&payload, parent)?;
                let waits = kind == "rlm.collect" && self.collect_mode(lane) == CollectMode::InCell;
                if !waits {
                    let running = self.unfinished(&ids);
                    if kind == "rlm.collect" && !running.is_empty() {
                        return Err(format!(
                            "rlm.collect: {} subagent(s) still running ({}). The host waits for them, not this cell: do other work, or end your turn; each end arrives as a child_done event, then collect again.",
                            running.len(),
                            running.join(", ")
                        ));
                    }
                    let mut out = self.collected(self.wait_until(&ids, Some(Duration::ZERO), None))?;
                    if kind == "rlm.results" {
                        out["running"] = json!(running);
                    }
                    return Ok(out);
                }
                // `timeout_ms` (0 or absent: no bound) ends the wait with the children still running named.
                let timeout = collect_timeout(payload.get("timeout_ms"))?;
                // An in-cell wait is never parked: the cell is not detached, so its result reaches the model.
                let outcomes = self.wait_until(&ids, timeout, cancel);
                if outcomes.len() < ids.len() {
                    if let Some(reason) = cancel.and_then(Signal::reason) {
                        return Err(format!("rlm.collect was cancelled: {reason}"));
                    }
                    if let Some(timeout) = timeout.filter(|_| !cancel.is_some_and(Signal::is_aborted)) {
                        let running = self.unfinished(&ids);
                        return Err(format!(
                            "rlm.collect: timed out after {} ms with {} subagent(s) still running ({}); they keep running. `await rlm.results()` returns the results of those that ended and the ids still running; collect again to wait more.",
                            timeout.as_millis(),
                            running.len(),
                            running.join(", ")
                        ));
                    }
                    return Err("rlm.collect was cancelled".into());
                }
                self.collected(outcomes)
            }
            "rlm.delete_subagent" => {
                jsrt::payload::fields(&payload, &["selector"])?;
                let selector = nonempty_string(payload.get("selector"), "selector")?;
                let known = {
                    let state = sync::lock(&self.inner.state);
                    state.is_child(&selector) && state.visible(&selector, parent)
                };
                if !known {
                    return Err("Unknown RLM child".into());
                }
                let result = self.cancel(&selector, "RLM child deleted")?;
                Ok(json!({ "deleted": result["status"] == "cancelled" }))
            }
            "rlm.merge" => self.merge_request(&payload, parent, lane, cell),
            "rlm.worktrees.list" => {
                jsrt::payload::fields(&payload, &["all"])?;
                let shared = self.inner.workspace.as_deref();
                let from = sync::lock(&self.inner.state)
                    .workspace_of(parent, shared)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_default();
                let state = sync::lock(&self.inner.state);
                let worktrees: Vec<Value> = wt::list_records(&from)
                    .into_iter()
                    .filter(|r| payload.get("all") == Some(&Value::Bool(true)) || r.session_id == self.inner.session_id)
                    .map(|r| {
                        let running = r
                            .task_id
                            .as_deref()
                            .and_then(|t| state.task(t))
                            .is_some_and(|t| t.outcome.is_none());
                        json!({ "id": r.id, "task": r.task_id, "name": r.name, "branch": r.branch, "path": r.path,
                                "state": r.state, "commit": r.commit.clone().flatten(), "running": running,
                                "session": r.session_id })
                    })
                    .collect();
                Ok(json!({ "worktrees": worktrees }))
            }
            "rlm.worktrees.cleanup" => {
                jsrt::payload::fields(&payload, &["branches"])?;
                let workspace = sync::lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
                let Some(workspace) = workspace else {
                    return Ok(json!({ "removed": [], "kept_branches": [], "running": [] }));
                };
                let running = |record: &WorktreeRecord| {
                    let state = sync::lock(&self.inner.state);
                    record
                        .task_id
                        .as_deref()
                        .and_then(|t| state.task(t))
                        .is_some_and(|t| t.outcome.is_none())
                };
                let branches = payload.get("branches") == Some(&Value::Bool(true));
                let session = self.inner.session_id.clone();
                let records: Vec<WorktreeRecord> = wt::list_records(&workspace)
                    .into_iter()
                    .filter(|r| r.session_id == session)
                    .collect();
                let select = |r: &WorktreeRecord| r.session_id == session && !running(r);
                let own = wt::prune_worktrees(
                    &workspace,
                    &wt::PruneOptions {
                        select: Some(&select),
                        force: true,
                        delete_branches: branches,
                    },
                );
                let orphans = wt::prune_worktrees(
                    &workspace,
                    &wt::PruneOptions {
                        select: None,
                        force: false,
                        delete_branches: branches,
                    },
                );
                let removed = [own.removed, orphans.removed].concat();
                let kept = [own.kept_branches, orphans.kept_branches].concat();
                let still: Vec<String> = records
                    .iter()
                    .filter(|r| running(r))
                    .map(|r| r.branch.clone())
                    .collect();
                Ok(json!({ "removed": removed, "kept_branches": kept, "running": still }))
            }
            other => Err(format!("Unknown host request: {other}")),
        }
    }

    /// `rlm.merge`: the children end first (a host-side wait, cancelled with the cell), then each child's branch is
    /// merged, in the given order and one merge call at a time, into the caller's working tree.
    fn merge_request(
        &self,
        payload: &Map<String, Value>,
        parent: Option<&str>,
        lane: &str,
        cell: &CellContext,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["selectors", "on_conflict", "include_failed"])?;
        let on_conflict = match payload.get("on_conflict") {
            None => "stop",
            Some(Value::String(s)) if ["stop", "markers", "skip"].contains(&s.as_str()) => s.as_str(),
            Some(_) => return Err("on_conflict must be \"stop\", \"skip\" or \"markers\"".into()),
        };
        let selectors = match payload.get("selectors").and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .map(|s| nonempty_string(Some(s), "selector"))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let children: Vec<String> = {
            let state = sync::lock(&self.inner.state);
            if selectors.is_empty() {
                // Default: the caller's own worktree children, in the order they were spawned.
                state
                    .tasks
                    .iter()
                    .filter(|t| t.record["definition"] == CHILD && t.parent_id.as_deref() == parent)
                    .filter(|t| {
                        t.worktree.is_some() || matches!(&t.outcome, Some(Ok(result)) if !result["worktree"].is_null())
                    })
                    .map(|t| t.id.clone())
                    .collect()
            } else {
                for id in &selectors {
                    if !state.is_child(id) || Some(id.as_str()) == parent || !state.visible(id, parent) {
                        return Err(format!("Unknown RLM child: {id}"));
                    }
                }
                selectors.clone()
            }
        };
        if (1..children.len()).any(|i| children[..i].contains(&children[i])) {
            return Err("rlm.merge: a child is listed twice".into());
        }
        // Every child ends first: parked (the lane detaches the cell) unless the lane waits in its cell.
        let running = self.unfinished(&children);
        let park = cell
            .park
            .as_ref()
            .filter(|_| !running.is_empty() && self.collect_mode(lane) != CollectMode::InCell);
        if let Some(park) = park {
            park(true);
        }
        let ended = self.wait_until(&children, None, cell.cancel.as_ref());
        if let Some(park) = park {
            park(false);
        }
        if ended.len() < children.len() {
            return Err(cell
                .cancel
                .as_ref()
                .and_then(Signal::reason)
                .map_or("rlm.merge was cancelled".into(), |r| {
                    format!("rlm.merge was cancelled: {r}")
                }));
        }
        let target = sync::lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
        let Some(target) = target else {
            return Err("rlm.merge needs a working directory, and this session has none".into());
        };
        let _serial = self.inner.merging.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.merge_children(
            &children,
            &target,
            on_conflict,
            payload.get("include_failed") == Some(&Value::Bool(true)),
        ))
    }

    /// `mergeChildren`: the first conflict stops the rest (`skip` goes on); nothing of a conflicting child is written
    /// unless `markers`. A merged or empty child's worktree and branch are removed; a conflicting one is kept.
    fn merge_children(&self, children: &[String], target: &Path, on_conflict: &str, include_failed: bool) -> Value {
        let mut results: Vec<Value> = Vec::new();
        let mut stopped_at: Option<String> = None;
        for id in children {
            let (result, record, merge_status) = {
                let state = sync::lock(&self.inner.state);
                let task = state.task(id);
                (
                    task.and_then(|t| t.outcome.clone()).and_then(Result::ok),
                    task.and_then(|t| t.worktree.clone()),
                    task.and_then(|t| t.merge_status.clone()),
                )
            };
            let result = result.unwrap_or(Value::Null);
            let info = &result["worktree"];
            let name = record
                .as_ref()
                .map(|r| r.name.clone())
                .or_else(|| {
                    info["branch"]
                        .as_str()
                        .and_then(|b| b.rsplit('/').next())
                        .map(String::from)
                })
                .unwrap_or_else(|| id.clone());
            let mut base = json!({ "id": id, "name": name });
            if info.is_object() {
                base["branch"] = info["branch"].clone();
            }
            let with = |extra: Value| {
                let mut item = base.clone();
                if let (Some(item), Value::Object(extra)) = (item.as_object_mut(), extra) {
                    item.extend(extra);
                }
                item
            };
            if let Some(at) = &stopped_at {
                results.push(with(
                    json!({ "status": "not_attempted", "reason": format!("stopped at the conflict of {at}") }),
                ));
                continue;
            }
            if !info.is_object() {
                results.push(with(json!({ "status": "no_worktree",
                    "reason": "it ran in the shared tree: its changes are already there" })));
                continue;
            }
            if merge_status.as_deref() == Some("merged") {
                results.push(with(
                    json!({ "status": "merged", "files": info["changed_files"], "note": "already merged" }),
                ));
                continue;
            }
            let verdict = &result["verdict"];
            let passed = result["status"] == "succeeded" && (!verdict.is_object() || verdict["status"] == "passed");
            if !passed && !include_failed {
                let status = result["status"].as_str().unwrap_or("unknown");
                let verdict_note = verdict["status"]
                    .as_str()
                    .map_or(String::new(), |v| format!(", verdict {v}"));
                let mut item = with(json!({ "status": "skipped",
                    "reason": format!("the child did not succeed ({status}{verdict_note}); include_failed=True merges it anyway") }));
                if !info["commit"].is_null() {
                    item["commit"] = info["commit"].clone();
                }
                results.push(item);
                continue;
            }
            if let Some(error) = info["error"].as_str() {
                results.push(with(json!({ "status": "failed", "reason": error })));
                stopped_at = (on_conflict != "skip").then(|| id.clone());
                continue;
            }
            let source = wt::MergeSource {
                name: name.clone(),
                branch: info["branch"].as_str().unwrap_or_default().to_string(),
                base: info["base"].as_str().unwrap_or_default().to_string(),
                commit: info["commit"].as_str().map(String::from),
            };
            let outcome = match wt::merge_child_worktree(target, &source, on_conflict == "markers") {
                Ok(outcome) => outcome,
                Err(error) => {
                    results.push(with(json!({ "status": "failed", "reason": error.message })));
                    if on_conflict != "skip" {
                        stopped_at = Some(id.clone());
                    }
                    continue;
                }
            };
            let record = record.or_else(|| {
                wt::list_records(target)
                    .into_iter()
                    .find(|r| r.task_id.as_deref() == Some(id))
            });
            let status = outcome.status();
            {
                let mut state = sync::lock(&self.inner.state);
                if let Some(task) = state.task_mut(id) {
                    task.merge_status = Some(status.to_string());
                }
            }
            self.merged(id, status);
            match outcome {
                wt::MergeOutcome::Conflict {
                    files,
                    conflicts,
                    markers,
                    message,
                } => {
                    let record_error = record.and_then(|mut record| {
                        record.state = "conflict".into();
                        self.store_record(id, record).err()
                    });
                    let text = if markers {
                        format!(
                            "conflict markers written in {}: resolve them, or restore those files",
                            files.join(", ")
                        )
                    } else {
                        format!(
                            "nothing of {name} was written: {}. Its branch {} keeps the work (worktree {})",
                            if message.is_empty() {
                                "the merge conflicts"
                            } else {
                                message.as_str()
                            },
                            source.branch,
                            info["path"].as_str().unwrap_or_default()
                        )
                    };
                    let mut item = with(json!({ "status": "conflict", "files": files,
                        "conflicts": conflicts.iter().map(wt::ConflictFile::to_json).collect::<Vec<_>>(),
                        "markers": markers, "message": text, "commit": info["commit"] }));
                    if let Some(error) = record_error {
                        item["record_error"] = error.into();
                    }
                    results.push(item);
                    if on_conflict != "skip" {
                        stopped_at = Some(id.clone());
                    }
                }
                other => {
                    // Merged or empty: its worktree and branch have done their job.
                    let record_error = record.and_then(|mut record| {
                        if !self.inner.keep_worktrees {
                            wt::remove_child_worktree(&record, true);
                            return None;
                        }
                        record.state = "merged".into();
                        self.store_record(id, record).err()
                    });
                    let mut item = match other {
                        wt::MergeOutcome::Merged { files, diffstat } => {
                            with(json!({ "status": "merged", "files": files, "diffstat": diffstat }))
                        }
                        _ => with(json!({ "status": "empty" })),
                    };
                    if let Some(error) = record_error {
                        item["record_error"] = error.into();
                    }
                    results.push(item);
                }
            }
        }
        let ok = results
            .iter()
            .all(|item| ["merged", "empty", "no_worktree"].contains(&item["status"].as_str().unwrap_or("")));
        let merged: Vec<Value> = results
            .iter()
            .filter(|item| item["status"] == "merged")
            .map(|item| item["id"].clone())
            .collect();
        let mut out = json!({ "ok": ok, "merged": merged });
        if let Some(at) = stopped_at {
            out["stopped_at"] = json!(at);
        }
        out["results"] = Value::from(results);
        out["note"] = json!(
            "Merged changes are uncommitted modifications in your working tree (new files untracked); review and commit them yourself."
        );
        out
    }

    /// Saves the task's worktree record to disk and keeps it as the task's record; the save's error, if any.
    fn store_record(&self, id: &str, record: WorktreeRecord) -> Result<(), String> {
        let saved = wt::save_record(&record).map_err(|error| error.message);
        let mut state = sync::lock(&self.inner.state);
        if let Some(task) = state.task_mut(id)
            && task.worktree.is_some()
        {
            task.worktree = Some(record);
        }
        saved
    }

    fn spawn(&self, payload: &Map<String, Value>, parent: Option<&str>, lane: &str) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["prompt", "kwargs"])?;
        let limit = self.inner.spawn_depth;
        let root = self.inner.root.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let (depth, allowance) = {
            let state = sync::lock(&self.inner.state);
            let children: Vec<String> = state.line(parent).into_iter().filter(|id| state.is_child(id)).collect();
            let allowance = children
                .first()
                .and_then(|id| state.task(id))
                .map_or(root.allowance.unwrap_or(i64::MAX), |spawner| {
                    i64::from(spawner.spawn_depth)
                });
            (i64::from(root.level) + children.len() as i64, allowance)
        };
        if limit > 0 && depth >= i64::from(limit) {
            return Err(format!(
                "rlm.spawn refused: subagents nest at most {limit} level(s) deep (ULTRON_SPAWN_DEPTH). Do this part yourself: narrow with code, read the candidates, and use rlm.map for passages you cannot settle."
            ));
        }
        if allowance < 1 {
            return Err("rlm.spawn refused: this subagent was started with depth=0, so it does its brief itself. A parent that wants nested delegation passes rlm.spawn(brief, name=..., depth=N).".into());
        }
        let prompt = nonempty_string(payload.get("prompt"), "prompt")?;
        let empty = Map::new();
        let kwargs = match payload.get("kwargs") {
            None => &empty,
            Some(Value::Object(map)) => map,
            Some(_) => return Err("agent input must be a JSON object".into()),
        };
        jsrt::payload::fields(
            kwargs,
            &[
                "name",
                "model",
                "thinking",
                "timeout_ms",
                "depth",
                "worktree",
                "worktree_setup",
                "fork",
                "role",
                "contract",
                "hop",
            ],
        )?;
        let contract = SpawnContract::parse(kwargs)?.map(Arc::new);
        let fork = match kwargs.get("fork") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => return Err("fork must be True or False".into()),
        };
        let fork_hook = self
            .inner
            .fork_point
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if fork && fork_hook.is_none() {
            return Err("rlm.spawn fork=True is not available when subagents run as external processes".into());
        }
        let name = nonempty_string(kwargs.get("name"), "name")?;
        let mode = match kwargs.get("worktree") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => WorktreeMode::Off,
            Some(Value::Bool(true)) => WorktreeMode::On,
            Some(Value::String(s)) if s == "auto" => WorktreeMode::Auto,
            Some(_) => {
                return Err(
                    "worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)"
                        .into(),
                );
            }
        };
        let setup = check_worktree_setup(kwargs.get("worktree_setup"))?;
        if setup.is_some() && mode == WorktreeMode::Off {
            return Err("worktree_setup needs worktree=True".into());
        }
        let child_depth = match kwargs.get("depth") {
            None => 0,
            Some(value) => number::safe_integer(value)
                .filter(|n| *n >= 0)
                .ok_or("depth must be a non-negative integer")?,
        };
        let max_child_depth = (allowance.saturating_sub(1)).min(if limit > 0 {
            i64::from(limit) - depth - 1
        } else {
            i64::MAX
        });
        if child_depth > max_child_depth {
            return Err(format!(
                "rlm.spawn depth={child_depth} is too deep here: at most {max_child_depth} (ULTRON_SPAWN_DEPTH={limit} levels in all)."
            ));
        }
        // A named model must be `provider/model` and one the session can run: never silently the root's model.
        let model = match kwargs.get("model") {
            Some(value @ Value::String(_)) => {
                let model = model_scope::model_ref(value)?;
                if let Some(check) = &self.inner.model_check {
                    check(&model)?;
                }
                Some(model)
            }
            _ => self.child_model(),
        };
        // A `timeout_ms` that is not a number is the default (native-host.ts `typeof === "number"`).
        let timeout_ms = crate::timeout_ms(kwargs.get("timeout_ms").filter(|t| t.is_number()))?;
        let fork_at = match fork_hook.filter(|_| fork) {
            Some(hook) => {
                Some(hook(lane)?.ok_or("rlm.spawn fork=True: your lane has no finished conversation to fork yet")?)
            }
            None => None,
        };
        // The worktree branches from the spawner's tree as it is now, before the child is admitted.
        let worktree = self.new_worktree(mode, &name, setup.as_ref(), parent)?;
        let thinking = match kwargs.get("thinking").map(model_scope::thinking_level) {
            None => None,
            Some(Ok(level)) => Some(level),
            Some(Err(error)) => {
                if let Some(record) = &worktree {
                    wt::remove_child_worktree(record, true);
                }
                return Err(error);
            }
        };
        let mut fingerprinted = json!({
            "definition": CHILD,
            "input": { "prompt": prompt },
            "model": model,
            "timeout_ms": timeout_ms,
        });
        // A contract is part of what was asked (a spawn without one keeps the TypeScript fingerprint).
        if let Some(contract) = &contract {
            fingerprinted["contract"] = contract.describe();
        }
        let fingerprint = task_fingerprint(&fingerprinted);
        let signal = Signal::new();
        let mut record_error = None;
        let (task_id, level) = {
            let mut state = sync::lock(&self.inner.state);
            if state.closed {
                return Err("Ultron task host is closed".into());
            }
            let admitted = state
                .journal
                .admit(CHILD, &fingerprint, &jsrt::random::uuid_v4(), parent);
            let (record, _) = match admitted {
                Ok(admitted) => admitted,
                Err(error) => {
                    if let Some(record) = &worktree {
                        wt::remove_child_worktree(record, true);
                    }
                    return Err(error);
                }
            };
            let id = record["id"].as_str().unwrap_or_default().to_string();
            state.tasks.push(Task {
                id: id.clone(),
                parent_id: parent.map(String::from),
                record,
                outcome: None,
                spawn_depth: child_depth as u32,
                lane_name: None,
                name: Some(name.clone()),
                model: model.clone(),
                started_at: Some((self.inner.now)()),
                ended_at: None,
                verdict: None,
                verdict_rejections: 0,
                verdict_problems: Vec::new(),
                verdict_invalid: false,
                waiters: 0,
                signal: signal.clone(),
                finishing: false,
                worktree: worktree.clone().map(|mut record| {
                    record.task_id = Some(id.clone());
                    record_error = wt::save_record(&record).err().map(|error| error.message);
                    record
                }),
                worktree_baseline: None,
                outside_before: None,
                before: None,
                merge_status: None,
                contract: contract.clone(),
                git_baseline: None,
            });
            let level = root.level + state.line(Some(&id)).iter().filter(|a| state.is_child(a)).count() as u32;
            let running = state.journal.transition(&id, "running", None)?;
            if let Some(task) = state.task_mut(&id) {
                task.record = running;
            }
            (id, level)
        };
        let lane_name = crate::lane_names::child_lane(&task_id);
        let (cwd, env, brief) = {
            let state = sync::lock(&self.inner.state);
            let inherited = state.worktree_of(Some(&task_id));
            let note = worktree.as_ref().map(wt::worktree_brief_note);
            // The role and the checks the host will run lead the brief.
            let prompt = match &contract {
                Some(contract) => contract.prompt(&prompt),
                None => prompt,
            };
            let prompt = match &fork_at {
                Some(_) => format!("{}\n\n{prompt}", crate::fork::FORK_BRIEF_NOTE),
                None => prompt,
            };
            (
                inherited.map(|w| w.cwd.clone()),
                inherited.map(wt::worktree_env).unwrap_or_default(),
                match note {
                    Some(note) => format!("{prompt}\n\n{note}"),
                    None => prompt,
                },
            )
        };
        let run = ChildRun {
            task_id: task_id.clone(),
            name: name.clone(),
            lane_name: lane_name.clone(),
            prompt: brief,
            level,
            allowance: child_depth as u32,
            model: model.clone(),
            thinking,
            timeout_ms,
            cwd,
            env,
            fork_at: fork_at.clone(),
            signal: signal.clone(),
            host: self.clone(),
        };
        {
            let mut state = sync::lock(&self.inner.state);
            state.lane_tasks.insert(lane_name.clone(), task_id.clone());
            if let Some(task) = state.task_mut(&task_id) {
                task.lane_name = Some(lane_name);
            }
        }
        let observer = self
            .inner
            .on_spawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(observer) = observer {
            observer(&task_id, parent, lane);
        }
        let host = self.clone();
        std::thread::spawn(move || host.run_child(run));
        let host = self.clone();
        let (id, deadline) = (task_id.clone(), signal);
        std::thread::spawn(move || {
            if !deadline.wait(Duration::from_millis(timeout_ms)) {
                let _ = host.cancel(&id, &format!("Ultron task exceeded {timeout_ms}ms timeout"));
            }
        });
        let mut worktree = worktree.map_or(
            Value::Null,
            |w| json!({ "branch": w.branch, "path": w.path, "cwd": w.cwd }),
        );
        if let Some(error) = record_error {
            worktree["record_error"] = error.into();
        }
        Ok(json!({
            "rlm_child_id": task_id,
            "name": name,
            "session_dir": "",
            "model": model.unwrap_or_default(),
            "timeout_ms": timeout_ms,
            "parent_branch_anchor": fork_at.unwrap_or_default(),
            "worktree": worktree,
        }))
    }

    /// `newWorktree`: a private worktree for a child of `parent` (`worktree=True`), branched from the spawner's tree.
    fn new_worktree(
        &self,
        mode: WorktreeMode,
        name: &str,
        setup: Option<&WorktreeSetup>,
        parent: Option<&str>,
    ) -> Result<Option<WorktreeRecord>, String> {
        if mode == WorktreeMode::Off {
            return Ok(None);
        }
        let from = sync::lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
        let Some(from) = from else {
            if mode == WorktreeMode::Auto {
                return Ok(None);
            }
            return Err("worktree=True needs a working directory, and this session has none".into());
        };
        let default_setup = (self.inner.worktree_setup)();
        let setup = merged_setup(default_setup.as_ref(), setup);
        let created = wt::create_child_worktree(&wt::CreateWorktreeOptions {
            cwd: &from,
            session_id: &self.inner.session_id,
            name,
            setup: Some(&setup),
            now: (self.inner.now)(),
        });
        match created {
            Ok(record) => Ok(Some(record)),
            Err(error) if mode == WorktreeMode::Auto && error.fallback => Ok(None),
            Err(error) => Err(format!("worktree=True: {}", error.message)),
        }
    }

    fn run_child(&self, run: ChildRun) {
        let id = run.task_id.clone();
        let signal = run.signal.clone();
        let contract = sync::lock(&self.inner.state).task(&id).and_then(|t| t.contract.clone());
        let started = self.child_start(&id, &signal);
        let reply = match &started {
            Ok(()) => (self.inner.runner)(run.clone()),
            Err(error) => Err(error.clone()),
        };
        if signal.is_aborted() {
            return;
        }
        let result = self.reply_result(&id, reply);
        let result = match &contract {
            Some(contract) => match self.enforce_contract(&id, contract, Some(run), result, &signal) {
                Some(result) => result,
                None => return,
            },
            None => result,
        };
        let result = self.with_limit_reason(&id, result);
        let _ = self.end(&id, result);
    }

    /// A child run's reply as its result (a failed run is a failed, unverified result).
    fn reply_result(&self, id: &str, reply: Result<String, String>) -> Value {
        let result = match reply {
            Ok(text) => self.child_result(id, &text),
            Err(error) => Err(error),
        };
        result.unwrap_or_else(|error| json!({ "status": "failed", "error": error, "verification": "unverified" }))
    }

    /// Hold a child's `result` to its contract ([`enforce`]): a retry re-runs `run` (with no run to repeat, the
    /// failure is reported as it is), a review spawns the reviewer as a sibling of the child. `None` when the task
    /// was cancelled meanwhile.
    fn enforce_contract(
        &self,
        id: &str,
        contract: &SpawnContract,
        mut run: Option<ChildRun>,
        result: Value,
        signal: &Signal,
    ) -> Option<Value> {
        let name = run.as_ref().map_or_else(|| id.to_string(), |run| run.name.clone());
        let mut check = || {
            let report = self.check_contract(id, contract, signal);
            (!signal.is_aborted()).then_some(report)
        };
        let mut retry = |note: &str| {
            let Some(again) = run.as_mut() else {
                return Some(None);
            };
            // Again, with what failed appended to its brief, on a verdict of its own.
            if let Some(task) = sync::lock(&self.inner.state).task_mut(id) {
                task.verdict = None;
                task.verdict_rejections = 0;
                task.verdict_problems.clear();
                task.verdict_invalid = false;
            }
            again.prompt = format!("{}\n\n{note}", again.prompt);
            let reply = (self.inner.runner)(again.clone());
            if signal.is_aborted() {
                return None;
            }
            Some(Some(self.reply_result(id, reply)))
        };
        let mut review = |name: &str, prompt: &str| {
            let (parent, owner) = {
                let state = sync::lock(&self.inner.state);
                let parent = state.task(id).and_then(|t| t.parent_id.clone());
                let owner = parent
                    .as_deref()
                    .and_then(|p| state.task(p).and_then(|t| t.lane_name.clone()))
                    .unwrap_or_else(|| "main".into());
                (parent, owner)
            };
            let payload = json!({ "prompt": prompt, "kwargs": { "name": name } });
            let payload = payload.as_object().cloned().unwrap_or_default();
            self.spawn(&payload, parent.as_deref(), &owner)
        };
        let mut enforcer = Enforcer {
            check: &mut check,
            retry: &mut retry,
            review: &mut review,
        };
        enforce(contract, result, id, &name, &mut enforcer)
    }

    /// The contract of task `id` as [`SpawnContract::describe`] writes it, for its session to keep with the task
    /// (`None`: it was spawned without one).
    /// A contract with workspace checks carries the Git baseline of its start (`baseline`), when there is one.
    pub fn contract_of(&self, id: &str) -> Option<Value> {
        let state = sync::lock(&self.inner.state);
        let task = state.task(id)?;
        let mut described = task.contract.as_ref()?.describe();
        if let Some(baseline) = &task.git_baseline {
            described["baseline"] = baseline.describe();
        }
        Some(described)
    }

    /// Run a child's contract checks in its working directory (its worktree's, else the shared one), on the files
    /// as they are now, against the snapshot taken when it started.
    fn check_contract(&self, id: &str, contract: &SpawnContract, signal: &Signal) -> Value {
        let (verdict, before, baseline, workspace, env) = {
            let state = sync::lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return json!({ "verdict": "contract_failed", "checks": [], "failed": ["Unknown Ultron task"] });
            };
            (
                task.verdict.clone(),
                task.before.clone(),
                task.git_baseline.clone(),
                state.workspace_of(Some(id), self.inner.workspace.as_deref()),
                state.worktree_of(Some(id)).map(wt::worktree_env).unwrap_or_default(),
            )
        };
        // The snapshot of its start, else (a child reattached after a restart) the kept Git baseline.
        let diff = match (&before, &baseline, &workspace) {
            (Some(before), _, Some(_)) => diff_snapshots(before, &snapshot_workspace(&before.root)).ok(),
            (None, Some(baseline), Some(workspace)) => baseline.changes(workspace, &self.inner.command_env).ok(),
            _ => None,
        };
        let place = CheckPlace {
            cwd: workspace.as_deref(),
            env: &env,
            diff: diff.as_ref(),
            signal,
        };
        contract.evaluate(verdict.as_ref(), &place, &self.inner.check_runner)
    }

    /// `childStart`: a worktree child's setup command runs, then its working directory is pictured (plus the parent's
    /// tree for a worktree child, to see writes it makes outside its worktree).
    fn child_start(&self, id: &str, signal: &Signal) -> Result<(), String> {
        let (own, workspace, parent_workspace, needs_baseline) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            let shared = self.inner.workspace.as_deref();
            (
                task.worktree.clone(),
                state.workspace_of(Some(id), shared),
                state.workspace_of(task.parent_id.as_deref(), shared),
                task.contract.as_ref().is_some_and(|c| c.needs_baseline()),
            )
        };
        if let Some(record) = &own {
            wt::run_setup_command(record, Some(signal)).map_err(|e| e.message)?;
        }
        // Workspace checks keep a Git baseline too, for a host that reattaches the child after a restart.
        let git_baseline = match (&workspace, needs_baseline) {
            (Some(w), true) => GitBaseline::capture(w, &self.inner.command_env),
            _ => None,
        };
        let before = workspace.map(|w| snapshot_workspace(&w));
        let outside = match (&own, &before) {
            (Some(_), Some(_)) => parent_workspace.map(|w| snapshot_workspace(&w)),
            _ => None,
        };
        let mut state = sync::lock(&self.inner.state);
        if let Some(task) = state.task_mut(id) {
            if own.is_some() {
                task.worktree_baseline = before.clone();
                task.outside_before = outside;
            }
            task.before = before;
            task.git_baseline = git_baseline;
        }
        Ok(())
    }

    /// `childResult`: the reply, the verdict (null without one), and the check of its declared files against the
    /// files that changed in its working directory while it ran.
    fn child_result(&self, id: &str, text: &str) -> Result<Value, String> {
        let (verdict, invalid, problems, before, workspace) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            (
                task.verdict.clone(),
                task.verdict_invalid,
                task.verdict_problems.clone(),
                task.before.clone(),
                state.workspace_of(Some(id), self.inner.workspace.as_deref()),
            )
        };
        if text.trim().is_empty() && verdict.is_none() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if text.trim().is_empty() {
            verdict.as_ref().map_or(Value::Null, |v| v["summary"].clone())
        } else {
            json!(text)
        };
        let after = match (&before, &workspace) {
            (Some(_), Some(w)) => Some(snapshot_workspace(w)),
            _ => None,
        };
        let (concurrent, explained) = {
            let state = sync::lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            let others = self.concurrent_with(&state, task);
            let explained: Vec<String> = others
                .iter()
                .filter_map(|other| state.task(other))
                .filter_map(|other| match &other.outcome {
                    Some(Ok(result)) => result["verdict"]["changed_files"].as_array().cloned(),
                    _ => None,
                })
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            (others, explained)
        };
        let mut check = match (&before, &after) {
            (Some(before), Some(after)) => {
                let declared: Vec<String> = verdict
                    .as_ref()
                    .and_then(|v| v["changed_files"].as_array())
                    .map(|files| files.iter().filter_map(|f| f.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let diff = diff_snapshots(before, after)?;
                let mut check = check_files(&declared, &before.root, before, after, &diff, &explained, &concurrent);
                if verdict.is_none() {
                    check["outcome"] = json!(if invalid { "invalid" } else { "unchecked" });
                    check["reason"] = json!("the subagent ended without a verdict (rlm.finish)");
                }
                check
            }
            _ => json!({
                "outcome": if invalid { "invalid" } else { "unchecked" },
                "unobserved": [], "unreported": [], "unlisted": [],
                "concurrent": concurrent,
                "reason": "no workspace to compare",
            }),
        };
        if invalid {
            check["problems"] = json!(problems);
        }
        let outside = self.outside_writes(id);
        if !outside.is_empty() {
            check["outside"] = json!(outside);
        }
        let nothing_checked = verdict.is_none() && !invalid && before.is_none();
        let mut result = json!({ "status": "succeeded", "value": value, "verification": "unverified",
                                 "verdict": verdict.clone().unwrap_or(Value::Null) });
        if !nothing_checked {
            result["check"] = check;
        }
        if verdict.is_none() {
            result["unverified"] = json!(true);
        }
        Ok(result)
    }

    /// `outsideWrites`: files in the parent's tree that changed while a worktree child ran, minus what other tasks
    /// working there declared.
    fn outside_writes(&self, id: &str) -> Vec<String> {
        let (root, before, explained) = {
            let state = sync::lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return Vec::new();
            };
            let (Some(_), Some(before)) = (&task.worktree, &task.outside_before) else {
                return Vec::new();
            };
            let shared = self.inner.workspace.as_deref();
            let target = state.workspace_of(task.parent_id.as_deref(), shared);
            let explained: Vec<String> = state
                .tasks
                .iter()
                .filter(|other| other.id != id && state.workspace_of(Some(&other.id), shared) == target)
                .filter_map(|other| match &other.outcome {
                    Some(Ok(result)) => result["verdict"]["changed_files"].as_array().cloned(),
                    _ => other
                        .verdict
                        .as_ref()
                        .and_then(|v| v["changed_files"].as_array().cloned()),
                })
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            (before.root.clone(), before.clone(), explained)
        };
        let after = snapshot_workspace(&root);
        diff_snapshots(&before, &after)
            .map(|diff| {
                diff.changed
                    .into_iter()
                    .filter(|path| !explained.contains(path))
                    .take(100)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Other started tasks whose runs overlapped this one in the same working directory, outside its own line
    /// (ancestors and descendants).
    fn concurrent_with(&self, state: &State, task: &Task) -> Vec<String> {
        let start = task.started_at.unwrap_or(0.0);
        let end = (self.inner.now)();
        let ancestors = state.line(task.parent_id.as_deref());
        let shared = self.inner.workspace.as_deref();
        let workspace = state.workspace_of(Some(&task.id), shared);
        state
            .tasks
            .iter()
            .filter(|other| other.id != task.id && other.lane_name.is_some())
            .filter(|other| state.workspace_of(Some(&other.id), shared) == workspace)
            .filter(|other| {
                other.started_at.is_some_and(|s| s <= end) && other.ended_at.unwrap_or(f64::INFINITY) >= start
            })
            .filter(|other| !ancestors.contains(&other.id))
            .filter(|other| !state.line(other.parent_id.as_deref()).contains(&task.id))
            .map(|other| other.id.clone())
            .collect()
    }

    /// `finish`: the first terminal result wins; nothing is published until the journal commits it.
    fn end(&self, id: &str, result: Value) -> Result<Value, String> {
        {
            let mut state = sync::lock(&self.inner.state);
            loop {
                let task = state.task(id).ok_or("Unknown Ultron task")?;
                if let Some(outcome) = &task.outcome {
                    return outcome.clone();
                }
                if !task.finishing {
                    break;
                }
                state = self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.finishing = true;
            // Only the first party to end a task aborts its work, and before finalizing it, as cancel() does in TS.
            if result["status"] == "cancelled" {
                task.signal.abort(result["error"].as_str().unwrap_or("cancelled"));
            }
        }
        // A worktree child's work is committed on its branch before its result is published.
        let result = self.finalize_worktree(id, result);
        let mut ended = None;
        let event = {
            let mut state = sync::lock(&self.inner.state);
            let terminal = match result["status"].as_str() {
                Some("succeeded") => "completed",
                Some(other) => other,
                None => "failed",
            }
            .to_string();
            // A child reattached after a restart (`restore`: running again, never started by this host) is already
            // terminal in the journal, which recorded it `interrupted` when it reopened (and keeps that first
            // terminal record): its real end (a result, or a cancel) is the host's, kept in memory.
            let reattached = state
                .task(id)
                .is_some_and(|t| t.started_at.is_none() && t.lane_name.is_some() && t.record["state"] == "running");
            let committed = if reattached {
                let mut record = state.task(id).map(|t| t.record.clone()).unwrap_or_default();
                record["state"] = json!(terminal);
                record["result"] = result;
                Ok(record)
            } else {
                state.journal.transition(id, &terminal, Some(result))
            };
            let now = (self.inner.now)();
            let closed = state.closed;
            let owner_lane = {
                let parent = state.task(id).and_then(|t| t.parent_id.clone());
                parent
                    .and_then(|p| state.task(&p).and_then(|t| t.lane_name.clone()))
                    .unwrap_or_else(|| "main".into())
            };
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.ended_at.get_or_insert(now);
            task.signal.abort("ended");
            let outcome = committed.map(|record| {
                task.record = record.clone();
                record["result"].clone()
            });
            task.outcome = Some(outcome.clone());
            if outcome.is_ok() {
                ended = Some(public_record(&task.record));
            }
            self.inner.changed.notify_all();
            let event = match outcome {
                Ok(_) if !closed => Some(DetachedEnd {
                    task: task.record.clone(),
                    kind: "child_done",
                    owner_lane,
                    awaited: task.waiters > 0,
                    fetch: format!("await rlm.collect([\"{id}\"])"),
                }),
                Ok(_) => None,
                Err(error) => return Err(error),
            };
            // Queued in the critical section that ends the task: to anyone holding the state lock a child is
            // either running or has its end queued. Sent after the lock, the session's activity check could run
            // in between, see neither, and retire an unattached worker with the child's end never delivered.
            if let Some(event) = &event {
                let _ = self
                    .inner
                    .events
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .send(event.clone());
            }
            event
        };
        let observer = self
            .inner
            .on_task_end
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let (Some(observer), Some(record)) = (observer, &ended) {
            observer(record);
        }
        if let Some(event) = event {
            return Ok(event.task["result"].clone());
        }
        let state = sync::lock(&self.inner.state);
        state
            .task(id)
            .and_then(|t| t.outcome.clone())
            .unwrap_or(Err("Unknown Ultron task".into()))
    }

    /// `finalizeWorktree`: commit what a worktree child changed on its branch and add the worktree to its result. A
    /// child that changed nothing has its worktree and branch removed at once. Never fails the task: a Git problem is
    /// reported in `worktree.error` and the worktree is kept.
    fn finalize_worktree(&self, id: &str, mut result: Value) -> Value {
        let (record, verdict, baseline) = {
            let state = sync::lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return result;
            };
            (
                task.worktree.clone().filter(|w| w.state == "active"),
                task.verdict.clone(),
                task.worktree_baseline.clone(),
            )
        };
        let Some(mut record) = record else {
            return result;
        };
        let status = verdict
            .as_ref()
            .and_then(|v| v["status"].as_str())
            .or(result["status"].as_str())
            .unwrap_or("")
            .to_string();
        let summary = match verdict.as_ref().and_then(|v| v["summary"].as_str()) {
            Some(summary) => summary.to_string(),
            None => match (&result["value"], &result["error"]) {
                (Value::String(value), _) => value.clone(),
                (_, Value::String(error)) => error.clone(),
                _ => String::new(),
            },
        };
        let paths = worktree_changes(&record, baseline.as_ref());
        let message = wt::commit_message(&summary, &record.name, Some(id), &status);
        let keep = self.inner.keep_worktrees;
        let mut empty = false;
        let info = match wt::commit_child_worktree(&record, &message, paths.as_deref()) {
            Ok(commit) => {
                record.state = if commit.commit.is_some() && status != "passed" && status != "succeeded" {
                    "failed"
                } else {
                    "ended"
                }
                .into();
                record.commit = Some(commit.commit.clone());
                empty = commit.commit.is_none();
                let saved = if empty && !keep {
                    wt::remove_child_worktree(&record, true);
                    Ok(())
                } else {
                    wt::save_record(&record)
                };
                let mut info = json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                    "base": record.base, "commit": commit.commit, "changed_files": commit.changed_files,
                    "diffstat": commit.diffstat });
                if empty && !keep {
                    info["removed"] = json!(true);
                }
                if let Err(error) = saved {
                    info["record_error"] = error.message.into();
                }
                info
            }
            Err(error) => {
                record.state = "failed".into();
                let mut info = json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                    "base": record.base, "commit": null, "changed_files": [], "diffstat": "",
                    "error": format!("committing the worktree failed: {}", error.message) });
                if let Err(error) = wt::save_record(&record) {
                    info["record_error"] = error.message.into();
                }
                info
            }
        };
        {
            let mut state = sync::lock(&self.inner.state);
            if let Some(task) = state.task_mut(id) {
                task.worktree = Some(record);
                if empty {
                    task.merge_status = Some("empty".into());
                }
            }
        }
        if empty {
            self.merged(id, "empty");
        }
        result["worktree"] = info;
        result
    }

    /// `agents.inspect` / `agents.result` / `agents.cancel` of subagent `id` (TypeScript keeps `rlm-child@1` tasks in
    /// the one task journal the typed agents use): `None` when `id` is no subagent `lane` may see (the root lane
    /// sees every one, another lane its own subtree). `cell` ends a result's wait.
    pub fn agents_request(
        &self,
        kind: &str,
        id: &str,
        lane: &str,
        cell: Option<&Signal>,
    ) -> Option<Result<Value, String>> {
        let (task, live) = {
            let state = sync::lock(&self.inner.state);
            let parent = state.lane_tasks.get(lane).cloned();
            let task = state
                .task(id)
                .filter(|_| state.is_child(id) && state.visible(id, parent.as_deref()))?;
            (public_task(task), task.outcome.is_none())
        };
        Some(match kind {
            "agents.inspect" => Ok(task),
            "agents.cancel" => self
                .cancel(id, "Ultron task cancelled")
                .map(|result| json!({ "cancelled": result["status"] == "cancelled" })),
            _ if !live => Ok(task.get("result").cloned().unwrap_or(Value::Null)),
            _ => match self.wait_until(&[id.to_string()], None, cell).pop() {
                Some((_, outcome)) => outcome,
                None => Err(cell
                    .and_then(Signal::reason)
                    .unwrap_or_else(|| "the calling cell was aborted".into())),
            },
        })
    }

    /// Cancel a task and, after it, its unfinished descendants.
    pub fn cancel(&self, id: &str, reason: &str) -> Result<Value, String> {
        let own = self.end(
            id,
            json!({ "status": "cancelled", "error": reason, "verification": "unverified" }),
        );
        let children: Vec<String> = {
            let state = sync::lock(&self.inner.state);
            state
                .tasks
                .iter()
                .filter(|t| t.parent_id.as_deref() == Some(id) && t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for child in children {
            let _ = self.cancel(&child, &format!("Ancestor {id} cancelled: {reason}"));
        }
        own
    }

    /// The children a collect selects: visible to the caller, not the caller itself, in admission order.
    fn selected(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<Vec<String>, String> {
        jsrt::payload::fields(payload, &["selectors", "timeout_ms"])?;
        let selectors = match payload.get("selectors").and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .map(|s| nonempty_string(Some(s), "selector"))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let state = sync::lock(&self.inner.state);
        Ok(state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && Some(t.id.as_str()) != parent)
            .filter(|t| state.visible(&t.id, parent))
            .filter(|t| selectors.is_empty() || selectors.contains(&t.id))
            .map(|t| t.id.clone())
            .collect())
    }

    fn unfinished(&self, ids: &[String]) -> Vec<String> {
        let state = sync::lock(&self.inner.state);
        ids.iter()
            .filter(|id| state.task(id).is_some_and(|t| t.outcome.is_none()))
            .cloned()
            .collect()
    }

    fn collected(&self, outcomes: Vec<(String, Result<Value, String>)>) -> Result<Value, String> {
        let results = outcomes
            .into_iter()
            .map(|(id, outcome)| outcome.map(|result| json!({ "id": id, "result": result })))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "results": results }))
    }

    /// `rlm.list_subagents` of `lane` with what the TUI's views show besides (never given to a cell): each
    /// subagent's spawn `name` and `model`, and its `startedAt` / `endedAt` (ms since the epoch) when known.
    pub fn subagent_views(&self, lane: &str) -> Result<Value, String> {
        let mut listed = self.handle("rlm.list_subagents", json!({}), lane)?;
        let state = sync::lock(&self.inner.state);
        for view in listed["subagents"].as_array_mut().into_iter().flatten() {
            let Some(task) = view["id"].as_str().and_then(|id| state.task(id)) else {
                continue;
            };
            if let Some(name) = &task.name {
                view["name"] = json!(name);
            }
            if let Some(model) = &task.model {
                view["model"] = json!(model);
            }
            // A host without a clock (`now` 0) knows no times.
            for (key, at) in [("startedAt", task.started_at), ("endedAt", task.ended_at)] {
                if let Some(at) = at.filter(|at| *at > 0.0) {
                    view[key] = json!(at);
                }
            }
        }
        Ok(listed)
    }

    /// Unfinished children started from `lane` ("main" for the root), in admission order: what a turn scheduler
    /// waits for (with [`Delegation::wait`] or the [`DetachedEnd`] channel) after the model ends its turn.
    pub fn running(&self, lane: &str) -> Vec<String> {
        let state = sync::lock(&self.inner.state);
        let parent = state.lane_tasks.get(lane).cloned();
        state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && t.outcome.is_none() && t.parent_id == parent)
            .map(|t| t.id.clone())
            .collect()
    }

    /// Host-side wait for tasks' results (in the given order), without holding any kernel. `timeout` bounds the
    /// whole wait; a task still running when it passes is left out.
    pub fn wait(&self, ids: &[String], timeout: Option<Duration>) -> Vec<(String, Result<Value, String>)> {
        self.wait_until(ids, timeout, None)
    }

    /// [`Delegation::wait`] that also ends when `cancel` is aborted.
    pub fn wait_until(
        &self,
        ids: &[String],
        timeout: Option<Duration>,
        cancel: Option<&Signal>,
    ) -> Vec<(String, Result<Value, String>)> {
        // Wakes this wait when the signal is aborted (from a thread of its own: the aborting thread may hold the
        // state).
        let listener = cancel.and_then(|cancel| {
            let inner = self.inner.clone();
            cancel.on_abort(move |_| {
                std::thread::spawn(move || {
                    let _state = sync::lock(&inner.state);
                    inner.changed.notify_all();
                });
            })
        });
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let mut state = sync::lock(&self.inner.state);
        for id in ids {
            if let Some(task) = state.task_mut(id) {
                task.waiters += 1;
            }
        }
        loop {
            let pending = ids.iter().any(|id| state.task(id).is_some_and(|t| t.outcome.is_none()));
            let left = deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
            if !pending || left.is_some_and(|l| l.is_zero()) || cancel.is_some_and(Signal::is_aborted) {
                break;
            }
            state = match left {
                Some(left) => {
                    self.inner
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner),
            };
        }
        let mut out = Vec::new();
        for id in ids {
            if let Some(task) = state.task_mut(id) {
                task.waiters -= 1;
                if let Some(outcome) = &task.outcome {
                    out.push((id.clone(), outcome.clone()));
                }
            }
        }
        drop(state);
        if let (Some(cancel), Some(listener)) = (cancel, listener) {
            cancel.forget(listener);
        }
        out
    }

    /// Requests now waiting on a task's result.
    pub fn waiters(&self, id: &str) -> usize {
        sync::lock(&self.inner.state).task(id).map_or(0, |t| t.waiters)
    }

    /// The journal's tasks, in admission order.
    pub fn tasks(&self) -> Result<Vec<Value>, String> {
        sync::lock(&self.inner.state).journal.list()
    }

    fn finish_request(&self, mut payload: Map<String, Value>, parent: Option<&str>) -> Result<Value, String> {
        // A root that is itself a subagent (an `ultron mcp --child` server) gives its verdict to its parent's host.
        if parent.is_none() {
            let finish = self
                .inner
                .root
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .finish
                .clone();
            if let Some(finish) = finish {
                return finish(payload);
            }
        }
        let mut state = sync::lock(&self.inner.state);
        let id = parent
            .filter(|id| state.is_child(id))
            .ok_or("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply")?
            .to_string();
        let worktree = state.worktree_of(Some(&id)).cloned();
        let workspace = state
            .workspace_of(Some(&id), self.inner.workspace.as_deref())
            .map(|w| w.display().to_string());
        let task = state.task_mut(&id).ok_or("Unknown Ultron task")?;
        if task.outcome.is_some() || task.finishing {
            return Err("rlm.finish: this subagent has already ended".into());
        }
        if task.verdict_invalid {
            return Err("rlm.finish: no attempts left; end your turn, your reply is returned unverified".into());
        }
        // A worktree child may name files by their path in its parent's checkout: they mean its own copies.
        if let (Some(worktree), Some(Value::Array(files))) = (&worktree, payload.get_mut("changed_files")) {
            for file in files.iter_mut() {
                if let Value::String(path) = file {
                    *path = wt::in_worktree(path, worktree);
                }
            }
        }
        // A contract's `returns` shape is checked here, so the child can fix its outputs like any rejected verdict.
        let checked = validate_verdict(&payload, workspace.as_deref()).and_then(|verdict| {
            let problems = task
                .contract
                .as_ref()
                .map(|contract| contract.returns_problems(&verdict["outputs"]))
                .unwrap_or_default();
            if problems.is_empty() {
                Ok(verdict)
            } else {
                Err(problems)
            }
        });
        match checked {
            Ok(verdict) => {
                let status = verdict["status"].clone();
                task.verdict = Some(verdict);
                Ok(json!({ "recorded": true, "status": status,
                    "next": "End your turn now with a short reply; the host checks changed_files against the files that changed while you ran." }))
            }
            Err(problems) => {
                task.verdict_rejections += 1;
                let list = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
                task.verdict_problems = problems;
                let left = MAX_VERDICT_REJECTIONS + 1 - task.verdict_rejections.min(MAX_VERDICT_REJECTIONS + 1);
                if left > 0 || task.verdict.is_some() {
                    let tail = if task.verdict.is_some() {
                        "; your earlier verdict stands until then".to_string()
                    } else {
                        format!(" ({left} attempt{} left)", if left == 1 { "" } else { "s" })
                    };
                    return Err(format!(
                        "rlm.finish rejected:\n{list}\nFix these and call rlm.finish again{tail}."
                    ));
                }
                task.verdict_invalid = true;
                Err(format!(
                    "rlm.finish rejected:\n{list}\nNo attempts left: end your turn; your reply is returned unverified, with these problems."
                ))
            }
        }
    }

    /// Stop accepting requests and cancel every unfinished task.
    pub fn close(&self) {
        let open: Vec<String> = {
            let mut state = sync::lock(&self.inner.state);
            state.closed = true;
            state
                .tasks
                .iter()
                .filter(|t| t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for id in open {
            let _ = self.cancel(&id, "Ultron task host closed");
        }
        // This session's finished, unmerged children: the directory goes, the branch keeps the work. Conflicted and
        // failed ones stay for inspection until a later session prunes them.
        if let Some(workspace) = self.inner.workspace.as_deref().filter(|_| !self.inner.keep_worktrees) {
            let (session, pid) = (self.inner.session_id.clone(), std::process::id());
            let select = |r: &WorktreeRecord| r.session_id == session && r.owner.pid == pid && r.state == "ended";
            wt::prune_worktrees(
                workspace,
                &wt::PruneOptions {
                    select: Some(&select),
                    force: true,
                    delete_branches: false,
                },
            );
        }
    }
}
