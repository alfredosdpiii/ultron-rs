//! Transaction settlement, batch assembly, owner validation, and post-commit adoption.

use super::types::{
    DocEntry, Plan, PlanChange, PlanRecord, PlanTracker, SubmissionChange, Target, TaskWrite, apply_submission_change,
    object_of,
};
use super::{INTERNAL_SCAN_PAGE_SIZE, Tx};
use crate::documents::{address_id, address_of};
use crate::session::{CommitChange, Inner, Loaded, LoadedDocument, Result, SessionError, lock};
use crate::{
    ConversationId, Cursor, DocumentContent, DocumentFork, DocumentId, DocumentPoint, DocumentQuery, DocumentRecord,
    DocumentScope, Seq, StorageWrite, SubmissionId, TaskId, TaskStatus,
};
use chord_delta::{Op, Prepared, Tracker};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

type PublishedDoc = (u64, Arc<Value>, Vec<Op>);

impl<'a> Tx<'a> {
    /// Revoke every draft without writing anything.
    pub(crate) fn discard(&self) {
        for entry in &self.documents {
            if let Some(draft) = &entry.draft {
                draft.abort();
            }
        }
    }

    /// Prepare every draft (revoking it) and assemble the atomic batch and the adoption plans. Any failure writes
    /// nothing and leaves every committed revision unchanged.
    pub(crate) fn settle(mut self) -> Result<(Vec<StorageWrite>, Vec<Plan>)> {
        let result = self.assemble();
        if result.is_err() {
            self.discard();
        }
        result
    }

    fn assemble(&mut self) -> Result<(Vec<StorageWrite>, Vec<Plan>)> {
        let mut plans = self.plan_documents()?;
        self.validate_commit(&plans)?;
        self.retire_terminal_task_documents(&mut plans)?;
        self.resolve_publication_conversations(&mut plans)?;
        self.apply_submission_changes()?;
        let writes = self.collect_batch_writes(&mut plans)?;
        Ok((writes, plans))
    }

    fn plan_documents(&mut self) -> Result<Vec<Plan>> {
        let mut plans = Vec::new();
        for entry in std::mem::take(&mut self.documents) {
            if let Some(plan) = plan_document(entry)? {
                plans.push(plan);
            }
        }
        Ok(plans)
    }

    fn validate_commit(&mut self, plans: &[Plan]) -> Result<()> {
        self.reject_fork_source_writes(plans)?;
        self.validate_owners()?;
        self.validate_replaced_tasks()
    }

