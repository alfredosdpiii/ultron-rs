//! Public and internal types for Session transactions.

use crate::documents::Doc;
use crate::session::{Loaded, Result, SessionError};
use crate::{
    ConversationId, DocumentCopySource, DocumentCreate, DocumentRecord, EntryId, StorageWrite, SubmissionRecord,
    SubmissionStatus, SubmissionType, TaskId, TaskRecord,
};
use chord_delta::{Draft, Prepared, Tracker};
use serde_json::Value;

/// Executable task definition as far as the Session needs it: its registered name, version, and first checkpoint.
pub struct TaskDefinition {
    pub name: String,
    pub version: u64,
    pub initial: Box<dyn Fn(&Value) -> Value + Send + Sync>,
}

impl TaskDefinition {
    pub fn new(name: &str, version: u64, initial: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        TaskDefinition {
            name: name.to_owned(),
            version,
            initial: Box::new(initial),
        }
    }
}

/// Who owns a task: its conversation (a top-level task) or another task of the same conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskOwnership {
    #[default]
    Conversation,
    Task(TaskId),
}

/// Creation options for a durable task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskOptions {
    pub ownership: TaskOwnership,
    /// Default: the owner task's conversation, or the transaction's bound conversation.
    pub conversation_id: Option<ConversationId>,
    /// Conversation-owned tasks only.
    pub background: bool,
    pub abandon_on_restart: bool,
}

/// Ownership selected explicitly whenever a conversation is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationOwnership {
    Ownerless,
    Task(TaskId),
}

/// First entry of the active context an entry selects; `SelfId` is the entry's own, newly assigned ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryHead {
    Entry(EntryId),
    SelfId,
}

/// Entry content supplied before the Session assigns identity and task attribution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntryDraft {
    pub kind: String,
    pub model: Option<Vec<Value>>,
    pub data: Option<Value>,
    pub head: Option<EntryHead>,
    pub edits: Option<Vec<Value>>,
}

impl EntryDraft {
    pub fn new(kind: &str) -> EntryDraft {
        EntryDraft {
            kind: kind.to_owned(),
            ..EntryDraft::default()
        }
    }
}

/// Submission fields supplied before the Session assigns an ID. No admission rules apply to a raw creation.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionCreate {
    pub conversation_id: ConversationId,
    pub request_id: Option<String>,
    pub kind: SubmissionType,
    pub status: SubmissionStatus,
    pub entry: Option<EntryId>,
    pub answer: Option<EntryId>,
    pub reason: Option<String>,
    pub detail: Option<Value>,
}

/// Terminal status staged for a submission.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionSettlement {
    Done { answer: EntryId },
    Unanswered { reason: String, detail: Option<Value> },
}

pub(super) enum SubmissionChange {
    Settle(SubmissionSettlement),
    Place(EntryId),
}

/// Complete record after one change: placement turns a queued input `placed` and a queued write `done`; only a placed
/// input can be answered; a settled record stays.
pub(super) fn apply_submission_change(
    current: &SubmissionRecord,
    change: &SubmissionChange,
) -> Result<SubmissionRecord> {
    if matches!(current.status, SubmissionStatus::Done | SubmissionStatus::Unanswered) {
        return Ok(current.clone());
    }
    let mut next = current.clone();
    match change {
        SubmissionChange::Place(entry) => {
            if current.status != SubmissionStatus::Queued {
                return Err(SessionError::Error(format!(
                    "Submission {} is not queued",
                    current.id.0
                )));
            }
            next.status = match current.kind {
                SubmissionType::Input => SubmissionStatus::Placed,
                SubmissionType::Write => SubmissionStatus::Done,
            };
            next.entry = Some(*entry);
        }
        SubmissionChange::Settle(SubmissionSettlement::Done { answer }) => {
            if current.status != SubmissionStatus::Placed {
                return Err(SessionError::Error(format!(
                    "Submission {} is not a placed input",
                    current.id.0
                )));
            }
            next.status = SubmissionStatus::Done;
            next.answer = Some(*answer);
        }
        SubmissionChange::Settle(SubmissionSettlement::Unanswered { reason, detail }) => {
            next.status = SubmissionStatus::Unanswered;
            next.reason = Some(reason.clone());
            next.detail = detail.clone();
        }
    }
    Ok(next)
}

/// Defaults a commit binds to: the `create_task` conversation and the task attributed to appended entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxScope {
    pub conversation_id: Option<ConversationId>,
    pub task_id: Option<TaskId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskWrite {
    Create,
    Replace,
}

#[derive(Default)]
pub(super) struct TxTask {
    pub(super) committed: Option<Option<TaskRecord>>,
    pub(super) write: Option<(TaskWrite, TaskRecord)>,
    pub(super) publication_conversation: Option<ConversationId>,
}

/// Storage/cache provenance of one staged incarnation.
pub(super) enum Target {
    Loaded(Loaded),
    Created {
        record: DocumentCreate,
        version: u64,
        tracker: Tracker,
    },
    ForkCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
}

/// One incarnation acquired, created, copied, or retired by this transaction.
pub(super) struct DocEntry {
    pub(super) address_id: String,
    /// Absent for definition-free fork copies.
    pub(super) definition: Option<Doc>,
    pub(super) draft: Option<Draft>,
    pub(super) target: Option<Target>,
    pub(super) retire: bool,
}

pub(super) enum PlanRecord {
    New(DocumentCreate),
    Committed(DocumentRecord),
}

impl PlanRecord {
    pub(super) fn identity(&self) -> &DocumentCreate {
        match self {
            PlanRecord::New(record) => record,
            PlanRecord::Committed(record) => &record.identity,
        }
    }
}

pub(super) enum PlanTracker {
    Created(Tracker),
    Loaded(Loaded),
}

pub(super) struct PlanChange {
    pub(super) tracker: PlanTracker,
    pub(super) prepared: Prepared,
    pub(super) version: u64,
    pub(super) definition: Option<Doc>,
}

/// What one staged incarnation writes and publishes, decided before Storage admission so adoption only applies it.
pub(crate) struct Plan {
    pub(super) address_id: String,
    pub(super) record: PlanRecord,
    pub(super) retire: bool,
    pub(super) content: Option<StorageWrite>,
    pub(super) change: Option<PlanChange>,
    pub(super) conversation_id: Option<ConversationId>,
}

impl Plan {
    /// Every creation, copy, and retirement publishes, and a loaded incarnation that writes content (a
    /// migration-only base included, so observers of the older shape receive the new value).
    pub(super) fn publishes(&self) -> bool {
        self.retire
            || !matches!(
                self.change,
                Some(PlanChange {
                    tracker: PlanTracker::Loaded(_),
                    ..
                })
            )
            || self.content.is_some()
    }
}

pub(super) fn object_of(value: &Value) -> crate::JsonObject {
    value.as_object().cloned().expect("document values are objects")
}
