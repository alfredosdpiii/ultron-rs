//! Observation, reconciliation, and reservation passes for the durable task scheduler.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use durable::{
    AbortReason, CommitChange, CommitPublication, ConversationId, JoinPolicy, SessionError, StorageWrite,
    SubmissionQuery, SubmissionStatus, SubmissionType, TaskId, TaskOutcome, TaskRecord, TaskState, TaskStatus, Tx,
};
use indexmap::{IndexMap, IndexSet};
use serde_json::Value;

use super::cancel::{Cancel, lock};
use super::registry::RegistrySnapshot;
use super::state::{
    BlockedReason, Fit, Invocation, Mode, Overlay, Reservation, Resolution, SchedulerOutcome, State, Up,
    cancellation_intent, checkpoint_of, failed_outcome, missing_migration, node_of, overlay_of, parent_of,
    record_parent, with_abort_mark, with_state,
};
use super::{HarnessError, Result, SCAN_PAGE_SIZE, Scheduler, closed_error, contained};

impl Scheduler {
    pub(super) fn observe_abort_change(
        st: &mut State,
        record: &TaskRecord,
        previous: Option<&TaskRecord>,
        signal: &mut Vec<Cancel>,
    ) {
        if record.abort_requested && previous.is_none_or(|previous| !previous.abort_requested) {
            st.cascade_pending = true;
            if let Some(invocation) = st.invocations.get(&record.id)
                && invocation.mode == Mode::Run
            {
                signal.push(invocation.controller.clone());
            }
        }
        if previous.is_some_and(|previous| previous.abort_reason.is_some()) && record.abort_reason.is_none() {
            st.cascade_pending = true;
        }
    }

    pub(super) fn observe_live_task_change(
        st: &mut State,
        record: &TaskRecord,
        previous: Option<&TaskRecord>,
        signal: &mut Vec<Cancel>,
    ) {
        Self::observe_abort_change(st, record, previous, signal);
        let status = record.state.status();
        let previous_status = previous.map(|previous| previous.state.status());
        if status == TaskStatus::Completing && previous_status != Some(TaskStatus::Completing) {
            if cancellation_intent(record) {
                st.cascade_pending = true;
            }
            st.schedule_reconcile();
        }
        if matches!(
            record.state,
            TaskState::Waiting {
                policy: JoinPolicy::FailFast,
                ..
            }
        ) && previous_status != Some(TaskStatus::Waiting)
        {
            st.fail_fast_checks.insert(record.id);
            st.schedule_reconcile();
        }
    }

    pub(super) fn observe_task_change(
        st: &mut State,
        record: &TaskRecord,
        signal: &mut Vec<Cancel>,
        resolved: &mut Vec<TaskRecord>,
        updated: &mut Vec<TaskRecord>,
        failed: &mut Vec<TaskId>,
    ) {
        let previous = st.live.get(&record.id).cloned();
        if failed_outcome(record) && previous.as_ref().is_none_or(|previous| !failed_outcome(previous)) {
            failed.push(record.id);
        }
        if record.state.status() == TaskStatus::Terminal {
            st.live.shift_remove(&record.id);
            st.failed_migrations.remove(&record.id);
            st.fail_fast_checks.shift_remove(&record.id);
            if st.conversation_owners.contains(&record.id) {
                st.settled.insert(record.id, node_of(record));
            }
            resolved.push(record.clone());
            st.schedule_reconcile();
            return;
        }
        Self::observe_live_task_change(st, record, previous.as_ref(), signal);
        st.live.insert(record.id, record.clone());
        updated.push(record.clone());
    }

    pub(super) fn observe_failed_waiters(st: &mut State, failed: &[TaskId]) {
        for id in failed {
            let waiters: Vec<TaskId> = st
                .live
                .values()
                .filter(|record| {
                    matches!(&record.state, TaskState::Waiting { policy: JoinPolicy::FailFast, on, .. } if on.contains(id))
                })
                .map(|record| record.id)
                .collect();
            for waiter in waiters {
                st.fail_fast_checks.insert(waiter);
                st.schedule_reconcile();
            }
        }
    }

