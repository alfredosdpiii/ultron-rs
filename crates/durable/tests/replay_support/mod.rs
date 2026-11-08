//! A model of how the product's durable storage reopens (session-core `durable_store::open`): every commit is validated
//! by a `MemoryStorage` working copy and logged as JSON `{seq, writes}` (`durable_store::open` appends that record to the
//! session journal); a reopen replays the log into a fresh `MemoryStorage`, as `durable_store::open` replays the
//! journal. So a reopen keeps exactly the acknowledged commits, through a JSON round trip, and nothing in memory
//! (IDs minted but never stored are not reserved again). The real journal storage over a session file is checked
//! against the upstream conformance cases and the kill tests in `crates/session-core/tests`.

#![allow(dead_code)]

use durable::{MemoryStorage, PreparedCommit, Seq, StorageWrite};
use std::sync::{Arc, Mutex, PoisonError};

/// The durable log every handle opened from it appends to.
#[derive(Clone, Default)]
pub struct ReplayLog(Arc<Mutex<Vec<String>>>);

impl ReplayLog {
    /// A handle over everything committed so far, as a fresh process would open it.
    pub fn open(&self) -> MemoryStorage {
        let mut memory = MemoryStorage::new();
        for record in self.0.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            let record: serde_json::Value = serde_json::from_str(record).expect("a logged commit parses");
            let writes: Vec<StorageWrite> = serde_json::from_value(record["writes"].clone()).expect("logged writes");
            let seq = Seq(record["seq"].as_u64().expect("a logged seq"));
            let prepared = memory
                .prepare_commit(writes, Some(seq))
                .expect("a logged commit replays");
            memory.apply_commit(prepared);
        }
        let log = self.clone();
        memory.with_commit_sink(Box::new(move |prepared: &PreparedCommit| {
            let record = serde_json::json!({ "seq": prepared.seq.0, "writes": prepared.writes });
            log.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(record.to_string());
            Ok(())
        }))
    }
}
