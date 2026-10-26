//! `ctx.state`, which the TUI polls every few seconds, reads the main branch again only when the branch changed: an
//! idle session's poll copied its whole transcript each time.

use kernel_apis::context_control::{CONTEXT_NOTE_CUSTOM_TYPE, ContextBranches, ContextControl};
use kernel_apis::util::lock;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Counted {
    entries: Mutex<Vec<Value>>,
    reads: Mutex<usize>,
}

impl ContextBranches for Counted {
    fn entries(&self, _lane: &str) -> Result<Vec<Value>, String> {
        *lock(&self.reads) += 1;
        Ok(lock(&self.entries).clone())
    }

    fn append_custom(&self, _lane: &str, _custom_type: &str, _data: &Value) -> Result<String, String> {
        Err("not used".into())
    }

    fn tip_id(&self, _lane: &str) -> Result<Option<String>, String> {
        Ok(lock(&self.entries)
            .last()
            .and_then(|e| e["id"].as_str().map(str::to_string)))
    }
}

fn note(id: &str) -> Value {
    json!({"id": id, "type": "custom", "customType": CONTEXT_NOTE_CUSTOM_TYPE, "data": {"text": id}})
}

#[test]
fn kernel_apis_ctx_state_rereads_the_branch_only_when_it_changed() {
    let branches = Arc::new(Counted::default());
    lock(&branches.entries).push(note("n1"));
    let control = ContextControl::new(branches.clone());

    let first = control.state().unwrap();
    assert_eq!(control.state().unwrap(), first);
    assert_eq!(*lock(&branches.reads), 1);
    assert_eq!(first["notes"], 1);

    lock(&branches.entries).push(note("n2"));
    assert_eq!(control.state().unwrap()["notes"], 2);
    assert_eq!(*lock(&branches.reads), 2);
}
