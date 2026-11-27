//! Durable task scheduler and task runtime over a `durable::Session`.
//!
//! Port of upstream Pi Durable `packages/durable/src/harness/{scheduler,util,define}.ts` and the task half of
//! `harness.ts` (earendil-works/pi, MIT; pinned revision in `crates/durable/fixtures/storage-conformance.json`), spec
//! §5 (tasks) and §10.1 (failures).
//!
//! Threads, not async: the Session is synchronous with a FIFO ticket line, so upstream's promise chain on the line
//! becomes threads queued on it. One worker thread per Harness runs upstream's two microtask jobs (reconcile and the
//! reservation drain); every invocation (a run or an abort of one task) runs on its own thread; waits block on
//! condition variables and are cancelled through [`Cancel`].
//!
//! `live` mirrors every committed non-terminal task record. The commit listener updates it on the Session line, so
//! code on the line reads exactly the committed state from it. Every task transition is decided and written by one
//! callback serialized on the line: reservation, marks, runtime commits, finalization, and the step before each
//! phase. Handlers and joins run off the line. An invocation ends inside the step that decides its end, so a runtime
//! commit it queued either lands before that decision or is rejected.
//!
//! Lock discipline: the state mutex is never held while waiting for the line, calling user code (handlers, hooks,
//! registry readers, reports), or blocking; code on the line takes it in short sections.
//!
//! Not ported here: agents, hooks, tools, settings, context reads and kept contexts, conversation handles bound to an
//! invocation, `watchDoc` through the runtime, the task-graph replicated state, and the Harness's built-in documents
//! and tasks (generation, tool, compaction).

pub mod cancel;
mod harness;
mod reconcile;
pub mod registry;
mod runtime;
mod state;

pub use cancel::{Cancel, Ctx};
pub(crate) use harness::FallibleClock;
pub use harness::{
    AbortResult, Conversation, Harness, HarnessOptions, Inspection, Scheduling, TaskInspection, TaskInspectionState,
    safe_now,
};
pub use registry::{Extension, Migrate, Registry, RegistryReader, RegistrySnapshot, Task};
pub use runtime::{NextState, TaskRuntime, aborted_with, completed};
pub use state::{BlockedReason, Clock, Report, SchedulerOutcome};
use state::{
    Decision, Fit, Overlay, PhaseResult, Reservation, Resolution, Scope, State, Step, Up, can_reserve, checkpoint_of,
    missing_migration, overlay_of, record_parent, status_name, with_abort_mark,
};
pub(crate) use state::{Invocation, Mode, PhaseState, SettleOutcome, WithdrawInputs, json_equal, with_state};

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use durable::{
    AbortReason, ConversationId, JoinPolicy, Session, SessionError, TaskId, TaskQuery, TaskRecord, TaskState,
    TaskStatus, Tx, TxScope,
};
use serde_json::Value;

use cancel::{Waiters, lock, wait_any};
use registry::phase_of;

const SCAN_PAGE_SIZE: usize = 256;
const LIVE_STATUSES: [TaskStatus; 4] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Completing,
];

/// A Harness or task-runtime failure. `name()` is the upstream error class name.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessError {
    /// A Session error: `SessionFailed` (with its cause), a storage error, `ReadAfterWrite`, ...
    Session(SessionError),
    /// An abort without a reason (upstream `AbortError`, "This operation was aborted").
    Aborted,
    /// Upstream plain `Error`.
    Error(String),
    /// Upstream `new Error(message, { cause })` with a string cause.
    Caused { message: String, cause: String },
}