    pub(super) fn observe_non_task_changes(st: &mut State, changes: &[CommitChange]) {
        for change in changes {
            if let CommitChange::Table(StorageWrite::Conversation { value }) = change
                && !st.edges.contains_key(&value.id)
            {
                st.set_edge(value.id, value.owner.map(|owner| owner.task_id));
            }
        }
        for change in changes {
            if let CommitChange::Table(StorageWrite::Submission { value }) = change
                && value.status == SubmissionStatus::Queued
                && value.kind == SubmissionType::Input
            {
                let up = Up::Conversation(value.conversation_id);
                if !st.chain_known(up, None) || st.below_cancelled(up) {
                    st.cascade_pending = true;
                }
            }
        }
    }

    pub(super) fn observe_updated_parents(st: &mut State, updated: &[TaskRecord]) {
        for record in updated {
            let parent = record_parent(record);
            if !st.chain_known(parent, None) {
                st.schedule_reconcile();
            } else if !record.background && !record.abort_requested && st.below_cancelled(parent) {
                st.cascade_pending = true;
            }
        }
    }

    pub(super) fn observe(&self, publication: &CommitPublication) {
        let mut signal = Vec::new();
        let mut resolved = Vec::new();
        let mut changed = false;
        {
            let mut st = self.st();
            let mut updated = Vec::new();
            let mut failed = Vec::new();
            for change in &publication.changes {
                if let CommitChange::Table(StorageWrite::Task { value: record }) = change {
                    changed = true;
                    Self::observe_task_change(&mut st, record, &mut signal, &mut resolved, &mut updated, &mut failed);
                }
            }
            Self::observe_failed_waiters(&mut st, &failed);
            Self::observe_non_task_changes(&mut st, &publication.changes);
            Self::observe_updated_parents(&mut st, &updated);
            if st.cascade_pending {
                st.schedule_reconcile();
            }
        }
        self.wake.notify_all();
        for controller in signal {
            controller.abort();
        }
        for record in resolved {
            self.task_waiters.resolve(&record.id, record.clone());
        }
        if !changed {
            return;
        }
        self.settle_idle();
        self.kick();
    }

    /// Resolve idle waiters whose scope is idle.
    pub(super) fn settle_idle(&self) {
        let idle: Vec<Option<ConversationId>> = {
            let st = self.st();
            self.idle_waiters
                .keys()
                .into_iter()
                .filter(|conversation_id| st.idle(*conversation_id))
                .collect()
        };
        for conversation_id in idle {
            self.idle_waiters.resolve(&conversation_id, ());
        }
    }

    pub(super) fn kick(&self) {
        self.st().dirty = true;
        self.wake.notify_all();
    }

