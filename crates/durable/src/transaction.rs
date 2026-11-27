//! One Session commit callback's transaction: table reads before the first table write, staged table writes,
//! document acquisition and retirement, fork copies, and the atomic batch assembled from them.
//!
//! Port of `packages/durable/src/session/{transaction,forks}.ts`. Every Tx operation runs synchronously while the
//! commit holds the Session line.

mod assembly;
mod types;

pub(crate) use assembly::adopt;
pub use types::{
    ConversationOwnership, EntryDraft, EntryHead, SubmissionCreate, SubmissionSettlement, TaskDefinition, TaskOptions,
    TaskOwnership, TxScope,
};

use crate::documents::{
    Doc, DocDefinition, Owner, Resolved, address_id, address_of, check_record_scope, check_record_version,
    document_create, materialize_value, object, resolve_address,
};
use crate::session::{Inner, LineState, Loaded, Result, SessionError, lock};
use crate::{
    ConversationId, ConversationOwner, ConversationParent, ConversationQuery, ConversationRecord, Cursor,
    DocumentAddress, DocumentCopySource, DocumentCreate, DocumentFork, DocumentId, DocumentPoint, DocumentQuery,
    DocumentRecord, DocumentScope, EntryId, EntryQuery, EntryRecord, Page, ROOT_CONVERSATION_ID, StorageWrite,
    SubmissionId, SubmissionRecord, TaskId, TaskQuery, TaskRecord, TaskState, TaskStatus,
};
use chord_delta::{Draft, Tracker};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use types::{DocEntry, SubmissionChange, Target, TaskWrite, TxTask};

const INTERNAL_SCAN_PAGE_SIZE: usize = 256;

/// The transaction of one Session commit callback. Table reads fail with [`SessionError::ReadAfterWrite`] after the
/// first table write; document access stays available.
pub struct Tx<'a> {
    inner: &'a Inner,
    line: &'a mut LineState,
    scope: TxScope,
    has_table_write: bool,
    /// Conversation and entry writes, staged eagerly in order.
    writes: Vec<StorageWrite>,
    created_conversations: HashSet<ConversationId>,
    fork_source_conversations: HashSet<ConversationId>,
    fork_source_documents: HashSet<DocumentId>,
    tasks: IndexMap<TaskId, TxTask>,
    submissions: IndexMap<SubmissionId, SubmissionRecord>,
    submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    documents: Vec<DocEntry>,
    /// Latest entry (index into `documents`) at each logical address.
    latest: HashMap<String, usize>,
}