    fn validate_replaced_tasks(&mut self) -> Result<()> {
        let replaced: Vec<(TaskId, ConversationId)> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| match &task.write {
                Some((TaskWrite::Replace, record)) => Some((*id, record.conversation_id)),
                _ => None,
            })
            .collect();
        for (id, conversation_id) in replaced {
            self.validate_replaced_task(id, conversation_id)?;
        }
        Ok(())
    }

    fn validate_replaced_task(&mut self, id: TaskId, conversation_id: ConversationId) -> Result<()> {
        let Some(committed) = self.committed_task(id)? else {
            return Err(SessionError::Error(format!("Task {} does not exist", id.0)));
        };
        if committed.state.status() == TaskStatus::Terminal {
            return Err(SessionError::Error(format!("Task {} is already terminal", id.0)));
        }
        if committed.conversation_id != conversation_id {
            return Err(SessionError::Error(format!(
                "Task {} cannot change conversations",
                id.0
            )));
        }
        Ok(())
    }

    fn retire_terminal_task_documents(&mut self, plans: &mut Vec<Plan>) -> Result<()> {
        let terminal: Vec<(TaskId, bool)> = self
            .tasks
            .values()
            .filter_map(|task| match &task.write {
                Some((kind, record)) if record.state.status() == TaskStatus::Terminal => {
                    Some((record.id, *kind == TaskWrite::Create))
                }
                _ => None,
            })
            .collect();
        if terminal.is_empty() {
            return Ok(());
        }
        let mut retiring = mark_terminal_plans(plans, &terminal);
        for (task_id, created) in terminal {
            if !created {
                self.retire_committed_task_documents(task_id, &mut retiring, plans)?;
            }
        }
        Ok(())
    }

    fn retire_committed_task_documents(
        &mut self,
        task_id: TaskId,
        retiring: &mut HashSet<DocumentId>,
        plans: &mut Vec<Plan>,
    ) -> Result<()> {
        let query = DocumentQuery {
            scope: DocumentScope::Task { task_id },
            at: DocumentPoint::Current,
            kind: None,
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self.read(|storage| storage.scan_documents(&query, INTERNAL_SCAN_PAGE_SIZE, cursor.as_ref()))?;
            for record in page.items {
                if retiring.insert(record.identity.id) {
                    plans.push(committed_retire_plan(record));
                }
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
        }
    }

    fn resolve_publication_conversations(&mut self, plans: &mut [Plan]) -> Result<()> {
        for plan in plans {
            if plan.publishes() {
                self.resolve_plan_conversation(plan)?;
            }
        }
        Ok(())
    }

    fn resolve_plan_conversation(&mut self, plan: &mut Plan) -> Result<()> {
        match plan.record.identity().scope {
            DocumentScope::Conversation { conversation_id } => plan.conversation_id = Some(conversation_id),
            DocumentScope::Task { task_id } => {
                let known = self.tasks.get(&task_id).and_then(|task| task.publication_conversation);
                let conversation = match known {
                    Some(conversation) => Some(conversation),
                    None => self.current_task(task_id)?.map(|task| task.conversation_id),
                };
                if let Some(conversation) = conversation {
                    self.tasks.entry(task_id).or_default().publication_conversation = Some(conversation);
                }
                plan.conversation_id = conversation;
            }
            DocumentScope::Session => {}
        }
        Ok(())
    }

    fn apply_submission_changes(&mut self) -> Result<()> {
        for (id, change) in std::mem::take(&mut self.submission_changes) {
            self.apply_one_submission_change(id, &change)?;
        }
        Ok(())
    }

    fn apply_one_submission_change(&mut self, id: SubmissionId, change: &SubmissionChange) -> Result<()> {
        let current = match self.submissions.get(&id) {
            Some(current) => Some(current.clone()),
            None => self.read(|storage| storage.submission(id))?,
        };
        let Some(current) = current else {
            return Err(SessionError::Error(format!("Submission {} does not exist", id.0)));
        };
        let next = apply_submission_change(&current, change)?;
        if next != current {
            self.submissions.insert(id, next);
        }
        Ok(())
    }

    fn collect_batch_writes(&mut self, plans: &mut [Plan]) -> Result<Vec<StorageWrite>> {
        let mut writes = std::mem::take(&mut self.writes);
        writes.extend(
            self.submissions
                .values()
                .map(|value| StorageWrite::Submission { value: value.clone() }),
        );
        writes.extend(self.tasks.values().filter_map(|task| {
            task.write
                .as_ref()
                .map(|(_, record)| StorageWrite::Task { value: record.clone() })
        }));
        for plan in plans {
            maybe_checkpoint_plan(plan)?;
            if let Some(content) = &plan.content {
                writes.push(content.clone());
            }
            if plan.retire {
                writes.push(StorageWrite::DocumentRetire {
                    id: plan.record.identity().id,
                });
            }
        }
        Ok(writes)
    }

    fn collect_new_owners(&self) -> Vec<(&'static str, TaskId)> {
        let mut owners = Vec::new();
        for write in &self.writes {
            if let StorageWrite::Conversation { value } = write
                && let Some(owner) = value.owner
            {
                owners.push(("Conversation owner task", owner.task_id));
            }
        }
        for task in self.tasks.values() {
            if let Some((TaskWrite::Create, record)) = &task.write
                && let Some(owner) = record.owner
            {
                owners.push(("Task owner", owner));
            }
        }
        owners
    }

    fn validate_owner(&mut self, what: &'static str, task_id: TaskId) -> Result<()> {
        let Some(task) = self.current_task(task_id)? else {
            return Err(SessionError::Error(format!("{what} {} does not exist", task_id.0)));
        };
        match task.state.status() {
            TaskStatus::Terminal => return Err(SessionError::Error(format!("{what} {} is terminal", task_id.0))),
            TaskStatus::Completing => {
                return Err(SessionError::Error(format!("{what} {} is completing", task_id.0)));
            }
            _ => {}
        }
        if task.abort_requested {
            return Err(SessionError::Error(format!("{what} {} is abort-marked", task_id.0)));
        }
        Ok(())
    }

    /// New owned work needs a live owner, judged on its final candidate: not `completing`, terminal, or abort-marked.
    fn validate_owners(&mut self) -> Result<()> {
        for (what, task_id) in self.collect_new_owners() {
            self.validate_owner(what, task_id)?;
        }
        Ok(())
    }

    fn reject_fork_source_writes(&self, plans: &[Plan]) -> Result<()> {
        for plan in plans {
            if plan.content.is_none() && !plan.retire {
                continue;
            }
            let record = plan.record.identity();
            if self.fork_source_documents.contains(&record.id) {
                return Err(SessionError::Error(format!(
                    "Cannot change fork source document {} in the fork transaction",
                    record.id.0
                )));
            }
            if let DocumentScope::Conversation { conversation_id } = record.scope
                && self.fork_source_conversations.contains(&conversation_id)
                && record.fork == Some(DocumentFork::Current)
            {
                return Err(SessionError::Error(format!(
                    "Cannot fork conversation {} while changing its current-policy documents",
                    conversation_id.0
                )));
            }
        }
        Ok(())
    }
}