impl HarnessError {
    pub fn name(&self) -> &'static str {
        match self {
            HarnessError::Session(error) => error.name(),
            HarnessError::Aborted => "AbortError",
            HarnessError::Error(_) | HarnessError::Caused { .. } => "Error",
        }
    }

    pub fn message(&self) -> String {
        self.to_string()
    }

    pub fn cause(&self) -> Option<&str> {
        match self {
            HarnessError::Caused { cause, .. } => Some(cause),
            _ => None,
        }
    }

    pub fn msg(message: impl Into<String>) -> HarnessError {
        HarnessError::Error(message.into())
    }

    fn into_session(self) -> SessionError {
        match self {
            HarnessError::Session(error) => error,
            other => SessionError::Error(other.to_string()),
        }
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HarnessError::Session(error) => error.fmt(f),
            HarnessError::Aborted => f.write_str("This operation was aborted"),
            HarnessError::Error(message) | HarnessError::Caused { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<SessionError> for HarnessError {
    fn from(error: SessionError) -> Self {
        HarnessError::Session(error)
    }
}

pub type Result<T> = std::result::Result<T, HarnessError>;

/// Run one commit whose callback fails with a [`HarnessError`]; the callback's own error comes back unchanged.
pub(crate) fn commit_on<T>(
    session: &Session,
    scope: TxScope,
    change: impl FnOnce(&mut Tx<'_>) -> Result<T>,
) -> Result<T> {
    let mut own = None;
    let result = session.commit_with(scope, |tx| match change(tx) {
        Ok(value) => Ok(value),
        Err(error) => {
            let carried = error.clone().into_session();
            own = Some(error);
            Err(carried)
        }
    });
    result.map_err(|error| own.take().unwrap_or(HarnessError::Session(error)))
}

/// What a closed Harness rejects with: `SessionFailed` with the cause when the Session failed.
pub(crate) fn closed_error(session: &Session) -> HarnessError {
    match session.failure() {
        Some(failure) => HarnessError::Session(SessionError::Failed(Box::new(failure))),
        None => HarnessError::msg("Harness is closed"),
    }
}

/// Run user code, turning a panic into an error.
pub(crate) fn contained<T>(call: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "[panic]".to_owned());
            Err(HarnessError::Error(message))
        }
    }
}

pub(crate) struct SchedulerOptions {
    pub session: Session,
    pub registry: Arc<dyn RegistryReader>,
    pub now: Clock,
    pub report: Report,
    pub settle_outcome: SettleOutcome,
    pub withdraw_inputs: WithdrawInputs,
}

/// Durable task scheduler of one Harness.
pub(crate) struct Scheduler {
    pub(crate) session: Session,
    registry: Arc<dyn RegistryReader>,
    pub(crate) now: Clock,
    pub(crate) report: Report,
    settle_outcome: SettleOutcome,
    withdraw_inputs: WithdrawInputs,
    state: Mutex<State>,
    /// Wakes the worker; paired with `state`.
    wake: Condvar,
    task_waiters: Waiters<TaskId, TaskRecord>,
    /// Idle waiters by conversation; `None` waits for the whole Harness.
    idle_waiters: Waiters<Option<ConversationId>, ()>,
    /// Cancelled with `SessionFailed` the moment the Session fails.
    failed: Cancel,
    unsubscribe_registry: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Whether the Session's failure has been reported (upstream `SessionImpl.fail` reports it once).
    failure_reported: AtomicBool,
    this: Weak<Scheduler>,
}

impl Scheduler {
    pub(crate) fn new(options: SchedulerOptions) -> Arc<Scheduler> {
        Arc::new_cyclic(|this| Scheduler {
            session: options.session,
            registry: options.registry,
            now: options.now,
            report: options.report,
            settle_outcome: options.settle_outcome,
            withdraw_inputs: options.withdraw_inputs,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            task_waiters: Waiters::new(),
            idle_waiters: Waiters::new(),
            failed: Cancel::new(),
            unsubscribe_registry: Mutex::new(None),
            failure_reported: AtomicBool::new(false),
            this: this.clone(),
        })
    }

    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn arc(&self) -> Arc<Scheduler> {
        self.this.upgrade().expect("scheduler is alive while used")
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.st().closing
    }

    fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        commit_on(&self.session, TxScope::default(), change)
    }

    fn subscribe_scheduler(&self) -> Result<()> {
        let weak = self.this.clone();
        self.session.subscribe_commits(move |publication| {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.observe(publication);
            }
        })?;
        let weak = self.this.clone();
        self.session.subscribe_close(move || {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.seal();
            }
        })?;
        let weak = self.this.clone();
        let unsubscribe = self.registry.subscribe(Box::new(move || {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.kick();
            }
        }));
        *lock(&self.unsubscribe_registry) = Some(unsubscribe);
        if self.is_closing()
            && let Some(unsubscribe) = lock(&self.unsubscribe_registry).take()
        {
            unsubscribe();
        }
        Ok(())
    }

    fn scan_live_tasks(tx: &mut Tx<'_>) -> Result<Vec<TaskRecord>> {
        let mut scans = Vec::new();
        for status in LIVE_STATUSES {
            let query = TaskQuery {
                status: Some(status),
                ..TaskQuery::default()
            };
            let mut cursor = None;
            loop {
                let page = tx.scan_tasks(&query, SCAN_PAGE_SIZE, cursor.as_ref())?;
                scans.extend(page.items);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        Ok(scans)
    }

    fn register_loaded_tasks(&self, scans: Vec<TaskRecord>) -> Vec<TaskRecord> {
        let mut writes = Vec::new();
        let mut st = self.st();
        for record in scans {
            if record.abandon_on_restart {
                st.abandoned.insert(record.id);
            }
            if let TaskState::Running { checkpoint } = &record.state {
                writes.push(with_state(
                    &record,
                    TaskState::Pending {
                        checkpoint: checkpoint.clone(),
                    },
                ));
            }
            if let TaskState::Waiting {
                policy: JoinPolicy::FailFast,
                ..
            } = &record.state
            {
                st.fail_fast_checks.insert(record.id);
            }
            st.live.insert(record.id, record);
        }
        writes
    }

    /// Load live tasks and change surviving `running` tasks back to `pending`. Dispatches nothing.
    pub(crate) fn open(&self) -> Result<()> {
        self.subscribe_scheduler()?;
        self.commit(|tx| {
            let scans = Self::scan_live_tasks(tx)?;
            for record in self.register_loaded_tasks(scans) {
                tx.set_task(record)?;
            }
            Ok(())
        })?;
        {
            let mut st = self.st();
            st.cascade_pending = true;
            st.schedule_reconcile();
        }
        let scheduler = self.arc();
        std::thread::Builder::new()
            .name("durable-scheduler".into())
            .spawn(move || scheduler.work())
            .map_err(|error| HarnessError::Error(error.to_string()))?;
        self.wake.notify_all();
        Ok(())
    }

    /// Enable scheduling. Idempotent; the kick does nothing once closing.
    pub(crate) fn resume(&self) {
        self.st().enabled = true;
        self.kick();
    }

    /// Signal every invocation and wait for them; writes nothing.
    pub(crate) fn join(&self) {
        let invocations: Vec<Arc<Invocation>> = self.st().invocations.values().cloned().collect();
        for invocation in &invocations {
            invocation.controller.abort();
        }
        for invocation in &invocations {
            invocation.done.wait();
        }
    }

    fn try_orphan_blocked(&self, tx: &mut Tx<'_>, current: &TaskRecord) -> Result<bool> {
        self.load_scopes(tx, false)?;
        if self.st().owned_live(None).contains_key(&current.id) {
            return Ok(false);
        }
        let snapshot = self.registry.snapshot();
        if let Resolution::Blocked(reason) = self.resolve(current, &snapshot) {
            let outcome = SchedulerOutcome::Orphaned {
                reason: reason.as_str().into(),
            };
            self.terminate(tx, current, &outcome)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn ensure_abort_marked(tx: &mut Tx<'_>, current: &TaskRecord) -> Result<()> {
        if !current.abort_requested || current.abort_reason.is_some() {
            tx.set_task(with_abort_mark(current, None))?;
        }
        Ok(())
    }

    fn lookup_non_terminal(tx: &mut Tx<'_>, id: TaskId) -> Result<Option<TaskRecord>> {
        let Some(current) = tx.task(id)? else {
            return Err(HarnessError::Error(format!("Task {} does not exist", id.0)));
        };
        Ok((current.state.status() != TaskStatus::Terminal).then_some(current))
    }

    fn abort_in_tx(
        &self,
        tx: &mut Tx<'_>,
        id: TaskId,
        ctx: &Ctx,
        keep_restart: bool,
    ) -> Result<(AbortResult, Option<Arc<Invocation>>)> {
        ctx.check()?;
        let Some(current) = Self::lookup_non_terminal(tx, id)? else {
            return Ok((AbortResult::Terminal, None));
        };
        let invocation = self.st().invocations.get(&id).cloned();
        if keep_restart && current.abort_reason == Some(AbortReason::Restart) {
            return Ok((AbortResult::Marked, None));
        }
        let orphanable = invocation.is_none() && current.state.status() != TaskStatus::Completing;
        if orphanable && self.try_orphan_blocked(tx, &current)? {
            return Ok((AbortResult::Marked, None));
        }
        Self::ensure_abort_marked(tx, &current)?;
        let run = invocation.filter(|invocation| invocation.mode == Mode::Run);
        Ok((AbortResult::Marked, run))
    }

    /// Commit the abort mark, or settle a task no registered definition can take as `orphaned` when nothing it owns is
    /// live, then join the run invocation seen on the line. With `keep_restart`, a restart-marked task keeps its mark.
    pub(crate) fn abort(&self, id: TaskId, ctx: &Ctx, keep_restart: bool) -> Result<AbortResult> {
        let (result, run) = self.commit(|tx| self.abort_in_tx(tx, id, ctx, keep_restart))?;
        if let Some(run) = run {
            let caller = ctx.signal().cloned().unwrap_or_default();
            let (index, reason) = wait_any(&[&run.done, &self.failed, &caller]);
            if index != 0 {
                return Err(reason);
            }
        }
        Ok(result)
    }

    pub(crate) fn wait_for_task(&self, id: TaskId, ctx: &Ctx) -> Result<TaskRecord> {
        let slot = self.commit(|tx| {
            if self.is_closing() {
                return Err(closed_error(&self.session));
            }
            if self.st().live.contains_key(&id) {
                return self.task_waiters.add(id, ctx);
            }
            match tx.task(id)? {
                Some(record) => Ok(cancel::Slot::settled(Ok(record))),
                None => Err(HarnessError::Error(format!("Task {} does not exist", id.0))),
            }
        })?;
        slot.wait(ctx)
    }

    /// Resolve when ordinary traversal from the conversation, or from every ownerless conversation, reaches no live
    /// non-background task.
    pub(crate) fn wait_for_idle(&self, conversation_id: Option<ConversationId>, ctx: &Ctx) -> Result<()> {
        let slot = {
            let mut st = self.st();
            if st.closing {
                drop(st);
                return Err(closed_error(&self.session));
            }
            if st.idle(conversation_id) {
                return Ok(());
            }
            st.schedule_reconcile();
            self.wake.notify_all();
            self.idle_waiters.add(conversation_id, ctx)?
        };
        slot.wait(ctx)
    }

    fn collect_conversation_aborts(
        &self,
        scope: Scope,
        background: bool,
        queued: Vec<ConversationId>,
    ) -> (Vec<TaskId>, Vec<TaskRecord>, Vec<ConversationId>) {
        let st = self.st();
        let mut reached = Vec::new();
        let mut marks = Vec::new();
        for record in st.live.values() {
            if (record.background && !background) || st.in_scope(record_parent(record), scope, background) != Some(true)
            {
                continue;
            }
            reached.push(record.id);
            if !record.abort_requested || record.abort_reason.is_some() {
                marks.push(with_abort_mark(record, None));
            }
        }
        let withdraw: Vec<ConversationId> = queued
            .into_iter()
            .filter(|id| st.in_scope(Up::Conversation(*id), scope, background) == Some(true))
            .collect();
        (reached, marks, withdraw)
    }

    fn abort_conversation_in_tx(
        &self,
        tx: &mut Tx<'_>,
        conversation_id: ConversationId,
        background: bool,
        ctx: &Ctx,
    ) -> Result<Vec<TaskId>> {
        ctx.check()?;
        let queued = self.load_scopes(tx, true)?;
        let scope = Scope::Conversation(conversation_id);
        let (reached, marks, withdraw) = self.collect_conversation_aborts(scope, background, queued);
        for mark in marks {
            tx.set_task(mark)?;
        }
        for id in withdraw {
            (self.withdraw_inputs)(tx, id)?;
        }
        Ok(reached)
    }

    /// `Conversation.abort()`: in one commit, withdraw the queued inputs and mark every live non-background task that
    /// ordinary traversal from the conversation reaches; returns once the scope is idle. With `background`, traversal
    /// crosses background boundaries, and the wait also covers every task it reached.
    pub(crate) fn abort_conversation(
        &self,
        conversation_id: ConversationId,
        background: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        let reached = self.commit(|tx| self.abort_conversation_in_tx(tx, conversation_id, background, ctx))?;
        if background {
            for id in reached {
                self.wait_for_task(id, ctx)?;
            }
        }
        self.wait_for_idle(Some(conversation_id), ctx)
    }

    /// Scheduling state and every live task with its derived state, read on the line. Runs no task code.
    pub(crate) fn inspect(
        &self,
        tx: &mut Tx<'_>,
        snapshot: &RegistrySnapshot,
    ) -> Result<(Scheduling, Vec<TaskInspection>)> {
        self.load_scopes(tx, false)?;
        let st = self.st();
        let owned = st.owned_live(None);
        let tasks = st
            .live
            .values()
            .map(|record| TaskInspection {
                record: record.clone(),
                state: self.inspect_task(&st, record, snapshot, &owned),
            })
            .collect();
        let scheduling = if st.closing {
            Scheduling::Closing
        } else if st.enabled {
            Scheduling::Running
        } else {
            Scheduling::Paused
        };
        Ok((scheduling, tasks))
    }

    fn inspect_task(
        &self,
        st: &State,
        record: &TaskRecord,
        snapshot: &RegistrySnapshot,
        owned: &HashMap<TaskId, Vec<TaskId>>,
    ) -> TaskInspectionState {
        if st.invocations.contains_key(&record.id) {
            return TaskInspectionState::Running;
        }
        if record.state.status() == TaskStatus::Completing {
            return TaskInspectionState::Completing;
        }
        let on = st.waiting_on(record, owned);
        if !on.is_empty() {
            return TaskInspectionState::Waiting { on };
        }
        match st.fit(record, snapshot.task(&record.kind)) {
            Fit::Blocked { reason, error } => TaskInspectionState::Blocked { reason, error },
            Fit::Ready { task, migrates } => {
                if migrates && task.migration().is_none() {
                    TaskInspectionState::Blocked {
                        reason: BlockedReason::MigrationFailed,
                        error: Some(missing_migration(record, &task)),
                    }
                } else {
                    TaskInspectionState::Ready { migrates }
                }
            }
        }
    }

    fn create_invocation(&self, record: &TaskRecord, mode: Mode) -> Arc<Invocation> {
        let invocation = Arc::new(Invocation {
            task_id: record.id,
            conversation_id: record.conversation_id,
            mode,
            controller: Cancel::new(),
            ended: AtomicBool::new(false),
            done: Cancel::new(),
        });
        self.st().invocations.insert(record.id, invocation.clone());
        invocation
    }

    fn start(&self, reservation: Reservation) {
        let scheduler = self.arc();
        let name = format!("durable-task-{}", reservation.invocation.task_id.0);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let invocation = reservation.invocation.clone();
            let result = match invocation.mode {
                Mode::Run => scheduler.run(reservation),
                Mode::Abort => scheduler.run_abort(reservation),
            };
            if let Err(error) = result {
                (scheduler.report)(&error);
            }
            scheduler.end(&invocation);
            invocation.done.abort();
            scheduler.kick();
        });
        if let Err(error) = spawned {
            (self.report)(&HarnessError::Error(error.to_string()));
        }
    }

    /// Run phase handlers, each preceded by a step that decides on the line whether the invocation continues.
    fn run(&self, reservation: Reservation) -> Result<()> {
        let invocation = reservation.invocation;
        let phase = Arc::new(Mutex::new(PhaseState {
            task: reservation.task,
            snapshot: reservation.snapshot,
            reported: None,
        }));
        let runtime = TaskRuntime::new(self.arc(), invocation.clone(), phase.clone());
        let mut previous: Option<PhaseResult> = None;
        loop {
            let current = self.step(&invocation, |tx, current| {
                self.decide(tx, current, previous.as_ref(), &phase)
            });
            let Some(current) = current else { return Ok(()) };
            if self.is_closing() {
                return Ok(());
            }
            let checkpoint = checkpoint_of(&current).cloned().unwrap_or(Value::Null);
            let task = lock(&phase).task.clone();
            let name = phase_of(&current).unwrap_or_default().to_owned();
            let result = match task.phase(&name) {
                Some(handler) => contained(|| handler(&current, &runtime)),
                None => Err(HarnessError::Error(format!(
                    "Task {} has no phase {}",
                    current.kind, name
                ))),
            };
            previous = Some(PhaseResult {
                checkpoint,
                failure: result.err(),
            });
        }
    }

    fn check_cancelled_owner(&self, current: &TaskRecord) -> bool {
        let mut st = self.st();
        if !current.background && st.below_cancelled(record_parent(current)) {
            st.cascade_pending = true;
            st.schedule_reconcile();
            drop(st);
            self.wake.notify_all();
            return true;
        }
        false
    }

    fn replacement_report(phase: &mut PhaseState, current: &TaskRecord, next: &Option<Task>) -> Option<HarnessError> {
        let already = phase.reported.as_ref().is_some_and(|reported| match (reported, next) {
            (None, None) => true,
            (Some(reported), Some(next)) => reported.same(next),
            _ => false,
        });
        if already {
            return None;
        }
        phase.reported = Some(next.clone());
        Some(HarnessError::Caused {
            message: format!(
                "Task {} keeps running under its old {} definition",
                current.id.0, current.kind
            ),
            cause: if next.is_none() {
                "missing_task"
            } else {
                "incompatible_task"
            }
            .into(),
        })
    }

    fn check_definition_replacement(
        &self,
        tx: &mut Tx<'_>,
        current: &TaskRecord,
        checkpoint: Value,
        phase: &Mutex<PhaseState>,
    ) -> Result<Decision> {
        let snapshot = self.registry.snapshot();
        let next = snapshot.task(&current.kind).cloned();
        let report = {
            let mut phase = lock(phase);
            phase.snapshot = snapshot;
            let replaced = next.as_ref().is_none_or(|next| !next.same(&phase.task));
            if !replaced {
                None
            } else if next.as_ref().is_some_and(|next| can_reserve(next, current)) {
                drop(phase);
                tx.set_task(with_state(current, TaskState::Pending { checkpoint }))?;
                return Ok(Decision::End);
            } else {
                Self::replacement_report(&mut phase, current, &next)
            }
        };
        if let Some(error) = report {
            (self.report)(&error);
        }
        Ok(Decision::Continue)
    }

    /// Precedence rules for a run invocation, on the line. Rules 1 (terminal, `completing`, or `waiting`) and 2
    /// (closing) are applied by `step`.
    fn decide(
        &self,
        tx: &mut Tx<'_>,
        current: &TaskRecord,
        previous: Option<&PhaseResult>,
        phase: &Mutex<PhaseState>,
    ) -> Result<Decision> {
        if current.abort_requested || self.check_cancelled_owner(current) {
            return Ok(Decision::End);
        }
        let Some(previous) = previous else {
            return Ok(Decision::Continue);
        };
        if let Some(failure) = &previous.failure {
            return Ok(Decision::Fault(failure.clone()));
        }
        let checkpoint = checkpoint_of(current).cloned().unwrap_or(Value::Null);
        if json_equal(&checkpoint, &previous.checkpoint) {
            let name = previous
                .checkpoint
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Ok(Decision::Fault(HarnessError::Error(format!(
                "Task {} phase {} returned without durable progress",
                current.kind, name
            ))));
        }
        self.check_definition_replacement(tx, current, checkpoint, phase)
    }

    /// Run the abort handler once; rules 1, 2, and 4 apply, and returning without an outcome faults.
    fn run_abort(&self, reservation: Reservation) -> Result<()> {
        let invocation = reservation.invocation;
        let current = self.st().live.get(&invocation.task_id).cloned();
        let Some(current) = current else { return Ok(()) };
        if self.is_closing() {
            return Ok(());
        }
        let phase = Arc::new(Mutex::new(PhaseState {
            task: reservation.task.clone(),
            snapshot: reservation.snapshot,
            reported: None,
        }));
        let runtime = TaskRuntime::new(self.arc(), invocation.clone(), phase);
        let handler = reservation.task.abort_handler().clone();
        let failure = contained(|| handler(&current, &runtime)).err();
        let message = format!(
            "Abort handler of task {} returned without a terminal outcome",
            invocation.task_id.0
        );
        let fault = failure.unwrap_or(HarnessError::Error(message));
        self.step(&invocation, |_, _| Ok(Decision::Fault(fault)));
        Ok(())
    }

    /// One decision on the Session line. A task that is no longer running or a closing Harness ends the invocation
    /// without a write; otherwise `decide` may stage a write and returns whether the invocation continues. A rejected
    /// step, such as admission after close, also ends the invocation.
    fn step(
        &self,
        invocation: &Arc<Invocation>,
        decide: impl FnOnce(&mut Tx<'_>, &TaskRecord) -> Result<Decision>,
    ) -> Option<TaskRecord> {
        let result = self.commit(|tx| {
            let current = {
                let st = self.st();
                match st.live.get(&invocation.task_id) {
                    Some(found) if found.state.status() == TaskStatus::Running && !st.closing => Some(found.clone()),
                    _ => None,
                }
            };
            let decision = match &current {
                Some(current) => decide(tx, current)?,
                None => Decision::End,
            };
            if let Decision::Continue = decision {
                return Ok(current);
            }
            self.end(invocation);
            if let (Decision::Fault(error), Some(current)) = (decision, &current) {
                let outcome = SchedulerOutcome::Faulted {
                    message: error.message(),
                };
                self.terminate(tx, current, &outcome)?;
            }
            Ok(None)
        });
        match result {
            Ok(current) => current,
            Err(error) => {
                self.end(invocation);
                self.fail_session(error);
                None
            }
        }
    }

    /// Write an outcome the scheduler decided. While the task's ordinary owned work is live it holds as `completing`
    /// and its Harness cleanup waits for the final commit; otherwise it is terminal with its cleanup.
    fn terminate(&self, tx: &mut Tx<'_>, record: &TaskRecord, outcome: &SchedulerOutcome) -> Result<()> {
        self.load_scopes(tx, false)?;
        let overlay = overlay_of(tx);
        let held = self.st().owned_live(Some(&overlay)).contains_key(&record.id);
        if held {
            tx.set_task(with_state(
                record,
                TaskState::Completing {
                    outcome: outcome.outcome(),
                },
            ))?;
            return Ok(());
        }
        tx.set_task(with_state(
            record,
            TaskState::Terminal {
                outcome: outcome.outcome(),
            },
        ))?;
        (self.settle_outcome)(tx, record, outcome)
    }

    /// Replace a running task's state with what it committed. A terminal state holds as `completing` while ordinary
    /// owned work is live, judged on the commit's candidates. A wait is validated first.
    pub(crate) fn commit_state(
        &self,
        tx: &mut Tx<'_>,
        invocation: &Invocation,
        current: &TaskRecord,
        next: NextState,
    ) -> Result<()> {
        let state = match next {
            NextState::Running { checkpoint } => TaskState::Running { checkpoint },
            NextState::Waiting { checkpoint, on, policy } => {
                self.validate_wait(tx, invocation, current, &on, policy)?;
                TaskState::Waiting { checkpoint, on, policy }
            }
            NextState::Terminal { outcome } => {
                let overlay = overlay_of(tx);
                self.load_scopes(tx, false)?;
                for record in overlay.tasks.values() {
                    self.load_chain(tx, record_parent(record), Some(&overlay))?;
                }
                if self.st().owned_live(Some(&overlay)).contains_key(&current.id) {
                    tx.set_task(with_state(current, TaskState::Completing { outcome }))?;
                    return Ok(());
                }
                TaskState::Terminal { outcome }
            }
        };
        tx.set_task(with_state(current, state))?;
        Ok(())
    }

    fn validate_wait_member(
        &self,
        tx: &mut Tx<'_>,
        current: &TaskRecord,
        id: TaskId,
        owners: &HashSet<TaskId>,
        overlay: &Overlay,
        policy: JoinPolicy,
    ) -> Result<()> {
        if id == current.id || owners.contains(&id) {
            return Err(HarnessError::Error(format!(
                "Task {} cannot wait on itself or its owner {}",
                current.id.0, id.0
            )));
        }
        let known = overlay
            .tasks
            .get(&id)
            .cloned()
            .or_else(|| self.st().live.get(&id).cloned());
        let member = match known {
            Some(member) => Some(member),
            None => tx.committed(|storage| storage.task(id))?,
        };
        let Some(member) = member else {
            return Err(HarnessError::Error(format!("Task {} does not exist", id.0)));
        };
        if policy == JoinPolicy::FailFast && member.owner != Some(current.id) {
            return Err(HarnessError::Error(format!(
                "Task {} can wait failFast only on tasks it owns; {} is not one",
                current.id.0, id.0
            )));
        }
        Ok(())
    }

    /// A wait names existing tasks other than the waiter and its owners; `failFast` only tasks the waiter owns. An
    /// abort handler cannot wait.
    fn validate_wait(
        &self,
        tx: &mut Tx<'_>,
        invocation: &Invocation,
        current: &TaskRecord,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> Result<()> {
        if invocation.mode == Mode::Abort {
            return Err(HarnessError::Error(format!(
                "Abort handler of task {} cannot wait",
                current.id.0
            )));
        }
        let overlay = overlay_of(tx);
        self.load_chain(tx, record_parent(current), None)?;
        let owners: HashSet<TaskId> = self
            .st()
            .above(record_parent(current), None)
            .into_iter()
            .filter_map(|step| match step {
                Step::Task(id, _) => Some(id),
                _ => None,
            })
            .collect();
        for id in on {
            self.validate_wait_member(tx, current, *id, &owners, &overlay, policy)?;
        }
        Ok(())
    }

    /// End an invocation: its runtime operations reject from now on, its signal aborts, and its task is free.
    pub(crate) fn end(&self, invocation: &Arc<Invocation>) {
        if invocation.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut st = self.st();
            if st
                .invocations
                .get(&invocation.task_id)
                .is_some_and(|current| Arc::ptr_eq(current, invocation))
            {
                st.invocations.remove(&invocation.task_id);
            }
        }
        invocation.controller.cancel(invocation.ended_error());
    }

    pub(crate) fn live_record(&self, id: TaskId) -> Option<TaskRecord> {
        self.st().live.get(&id).cloned()
    }

    pub(crate) fn registry(&self) -> &Arc<dyn RegistryReader> {
        &self.registry
    }
}
