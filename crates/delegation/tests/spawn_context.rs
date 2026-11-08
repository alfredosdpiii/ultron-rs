//! `rlm.spawn(context=...)` (`fresh`, `fork`, `retrieve`) and `rlm.progress` in the subagent host: a retrieve child's
//! brief names its parent's session and how to search it; the option is checked against `fork=`; a running child's
//! progress is a described level with forecasts, read from its transcript. New in Rust: literal expectations.

use delegation::fork::{FORK_BRIEF_NOTE, SpawnContext, retrieve_brief_note};
use delegation::host::{ChildRunner, Delegation, DelegationOptions};
use delegation::progress::{Activity, STALL_MS, report};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

fn host(runner: ChildRunner) -> Delegation {
    let mut options = DelegationOptions::new(runner);
    options.session_id = "sess-1".into();
    options.now = Box::new(|| 1_000_000.0);
    Delegation::new(options).0
}

fn spawn(delegation: &Delegation, kwargs: Value) -> Result<Value, String> {
    let mut kwargs = kwargs;
    kwargs["name"] = json!("helper");
    delegation.handle("rlm.spawn", json!({ "prompt": "Brief B", "kwargs": kwargs }), "main")
}

#[test]
fn delegation_spawn_context_retrieve_names_the_parent_session_in_the_brief() {
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = prompts.clone();
    let delegation = host(Arc::new(move |run| {
        seen.lock().unwrap().push(run.prompt.clone());
        Ok("done".into())
    }));
    for kwargs in [json!({"context": "retrieve"}), json!({"context": "fresh"}), json!({})] {
        let child = spawn(&delegation, kwargs).unwrap();
        assert_eq!(child["parent_branch_anchor"], "");
        delegation.wait(&[child["rlm_child_id"].as_str().unwrap().to_string()], Some(HANG));
    }
    let prompts = prompts.lock().unwrap().clone();
    assert_eq!(prompts[0], format!("{}\n\nBrief B", retrieve_brief_note("sess-1")));
    assert!(
        prompts[0].contains(r#"sessions.search("regex", sessions=["sess-1"])"#),
        "{}",
        prompts[0]
    );
    assert_eq!(prompts[1], "Brief B");
    assert_eq!(prompts[2], "Brief B");
    assert!(!FORK_BRIEF_NOTE.is_empty());
}

#[test]
fn delegation_spawn_context_is_checked_against_fork() {
    let of = |context: Value, fork: Value| {
        let (c, f) = (
            Some(&context).filter(|v| !v.is_null()),
            Some(&fork).filter(|v| !v.is_null()),
        );
        SpawnContext::of(c, f)
    };
    assert_eq!(of(Value::Null, Value::Null), Ok(SpawnContext::Fresh));
    assert_eq!(of(Value::Null, json!(true)), Ok(SpawnContext::Fork));
    assert_eq!(of(json!("fork"), json!(true)), Ok(SpawnContext::Fork));
    assert_eq!(of(json!("fork"), json!(false)), Ok(SpawnContext::Fork));
    assert_eq!(of(json!("retrieve"), json!(false)), Ok(SpawnContext::Retrieve));
    assert_eq!(
        of(json!("retrieve"), json!(true)),
        Err("rlm.spawn fork=True is context=\"fork\": pass one of them".into())
    );
    assert_eq!(
        of(json!("all"), Value::Null),
        Err("context must be \"fresh\", \"fork\" or \"retrieve\"".into())
    );
    assert_eq!(of(Value::Null, json!("yes")), Err("fork must be True or False".into()));
    // Through the host: a bad context admits nothing.
    let delegation = host(Arc::new(|_| Ok("done".into())));
    assert_eq!(
        spawn(&delegation, json!({"context": 3})),
        Err("context must be \"fresh\", \"fork\" or \"retrieve\"".into())
    );
    assert!(delegation.tasks().unwrap().is_empty());
}

fn assistant(code: &str, cost: f64, at: f64) -> Value {
    json!({"role": "assistant", "timestamp": at, "usage": {"cost": {"total": cost}},
        "content": [{"type": "toolCall", "id": "c", "name": "rlm", "arguments": {"code": code}}]})
}

fn result(error: bool, at: f64) -> Value {
    json!({"role": "toolResult", "timestamp": at, "isError": error, "content": []})
}

#[test]
fn delegation_progress_levels_are_described_from_the_transcript() {
    let now = 10_000_000.0;
    let level = |messages: &[Value]| Activity::of(messages).level(now).0;
    let user = json!({"role": "user", "content": "brief", "timestamp": now});
    assert_eq!(level(std::slice::from_ref(&user)), "starting");
    let read = [
        user.clone(),
        assistant("print(read('a.py'))", 0.01, now),
        result(false, now),
    ];
    assert_eq!(level(&read), "exploring");
    let mut edited = read.to_vec();
    edited.extend([assistant("edit('a.py', 'x', 'y')", 0.01, now), result(false, now)]);
    assert_eq!(level(&edited), "changing");
    let mut checked = edited.clone();
    checked.extend([
        assistant("print(await bash('pytest -q'))", 0.01, now),
        result(false, now),
    ]);
    assert_eq!(level(&checked), "checking");
    let mut edited_again = checked.clone();
    edited_again.extend([assistant("write('b.py', 'z')", 0.01, now), result(false, now)]);
    assert_eq!(level(&edited_again), "changing");
    let mut finished = checked.clone();
    finished.push(assistant(
        "print(await rlm.finish('passed', 'ok', evidence=['pytest -q: ok']))",
        0.0,
        now,
    ));
    assert_eq!(level(&finished), "finishing");
    let mut failing = read.to_vec();
    for _ in 0..3 {
        failing.extend([assistant("1/0", 0.0, now), result(true, now)]);
    }
    assert_eq!(
        Activity::of(&failing).level(now),
        ("stalled", "Its last 3 cells failed in a row.".to_string())
    );
    let mut old = [user.clone(), assistant("print(1)", 0.0, now - STALL_MS - 60_000.0)];
    old[0]["timestamp"] = json!(now - STALL_MS - 60_000.0);
    assert_eq!(
        Activity::of(&old).level(now),
        ("stalled", "No activity for 6 min.".to_string())
    );
}

#[test]
fn delegation_progress_forecasts_time_and_cost_from_the_timeout_and_spend() {
    let now = 10_000_000.0;
    let messages = [
        assistant("print(1)", 0.2, now),
        result(false, now),
        assistant("print(2)", 0.1, now),
    ];
    // Two minutes in, ten-minute timeout: 0.3 USD so far, 0.15 USD a minute, 1.5 USD by the timeout.
    let entry = report(&messages, now, 120_000.0, Some(600_000));
    assert_eq!(entry["level"], "exploring");
    assert_eq!(
        (entry["responses"].clone(), entry["cells"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(entry["elapsed_s"], 120.0);
    let forecast = &entry["forecast"];
    assert_eq!(forecast["time_left_s_max"], 480.0);
    assert!(
        (forecast["cost_usd_per_min"].as_f64().unwrap() - 0.15).abs() < 1e-9,
        "{entry}"
    );
    assert!(
        (forecast["cost_usd_at_timeout"].as_f64().unwrap() - 1.5).abs() < 1e-9,
        "{entry}"
    );
    // No timeout known: no time bound and no projection.
    let unbounded = report(&messages, now, 120_000.0, None);
    assert_eq!(unbounded["forecast"]["time_left_s_max"], Value::Null);
    assert_eq!(unbounded["forecast"]["cost_usd_at_timeout"], Value::Null);
}

#[test]
fn delegation_progress_lists_running_children_with_their_transcript() {
    let release = Signal::new();
    let hold = release.clone();
    let delegation = host(Arc::new(move |_| {
        hold.wait(HANG);
        Ok("done".into())
    }));
    let lanes: Arc<Mutex<Vec<String>>> = Arc::default();
    let asked = lanes.clone();
    delegation.set_transcripts(Arc::new(move |lane: &str| {
        asked.lock().unwrap().push(lane.to_string());
        Some(vec![assistant("edit('a', 'b', 'c')", 0.5, 1_000_000.0)])
    }));
    let child = spawn(&delegation, json!({"timeout_ms": 60_000})).unwrap();
    let id = child["rlm_child_id"].as_str().unwrap().to_string();
    let listed = delegation
        .handle("rlm.progress", json!({"selectors": []}), "main")
        .unwrap();
    let entries = listed["subagents"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{listed}");
    assert_eq!(
        (
            entries[0]["id"].clone(),
            entries[0]["name"].clone(),
            entries[0]["level"].clone()
        ),
        (json!(id), json!("helper"), json!("changing"))
    );
    assert_eq!(entries[0]["cost_usd"], 0.5);
    assert_eq!(entries[0]["forecast"]["time_left_s_max"], 60.0);
    assert_eq!(*lanes.lock().unwrap(), vec![lanes::lane_names::child_lane(&id)]);
    release.abort("go");
    delegation.wait(std::slice::from_ref(&id), Some(HANG));
    // A finished child is left out.
    let listed = delegation
        .handle("rlm.progress", json!({"selectors": [id]}), "main")
        .unwrap();
    assert_eq!(listed["subagents"], json!([]));
}
