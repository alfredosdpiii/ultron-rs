//! Internal state, ownership walks, and helper types for the durable task scheduler.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use durable::{
    AbortReason, ConversationId, TaskId, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState, TaskStatus, Tx,
};
use indexmap::{IndexMap, IndexSet};
use serde_json::Value;

use super::cancel::Cancel;
use super::registry::{RegistrySnapshot, Task};
use super::{HarnessError, Result};

/// Terminal outcomes the scheduler writes without running task code.
#[derive(Clone, Debug, PartialEq)]
pub enum SchedulerOutcome {
    Faulted { message: String },
    Orphaned { reason: String },
}

impl SchedulerOutcome {
    pub(super) fn outcome(&self) -> TaskOutcome {
        match self {
            SchedulerOutcome::Faulted { message } => TaskOutcome::Faulted {
                error: TaskOutcomeError {
                    message: message.clone(),
                    detail: None,
                },
            },
            SchedulerOutcome::Orphaned { reason } => TaskOutcome::Orphaned { reason: reason.clone() },
        }
    }
}

/// Harness cleanup staged in the commit that makes a scheduler outcome terminal.
pub(crate) type SettleOutcome = Arc<dyn Fn(&mut Tx<'_>, &TaskRecord, &SchedulerOutcome) -> Result<()> + Send + Sync>;
/// Withdraw a conversation's queued inputs, for conversation abort and abort cascades.
pub(crate) type WithdrawInputs = Arc<dyn Fn(&mut Tx<'_>, ConversationId) -> Result<()> + Send + Sync>;
pub type Report = Arc<dyn Fn(&HarnessError) + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Run,
    Abort,
}

/// The immutable ownership fields of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TaskNode {
    pub(super) conversation_id: ConversationId,
    pub(super) owner: Option<TaskId>,
    pub(super) background: bool,
}

/// Where a walk up the ownership tree continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Up {
    Task(TaskId),
    Conversation(ConversationId),
}

pub(super) enum Step {
    Task(TaskId, TaskNode),
    Conversation(ConversationId),
    Unknown,
}

/// Candidate records a commit staged; they override committed records in ownership walks.
#[derive(Default)]
pub(super) struct Overlay {
    pub(super) tasks: IndexMap<TaskId, TaskRecord>,
    pub(super) edges: HashMap<ConversationId, Option<TaskId>>,
}

#[derive(Clone, Copy)]
pub(super) enum Scope {
    Conversation(ConversationId),
    Roots,
}

/// Why a pending task cannot be reserved under a registry snapshot. Derived, never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    MissingTask,
    TaskTooOld,
    MigrationFailed,
}

impl BlockedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedReason::MissingTask => "missing_task",
            BlockedReason::TaskTooOld => "task_too_old",
            BlockedReason::MigrationFailed => "migration_failed",
        }
    }
}

pub(super) enum Fit {
    Ready {
        task: Task,
        migrates: bool,
    },
    Blocked {
        reason: BlockedReason,
        error: Option<HarnessError>,
    },
}

pub(super) enum Resolution {
    Ready { task: Task, record: Box<TaskRecord> },
    Blocked(BlockedReason),
}

pub(super) fn node_of(record: &TaskRecord) -> TaskNode {
    TaskNode {
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
    }
}

pub(super) fn parent_of(node: &TaskNode) -> Up {
    match node.owner {
        Some(owner) => Up::Task(owner),
        None => Up::Conversation(node.conversation_id),
    }
}

pub(super) fn record_parent(record: &TaskRecord) -> Up {
    parent_of(&node_of(record))
}

pub(super) fn outcome_of(record: &TaskRecord) -> Option<&TaskOutcome> {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => Some(outcome),
        _ => None,
    }
}

/// Whether the record holds or ends with an outcome other than `completed`.
pub(super) fn failed_outcome(record: &TaskRecord) -> bool {
    outcome_of(record).is_some_and(|outcome| !matches!(outcome, TaskOutcome::Completed { .. }))
}

