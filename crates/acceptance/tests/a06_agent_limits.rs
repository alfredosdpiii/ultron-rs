//! A06 (the frame and wall-time cases): "an artifact larger than the RPC frame never reaches the service from Python"
//! (as any host request: `artifacts.*` is not ported), "a task past its timeout is cancelled with a durable cancelled
//! result" (through `agents.spawn`, as the TS case does) and "the root wall deadline caps task timeouts and rejects
//! work after it passes" of packages/coding-agent/test/ultron-a06-limits.test.ts, over the real runtime.py kernel and
//! the Rust typed-agent host with its usage ledger.

use crate::agents_support;
use crate::common;

use agents_support::{Memory, Options, aborted, fixture};
use common::{Recorder, kernel_in, run, value};
use delegation::usage::{UsageLedger, UsageLimits};
use kernel::Status;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn acceptance_a06_a_request_larger_than_the_rpc_frame_never_reaches_the_host_from_python() {
    let (handler, requests) =
        Recorder::new(|_: &str, _: Map<String, Value>, _: &kernel::Signal| Ok(json!({ "id": "stored" })));
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path(), handler);
    value(&kernel, "kept = 1");
    // Refused in the cell (a ValueError), so the kernel and its variables survive.
    let refused = run(
        &kernel,
        "await rlm.host_request('notes.put', {'text': 'z' * (2 * 1024 * 1024)})",
    );
    assert_eq!(refused.status, Status::Error);
    let error = refused.error.unwrap();
    assert_eq!(error.ename, "ValueError");
    assert!(
        error.evalue.contains("over the kernel's 1 MiB frame"),
        "{}",
        error.evalue
    );
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(value(&kernel, "kept"), "1");
    assert_eq!(
        value(&kernel, "await rlm.host_request('notes.put', {'text': 'small'})"),
        "{'id': 'stored'}"
    );
    assert_eq!(*requests.lock().unwrap(), ["notes.put"]);
    kernel.shutdown();
}

/// The TS `nativeHost(store, usage)`: every task lane hangs until its task is aborted.
fn hanging(store: &Memory, usage: Option<UsageLedger>) -> agents_support::Fixture {
    fixture(Options {
        store: Some(store.clone()),
        usage,
        script: Some(Arc::new(|_, _, signal, _| aborted(signal))),
        ..Options::default()
    })
}

#[test]
fn acceptance_a06_an_agents_spawn_task_past_its_timeout_is_cancelled_with_a_durable_cancelled_result() {
    let store = Memory::default();
    let host = hanging(&store, None);
    let spawned = host
        .call(
            "agents.spawn",
            json!({ "definition": "security-reviewer@1", "input": { "request": "review" }, "timeout_ms": 50 }),
        )
        .unwrap();
    let result = host.call("agents.result", json!({ "id": spawned["id"] })).unwrap();
    assert_eq!(
        (result["status"].clone(), result["error"].clone()),
        (json!("cancelled"), json!("Ultron task exceeded 50ms timeout"))
    );
    host.agents.close();
    // A new host over the same journal sees the committed terminal result.
    let reopened = hanging(&store, None);
    let inspected = reopened.call("agents.inspect", json!({ "id": spawned["id"] })).unwrap();
    assert_eq!(inspected["state"], "cancelled");
    assert_eq!(
        (
            inspected["result"]["status"].clone(),
            inspected["result"]["error"].clone()
        ),
        (json!("cancelled"), json!("Ultron task exceeded 50ms timeout"))
    );
    reopened.agents.close();
}

#[test]
fn acceptance_a06_the_root_wall_deadline_caps_task_timeouts_and_rejects_work_after_it_passes() {
    let usage = UsageLedger::in_memory(UsageLimits {
        max_wall_ms: Some(200),
        ..UsageLimits::default()
    });
    let host = hanging(&Memory::default(), Some(usage));
    let spawned = host
        .call(
            "agents.spawn",
            json!({ "definition": "security-reviewer@1", "input": { "request": "review" }, "timeout_ms": 100 }),
        )
        .unwrap();
    assert_eq!(
        host.call("agents.result", json!({ "id": spawned["id"] })).unwrap()["status"],
        "cancelled"
    );
    std::thread::sleep(Duration::from_millis(200));
    let refused = host
        .call(
            "agents.spawn",
            json!({ "definition": "security-reviewer@1", "input": { "request": "late" }, "timeout_ms": 100 }),
        )
        .unwrap_err();
    assert!(refused.contains("Usage wall deadline exceeded"), "{refused}");
    host.agents.close();
}
