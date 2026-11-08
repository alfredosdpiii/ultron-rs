//! The delegation host's view of the one task journal. The journal itself (`task-store.ts`: validation, bounded
//! persistence, recovery that interrupts unfinished tasks, the error texts) lives once, in `harness::task_store`;
//! this module keeps the JSON-valued API `host.rs` uses and the boxed [`TaskStore`] it is configured with.

pub use harness::task_store::task_fingerprint;
use harness::task_store::{TaskDocumentStore, TaskState};
use serde_json::Value;

/// Atomic document replacement (`NativeHostStore`).
pub trait TaskStore: Send {
    fn read(&mut self) -> Result<Option<Value>, String>;
    fn write(&mut self, document: &Value) -> Result<(), String>;
}

/// An in-memory store (the session value in tests).
#[derive(Default)]
pub struct MemoryStore {
    pub value: Option<Value>,
}

impl TaskStore for MemoryStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.value.clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.value = Some(document.clone());
        Ok(())
    }
}

struct Store(Box<dyn TaskStore>);

impl TaskDocumentStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        self.0.read()
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.0.write(document)
    }
}

/// `NativeTaskJournal`, synchronous: callers serialize access (the host keeps it behind a mutex).
pub struct TaskJournal(harness::task_store::TaskJournal<Store>);

impl TaskJournal {
    pub fn new(store: Box<dyn TaskStore>) -> Self {
        TaskJournal(harness::task_store::TaskJournal::new(Store(store)))
    }

    pub fn list(&mut self) -> Result<Vec<Value>, String> {
        let tasks = self.0.list().map_err(|error| error.to_string())?;
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