/// A live owner's durable cancellation intent: its abort mark, or a held outcome other than `completed`.
pub(super) fn cancellation_intent(record: &TaskRecord) -> bool {
    record.state.status() != TaskStatus::Terminal && (record.abort_requested || failed_outcome(record))
}

/// The record with an abort mark: `restart` for an abandonment; without a reason, a request, which replaces `restart`.
pub(super) fn with_abort_mark(record: &TaskRecord, reason: Option<AbortReason>) -> TaskRecord {
    TaskRecord {
        abort_requested: true,
        abort_reason: reason,
        ..record.clone()
    }
}

/// Replace a live record's state; memos disappear once an outcome is decided.
pub(crate) fn with_state(record: &TaskRecord, state: TaskState) -> TaskRecord {
    let memos = match state {
        TaskState::Terminal { .. } | TaskState::Completing { .. } => None,
        _ => record.memos.clone(),
    };
    TaskRecord {
        state,
        memos,
        ..record.clone()
    }
}

pub(super) fn checkpoint_of(record: &TaskRecord) -> Option<&Value> {
    match &record.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => Some(checkpoint),
        _ => None,
    }
}

pub(super) fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completing => "completing",
        TaskStatus::Terminal => "terminal",
    }
}

/// Whether a definition can take the task at reservation: same version, or newer with a migration.
pub(super) fn can_reserve(task: &Task, record: &TaskRecord) -> bool {
    task.version() == record.version || (task.version() > record.version && task.migration().is_some())
}

pub(super) fn missing_migration(record: &TaskRecord, task: &Task) -> HarnessError {
    HarnessError::Error(format!(
        "Task {} version {} has no migration from {}",
        record.kind,
        task.version(),
        record.version
    ))
}

/// Structural equality of two JSON values; object key order is ignored and numbers compare by value.
pub(crate) fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a == b || a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_equal(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| json_equal(value, other)))
        }
        _ => left == right,
    }
}

/// One in-memory execution of a task in run or abort mode.
pub(crate) struct Invocation {
    pub(crate) task_id: TaskId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) mode: Mode,
    /// Signal passed to handlers.
    pub(crate) controller: Cancel,
    /// The invocation has ended: its runtime operations are refused.
    pub(super) ended: AtomicBool,
    /// Cancelled once the invocation's thread has finished.
    pub(crate) done: Cancel,
}

impl Invocation {
    pub(super) fn ended_error(&self) -> HarnessError {
        HarnessError::Error(format!("Task {} invocation has ended", self.task_id.0))
    }

    /// Fails once the invocation has ended.
    pub(crate) fn check(&self) -> Result<()> {
        if self.is_ended() {
            return Err(self.ended_error());
        }
        Ok(())
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}

/// What a runtime reads for the phase handler it serves: the phase's snapshot and task.
pub(crate) struct PhaseState {
    pub(crate) task: Task,
    pub(crate) snapshot: Arc<RegistrySnapshot>,
    /// Replacement definition already reported as unable to take over: `Some(None)` for a missing one.
    pub(super) reported: Option<Option<Task>>,
}

pub(super) struct Reservation {
    pub(super) invocation: Arc<Invocation>,
    pub(super) task: Task,
    pub(super) snapshot: Arc<RegistrySnapshot>,
}

/// Outcome of the phase that just returned, judged by the next step.
pub(super) struct PhaseResult {
    pub(super) checkpoint: Value,
    pub(super) failure: Option<HarnessError>,
}

/// Step decision: continue with the next phase, end the invocation, or end it by writing `faulted`.
pub(super) enum Decision {
    Continue,
    End,
    Fault(HarnessError),
}

#[derive(Default)]
pub(super) struct State {
    pub(super) live: IndexMap<TaskId, TaskRecord>,
    pub(super) invocations: HashMap<TaskId, Arc<Invocation>>,
    /// Definition whose migration failed per task; retried only once the registry resolves another definition.
    pub(super) failed_migrations: HashMap<TaskId, (Task, HarnessError)>,
    /// Owner task of each loaded conversation, `None` when ownerless.
    pub(super) edges: HashMap<ConversationId, Option<TaskId>>,
    /// Tasks that own a loaded conversation; their nodes stay in `settled` once terminal.
    pub(super) conversation_owners: HashSet<TaskId>,
    /// Ownership fields of terminal tasks that walks pass through.
    pub(super) settled: HashMap<TaskId, TaskNode>,
    /// `failFast` waiters the next reconcile checks for a failed task in `on`.
    pub(super) fail_fast_checks: IndexSet<TaskId>,
    /// Live tasks with `abandonOnRestart` found at open; the first reservation pass abort-marks them.
    pub(super) abandoned: IndexSet<TaskId>,
    pub(super) reconcile_scheduled: bool,
    pub(super) cascade_pending: bool,
    pub(super) enabled: bool,
    pub(super) closing: bool,
    pub(super) dirty: bool,
}

impl State {
    pub(super) fn schedule_reconcile(&mut self) {
        if !self.reconcile_scheduled && !self.closing {
            self.reconcile_scheduled = true;
        }
    }

