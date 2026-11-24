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
mod merge;
mod spawn;

use crate::fork::SpawnContext;
use crate::git_baseline::GitBaseline;
use crate::spawn_contract::{CheckPlace, CheckRunner, Enforcer, SpawnContract, enforce, plain_check_runner};
use crate::task_store::TaskJournal;
use crate::verdict::{MAX_VERDICT_REJECTIONS, validate_verdict};
use crate::workspace::{WorkspaceSnapshot, check_files, diff_snapshots, snapshot_workspace};
use crate::worktrees::{self as wt, WorktreeRecord, WorktreeSetup};
use harness::task_store::task_fingerprint;
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
    /// The child's working directory (its worktree's, or `rlm.spawn(cwd=)`'s, or an ancestor's) and a worktree
    /// child's extra environment; `None` / empty: the host's.
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
    /// Its `timeout_ms` (`None`: a reattached child whose run was not kept).
    timeout_ms: Option<u64>,
    verdict: Option<Value>,
    verdict_rejections: u32,
    verdict_problems: Vec<String>,
    verdict_invalid: bool,
    waiters: usize,
    signal: Signal,
    /// A terminal result is being finalized (worktree commit) before it is committed.
    finishing: bool,
    worktree: Option<WorktreeRecord>,
    /// `rlm.spawn(cwd=)` without a worktree of its own: the directory it (and its descendants) works in.
    cwd: Option<PathBuf>,
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
    /// A running child's transcript by lane, for `rlm.progress` (see [`Delegation::set_transcripts`]).
    transcripts: Mutex<Option<crate::progress::TranscriptHook>>,
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
    /// The directory a task works in: its (or the nearest ancestor's) worktree or `cwd=`, else the shared workspace.
    fn workspace_of(&self, id: Option<&str>, shared: Option<&Path>) -> Option<PathBuf> {
        self.line(id)
            .iter()
            .filter_map(|id| self.task(id))
            .find_map(|t| {
                t.worktree
                    .as_ref()
                    .map(|w| PathBuf::from(&w.cwd))
                    .or_else(|| t.cwd.clone())
            })
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
                transcripts: Mutex::new(None),
                child_model: Mutex::new(Arc::new(|| None)),
                on_task_end: Mutex::new(None),
                on_spawn: Mutex::new(None),
                root: Mutex::new(RootOptions::default()),
            }),
        };
        (host, receiver)
    }

    pub fn collect_mode(&self, lane: &str) -> CollectMode {
        (self.inner.collect)(lane)
    }

    /// `withLimitReason`: a child that failed once its root's tree ran out of turns, tokens or cost (its run was
    /// stopped by the budget gate) fails with the limit text rather than whatever the stopped run left behind.
    /// `reason(task_id)` is that text while the child's root is spent.
    pub fn set_limit_reason(&self, reason: LimitReason) {
        *jsrt::sync::lock(&self.inner.limit_reason) = Some(reason);
    }

    fn with_limit_reason(&self, id: &str, result: Value) -> Value {
        if result["status"] != "failed" {
            return result;
        }
        let hook = jsrt::sync::lock(&self.inner.limit_reason).clone();
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
        *jsrt::sync::lock(&self.inner.fork_point) = Some(hook);
    }

    /// Where `rlm.progress` reads a running child's transcript (none: it reports levels from no activity).
    pub fn set_transcripts(&self, hook: crate::progress::TranscriptHook) {
        *jsrt::sync::lock(&self.inner.transcripts) = Some(hook);
    }

    /// Read the model of spawns that name none from `source` at each spawn (TS `childModel` as a function: a
    /// changed setting applies to the next subagent); by default they name none.
    pub fn set_child_model(&self, source: ChildModelSource) {
        *jsrt::sync::lock(&self.inner.child_model) = source;
    }

    fn child_model(&self) -> Option<String> {
        let source = jsrt::sync::lock(&self.inner.child_model).clone();
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
        let mut root = jsrt::sync::lock(&self.inner.root);
        root.level = level;
        root.allowance = allowance;
    }

    /// `rootFinish`: the root's own `rlm.finish` goes to `finish` (its parent's host) instead of being refused.
    pub fn set_root_finish(&self, finish: RootFinish) {
        jsrt::sync::lock(&self.inner.root).finish = Some(finish);
    }

    /// Who sees each subagent's end (`onTaskEnd(publicRecord(task), info)`): its terminal journal record (id, key,
    /// definition, state, result, parentId?). Observers never affect the result.
    pub fn on_task_end(&self, observer: TaskEndObserver) {
        *jsrt::sync::lock(&self.inner.on_task_end) = Some(observer);
    }

    /// Tell `observer` of each subagent `rlm.spawn` admits, on the spawning request's thread before the child starts
    /// (an external root charges the child's work to the turn that spawned it).
    pub fn on_spawn(&self, observer: SpawnObserver) {
        *jsrt::sync::lock(&self.inner.on_spawn) = Some(observer);
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

    /// Whether `kind` is a host request [`Delegation::handle_with`] answers.
    pub fn serves(kind: &str) -> bool {
        Request::of(kind).is_some()
    }

    pub fn handle(&self, kind: &str, payload: Value, lane: &str) -> Result<Value, String> {
        self.handle_with(kind, payload, lane, &CellContext::default())
    }

    pub fn handle_with(&self, kind: &str, payload: Value, lane: &str, cell: &CellContext) -> Result<Value, String> {
        let request = Request::of(kind);
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
        let Some(request) = request else {
            return Err(format!("Unknown host request: {kind}"));
        };
        match request {
            Request::Spawn => self.spawn(&payload, parent, lane),
            Request::Finish => self.finish_request(payload, parent),
            Request::ListSubagents => {
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
            Request::Progress => {
                let ids = self.selected(&payload, parent)?;
                Ok(json!({ "subagents": self.progress(&ids) }))
            }
            Request::Collect | Request::Results => {
                let collect = matches!(request, Request::Collect);
                let ids = self.selected(&payload, parent)?;
                let waits = collect && self.collect_mode(lane) == CollectMode::InCell;
                if !waits {
                    let running = self.unfinished(&ids);
                    if collect && !running.is_empty() {
                        return Err(format!(
                            "rlm.collect: {} subagent(s) still running ({}). The host waits for them, not this cell: do other work, or end your turn; each end arrives as a child_done event, then collect again.",
                            running.len(),
                            running.join(", ")
                        ));
                    }
                    let mut out = self.collected(self.wait_until(&ids, Some(Duration::ZERO), None))?;
                    if !collect {
                        out["running"] = json!(running);
                    }
                    return Ok(out);
                }
                let timeout = collect_timeout(payload.get("timeout_ms"))?;
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
            Request::DeleteSubagent => {
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
            Request::Merge => self.merge_request(&payload, parent, lane, cell),
            Request::WorktreesList => {
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
            Request::WorktreesCleanup => {
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
        }
    }

    /// `rlm.merge`: the children end first (a host-side wait, cancelled with the cell), then each child's branch is
    /// merged, in the given order and one merge call at a time, into the caller's working tree.
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

/// A host request [`Delegation`] answers.
#[derive(Clone, Copy)]
enum Request {
    Spawn,
    Finish,
    ListSubagents,
    Progress,
    Collect,
    Results,
    DeleteSubagent,
    Merge,
    WorktreesList,
    WorktreesCleanup,
}

impl Request {
    fn of(kind: &str) -> Option<Request> {
        Some(match kind {
            "rlm.spawn" => Request::Spawn,
            "rlm.finish" => Request::Finish,
            "rlm.list_subagents" => Request::ListSubagents,
            "rlm.progress" => Request::Progress,
            "rlm.collect" => Request::Collect,
            "rlm.results" => Request::Results,
            "rlm.delete_subagent" => Request::DeleteSubagent,
            "rlm.merge" => Request::Merge,
            "rlm.worktrees.list" => Request::WorktreesList,
            "rlm.worktrees.cleanup" => Request::WorktreesCleanup,
            _ => return None,
        })
    }
}