fn mark_terminal_plans(plans: &mut [Plan], terminal: &[(TaskId, bool)]) -> HashSet<DocumentId> {
    let mut retiring = HashSet::new();
    for plan in plans {
        if let DocumentScope::Task { task_id } = plan.record.identity().scope
            && terminal.iter().any(|(id, _)| *id == task_id)
        {
            plan.retire = true;
            retiring.insert(plan.record.identity().id);
        }
    }
    retiring
}

fn committed_retire_plan(record: DocumentRecord) -> Plan {
    Plan {
        address_id: address_id(&address_of(&record.identity)),
        record: PlanRecord::Committed(record),
        retire: true,
        content: None,
        change: None,
        conversation_id: None,
    }
}

fn maybe_checkpoint_plan(plan: &mut Plan) -> Result<()> {
    let (
        Some(StorageWrite::DocumentChange {
            id,
            content: DocumentContent::Delta { .. },
        }),
        Some(PlanChange {
            tracker: PlanTracker::Loaded(loaded),
            prepared,
            version,
            definition: Some(doc),
        }),
    ) = (&plan.content, &plan.change)
    else {
        return Ok(());
    };
    let Some(predicate) = &doc.definition().checkpoint_when else {
        return Ok(());
    };
    let info = crate::documents::CheckpointInfo {
        deltas_since_base: lock(loaded).deltas_since_base,
    };
    if predicate(doc.definition(), &prepared.value, &prepared.ops, info).map_err(SessionError::Error)? {
        plan.content = Some(StorageWrite::DocumentChange {
            id: *id,
            content: DocumentContent::Base {
                version: *version,
                value: object_of(&prepared.value),
            },
        });
    }
    Ok(())
}

/// Plan of one staged document: its record, content write, and prepared change. Retirement is decided later.
fn plan_document(entry: DocEntry) -> Result<Option<Plan>> {
    let DocEntry {
        address_id,
        definition,
        draft,
        target,
        retire,
    } = entry;
    let Some(target) = target else {
        return Ok(None);
    };
    let prepared = match &draft {
        Some(draft) => Some(draft.prepare()?),
        None => None,
    };
    let plan = |record, content, change| Plan {
        address_id: address_id.clone(),
        record,
        retire,
        content,
        change,
        conversation_id: None,
    };
    Ok(Some(match target {
        Target::Created {
            record,
            version,
            tracker,
        } => {
            let prepared = prepared.expect("a created incarnation has a draft");
            let content = StorageWrite::DocumentCreate {
                record: record.clone(),
                content: DocumentContent::Base {
                    version,
                    value: object_of(&prepared.value),
                },
            };
            plan(
                PlanRecord::New(record),
                Some(content),
                Some(PlanChange {
                    tracker: PlanTracker::Created(tracker),
                    prepared,
                    version,
                    definition,
                }),
            )
        }
        Target::ForkCopy { record, source } => {
            let content = StorageWrite::DocumentCopy {
                record: record.clone(),
                source,
            };
            plan(PlanRecord::New(record), Some(content), None)
        }
        Target::Loaded(loaded) => {
            let prepared = prepared.expect("a loaded incarnation has a draft");
            let doc = definition.expect("a loaded incarnation has a definition");
            let version = doc.definition().version;
            let (record, stored_version) = {
                let document = lock(&loaded);
                (document.record.clone(), document.stored_version)
            };
            let id = record.identity.id;
            let content = if stored_version < version {
                Some(DocumentContent::Base {
                    version,
                    value: object_of(&prepared.value),
                })
            } else if !prepared.ops.is_empty() {
                Some(DocumentContent::Delta {
                    version,
                    ops: prepared.ops.clone(),
                })
            } else {
                None
            };
            plan(
                PlanRecord::Committed(record),
                content.map(|content| StorageWrite::DocumentChange { id, content }),
                Some(PlanChange {
                    tracker: PlanTracker::Loaded(loaded),
                    prepared,
                    version,
                    definition: Some(doc),
                }),
            )
        }
    }))
}