    pub(super) fn set_edge(&mut self, conversation_id: ConversationId, owner: Option<TaskId>) {
        self.edges.insert(conversation_id, owner);
        if let Some(owner) = owner {
            self.conversation_owners.insert(owner);
        }
    }

    pub(super) fn edge(&self, id: ConversationId, overlay: Option<&Overlay>) -> Option<Option<TaskId>> {
        if let Some(edge) = overlay.and_then(|overlay| overlay.edges.get(&id)) {
            return Some(*edge);
        }
        self.edges.get(&id).copied()
    }

    pub(super) fn node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        overlay
            .and_then(|overlay| overlay.tasks.get(&id))
            .or_else(|| self.live.get(&id))
            .map(node_of)
            .or_else(|| self.settled.get(&id).copied())
    }

    /// Walk up from `start`: owner tasks and conversations, ending at an ownerless root or an edge not loaded yet.
    pub(super) fn above(&self, start: Up, overlay: Option<&Overlay>) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut at = Some(start);
        while let Some(up) = at {
            match up {
                Up::Task(id) => {
                    let Some(node) = self.node(id, overlay) else {
                        steps.push(Step::Unknown);
                        return steps;
                    };
                    steps.push(Step::Task(id, node));
                    at = Some(parent_of(&node));
                }
                Up::Conversation(id) => {
                    steps.push(Step::Conversation(id));
                    match self.edge(id, overlay) {
                        None => {
                            steps.push(Step::Unknown);
                            return steps;
                        }
                        Some(edge) => at = edge.map(Up::Task),
                    }
                }
            }
        }
        steps
    }

    pub(super) fn chain_known(&self, start: Up, overlay: Option<&Overlay>) -> bool {
        !self
            .above(start, overlay)
            .iter()
            .any(|step| matches!(step, Step::Unknown))
    }

    /// Live records, with the overlay's candidates replacing committed ones; terminal candidates are gone.
    pub(super) fn live_records<'a>(&'a self, overlay: Option<&'a Overlay>) -> Vec<&'a TaskRecord> {
        let mut records = Vec::new();
        for record in self.live.values() {
            let candidate = overlay
                .and_then(|overlay| overlay.tasks.get(&record.id))
                .unwrap_or(record);
            if candidate.state.status() != TaskStatus::Terminal {
                records.push(candidate);
            }
        }
        if let Some(overlay) = overlay {
            for record in overlay.tasks.values() {
                if !self.live.contains_key(&record.id) && record.state.status() != TaskStatus::Terminal {
                    records.push(record);
                }
            }
        }
        records
    }

    /// Every task with live ordinary owned work, mapped to that work: each live non-background task counts for every
    /// owner task above it up to and including the first background one. Owner chains must be loaded.
    pub(super) fn owned_live(&self, overlay: Option<&Overlay>) -> HashMap<TaskId, Vec<TaskId>> {
        let mut owned: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        for record in self.live_records(overlay) {
            if record.background {
                continue;
            }
            for step in self.above(record_parent(record), overlay) {
                match step {
                    Step::Unknown => break,
                    Step::Conversation(_) => continue,
                    Step::Task(id, node) => {
                        owned.entry(id).or_default().push(record.id);
                        if node.background {
                            break;
                        }
                    }
                }
            }
        }
        owned
    }

    /// Whether ordinary traversal from `scope` reaches `start`; `None` while an edge is not loaded.
    pub(super) fn in_scope(&self, start: Up, scope: Scope, cross_background: bool) -> Option<bool> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(id) => {
                    if matches!(scope, Scope::Conversation(target) if target == id) {
                        return Some(true);
                    }
                }
                Step::Task(_, node) => {
                    if node.background && !cross_background {
                        return Some(false);
                    }
                }
            }
        }
        Some(matches!(scope, Scope::Roots))
    }

    /// The nearest live owner above `start` whose cancellation intent reaches it: one with intent before a
    /// background owner without it. Terminal owners never cascade.
    pub(super) fn cancelling_owner(&self, start: Up) -> Option<&TaskRecord> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(_) => continue,
                Step::Task(id, node) => {
                    if let Some(live) = self.live.get(&id)
                        && cancellation_intent(live)
                    {
                        return Some(live);
                    }
                    if node.background {
                        return None;
                    }
                }
            }
        }
        None
    }

    pub(super) fn below_cancelled(&self, start: Up) -> bool {
        self.cancelling_owner(start).is_some()
    }

    /// No live non-background task in the scope; a task whose owner edges are not loaded yet counts as inside.
    pub(super) fn idle(&self, conversation_id: Option<ConversationId>) -> bool {
        let scope = match conversation_id {
            Some(id) => Scope::Conversation(id),
            None => Scope::Roots,
        };
        !self
            .live
            .values()
            .any(|record| !record.background && self.in_scope(record_parent(record), scope, false) != Some(false))
    }

    /// Live tasks a task waits for before its next invocation: its live ordinary owned work when abort-marked, since
    /// abort runs bottom-up, otherwise the live part of the `on` of a wait.
    pub(super) fn waiting_on(&self, record: &TaskRecord, owned: &HashMap<TaskId, Vec<TaskId>>) -> Vec<TaskId> {
        if record.abort_requested {
            return owned.get(&record.id).cloned().unwrap_or_default();
        }
        match &record.state {
            TaskState::Waiting { on, .. } => on.iter().copied().filter(|id| self.live.contains_key(id)).collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn fit(&self, record: &TaskRecord, task: Option<&Task>) -> Fit {
        let Some(task) = task else {
            return Fit::Blocked {
                reason: BlockedReason::MissingTask,
                error: None,
            };
        };
        if task.version() == record.version {
            return Fit::Ready {
                task: task.clone(),
                migrates: false,
            };
        }
        if task.version() < record.version {
            return Fit::Blocked {
                reason: BlockedReason::TaskTooOld,
                error: None,
            };
        }
        if let Some((failed, error)) = self.failed_migrations.get(&record.id)
            && failed.same(task)
        {
            return Fit::Blocked {
                reason: BlockedReason::MigrationFailed,
                error: Some(error.clone()),
            };
        }
        Fit::Ready {
            task: task.clone(),
            migrates: true,
        }
    }
}

pub(super) fn overlay_of(tx: &Tx<'_>) -> Overlay {
    Overlay {
        tasks: tx
            .staged_tasks()
            .into_iter()
            .map(|record| (record.id, record))
            .collect(),
        edges: tx
            .staged_conversations()
            .into_iter()
            .map(|record| (record.id, record.owner.map(|owner| owner.task_id)))
            .collect(),
    }
}
