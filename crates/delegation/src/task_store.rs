//! The delegation host's view of the one task journal. The journal itself (`task-store.ts`: validation, bounded
//! persistence, recovery that interrupts unfinished tasks, the error texts) lives once, in `harness::task_store`;
//! this module keeps the JSON-valued API `host.rs` uses over the boxed [`DocumentStore`] it is configured with.

use harness::task_store::TaskState;
use kernel::store::DocumentStore;
use serde_json::Value;

/// `NativeTaskJournal`, synchronous: callers serialize access (the host keeps it behind a mutex).
pub struct TaskJournal(harness::task_store::TaskJournal<Box<dyn DocumentStore>>);

impl TaskJournal {
    pub fn new(store: Box<dyn DocumentStore>) -> Self {
        TaskJournal(harness::task_store::TaskJournal::new(store))
    }

    pub fn list(&mut self) -> Result<Vec<Value>, String> {
        let tasks = self.0.list().map_err(|error| error.to_string())?;
        Ok(tasks.iter().map(|task| self.0.stored_json(task)).collect())
    }

    /// Opens the journal for a new owner ([`harness::task_store::TaskJournal::reopen`]): the unfinished records
    /// `keep` accepts stay `running`, every other unfinished one is recorded `interrupted`.
    pub fn reopen(&mut self, mut keep: impl FnMut(&Value) -> bool) -> Result<Vec<Value>, String> {
        let tasks = self
            .0
            .reopen(&mut |task| keep(&task.to_json()))
            .map_err(|error| error.to_string())?;
        Ok(tasks.iter().map(|task| self.0.stored_json(task)).collect())
    }

    pub fn find(&mut self, id: &str) -> Option<Value> {
        self.list().ok()?.into_iter().find(|task| task["id"] == id)
    }

    /// Admit a task; returns `(task, created)`. A known key with the same fingerprint returns the earlier task.
    pub fn admit(
        &mut self,
        definition: &str,
        fingerprint: &str,
        key: &str,
        parent_id: Option<&str>,
    ) -> Result<(Value, bool), String> {
        let (task, created) = self
            .0
            .admit(definition, fingerprint, key, parent_id)
            .map_err(|error| error.to_string())?;
        Ok((self.0.stored_json(&task), created))
    }

    pub fn transition(&mut self, id: &str, state: &str, result: Option<Value>) -> Result<Value, String> {
        self.0.ready().map_err(|error| error.to_string())?;
        if id.is_empty() {
            return Err("task ID must be nonempty".into());
        }
        let state = TaskState::parse(state).ok_or("Invalid task state")?;
        let task = self
            .0
            .transition(id, state, result)
            .map_err(|error| error.to_string())?;
        Ok(self.0.stored_json(&task))
    }
}
