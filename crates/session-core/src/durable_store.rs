//! The session's Pi Durable storage, kept in the session's own format-4 journal: the durable conversation runtime
//! (`harness::chat`: generations, REPL cells, the inbox, children) commits through the storage [`open`] returns, which
//! validates each commit against an in-memory [`MemoryStorage`] and appends it to the session file as one
//! `ultron.module` list element (`durable`: `{seq, writes}`) before applying it. Reopening the session replays those
//! elements, so every durable record survives a crash with the transcript it belongs to, in one file.
//!
//! `ultron.module` is the namespace of Ultron's own module state: the TypeScript product keeps it opaque, and the
//! cross-implementation journal comparison leaves it out, so the TypeScript-visible records are unchanged.
//!
//! Each commit is also shown to an [`Observer`] after it is durable: the session actor projects the runtime's
//! progress (answers, cell results) onto the TypeScript transcript from there.

use durable::{MemoryStorage, PreparedCommit, Seq, StorageError, StorageWrite};
use serde_json::json;
use session_journal::{Session, Write};
use std::sync::{Arc, Mutex, MutexGuard};

/// The session journal, shared by the session actor and the durable runtime's commits.
pub type SharedSession = Arc<Mutex<Session>>;

pub fn lock(session: &SharedSession) -> MutexGuard<'_, Session> {
    jsrt::sync::lock(session)
}

/// Sees every durable commit after it is in the journal (on the committing thread; it must not block).
pub type Observer = Arc<dyn Fn(&[StorageWrite]) + Send + Sync>;

fn failed(message: impl Into<String>) -> StorageError {
    StorageError::Failed(message.into())
}

/// The durable commits a session file holds, in order, as `(seq, writes)`.
pub fn stored_commits(session: &Session) -> Result<Vec<(u64, Vec<StorageWrite>)>, StorageError> {
    session
        .state()
        .read_list(session_journal::ns::MODULE, session_journal::ns::DURABLE_KEY)
        .into_iter()
        .map(|element| {
            let seq = element.value["seq"]
                .as_u64()
                .ok_or_else(|| StorageError::Corrupt("A durable record has no seq".into()))?;
            let writes: Vec<StorageWrite> = serde_json::from_value(element.value["writes"].clone())
                .map_err(|e| StorageError::Corrupt(format!("A durable record does not parse: {e}")))?;
            Ok((seq, writes))
        })
        .collect()
}

/// Pi Durable storage inside a format-4 session file: a [`MemoryStorage`] holding the durable records `session`
/// has, whose every later commit is appended to it (then shown to `observer`) before it is applied.
pub fn open(session: SharedSession, observer: Option<Observer>) -> Result<MemoryStorage, StorageError> {
    let mut memory = MemoryStorage::new();
    for (seq, writes) in stored_commits(&lock(&session))? {
        let prepared = memory.prepare_commit(writes, Some(Seq(seq)))?;
        memory.apply_commit(prepared);
    }
    Ok(memory.with_commit_sink(Box::new(move |prepared: &PreparedCommit| {
        let record = json!({
            "seq": prepared.seq.0,
            "writes": serde_json::to_value(&prepared.writes).map_err(|e| failed(e.to_string()))?,
        });
        lock(&session)
            .commit(&[Write::append(
                session_journal::ns::MODULE,
                session_journal::ns::DURABLE_KEY,
                record,
            )])
            .map_err(failed)?;
        if let Some(observer) = &observer {
            observer(&prepared.writes);
        }
        Ok(())
    })))
}

pub fn has_records(session: &Session) -> bool {
    !session
        .state()
        .read_list(session_journal::ns::MODULE, session_journal::ns::DURABLE_KEY)
        .is_empty()
}