fn materialize_plan_record(record: PlanRecord, retire: bool, seq: Seq) -> (DocumentRecord, bool) {
    let committed = matches!(record, PlanRecord::Committed(_));
    let mut record = match record {
        PlanRecord::Committed(record) => record,
        PlanRecord::New(identity) => DocumentRecord {
            identity,
            created_at: seq,
            retired_at: None,
        },
    };
    if retire {
        record.retired_at = Some(seq);
    }
    (record, committed)
}

fn adopt_created_tracker(
    inner: &Inner,
    address_id: &str,
    record: &DocumentRecord,
    retire: bool,
    version: u64,
    mut tracker: Tracker,
    prepared: Prepared,
) -> Result<PublishedDoc> {
    if !retire {
        tracker.adopt(&prepared)?;
        let loaded = LoadedDocument {
            record: record.clone(),
            stored_version: version,
            value_version: version,
            deltas_since_base: 0,
            tracker,
        };
        lock(&inner.documents).insert(address_id.to_owned(), Arc::new(Mutex::new(loaded)));
    }
    Ok((version, prepared.value, Vec::new()))
}

fn adopt_loaded_tracker(
    loaded: Loaded,
    version: u64,
    prepared: Prepared,
    content: &Option<StorageWrite>,
) -> Result<PublishedDoc> {
    let mut document = lock(&loaded);
    if !prepared.ops.is_empty() {
        document.tracker.adopt(&prepared)?;
    }
    if document.stored_version < version {
        document.stored_version = version;
    }
    if let Some(StorageWrite::DocumentChange { content, .. }) = content {
        match content {
            DocumentContent::Base { .. } => document.deltas_since_base = 0,
            DocumentContent::Delta { .. } => document.deltas_since_base += 1,
        }
    }
    Ok((version, prepared.value, prepared.ops))
}

fn adopt_plan_change(
    inner: &Inner,
    address_id: &str,
    record: &DocumentRecord,
    retire: bool,
    content: &Option<StorageWrite>,
    change: PlanChange,
) -> Result<PublishedDoc> {
    let PlanChange {
        tracker,
        prepared,
        version,
        ..
    } = change;
    match tracker {
        PlanTracker::Created(tracker) => {
            adopt_created_tracker(inner, address_id, record, retire, version, tracker, prepared)
        }
        PlanTracker::Loaded(loaded) => adopt_loaded_tracker(loaded, version, prepared, content),
    }
}

fn evict_retired_document(inner: &Inner, address_id: &str, id: DocumentId) {
    let mut documents = lock(&inner.documents);
    if documents
        .get(address_id)
        .is_some_and(|loaded| lock(loaded).record.identity.id == id)
    {
        documents.remove(address_id);
    }
}

fn build_live_publication(
    record: DocumentRecord,
    conversation_id: Option<ConversationId>,
    content: Option<StorageWrite>,
    published: Option<PublishedDoc>,
    publishes: bool,
) -> Option<CommitChange> {
    if let Some(StorageWrite::DocumentCopy { source, .. }) = content {
        let conversation_id = conversation_id.expect("copies are conversation documents");
        return Some(CommitChange::DocumentCopy {
            record,
            conversation_id,
            source,
        });
    }
    let (version, value, ops) = published.filter(|_| publishes)?;
    Some(CommitChange::Document {
        record,
        conversation_id,
        version: Some(version),
        value: Some(value),
        ops,
    })
}

fn adopt_plan(inner: &Inner, plan: Plan, seq: Seq) -> Result<Option<CommitChange>> {
    let publishes = plan.publishes();
    let (record, committed) = materialize_plan_record(plan.record, plan.retire, seq);
    let published = match plan.change {
        Some(change) => Some(adopt_plan_change(
            inner,
            &plan.address_id,
            &record,
            plan.retire,
            &plan.content,
            change,
        )?),
        None => None,
    };
    if plan.retire {
        if committed {
            evict_retired_document(inner, &plan.address_id, record.identity.id);
        }
        return Ok(Some(CommitChange::Document {
            record,
            conversation_id: plan.conversation_id,
            version: None,
            value: None,
            ops: Vec::new(),
        }));
    }
    Ok(build_live_publication(
        record,
        plan.conversation_id,
        plan.content,
        published,
        publishes,
    ))
}

/// Adopt every prepared change after Storage acknowledged `seq`, install new incarnations, evict retired ones, and
/// describe the document publication.
pub(crate) fn adopt(inner: &Inner, plans: Vec<Plan>, seq: Seq) -> Result<Vec<CommitChange>> {
    let mut publications = Vec::new();
    for plan in plans {
        if let Some(change) = adopt_plan(inner, plan, seq)? {
            publications.push(change);
        }
    }
    Ok(publications)
}
