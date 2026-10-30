//! `ultron usage` and automatic memory: a session whose runs opened with recalled memories (`ultron.memory` custom
//! messages, session-core auto_memory) reports them, and does not say Hindsight was not configured; a session with
//! no memory at all reports as before.

use cli::localtime::Zone;
use cli::usage::log::read_session_log;
use cli::usage::report::build_session_report;
use cli::usage::text::{TextOptions, render_session_report};
use serde_json::{Value, json};
use std::path::Path;

fn recall(id: &str, ids: &[&str], digest: Option<&str>, stale: u64) -> Value {
    let mut details = json!({"ids": ids, "scopes": ids.iter().map(|_| "project").collect::<Vec<_>>()});
    if let Some(digest) = digest {
        details["digest"] = digest.into();
    }
    if stale > 0 {
        details["stale"] = stale.into();
    }
    json!({"kind": "entry", "id": id, "parentId": null, "timestamp": 1001, "type": "message",
        "message": {"role": "custom", "customType": "ultron.memory", "content": "[memory] ...", "display": true,
            "details": details, "timestamp": 1001}})
}

fn memory_lines(dir: &Path, lines: &[Value]) -> (Value, Vec<String>) {
    let path = dir.join("session.jsonl");
    let mut text = String::from(r#"{"v":4,"kind":"header","id":"s1","storageVersion":1,"createdAt":1000,"cwd":"/x"}"#);
    text.push('\n');
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    let report = build_session_report(&read_session_log(&path).unwrap());
    let zone = Zone::utc();
    let rendered = render_session_report(
        &report,
        &TextOptions {
            utc: true,
            home: "/home",
            zone: &zone,
        },
    );
    let at = rendered.iter().position(|line| line == "Memory").unwrap();
    (report["memory"].clone(), rendered[at + 1..].to_vec())
}

#[test]
fn cli_usage_reports_automatic_recalls() {
    let dir = tempfile::tempdir().unwrap();
    let (memory, lines) = memory_lines(
        dir.path(),
        &[
            recall("e1", &["m1", "f2"], Some("brief-1"), 0),
            recall("e2", &["f3"], None, 1),
            json!({"kind": "entry", "id": "e3", "parentId": "e2", "timestamp": 1002, "type": "message",
                "message": {"role": "custom", "customType": "loki", "content": "x", "details": {"ids": ["d"]}}}),
        ],
    );
    assert_eq!(
        memory,
        json!({"operations": null, "recall": {"messages": 2, "memories": 3, "briefs": 1, "stale": 1}})
    );
    let text = lines.join("\n");
    assert!(
        text.contains("Recall") && text.contains("2 automatic recalls · 3 memories · 1 project brief · 1 stale"),
        "{text}"
    );
    assert!(
        text.contains("the model made no memory call; automatic recall is below"),
        "{text}"
    );
    assert!(!text.contains("Hindsight not configured"), "{text}");
}

#[test]
fn cli_usage_without_memory_reports_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let (memory, lines) = memory_lines(dir.path(), &[]);
    assert_eq!(memory, json!({"operations": null}));
    assert_eq!(
        lines,
        ["  Store    not recorded (no memory operation recorded: Hindsight not configured, or memory unused)"]
    );
}