impl<'a> Tx<'a> {
    pub(crate) fn new(inner: &'a Inner, line: &'a mut LineState, scope: TxScope) -> Tx<'a> {
        Tx {
            inner,
            line,
            scope,
            has_table_write: false,
            writes: Vec::new(),
            created_conversations: HashSet::new(),
            fork_source_conversations: HashSet::new(),
            fork_source_documents: HashSet::new(),
            tasks: IndexMap::new(),
            submissions: IndexMap::new(),
            submission_changes: Vec::new(),
            documents: Vec::new(),
            latest: HashMap::new(),
        }
    }

    fn read<T>(&mut self, call: impl FnOnce(&mut dyn crate::Storage) -> crate::Result<T>) -> Result<T> {
        self.inner.guard(self.line, true, call)
    }

    /// A read of committed Storage that ignores the read-after-write rule: the conversation runtime and the task
    /// scheduler (`crates/harness`) read inside their own commit callbacks, as upstream's read Storage directly.
    pub fn committed<T>(&mut self, read: impl FnOnce(&dyn crate::Storage) -> crate::Result<T>) -> Result<T> {
        self.read(|storage| read(storage))
    }

    /// Latest candidate of every task this transaction created or replaced, in first-write order.
    pub fn staged_tasks(&self) -> Vec<TaskRecord> {
        self.tasks
            .values()
            .filter_map(|task| task.write.as_ref().map(|(_, record)| record.clone()))
            .collect()
    }

    /// Every conversation this transaction created or forked, in order.
    pub fn staged_conversations(&self) -> Vec<ConversationRecord> {
        self.writes
            .iter()
            .filter_map(|write| match write {
                StorageWrite::Conversation { value } => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    /// Create the ownerless root conversation at its reserved ID.
    pub fn create_root_conversation(&mut self) -> Result<ConversationRecord> {
        self.has_table_write = true;
        let record = ConversationRecord {
            id: ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        };
        self.created_conversations.insert(record.id);
        self.writes.push(StorageWrite::Conversation { value: record.clone() });
        Ok(record)
    }

    fn mint(&mut self) -> Result<u64> {
        self.inner.guard(self.line, false, |storage| storage.mint_id())
    }

    fn table_read(&self, method: &'static str) -> Result<()> {
        if self.has_table_write {
            Err(SessionError::ReadAfterWrite(method))
        } else {
            Ok(())
        }
    }

    pub fn conversation(&mut self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.table_read("conversation")?;
        self.read(|storage| storage.conversation(id))
    }

    pub fn entry(&mut self, id: EntryId) -> Result<Option<EntryRecord>> {
        self.table_read("entry")?;
        Ok(self.read(|storage| storage.entry(id))?.map(|stored| stored.entry))
    }

    /// `None` when the entry is absent or has another kind.
    pub fn entry_of(&mut self, kind: &str, id: EntryId) -> Result<Option<EntryRecord>> {
        Ok(self.entry(id)?.filter(|entry| entry.kind == kind))
    }

    pub fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.table_read("task")?;
        self.committed_task(id)
    }

    pub fn scan_conversations(
        &mut self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.table_read("scanConversations")?;
        self.read(|storage| storage.scan_conversations(query, limit, cursor))
    }

    pub fn scan_entries(
        &mut self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<EntryRecord>> {
        self.table_read("scanEntries")?;
        self.read(|storage| storage.scan_entries(query, limit, cursor))
    }

    /// Newest visible entry of the conversation that carries a `head`.
    pub fn latest_head_marker(&mut self, conversation_id: ConversationId) -> Result<Option<EntryRecord>> {
        self.table_read("latestHeadMarker")?;
        self.read(|storage| storage.find_latest_head_marker(conversation_id, None))
    }

    pub fn scan_tasks(&mut self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.table_read("scanTasks")?;
        self.read(|storage| storage.scan_tasks(query, limit, cursor))
    }

    pub fn submission(&mut self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.table_read("submission")?;
        self.read(|storage| storage.submission(id))
    }

    /// Committed submission with a conversation-scoped request ID, for host deduplication.
    pub fn submission_by_request(
        &mut self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.table_read("submissionByRequest")?;
        self.read(|storage| storage.submission_by_request(conversation_id, request_id))
    }

    pub fn create_conversation(&mut self, ownership: ConversationOwnership) -> Result<ConversationRecord> {
        self.has_table_write = true;
        self.stage_conversation(None, ownership)
    }

    /// Fork history at one visible entry; conversation documents are copied by their fork policy.
    pub fn fork_conversation(
        &mut self,
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord> {
        self.has_table_write = true;
        self.stage_conversation(
            Some(ConversationParent {
                conversation_id: parent,
                at,
            }),
            ownership,
        )
    }

    fn stage_conversation(
        &mut self,
        parent: Option<ConversationParent>,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord> {
        let id = ConversationId(self.mint()?);
        let owner = match ownership {
            ConversationOwnership::Ownerless => None,
            ConversationOwnership::Task(task_id) => {
                let Some(task) = self.current_task(task_id)? else {
                    return Err(SessionError::Error(format!(
                        "Conversation owner task {} does not exist",
                        task_id.0
                    )));
                };
                Some(ConversationOwner {
                    conversation_id: task.conversation_id,
                    task_id,
                })
            }
        };
        let record = ConversationRecord { id, parent, owner };
        if let Some(parent) = parent {
            for (copy, source) in self.fork_copies(parent.conversation_id, parent.at, id)? {
                self.fork_source_documents.insert(source.id);
                let address = address_of(&copy);
                let entry = DocEntry {
                    address_id: address_id(&address),
                    definition: None,
                    draft: None,
                    target: Some(Target::ForkCopy { record: copy, source }),
                    retire: false,
                };
                self.latest.insert(entry.address_id.clone(), self.documents.len());
                self.documents.push(entry);
            }
            self.fork_source_conversations.insert(parent.conversation_id);
        }
        self.created_conversations.insert(id);
        self.writes.push(StorageWrite::Conversation { value: record.clone() });
        Ok(record)
    }

    /// Every persisted conversation document one fork copies: `asOf` documents of the entry's own conversation at the
    /// entry's commit, then `current` documents of the immediate parent (upstream `prepareForkDocumentCopies`).
    fn fork_copies(
        &mut self,
        parent: ConversationId,
        at: EntryId,
        child: ConversationId,
    ) -> Result<Vec<(DocumentCreate, DocumentCopySource)>> {
        let Some(entry) = self.read(|storage| storage.visible_entry(parent, at))? else {
            return Err(SessionError::Error(format!(
                "Entry {} is not visible from conversation {}",
                at.0, parent.0
            )));
        };
        let mut copies = Vec::new();
        let mut copied = HashSet::new();
        let selections = [
            (
                entry.entry.conversation_id,
                DocumentPoint::At(entry.commit_seq),
                DocumentFork::AsOf,
            ),
            (parent, DocumentPoint::Current, DocumentFork::Current),
        ];
        for (conversation_id, point, policy) in selections {
            self.collect_fork_copies(conversation_id, point, policy, child, &mut copied, &mut copies)?;
        }
        Ok(copies)
    }

    fn collect_fork_copies(
        &mut self,
        conversation_id: ConversationId,
        point: DocumentPoint,
        policy: DocumentFork,
        child: ConversationId,
        copied: &mut HashSet<String>,
        copies: &mut Vec<(DocumentCreate, DocumentCopySource)>,
    ) -> Result<()> {
        let query = DocumentQuery {
            scope: DocumentScope::Conversation { conversation_id },
            at: point,
            kind: None,
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = self.read(|storage| storage.scan_documents(&query, INTERNAL_SCAN_PAGE_SIZE, cursor.as_ref()))?;
            for source in page.items {
                self.stage_fork_copy_source(&source, point, policy, child, copied, copies)?;
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
        }
    }

    fn stage_fork_copy_source(
        &mut self,
        source: &DocumentRecord,
        point: DocumentPoint,
        policy: DocumentFork,
        child: ConversationId,
        copied: &mut HashSet<String>,
        copies: &mut Vec<(DocumentCreate, DocumentCopySource)>,
    ) -> Result<()> {
        if !matches!(source.identity.scope, DocumentScope::Conversation { .. }) || source.identity.fork != Some(policy)
        {
            return Ok(());
        }
        let id = DocumentId(self.mint()?);
        let record = DocumentCreate {
            id,
            kind: source.identity.kind.clone(),
            key: source.identity.key.clone(),
            scope: DocumentScope::Conversation { conversation_id: child },
            history: source.identity.history,
            fork: source.identity.fork,
        };
        if !copied.insert(address_id(&address_of(&record))) {
            let member = match &record.key {
                None => record.kind.clone(),
                Some(key) => format!("{}/{}", record.kind, key),
            };
            return Err(SessionError::Error(format!(
                "Fork selects multiple source documents for {member}"
            )));
        }
        copies.push((
            record,
            DocumentCopySource {
                id: source.identity.id,
                at: point,
            },
        ));
        Ok(())
    }

    /// Append an entry; `head: SelfId` starts the active context at the new entry.
    pub fn append_entry(&mut self, conversation_id: ConversationId, draft: EntryDraft) -> Result<EntryRecord> {
        self.has_table_write = true;
        self.require_conversation(conversation_id)?;
        let id = EntryId(self.mint()?);
        let record = EntryRecord {
            id,
            conversation_id,
            kind: draft.kind,
            model: draft.model,
            data: draft.data,
            head: draft.head.map(|head| match head {
                EntryHead::Entry(entry) => entry,
                EntryHead::SelfId => id,
            }),
            edits: draft.edits,
            by_task_id: self.scope.task_id,
        };
        self.writes.push(StorageWrite::Entry { value: record.clone() });
        Ok(record)
    }

    pub fn create_task(&mut self, task: &TaskDefinition, input: Value, options: TaskOptions) -> Result<TaskId> {
        self.has_table_write = true;
        let owner = match options.ownership {
            TaskOwnership::Conversation => None,
            TaskOwnership::Task(owner_id) => {
                let Some(owner) = self.current_task(owner_id)? else {
                    return Err(SessionError::Error(format!("Task owner {} does not exist", owner_id.0)));
                };
                if options.background {
                    return Err(SessionError::Type("A child task cannot be background".into()));
                }
                if options.conversation_id.is_some_and(|id| id != owner.conversation_id) {
                    return Err(SessionError::Error(format!(
                        "A child task lives in its owner's conversation {}",
                        owner.conversation_id.0
                    )));
                }
                Some(owner)
            }
        };
        let Some(conversation_id) = owner
            .as_ref()
            .map(|owner| owner.conversation_id)
            .or(options.conversation_id)
            .or(self.scope.conversation_id)
        else {
            return Err(SessionError::Type(
                "Tx.createTask() requires options.conversationId".into(),
            ));
        };
        self.require_conversation(conversation_id)?;
        let checkpoint = (task.initial)(&input);
        let id = TaskId(self.mint()?);
        let record = TaskRecord {
            id,
            conversation_id,
            kind: task.name.clone(),
            version: task.version,
            input,
            owner: owner.map(|owner| owner.id),
            background: options.background,
            abort_requested: false,
            abort_reason: None,
            abandon_on_restart: options.abandon_on_restart,
            started_at: None,
            ended_at: None,
            state: TaskState::Pending { checkpoint },
            memos: None,
        };
        self.tasks.insert(
            id,
            TxTask {
                write: Some((TaskWrite::Create, record)),
                ..TxTask::default()
            },
        );
        Ok(id)
    }

    /// Create a raw submission record with a fresh ID. No admission rules apply.
    pub fn create_submission(&mut self, create: SubmissionCreate) -> Result<SubmissionRecord> {
        self.has_table_write = true;
        self.require_conversation(create.conversation_id)?;
        let id = SubmissionId(self.mint()?);
        let record = SubmissionRecord {
            id,
            conversation_id: create.conversation_id,
            request_id: create.request_id,
            kind: create.kind,
            status: create.status,
            entry: create.entry,
            answer: create.answer,
            reason: create.reason,
            detail: create.detail,
        };
        self.submissions.insert(id, record.clone());
        Ok(record)
    }

    /// Settle a queued or placed submission; resolved during assembly against this transaction's latest record.
    pub fn settle_submission(&mut self, id: SubmissionId, settlement: SubmissionSettlement) {
        self.has_table_write = true;
        self.submission_changes.push((id, SubmissionChange::Settle(settlement)));
    }

    /// Place a queued submission at `entry`: an input becomes `placed`, a write `done`.
    pub fn place_submission(&mut self, id: SubmissionId, entry: EntryId) {
        self.has_table_write = true;
        self.submission_changes.push((id, SubmissionChange::Place(entry)));
    }

    /// Replace one task record completely, stamping `startedAt` on its first change to `running` and `endedAt` on the
    /// change to `terminal`.
    pub fn set_task(&mut self, value: TaskRecord) -> Result<()> {
        self.has_table_write = true;
        let now = &self.inner.now;
        let task = self.tasks.entry(value.id).or_default();
        let candidate = task.write.as_ref().map(|(_, record)| record);
        if let Some(candidate) = candidate {
            if candidate.state.status() == TaskStatus::Terminal {
                return Err(SessionError::Error(format!(
                    "Task {} already has a terminal candidate",
                    value.id.0
                )));
            }
            if candidate.conversation_id != value.conversation_id {
                return Err(SessionError::Error(format!(
                    "Task {} cannot change conversations",
                    value.id.0
                )));
            }
        }
        let status = value.state.status();
        let started_at = candidate
            .and_then(|candidate| candidate.started_at)
            .or(value.started_at)
            .or_else(|| (status == TaskStatus::Running).then(now));
        let ended_at = candidate
            .and_then(|candidate| candidate.ended_at)
            .or(value.ended_at)
            .or_else(|| (status == TaskStatus::Terminal).then(now));
        let kind = match task.write {
            Some((TaskWrite::Create, _)) => TaskWrite::Create,
            _ => TaskWrite::Replace,
        };
        task.write = Some((
            kind,
            TaskRecord {
                started_at,
                ended_at,
                ..value
            },
        ));
        Ok(())
    }

    /// Acquire a draft of a singleton document, creating it from `initial` when absent.
    pub fn doc(&mut self, doc: &Doc, owner: Owner) -> Result<Draft> {
        let resolved = resolve_address(doc.definition(), owner)?;
        if let DocumentScope::Task { task_id } = resolved.address.scope
            && self
                .tasks
                .get(&task_id)
                .and_then(|task| task.write.as_ref())
                .is_some_and(|(_, record)| record.state.status() == TaskStatus::Terminal)
        {
            return Err(SessionError::Error(format!("Task {} is terminal", task_id.0)));
        }
        let latest = self.latest.get(&resolved.id).copied();
        if let Some(index) = latest
            && !self.documents[index].retire
        {
            if let Some(draft) = &self.documents[index].draft {
                return Ok(draft.clone());
            }
            if matches!(self.documents[index].target, Some(Target::ForkCopy { .. })) {
                return self.acquire_fork_copy(index, doc);
            }
        }
        let skip_load = latest.is_some_and(|index| self.documents[index].retire);
        let (target, draft) = self.acquire(doc, &resolved, skip_load)?;
        self.latest.insert(resolved.id.clone(), self.documents.len());
        self.documents.push(DocEntry {
            address_id: resolved.id,
            definition: Some(doc.clone()),
            draft: Some(draft.clone()),
            target: Some(target),
            retire: false,
        });
        Ok(draft)
    }

    fn acquire(&mut self, doc: &Doc, resolved: &Resolved, skip_load: bool) -> Result<(Target, Draft)> {
        let definition = doc.definition();
        if !skip_load
            && let Some(loaded) = self
                .inner
                .load(self.line, definition, &resolved.id, &resolved.address)?
        {
            return begin_loaded_draft(definition, loaded);
        }
        self.validate_new_document_scope(resolved.address.scope)?;
        self.create_document_target(definition, &resolved.address)
    }

    fn validate_new_document_scope(&mut self, scope: DocumentScope) -> Result<()> {
        match scope {
            DocumentScope::Conversation { conversation_id } => self.require_conversation(conversation_id),
            DocumentScope::Task { task_id } => self.require_live_task(task_id),
            DocumentScope::Session => Ok(()),
        }
    }

    fn require_live_task(&mut self, task_id: TaskId) -> Result<()> {
        match self.current_task(task_id)? {
            None => Err(SessionError::Error(format!("Task {} does not exist", task_id.0))),
            Some(task) if task.state.status() == TaskStatus::Terminal => {
                Err(SessionError::Error(format!("Task {} is terminal", task_id.0)))
            }
            Some(_) => Ok(()),
        }
    }

    fn create_document_target(
        &mut self,
        definition: &DocDefinition,
        address: &DocumentAddress,
    ) -> Result<(Target, Draft)> {
        let value = object((definition.initial)(&Value::Null), &definition.kind)?;
        let id = DocumentId(self.mint()?);
        let tracker = Tracker::new(value);
        let draft = tracker.begin_change()?;
        let record = document_create(definition, address, id);
        Ok((
            Target::Created {
                record,
                version: definition.version,
                tracker,
            },
            draft,
        ))
    }

    /// Hydrate a staged fork copy from its exact source for typed access; the copy then writes a creation base.
    fn acquire_fork_copy(&mut self, index: usize, doc: &Doc) -> Result<Draft> {
        let Some(Target::ForkCopy { record, source }) = &self.documents[index].target else {
            unreachable!("checked by the caller");
        };
        let (record, source) = (record.clone(), *source);
        let Some(stored) = self.read(|storage| storage.document(source.id, source.at))? else {
            return Err(SessionError::Error(format!(
                "Fork source document {} cannot be read",
                source.id.0
            )));
        };
        let original = &stored.record.identity;
        if !matches!(original.scope, DocumentScope::Conversation { .. })
            || original.kind != record.kind
            || original.key != record.key
            || original.history != record.history
            || original.fork != record.fork
        {
            return Err(SessionError::Error(format!(
                "Fork source document {} does not match the copied record",
                source.id.0
            )));
        }
        let definition = doc.definition();
        let value = materialize_value(definition, &record, stored.version, &stored.value)?;
        let tracker = Tracker::new(value);
        let draft = tracker.begin_change()?;
        let entry = &mut self.documents[index];
        entry.definition = Some(doc.clone());
        entry.target = Some(Target::Created {
            record,
            version: definition.version,
            tracker,
        });
        entry.draft = Some(draft.clone());
        Ok(draft)
    }

    fn require_conversation(&mut self, id: ConversationId) -> Result<()> {
        if self.created_conversations.contains(&id) || self.read(|storage| storage.conversation(id))?.is_some() {
            Ok(())
        } else {
            Err(SessionError::Error(format!("Conversation {} does not exist", id.0)))
        }
    }

    /// Latest candidate task record, falling back to committed state; not a caller table read.
    fn current_task(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        if let Some((_, record)) = self.tasks.get(&id).and_then(|task| task.write.as_ref()) {
            return Ok(Some(record.clone()));
        }
        self.committed_task(id)
    }

    fn committed_task(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        if let Some(Some(committed)) = self.tasks.get(&id).map(|task| &task.committed) {
            return Ok(committed.clone());
        }
        let committed = self.read(|storage| storage.task(id))?;
        self.tasks.entry(id).or_default().committed = Some(committed.clone());
        Ok(committed)
    }
}

fn begin_loaded_draft(definition: &DocDefinition, loaded: Loaded) -> Result<(Target, Draft)> {
    let draft = {
        let document = lock(&loaded);
        check_record_scope(definition, &document.record.identity)?;
        check_record_version(definition, &document.record.identity, document.stored_version)?;
        document.tracker.begin_change()?
    };
    Ok((Target::Loaded(loaded), draft))
}