    /// The worker: upstream's reconcile and drain microtasks, one at a time.
    pub(super) fn work(self: Arc<Self>) {
        enum Job {
            Reconcile,
            Reserve,
        }
        loop {
            let job = {
                let mut st = self.st();
                loop {
                    if st.closing {
                        return;
                    }
                    if st.reconcile_scheduled {
                        break Job::Reconcile;
                    }
                    if st.dirty && st.enabled {
                        st.dirty = false;
                        break Job::Reserve;
                    }
                    st = self.wake.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            match job {
                Job::Reconcile => self.reconcile(),
                Job::Reserve => match self.reserve() {
                    Ok(reservations) => {
                        for reservation in reservations {
                            self.start(reservation);
                        }
                    }
                    Err(error) => self.fail_session(error),
                },
            }
        }
    }

    pub(super) fn stage_abort_mark(
        marks: &mut IndexMap<TaskId, (TaskRecord, Option<AbortReason>)>,
        record: &TaskRecord,
        reason: Option<AbortReason>,
    ) {
        if let Some((_, staged)) = marks.get(&record.id)
            && staged.is_none()
        {
            return;
        }
        if record.abort_requested && (record.abort_reason.is_none() || reason.is_some()) {
            return;
        }
        marks.insert(record.id, (record.clone(), reason));
    }

    pub(super) fn collect_reconcile_marks(
        &self,
        checks: &[TaskId],
        marks: &mut IndexMap<TaskId, (TaskRecord, Option<AbortReason>)>,
    ) -> Vec<(TaskId, Vec<TaskId>)> {
        let st = self.st();
        for record in st.live.values() {
            if record.background {
                continue;
            }
            let Some(owner) = st.cancelling_owner(record_parent(record)) else {
                continue;
            };
            let restart = owner.abort_reason == Some(AbortReason::Restart) && !failed_outcome(owner);
            Self::stage_abort_mark(marks, record, restart.then_some(AbortReason::Restart));
        }
        checks
            .iter()
            .filter_map(|id| match st.live.get(id).map(|record| &record.state) {
                Some(TaskState::Waiting { on, .. }) => Some((*id, on.clone())),
                _ => None,
            })
            .collect()
    }

    pub(super) fn apply_fail_fast_marks(
        &self,
        tx: &mut Tx<'_>,
        fail_fast: Vec<(TaskId, Vec<TaskId>)>,
        marks: &mut IndexMap<TaskId, (TaskRecord, Option<AbortReason>)>,
    ) -> Result<()> {
        for (_, on) in fail_fast {
            if !self.any_failed(tx, &on)? {
                continue;
            }
            let st = self.st();
            for member in &on {
                if let Some(record) = st.live.get(member)
                    && !failed_outcome(record)
                {
                    Self::stage_abort_mark(marks, record, None);
                }
            }
        }
        Ok(())
    }

    pub(super) fn reconcile_in_tx(&self, tx: &mut Tx<'_>, cascade: bool, checks: &[TaskId]) -> Result<()> {
        if self.is_closing() {
            return Ok(());
        }
        let queued = self.load_scopes(tx, cascade)?;
        let mut marks: IndexMap<TaskId, (TaskRecord, Option<AbortReason>)> = IndexMap::new();
        let fail_fast = self.collect_reconcile_marks(checks, &mut marks);
        self.apply_fail_fast_marks(tx, fail_fast, &mut marks)?;
        for (record, reason) in marks.values() {
            tx.set_task(with_abort_mark(record, *reason))?;
        }
        let withdraw: Vec<ConversationId> = {
            let st = self.st();
            queued
                .into_iter()
                .filter(|id| st.below_cancelled(Up::Conversation(*id)))
                .collect()
        };
        for id in withdraw {
            (self.withdraw_inputs)(tx, id)?;
        }
        self.finalize(tx)
    }

    /// One commit that applies what committed records imply: abort marks below live owners with cancellation intent,
    /// `failFast` marks, withdrawn queued inputs below cancelled owners, and the final terminal record of every
    /// `completing` task whose ordinary owned work is gone. The durable records are the intent, so this also repairs
    /// whatever a crash left unapplied. Resolves idle waiters that the loaded edges decide.
    pub(super) fn reconcile(&self) {
        let (cascade, checks) = {
            let mut st = self.st();
            st.reconcile_scheduled = false;
            let cascade = std::mem::take(&mut st.cascade_pending);
            let checks: Vec<TaskId> = std::mem::take(&mut st.fail_fast_checks).into_iter().collect();
            (cascade, checks)
        };
        if let Err(error) = self.commit(|tx| self.reconcile_in_tx(tx, cascade, &checks)) {
            self.fail_session(error);
        }
        self.settle_idle();
    }

    /// Whether any of `ids` holds or ended with an outcome other than `completed`.
    pub(super) fn any_failed(&self, tx: &mut Tx<'_>, ids: &[TaskId]) -> Result<bool> {
        for id in ids {
            let live = self.st().live.get(id).cloned();
            let record = match live {
                Some(record) => Some(record),
                None => tx.committed(|storage| storage.task(*id))?,
            };
            if record.is_some_and(|record| failed_outcome(&record)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Write the terminal record of every `completing` task without live ordinary owned work, repeating over the
    /// commit's candidates until nothing changes. A held scheduler outcome gets its Harness cleanup here.
    pub(super) fn finalize(&self, tx: &mut Tx<'_>) -> Result<()> {
        loop {
            let overlay = overlay_of(tx);
            let done: Vec<TaskRecord> = {
                let st = self.st();
                let owned = st.owned_live(Some(&overlay));
                st.live_records(Some(&overlay))
                    .into_iter()
                    .filter(|record| record.state.status() == TaskStatus::Completing && !owned.contains_key(&record.id))
                    .cloned()
                    .collect()
            };
            if done.is_empty() {
                return Ok(());
            }
            for record in done {
                let TaskState::Completing { outcome } = &record.state else {
                    unreachable!("filtered")
                };
                let outcome = outcome.clone();
                tx.set_task(with_state(
                    &record,
                    TaskState::Terminal {
                        outcome: outcome.clone(),
                    },
                ))?;
                let scheduler_outcome = match outcome {
                    TaskOutcome::Faulted { error } => Some(SchedulerOutcome::Faulted { message: error.message }),
                    TaskOutcome::Orphaned { reason } => Some(SchedulerOutcome::Orphaned { reason }),
                    _ => None,
                };
                if let Some(outcome) = scheduler_outcome {
                    (self.settle_outcome)(tx, &record, &outcome)?;
                }
            }
        }
    }

    /// Load the owner chains of every live task and, with `queued`, of every conversation with queued submissions;
    /// returns the latter. Reads committed Storage, so it may run inside a commit callback.
    pub(super) fn load_scopes(&self, tx: &mut Tx<'_>, queued: bool) -> Result<Vec<ConversationId>> {
        let parents: Vec<Up> = {
            let st = self.st();
            st.live
                .values()
                .map(record_parent)
                .filter(|parent| !st.chain_known(*parent, None))
                .collect()
        };
        for parent in parents {
            self.load_chain(tx, parent, None)?;
        }
        if !queued {
            return Ok(Vec::new());
        }
        let query = SubmissionQuery {
            status: Some(SubmissionStatus::Queued),
            ..SubmissionQuery::default()
        };
        let mut conversations: IndexSet<ConversationId> = IndexSet::new();
        let mut cursor = None;
        loop {
            let page = tx.committed(|storage| storage.scan_submissions(&query, SCAN_PAGE_SIZE, cursor.as_ref()))?;
            conversations.extend(page.items.iter().map(|submission| submission.conversation_id));
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        for id in &conversations {
            self.load_chain(tx, Up::Conversation(*id), None)?;
        }
        Ok(conversations.into_iter().collect())
    }

    /// Load the owner edges and task nodes from `start` up to its ownerless root.
    pub(super) fn load_chain(&self, tx: &mut Tx<'_>, start: Up, overlay: Option<&Overlay>) -> Result<()> {
        let mut at = Some(start);
        while let Some(up) = at {
            match up {
                Up::Task(id) => {
                    let known = self.st().node(id, overlay);
                    let node = match known {
                        Some(node) => node,
                        None => {
                            let Some(record) = tx.committed(|storage| storage.task(id))? else {
                                return Ok(());
                            };
                            let node = node_of(&record);
                            if record.state.status() == TaskStatus::Terminal {
                                self.st().settled.insert(record.id, node);
                            }
                            node
                        }
                    };
                    at = Some(parent_of(&node));
                }
                Up::Conversation(id) => {
                    let known = self.st().edge(id, overlay);
                    let edge = match known {
                        Some(edge) => edge,
                        None => {
                            let record = tx.committed(|storage| storage.conversation(id))?;
                            let edge = record.and_then(|record| record.owner).map(|owner| owner.task_id);
                            self.st().set_edge(id, edge);
                            edge
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        Ok(())
    }

    /// Fail the Session for a throw in one of the scheduler's own commits; ignored once closing. A commit refused
    /// with `Closed` is a close in progress, not a failure: `Session::close` seals admission before it runs the close
    /// listener (`seal`) that marks the scheduler closing, and a pass committing in between is refused. Failing the
    /// Session for it turned a plain close into "Session failed" for every later call.
    pub(super) fn fail_session(&self, error: HarnessError) {
        if !self.is_closing() && !matches!(error, HarnessError::Session(SessionError::Closed)) {
            self.session.fail(error.into_session());
        }
    }

    /// Close listener: runs synchronously once admission is sealed, or when the Session fails (on the failing
    /// thread, which may hold the line: then it only signals). A plain close also joins every invocation here, after
    /// admission is sealed and before Storage closes (upstream `beforeClose`).
    pub(super) fn seal(&self) {
        {
            let mut st = self.st();
            st.closing = true;
        }
        self.wake.notify_all();
        if let Some(unsubscribe) = lock(&self.unsubscribe_registry).take() {
            unsubscribe();
        }
        let error = closed_error(&self.session);
        self.task_waiters.reject_all(error.clone());
        self.idle_waiters.reject_all(error.clone());
        match self.session.failure() {
            Some(_) => {
                self.failed.cancel(error);
                self.report_failure();
                let invocations: Vec<Arc<Invocation>> = self.st().invocations.values().cloned().collect();
                for invocation in invocations {
                    invocation.controller.abort();
                }
            }
            None => self.join(),
        }
    }

    /// Report the Session's failure once. A failure during a close that already ran the close listeners (a commit
    /// admitted before close failing) is reported when the close finishes.
    pub(crate) fn report_failure(&self) {
        if let Some(failure) = self.session.failure()
            && !self.failure_reported.swap(true, Ordering::SeqCst)
        {
            self.failed.cancel(closed_error(&self.session));
            (self.report)(&HarnessError::Session(failure));
        }
    }

    pub(super) fn mark_abandoned_in_tx(&self, tx: &mut Tx<'_>) -> Result<Option<Vec<TaskId>>> {
        let (enabled, marks) = {
            let st = self.st();
            if !st.enabled || st.closing {
                return Ok(Some(Vec::new()));
            }
            let marks: Vec<TaskRecord> = st
                .abandoned
                .iter()
                .filter_map(|id| st.live.get(id))
                .filter(|record| !record.abort_requested)
                .map(|record| with_abort_mark(record, Some(AbortReason::Restart)))
                .collect();
            (true, marks)
        };
        let _ = enabled;
        for mark in marks {
            tx.set_task(mark)?;
        }
        let st = self.st();
        if !st.abandoned.is_empty() {
            return Ok(Some(st.abandoned.iter().copied().collect()));
        }
        Ok(None)
    }

    pub(super) fn can_reserve_candidate(
        &self,
        record: &TaskRecord,
        mode: Mode,
        owned: &HashMap<TaskId, Vec<TaskId>>,
    ) -> bool {
        let mut st = self.st();
        if st.invocations.contains_key(&record.id)
            || !st.waiting_on(record, owned).is_empty()
            || record.state.status() == TaskStatus::Completing
        {
            return false;
        }
        if mode == Mode::Run && !record.background && st.below_cancelled(record_parent(record)) {
            st.cascade_pending = true;
            st.schedule_reconcile();
            return false;
        }
        true
    }

    pub(super) fn try_reserve_candidate(
        &self,
        tx: &mut Tx<'_>,
        record: &TaskRecord,
        mode: Mode,
        snapshot: &mut Option<Arc<RegistrySnapshot>>,
    ) -> Result<Option<Reservation>> {
        let snapshot = snapshot.get_or_insert_with(|| self.registry.snapshot()).clone();
        let (task, resolved) = match self.resolve(record, &snapshot) {
            Resolution::Blocked(reason) => {
                if mode == Mode::Abort && record.abort_reason.is_none() {
                    let outcome = SchedulerOutcome::Orphaned {
                        reason: reason.as_str().into(),
                    };
                    self.terminate(tx, record, &outcome)?;
                }
                return Ok(None);
            }
            Resolution::Ready { task, record } => (task, *record),
        };
        if resolved != *record || record.state.status() != TaskStatus::Running {
            let checkpoint = checkpoint_of(&resolved).cloned().unwrap_or(Value::Null);
            tx.set_task(with_state(&resolved, TaskState::Running { checkpoint }))?;
        }
        let invocation = self.create_invocation(record, mode);
        Ok(Some(Reservation {
            invocation,
            task,
            snapshot,
        }))
    }

    pub(super) fn rollback_reservations(&self, reservations: &[Reservation]) {
        let mut st = self.st();
        for reservation in reservations {
            let invocation = &reservation.invocation;
            if st
                .invocations
                .get(&invocation.task_id)
                .is_some_and(|current| Arc::ptr_eq(current, invocation))
            {
                st.invocations.remove(&invocation.task_id);
            }
            invocation.done.abort();
        }
    }

    pub(super) fn reserve_in_tx(
        &self,
        tx: &mut Tx<'_>,
        reservations: &mut Vec<Reservation>,
        abandoned: &mut Vec<TaskId>,
    ) -> Result<()> {
        if let Some(early) = self.mark_abandoned_in_tx(tx)? {
            *abandoned = early;
            return Ok(());
        }
        self.load_scopes(tx, false)?;
        let (candidates, owned) = {
            let st = self.st();
            (st.live.values().cloned().collect::<Vec<_>>(), st.owned_live(None))
        };
        let mut snapshot: Option<Arc<RegistrySnapshot>> = None;
        for record in candidates {
            let mode = if record.abort_requested { Mode::Abort } else { Mode::Run };
            if !self.can_reserve_candidate(&record, mode, &owned) {
                continue;
            }
            if let Some(reservation) = self.try_reserve_candidate(tx, &record, mode, &mut snapshot)? {
                reservations.push(reservation);
            }
        }
        Ok(())
    }

    /// Reserve every eligible task in one commit; orphan abort-marked tasks no definition can take, unless abandoned
    /// after a restart. The first pass after open only abort-marks the abandoned tasks, so later passes see the marks.
    pub(super) fn reserve(&self) -> Result<Vec<Reservation>> {
        let mut reservations: Vec<Reservation> = Vec::new();
        let mut abandoned: Vec<TaskId> = Vec::new();
        if let Err(error) = self.commit(|tx| self.reserve_in_tx(tx, &mut reservations, &mut abandoned)) {
            self.rollback_reservations(&reservations);
            return Err(error);
        }
        let mut st = self.st();
        for id in &abandoned {
            st.abandoned.shift_remove(id);
        }
        if !abandoned.is_empty() {
            st.dirty = true;
        }
        Ok(reservations)
    }

    /// Resolve the record's definition by kind, migrating an older stored version.
    pub(super) fn resolve(&self, record: &TaskRecord, snapshot: &RegistrySnapshot) -> Resolution {
        let fit = self.st().fit(record, snapshot.task(&record.kind));
        let (task, migrates) = match fit {
            Fit::Blocked { reason, .. } => return Resolution::Blocked(reason),
            Fit::Ready { task, migrates } => (task, migrates),
        };
        if !migrates {
            return Resolution::Ready {
                task,
                record: Box::new(record.clone()),
            };
        }
        let migrated = match task.migration() {
            None => Err(missing_migration(record, &task)),
            Some(migrate) => {
                let checkpoint = checkpoint_of(record).cloned().unwrap_or(Value::Null);
                contained(|| migrate(&record.input, &checkpoint, record.version))
            }
        };
        match migrated {
            Ok((input, checkpoint)) => {
                let state = match &record.state {
                    TaskState::Waiting { on, policy, .. } => TaskState::Waiting {
                        checkpoint,
                        on: on.clone(),
                        policy: *policy,
                    },
                    TaskState::Running { .. } => TaskState::Running { checkpoint },
                    _ => TaskState::Pending { checkpoint },
                };
                Resolution::Ready {
                    record: Box::new(TaskRecord {
                        version: task.version(),
                        input,
                        state,
                        ..record.clone()
                    }),
                    task,
                }
            }
            Err(error) => {
                self.st().failed_migrations.insert(record.id, (task, error.clone()));
                (self.report)(&error);
                Resolution::Blocked(BlockedReason::MigrationFailed)
            }
        }
    }
}
