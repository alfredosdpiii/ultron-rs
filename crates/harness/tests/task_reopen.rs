//! `TaskJournal::reopen`: a new owner keeps the unfinished tasks it reattaches `running` and records the others
//! `interrupted`, in one write (specs/spawn-collect, `fix_two_restarts`). New in Rust: literal expectations.

use harness::task_store::{TaskJournal, TaskState};
use kernel::store::DocumentStore;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Store {
    value: Arc<Mutex<Option<Value>>>,
    writes: Arc<Mutex<usize>>,
}

impl DocumentStore for Store {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.value.lock().unwrap().clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        *self.writes.lock().unwrap() += 1;
        *self.value.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

const FINGERPRINT: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn task(id: &str, state: &str) -> Value {
    json!({"id": id, "definition": "rlm-child@1", "fingerprint": FINGERPRINT, "key": format!("k-{id}"), "state": state})
}

fn seeded() -> Store {
    let store = Store::default();
    *store.value.lock().unwrap() =
        Some(json!({"version": 1, "tasks": [task("a", "running"), task("b", "admitted"), task("c", "running")]}));
    store
}

fn states(store: &Store) -> Vec<(String, String)> {
    let document = store.value.lock().unwrap().clone().unwrap();
    document["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["id"].as_str().unwrap().to_string(),
                t["state"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

#[test]
fn task_store_reopen_keeps_the_reattached_tasks_running_in_one_write() {
    let store = seeded();
    let mut journal = TaskJournal::new(store.clone());
    let tasks = journal.reopen(&mut |task| task.id != "c").unwrap();
    assert_eq!(*store.writes.lock().unwrap(), 1);
    assert_eq!(
        states(&store),
        pairs(&[("a", "running"), ("b", "running"), ("c", "interrupted")])
    );
    assert!(tasks.iter().filter(|t| t.id != "c").all(|t| t.result.is_none()));
    // A kept task ends as any task does, and its end is journaled.
    let reply = json!({"status": "succeeded", "value": "done", "verification": "unverified"});
    let ended = journal
        .transition("a", TaskState::Completed, Some(reply.clone()))
        .unwrap();
    assert_eq!(ended.result, Some(reply));
    // The next owner finds it finished: it is neither reattached nor interrupted.
    let mut asked = Vec::new();
    let mut next = TaskJournal::new(store.clone());
    next.reopen(&mut |task| {
        asked.push(task.id.clone());
        true
    })
    .unwrap();
    assert_eq!(asked, vec!["b".to_string()]);
    assert_eq!(
        states(&store),
        pairs(&[("a", "completed"), ("b", "running"), ("c", "interrupted")])
    );
}

#[test]
fn task_store_reopen_after_the_first_use_moves_the_recovered_tasks_back() {
    let store = seeded();
    let mut journal = TaskJournal::new(store.clone());
    journal.list().unwrap();
    assert_eq!(
        states(&store),
        pairs(&[("a", "interrupted"), ("b", "interrupted"), ("c", "interrupted")])
    );
    journal.reopen(&mut |task| task.id == "a").unwrap();
    assert_eq!(
        states(&store),
        pairs(&[("a", "running"), ("b", "interrupted"), ("c", "interrupted")])
    );
    assert_eq!(*store.writes.lock().unwrap(), 2);
    // Only once: what this owner recovered is not reopened again.
    journal.reopen(&mut |_| true).unwrap();
    assert_eq!(
        states(&store),
        pairs(&[("a", "running"), ("b", "interrupted"), ("c", "interrupted")])
    );
    assert_eq!(*store.writes.lock().unwrap(), 2);
}
