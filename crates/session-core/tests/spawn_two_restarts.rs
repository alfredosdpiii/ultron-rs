//! A subagent across two restarts, the counterexamples of specs/spawn-collect (`code_two_restarts`) replayed against
//! the code: three in-process workers over one private session file, two hand-overs.
//!
//! - KeepsRunning: a child reattached by the second worker and still running when it hands over is reattached again by
//!   the third (it used to be recorded `interrupted` when the second worker reopened the journal, so the third aborted
//!   it).
//! - OneResult: a reattached child's end is journaled, so a result the root collected in the second worker reads the
//!   same in the third (it used to be kept in memory only and read `interrupted` after the next hand-over).
//!
//! New in Rust: literal expectations.

use crate::p_session_support;
use crate::reopen_support;

use p_session_support::{HANG, Script, text, tool, tool_results, user_texts};
use reopen_support::{Home, Worker};
use serde_json::{Value, json};
use session_core::core::Op;
use std::path::Path;
use std::time::{Duration, Instant};

const HANDLE: &str = "handle.txt";

fn texts(request: &Value) -> Vec<String> {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .filter(|t| !t.starts_with("[clock]"))
        .map(str::to_string)
        .collect()
}

fn is_child(request: &Value) -> bool {
    texts(request).first().is_some_and(|t| t.contains("CHILD: fix status"))
}

/// A child cell that says it started and runs until `release` exists (a hang guard bounds it at about 120 s).
fn blocked_cell(started: &str, release: &str) -> String {
    format!(
        "import asyncio, os\nopen({}, 'w').write('1')\nfor _ in range(12000):\n    if os.path.exists({}):\n        break\n    await asyncio.sleep(0.01)\nprint('released')",
        json!(started),
        json!(release)
    )
}

/// A root cell that waits for the child's result and writes it to `out`.
fn collect_cell(out: &str) -> String {
    format!(
        "import asyncio, json\nhid = open({}).read()\nr = None\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([hid]))[0]['result']\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\nopen({}, 'w').write(json.dumps(r))\nprint('collected')",
        json!(HANDLE),
        json!(out)
    )
}

fn wait_file(path: &Path) {
    let end = Instant::now() + HANG;
    while !path.exists() {
        assert!(Instant::now() < end, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn prompt(worker: &Worker, message: &str) {
    let reply = worker
        .call(Op::PromptAndWait {
            message: message.into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(reply["error"], Value::Null, "{reply}");
}

fn result(home: &Home, file: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.cwd.join(file)).unwrap()).unwrap()
}

/// The subagent journal's last stored state of the child.
fn stored_state(home: &Home) -> Value {
    let documents = home.values("ultron.tasks");
    documents.last().unwrap()["tasks"][0]["state"].clone()
}

/// Worker 1: the root spawns the child, which is still running its cell when the worker hands over.
fn first_script() -> Script {
    let spawn = format!(
        "h = await rlm.spawn('CHILD: fix status', name='c')\nopen({}, 'w').write(h.rlm_child_id)\nprint('spawned')",
        json!(HANDLE)
    );
    Box::new(move |request, _| {
        let results = tool_results(request).len();
        if is_child(request) {
            return match results {
                0 => tool("toolu_c0", &blocked_cell("started1", "release1")),
                _ => text("(first worker)"),
            };
        }
        match results {
            0 => tool("toolu_root", &spawn),
            _ => text("spawned"),
        }
    })
}

/// Worker 2: the reattached child either runs another blocked cell (`finish == false`) or ends; the root collects
/// when its prompt starts with ROOT2.
fn second_script(finish: bool) -> Script {
    Box::new(move |request, _| {
        let results = tool_results(request).len();
        if is_child(request) {
            return match (finish, results) {
                (false, 1) => tool("toolu_c1", &blocked_cell("started2", "release2")),
                (true, 1) => tool(
                    "toolu_c1",
                    "print(await rlm.finish('passed', 'did the work', evidence=['done']))",
                ),
                _ => text("child done"),
            };
        }
        let collecting = texts(request).last().is_some_and(|t| t.starts_with("ROOT2"));
        match (collecting, results) {
            (true, 1) => tool("toolu_collect2", &collect_cell("result2.json")),
            _ => text("root done"),
        }
    })
}

/// Worker 3: the child (if it still runs) replies; the root collects when its prompt starts with ROOT3.
fn third_script(collected_before: usize) -> Script {
    Box::new(move |request, _| {
        let results = tool_results(request).len();
        if is_child(request) {
            return text("child seen by worker 3");
        }
        let collecting = texts(request).last().is_some_and(|t| t.starts_with("ROOT3"));
        match (collecting, results) {
            (true, n) if n == collected_before => tool("toolu_collect3", &collect_cell("result3.json")),
            _ => text("root done"),
        }
    })
}

#[test]
fn spawn_collect_a_child_running_across_two_restarts_keeps_running() {
    let home = Home::new(None);
    let w1 = home.open(first_script(), &[], None);
    prompt(&w1, "ROOT1: delegate");
    wait_file(&home.cwd.join("started1"));
    w1.hand_over();
    std::fs::write(home.cwd.join("release1"), "1").unwrap();
    let w2 = home.open(second_script(false), &[], None);
    wait_file(&home.cwd.join("started2"));
    // Reattached, the child stays unfinished in the journal.
    assert_eq!(stored_state(&home), "running");
    w2.hand_over();
    std::fs::write(home.cwd.join("release2"), "1").unwrap();
    let w3 = home.open(third_script(1), &[], None);
    prompt(&w3, "ROOT3: collect");
    let result3 = result(&home, "result3.json");
    assert_eq!(result3["status"], "succeeded", "{result3}");
    assert_eq!(result3["value"], "child seen by worker 3", "{result3}");
    assert!(
        w3.requests().iter().any(is_child),
        "the third worker never ran the child"
    );
    assert_eq!(stored_state(&home), "completed");
}

#[test]
fn spawn_collect_a_collected_result_reads_the_same_after_the_next_restart() {
    let home = Home::new(None);
    let w1 = home.open(first_script(), &[], None);
    prompt(&w1, "ROOT1: delegate");
    wait_file(&home.cwd.join("started1"));
    w1.hand_over();
    std::fs::write(home.cwd.join("release1"), "1").unwrap();
    let w2 = home.open(second_script(true), &[], None);
    prompt(&w2, "ROOT2: collect");
    let result2 = result(&home, "result2.json");
    assert_eq!(result2["status"], "succeeded", "{result2}");
    assert_eq!(stored_state(&home), "completed");
    w2.hand_over();
    let w3 = home.open(third_script(2), &[], None);
    prompt(&w3, "ROOT3: collect");
    assert_eq!(result(&home, "result3.json"), result2);
}
